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
  const [loaded, setData] = useState<Settings | null>(null),
    [error, setError] = useState("");
  const data =
    loaded?.version_id === component.active_version_id ? loaded : null;
  useEffect(() => {
    setData(null);
    setError("");
  }, [api, component.component_id, component.active_version_id]);
  useEffect(() => {
    let current = true;
    api
      .config(component.component_id)
      .then((result) => {
        if (!current) return;
        if (result.version_id !== component.active_version_id)
          throw new Error(
            "配備が変更されました。「更新」で現在の設定を取得してください。",
          );
        setData(result);
        setError("");
      })
      .catch((error) => {
        if (current) setError(errorMessage(error));
      });
    return () => {
      current = false;
    };
  }, [api, component]);
  const warning = error && (
    <Notice error>
      {error}
      {data && " 表示中の設定は前回取得した内容です。"}
    </Notice>
  );
  if (!data) return warning || <Notice>設定を読み込み中…</Notice>;
  if (!data.version_id)
    return (
      <>
        {warning}
        <Notice>配備されたバージョンはありません。</Notice>
      </>
    );
  return (
    <div className="settings-sections">
      {warning}
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
