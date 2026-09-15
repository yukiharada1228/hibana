import { useCallback, useEffect, useRef, useState } from "react";
import { Api, errorMessage } from "./api";
import type { Component, Session } from "./types";
import { Login } from "./Login";
import { Applications, Application } from "./Applications";
import { Usage } from "./Usage";
import { Deploy } from "./Deploy";
import { Brand, Icon, Notice } from "./components/common";
import { Button } from "./components/ui/button";

type Connection = {
  api: Api;
  session: Session;
  expires: string;
  email: string;
};
export function App() {
  const [connection, setConnection] = useState<Connection | null>(null),
    [message, setMessage] = useState("");
  function clear(message = "") {
    setConnection(null);
    setMessage(message);
  }
  useEffect(() => {
    if (!connection) return;
    connection.api.onExpired = () =>
      clear("セッションの有効期限が切れました。再度ログインしてください。");
    const timer = setTimeout(
      () => {
        connection.api.close();
        clear("セッションの有効期限が切れました。");
      },
      Math.max(0, Date.parse(connection.expires) - Date.now()),
    );
    return () => {
      clearTimeout(timer);
      connection.api.close();
    };
  }, [connection]);
  if (!connection)
    return (
      <Login
        message={message}
        onLogin={(api, session, expires, email) => {
          setMessage("");
          setConnection({ api, session, expires, email });
        }}
      />
    );
  return <Console connection={connection} onLogout={() => clear()} />;
}

function Console({
  connection: { api, session, email },
  onLogout,
}: {
  connection: Connection;
  onLogout: () => void;
}) {
  const [route, setRoute] = useState(location.hash.slice(1) || "apps");
  const [components, setComponents] = useState<Component[] | null>(null),
    [error, setError] = useState("");
  const [loading, setLoading] = useState(false),
    [leaving, setLeaving] = useState(false);
  const revision = useRef(0);
  const reload = useCallback(async () => {
    const current = ++revision.current;
    setLoading(true);
    setError("");
    try {
      const result = await api.components();
      if (current === revision.current) setComponents(result);
    } catch (error) {
      if (current === revision.current) setError(errorMessage(error));
    } finally {
      if (current === revision.current) setLoading(false);
    }
  }, [api]);
  useEffect(() => {
    void reload();
    const refresh = () => {
      if (document.visibilityState === "visible") void reload();
    };
    const timer = setInterval(refresh, 30_000);
    window.addEventListener("focus", refresh);
    return () => {
      clearInterval(timer);
      window.removeEventListener("focus", refresh);
    };
  }, [reload]);
  useEffect(() => {
    const changed = () => setRoute(location.hash.slice(1) || "apps");
    window.addEventListener("hashchange", changed);
    return () => window.removeEventListener("hashchange", changed);
  }, []);
  async function logout() {
    setLeaving(true);
    setError("");
    try {
      await api.logout();
      onLogout();
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setLeaving(false);
    }
  }
  const selected = components?.find((c) => route === `apps/${c.component_id}`);
  return (
    <div className="shell">
      <a className="skip-link" href="#main">
        本文へ移動
      </a>
      <header className="header">
        <a href="#apps" aria-label="Hibana アプリケーション">
          <Brand />
        </a>
        <div className="header-account">
          <span className="connection-dot" />
          <span>{session.tenant_name}</span>
          <Button variant="text" size="sm" disabled={leaving} onClick={logout}>
            ログアウト
          </Button>
        </div>
      </header>
      <aside className="sidebar">
        <div className="workspace">
          <span className="workspace-avatar">
            {session.tenant_name.slice(0, 1).toUpperCase()}
          </span>
          <div>
            <small>WORKSPACE</small>
            <strong>{session.tenant_slug}</strong>
          </div>
        </div>
        <nav aria-label="メインナビゲーション">
          {(
            [
              { id: "apps", label: "アプリケーション", icon: "apps" },
              { id: "usage", label: "利用状況", icon: "usage" },
              { id: "deploy", label: "デプロイ", icon: "deploy" },
            ] as const
          ).map((item) => (
            <a
              key={item.id}
              href={`#${item.id}`}
              aria-current={
                route.split("/")[0] === item.id ? "page" : undefined
              }
            >
              <Icon name={item.icon} />
              {item.label}
            </a>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <span className="connection-dot" />
          <span>イントラネット接続</span>
          <small>{location.host}</small>
        </div>
      </aside>
      <main id="main" className="main">
        <div className="context-bar">
          <span>
            {session.tenant_slug} <span className="muted">/ コンソール</span>
          </span>
          <Button variant="text" size="sm" onClick={reload} disabled={loading}>
            <Icon name="refresh" />
            {loading ? "更新中…" : "更新"}
          </Button>
        </div>
        {error && <Notice error>{error}</Notice>}
        {!components ? (
          !error && <Notice>アプリケーションを読み込み中…</Notice>
        ) : route === "usage" ? (
          <Usage api={api} components={components} />
        ) : route === "deploy" ? (
          <Deploy session={session} email={email} />
        ) : selected ? (
          <Application
            key={selected.component_id}
            api={api}
            component={selected}
            session={session}
            onChange={reload}
          />
        ) : route.startsWith("apps/") ? (
          <Notice error>
            このアプリは見つかりません。<a href="#apps">一覧へ戻る</a>
          </Notice>
        ) : (
          <Applications components={components} session={session} />
        )}
        <footer>
          Hibana · WebAssembly application platform
          <a href="/licenses/NOTICE">ライセンス</a>
        </footer>
      </main>
    </div>
  );
}
