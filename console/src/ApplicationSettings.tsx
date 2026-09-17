import { useEffect, useState } from "react";
import { Api, errorMessage } from "./api";
import type { Component, Settings } from "./types";
import { bytes, Notice } from "./components/common";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "./components/ui/table";

export function ApplicationSettings({
  api,
  component,
}: {
  api: Api;
  component: Component;
}) {
  const [data, setData] = useState<Settings | null>(null),
    [error, setError] = useState("");
  useEffect(() => {
    let current = true;
    setData(null);
    setError("");
    api
      .config(component.component_id)
      .then((result) => {
        if (current) setData(result);
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      });
    return () => {
      current = false;
    };
  }, [api, component]);
  if (error) return <Notice error>{error}</Notice>;
  if (!data) return <Notice>設定を読み込み中…</Notice>;
  if (!data.version_id)
    return <Notice>配備されたバージョンはありません。</Notice>;
  if (data.version_id !== component.active_version_id)
    return (
      <Notice>
        配備が変更されました。「更新」で現在の設定を取得してください。
      </Notice>
    );
  return (
    <div className="settings-sections">
      <section className="panel">
        <div className="panel-toolbar">
          <div>
            <h2>環境変数</h2>
            <p className="muted small">
              変更は hibana.json の vars に記述し、再デプロイします。
            </p>
          </div>
        </div>
        {Object.keys(data.env).length ? (
          <div className="table-scroll">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>名前</TableHead>
                  <TableHead>値</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {Object.entries(data.env).map(([name, value]) => (
                  <TableRow key={name}>
                    <TableCell>
                      <code>{name}</code>
                    </TableCell>
                    <TableCell className="env-value">{value}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        ) : (
          <p className="panel-text muted">環境変数はありません。</p>
        )}
      </section>
      <section className="panel">
        <div className="panel-toolbar">
          <div>
            <h2>Secret の参照</h2>
            <p className="muted small">
              現在のバージョンが参照する Secret です。値は表示しません。
            </p>
          </div>
        </div>
        {data.secrets.length ? (
          <div className="table-scroll">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>名前</TableHead>
                  <TableHead>状態</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.secrets.map((secret) => (
                  <TableRow key={secret.name}>
                    <TableCell>
                      <code>{secret.name}</code>
                    </TableCell>
                    <TableCell>
                      {secret.available
                        ? "参照先あり"
                        : "参照先が削除されています"}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        ) : (
          <p className="panel-text muted">Secret の参照はありません。</p>
        )}
      </section>
      <section className="panel">
        <div className="panel-toolbar">
          <h2>実行設定</h2>
        </div>
        <dl className="settings-list">
          <dt>メモリ上限</dt>
          <dd>
            {data.resource_limits
              ? bytes(data.resource_limits.max_memory_bytes)
              : "—"}
          </dd>
          <dt>Wasm の実行時間上限</dt>
          <dd>
            {data.resource_limits
              ? `${data.resource_limits.max_wall_time_ms} ms`
              : "—"}
          </dd>
          <dt>通信を含む実行時間上限</dt>
          <dd>
            {data.resource_limits
              ? `${data.resource_limits.max_execution_time_ms} ms`
              : "—"}
          </dd>
        </dl>
      </section>
    </div>
  );
}
