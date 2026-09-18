import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type {
  Component,
  EgressPolicy,
  Extension,
  Version,
  VersionDetails,
} from "./types";
import { Empty, Notice, versionLabel } from "./components/common";
import { Button } from "./components/ui/button";

function ExtensionList({ extensions }: { extensions: Extension[] }) {
  return (
    <ul className="extension-list">
      {extensions.map((extension) => (
        <li key={extension.name}>
          <div className="extension-name">
            <code>{extension.name}</code>
            <span className="muted">
              {extension.version ||
                (extension.name.startsWith("./")
                  ? "ローカル拡張・版未指定"
                  : "版未記録")}
            </span>
          </div>
          {extension.dependencies.length > 0 && (
            <p className="muted small">
              依存：{extension.dependencies.join("、")}
            </p>
          )}
          {extension.permissions.length > 0 && (
            <p className="small">要求権限：外部通信</p>
          )}
        </li>
      ))}
    </ul>
  );
}

export function Extensions({
  api,
  component,
  versions,
  selected,
  onSelect,
  onOpenSettings,
}: {
  api: Api;
  component: Component;
  versions: Version[];
  selected: string | null;
  onSelect: (version: string) => void;
  onOpenSettings?: () => void;
}) {
  const version = versions.find((v) => v.version === selected);
  const [data, setData] = useState<VersionDetails | null>(null);
  const [error, setError] = useState("");
  const [policy, setPolicy] = useState<EgressPolicy | null>(null);
  const [policyError, setPolicyError] = useState("");
  useEffect(() => {
    let current = true;
    setPolicy(null);
    setPolicyError("");
    api
      .egress(component.component_id)
      .then((result) => {
        if (current) setPolicy(result);
      })
      .catch((error) => {
        if (current) setPolicyError(errorMessage(error));
      });
    return () => {
      current = false;
    };
  }, [api, component]);
  useEffect(() => {
    let current = true;
    setData(null);
    setError("");
    if (version)
      api
        .versionDetails(component.component_id, version.version_id)
        .then((result) => {
          if (
            result.version_id !== version.version_id ||
            result.wasm_sha256 !== version.wasm_sha256
          )
            throw new Error(
              "バージョンの情報が一致しません。「更新」で再取得してください。",
            );
          if (current) setData(result);
        })
        .catch((error) => {
          if (current) setError(errorMessage(error));
        });
    return () => {
      current = false;
    };
  }, [api, component, version]);
  if (!versions.length)
    return (
      <Empty title="バージョンはまだありません">
        CLI からデプロイしてください。
      </Empty>
    );
  const metadata = data?.build_metadata;
  const direct =
    metadata?.extensions.filter((e) => metadata.roots.includes(e.name)) || [];
  const dependencies =
    metadata?.extensions.filter((e) => !metadata.roots.includes(e.name)) || [];
  return (
    <section className="panel">
      <div className="panel-toolbar">
        <div>
          <h2>拡張の構成</h2>
          <p className="muted small">
            デプロイ時のビルド構成です。要求権限の記録だけでは、外部通信は許可されません。
          </p>
        </div>
      </div>
      <div className="extension-content">
        <label htmlFor="extension-version">確認するバージョン</label>
        <select
          id="extension-version"
          value={selected || ""}
          onChange={(event) => onSelect(event.target.value)}
        >
          {!version && (
            <option value={selected || ""}>バージョンを選択してください</option>
          )}
          {versions.map((v) => (
            <option key={v.version_id} value={v.version}>
              {versionLabel(v.version)}
              {v.version_id === component.active_version_id ? "（現在）" : ""}
            </option>
          ))}
        </select>
        {error ? (
          <Notice error>{error}</Notice>
        ) : !version ? (
          <Notice>確認するバージョンを選択してください。</Notice>
        ) : !data ? (
          <Notice>拡張の構成を読み込み中…</Notice>
        ) : (
          <>
            {metadata == null ? (
              <Notice>
                構成情報が未記録です。対応する CLI
                でビルドして再デプロイすると記録されます。
              </Notice>
            ) : (
              <>
                {metadata.input === "component" && (
                  <Notice>
                    CLI で追加した拡張を表示しています。元の Wasm
                    内部の構成は含みません。
                  </Notice>
                )}
                <h3>直接指定した拡張（{direct.length}）</h3>
                {direct.length ? (
                  <ExtensionList extensions={direct} />
                ) : (
                  <p className="muted">指定された拡張はありません。</p>
                )}
                {dependencies.length > 0 && (
                  <details className="extension-dependencies">
                    <summary>依存する拡張（{dependencies.length}）</summary>
                    <ExtensionList extensions={dependencies} />
                  </details>
                )}
              </>
            )}
            <div className="extension-egress">
              <h3>外部通信</h3>
              {policyError ? (
                <Notice error>
                  通信先の適用元を取得できませんでした。{policyError}
                </Notice>
              ) : !policy ? (
                <p className="muted">通信先を読み込み中…</p>
              ) : policy.allow_outbound !== null ? (
                <>
                  <p className="muted small">アプリ共通の設定を適用中。</p>
                  {policy.allow_outbound.length ? (
                    <details className="extension-destination-details inline-help">
                      <summary>
                        許可された通信先（{policy.allow_outbound.length}件）
                      </summary>
                      <ul className="extension-destinations">
                        {policy.allow_outbound.map((host) => (
                          <li key={host}>
                            <code>{host}</code>
                          </li>
                        ))}
                      </ul>
                    </details>
                  ) : (
                    <p className="muted">なし（外部通信は拒否）</p>
                  )}
                </>
              ) : (
                <>
                  <p className="muted small">
                    このバージョンの旧設定を適用中。アプリ共通の設定は未設定です。
                  </p>
                  {data.net_allow_outbound.length ? (
                    <ul className="extension-destinations">
                      {data.net_allow_outbound.map((host) => (
                        <li key={host}>
                          <code>{host}</code>
                        </li>
                      ))}
                    </ul>
                  ) : (
                    <p className="muted">なし（外部通信は拒否）</p>
                  )}
                </>
              )}
              {onOpenSettings && (
                <Button variant="text" size="sm" onClick={onOpenSettings}>
                  通信先の設定を開く
                </Button>
              )}
            </div>
          </>
        )}
      </div>
    </section>
  );
}
