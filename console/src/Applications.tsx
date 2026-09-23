import { useEffect, useRef, useState } from "react";
import { Api, ApiError, appUrl, publication, errorMessage } from "./api";
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
  Notice,
  versionLabel,
} from "./components/common";
import { ApplicationSettings } from "./ApplicationSettings";
import { EgressSettings } from "./EgressSettings";
import { Executions } from "./Executions";
import { Extensions } from "./Extensions";
import { ApplicationActivity } from "./ApplicationActivity";

function deletionReason(version: Version, component: Component): string | null {
  const reason = version.deletion_blocked_reason;
  if (
    reason === "active_version" ||
    version.version_id === component.active_version_id
  )
    return "現在のバージョンのため削除できません。";
  if (reason === "rollback_target")
    return "直前のバージョンは切り戻し先として保護されています。";
  if (reason === "active_executions")
    return "実行中・実行待ちの処理があるため削除できません。処理の完了後に更新してください。";
  return reason
    ? "現在の状態では削除できません。一覧を更新してください。"
    : null;
}

type Confirmation =
  | { action: "delete-component" }
  | { action: "rollback"; version: string }
  | { action: "delete-version"; version: string; versionId: string };

function PublicUrl({ component }: { component: Component }) {
  const url = appUrl(component);
  const [message, setMessage] = useState("");
  if (!url) return <span className="muted">—</span>;
  return (
    <div className="public-url">
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        aria-label={`${component.name} を開く`}
      >
        {url}
      </a>
      <Button
        variant="text"
        size="sm"
        aria-label={`${component.name} の URL をコピー`}
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(url);
            setMessage("コピーしました");
          } catch {
            setMessage("URL を選択してコピーしてください");
          }
        }}
      >
        コピー
      </Button>
      {message && (
        <span role="status" className="small">
          {message}
        </span>
      )}
    </div>
  );
}

