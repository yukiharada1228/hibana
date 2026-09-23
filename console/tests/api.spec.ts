import { expect, test } from "@playwright/test";
import { Api, ApiError } from "../src/api";

test("malformed and interrupted successful responses hide private data without retrying mutations", async () => {
  const original = globalThis.fetch;
  try {
    for (const interrupted of [false, true]) {
      let calls = 0;
      globalThis.fetch = async () => {
        calls += 1;
        return new Response(
          interrupted
            ? new ReadableStream({
                start(controller) {
                  controller.error(new Error("fixture-private-response"));
                },
              })
            : "fixture-private-response",
          { status: 200 },
        );
      };
      const api = new Api();
      try {
        await expect(api.rollback("app", "v1")).rejects.toMatchObject({
          message:
            "基盤からの応答を読み取れませんでした。操作中だった場合は、再実行の前に状態を確認してください。",
        });
        expect(calls).toBe(1);
      } finally {
        api.close();
      }
    }
  } finally {
    globalThis.fetch = original;
  }
});

test("HTTP errors release unfinished bodies and retain their status even if cancellation fails", async () => {
  const original = globalThis.fetch;
  try {
    for (const failCancellation of [false, true]) {
      let cancelled = 0;
      let calls = 0;
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(new TextEncoder().encode("fixture-private-error"));
          // The server never closes this body.
        },
        cancel() {
          cancelled += 1;
          if (failCancellation) throw new Error("fixture-cancel-error");
        },
      });
      globalThis.fetch = async () => {
        calls += 1;
        return new Response(body, { status: 503 });
      };
      const api = new Api();
      try {
        await expect(api.components()).rejects.toMatchObject({
          constructor: ApiError,
          status: 503,
          message:
            "基盤が処理を受け付けられません。しばらく待ってから状態を確認してください。",
        });
        expect(cancelled).toBe(1);
        expect(calls).toBe(1);
      } finally {
        api.close();
      }
    }
  } finally {
    globalThis.fetch = original;
  }
});
