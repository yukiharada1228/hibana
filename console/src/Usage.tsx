import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type { Component, Usage as UsageData } from "./types";
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
import { bytes, Empty, Notice, number } from "./components/common";

const today = () => new Date().toISOString().slice(0, 10);
const monthAgo = () =>
  new Date(Date.now() - 30 * 86400000).toISOString().slice(0, 10);
export function Usage({
  api,
  components,
}: {
  api: Api;
  components: Component[];
}) {
  const [range, setRange] = useState({ from: monthAgo(), to: today() });
  const [data, setData] = useState<UsageData | null>(null),
    [error, setError] = useState("");
  useEffect(() => {
    let current = true;
    setData(null);
    setError("");
    api
      .usage(range.from, range.to)
      .then((result) => {
        if (current) setData(result);
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      });
    return () => {
      current = false;
    };
  }, [api, range, components]);
  return (
    <>
      <div className="page-title">
        <div>
          <h1>利用状況</h1>
          <p className="muted">
            ランタイムの実行量です。HTTP 応答の成否とは異なります。集計は UTC
            日付です。
          </p>
        </div>
      </div>
      <form
        className="range"
        onSubmit={(event) => {
          event.preventDefault();
          const form = new FormData(event.currentTarget);
          const from = String(form.get("from")),
            to = String(form.get("to"));
          if (from > to) {
            setError("開始日は終了日以前を指定してください。");
            return;
          }
          setRange({ from, to });
        }}
      >
        <label>
          開始日
          <Input name="from" type="date" required defaultValue={range.from} />
        </label>
        <label>
          終了日
          <Input name="to" type="date" required defaultValue={range.to} />
        </label>
        <Button type="submit" variant="outline">
          集計する
        </Button>
      </form>
      {error && <Notice error>{error}</Notice>}
      {!data && !error && <Notice>利用量を読み込み中…</Notice>}
      {data && (
        <>
          <div className="stats">
            <div>
              <span>実行回数</span>
              <strong>
                {number(data.totals.invocation_count)}
                <small>回</small>
              </strong>
            </div>
            <div>
              <span>失敗 / タイムアウト</span>
              <strong>
                {number(data.totals.failed_count)}
                <small>/ {number(data.totals.timeout_count)}</small>
              </strong>
            </div>
            <div>
              <span>累計実行時間</span>
              <strong>
                {number(data.totals.wall_time_ms)}
                <small>ms</small>
              </strong>
            </div>
          </div>
          <section className="panel">
            <div className="panel-toolbar">
              <h2>アプリ別の利用量</h2>
              <span className="muted small">
                {data.from} — {data.to} UTC
              </span>
            </div>
            {!data.by_component.length ? (
              <Empty title="この期間の実行記録はありません">
                配備したアプリにアクセスすると、実行完了後に利用量が集計されます。
              </Empty>
            ) : (
              <div className="table-scroll">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>アプリ</TableHead>
                      <TableHead>実行回数</TableHead>
                      <TableHead>成功</TableHead>
                      <TableHead>失敗</TableHead>
                      <TableHead>タイムアウト</TableHead>
                      <TableHead>出力量</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {data.by_component.map((row) => (
                      <TableRow key={row.component_id}>
                        <TableCell>
                          <a href={`#apps/${row.component_id}`}>
                            {components.find(
                              (c) => c.component_id === row.component_id,
                            )?.name || row.component_id}
                          </a>
                        </TableCell>
                        <TableCell>{number(row.invocation_count)}</TableCell>
                        <TableCell>{number(row.succeeded_count)}</TableCell>
                        <TableCell>{number(row.failed_count)}</TableCell>
                        <TableCell>{number(row.timeout_count)}</TableCell>
                        <TableCell>{bytes(row.output_bytes)}</TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            )}
          </section>
        </>
      )}
    </>
  );
}
