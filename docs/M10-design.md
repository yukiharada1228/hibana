# M10 設計: 可観測性の完成（分散トレーシング）

仕様書 §15 M10 / §3.8 / §10。

> **完了条件（M10 のうち本スライスが担う部分）**: `POST /invoke` → JetStream → worker 実行 →
> `.result` → subscriber 終端、の経路が**1 本のトレースに end-to-end で繋がって追える**。

M10 は「可観測性の完成 / Multi Region」の 2 本立てだが、**Multi Region は需要発火型**（地理分散
要求が顕在化した時点で着手）であり、本設計では扱わない。ここで実装するのは **分散トレーシング
（OpenTelemetry）** のみ。仕様書 §15 が「WASM + NATS 経路はブラックボックス化しやすく高レバレッジの
ため前倒し推奨」と述べている、その前倒しである。

---

## 1. 現状（着手前の事実）

- ログは `tracing` + `tracing-subscriber`（`fmt`。`LOG_FORMAT=json` で JSON 化, M4a）。
  span は張られており（例: `subscriber.handle_message{execution_id, tenant_id}`）、
  **`execution_id` を correlation ID として全ログに付与**する運用が既にある（§3.8）。
- **OpenTelemetry は無い**。トレースの概念（trace_id / span 親子 / サービス間伝搬）は未導入。
- CP → worker は JetStream 経由で、publish 時に **NATS ヘッダ**を付けている
  （`enqueue.rs` の `Nats-Msg-Id=execution_id`）。ここが**サービス間の trace context を運ぶ器**になる。
- 同期 invoke の reply は Core NATS の correlation subject（`subscriber.rs`）。

つまり「correlation ID で人間が grep して追う」ことはできるが、「1 リクエストの因果を機械が
1 トレースに束ねる」ことはできない。M10 はその 1 本を通す。

---

## 2. 何を作るか（3 スライス）

### 2.1 M10-1: OTel 配線（opt-in・既定 off の no-op）

- 依存を追加: `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp` / `tracing-opentelemetry`。
  **exporter は HTTP/protobuf（reqwest ベース）**を選ぶ。理由: 本リポジトリは reqwest + rustls で
  C 依存（cc / aws-lc）を避ける方針（Cargo.toml のコメント参照）。gRPC exporter（tonic/prost）は
  重い依存を持ち込むので採らない。
- **有効化は `OTEL_EXPORTER_OTLP_ENDPOINT`（OTel 標準 env）が設定されているときだけ**。
  未設定なら OTel layer を一切足さず、`init_tracing` は現在と 1 ビットも変わらない（既存の
  fmt/json ログのみ）。これが「既定で挙動不変」を保証する唯一の条件。
- `init_tracing` を `fmt().init()` ショートカットから **`Registry` + layer 合成**へ書き換える:
  `registry().with(fmt_layer).with(otel_layer?)`。fmt 側は現状と同じ設定を維持する。
- サービス名は `service.name` リソースで CP / worker を区別（`faas-control-plane` / `faas-worker`）。
- **batch exporter を使い、プロセス終了時に flush**（`shutdown_tracer_provider` 相当）。flush しないと
  短命プロセスやテストで span が送信されずに消える。init はガードを返し、main の終わりで drop/flush する。

### 2.2 M10-2: CP → worker の trace context 伝搬（W3C traceparent over NATS）

サービス境界（NATS）を跨いで trace を繋ぐのが本丸。W3C TraceContext（`traceparent` ヘッダ）を使う。

- **inject（CP 側, `enqueue.rs`）**: publish 直前、現在の span のコンテキストを
  `TraceContextPropagator` で `traceparent`（必要なら `tracestate`）へ書き出し、既存の
  `NatsHeaderMap` に足す。`Nats-Msg-Id` は触らない（冪等 layer 3 を壊さない）。
