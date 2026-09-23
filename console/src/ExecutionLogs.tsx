import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type { ExecutionDetails } from "./types";
import { Notice } from "./components/common";
import { Button } from "./components/ui/button";

const display = (value: string) =>
  value.replace(
    /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g,
    "�",
  );

export function ExecutionLogs({
  api,
  executionId,
}: {
  api: Api;
  executionId: string;
}) {
  const [revision, setRevision] = useState(0);
  const [result, setResult] = useState<ExecutionDetails>();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!revision) return;
    let current = true;
    setBusy(true);
    setError("");
    setResult(undefined);
    api
      .execution(executionId)
      .then((value) => {
        if (current) setResult(value);
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      })
      .finally(() => {
        if (current) setBusy(false);
      });
    return () => {
      current = false;
    };
  }, [api, executionId, revision]);
  return (
    <div className="execution-logs">
      <Button
        variant="outline"
        size="sm"
        disabled={busy}
        onClick={() => setRevision((value) => value + 1)}
      >
        {busy
          ? "ログを取得中…"
          : revision
            ? "ログを再取得"
            : "アプリログを表示"}
      </Button>
      {error && <Notice error>{error}</Notice>}
      {result && (
        <>
          <p className="muted small">
            ログは実行完了後に取得でき、完了から24時間保存されます。
          </p>
          {!result.logs ? (
            <p>
              ログは取得できません。未収集、保存期限切れ、または実行中です。
            </p>
          ) : (
            <>
              {!result.logs.stdout && !result.logs.stderr && (
                <p>アプリからの出力はありません。</p>
              )}
              {(["stdout", "stderr"] as const).map(
                (stream) =>
                  result.logs?.[stream] && (
                    <div key={stream}>
                      <p>{stream === "stdout" ? "標準出力" : "標準エラー"}</p>
                      <pre
                        className="execution-error"
                        aria-label={
                          stream === "stdout"
                            ? "標準出力ログ"
                            : "標準エラーログ"
                        }
                      >
                        {display(result.logs[stream])}
                      </pre>
                    </div>
                  ),
              )}
              {result.logs.truncated && (
                <p role="status">
                  出力が上限（合計16KiB）を超えたため、ログの一部を省略しています。
                </p>
              )}
            </>
          )}
        </>
      )}
    </div>
  );
}
