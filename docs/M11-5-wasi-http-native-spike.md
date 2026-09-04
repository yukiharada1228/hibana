# M11-5: wasi:http/incoming-handler ネイティブ実行

> **状態: 実装済み**（本ブランチ）。SDK は native ビルドへ、worker は dual-path（native/bytes）へ、
> validation は depth-0 import 収集へ。以下はスパイク時の検証記録＋設計根拠。

## スパイク結論（実装の土台）

## 問い

`sdk/src/adapter.js`（Hono の `fetch` ↔ 基盤の `handle(bytes)->bytes` を HTTP エンベロープ
JSON で橋渡し）は、基盤が **bytes world** だから必要になっているグルーである。
より素な設計 —— コンポーネントが **`wasi:http/incoming-handler` を直接 export** し、
StarlingMonkey が Hono の `fetch` をそこへ自動配線する —— は、この基盤で実現できるか？

## 結論: **実現できる。end-to-end で実機確認済み。** adapter.js / エンベロープ / base64 は消える。

- **ビルド**: Hono を `wasi:http/incoming-handler@0.2.3` を export する component にできた。
  imports は **承認済み baseline のみ**（io / clocks / random / http/types / cli / filesystem, 全 0.2.3）。
  **`wasi:http/outgoing-handler`（egress）は import されない** → egress は M9c のまま到達不能。
- **実行**: worker と同一の **wasmtime-wasi-http 29** の `ProxyPre` で駆動し、実 HTTP 応答が返った
  （`crates/worker/tests/http_native_spike.rs`）。
  - 最小 fixture: `GET /` → `200` `"hi from incoming-handler"`
  - Hono fixture: `GET /` → `200` `"native wasi:http Hono 🔥"`（`app.get("/")` の実応答）

## ビルドの正しいレシピ（重要な落とし穴つき）

StarlingMonkey が fetch イベント→incoming-handler を配線するのは、**targetする world の
wasi:http バージョンによって使われる componentize-js エンジンが変わる**ため、版合わせが要る。

- jco 1.32.1 は `wasi:http < 0.2.10` の world を検出すると **componentize-js 0.19.3** に fallback し、
  こちらが `incoming-handler` を正しく生やす。
- `0.2.10` / `0.2.12`（jco 同梱の builtin WIT）を target すると **0.22.0** エンジンが使われ、
  `incoming-handler#handle` の export が生成されず失敗する。
- **host（worker / wasmtime-wasi-http 29）は `wasi:http@0.2.3` を実装**している。
  → **0.2.3 の WIT を使う**のが版的にも最適（host 一致 & 0.19.3 fallback を誘発）。
  0.2.3 の WIT は `wasmtime-wasi-http-29.0.1` crate の `wit/` にある（`deps/http/proxy.wit` 他）。

手順:
1. `shim.js`: `import app from "./app"; addEventListener("fetch", e => e.respondWith(app.fetch(e.request)))`
   （adapter.js の代替。**これだけ**。envelope も base64 も不要）
2. esbuild で `shim + app + hono` を 1本の ESM に。
3. world: `package hibana:app; world http { export wasi:http/incoming-handler@0.2.3; }`
   + `deps/` に 0.2.3 の wasi WIT 一式。
4. `jco componentize bundle.js --wit <witdir> --world-name http --disable http -o out.wasm`
   - `--disable http` が **outgoing-handler（egress）を落とす**。fetch-event（incoming）は残る。
   - `< 0.2.10` を target しているので自動的に componentize-js 0.19.3 が使われる。

なお wasmtime は 0.2.x のマイナー跨ぎを許容する（既存の bytes-world component は
`wasi:http/types@0.2.10` を import しつつ 0.2.3 host で動いている）。0.2.3 export も同 host で動作。

## 本実装（M11-5）でやること

| 面 | 変更 |
|---|---|
| SDK | build を incoming-handler world + fetch shim へ切替。0.2.3 WIT を同梱。**adapter.js を削除**。 |
| worker | **第2の実行パス**を追加。load 時に export が `wasi:http/incoming-handler` か `faas:component/handler`（bytes）かを判定し、前者は `ProxyPre` で駆動。invoke input の HTTP エンベロープ→`IncomingRequest`、`ResponseOutparam`→result bytes。**エンベロープは JS から host(Rust) へ移るだけでユーザには不可視**。 |
| validation | incoming-handler の **export** を許可（今は `handle` export 前提）。import 許可リストは不変（全て承認済み・outgoing-handler なし）。 |
| gateway | native component には実 HTTP をほぼ素通しで渡す。 |

## トレードオフ

- **得**: adapter.js / エンベロープ / base64 が消える。標準的（`jco serve` 等と同じ）。言語非依存
  （Rust/Go の HTTP フレームワークも同じ口）。往復コピー減。
- **払う**: worker に **実行 world が 2本**（HTTP native と bytes）増える恒久的な複雑さ。
  bytes world は cron / event / chain の統一起動のために残す（HTTP 以外は HTTP リクエストが無い）。

## 現状

スパイクの harness（`crates/worker/tests/http_native_spike.rs` + worker の dev-deps）は本ブランチに
コミット済み。`HIBANA_HTTP_SPIKE_WASM` に component パスを渡すと再現できる（未設定は skip）。
bytes-world 版（PR #15）は無傷。
