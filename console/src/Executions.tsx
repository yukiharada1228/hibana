import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type { Component, ExecutionsPage, Version } from "./types";
import { date, Empty, Notice, versionLabel } from "./components/common";
import { Button } from "./components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "./components/ui/table";

const statuses: Record<string, string> = {
  pending: "受付済み",
  running: "実行中",
  succeeded: "成功",
  failed: "失敗",
  timeout: "タイムアウト",
  cancelled: "キャンセル",
};

export function Executions({
  api,
  component,
  versions,
}: {
  api: Api;
  component: Component;
  versions: Version[];
}) {
  const [errorsOnly, setErrorsOnly] = useState(true),
    [cursor, setCursor] = useState<string>();
  const [data, setData] = useState<ExecutionsPage | null>(null),
    [error, setError] = useState("");
  useEffect(() => {
    setCursor(undefined);
  }, [component.component_id]);
  useEffect(() => {
    let current = true;
    setData(null);
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
            新しい順に20件表示します。ランタイムの実行結果で、HTTP
            ステータスやアプリのログは含みません。
          </p>
        </div>
        <label className="execution-filter">
          <input
            type="checkbox"
            checked={errorsOnly}
            onChange={(e) => {
              setCursor(undefined);
              setErrorsOnly(e.target.checked);
            }}
          />
          失敗・タイムアウトのみ
        </label>
      </div>
      {error ? (
        <Notice error>{error}</Notice>
      ) : !data ? (
        <Notice>実行履歴を読み込み中…</Notice>
      ) : !data.items.length ? (
        <Empty
          title={
            errorsOnly
              ? "該当する実行エラーはありません"
              : "実行記録はありません"
          }
        >
          保存されている直近24時間の記録が対象です。
        </Empty>
      ) : (
        <div className="table-scroll">
          <Table>
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
                    <TableCell className="nowrap">
                      {date(item.created_at)}
                    </TableCell>
                    <TableCell>{statuses[item.status] || "不明"}</TableCell>
                    <TableCell title={version}>
                      {version ? versionLabel(version) : "削除済み"}
                    </TableCell>
                    <TableCell className="nowrap">
                      {item.wall_time_ms === null
                        ? "—"
                        : `${item.wall_time_ms} ms`}
                    </TableCell>
                    <TableCell>
                      {item.error != null && (
                        <pre className="execution-error">
                          {JSON.stringify(item.error, null, 2)}
                        </pre>
                      )}
                      <details>
                        <summary>実行の詳細</summary>
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
