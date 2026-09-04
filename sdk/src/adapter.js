// Hibana Hono アダプタ (M11-2, §4.2)
//
// プラットフォームの world は同期の bytes-in / bytes-out:
//   handle: func(input: list<u8>) -> result<list<u8>, handler-error>
// 一方 Hono は Web 標準の `app.fetch(Request) -> Promise<Response>`。
// この差を「HTTP エンベロープ JSON」で橋渡しする。
//
//   入力バイト  = JSON.stringify(RequestEnvelope) の UTF-8
//   出力バイト  = JSON.stringify(ResponseEnvelope) の UTF-8
//
// RequestEnvelope  = { method, path, query?, headers?, body?, bodyBase64? }
// ResponseEnvelope = { status, headers, body, bodyBase64 }
//
// body は既定で UTF-8 文字列。バイナリの場合のみ base64 とし bodyBase64:true を立てる
// （入力・出力とも同じ規約）。これにより JSON API は body がそのまま読める形になり、
// バイナリ（画像等）も欠損なく往復できる。
//
// handle は WIT 上は同期だが、実装は async にできる（StarlingMonkey が
// event loop を完了まで pump する。実機検証済み）。Hono の app.fetch は
// Promise を返すため、この async 性が必須である。

// **base64url（パディングなし）**。プラットフォームの Rust 側 `faas_shared::b64url_*`
// と同一スキーム（url-safe `-_`・`=` 無し）。ingress gateway が Rust で往復するため一致必須。
const B64 =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

function bytesToBase64(bytes) {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i];
    const b1 = i + 1 < bytes.length ? bytes[i + 1] : 0;
    const b2 = i + 2 < bytes.length ? bytes[i + 2] : 0;
    out += B64[b0 >> 2];
    out += B64[((b0 & 3) << 4) | (b1 >> 4)];
    if (i + 1 < bytes.length) out += B64[((b1 & 15) << 2) | (b2 >> 6)];
    if (i + 2 < bytes.length) out += B64[b2 & 63];
  }
  return out;
}

function base64ToBytes(str) {
  const clean = str.replace(/[^A-Za-z0-9\-_]/g, "");
  const len = Math.floor((clean.length * 3) / 4);
  const out = new Uint8Array(len);
  let p = 0;
  for (let i = 0; i < clean.length; i += 4) {
    const c0 = B64.indexOf(clean[i]);
    const c1 = B64.indexOf(clean[i + 1]);
    const c2 = B64.indexOf(clean[i + 2]);
    const c3 = B64.indexOf(clean[i + 3]);
    out[p++] = (c0 << 2) | (c1 >> 4);
    if (i + 2 < clean.length && c2 >= 0) out[p++] = ((c1 & 15) << 4) | (c2 >> 2);
    if (i + 3 < clean.length && c3 >= 0) out[p++] = ((c2 & 3) << 6) | c3;
  }
  return out.subarray(0, p);
}

// bytes が妥当な UTF-8 なら文字列を、そうでなければ null を返す（バイナリ判定）。
function tryUtf8(bytes) {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}

// Hono app（app.fetch を持つオブジェクト）を受け取り、プラットフォームの
// handle(list<u8>) -> list<u8> を実装する async 関数を返す。
export function createHandle(app) {
  return async function handle(input) {
    const inBytes =
      input instanceof Uint8Array ? input : new Uint8Array(input);
    let env;
    try {
      env = JSON.parse(new TextDecoder().decode(inBytes));
    } catch (e) {
      throw { tag: "invalid-input", val: `request envelope is not JSON: ${e}` };
    }

    const method = String(env.method || "GET").toUpperCase();
    const path = env.path || "/";
    const query = env.query || "";
    // オリジンは任意。Hono は path/method/headers を見るため localhost 固定で十分。
    const url = "http://hibana.local" + path + query;

    const headers = new Headers();
    if (env.headers && typeof env.headers === "object") {
      for (const [k, v] of Object.entries(env.headers)) {
        if (v != null) headers.set(k, String(v));
      }
    }

    let body;
    if (env.body != null && method !== "GET" && method !== "HEAD") {
      body = env.bodyBase64 ? base64ToBytes(env.body) : env.body;
    }

    let res;
    try {
      res = await app.fetch(new Request(url, { method, headers, body }));
    } catch (e) {
      throw { tag: "runtime", val: `handler threw: ${e && e.stack ? e.stack : e}` };
    }

    const outHeaders = {};
    res.headers.forEach((v, k) => {
      outHeaders[k] = v;
    });

    const respBytes = new Uint8Array(await res.arrayBuffer());
    const asText = tryUtf8(respBytes);
    const outEnv =
      asText !== null
        ? { status: res.status, headers: outHeaders, body: asText, bodyBase64: false }
        : {
            status: res.status,
            headers: outHeaders,
            body: bytesToBase64(respBytes),
            bodyBase64: true,
          };

    return new TextEncoder().encode(JSON.stringify(outEnv));
  };
}
