// Authentication owns one ordered exchange per connection. Async password/hash
// results may only advance an exchange that is still active.
function unavailable(method) {
  return Object.assign(
    new Error(
      `PostgreSQL authentication is not selected or is out of order: ${method}`,
    ),
    { code: "ERR_PG_AUTH_UNAVAILABLE" },
  );
}
function synchronous(value, method) {
  if (value && typeof value.then === "function") {
    // pg can deliver SASLFinal, AuthenticationOk and ReadyForQuery in the same
    // packet. Verification must finish before the next message is dispatched.
    Promise.resolve(value).catch(() => {});
    throw new TypeError(`SCRAM ${method} must complete synchronously`);
  }
  return value;
}

export function authenticatedClient(PgClient, auth) {
  return class extends PgClient {
    constructor(options) {
      super(options);
      this._authenticationState = "initial";
      this._requireChannelBinding = options.channel_binding === "require";
      this._channelBindingVerified = false;
    }
    get channelBindingUsed() {
      return this._channelBindingVerified;
    }
    end(callback) {
      if (
        this._connecting &&
        !this._connected &&
        !this._connectionError &&
        !this._ended
      ) {
        // pg's normal end() closes the socket but leaves an in-flight connect()
        // pending. Settle it before shutdown, including callback reentrancy.
        this._ending = true;
        this._handleErrorWhileConnecting(
          Object.assign(
            new Error("PostgreSQL connection was cancelled by end()"),
            { code: "ERR_PG_CONNECTION_CANCELLED" },
          ),
        );
      }
      return super.end(callback);
    }
    _connectionAvailable() {
      return (
        this._authenticationState !== "failed" &&
        !this._connectionError &&
        !this._ending &&
        !this._ended &&
        !this.connection.stream.destroyed
      );
    }
    _canAuthenticate() {
      return (
        this._connectionAvailable() && this._authenticationState !== "complete"
      );
    }
    _expectAuthentication(...states) {
      if (!this._connectionAvailable()) return false;
      if (states.includes(this._authenticationState)) return true;
      this._failAuthentication(
        unavailable(`unexpected message during ${this._authenticationState}`),
      );
      return false;
    }
    _failAuthentication(error) {
      if (!this._connectionAvailable()) return;
      this._authenticationState = "failed";
      this._channelBindingVerified = false;
      try {
        this.connection.emit("error", error);
      } finally {
        this.connection.stream.destroy();
      }
    }
    _rejectChannelBinding() {
      this._failAuthentication(
        Object.assign(
          new Error(
            "Required SCRAM-SHA-256-PLUS channel binding was not verified",
          ),
          { code: "ERR_PG_CHANNEL_BINDING" },
        ),
      );
    }
    _handleErrorWhileConnecting(error) {
      this._authenticationState = "failed";
      this._channelBindingVerified = false;
      try {
        super._handleErrorWhileConnecting(error);
      } finally {
        if (!this.connection.stream.destroyed) this.connection.stream.destroy();
      }
    }
    async _runAuthentication(operation, complete) {
      if (!this._canAuthenticate()) return;
      try {
        const value = await operation();
        if (this._canAuthenticate()) await complete(value);
      } catch (error) {
        if (this._canAuthenticate()) this._failAuthentication(error);
      }
    }
    _withPassword(complete) {
      return this._runAuthentication(
        () =>
          typeof this.password === "function"
            ? this.password(this.connectionParameters)
            : this.password,
        (password) => {
          if (typeof password !== "string")
            throw new TypeError(
              "PostgreSQL password provider must return a string",
            );
          this.connectionParameters.password = this.password = password;
          return complete(password);
        },
      );
    }
    _beginPassword(method) {
      if (!this._expectAuthentication("initial")) return false;
      if (this._requireChannelBinding) {
        this._rejectChannelBinding();
        return false;
      }
      if (!auth[method]) {
        this._failAuthentication(unavailable(method));
        return false;
      }
      this._authenticationState = "password-pending";
      return true;
    }
    _sendPassword(response) {
      this._authenticationState = "password-sent";
      this.connection.password(response);
    }
    _handleAuthCleartextPassword() {
      if (!this._beginPassword("cleartext")) return;
      return this._withPassword((password) => this._sendPassword(password));
    }
    _handleAuthMD5Password(message) {
      if (!this._beginPassword("md5")) return;
      return this._withPassword((password) =>
        this._runAuthentication(
          () => auth.md5(this.user, password, message.salt),
          (response) => this._sendPassword(response),
        ),
      );
    }
    _scramStream() {
      return this.enableChannelBinding && this.connection.stream;
    }
    _handleAuthSASL(message) {
      if (!this._expectAuthentication("initial")) return;
      if (
        this._requireChannelBinding &&
        (!this.connection.stream.authorized ||
          !message.mechanisms.includes("SCRAM-SHA-256-PLUS"))
      )
        return this._rejectChannelBinding();
      if (!auth.scram) return this._failAuthentication(unavailable("scram"));
      this._authenticationState = "scram-starting";
      return this._withPassword(() => {
        this.saslSession = synchronous(
          auth.scram.startSession(
            message.mechanisms,
            this._scramStream(),
            this.scramMaxIterations,
          ),
          "startSession",
        );
        this._authenticationState = "scram-first-sent";
        this.connection.sendSASLInitialResponseMessage(
          this.saslSession.mechanism,
          this.saslSession.response,
        );
      });
    }
    _handleAuthSASLContinue(message) {
      if (!this._expectAuthentication("scram-first-sent")) return;
      this._authenticationState = "scram-computing";
      return this._runAuthentication(
        () =>
          auth.scram.continueSession(
            this.saslSession,
            this.password,
            message.data,
            this._scramStream(),
          ),
        () => {
          this._authenticationState = "scram-final-sent";
          this.connection.sendSCRAMClientFinalMessage(
            this.saslSession.response,
          );
        },
      );
    }
    _handleAuthSASLFinal(message) {
      if (!this._expectAuthentication("scram-final-sent")) return;
      try {
        synchronous(
          auth.scram.finalizeSession(this.saslSession, message.data),
          "finalizeSession",
        );
        this._channelBindingVerified =
          this.saslSession.mechanism === "SCRAM-SHA-256-PLUS";
        this.saslSession = null;
        this._authenticationState = "scram-verified";
        if (this._requireChannelBinding && !this._channelBindingVerified)
          this._rejectChannelBinding();
      } catch (error) {
        this._failAuthentication(error);
      }
    }
    _attachListeners(connection) {
      super._attachListeners(connection);
      connection.on("authenticationOk", () => {
        const allowed = ["password-sent", "scram-verified"];
        if (auth.trust) allowed.push("initial");
        if (!this._connectionAvailable()) return;
        if (this._requireChannelBinding && !this._channelBindingVerified)
          return this._rejectChannelBinding();
        if (this._expectAuthentication(...allowed))
          this._authenticationState = "complete";
      });
    }
    _handleReadyForQuery(message) {
      // The first ReadyForQuery completes authentication. Later ones complete
      // queries, including the pipeline that pg drains after end() is called.
      if (!this._connected && !this._expectAuthentication("complete")) return;
      if (
        this._authenticationState !== "complete" ||
        !this._queryable ||
        this._ended ||
        this.connection.stream.destroyed
      )
        return;
      super._handleReadyForQuery(message);
    }
  };
}
