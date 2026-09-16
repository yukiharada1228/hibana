import { useState, type FormEvent } from "react";
import { Api, errorMessage } from "./api";
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
  const [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const secure =
    location.protocol === "https:" ||
    ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!secure || busy) return;
    setBusy(true);
    setError("");
    const form = event.currentTarget,
      fields = new FormData(form),
      api = new Api();
    try {
      const email = String(fields.get("email")).trim();
      const { session, expires } = await api.login(
        String(fields.get("tenant")).trim(),
        email,
        String(fields.get("password")),
      );
      form.reset();
      onLogin(api, session, expires, email);
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
            管理者から受け取ったアカウントを入力してください。
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
          <label htmlFor="email">メールアドレス</label>
          <Input
            id="email"
            name="email"
            type="email"
            autoComplete="username"
            required
          />
          <label htmlFor="password">パスワード</label>
          <Input
            id="password"
            name="password"
            type="password"
            autoComplete="current-password"
            required
          />
          <Button type="submit" disabled={busy || !secure}>
            {busy ? "接続中…" : "ログイン"}
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
