import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type { Component, Execution, Version } from "./types";
import { date, versionLabel } from "./components/common";
import { executionFailed, executionResult } from "./lib/execution";

export function ApplicationActivity({
  api,
  component,
  versions,
  onOpen,
}: {
  api: Api;
  component: Component;
  versions: Version[];
  onOpen: () => void;
}) {
  const [latest, setLatest] = useState<Execution | null>();
  const [error, setError] = useState("");
  useEffect(() => {
    let current = true;
    api
      .executions(component.component_id, false)
      .then((result) => {
        if (current) {
          setLatest(result.items[0] || null);
          setError("");
        }
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      });
    return () => {
      current = false;
    };
  }, [api, component]);
  const version = versions.find(
    (v) => v.version_id === latest?.version_id,
  )?.version;
  return (
    <div className="application-activity" aria-label="最新の実行">
      <span className="muted">最新の実行（24時間以内）</span>
      {error ? (
        <span role="alert">{error}</span>
      ) : latest === undefined ? (
        <span>確認中…</span>
      ) : latest === null ? (
        <span>記録なし</span>
      ) : (
        <>
          <strong
            className={executionFailed(latest) ? "execution-failed" : undefined}
          >
            {executionResult(latest)}
          </strong>
          <span>{date(latest.created_at)}</span>
          {version && <span className="muted">{versionLabel(version)}</span>}
        </>
      )}
      <button type="button" onClick={onOpen}>
        実行履歴を見る
      </button>
    </div>
  );
}
