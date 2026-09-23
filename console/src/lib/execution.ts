import type { Execution } from "../types";

export const runtimeStatuses: Record<string, string> = {
  pending: "受付済み",
  running: "実行中",
  succeeded: "実行完了",
  failed: "実行失敗",
  timeout: "タイムアウト",
  cancelled: "キャンセル",
};

export function executionResult(item: Execution): string {
  if (item.status === "succeeded" && item.http_status != null)
    return `HTTP ${item.http_status}`;
  return runtimeStatuses[item.status] || "不明";
}

export function executionFailed(item: Execution): boolean {
  return (
    ["failed", "timeout"].includes(item.status) ||
    (item.http_status ?? 0) >= 400
  );
}

export function executionError(item: Execution): string | null {
  const error = item.error;
  const message =
    typeof error === "string"
      ? error
      : error &&
          typeof error === "object" &&
          "message" in error &&
          typeof error.message === "string"
        ? error.message
        : null;
  if (message) return message.replace(/\s+/g, " ").slice(0, 160);
  if (item.status === "timeout") return "実行時間の上限を超えました。";
  if (item.status === "failed")
    return "実行に失敗しました。詳細を確認してください。";
  if ((item.http_status ?? 0) >= 400)
    return "アプリが HTTP エラーを返しました。";
  return null;
}
