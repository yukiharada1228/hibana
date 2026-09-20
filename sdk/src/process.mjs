import { spawn } from "node:child_process";

// Arguments are passed literally. hibana.json never implicitly invokes a shell.
// Give each command its own POSIX process group so npm/build tool descendants
// receive cancellation too, without signaling the CLI or its caller.
export function run(command, args, options = {}) {
  const {
    signal,
    stopTimeoutMs = 5000,
    capture = false,
    maxBuffer = 2 * 1024 * 1024,
    timeout,
    ...spawnOptions
  } = options;
  return new Promise((resolve, reject) => {
    signal?.throwIfAborted();
    const grouped = process.platform !== "win32";
    const child = spawn(command, args, {
      stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
      ...spawnOptions,
      detached: grouped,
      shell: false,
    });
    let failure, killTimer, timeoutTimer;
    const output = { stdout: [], stderr: [] };
    const sizes = { stdout: 0, stderr: 0 };
    function sendSignal(value) {
      try {
        if (grouped && child.pid) process.kill(-child.pid, value);
        else child.kill(value);
      } catch (error) {
        if (error.code !== "ESRCH") failure ||= error;
      }
    }
    function cancel(value, reason) {
      if (failure) return;
      failure = reason;
      sendSignal(value);
      // Keep the timer referenced until even an uncooperative command is reaped.
      killTimer = setTimeout(() => sendSignal("SIGKILL"), stopTimeoutMs);
    }
    const interrupted = (value) =>
      cancel(
        value,
        new DOMException(`Command interrupted (${value})`, "AbortError"),
      );
    const sigint = () => interrupted("SIGINT");
    const sigterm = () => interrupted("SIGTERM");
    const aborted = () => cancel("SIGTERM", signal.reason);
    process.on("SIGINT", sigint);
    process.on("SIGTERM", sigterm);
    signal?.addEventListener("abort", aborted, { once: true });
    if (timeout)
      timeoutTimer = setTimeout(
        () => cancel("SIGTERM", new Error(`${command} timed out`)),
        timeout,
      );
    if (capture) {
      for (const name of ["stdout", "stderr"]) {
        child[name].on("data", (chunk) => {
          sizes[name] += chunk.length;
          if (sizes[name] <= maxBuffer) output[name].push(chunk);
          else
            cancel(
              "SIGTERM",
              new Error(`${command} ${name} exceeds ${maxBuffer} bytes`),
            );
        });
      }
    }
    child.once("error", (error) => {
      failure ||=
        error.code === "ENOENT"
          ? new Error(
              `Command '${command}' was not found. Install it and make sure it is on PATH.`,
              { cause: error },
            )
          : error;
    });
    child.once("exit", () => {
      // A wrapper may exit on TERM while one of its descendants ignores it.
      if (failure && grouped) sendSignal("SIGKILL");
    });
    child.once("close", (code, exitSignal) => {
      clearTimeout(killTimer);
      clearTimeout(timeoutTimer);
      process.removeListener("SIGINT", sigint);
      process.removeListener("SIGTERM", sigterm);
      signal?.removeEventListener("abort", aborted);
      const result = Object.fromEntries(
        Object.entries(output).map(([name, chunks]) => [
          name,
          Buffer.concat(chunks).toString("utf8"),
        ]),
      );
      if (failure || code !== 0) {
        const error =
          failure || new Error(`${command} failed (${exitSignal || code})`);
        if (capture) Object.assign(error, result);
        reject(error);
      } else resolve(capture ? result : undefined);
    });
  });
}

// Explicit stdin input for scripts, shared by passwords and secrets.
export async function readStdin(label, input = process.stdin) {
  if (input.isTTY)
    throw new Error(
      label === "Password"
        ? "Use --password-stdin < password.txt, or omit --password-stdin to enter the password interactively"
        : `Pipe the ${label.toLowerCase()} value through stdin; it is not accepted as a command argument`,
    );
  const chunks = [];
  let size = 0;
  for await (const chunk of input) {
    size += chunk.length;
    if (size > 4096) throw new Error(`${label} exceeds 4096 bytes`);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks)
    .toString("utf8")
    .replace(/\r?\n$/, "");
}