- **extract（worker 側, `lane_loop`）**: メッセージ処理の span を作る前に、NATS ヘッダから
  `traceparent` を取り出して親コンテキストにし、`handle_payload` の span をその子にする
  （`span.set_parent(cx)`）。これで **CP の invoke span → worker の実行 span** が 1 トレースになる。
- `async_nats::HeaderMap` と OTel の `Injector`/`Extractor` の間に薄いアダプタを 1 つ書く
  （HeaderMap は文字列 KV なのでアダプタは数行）。
- **result 経路（worker → CP subscriber）**も同様に伝搬できるが、`.result` は Core NATS publish
  （`publish_result`）。ここにも traceparent を載せ、subscriber の finalize span を子にすれば
  invoke → worker → result が 1 トレースで閉じる。最小実装は invoke→worker を先に通し、
  result 側は同じアダプタの再利用で足す。

### 2.3 M10-3: 検証（Jaeger + end-to-end + no-op 回帰）

- `docker-compose.yml` に **Jaeger all-in-one**（OTLP 受信: 4317/gRPC・4318/HTTP、UI: 16686）を追加。
  既存サービスと独立で、`OTEL_EXPORTER_OTLP_ENDPOINT` を設定したときだけ使う。
- 実機で `POST /invoke?wait=1` → Jaeger UI で **1 トレースに CP と worker の span が親子で並ぶ**ことを確認。
- **既定 off（`OTEL_EXPORTER_OTLP_ENDPOINT` 未設定）で、既存の gates と chaos が全緑**であること
  （no-op 回帰。OTel を足しても既存挙動が変わらないことの担保）。
- README / `.env.example` に有効化手順と「既定 off」を明記。

---

## 3. 設計上の判断（と却下案）

| 論点 | 判断 | 理由 |
| --- | --- | --- |
| exporter プロトコル | **HTTP/protobuf（reqwest）** | repo は C 依存回避（rustls / reqwest）。gRPC は tonic/prost を引き込む |
| 有効化 | **`OTEL_EXPORTER_OTLP_ENDPOINT` の有無** | OTel 標準。専用フラグを増やさない。未設定 = 完全 no-op |
| 伝搬フォーマット | **W3C TraceContext（traceparent）** | 業界標準。Jaeger/OTel collector がそのまま解釈 |
| 伝搬の器 | **NATS ヘッダ**（既存の publish ヘッダに相乗り） | JobMessage 本文を変えない（スキーマ互換を壊さない）。冪等 `Nats-Msg-Id` と共存 |
| span 送信方式 | **batch + 終了時 flush** | simple exporter は同期で hot path を汚す。短命プロセスの取りこぼしは flush で防ぐ |
| Multi Region | **本スライスでは扱わない** | 需要発火型（§15）。トレーシングだけで M10 の可観測性完了条件は満たせる |
| メトリクス（Prometheus）の OTel 化 | **やらない** | 既存の `/metrics`（M4a）は十分機能している。二重化はコスト過多 |

---

## 4. 非スコープ（M10 で「やらない」）

- **Multi Region**（需要発火型, §15）。
- Prometheus メトリクスの OTel 移行（既存の pull 型 `/metrics` を維持）。
- ログの OTel Logs への移行（構造化ログ + correlation ID を維持。trace_id をログにも出す程度に留める）。
- サンプリング戦略の作り込み（まずは常時サンプルで end-to-end を通す。本番のサンプリング率調整は運用時）。

---

## 5. 完了条件（本スライス）

1. `OTEL_EXPORTER_OTLP_ENDPOINT` を設定して invoke すると、Jaeger で **invoke → worker（→ result）が
   1 トレース**に繋がって見える。
2. 未設定なら OTel は一切動かず、**既存 gates（fmt/clippy/rls-lint/unit）と chaos が全緑**（no-op）。
3. CP と worker が別 `service.name` で区別でき、span に `execution_id` / `tenant_id` が乗る。
