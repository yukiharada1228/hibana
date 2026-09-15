import { useEffect, useState } from "react";
import { Api, appUrl, errorMessage } from "./api";
import type { Component, Session, Version } from "./types";
import { Button } from "./components/ui/button";
import { Input } from "./components/ui/input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "./components/ui/table";
import {
  Badge,
  bytes,
  Confirm,
  date,
  Empty,
  Icon,
  Notice,
} from "./components/common";

export function Applications({
  components,
  session,
}: {
  components: Component[];
  session: Session;
}) {
  const [query, setQuery] = useState("");
  const filtered = components.filter((item) =>
    item.name.toLowerCase().includes(query.toLowerCase()),
  );
  return (
    <>
      <div className="page-title">
        <div>
          <p className="eyebrow">APPLICATIONS</p>
          <h1>アプリケーション</h1>
          <p className="muted">このテナントに配備されたアプリを管理します。</p>
        </div>
        <Button asChild>
          <a href="#deploy">
            <Icon name="deploy" />
            デプロイする
          </a>
        </Button>
      </div>
      <div className="stats">
        <div>
          <span>アプリケーション</span>
          <strong>
            {components.length}
            <small>件</small>
          </strong>
        </div>
        <div>
          <span>HTTP 配信中</span>
          <strong>
            {
              components.filter((c) => c.active_version_id && c.ingress_enabled)
                .length
            }
            <small>件</small>
          </strong>
        </div>
        <div className="stat-context">
          <span>実行環境</span>
          <strong>WebAssembly</strong>
          <p>接続先の Hibana 基盤で実行</p>
        </div>
      </div>
      <section className="panel">
        <div className="panel-toolbar">
          <h2>
            すべてのアプリ <span className="count">{components.length}</span>
          </h2>
          <div className="search">
            <label htmlFor="app-search">名前で絞り込み</label>
            <Input
              id="app-search"
              blockSize="sm"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
            />
          </div>
        </div>
        {!components.length ? (
          <Empty title="最初のアプリをデプロイしましょう">
            <a href="#deploy">CLI の接続・デプロイ手順を見る</a>
          </Empty>
        ) : !filtered.length ? (
          <Empty title="一致するアプリがありません">
            別の名前で検索してください。
          </Empty>
        ) : (
          <div className="table-scroll">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>アプリ名</TableHead>
                  <TableHead>状態</TableHead>
                  <TableHead>作成日時</TableHead>
                  <TableHead>
                    <span className="sr-only">アプリを開く</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((component) => {
                  const url = appUrl(component, session);
                  return (
                    <TableRow key={component.component_id}>
                      <TableCell>
                        <a
                          className="app-name"
                          href={`#apps/${component.component_id}`}
                        >
                          <span className="app-icon">
                            <Icon name="apps" />
                          </span>
                          {component.name}
                        </a>
                      </TableCell>
                      <TableCell>
                        <Badge active={!!url}>
                          {url
                            ? "配信中"
                            : component.active_version_id
                              ? "配備済み"
                              : "未配備"}
                        </Badge>
                      </TableCell>
                      <TableCell className="muted nowrap">
                        {date(component.created_at)}
                      </TableCell>
                      <TableCell>
                        {url && (
                          <a
                            className="external-link"
                            href={url}
                            target="_blank"
                            rel="noopener noreferrer"
                            aria-label={`${component.name} を開く`}
                          >
                            <Icon name="arrow" />
                          </a>
                        )}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </div>
        )}
      </section>
      <div className="info-strip">
        <Icon name="deploy" />
        <div>
          <strong>いつものターミナルから、デプロイ。</strong>
          <p>
            <code>hibana deploy</code> で配備したアプリがここに表示されます。
          </p>
        </div>
        <a href="#deploy">接続方法を見る →</a>
      </div>
    </>
  );
}

export function Application({
  api,
  component,
  session,
  onChange,
}: {
  api: Api;
  component: Component;
  session: Session;
  onChange: () => Promise<void>;
}) {
  const [versions, setVersions] = useState<Version[]>([]),
    [tab, setTab] = useState("versions");
  const [env, setEnv] = useState<Record<string, string>>({}),
    [error, setError] = useState(""),
    [message, setMessage] = useState("");
  const [loading, setLoading] = useState(true),
    [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<{ version: string } | "delete" | null>(
    null,
  );
  const canDeploy = session.scopes.includes("deploy"),
    canAdmin = session.scopes.includes("admin");
  useEffect(() => {
    let current = true;
    setLoading(true);
    setError("");
    setEnv({});
    const request =
      tab === "settings"
        ? api.config(component.component_id)
        : api.versions(component.component_id);
    request
      .then((result) => {
        if (current) {
          if (Array.isArray(result)) setVersions(result);
          else setEnv(result.env);
        }
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      })
      .finally(() => {
        if (current) setLoading(false);
      });
    return () => {
      current = false;
    };
  }, [api, component, tab]);
  async function apply() {
    if (!confirm) return;
    setBusy(true);
    setError("");
    setMessage("");
    try {
      if (confirm === "delete") {
        await api.deleteComponent(component.component_id);
        location.hash = "apps";
      } else {
        await api.rollback(component.component_id, confirm.version);
        setMessage(`バージョン ${confirm.version} に切り戻しました。`);
      }
      setConfirm(null);
      await onChange();
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }
  const url = appUrl(component, session);
  return (
    <>
      <a className="back" href="#apps">
        ← アプリケーション
      </a>
      <div className="page-title">
        <div>
          <p className="eyebrow">APPLICATION</p>
          <h1>{component.name}</h1>
          <p className="muted">
            {url ? (
              <a href={url} target="_blank" rel="noopener noreferrer">
                {url} ↗
              </a>
            ) : (
              "HTTP 配信は有効になっていません。"
            )}
          </p>
        </div>
        <Badge active={!!url}>{url ? "配信中" : "配信停止中"}</Badge>
      </div>
      <div className="tabs" role="tablist" aria-label="アプリの詳細">
        <button
          role="tab"
          aria-selected={tab === "versions"}
          onClick={() => setTab("versions")}
        >
          バージョン
        </button>
        {canDeploy && (
          <button
            role="tab"
            aria-selected={tab === "settings"}
            onClick={() => setTab("settings")}
          >
            環境変数
          </button>
        )}
      </div>
      {error && !confirm && <Notice error>{error}</Notice>}
      {message && <Notice>{message}</Notice>}
      {loading ? (
        <Notice>読み込み中…</Notice>
      ) : error && !confirm ? null : tab === "versions" ? (
        <section className="panel">
          <div className="panel-toolbar">
            <div>
              <h2>デプロイ履歴</h2>
              <p className="muted">
                配備したバージョンを確認し、実行するバージョンを切り替えます。
              </p>
            </div>
          </div>
          {!versions.length ? (
            <Empty title="バージョンはまだありません">
              CLI からこのアプリをデプロイしてください。
            </Empty>
          ) : (
            <div className="table-scroll">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>バージョン</TableHead>
                    <TableHead>状態</TableHead>
                    <TableHead>サイズ</TableHead>
                    <TableHead>配備日時</TableHead>
                    <TableHead>操作</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {versions.map((version) => (
                    <TableRow key={version.version_id}>
                      <TableCell>
                        <strong>{version.version}</strong>
                        <div className="hash" title={version.wasm_sha256}>
                          SHA-256 {version.wasm_sha256.slice(0, 12)}
                        </div>
                      </TableCell>
                      <TableCell>
                        <Badge
                          active={
                            version.version_id === component.active_version_id
                          }
                        >
                          {version.version_id === component.active_version_id
                            ? "現在のバージョン"
                            : ["ready", "active"].includes(version.status)
                              ? "待機中"
                              : version.status}
                        </Badge>
                      </TableCell>
                      <TableCell className="nowrap">
                        {bytes(version.size_bytes)}
                      </TableCell>
                      <TableCell className="nowrap muted">
                        {date(version.created_at)}
                      </TableCell>
                      <TableCell>
                        {canDeploy &&
                          version.version_id !==
                            component.active_version_id && (
                            <Button
                              size="sm"
                              variant="outline"
                              onClick={() => {
                                setError("");
                                setConfirm({ version: version.version });
                              }}
                            >
                              切り戻す
                            </Button>
                          )}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )}
        </section>
      ) : (
        <section className="panel">
          <div className="panel-toolbar">
            <div>
              <h2>現在のバージョンの環境変数</h2>
              <p className="muted">
                変更は <code>hibana.json</code> の <code>vars</code>{" "}
                を更新し、新しいバージョンをデプロイします。
              </p>
            </div>
          </div>
          {Object.keys(env).length ? (
            <div className="table-scroll">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>名前</TableHead>
                    <TableHead>値</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {Object.entries(env).map(([key, value]) => (
                    <TableRow key={key}>
                      <TableCell>
                        <code>{key}</code>
                      </TableCell>
                      <TableCell className="env-value">{value}</TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          ) : (
            <Empty title="環境変数はありません">
              Secrets の値はこの画面には表示されません。
            </Empty>
          )}
        </section>
      )}
      {canAdmin && (
        <div className="danger-zone">
          <div>
            <strong>アプリを削除</strong>
            <p>HTTP 配信が停止し、一覧から削除されます。</p>
          </div>
          <Button
            variant="outline"
            onClick={() => {
              setError("");
              setConfirm("delete");
            }}
          >
            アプリを削除
          </Button>
        </div>
      )}
      {confirm && (
        <Confirm
          title={
            confirm === "delete"
              ? `${component.name} を削除しますか？`
              : `バージョン ${confirm.version} に切り戻しますか？`
          }
          busy={busy}
          confirmLabel={confirm === "delete" ? "削除する" : "切り戻す"}
          onCancel={() => {
            setConfirm(null);
            setError("");
          }}
          onConfirm={apply}
        >
          <p>
            {confirm === "delete"
              ? "このアプリへのアクセスはできなくなります。"
              : "コードと、そのバージョンに保存された環境変数・Secret の参照が切り替わります。"}
          </p>
          {error && <Notice error>{error}</Notice>}
        </Confirm>
      )}
    </>
  );
}
