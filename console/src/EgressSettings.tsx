import { useEffect, useRef, useState } from "react";
import { Api, ApiError, errorMessage } from "./api";
import type { Component, EgressPolicy } from "./types";
import { Notice } from "./components/common";
import { Button } from "./components/ui/button";
import { Input } from "./components/ui/input";

export function EgressSettings({
  api,
  component,
  canManage,
  onChange,
}: {
  api: Api;
  component: Component;
  canManage: boolean;
  onChange: () => Promise<void>;
}) {
  const [policy, setPolicy] = useState<EgressPolicy | null>(null);
  const [destination, setDestination] = useState("");
  const [error, setError] = useState("");
  const [loadError, setLoadError] = useState("");
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const revision = useRef(0);
  const mounted = useRef(true);
  const changing = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    if (changing.current) return;
    const current = ++revision.current;
    api
      .egress(component.component_id)
      .then((result) => {
        if (revision.current === current) {
          setPolicy(result);
          setLoadError("");
        }
      })
      .catch((error) => {
        if (revision.current === current) {
          setLoadError(errorMessage(error));
        }
      });
    return () => {
      ++revision.current;
    };
  }, [api, component]);

  async function change(action: "allow" | "deny", endpoint: string) {
    if (changing.current) return;
    ++revision.current;
    changing.current = true;
    setBusy(true);
    setError("");
    setMessage("");
    try {
      const result = await api.changeEgress(
        component.component_id,
        action,
        endpoint,
      );
      if (!mounted.current) return;
      ++revision.current;
      setPolicy(result);
      setLoadError("");
      if (action === "allow") setDestination("");
      setMessage("外部通信の許可を更新しました。");
      changing.current = false;
      await onChange();
    } catch (error) {
      if (mounted.current)
        setError(
          error instanceof ApiError && error.status === 400
            ? "ホスト名:ポートの形式で指定してください。URL・認証情報・ワイルドカードは使用できません。上限は 64 件です。"
            : errorMessage(error),
        );
    } finally {
      changing.current = false;
      if (mounted.current) setBusy(false);
    }
  }

  return (
    <section className="panel egress-settings">
      <div className="panel-toolbar">
        <div>
          <h2>許可された外部通信先</h2>
          <p className="muted small">
            全バージョン共通。変更は次の実行から適用されます。
          </p>
        </div>
      </div>
      <div className="egress-content">
        {loadError && (
          <Notice error>
            {loadError}
            {policy &&
              " 表示中の通信先は前回取得した内容です。現在の許可状態は確認できていません。"}
          </Notice>
        )}
        {error && <Notice error>{error}</Notice>}
        {message && <p role="status">{message}</p>}
        {!policy ? (
          !loadError && <p>通信先を読み込み中…</p>
        ) : (
          <>
            {policy.allow_outbound.length ? (
              <ul className="egress-destinations">
                {policy.allow_outbound.map((endpoint) => (
                  <li key={endpoint}>
                    <code>{endpoint}</code>
                    {canManage && (
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        aria-label={`${endpoint} の許可を取り消す`}
                        onClick={() => void change("deny", endpoint)}
                      >
                        許可を取り消す
                      </Button>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="muted">なし（外部通信は拒否）</p>
            )}
            {canManage ? (
              <form
                className="egress-form"
                onSubmit={(event) => {
                  event.preventDefault();
                  void change("allow", destination.trim());
                }}
              >
                <label htmlFor="egress-destination">
                  通信先（ホスト名:ポート）
                </label>
                <p className="muted small" id="egress-hint">
                  例: db.example.com:5432。接続 URL やパスワードは入力しません。
                </p>
                <div>
                  <Input
                    id="egress-destination"
                    aria-describedby="egress-hint"
                    required
                    maxLength={300}
                    value={destination}
                    disabled={busy}
                    onChange={(event) => setDestination(event.target.value)}
                    autoComplete="off"
                    spellCheck={false}
                  />
                  <Button type="submit" disabled={busy || !destination.trim()}>
                    通信先を許可
                  </Button>
                </div>
              </form>
            ) : (
              <p className="muted small">
                通信先の変更には管理者権限が必要です。
              </p>
            )}
          </>
        )}
      </div>
    </section>
  );
}
