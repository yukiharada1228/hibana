# アーキテクチャ（WASM FaaS Platform）

> 基準: ブランチ `feat/m6`（実体は **M5: 課金・メータリング基盤** 時点）、2026-06-25。
> この図はコードベース（`crates/*` / `migrations/*` / `wit/*`）を唯一の正として起こしている。
> 既存の説明文と食い違う場合はコードを信じること。

対象は 3 つの成果物（`control-plane` / `worker` / `components/echo`）と、4 つの依存ミドルウェア
（PostgreSQL / NATS JetStream / Redis / MinIO）。粒度別に 3 枚用意した。

- [25% — コンテキスト図](#25--コンテキスト図全体像) … 新規参加者・発表向け
- [100% — コンポーネント / データフロー図](#100--コンポーネント--データフロー図) … 実装者・レビュアー向け
- [invoke→result シーケンス図（本番 default 経路）](#invokeresult-シーケンス図本番-default-経路)

---

## 25% — コンテキスト図（全体像）

```mermaid
flowchart TD
  client["Client"]
  cp["control-plane<br>(REST API / 認証・認可 / 検証 / 署名 / 終端 writer)"]
  worker["worker<br>(Wasmtime 実行 + 計量)"]
  comp["Component (echo)<br>WASM Component"]

  pg[("PostgreSQL<br>状態 + 監査 + 利用量集計")]
  minio[("MinIO (S3 互換)<br>wasm 本体 / 大容量 I/O")]
  redis[("Redis<br>共有 admission ストア")]
  nats["NATS JetStream<br>invoke / result / failed"]

  client -->|"HTTP/REST"| cp
  cp -->|"invoke (JobMessage)"| nats
  nats -->|"pull consumer"| worker
  worker -->|"result / failed"| nats
  nats -->|"終端結果"| cp
  worker --> comp

  cp --- pg
  cp --- redis
  cp <-->|"put / presigned URL"| minio
  worker -->|"HTTP GET 本体"| minio

  classDef svc fill:#cfe8ff,stroke:#1f6feb,color:#0b2942;
  classDef store fill:#d6f5dd,stroke:#1a7f37,color:#06281a;
  classDef broker fill:#e7d9fb,stroke:#7b46c9,color:#23104a;
  classDef guest fill:#fff3c4,stroke:#b08800,color:#3d2f00;
  classDef ext fill:#eaeaea,stroke:#777,color:#222,stroke-dasharray:4 3;

  class cp,worker svc;
  class pg,minio,redis store;
  class nats broker;
  class comp guest;
  class client ext;
```

---

## 100% — コンポーネント / データフロー図

`control-plane`（axum 単一バイナリ。内部に複数の非同期タスク）と `worker` の責務分割、
依存ミドルウェアへの経路、メッセージの中身を実装に忠実に描いている。
括弧内は対応するソース（`crates/control-plane/src/*` など）。

```mermaid
flowchart TD
  client["Client"]

  subgraph cpbox["control-plane (axum, 単一バイナリ)"]
    rest["REST handlers (handlers.rs)<br>auth / authz / RLS / invoke / uploads / usage"]
    valid["検証パイプライン (validation.rs)<br>サイズ上限 → wasmparser → import 厳密照合 → sha256"]
    sign["署名 (signing.rs, crypto.rs)<br>Ed25519 で job_token 発行 (kid 付き)"]
    adm["admission (admission.rs, store.rs)<br>rate / in-flight / login lockout を Lua 原子化"]
    subr["result subscriber (subscriber.rs::run)<br>kid 検証 → claim 突合 → CAS 終端 + rollup"]
    dlq["DLQ subscriber (subscriber.rs::run_failed)<br>.failed → failed 終端 + in-flight DECR"]
    reaper["reaper (reaper.rs)<br>stuck sweeper + in-flight 再同期"]
  end

  subgraph workerbox["worker (単一バイナリ)"]
    pull["JetStream Pull consumer<br>durable workers / backoff / max_ack_pending"]
    exec["実行 (main.rs)<br>cwasm キャッシュ + MeteredLimits<br>fuel / wall / peak mem / output bytes 計量"]
    wmetrics["metrics 公開 (metrics.rs)<br>独立 axum :9090 /metrics /readyz /healthz"]
  end

  comp["Component (echo / always-trap / slow)<br>wasm32-wasip2"]

  pg[("PostgreSQL (db.rs)<br>tenants / users / api_tokens<br>components / component_versions / executions<br>audit_logs (append-only) / usage_rollups<br>全テナント表 FORCE ROW LEVEL SECURITY")]
  minio[("MinIO / S3 互換 (storage.rs)<br>wasm 本体 + tenants/{t}/io/{exec}/input|output")]
  redis[("Redis<br>共有 admission カウンタ<br>(到達不能時は DegradedStore に縮退)")]

  subgraph natsbox["NATS"]
    js["invoke stream (JetStream)<br>tenant.*.component.invoke"]
    ressub["result subject (core NATS)<br>tenant.*.component.result"]
    failsub["failed subject / DLQ (core NATS)<br>tenant.*.component.failed"]
  end

  %% --- クライアント → control-plane ---
  client -->|"HTTP/REST + Bearer token"| rest

  %% --- アップロード / デプロイ経路 ---
  rest -->|"POST /components/{id}/versions (multipart)"| valid
  valid -->|"通過した本体を put"| minio
  rest -->|"POST /uploads: single-key presigned PUT URL"| minio

  %% --- invoke 受付 ---
  rest -->|"admission 原子チェック"| adm
  adm --> redis
  rest -->|"SET LOCAL app.tenant_id / 状態 INSERT"| pg
  rest --> sign
  sign -->|"JobMessage<br>(presigned GET URL + 署名 job_token)"| js

  %% --- worker 実行 ---
  js -->|"pull (durable workers)"| pull
  pull --> exec
  exec -->|"presigned GET で本体取得"| minio
  exec --> comp
  exec -->|"ResultMessage<br>(job_token を verbatim echo + UsageMetrics)"| ressub
  exec -.->|"最終配送失敗時に自前 publish<br>FailedMessage"| failsub

  %% --- 終端 writer (出所認証 + 冪等会計) ---
  ressub --> subr
  failsub --> dlq
  subr -->|"CAS finalize + usage_rollups UPSERT (同一 tx)"| pg
  subr -.->|"検証失敗は drop + audit_logs"| pg
  dlq -->|"failed 終端 + DECR"| pg

  %% --- reaper ---
  reaper -->|"stuck pending/running を failed 終端"| pg
  reaper -->|"DB COUNT で in-flight 再同期"| redis

  classDef svc fill:#cfe8ff,stroke:#1f6feb,color:#0b2942;
  classDef store fill:#d6f5dd,stroke:#1a7f37,color:#06281a;
  classDef broker fill:#e7d9fb,stroke:#7b46c9,color:#23104a;
  classDef guest fill:#fff3c4,stroke:#b08800,color:#3d2f00;
  classDef ext fill:#eaeaea,stroke:#777,color:#222,stroke-dasharray:4 3;

  class rest,valid,sign,adm,subr,dlq,reaper,pull,exec,wmetrics svc;
  class pg,minio,redis store;
  class js,ressub,failsub broker;
  class comp guest;
  class client ext;
```

ポイント（実装上の不変条件）:

- **worker は署名鍵を持たない**。`job_token` を全 result に verbatim に echo するだけで、
  検証は control-plane の subscriber が kid で公開鍵を選んで行う（§3.3 MUST NOT）。
- **二重計上が構造的に不可能**: `usage_rollups` の UPSERT は CAS finalize が状態を遷移させた
  同一トランザクション内でのみ走る。再配送 / DLQ 後着 / sweeper 先着では CAS が no-op になり
  rollup を触らない（冪等アンカー, §15）。
- **Redis は速い近似、DB COUNT が唯一の真実**。reaper が定期的に in-flight を再同期する。
- **result / failed は core NATS**（JetStream ではない）。CP 再起動で in-flight 結果が
  ドロップしうるのは既知の follow-up（README §M4 非スコープ参照）。

---

## invoke→result シーケンス図（本番 default 経路）

`Idempotency-Key` なし・インライン input・succeeded で終端する標準経路。
大容量 I/O（`POST /uploads`）と冪等再送は分岐として末尾に注記。

```mermaid
sequenceDiagram
  participant C as "Client"
  participant CP as "control-plane"
  participant R as "Redis (admission)"
  participant DB as "PostgreSQL"
  participant J as "NATS invoke (JetStream)"
  participant W as "worker"
  participant S3 as "MinIO"
  participant RES as "NATS result (core)"

  C->>CP: POST /invoke (Bearer, component, input)
  CP->>R: admission 原子チェック (rate / in-flight INCR)
  R-->>CP: 許可
  CP->>DB: SET LOCAL app.tenant_id / executions INSERT (pending)
  CP->>CP: active version 解決 + presigned GET URL + Ed25519 で job_token 署名
  CP->>J: publish JobMessage (Nats-Msg-Id = execution_id)
  CP-->>C: 202 Accepted (execution_id, pending)

  J->>W: pull (durable workers)
  W->>S3: presigned GET で wasm 本体取得
  S3-->>W: wasm bytes (sha256 で cwasm キャッシュ)
  W->>W: Wasmtime 実行 + 計量 (fuel/wall/mem/bytes)
  W->>RES: ResultMessage (job_token echo + UsageMetrics)
  W->>J: ack (publish 成功後)

  RES->>CP: result subscriber 受信
  CP->>CP: kid で署名検証 + claim/subject 突合
  CP->>DB: CAS finalize (succeeded) + usage_rollups UPSERT (同一 tx) + in-flight DECR

  C->>CP: GET /executions/{id}
  CP-->>C: status=succeeded, output
```

> 分岐: ① 大入力は事前に `POST /uploads` で `execution_id` を予約し presigned PUT で退避、
> invoke 時に `input_ref` を完全一致検証（§3.4）。② `Idempotency-Key` 提示時は 3 層冪等で
> 重複実行を防ぐ（§6.6）。③ trap / timeout は subscriber が failed/timeout 終端、
> worker 落下や無音失踪は reaper の stuck sweeper、最終配送失敗は `.failed` DLQ subscriber が回収。
