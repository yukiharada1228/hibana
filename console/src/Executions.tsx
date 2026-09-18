import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type { Component, ExecutionsPage, Version } from "./types";
import { date, Empty, Notice, versionLabel } from "./components/common";
import { Button } from "./components/ui/button";
import {
  executionError,
  executionFailed,
  executionResult,
  runtimeStatuses,
} from "./lib/execution";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "./components/ui/table";

export function Executions({
  api,
  component,
  versions,
}: {
  api: Api;
  component: Component;
  versions: Version[];
}) {
  const [errorsOnly, setErrorsOnly] = useState(false),
    [cursor, setCursor] = useState<string>();
  const [data, setData] = useState<ExecutionsPage | null>(null),
    [error, setError] = useState("");
  useEffect(() => {
    setCursor(undefined);
  }, [component.component_id]);
  useEffect(() => {
    // Clear only when the requested page changes. Background refreshes keep
    // the same keyed rows mounted, including their open details and focus.
    setData(null);
  }, [api, component.component_id, errorsOnly, cursor]);
  useEffect(() => {
    let current = true;
    setError("");
    api
      .executions(component.component_id, errorsOnly, cursor)
      .then((result) => {
        if (current) setData(result);
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      });
    return () => {
      current = false;
    };
  }, [api, component, errorsOnly, cursor]);
  return (
    <section className="panel">
      <div className="panel-toolbar">
        <div>
          <h2>直近24時間の実行</h2>
          <p className="muted small">
            新しい順に20件表示します。HTTP
            応答とランタイムの結果を確認できます。
          </p>
        </div>
        <label
          className="execution-filter"
          title="HTTP 4xx・5xx、実行失敗、タイムアウトを表示"
        >
          <input
            type="checkbox"
            checked={errorsOnly}
            onChange={(e) => {
              setCursor(undefined);
              setErrorsOnly(e.target.checked);
            }}
          />
          HTTPエラー・実行失敗のみ
        </label>
      </div>
      {error && (
        <Notice error>
          {error}
          {data && " 表示中の履歴は前回取得した内容です。"}
        </Notice>
      )}
      {!data ? (
        !error && <Notice>実行履歴を読み込み中…</Notice>
      ) : !data.items.length ? (
        <Empty
          title={
            errorsOnly ? "該当するエラーはありません" : "実行記録はありません"
          }
        >
          保存されている直近24時間の記録が対象です。
        </Empty>
      ) : (
        <div className="table-scroll">
          <Table className="execution-table">
            <TableHeader>
              <TableRow>
                <TableHead>日時</TableHead>
                <TableHead>結果</TableHead>
                <TableHead>バージョン</TableHead>
                <TableHead>実行時間</TableHead>
                <TableHead>エラー・詳細</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {data.items.map((item) => {
                const version = versions.find(
                  (v) => v.version_id === item.version_id,
                )?.version;
                return (
                  <TableRow key={item.execution_id}>
                    <TableCell className="nowrap" data-label="日時">
                      {date(item.created_at)}
                    </TableCell>
                    <TableCell data-label="結果" className="nowrap">
                      <strong
                        className={
                          executionFailed(item) ? "execution-failed" : undefined
                        }
                      >
                        {executionResult(item)}
                      </strong>
                      {item.status === "succeeded" &&
                        item.http_status == null && (
                          <div className="muted small">HTTP 応答は未記録</div>
                        )}
                    </TableCell>
                    <TableCell title={version} data-label="バージョン">
                      {version ? versionLabel(version) : "削除済み"}
                    </TableCell>
                    <TableCell className="nowrap" data-label="実行時間">
                      {item.wall_time_ms === null
                        ? "—"
                        : `${item.wall_time_ms} ms`}
                    </TableCell>
                    <TableCell data-label="詳細">
                      {executionError(item) && (
                        <p className="execution-message">
                          {executionError(item)}
                        </p>
                      )}
                      <details>
                        <summary>実行の詳細</summary>
                        <p>
                          ランタイム：{runtimeStatuses[item.status] || "不明"}
                        </p>
                        <p>HTTP 応答：{item.http_status ?? "未記録"}</p>
                        {item.error != null && (
                          <pre className="execution-error">
                            {JSON.stringify(item.error, null, 2)}
                          </pre>
                        )}
                        <p className="hash">
                          実行 ID <code>{item.execution_id}</code>
                        </p>
                        {version && (
                          <p className="hash">
                            完全なバージョン <code>{version}</code>
                          </p>
                        )}
                      </details>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </div>
      )}
      {(cursor || data?.next_cursor) && (
        <div className="panel-toolbar">
          <Button
            variant="outline"
            size="sm"
            disabled={!cursor}
            onClick={() => setCursor(undefined)}
          >
            最新に戻る
          </Button>
          <Button
            variant="outline"
            size="sm"
            disabled={!data?.next_cursor}
            onClick={() => setCursor(data?.next_cursor || undefined)}
          >
            次の20件
          </Button>
        </div>
      )}
    </section>
  );
}
