import { useState } from "react";
import type { Session } from "./types";
import { Button } from "./components/ui/button";
import { Notice } from "./components/common";
import { version } from "../package.json";
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

export function Deploy({
  session,
  email,
}: {
  session: Session;
  email: string;
}) {
  const [copied, setCopied] = useState("");
  const cli = `npx --yes @yukiharada1228/hibana@${version}`;
  const login = `${cli} login \\\n  --url ${quote(`${location.origin}/api`)} \\\n  --tenant ${quote(session.tenant_slug)} \\\n  --email ${quote(email)}`;
  const commands = [
    {
      title: "この基盤にログイン",
      description:
        "Password: と表示されたらパスワードを入力します。入力した文字は表示されません。接続先は保存され、次回から指定を省略できます。",
      command: login,
    },
    {
      title: "アプリを作成",
      description:
        "Hono アプリを作成します。既存の Hibana プロジェクトがあれば、そのディレクトリへ移動してください。",
      command: `${cli} init hello\ncd hello`,
    },
    {
      title: "デプロイ",
      description: "PC でビルドした Wasm が、この基盤へアップロードされます。",
      command: "npm run deploy",
    },
  ];
  return (
    <>
      <div className="page-title">
        <div>
          <h1>CLI の接続</h1>
          <p className="muted">
            Node.js 24 以上がある PC から接続できます。CLI は npx
            が取得するため、事前のインストールは不要です。
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
              {copied === step.title && (
                <span role="status" className="small">
                  {" "}
                  コピーしました
                </span>
              )}
            </div>
          </section>
        ))}
      </div>
      {copied && !commands.some((c) => c.title === copied) && (
        <Notice>{copied}</Notice>
      )}
    </>
  );
}
