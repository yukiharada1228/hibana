import { useState } from "react";
import type { Session } from "./types";
import { Button } from "./components/ui/button";
import { Notice } from "./components/common";
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

export function Deploy({
  session,
  email,
}: {
  session: Session;
  email: string;
}) {
  const [copied, setCopied] = useState("");
  const login = `hibana login --profile intranet \\\n  --url ${quote(`${location.origin}/api`)} \\\n  --tenant ${quote(session.tenant_slug)} \\\n  --email ${quote(email)}${session.ingress_base_domain ? ` \\\n  --ingress-domain ${quote(session.ingress_base_domain)}` : ""}`;
  const commands = [
    {
      title: "この基盤にログイン",
      description:
        "最新の hibana CLI を PC に導入し、以下のコマンドを実行してください。Password: と表示されたらパスワードを入力して Enter を押します。入力した文字は表示されません。",
      command: login,
    },
    {
      title: "アプリを作成",
      description:
        "Hono アプリを作成します。既存の Hibana プロジェクトがあれば、そのディレクトリへ移動してください。",
      command: "hibana init hello\ncd hello",
    },
    {
      title: "デプロイ",
      description: "PC でビルドした Wasm が、この基盤へアップロードされます。",
      command: "hibana deploy --profile intranet --version 1.0.0",
    },
  ];
  return (
    <>
      <div className="page-title">
        <div>
          <p className="eyebrow">DEPLOY</p>
          <h1>CLI からデプロイ</h1>
          <p className="muted">
            手元の PC から、この Hibana 基盤へ接続します。
          </p>
        </div>
      </div>
      {!session.scopes.includes("deploy") && (
        <Notice>
          現在のアカウントには Deploy
          権限がありません。配備には権限を持つアカウントを使用してください。
        </Notice>
      )}
      <div className="steps">
        {commands.map((step, i) => (
          <section className="panel step" key={step.title}>
            <div className="step-number">{i + 1}</div>
            <div>
              <h2>{step.title}</h2>
              <p className="muted">{step.description}</p>
              <pre>
                <code>{step.command}</code>
              </pre>
              <Button
                variant="outline"
                size="sm"
                onClick={async () => {
                  try {
                    await navigator.clipboard.writeText(step.command);
                    setCopied(step.title);
                  } catch {
                    setCopied(
                      "コピーできませんでした。コマンドを選択してコピーしてください。",
                    );
                  }
                }}
              >
                コマンドをコピー
              </Button>
            </div>
          </section>
        ))}
      </div>
      {copied && (
        <Notice>
          {commands.some((c) => c.title === copied)
            ? `「${copied}」のコマンドをコピーしました。`
            : copied}
        </Notice>
      )}
      <div className="info-strip">
        <div>
          <strong>デプロイ完了後は、基盤がアプリを実行・配信します。</strong>
          <p>PC を閉じても、配備したアプリは基盤側で動き続けます。</p>
        </div>
        <a href="#apps">アプリ一覧へ →</a>
      </div>
    </>
  );
}
