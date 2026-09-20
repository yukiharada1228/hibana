import { useEffect, useState, type FormEvent } from "react";
import { Api, errorMessage, type LoginOptions } from "./api";
import { beginOidc, completeOidc } from "./oidc";
import type { Session } from "./types";
import { Button } from "./components/ui/button";
import { Input } from "./components/ui/input";
import { Brand, Notice } from "./components/common";

export function Login({
  message,
  onLogin,
}: {
  message: string;
  onLogin: (api: Api, session: Session, expires: string, email: string) => void;
}) {
  const [busy, setBusy] = useState(true),
    [error, setError] = useState("");
  const [options, setOptions] = useState<LoginOptions | null>(null);
  const secure =
    location.protocol === "https:" ||
    ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  useEffect(() => {
    if (!secure) return;
    let active = true;
    let adopted = false;
    const api = new Api();
    const completion =
      completeOidc() ??
      api.restoreSession().then((result) => (result ? { api, ...result } : null));
    completion
      .then((result) => {
        if (active && result) {
          adopted = result.api === api;
          onLogin(
            result.api,
            result.session,
            result.expires,
            result.session.email || "ログイン中",
          );
        }
      })
      .catch((error) => {
        if (active) setError(errorMessage(error));
      })
      .finally(() => {
        if (active) setBusy(false);
      });
    api
      .loginOptions()
      .then((options) => {
        if (active) {
          setOptions(options);
        }
      })
      .catch((error) => {
        if (active) setError(errorMessage(error));
      });
    return () => {
      active = false;
      if (!adopted) api.close();
    };
  }, []);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!secure || busy || !options) return;
    setBusy(true);
    setError("");
    const form = event.currentTarget,
      fields = new FormData(form),
      api = new Api();
    try {
      await beginOidc(api, String(fields.get("tenant")).trim(), options);
    } catch (error) {
      api.close();
      setError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }
  return (
    <main className="login">
      <div className="login-main">
        <form onSubmit={submit} className="login-form">
          <Brand />
          <h1>コンソールにログイン</h1>
          <p className="muted">
            テナントを入力し、組織のアカウントでログインしてください。
          </p>
          <div className="connection">
            接続先 <strong>{location.host}</strong>
          </div>
          {message && <Notice>{message}</Notice>}
          {error && <Notice error>{error}</Notice>}
          {!secure && (
            <Notice error>この接続先には HTTPS でアクセスしてください。</Notice>
          )}
          <label htmlFor="tenant">テナント</label>
          <Input
            id="tenant"
            name="tenant"
            autoComplete="organization"
            required
            maxLength={63}
          />
          <Button type="submit" disabled={busy || !secure || !options}>
            {busy ? "接続中…" : "組織のアカウントでログイン"}
          </Button>
          <p className="muted small">
            アカウントの発行は基盤の管理者へお問い合わせください。
          </p>
        </form>
        <a className="license-link" href="/licenses/NOTICE">
          ライセンス
        </a>
      </div>
    </main>
  );
}