export function Applications({ components }: { components: Component[] }) {
  const [query, setQuery] = useState("");
  const filtered = components.filter((item) =>
    item.name.toLowerCase().includes(query.toLowerCase()),
  );
  return (
    <>
      <div className="page-title applications-title">
        <div className="title-with-count">
          <h1>アプリケーション</h1>
          <span className="count" role="status">
            {query
              ? `${filtered.length} / ${components.length} 件`
              : `${components.length} 件`}
          </span>
        </div>
        <div className="search">
          <label htmlFor="app-search">名前で絞り込み</label>
          <Input
            id="app-search"
            blockSize="sm"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
      </div>
      <section className="panel">
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
                  <TableHead>現在のバージョン</TableHead>
                  <TableHead>公開設定</TableHead>
                  <TableHead>公開 URL</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((component) => (
                  <TableRow key={component.component_id}>
                    <TableCell>
                      <a
                        className="app-name"
                        href={`#apps/${component.component_id}`}
                      >
                        {component.name}
                      </a>
                    </TableCell>
                    <TableCell>
                      <strong title={component.active_version || undefined}>
                        {component.active_version
                          ? versionLabel(component.active_version)
                          : "—"}
                      </strong>
                      {component.active_version_created_at && (
                        <div className="muted small">
                          登録 {date(component.active_version_created_at)}
                        </div>
                      )}
                    </TableCell>
                    <TableCell>
                      <Badge>{publication(component)}</Badge>
                    </TableCell>
                    <TableCell>
                      <PublicUrl component={component} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </section>
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
  const [versionData, setVersions] = useState<Version[] | null>(null),
    [tab, setTab] = useState("versions");
  const versions = versionData ?? [];
  const [error, setError] = useState(""),
    [message, setMessage] = useState("");
  const [loadError, setLoadError] = useState("");
  const [loading, setLoading] = useState(true),
    [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<Confirmation | null>(null);
  const [extensionVersion, setExtensionVersion] = useState<string | null>(null);
  const settingsTab = useRef<HTMLButtonElement>(null);
  const canDeploy = session.scopes.includes("deploy"),
    canAdmin = session.scopes.includes("admin");
  const deleteTarget =
    confirm?.action === "delete-version"
      ? versions.find((version) => version.version_id === confirm.versionId)
      : undefined;
  const deleteBlocked =
    confirm?.action === "delete-version"
      ? deleteTarget
        ? deletionReason(deleteTarget, component)
        : "このバージョンは一覧にありません。削除済みの可能性があります。"
      : null;
  useEffect(() => {
    setVersions(null);
  }, [api, component.component_id]);
  useEffect(() => {
    let current = true;
    setLoading(true);
    setLoadError("");
    api
      .versions(component.component_id)
      .then((result) => {
        if (current) setVersions(result);
      })
      .catch((error) => {
        if (current) setLoadError(errorMessage(error));
      })
      .finally(() => {
        if (current) setLoading(false);
      });
    return () => {
      current = false;
    };
  }, [api, component]);
  async function apply() {
    if (
      !confirm ||
      busy ||
      deleteBlocked ||
      (confirm.action === "delete-version" && (loading || loadError))
    )
      return;
    setBusy(true);
    setError("");
    setMessage("");
    try {
      if (confirm.action === "delete-component") {
        await api.deleteComponent(component.component_id);
        location.hash = "apps";
      } else if (confirm.action === "delete-version") {
        await api.deleteVersion(component.component_id, confirm.versionId);
        setVersions((items) =>
          (items ?? []).filter((item) => item.version_id !== confirm.versionId),
        );
        setMessage(
          `バージョン ${versionLabel(confirm.version)} を削除しました。`,
        );
      } else {
        await api.rollback(component.component_id, confirm.version);
        setMessage(
          `バージョン ${versionLabel(confirm.version)} に切り戻しました。`,
        );
      }
      setConfirm(null);
      await onChange();
    } catch (error) {
      setError(errorMessage(error));
      if (
        confirm.action === "delete-version" &&
        error instanceof ApiError &&
        [404, 409].includes(error.status)
      ) {
        // The version may have become active or started executing since the list loaded.
        // Keep the confirmation open with the refreshed protection reason.
        await onChange();
      }
    } finally {
      setBusy(false);
    }
  }
  return (
    <>
      <a className="back" href="#apps">
        ← アプリケーション
      </a>
      <div className="application-header">
        <div className="application-heading">
          <h1>{component.name}</h1>
          <Badge>{publication(component)}</Badge>
        </div>
        <div className="application-summary">
          <p className="muted small">
            現在のバージョン：
            <strong title={component.active_version || undefined}>
              {component.active_version
                ? versionLabel(component.active_version)
                : "未配備"}
            </strong>
          </p>
          {appUrl(component) && <PublicUrl component={component} />}
        </div>
      </div>
      {tab !== "executions" && (
        <ApplicationActivity
          api={api}
          component={component}
          versions={versions}
          onOpen={() => setTab("executions")}
        />
      )}
      <div className="tabs" role="tablist" aria-label="アプリの詳細">
        <button
          role="tab"
          aria-selected={tab === "versions"}
          onClick={() => setTab("versions")}
        >
          バージョン
        </button>
        <button
          role="tab"
          aria-selected={tab === "extensions"}
          onClick={() => {
            setExtensionVersion(null);
            setTab("extensions");
          }}
        >
          拡張
        </button>
        <button
          role="tab"
          aria-selected={tab === "executions"}
          onClick={() => setTab("executions")}
        >
          実行履歴
        </button>
        {(canDeploy || canAdmin) && (
          <button
            ref={settingsTab}
            role="tab"
            aria-selected={tab === "settings"}
            onClick={() => setTab("settings")}
          >
            設定
          </button>
        )}
      </div>
      {error && !confirm && <Notice error>{error}</Notice>}
      {message && <Notice>{message}</Notice>}
      {loadError && (tab === "versions" || tab === "extensions") && (
        <Notice error>
          {loadError}
          {versionData && " 表示中のバージョン一覧は前回取得した内容です。"}
        </Notice>
      )}
      {tab === "extensions" ? (
        !versionData ? (
          !loadError && <Notice>読み込み中…</Notice>
        ) : (
          <Extensions
            api={api}
            component={component}
            versions={versions}
            selected={extensionVersion || component.active_version}
            onSelect={setExtensionVersion}
            onOpenSettings={
              canDeploy || canAdmin
                ? () => {
                    setTab("settings");
                    settingsTab.current?.focus();
                  }
                : undefined
            }
          />
        )
      ) : tab === "executions" ? (
        <Executions api={api} component={component} versions={versions} />
      ) : tab === "settings" ? (
        <>
          <EgressSettings
            key={component.component_id}
            api={api}
            component={component}
            canManage={canAdmin}
            onChange={onChange}
          />
          {canDeploy && <ApplicationSettings api={api} component={component} />}
          {canAdmin && (
            <div className="danger-zone">
              <div>
                <strong>アプリを削除</strong>
                <p>公開が停止し、一覧から削除されます。</p>
              </div>
              <Button
                variant="outline"
                onClick={() => {
                  setError("");
                  setConfirm({ action: "delete-component" });
                }}
              >
                アプリを削除
              </Button>
            </div>
          )}
        </>
      ) : !versionData ? (
        !loadError && <Notice>読み込み中…</Notice>
      ) : (
        <section className="panel">
          <div className="panel-toolbar">
            <h2>バージョン履歴</h2>
            <details className="inline-help">
              <summary>登録日時について</summary>
              <p className="muted small">
                登録日時はバージョンを初めて配備した日時です。切り戻しても変わりません。
              </p>
            </details>
          </div>
          {!versions.length ? (
            <Empty title="バージョンはまだありません">
              CLI からこのアプリをデプロイしてください。
            </Empty>
          ) : (
            <div className="table-scroll">
              <Table className="version-table">
                <TableHeader>
                  <TableRow>
                    <TableHead>バージョン</TableHead>
                    <TableHead>選択状態</TableHead>
                    <TableHead>登録日時</TableHead>
                    <TableHead>操作</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {versions.map((version) => (
                    <TableRow key={version.version_id}>
                      <TableCell>
                        <strong title={version.version}>
                          {versionLabel(version.version)}
                        </strong>
                        <details className="artifact-details">
                          <summary>バージョンの詳細</summary>
                          <p className="hash">
                            完全なバージョン <code>{version.version}</code>
                          </p>
                          <p>{bytes(version.size_bytes)}</p>
                          <p className="hash">SHA-256 {version.wasm_sha256}</p>
                          <Button
                            size="sm"
                            variant="text"
                            onClick={() => {
                              setExtensionVersion(version.version);
                              setTab("extensions");
                            }}
                          >
                            拡張の構成を見る
                          </Button>
                        </details>
                      </TableCell>
                      <TableCell>
                        <Badge
                          active={
                            version.version_id === component.active_version_id
                          }
                        >
                          {version.version_id === component.active_version_id
                            ? "現在のバージョン"
                            : version.deletion_blocked_reason ===
                                "rollback_target"
                              ? "切り戻し先"
                              : "未選択"}
                        </Badge>
                      </TableCell>
                      <TableCell className="nowrap muted">
                        {date(version.created_at)}
                      </TableCell>
                      <TableCell>
                        <div className="version-actions">
                          {canDeploy &&
                            version.version_id !==
                              component.active_version_id && (
                              <Button
                                size="sm"
                                variant="outline"
                                onClick={() => {
                                  setError("");
                                  setConfirm({
                                    action: "rollback",
                                    version: version.version,
                                  });
                                }}
                              >
                                切り戻す
                              </Button>
                            )}
                          {canAdmin && (
                            <Button
                              size="sm"
                              variant="text"
                              className="version-delete"
                              disabled={
                                busy ||
                                Boolean(deletionReason(version, component))
                              }
                              aria-describedby={
                                deletionReason(version, component)
                                  ? `delete-reason-${version.version_id}`
                                  : undefined
                              }
                              onClick={() => {
                                setError("");
                                setConfirm({
                                  action: "delete-version",
                                  version: version.version,
                                  versionId: version.version_id,
                                });
                              }}
                            >
                              削除
                            </Button>
                          )}
                        </div>
                        {canAdmin && deletionReason(version, component) && (
                          <details className="version-delete-reason inline-help">
                            <summary>削除できない理由</summary>
                            <p id={`delete-reason-${version.version_id}`}>
                              {deletionReason(version, component)}
                            </p>
                          </details>
                        )}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )}
        </section>
      )}
      {confirm && (
        <Confirm
          title={
            confirm.action === "delete-component"
              ? `${component.name} を削除しますか？`
              : confirm.action === "delete-version"
                ? `バージョン ${versionLabel(confirm.version)} を削除しますか？`
                : `バージョン ${versionLabel(confirm.version)} に切り戻しますか？`
          }
          busy={busy}
          confirmLabel={confirm.action === "rollback" ? "切り戻す" : "削除する"}
          confirmDisabled={
            Boolean(deleteBlocked) ||
            (confirm.action === "delete-version" &&
              (loading || Boolean(loadError)))
          }
          onCancel={() => {
            setConfirm(null);
            setError("");
          }}
          onConfirm={apply}
        >
          <p>
            {confirm.action === "delete-component"
              ? "このアプリへのアクセスはできなくなります。"
              : confirm.action === "delete-version"
                ? "このバージョンは一覧から削除され、この版へ切り戻せなくなります。現在のバージョンの公開は継続します。"
                : "コードと、そのバージョンに保存された環境変数・Secret の参照が切り替わります。"}
          </p>
          {confirm.action !== "delete-component" && (
            <p className="hash">
              完全なバージョン <code>{confirm.version}</code>
            </p>
          )}
          {deleteBlocked && <Notice>{deleteBlocked}</Notice>}
          {confirm.action === "delete-version" && loadError && (
            <Notice error>{loadError}</Notice>
          )}
          {error && <Notice error>{error}</Notice>}
        </Confirm>
      )}
    </>
  );
}
