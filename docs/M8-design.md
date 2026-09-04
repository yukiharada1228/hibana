# M8 設計書 — 弾力スケールとテナント間アイソレーション（§8 / §10 / §15 M8）

本書は仕様書 §15 M8 の詳細設計である。M8 は 2 つの独立した設計案（弾力スケール側 / アイソレーション側）と
3 レンズの敵対的レビューを経て、**1 本の統合設計**に確定した。以下はその確定版であり、
実装は本書の §11 のステップ順にそのまま進められる。

**仕様書 §15 M8 の完了条件（原文、仕様書.md:917）**:

> 負荷に応じ worker が自動増減し、1 テナントのバースト負荷が他テナントの p99 レイテンシ / クォータを劣化させない。

本書は §1 でこの文言を**本リポジトリで実測可能な形へ翻訳**し（仕様書側の改訂提案を含む）、
§2 で「なぜ現状のコードでは成立していないか」を file:line で示し、§3〜§6 で設計、
§10 で CI / chaos への割り付け、§11 で実装ステップ、§12 で完了条件と検証手段の対応表を与える。

中核となる設計判断は 5 つである。

1. **「アイソレーション」を先に land し、「弾力スケール」をその上に載せる。** 2 案は同じ JetStream
   オブジェクト（durable `workers`, crates/worker/src/main.rs:93）と同じ env 名（`WORKER_MAX_CONCURRENCY`）を
   非互換に奪い合っていた。lane 分割が consumer トポロジを変える以上、backlog シグナルの定義は
   lane 分割の**後**にしか確定できない。順序はアイソレーション → 弾力スケールで一意に定まる（§1.4）。
2. **公平性は 3 層で担う（受付層 / 配送層 / 実行層）。** 受付層（admission）は「各テナントが自分の枠内か」
   しか見ないので、枠内の 2 テナントが資源をどう分け合うかを一切決めない。配送層（テナント別 lane
   consumer）と実行層（per-lane 実行クレジット）を新設する（§3 / §4）。
3. **worker の permit 規律を「メッセージを手にしてから await しない」に固定する。** レビューが指摘した
   lane 間デッドロック・idle lane による global permit 死蔵・`ack_wait` 超過による二重実行は、いずれも
   「permit を待つ位置」が原因である。**プロセス全体の上限は Semaphore ではなく
   `effective_lane_concurrency` という純関数で構成的に閉じる**（§4.1 / §4.2）。
4. **オートスケールの actuation は `scripts/` の参照実装 1 本に隔離し、k8s / KEDA マニフェストは非スコープ。**
   ただし「逃げ」にしないため、参照実装が実際にプロセス数を増減させることを chaos で整数 assert する（§5.5 / §10）。
5. **完了条件の「p99」を、サーバ時計のキュー待ち時間の絶対上限 + 整数 assert へ翻訳する。** 分位点の比較は
   本リポジトリでは測定基盤が存在せず（Prometheus が compose に無い、`faas_execution_duration_seconds` は
   tenant ラベルを持たず observe 配線も未了）、単一マシンでフレークする。仕様書 §15 の文言改訂を提案する（§1.2）。

---

## 含む / 含まない

### 含む

**(a) 配送層のテナント境界（M8-1 〜 M8-4）** — `FAAS_INVOKE` stream を `RetentionPolicy::WorkQueue` へ移行し、
テナント別 lane Consumer（+ 有界な overflow lane 1 本）を Control Plane が冪等に provisioning する。
consumer 作成責務を worker から CP へ移す。全テナント合算の `MAX_ACK_PENDING = 1000`
（crates/worker/src/main.rs:110）を廃し、lane ごとに `max_concurrent_executions + headroom` から導出する。

**(b) 実行層のテナント境界（M8-5）** — worker の無制限 `tokio::spawn`（crates/worker/src/main.rs:561）を撤去し、
lane ごとの実行クレジット + プロセス全体上限を導入する。上限は `effective_lane_concurrency()` という
DB / NATS 非依存の純関数で構成的に閉じる。

**(c) graceful shutdown（M8-6）** — `WORKER_DRAIN_TIMEOUT_SECS` による SIGTERM ドレイン。
オートスケールの scale-in が定常運用で毎日何度も起きる以上、ドレイン無しの scale-in は
「オートスケール機構自身が完了条件（レイテンシ劣化なし）を破る」ことを意味する。

**(d) 実行時間を制御できる検証用 component（M8-7）** — `components/burn`。入力 JSON から目標実行時間を
読んで busy-loop し **succeeded で終端する**。既存の `components/slow`（components/slow/src/lib.rs）は
無限 tight loop で必ず timeout 終端するため、「N ミリ秒かかって成功する」ノブとしては使えない。
chaos が backlog と滞留を決定的に作るために必要な設計要素であり、テストの都合ではない（§10.3）。

**(e) backlog シグナル + 判断ロジック（M8-8）** — CP 側の周期ポーラが全 lane の
`num_pending + num_ack_pending` を合算して gauge に載せ、`crates/control-plane/src/scale.rs`（新規・純関数）が
`backlog → desired_workers` を決める。内部専用 listener 上の `GET /internal/scale` で公開する。

**(f) 参照アクチュエータ（M8-9）** — `scripts/run-workers.sh` / `scripts/worker-autoscale.sh` /
`scripts/stop-workers.sh` と Makefile ターゲット。ローカルプロセス supervisor。

**(g) 観測と検証（M8-Z）** — lane 別 gauge / worker 側 gauge、`crates/control-plane/tests/chaos_m8.rs`
（U1〜U4）、README / `.env.example` / 仕様書の更新。

### 含まない（M9 以降の follow-up）

- **k8s HPA / KEDA マニフェスト**。本リポジトリに k8s クラスタも Dockerfile も無い
  （`find . -name "Dockerfile*"` は 0 件）。マニフェストを置いても検証できるのは静的スキーマ検査までで、
  「HPA が実際に効く」ことは CI でも手元でも証明できない。§12 の対応表に「検証手段: 無し」の行を
  作らないために非スコープとする（判断根拠は §1.3）。README に「本リポジトリの参照アクチュエータと
  **交換可能な外部実装**」として 1 段落だけ置く。
- **docker-compose への worker サービス追加 / Dockerfile 新規作成**。理由は §5.5。
- **優先度クラス（重み付き公平性）**。本書は**等分**（全 lane が同一の既定クレジット + テナント自身の
  クォータ由来の上限）のみを実装する。重み付けは §10 の「優先度分離」として M9 へ送る。
- **lane sharding（`MAX_DEDICATED_LANES` を超える規模でのハッシュ束ね）と、アクティビティ基準の
  lane 昇格 / 降格**。本書は overflow lane 1 本による有界化までを扱い、
  「本設計が完了条件を満たすテナント数の regime」を §3.3 で数値として明記する。
- **Result Ingestor 分離**（§2 / §10:776）。仕様書自身が「**需要発火型**: `.result` の write 経路が
  頭打ちになった時点で分離」と明記している（仕様書.md:916）ため、M8 の必須スコープではない。
- **`faas_execution_duration_seconds` の observe 配線と tenant ラベル**（README.md:1082-1084 の M4 follow-up）、
  および `METRICS_INCLUDE_TENANT_LABEL`（README.md:1086-1087）。本書は完了条件を Prometheus 非依存で
  実測する（§1.2 / §10）ため、この配線は完了条件の前提ではない。M9 の可観測性スライスへ送る。
- **`InstancePre` 事前インスタンス化 / Pooling アロケータ**（仕様書.md:324-325 の MUST、
  crates/worker/src/main.rs:1370 付近の TODO）。これらは coldstart ではなく **per-invocation** の CPU 削減施策で、
  `StoreLimits` / `ResourceLimiter` によるメモリ計量（M5 の計量不変条件）と相互作用する。単独スライスとして M9 へ。
- **cron / event / chain の admission 合流**（crates/control-plane/src/scheduler.rs / handlers.rs:2573 付近）。
  M8 は配送層と実行層でこれらを押さえるので、受付層（429 の意味論）には手を入れない（§7.3）。
- **Component LRU のテナント横断退避（H5, §2.6）**。`COMPONENT_CACHE_CAP = 64`
  （crates/worker/src/main.rs:103）の in-memory LRU は `wasm_sha256` キーでプロセス全体を共有するため、
  A が 64 種を超える component をバーストすると B にコールドスタートが出る。これは §3.6 キャッシュ設計の
  問題であり、M8 の chaos では A / B が同一 component を使うことでこの変数を固定する。M9 へ。
- **ローリングアップグレード**。M8-1 は stream 再作成を伴うため、CP と worker を同時に入れ替える
  停止メンテとして定義する（§9.1）。

---

## 0. サブマイルストン分解（依存順）

| ID | タイトル | 依存 | 破壊的 | 完了時の状態 |
| --- | --- | --- | --- | --- |
| M8-0 | 共有定数 / 純関数の一元化（挙動不変） | — | いいえ | `faas_shared` に `INVOKE_STREAM_NAME` / lane 名関数 / `assign_lanes` / `effective_lane_concurrency` / `parse_backoff_secs` / `invoke_stream_config()`。全列挙単体テストが CI で緑 |
| M8-1 | stream を CP 単独作成 + WorkQueue 化 | M8-0 | **はい**（stream 再作成） | CP が唯一の stream 作成者。CP / worker とも起動時に `retention == WorkQueue` を検査して fail-fast |
| M8-2 | CP 設定 / DB 読み取りの拡張（挙動不変） | M8-1 | いいえ | `lane_concurrency` クォータ、`list_active_tenants_with_quotas`、新 env 群。まだ lane を作らない |
| M8-3 | consumer 作成責務の CP 移管 + worker lane discovery | M8-2 | はい（同時入れ替え） | CP が legacy 共有 durable `workers` を作る。worker は `ensure_consumer` を廃し discovery で見つける。`TENANT_LANES_ENABLED=false` 既定なので**挙動は現状同等** |
| M8-4 | lane provisioning を有効化 | M8-3 | いいえ（env フラグ） | `TENANT_LANES_ENABLED=true` で reconcile が legacy → lane へ切り替える。**戻すのも同じフラグ 1 個** |
| M8-5 | worker の実行クレジット | M8-3 | いいえ | `WORKER_MAX_CONCURRENCY` / `WORKER_LANE_CONCURRENCY`、permit 規律、in-flight gauge |
| M8-6 | graceful shutdown（ドレイン） | M8-5 | いいえ | `WORKER_DRAIN_TIMEOUT_SECS`（既定 0 = 無効 = 現状と同一）、drain 中の `/readyz` 503 |
| M8-7 | `components/burn` | — | いいえ | 入力から目標 ms を読んで succeeded で終端する component |
| M8-8 | backlog シグナル + `scale.rs` + `/internal/scale` | M8-4 | いいえ | `SCALE_POLL_INTERVAL_SECS`（既定 0 = 無効）、gauge 群、純関数 + 全列挙テスト |
| M8-9 | 参照アクチュエータ + Makefile | M8-5, M8-8 | いいえ | `run-workers` / `autoscale` / `stop-workers` |
| M8-Z | chaos_m8（U1〜U4）+ ドキュメント | 全部 | いいえ | `crates/control-plane/tests/chaos_m8.rs`、README / `.env.example` / 仕様書 |

**M8 は DB マイグレーションを 1 本も持たない。** 新テーブル・新列・新 RLS ポリシーが無いため、
`migration_tests` / `rls-lint` が検出すべき退行の余地を一切増やさない。これは M7a が
「新規テーブルを作らないことで GRANT / RLS ポリシー漏れという最大の退行リスクを構造的に消す」
（docs/M7-design.md:123）と判断したのと同じ性質の成果であり、弱点ではない。次に migration が要るのは M9 である。

> **論証（なぜ DDL が不要か）**: M8 が必要とする永続状態は 4 つで、いずれも既存スキーマで足りる。
> (1)「どのテナントが lane を持つべきか」= `tenants (id, status, created_at)`（migrations/0001_init.sql）、
> (2)「lane の `max_ack_pending` の元になる値」= `tenants.quotas.max_concurrent_executions`、
> (3)「lane の worker 側並列度の上書き」= `tenants.quotas.lane_concurrency`（**JSONB の新キー**。
> `TenantQuotaOverrides`（crates/control-plane/src/db.rs:1468）は `deny_unknown_fields` を使わない前方互換設計なので、
> 新キーを書いた DB を旧 CP が読んでも壊れない）、
> (4)「lane トポロジそのもの」= **NATS が真実**（`Stream::consumer_names`）。

---

## 1. スコープの確定 — 完了条件を実測可能な形に翻訳する

### 1.1 何が「実測できない」のか（事実の確認）

| 原文の要素 | 本リポジトリでの測定可能性 | 根拠 |
| --- | --- | --- |
| 「worker が自動増減し」 | **測定手段が無い**（現状）。worker は `make run-worker`（Makefile:157-158）で人が起動するローカルプロセスで、台数を変えるコードがリポジトリ内に 1 行も無い | `Kubernetes上に複数のWasmtime Workerを並べる構成.md` は mermaid 図 51 行のみで実装が無い |
| 「他テナントの **p99 レイテンシ**」 | **測定手段が無い**。(1) `docker-compose.yml` に Prometheus が無い、(2) `faas_execution_duration_seconds`（crates/control-plane/src/metrics.rs:157 付近）は tenant ラベルを持たず observe 配線も未了（README.md:1082-1084）、(3) 単一マシンの docker + CPU 競合で分位点の比較はフレークし、しかも「アイソレーションが壊れた」という誤った症状に見える | metrics.rs:9-12 が「tenant_id は付けない」を方針として明記している |
| 「他テナントの **クォータ**を劣化させない」 | **測定可能**。被害テナントが受けた 429 の件数は整数で、`GET /usage` の `invocation_count` も整数 | crates/control-plane/src/handlers.rs:509 / :669 の 2 ゲートが 429 の唯一の発生源 |

加えて、本リポジトリは 2 マイルストン連続で「**分布や時間依存の assert を持たない**」を明文化している
（docs/M7-design.md:1891-1892、crates/control-plane/tests/chaos_m7.rs:745、chaos_m6.rs:184-186）。
比率や分位点を chaos に持ち込むことは、この確立した作法を破ることになる。

### 1.2 翻訳した完了条件（本書が責任を持つ形）

原文を次の 5 条件へ翻訳する。**すべて整数 assert または「サーバ時計で測った絶対上限」**であり、
比率・分位点・クライアント時計を一切含まない。

| # | 翻訳後の完了条件 | 原文のどの部分に対応するか |
| --- | --- | --- |
| **C1** | backlog が積み上がると、リポジトリ内の参照アクチュエータが **live worker プロセス数**を `min_workers` から `max_workers` へ増やし、backlog が枯れると `min_workers` へ戻す（`/healthz` プローブで数えた整数の到達を、env で上書き可能な絶対上限時間内に確認する） | 「負荷に応じ worker が自動増減し」 |
| **C2** | バーストテナント A の負荷中に、被害テナント B が受けた **429 の件数が 0** であり、B の全 M 件が `succeeded` で終端し、`GET /usage` の `invocation_count` の delta が **== M** である | 「他テナントの**クォータ**を劣化させない」 |
| **C3** | B の **キュー待ち時間** `queue_wait = started_at - created_at`（どちらもサーバ時計）の **最大値**が `CHAOS_M8_B_QUEUE_WAIT_MAX_MS`（既定 5000、env 上書き可）以下である。かつ**負の対照**として、同じ実行で A が実際に滞留していたこと（B の最終 invoke 時点で A の未終端が `CHAOS_M8_BURST/2` 件以上残っていた、かつ A の `max(queue_wait)` が同閾値以上）を要求する | 「他テナントの **p99 レイテンシ**を劣化させない」 |
| **C4** | 「B のジョブより後に始まった A のジョブ」の**個数**が `CHAOS_M8_BURST/2` 未満である（FIFO 単一レーンなら構造的に `== CHAOS_M8_BURST` になる） | 同上（head-of-line blocking の直接検出） |
| **C5** | スケール 1 サイクル（out → 全件終端 → in）を通して、worker の `executions_total` 合算 delta が **== K**（再実行ゼロ）、`faas_worker_redelivered_total` の delta が **== 0**、CP の `faas_dlq_finalized_total` の delta が **== 0**、終端が全件 `succeeded` である | 完了条件の暗黙の前提（スケール機構が実行の正しさを壊さない） |

### 1.3 仕様書 §15 M8 の完了条件の改訂提案

**なぜ原文のままでは検証不能か**は §1.1 のとおりである。要点は 2 つ。
(1)「p99」は tenant 次元を持つヒストグラムを要求するが、それは本リポジトリに存在せず、
かつ metrics.rs:9-12 が明示的に避けた設計（テナント数 × ルート数のカーディナリティ爆発）である。
(2)「自動増減」は actuation の実装主体を指定していないため、「HPA を書いた」と言えば達成したことに
できてしまう（が、それは検証できない）。

> **改訂提案（仕様書.md:917 の置き換え）**
>
> **完了条件**: (1) JetStream の未消化仕事量（全 lane の `num_pending + num_ack_pending` の合計）に応じ、
> リポジトリ内の参照アクチュエータが worker プロセス数を `SCALE_MIN_WORKERS` 〜 `SCALE_MAX_WORKERS` の
> 範囲で自動増減し、scale-to-zero / from-zero が成立する。
> (2) 1 テナントのバースト負荷の最中に、他テナントが受けた 429 が 0 件であり、他テナントの全ジョブが
> succeeded で終端し、その**キュー待ち時間**（`started_at - created_at`、いずれもサーバ時計）の最大値が
> 規定の絶対上限を超えない。かつ「バーストが実際に滞留を起こしていた」ことを負の対照として同時に確認する。
> (3) 上記のスケール 1 サイクルを通して、実行回数が投入件数と一致し（再実行ゼロ）、DLQ 終端が 0 件である。
> `crates/control-plane/tests/chaos_m8.rs` の U1〜U4 で end-to-end 検証。
>
> **注記**: テナント別の p99 レイテンシそのものは、運用者が `executions` 生表に対して
> `percentile_cont` で引く手順を README に置く（`VERSION_STATS_SQL`（crates/control-plane/src/db.rs:446-457）と
> 同型）。API として露出させると他テナントの統計が漏れるため、露出はしない。

### 1.4 M8(a) / M8(b) の land 順序を「アイソレーション先行」に確定する根拠

2 案は次の 5 点で非互換に衝突していた。順序を決めれば全て解ける。

| 衝突点 | アイソレーション先行での決着 |
| --- | --- |
| `chaos_m8.rs` を両案が新規作成し、両方が関数接頭辞 `chaos_u1_` を使う | **1 ファイル `chaos_m8.rs` に U1〜U4 を統合**。U1/U2/U3 がアイソレーション、U4 が弾力スケール |
| `WORKER_MAX_CONCURRENCY` の既定が 0（無制限）と 32 で、意味も違う | **単一所有 = worker プロセス全体の上限、既定 32**（§4.2）。0 = 無制限という「既知の壊れ方をする既定」は採らない |
| 弾力側は共有 durable `workers` の `consumer_info` を読み、その存在を不変条件に掲げる。アイソレーション側は同じ consumer を削除する | **backlog シグナルを「全 lane の合計」に再定義**（§5.2）。`Stream::consumers()`（async-nats-0.38.0/src/jetstream/stream.rs:1058）の 1 パス走査で取る |
| 弾力側は `MAX_ACK_PENDING = 1000` の意味を変えないことを不変条件にする。アイソレーション側は同じ行を削除する | **削除する**（§3.5）。合算の頭打ちこそが穴 H1 の本体であり、保存すべき不変条件ではない |
| 弾力側は worker 側 `get_or_create_stream` を残す。アイソレーション側は worker から stream 作成を撤去する | **撤去する**（§3.4）。「先に起動した側の設定が黙って勝つ」二重定義の事故を構造的に消す |

さらに決定的なのは、**弾力スケール側だけを先に land するとブートストラップ・デッドロックする**という事実である。
durable consumer `workers` を作るのは worker のみ（crates/worker/src/main.rs:656 の `ensure_consumer`、
呼び出しは :477）で、CP は stream しか作らない（crates/control-plane/src/main.rs:525）。
`SCALE_MIN_WORKERS=0` の環境では「worker が居ない → `CONSUMER.INFO` が not-found → シグナル無し →
desired 0 → worker が起動しない → consumer が作られない」という閉ループになる。
**consumer 作成責務を CP へ移すこと（= アイソレーション側の M8-3）が、弾力スケール側の前提条件**である。

---

## 2. 現状の穴 — 「admission を通過した後」に何が起きるか

### 2.1 admission が実際に守っているもの

`POST /invoke` は 2 ゲートを通る。
ゲート 1（token bucket）が crates/control-plane/src/handlers.rs:509 →
`admission::check_rate_limit`（crates/control-plane/src/admission.rs:183 付近）、
ゲート 2（in-flight reserve）が handlers.rs:669 → `admission::reserve_inflight`（同 :207 付近）。

パラメータは `AdmissionConfig::resolve_for_tenant`（crates/control-plane/src/state.rs:44-88）が
「グローバル既定（crates/control-plane/src/config.rs の `DEFAULT_INVOKE_RATE_PER_SEC=50` /
`DEFAULT_INVOKE_BURST=500` / `DEFAULT_MAX_CONCURRENT=20`）→ `tenants.quotas` 上書き（db.rs:1468-1494）→
推奨上限クランプ（state.rs:22-24 の `QUOTA_MAX_INVOKE_RATE_PER_SEC=500` /
`QUOTA_MAX_CONCURRENT_EXECUTIONS=200`）」の順で解決する。

ゲート 2 は **pending + running** を数える（仕様書 §8）。したがって「1 テナントが JetStream に無限に
メッセージを積む」ことは **HTTP 経路に限れば** 既定で起きない。**ここまでは仕様書 §8:751 の主張どおりである。
問題はその先にある。**

### 2.2 穴 H1 — `MaxAckPending` はテナント境界を持たない「合算」の歯止め

`MAX_ACK_PENDING: i64 = 1000`（crates/worker/src/main.rs:110）は共有 durable `workers`（同 :93）の
consumer config（同 :703 付近）にそのまま入る。これは**全テナント合算**の「配送済み未 ack」上限であり、
超えた瞬間 JetStream は**どのテナントに対しても**配送を止める。

- Σ が 1000 を超えないことを保証するコードは**リポジトリのどこにも無い**。仕様書 §8:751 は
  `Σ(アクティブテナント数 × max_concurrent_executions) ≲ MaxAckPending` を**運用目安**と書くだけで、
  state.rs にも store.rs にも Σ を見る箇所は存在しない。
- 既定（20/テナント）では **50 テナント**で破綻する。破綻の仕方が最悪で、
  **1 件しか投げていないテナント B が、49 テナントの合計で埋まった配送枠のせいで配送されない**。
- 配送されない B のジョブは `pending` のまま残る → `count_inflight_executions`（db.rs:1543 付近）が減らない →
  reaper の resync（crates/control-plane/src/reaper.rs:49 のループ内）が Redis カウンタを高いまま維持する →
  **B の次の invoke が `concurrency_limit` で 429（handlers.rs:669-677）**。
  つまり**他テナントの負荷が B のクォータを直接劣化させる経路がコード上に存在する**。完了条件 C2 への正面からの違反。

### 2.3 穴 H2 — admission をバイパスする enqueue 経路が H1 を無制限に踏める

`enqueue::enqueue_execution` を admission を通さずに呼ぶ経路が 2 つある
（Cron: crates/control-plane/src/scheduler.rs、外部イベント / chain: handlers.rs:2573 付近）。
`check_rate_limit` / `reserve_inflight` の呼び出しは **handlers.rs:509 と :669 の 2 箇所しか存在しない**
（grep で確認済み）。したがって §2.1 の「HTTP 経路に限れば in-flight は 20 で頭打ち」という保証は
cron / chain には効かず、**1 テナントの cron / chain が合算 1000 の配送枠を単独で食い尽くせる**。

### 2.4 穴 H3 — worker の実行スロットにテナントという概念が無い

crates/worker/src/main.rs:513-594 の pull ループは、取得したメッセージを**1 件ずつ無条件に
`tokio::spawn`** する（:561）。`JoinHandle` は保持されず、`Semaphore` は crate 全体に 1 つも存在しない。
外側ループは spawn 済みタスクの完了を待たずに次の `batch()` を回す（:514）。

結果として **1 worker プロセスの同時実行数に上限が無い**。唯一の歯止めが H1 の `MAX_ACK_PENDING=1000` であり、
これがテナント境界を持たないため:

- A が自分のクォータ（20）の範囲内で 20 並列、B が 1 並列のとき、同一プロセスの同一 tokio ランタイム上で
  **20:1 の CPU 配分**になる。両者とも自分のクォータを 1 バイトも超えていないのに、B のレイテンシは
  A の負荷に比例して悪化する。**これは admission では原理的に直せない。**
- さらに **水平スケールそのものを無効化する**。1 プロセスが最大 1000 件まで先取り（claim）でき、
  claim されたメッセージは `num_ack_pending` に移り `ack_wait`（既定 30 秒、crates/shared/src/lib.rs:638）が
  経過するまで他 worker へ再配送されない。**worker を 2 台目に増やしても、既に 1 台目が claim した
  ジョブは移らない。** 「pending depth を見て台数を増やす」というオートスケールの前提を根本から壊す。

### 2.5 穴 H4 — 単一ストリーム順配送による head-of-line blocking

stream は 1 本、consumer も 1 本、`filter_subject` は `tenant.*.component.invoke` のワイルドカード 1 本
（crates/worker/src/main.rs:692 付近）。`DeliverPolicy::All`（同 :691 付近）で **stream 順**に配送される。
A が 300 件を先に publish し、その後に B が 1 件 publish すると、B のメッセージは stream 上で A の 300 件の
後ろにいる。`PULL_BATCH = 16`（同 :99）なので 1 バッチが丸ごと A のジョブになりうる。
**テナントを選んで受け取らない手段が現状のトポロジには存在しない。**

### 2.6 穴 H5 — Component LRU のテナント横断退避（本書では扱わない）

`COMPONENT_CACHE_CAP = 64`（crates/worker/src/main.rs:103）の in-memory LRU は `wasm_sha256` キーで
プロセス全体を共有する（同 :465-467 / :1119-1186）。A が 64 種を超える component をバーストすると
B のエントリが退避され、B に coldstart が発生する。**本書では扱わない**（§含まない）。
chaos では A / B が同一 component（`burn`）を使うことでこの変数を固定する。

### 2.7 M8 が埋める穴の定義

> **M8 が埋めるのは H1 / H2 / H3 / H4 である。** すなわち「admission を通過した（あるいはバイパスした）後、
> **配送層**と**実行層**にテナント境界が一切存在しない」という 1 点に集約される。
> 受付層（admission）には手を入れない（§7.3）。

---

## 3. 配送層 — テナント別 lane Consumer

### 3.1 選択肢比較 — 「テナントを選んで受け取らない」ための物理的手段は 3 つしかない

worker が「A のメッセージは今は要らない、B のを寄越せ」と言うには、次のいずれかしかない。

| 手段 | 実現方法 | 致命的な副作用 |
| --- | --- | --- |
| (i) **park（保持）** | pull はするが permit が空くまで ack せず保持する | 保持中のメッセージは `MaxAckPending` を占有し続ける（H1 をむしろ悪化させる）。さらに `ack_wait=30s`（crates/shared/src/lib.rs:638）を超えると **再配送 = 二重実行**（仕様書 §3.3 の MUST NOT） |
| (ii) **NAK（差し戻し）** | `msg.ack_with(AckKind::Nak(delay))`（async-nats-0.38.0/src/jetstream/message.rs:213, :592-610 に実在を確認） | NAK は**配送回数を消費する**。`MAX_DELIVER = 5`（crates/shared/src/lib.rs:641）なので 5 回 NAK すると DLQ 送りになり、**ゲストが一度も失敗していないジョブをプラットフォームが恒久的な失敗に変換する** |
| (iii) **subject 分離** | consumer の `filter_subject` を分ける | consumer 数が増える。新規テナント時に provisioning が要る |

**(i) と (ii) は既存の不変条件（二重実行禁止 / max_deliver / DLQ）を壊すため採用不可**。
したがって**選択的フロー制御は subject 分離（= consumer 分離）でしか実現できない**。

| 比較軸 | (A) テナント別 Consumer | (B) 優先度クラス分離 | (C) worker 側 per-tenant 並行度制限のみ | (D) **(A)+(C) 統合**（採用） |
| --- | --- | --- | --- | --- |
| H1（配送枠の合算頭打ち） | 解消 | 部分解消 | **悪化しうる**（park が枠を食う） | 解消 |
| H2（バイパス経路） | 解消 | 部分解消 | 未解消 | 解消 |
| H3（worker 内 CPU 競合） | **未解消** | 未解消 | 解消 | 解消 |
| H4（head-of-line） | 解消 | 部分解消 | **原理的に未解消** | 解消 |
| テナント数スケーラビリティ | consumer = テナント数（線形）。要対策 | consumer = クラス数（定数） | consumer 1 本（最良） | **`min(T, MAX_DEDICATED_LANES) + 1` で有界化**（§3.3） |
| 完了条件の実測可能性 | 高（lane 別 depth が直接取れる） | 中 | **低** | **高** |
| 実装量 | 中〜大 | 中 | 小 | 大 |

**(C) 単独では完了条件を満たせないことの論証**: (C) は次のいずれかにしかならない。
(1) permit が空くまで pull しない構成 → worker は stream 順に A の 300 件を処理していくので、
B に到達するのは A を全部 pull し終えた後（H4 が残る）。**B のキュー待ちが A の backlog に比例したまま = C3 不成立。**
(2) permit が無くても pull して park する構成 → §3.1 表の (i) のとおり H1 悪化と二重実行の危険。
したがって **(A) と (C) の両方が必要**であり、しかも **(A) を入れた後は (C) が「pull 発行量の制御」として
park も NAK も無しに自然に実装できる**（lane が分かれていれば「A の lane からは今 pull しない」と言えるため）。
これが (D) を採る理由である。

### 3.2 トポロジと 2 つの不変条件

```text
stream FAAS_INVOKE  (retention = WorkQueue, subjects = ["tenant.*.component.invoke"])
   ├── consumer "workers-t-{tenant_a}"   filter_subject  = tenant.{a}.component.invoke
   ├── consumer "workers-t-{tenant_b}"   filter_subject  = tenant.{b}.component.invoke
   │      ...  （最大 MAX_DEDICATED_LANES 本）
   └── consumer "workers-overflow"       filter_subjects = [tenant.{x}.…, tenant.{y}.…]   ※あふれた分だけ
```

> **不変条件 I1（二重実行の構造的禁止）**: **どの invoke subject も、ちょうど 1 つの consumer の filter に属する。**
> これは設計上の約束ではなく **NATS サーバが強制する**。`RetentionPolicy::WorkQueue` の stream では
> filter が重複する consumer の作成をサーバが拒否するため、I1 違反は「静かな二重配送」ではなく
> 「consumer 作成エラー」として即座に現れる。仕様書 §3.3:269 の「同一ジョブの多重実行は許されない (MUST NOT)」が、
> lane 分割後も**サーバ強制の不変条件として保たれる**ことがこの設計の要である。

> **不変条件 I2（配送枠のテナント帰属）**: lane consumer の `max_ack_pending` は**そのテナント 1 者にのみ属する**。
> したがって「A の未 ack が B の配送を止める」経路（H1）が消滅する。全テナント合算の固定値
> `MAX_ACK_PENDING = 1000`（crates/worker/src/main.rs:110）は**廃止**され、仕様書 §8:751 の Σ 目安式は不要になる。
> **例外**: overflow lane に属するテナント同士は互いに干渉する（§3.3）。

### 3.3 lane 割り当て — 純関数（CI で守る本丸）

`crates/shared/src/lib.rs` の `invoke_subject_wildcard()`（:249）の隣に置く。テナント ID は subject-safe
（同 :233 `invoke_subject`、:52-53 のバリデーション）で `.` / `*` / `>` / 空白を含まないため、
durable 名に埋め込んでも NATS の名前制約に抵触しない。

```rust
/// invoke stream 名。worker(main.rs:96) と CP(main.rs:533 付近) の二重定義を解消する。
pub const INVOKE_STREAM_NAME: &str = "FAAS_INVOKE";
/// dedicated lane の durable 名接頭辞。worker はこの接頭辞で lane を discovery する。
pub const LANE_DURABLE_PREFIX: &str = "workers-t-";
/// overflow lane の durable 名（固定）。
pub const OVERFLOW_LANE_DURABLE: &str = "workers-overflow";
/// M8-4 以前 / ロールバック時の共有 durable 名（worker/main.rs:93 と同値）。
pub const LEGACY_SHARED_DURABLE: &str = "workers";

pub fn lane_durable(tenant: &str) -> String { format!("{LANE_DURABLE_PREFIX}{tenant}") }

/// durable 名からテナント ID を復元する（worker の discovery が使う）。
/// overflow / legacy / 未知の consumer は None。
pub fn tenant_from_lane_durable(name: &str) -> Option<&str> {
    name.strip_prefix(LANE_DURABLE_PREFIX).filter(|t| !t.is_empty())
}

/// lane 割り当て（純関数）。`tenants` は **(created_at ASC, id ASC) でソート済み**であること。
/// 先頭 `max_dedicated` 件 → 専有 lane、残り → overflow lane。
///
/// 安定性 (MUST): 入力が作成順なので、**新規テナントの追加は既存の割り当てを一切動かさない**。
pub fn assign_lanes(tenants: &[String], max_dedicated: usize) -> LaneAssignment;

pub struct LaneAssignment {
    /// (durable 名, tenant_id)。1 テナント 1 lane。
    pub dedicated: Vec<(String, String)>,
    /// overflow lane に入るテナント。**空なら overflow lane を作らない (MUST)**:
    /// `filter_subjects` が空の consumer は「全 subject 購読」を意味し I1 を破壊する。
    pub overflow: Vec<String>,
}
```

**CI で守る性質（DB / NATS 非依存の全列挙テスト）**:

1. `lanes_cover_every_subject_exactly_once` — 任意のテナント集合 × 任意の `max_dedicated` について、
   全 `invoke_subject(t)` がちょうど 1 lane の filter に現れる。**I1 の数学的証明**。
2. `adding_a_tenant_never_moves_existing_assignments` — 末尾追加が既存割り当てを変えない。
3. `overflow_is_absent_when_empty` — overflow が空なら lane を作らない。
4. `lane_durable_names_are_subject_and_name_safe` — 生成名に `.` / `*` / `>` / 空白 / `/` が現れない。
5. `tenant_from_lane_durable_roundtrips` — `lane_durable` の逆写像であること。

> **本設計が完了条件を満たすテナント数の regime（明示）**: **アクティブテナント数 ≤ `MAX_DEDICATED_LANES`（既定 64）**。
> これを超えると超過分は overflow lane 1 本を共有し、**overflow lane 内部では H1 / H4 が復活する**
> （§3.1 の (C) 単独が H4 を解けないのと同じ理由）。これは隠れた欠陥ではなく、
> **有界で明示的な劣化モード**として宣言する。加えて (a) overflow lane の `max_ack_pending` は
> 所属テナント数に依存しない**固定値** `LANE_OVERFLOW_ACK_PENDING`（既定 1000 = 旧 `MAX_ACK_PENDING`）とし、
> 所属数に比例して合算上限を事実上撤廃してしまう事故を避ける、(b) `faas_tenant_lanes{kind="overflow"}` と
> `faas_lane_overflow_tenants` gauge で「劣化モードに入った」ことを可観測にする、(c) 割り当て鍵が
> `created_at` 固定なので「初日に登録した休眠テナントが専有 lane を握り続ける」不公平が残る点を
> **M9 の follow-up（アクティビティ基準の昇格 / 降格 + ヒステリシス）として §含まない に明記する**。

### 3.4 stream: `WorkQueue` retention への移行（本設計で唯一の破壊的変更）

現状の stream config は worker（crates/worker/src/main.rs:674-681 付近）と
CP（crates/control-plane/src/main.rs:525-541）で**二重定義**され、両方とも `..Default::default()` である。
`async-nats` の `Default` により retention = `Limits`、storage = `File`、`max_age = 0`（無期限）、
`max_bytes` / `max_msgs` 無制限になる。

移行理由は 4 つあり、**どれか 1 つでも単独で移行を正当化する**。

1. **I1 をサーバ強制にする**（§3.2）。lane 分割で最も怖い「filter の重なりによる二重配送」を、
   レビューではなくサーバに検出させる。
2. **consumer の delete / recreate が replay 事故にならない**。現状は drift 時に `delete_consumer` →
   `get_or_create_consumer`（crates/worker/src/main.rs:730-744）で、`DeliverPolicy::All` + `Limits` +
   `max_age=0` の組み合わせにより **stream 全履歴の再配送**が起こりうる。同 :651-655 のコメントは
   「durable の場合でも stream 側に再配送状態が残っているため、消費中ジョブは取りこぼさない」と書くが、
   ack floor / delivery 状態は **consumer 側の状態**であり `delete_consumer` で失われる（コメントの誤り）。
   WorkQueue では ack 済みメッセージが stream から消えるため、`DeliverPolicy::All` の再作成は
   「未 ack 分だけの再配送」= 正しい意味になる。
3. **負荷試験が環境を壊さない**。`Limits` / `max_age=0` / `storage=File` のままで §10 の負荷を回すと
   docker volume が単調増加する。
4. **per-lane のキュー深さの意味が正しくなる**。ack 済みが消えるので `num_pending` が
   「本当に未消化の仕事」を指す（§5.2 の backlog 定義の前提）。

**新しい stream config（CP のみが作成者になる）**:

```rust
// faas_shared::invoke_stream_config() に置き、CP だけが呼ぶ（M8-0 / M8-1）
StreamConfig {
    name: faas_shared::INVOKE_STREAM_NAME.to_string(),
    subjects: vec![faas_shared::invoke_subject_wildcard().to_string()],
    // M8-1: I1 をサーバ強制にし、ack 済みを消して replay 事故と disk 単調増加を同時に消す。
    retention: RetentionPolicy::WorkQueue,
    storage: StorageType::File,
    // ★ 上限は「設定しない」。WorkQueue なので ack で消えるため通常は溜まらない。
    //   max_msgs / max_bytes / max_age を設定すると、既定の discard=Old により
    //   **最も古い = 他テナントのメッセージから捨てる**ことになり、
    //   「1 テナントのバーストが他テナントのクォータを劣化させない」(C2) を新設 config 自身が破る。
    //   将来どうしても上限が要る場合は discard=New を必須とする（§8 不変条件 #6）。
    discard: DiscardPolicy::New,
    ..Default::default()
}
```

> **MUST（README 運用手順）**: `retention` は NATS で**作成後に変更できない**。したがって M8-1 は
> 「stream を削除して作り直す」手順になる。順序は次でなければならない。
> 1. CP と worker を停止する。
> 2. `curl -s localhost:8222/jsz?streams=true`（監視ポートは docker-compose.yml:43-53 で公開済み）で
>    `FAAS_INVOKE` の `messages` と consumer の `num_pending` / `num_ack_pending` が**すべて 0** であることを確認する。
> 3. 0 でなければ worker だけ起動して捌ききる。
> 4. stream を削除して CP を起動する（CP が WorkQueue で作り直す）。
>
> **なぜ「0 の確認」が MUST か**: stream を作り直すと **`Nats-Msg-Id` の重複排除ウィンドウ**（既定 2 分）が
> リセットされる。冪等性三層（§6.6）の layer 3 が一時的に消えることを意味する。layer 1
> （`executions` の `(tenant_id, idempotency_key)` 部分 UNIQUE）と layer 2（`executions.id` PK）は DB 側なので
> 効き続けるが、「三層のうち 1 層を意図的に落とす瞬間がある」ことを手順書に明示する。

> **不変条件の起動時検査（MUST、レビュー指摘の反映）**: CP の作成経路は `get_or_create_stream` であり、
> **既存 stream があれば config を更新せずそのまま返す**（crates/control-plane/src/main.rs:531-538）。
> 運用者が上の手順を飛ばすと、`Limits` のままの stream に lane consumer が作られ、
> I1 は「サーバ強制」ではなく「たまたま filter が重ならない設計上の約束」に**静かに退化する**。
> したがって **CP / worker の両方が起動時に `Stream::info()` で `retention == WorkQueue` を検査し、
> 違えば fail-fast する**。これは `assert_non_privileged_runtime_role`（crates/worker/src/main.rs:443）が
> 確立した「起動時に不変条件を検査する」既存作法に完全に揃う。

**worker 側**（crates/worker/src/main.rs:674-681 付近）は **`get_stream` に変更**し、作成しない。
stream が無ければ fail-fast する（CP を先に起動する運用を README に明記）。

> **既知のハザード（README トラブルシュートに記載）**: WorkQueue では ack されないメッセージが stream から
> 消えない。worker は `max_deliver` 到達時に `.failed` を publish してから ack する
> （crates/worker/src/main.rs:567-584）が、その publish 自体が失敗した場合は un-ack のまま残り、
> **backlog シグナルを恒久的に押し上げてオートスケーラを >0 台に固定しうる**。
> 検出は `faas_lane_pending_messages` が下がらないことで可能。回収は運用者が該当メッセージを
> `nats` CLI で除去するか stream を作り直す（DB 行側は stuck sweeper が 900 秒で終端させるので会計は壊れない）。
> 発生条件は「最終配送試行での DLQ publish 失敗」だけであり稀。上限ノブ（`max_age`）で自動的に消す設計は
> **採らない**（上記のとおり他テナントのメッセージを捨てるため）。

### 3.5 lane consumer の config と `max_ack_pending` の再定義

**CP が唯一の consumer 作成者**になる（§3.7）。

```rust
PullConfig {
    durable_name: Some(faas_shared::lane_durable(tenant)),
    ack_policy: AckPolicy::Explicit,
    deliver_policy: DeliverPolicy::All,          // WorkQueue なので「未 ack 分だけ」の意味になる
    filter_subject: faas_shared::invoke_subject(tenant),   // dedicated lane
    // overflow lane では filter_subject を空にし filter_subjects に列挙する
    // （async-nats-0.38.0/src/jetstream/consumer/pull.rs:2045。server_2_10 は default feature（同 Cargo.toml:305-308）で有効）
    ack_wait:    Duration::from_secs(cfg.ack_wait_secs),   // faas_shared::ACK_WAIT_SECS 由来（TTL 結合, §3.3）
    max_deliver: cfg.max_deliver as i64,                   // faas_shared::MAX_DELIVER 由来
    backoff:     cfg.backoff.clone(),                      // BACKOFF_SECS 由来
    // ★ M8: テナント別の配送枠。合算 1000 の固定値を廃止する（I2）。
    max_ack_pending: lane_ack_pending(&resolved),
    // ★ M8: worker へ per-lane 設定を運ぶ（worker に DB を読ませない）。
    //    async-nats-0.38.0/src/jetstream/consumer/pull.rs:2091（server_2_10）
    metadata: HashMap::from([
        ("faas_tenant_id".into(),        tenant.to_string()),
        ("faas_lane_concurrency".into(), resolved.lane_concurrency.to_string()),
        ("faas_lane_kind".into(),        "dedicated".into()),
    ]),
    ..Default::default()
}

/// テナント別の配送枠 = そのテナントの in-flight 上限 + 再配送スラック（§7.2）。
fn lane_ack_pending(r: &ResolvedAdmissionParams) -> i64 {
    r.inflight.max.saturating_add(LANE_ACK_PENDING_HEADROOM /* 既定 8 */)
}
```

overflow lane は `filter_subjects: overflow.iter().map(invoke_subject).collect()`、
`max_ack_pending: LANE_OVERFLOW_ACK_PENDING`（既定 1000、§3.3 の判断）、`metadata.faas_lane_kind = "overflow"`。

> **`metadata` を使う設計判断**: worker は per-tenant 設定（lane 並列度）を知る必要があるが、
> worker に `tenants.quotas` を読ませると「テナント一覧・クォータの権威は CP」という責務分離が崩れる。
> consumer の `metadata` に CP が**解決済みの値**を書き込み、worker は
> `Stream::consumer_info`（async-nats-0.38.0/src/jetstream/stream.rs:887-890）で読む。
> **設定の解決点を CP 1 箇所に保ったまま worker へ配れる**（DB 往復も CP への HTTP 往復も不要）。
> metadata が欠損 / 壊れている場合は `WORKER_LANE_CONCURRENCY` の env 既定へ縮退する
> （crates/worker/src/main.rs の `parse_backoff_secs` 系が採る「壊れた値は既定へ」と同じ作法）。

**TTL 結合の所有移管（MUST、レビュー指摘の反映）**: consumer 設定の所有が worker から CP へ移る以上、
`ACK_WAIT_SECS` / `MAX_DELIVER` / `BACKOFF_SECS` を読むのも CP でなければならない。現状これらを読むのは
worker だけ（crates/worker/src/main.rs:247-278 の `Settings::from_env` と :290-309 の `parse_backoff_secs`）で、
README / `.env.example` も worker 側 env として案内している。移行後に運用者が worker にだけ設定すると、
lane consumer は `faas_shared` の既定（`ACK_WAIT_SECS=30` / `MAX_DELIVER=5`、crates/shared/src/lib.rs:638/641）で
作られ、CP の `token_exp_offset_secs`（同 :653-664）は自分の config から計算するため、
まさに §3.3 が防ごうとしたドリフトが静かに起きる。したがって M8-1 の必須作業として:

1. `parse_backoff_secs` を `faas_shared` へ移す。
2. CP の `Config` に `backoff_secs` を追加する（`ack_wait_secs` / `max_deliver` は既に config.rs:320-322 にある）。
   lane consumer config と `token_exp_offset_secs` の**両方を同じフィールドから導出**する。
3. worker 側は同 3 env を読まなくなる（読むと二重真実になる）ので撤去し、README の環境変数表を同じ PR で更新する。
4. CI に「`ack_wait * max_deliver + 実行上限 + margin <= token exp offset`」の関係を固定する DB-free 単体テストを 1 本足す。

### 3.6 データモデル — DDL ゼロ、ただし読み取りクエリは 1 本では足りない

`lane_ack_pending()` と `metadata.faas_lane_concurrency` は
`AdmissionConfig::resolve_for_tenant`（crates/control-plane/src/state.rs:44-88）の出力を要求し、
その入力である `TenantQuotaOverrides` は `load_tenant_status_and_quotas`
（crates/control-plane/src/db.rs:1490-1494、`SELECT status, quotas FROM tenants WHERE id = $1`）という
**単一テナント PK lookup** でしか取れない。既存の `list_active_tenant_ids`（同 :1526-1531）は id しか返さないので、
reconcile が毎周期テナント数ぶんの追加クエリを撃つ **N+1** になる。

したがって「DDL ゼロ」は成果として保つが「クエリ 1 本」という主張は**撤回**し、次を追加する。

```rust
/// M8: lane 割り当てのため、active テナントを **作成順** で quotas ごと列挙する。
/// 作成順で返すことが lane 割り当ての安定性（§3.3）の前提である。
/// tenants は RLS 対象外（migrations/0004_rls.sql）なので GUC 不要。
/// 壊れた JSONB は `load_tenant_status_and_quotas`（db.rs:1503 付近）と同じ規約で default へ縮退する。
pub async fn list_active_tenants_with_quotas(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<(String, TenantQuotaOverrides)>, sqlx::Error> {
    sqlx::query("SELECT id, quotas FROM tenants WHERE status = 'active' ORDER BY created_at ASC, id ASC")
        .fetch_all(executor).await /* map ... */
}
```

`TenantQuotaOverrides`（db.rs:1468-1478）に 1 フィールド追加する。

```rust
/// M8 (§8 / §10): 1 worker プロセスがこのテナントに割く同時実行スロット数の上書き。
/// 未指定はグローバル既定（WORKER_LANE_CONCURRENCY）を継承。
#[serde(default)] pub lane_concurrency: Option<u64>,
```

### 3.7 責務配置 — 誰が lane を作り、誰が見つけるか

| 責務 | 担当 | 理由 |
| --- | --- | --- |
| lane の **作成 / 更新 / 削除** | **CP（reconcile リーダーを取った 1 インスタンス）** | テナント一覧とクォータの権威が CP にある。worker が `tenants` を読むのは責務分離に反する |
| lane の **発見** | **worker** | 権威は NATS（`Stream::consumer_names`, async-nats-0.38.0/src/jetstream/stream.rs:1029）。worker は DB も CP HTTP も触らない |
| lane の **設定値の解決** | **CP**（state.rs:44-88） | 既存の解決順序を 1 箇所に保ち、結果を consumer `metadata` で配る |

#### 3.7.1 単一 writer の強制（advisory lock）— レビュー ops/blocker の反映

CP は「ステートレス x N」が前提（仕様書.md:723-728）である。`run_lane_reconcile` にリーダー選出も排他も
無いまま全インスタンスがそれぞれの DB スナップショットで回すと、
(a) A が overflow から subject X を外した直後に古い desired を持つ B が X を戻し、
**X がどの lane にも属さない窓**が生じる（その間の publish は誰にも配送されず、900 秒後に stuck sweeper が偽 failed に倒す）、
(b) 逆順なら X が両方に属し `update_consumer` がサーバに拒否されて drift が永久に収束しない、
(c) A が「inactive」と読んだテナントの lane を B が直前に作った直後に削除する、が起こる。

> **MUST**: `reconcile_lanes_once` の**1 パス全体**を `pg_try_advisory_lock(<固定キー>)` で囲み、
> 取れなかったインスタンスはそのラウンドを skip する。ロックはプールから取り出した専用
> `PoolConnection` 上のセッションロックとし、パス終了時に `pg_advisory_unlock` で必ず解放する
> （パス中に NATS RPC を挟むため、tx を張りっぱなしにはしない）。新規依存も DDL も不要で、
> cron スケジューラが `FOR UPDATE SKIP LOCKED` で single-flight を実現しているのと同じ性質の作法である。
> **削除 → overflow の filter 縮小 → 作成、という順序制約はロック下でのみ意味を持つ**ことを
> コードコメントに明記する。

#### 3.7.2 reconcile の 1 パス（slow path, 収束）

`reaper::run_secret_kid_gauge`（crates/control-plane/src/reaper.rs:208-236）と同型の背景タスクとして
`crates/control-plane/src/main.rs:261` の spawn 直後に 1 本足す。周期は `LANE_RECONCILE_INTERVAL_SECS`（既定 10）。
reaper の 30 秒を再利用しない理由は §3.7.4（新規テナントの初回同期 invoke に効く）。

```rust
async fn reconcile_lanes_once(state: &AppState) -> anyhow::Result<()> {
    // (0) advisory lock。取れなければこのラウンドは他インスタンスに任せる（§3.7.1）。
    let Some(_guard) = state.try_lane_reconcile_lock().await? else { return Ok(()) };

    // (1) 全体列挙の失敗はパス全体を Err にして次周期へ（reaper.rs:79-86 の誤り分離規約）。
    let tenants = db::list_active_tenants_with_quotas(state.pool()).await?;
    let stream  = state.jetstream().get_stream(faas_shared::INVOKE_STREAM_NAME).await?;
    let actual: BTreeSet<String> = collect_consumer_names(&stream).await?;

    // (2) TENANT_LANES_ENABLED=false のときは legacy 共有 durable 1 本へ収束させる。
    //     lane を先に全削除してから legacy を作る（WorkQueue では filter が重なると作成が拒否されるため、
    //     この順序が唯一成立する順序である = ロールバック経路の実体, §9.2）。
    let desired = if state.tenant_lanes_enabled() {
        LaneTopology::lanes(faas_shared::assign_lanes(&ids(&tenants), state.max_dedicated_lanes()))
    } else {
        LaneTopology::legacy()
    };

    // (3) DELETE → (4) UPDATE（overflow の filter_subjects を縮める）→ (5) CREATE の順で適用する。
    //     I1 を一瞬たりとも破らないための順序 MUST。
    //     削除は「不要になった lane」かつ「num_pending == 0 && num_ack_pending == 0」のときだけ。
    //     未処理が残る lane を消すと配送が止まるため、残っていれば warn して次周期へ繰り越す。
    //     **例外**: (2) のロールバック経路（lanes → legacy）では未処理 0 条件を明示的に免除する。
    //     免除で失われるのは consumer 側の ack floor / 配送状態であり、
    //     残メッセージは WorkQueue に残ったまま legacy consumer が DeliverPolicy::All で拾い直す。
    //     DB 行側は二重に stuck sweeper が保護する。この免除は §9.2 のロールバック手順の本体である。
    //
    //     drift 判定は `stream.consumer_info(name)` と比較し、差があれば
    //     **`Stream::update_consumer`（stream.rs:828-836）で更新する。`delete_consumer` は使わない。**
    //     これが worker/main.rs:730-744 の delete+recreate を置き換える最重要ポイント。
    //     比較軸は ack_wait / max_deliver / backoff / max_ack_pending /
    //     **filter_subject(s)** / **metadata**。既存の drift 判定（worker/main.rs:719-729）には
    //     filter 軸と metadata 軸が無く、誤った filter の lane が永久に是正されないので必ず追加する。
    Ok(())
}
```

パス末尾で 3 つの gauge を更新する（§6）。特に **`faas_lane_unsubscribed_subjects`**
（desired subject 集合 − 実 filter 集合の差分件数）は「購読者ゼロの subject が存在する」ことを
900 秒の偽 failed より先に検出するための指標であり、必ず出す。

#### 3.7.3 enqueue 時 ensure（fast path, 黒板の穴埋め）

`enqueue::enqueue_execution` の publish 直前（crates/control-plane/src/enqueue.rs:298-302 の直前）に
`ensure_lane_for_tenant(state, req.tenant)` を挟む。これで
**「publish は必ず lane 作成の後」**という不変条件が立ち、「メッセージは stream にあるが誰も購読していない」窓が消える。

レビュー指摘（overflow / キャッシュ無効化 / CP 多重化）を反映した確定仕様:

1. **dedicated 枠に空きがあるテナントの dedicated lane 作成だけ**を行う。
   overflow lane の `filter_subjects` は read-modify-write になり、CP N 台の同時更新が last-writer-wins で
   互いの subject を消すため、**overflow は reconcile（単一 writer）だけが更新する**。
2. overflow 対象になるテナントの enqueue では ensure を skip し、
   「reconcile が最大 1 周期（既定 10 秒）以内に購読を張る」ことを**明示的な遅延契約**として §7.1 の表に書く。
   「dedicated 枠に空きがあるか」は `AppState` が保持する reconcile 由来のスナップショット（lane 数）で判定し、
   ホットパスで `assign_lanes` を評価しない。
3. キャッシュは `DashMap<String, ()>`（`WaiterRegistry`（state.rs:126 付近）と同じ作法）だが、
   **`AppState` の世代カウンタ（`AtomicU64`）とセットにする**。`reconcile_lanes_once` が lane を
   1 本でも作成 / 削除したら世代をバンプし、次の ensure がキャッシュ全体を drop して張り直す。
   テナントは削除されず `status` が `active|suspended` で遷移するだけ（migrations/0001_init.sql:15-16）なので、
   「suspend → reconcile が lane 削除 → 再 activate」の後にキャッシュヒットで二度と作り直さない、
   という無音の暗転を構造的に消す。
4. `EnqueueError::PublishBackpressure`（enqueue.rs:314/326）を返すときは当該テナントのエントリを落とす（再 ensure を促す）。

#### 3.7.4 lane 作成の即時通知 — M6 完了条件の回帰を防ぐ

worker の周期 discovery だけだと、**新規テナントの初回 `POST /invoke?wait=1` が必ず 202 へ縮退する**。
`SYNC_REPLY_TIMEOUT_MS` は 5000（crates/control-plane/src/config.rs の `DEFAULT_SYNC_REPLY_TIMEOUT_MS`、Makefile:78）で、
discovery 周期がそれ以上なら「lane はあるが worker がまだ購読していない」窓に必ず当たるからである。
これは land 済みの M6 完了条件の回帰であり、許容できない。

> **採用**: CP が lane を作成 / 削除したとき、**core NATS** で `faas.lane.changed` を 1 発 publish する
> （ペイロードは空でよい。トポロジの真実は NATS 側の consumer 一覧であり、通知は「今すぐ見に行け」の合図に過ぎない）。
> worker は core で購読し、受信したら即座に discovery を 1 回走らせる。core NATS クライアントは
> worker が既に保持している（`Worker.nats`）ので追加依存はゼロ。
> **周期 discovery（`LANE_DISCOVERY_INTERVAL_SECS`、既定 10）は収束用の保険として残す**
> （通知の取りこぼしや worker 再起動直後を吸収する）。

`faas.lane.changed` は JetStream ではなく core NATS を使う。取りこぼしても周期 discovery が収束させるので
durable 保証は不要であり、stream を増やすと観測・運用の複雑度が増す（crates/worker/src/main.rs:484-491 付近の
`.failed` を core にした判断と同じ論理）。

---

## 4. 実行層 — worker の lane ループと実行クレジット

### 4.1 permit 規律（本設計で最も重要な不変条件）

レビューは 3 つの独立した障害を指摘した。いずれも「permit を待つ位置」が原因である。

| 障害 | 機序 | 本設計での封じ方 |
| --- | --- | --- |
| **lane 間デッドロック** | 各 lane タスクが `want` 個の permit をバッチ要求前にまとめて `await` 取得し、実際に spawn するまで解放しない。複数 lane が global permit を部分取得したまま互いに待つ hold-and-wait | **バッチ前に取るのは「1 組」だけ**。2 個目以降は `try_acquire` のみ（§4.3 の規律 R3） |
| **idle lane による global permit 死蔵** | `batch.next()` は `max_messages` が揃うか `expires` が切れるまで返らない。空 lane が最大 `expires` 秒ぶん global permit を占有し、lane 数 > global で他 lane が完全に締め出される | **プロセス全体の上限を Semaphore で持たない**。`effective_lane_concurrency`（§4.2）で構成的に閉じるため、lane が global を奪い合う経路そのものが存在しない |
| **`ack_wait` 超過による二重実行** | バッチ内側ループで 2 件目以降の permit を `await` すると、既に配送済みのメッセージが `ack_wait = 30s`（crates/shared/src/lib.rs:638）を超えて保持され、JetStream が再配送する = ゲスト二重実行 | **メッセージを手にしてから permit を `await` しない**（規律 R4）。permit は必ずバッチ要求より前に確保済み |

> **不変条件 W1（MUST）**: **pull ループの内側（メッセージを保持している区間）で permit を `await` してはならない。**
> `want` の上限は事前に確保済みの permit 数であり、サーバが `want` を超えて返すことは無いので不変条件が閉じる。

> **不変条件 W2（MUST）**: **1 lane ループが同時に保持する「未使用 permit」は高々 `PULL_BATCH` 個で、
> それらはすべて自 lane の専有資源である。** 共有資源（プロセス全体の上限）を待つ経路を作らないため、
> hold-and-wait が原理的に成立しない。

### 4.2 プロセス全体の上限を「純関数」で閉じる — `effective_lane_concurrency`

素直に「per-lane Semaphore + プロセス全体 Semaphore」の二段にすると、
`Σ(lane_concurrency) > WORKER_MAX_CONCURRENCY` のときに必ず上のいずれかの障害を踏む。
既定値（32 / 4）でも **9 テナント以上で Σ=36 > 32** となり、レビューが指摘したとおり出荷既定が
自分の不変条件を破っている。

> **採用**: **プロセス全体の上限は Semaphore で「待たせて」実現するのではなく、
> 各 lane に配る枠を割り算して構成的に守る。**

```rust
/// crates/shared/src/lib.rs（純関数・CI の全列挙テスト対象）
///
/// 1 lane に配る実効同時実行スロット数を決める。
/// - `global`: WORKER_MAX_CONCURRENCY（このプロセス全体の上限）
/// - `served_lanes`: このプロセスが現に購読している lane 数（1 以上）
/// - `configured`: CP が consumer metadata で配った per-lane 設定（WORKER_LANE_CONCURRENCY 既定）
///
/// 不変条件（CI で証明する）: `served_lanes * effective_lane_concurrency(global, served_lanes, cfg) <= max(global, served_lanes)`
/// すなわち `served_lanes <= global` の regime では **Σ が global を超えない**。
pub fn effective_lane_concurrency(global: usize, served_lanes: usize, configured: usize) -> usize {
    let fair_share = global / served_lanes.max(1);
    configured.min(fair_share).max(1)   // 最低 1 は必ず配る（0 だと lane が永久に停止する）
}

/// このプロセスが購読してよい lane の上限。超過分は「未提供 lane」として loud に晒す。
pub fn served_lane_capacity(global: usize) -> usize { global.max(1) }
```

**CI で守る性質**:

- `sum_of_effective_never_exceeds_global` — `global ∈ 1..=64`、`served ∈ 1..=global`、`configured ∈ 1..=64` の
  全組み合わせで `served * effective <= global`。
- `effective_is_at_least_one` — 常に 1 以上（0 で lane が停止しない）。
- `effective_never_exceeds_configured` — CP が配った上限を worker が勝手に超えない。
- `effective_is_monotone_in_global` — global を増やして減ることはない。

### 4.3 lane ループ（確定版の擬似コード）

```rust
// crates/worker/src/main.rs:477-483（ensure_consumer 呼び出し）と :513-594（pull ループ）を置き換える。
// stream は get_stream のみ（作成しない, §3.4）。

async fn discovery_loop(worker: Arc<Worker>, stream: Stream, nats: Client, settings: LaneSettings) {
    let mut lanes: HashMap<String, LaneHandle> = HashMap::new();
    // §3.7.4: CP からの即時通知。取りこぼしは周期 tick が吸収する。
    let mut changed = nats.subscribe(faas_shared::LANE_CHANGED_SUBJECT).await?;
    let mut tick = tokio::time::interval(Duration::from_secs(settings.discovery_interval_secs));

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = changed.next() => {}
        }

        let names = collect_consumer_names(&stream).await.unwrap_or_default();
        let mine: Vec<String> = names.into_iter()
            .filter(|n| faas_shared::tenant_from_lane_durable(n).is_some()
                     || n == faas_shared::OVERFLOW_LANE_DURABLE
                     || n == faas_shared::LEGACY_SHARED_DURABLE)
            .collect();   // 決定的順序（BTreeSet 由来）にすること

        // 消えた lane は pull を止めるだけ（実行中タスクは殺さない）。
        lanes.retain(|name, h| if mine.contains(name) { true } else { h.cancel(); false });

        // §4.2: lane 数がプロセス容量を超えたら、先頭 capacity 本だけを提供する。
        // 提供しない lane は loud に晒す（gauge + warn）。回転は次周期の offset で行う（§4.4）。
        let capacity = faas_shared::served_lane_capacity(settings.max_concurrency);
        let (served, unserved) = rotate_and_split(&mine, capacity, &mut settings.rotation_offset);
        worker.metrics.lanes_unserved.set(unserved.len() as i64);
        if !unserved.is_empty() {
            warn!(served = served.len(), unserved = unserved.len(), max_concurrency = settings.max_concurrency,
                  "more lanes than this worker can serve; rotating (raise WORKER_MAX_CONCURRENCY or add workers)");
        }

        // 実効クレジットは lane 数に応じて毎周期再計算し、既存 lane にも伝える（Semaphore を張り替える）。
        for name in served {
            let cfg = lane_metadata_concurrency(&stream, &name).await
                .unwrap_or(settings.default_lane_concurrency);
            let eff = faas_shared::effective_lane_concurrency(settings.max_concurrency, served.len(), cfg);
            lanes.entry(name.clone())
                 .or_insert_with(|| spawn_lane(worker.clone(), stream.clone(), name.clone()))
                 .set_credits(eff);
        }
        worker.metrics.lanes.set(lanes.len() as i64);
    }
}

async fn lane_loop(worker: Arc<Worker>, stream: Stream, name: String, credits: Arc<Semaphore>) {
    let consumer: Consumer<PullConfig> = stream.get_consumer(&name).await.expect("lane consumer");
    let max_deliver = worker.max_deliver as i64;

    loop {
        // R1: 自 lane の専有 permit を **1 個だけ** await で取る。
        //     これが「空きが無ければ pull しない = メッセージを他 worker に残す」背圧の本体。
        //     専有資源なので、ここで待つことは他テナントに一切影響しない（W2）。
        let anchor = Arc::clone(&credits).acquire_owned().await.expect("lane semaphore never closed");

        // R2: 追加分は **try_acquire のみ**。取れた数だけバッチを大きくする。
        let mut permits = vec![anchor];
        while permits.len() < PULL_BATCH {
            match Arc::clone(&credits).try_acquire_owned() { Ok(p) => permits.push(p), Err(_) => break }
        }

        // R3: 要求件数は「確保済み permit 数」ちょうど。expires は短く（5s）して、
        //     空 lane が permit を握る最悪時間を縮める（握っているのは自 lane の枠だけなので害は自テナント内）。
        let mut batch = match consumer.batch()
            .max_messages(permits.len())
            .expires(Duration::from_secs(LANE_PULL_EXPIRES_SECS))
            .messages().await
        {
            Ok(b) => b,
            Err(e) => { warn!(lane = %name, error = %e, "failed to pull batch; backing off");
                        drop(permits); tokio::time::sleep(Duration::from_secs(1)).await; continue }
        };

        while let Some(item) = batch.next().await {
            let msg = match item { Ok(m) => m, Err(e) => { warn!(error = %e, "error reading pulled message"); continue } };
            // R4 (= 不変条件 W1): ここで permit を await しない。必ず pop で取る。
            let Some(permit) = permits.pop() else { break };

            let delivered = msg.info().map(|i| i.delivered).unwrap_or(1);   // 既存 main.rs:551 と同一
            if delivered >= 2 { worker.metrics.redelivered_total.inc(); }   // §6: 二重実行の直接観測
            let is_final_attempt = max_deliver > 0 && delivered >= max_deliver;

            let inflight = InflightGuard::new(&worker.metrics);   // §4.5: panic 安全な gauge
            let worker = Arc::clone(&worker);
            let payload = msg.payload.clone();
            tokio::spawn(async move {
                let _permit = permit;      // タスク終了（panic 含む）で必ず返却される
                let _inflight = inflight;  // 同上
                /* 既存 main.rs:562-591 の ack-after-publish / DLQ 分岐を **1 行も変えずに** 移設する */
            });
        }
        drop(permits);   // 使わなかった permit は即座に返る
    }
}
```

- **ack セマンティクスを 1 行も変えない**（ack-after-publish、`delivered >= max_deliver` での DLQ 自前 publish、
  un-ack による再配送）。permit と gauge のガードが `tokio::spawn` の**手前と中**に入るだけである。
- `LANE_PULL_EXPIRES_SECS` は既定 5（現行は 30、crates/worker/src/main.rs:517）。短くする理由は
  「lane が消えた / クレジットが変わったことに気づくまでの最悪待ち」を縮めるため。
  空振り pull は JetStream にとって安価であり、`num_waiting` として観測できる。

### 4.4 lane 数がプロセス容量を超えたときの縮退（明示的な劣化モード）

`served_lanes > WORKER_MAX_CONCURRENCY` のとき、worker は先頭 `capacity` 本だけを提供し、
残りは `LANE_ROTATION_SECS`（既定 30）ごとに回転させる。

- **これは完了条件のスコープ外の regime である**ことを README に明記する。未提供 lane のテナントは
  最大 `LANE_ROTATION_SECS` 待たされる（喪失はしない。メッセージは WorkQueue に残る）。
- 検出は `faas_worker_lanes_unserved > 0`（gauge）と warn ログ。
- 正しい対処は 2 つ: `WORKER_MAX_CONCURRENCY` を上げるか、**worker 台数を増やす**（= §5 の弾力スケール）。
  後者が効くのは、複数 worker が同じ lane 集合を分担して購読するからである
  （competing consumers は lane 単位でも成立する）。
- 起動時 fail-fast にしない理由: lane 数は**実行時に増える**（テナント追加）ので、起動時検査では守れない。
  「静かに壊れる」ことだけを避ければよく、そのための手段が gauge + warn + 回転である。

### 4.5 `InflightGuard` — panic 経路で gauge をリークさせない

既存の spawn 本体（crates/worker/src/main.rs:561-592）は panic をキャッチしない。
`inc()` / `dec()` を素で書くと `handle_payload` 内の panic で `dec()` が実行されず gauge が単調増加し、
さらに §4.6 のドレインが「in-flight が 0 にならない」と誤認して必ずタイムアウトする。

```rust
struct InflightGuard(Arc<metrics::Metrics>);
impl InflightGuard { fn new(m: &Arc<metrics::Metrics>) -> Self { m.inflight_executions.inc(); Self(m.clone()) } }
impl Drop for InflightGuard { fn drop(&mut self) { self.0.inflight_executions.dec(); } }
```

permit（`let _permit = permit;`）と同じ寿命管理に統一する。

### 4.6 graceful shutdown（ドレイン）

#### 4.6.1 なぜ必要か（正しさではなく完了条件のため）

いま worker を強制終了すると何が起きるかを事実で追う。

1. `tokio::spawn` された detached タスクが即死する（crates/worker/src/main.rs:561-592）。
2. `.result` も `.failed` も publish されない。
3. メッセージは un-ack のまま。`backoff = [5,15,60]`（既定、同 :290-309）に従い **5 秒後**に再配送される。
4. 再配送先の worker が `mark_running` を呼ぶ（同 :854 付近）。SQL は
   `UPDATE executions SET status='running', started_at=$2 WHERE id=$3 AND status='pending'`（同 :1497-1506）。
   行は既に `running` なので **0 行更新**。
5. **しかし呼び出し側は `rows_affected` を見ない**。したがって**ゲストコンポーネントは 2 回実行される**。

冪等性三層（§6.6）は、この二重実行を防がない。防ぐのは会計だけである
（終端 CAS `FINALIZE_EXECUTION_SQL`（crates/control-plane/src/db.rs:1833-1839）の
`AND status NOT IN ('succeeded','failed','timeout')` により終端状態は 1 度しか書かれず、
`upsert_usage_rollup` は CAS が実際に遷移させたときだけ呼ばれる（subscriber.rs:729-746）ので**二重課金しない**）。

> **判定**: 強制 scale-in は「結果喪失なし・二重課金なし・カウンタリークなし」だが「**ゲストの副作用は 2 回**」である。
> at-least-once は仕様書.md:403 が Component 側の MUST として宣言済みなので regression ではない。
> しかし **scale-in は定常運用で毎日何度も起きる**。1 回の scale-in ごとに、その worker が抱えていた
> ジョブが `backoff[0] = 5 秒`の待ちと**フル再実行**を被る。これは完了条件（レイテンシを劣化させない）を
> **オートスケール機構自身が破る**ことを意味する。よってドレインを必須スコープに入れる。
> JetStream の再配送は「ドレインが失敗したときの最終保険」として残す（二重の網）。

#### 4.6.2 設計

`WORKER_DRAIN_TIMEOUT_SECS`（既定 **0** = ハンドラを一切インストールしない = 現行と完全に同一の SIGTERM 挙動）。
`> 0` のとき:

1. `tokio::signal::unix::signal(SignalKind::terminate())` と `ctrl_c()` を待つタスクを spawn
   （`tokio` は workspace で `features = ["full"]`、Cargo.toml:26 なので追加依存は不要）。
   受信で `CancellationToken` 相当（`Notify` + `AtomicBool`）を立てる。
2. **lane ループ**: `tokio::select!` で `batch.next()` と `shutdown.notified()` を待つ。
   ドレイン開始後は新しいバッチを要求しない。
3. **取得済み未処理メッセージ**: 現バッチに残っているものを
   `msg.ack_with(AckKind::Nak(Some(nak_delay)))` で明示的に差し戻す。**`nak_delay` は 0 ではなく
   `SCALE_IN_COOLDOWN_SECS` 相当（既定 60 秒）にする**。0 にすると、直後に「次に落とされる worker」へ
   再 prefetch されて NAK が連鎖し、`MAX_DELIVER = 5` を食い潰して**ゲストが一度も失敗していないジョブが
   DLQ で恒久的な失敗になる**（レビュー ops/major の指摘）。
   なお §4.3 の permit 規律により **over-fetch しない**ので、定常状態では NAK 対象がほぼ空になる。
   NAK 件数は `faas_worker_drain_naked_total` で観測する。
4. **in-flight の完了待ち**: `credits.acquire_many(total_credits)` を `timeout(drain_timeout)` で待つ。
   全 permit を取れた = 全タスク完了。
5. タイムアウトしたら残りを諦めてプロセス終了。`faas_worker_drain_abandoned_total` を inc し warn。
   放棄分は §4.6.1 の再配送経路に落ちる（保険が効く）。
6. **`/readyz` は drain 中 503**: `spawn_metrics_server`（crates/worker/src/main.rs:351-387）に
   `Arc<AtomicBool>` を渡し、`readyz`（同 :358-361、現在は無条件 200）が draining なら 503 を返す。
   同 :344-349 の設計メモは「**依存疎通を probe しない**（cascading restart 防止）」と述べているが、
   ドレインは依存の健全性ではなく**プロセスのライフサイクル状態**なので方針と衝突しない。コメントに明記する。

> **不変条件（§8 #7）**: `WORKER_DRAIN_TIMEOUT_SECS < ACK_WAIT_SECS`。
> 既定 `ack_wait = 30`（crates/shared/src/lib.rs:638）を超えてドレインすると、まだ実行中のジョブが
> JetStream 側で再配送され二重実行になる。`.env.example` は `WORKER_DRAIN_TIMEOUT_SECS=25` を出荷し、
> worker 起動時に `drain_timeout >= ack_wait` なら **fail-fast** する（CP と違い worker は両方の値を知っているので検査できる）。

### 4.7 設定の解決順序（既存の作法に完全準拠）

`AdmissionConfig::resolve_for_tenant`（crates/control-plane/src/state.rs:44-88）を拡張する。
**順序は既存と同一**（グローバル既定 → `tenants.quotas` 上書き → 推奨上限クランプ）。

| 項目 | グローバル既定 | env | `quotas` キー | クランプ上限 | 使われ方 |
| --- | --- | --- | --- | --- | --- |
| `max_concurrent_executions` | 20（config.rs `DEFAULT_MAX_CONCURRENT`） | `QUOTA_MAX_CONCURRENT_EXECUTIONS` | `max_concurrent_executions` | 200（state.rs:24） | admission ゲート 2 **＋** lane の `max_ack_pending`（+ headroom） |
| `lane_concurrency` | **4**（新 `DEFAULT_LANE_CONCURRENCY`） | `WORKER_LANE_CONCURRENCY` | `lane_concurrency` | **32**（新 `QUOTA_MAX_LANE_CONCURRENCY`、state.rs:24 の直後に置く） | lane consumer の `metadata` に載り、worker の per-lane クレジットの上限になる |

`ResolvedAdmissionParams`（state.rs:112-116）に `pub lane_concurrency: u64` を足す。
既存 2 ゲートはこのフィールドを見ないので **admission の挙動は 1 ビットも変わらない**。
マージは既存の `clamp_quota_u64`（state.rs:95 付近）を再利用する。

### 4.8 新 env の一覧（単一所有・重複定義なし）

**CP 側**（config.rs の `DEFAULT_*` const → 構造体フィールド → `env_u64` / `env_bool` の 3 点セットに従う）:

| env | 既定 | 意味 |
| --- | --- | --- |
| `TENANT_LANES_ENABLED` | `false`（M8-4 で `.env.example` を `true` にする） | lane を使うか。false は legacy 共有 consumer 1 本（緊急退避スイッチ, §9.2） |
| `MAX_DEDICATED_LANES` | `64` | 専有 lane の上限。超過分は overflow lane へ |
| `LANE_ACK_PENDING_HEADROOM` | `8` | 再配送スラック（§7.2） |
| `LANE_OVERFLOW_ACK_PENDING` | `1000` | overflow lane の固定 `max_ack_pending`（§3.3） |
| `WORKER_LANE_CONCURRENCY` | `4` | lane 並列度のグローバル既定。**CP が解決し metadata で配る**（worker も同名 env をフォールバックとして読む） |
| `LANE_RECONCILE_INTERVAL_SECS` | `10` | reconcile 周期 |
| `BACKOFF_SECS` | `5,15,60` | **worker から CP へ所有移管**（§3.5）。lane consumer config と token exp の唯一の真実 |
| `SCALE_POLL_INTERVAL_SECS` | `0`（無効。`.env.example` は `5`） | backlog ポーラの周期（§5.2） |
| `SCALE_JOBS_PER_WORKER` | `32` | desired の分母。**`WORKER_MAX_CONCURRENCY` と揃える**（§5.3） |
| `SCALE_MIN_WORKERS` | `1` | 0 で scale-to-zero を許可（opt-in） |
| `SCALE_MAX_WORKERS` | `4` | ローカルの PG 接続数から（§5.3） |
| `SCALE_IN_COOLDOWN_SECS` | `60` | scale-in ヒステリシス |
| `SCALE_SIGNAL_STALE_SECS` | `30` | シグナル陳腐化の閾値 |

**worker 側**（crates/worker/src/main.rs:247-278 の `Settings::from_env` / `env_u64`（:312 付近）に従う）:

| env | 既定 | 意味 |
| --- | --- | --- |
| `WORKER_MAX_CONCURRENCY` | `32` | **プロセス全体**の同時実行上限（H3 の直接の解）。**この env の所有者は worker のみ** |
| `WORKER_LANE_CONCURRENCY` | `4` | consumer metadata が読めない / 壊れているときのフォールバック |
| `LANE_DISCOVERY_INTERVAL_SECS` | `10` | 周期 discovery（即時通知の保険, §3.7.4） |
| `LANE_ROTATION_SECS` | `30` | lane 数 > 容量のときの回転周期（§4.4） |
| `WORKER_DRAIN_TIMEOUT_SECS` | `0`（`.env.example` は `25`） | SIGTERM ドレイン。`ACK_WAIT_SECS` 未満であること（起動時 fail-fast） |
| `WORKER_SLOT` | 未設定 | supervisor が注入する slot 番号。`faas_worker_slot` gauge に出す（§5.5 の同一性検証） |
| `ACK_WAIT_SECS` / `MAX_DELIVER` / `BACKOFF_SECS` | — | **撤去**（CP へ所有移管, §3.5）。ただし `ACK_WAIT_SECS` はドレイン上限の検査に必要なので**読み取り専用で残す**（consumer 設定には使わない） |

> **`WORKER_MAX_CONCURRENCY` と `SCALE_JOBS_PER_WORKER` の関係（MUST から降格した理由）**:
> 両者が乖離すると desired が実容量とズレる。しかし **CP は worker の env を知らないので検査できない**。
> したがって「MUST」ではなく「既定値を一致させたうえで、運用者が実測して決める分母」と定義し直す。
> 静かなズレを検出できるようにするため、(1) worker は `faas_worker_concurrency_limit` gauge を出す、
> (2) `/internal/scale` は `jobs_per_worker` を応答に含める、(3) chaos_m8 は両者の一致を**前提 assert**して
> 不一致なら明確なメッセージで panic する。

---

## 5. 弾力スケール — signal / decision / actuation

### 5.1 3 層モデルとスコープの線引き

| 層 | 内容 | 判定 | 根拠 |
| --- | --- | --- | --- |
| (i) signal | 未消化仕事量を正しく計測して公開する | **in-repo で実装（必須）** | 「何が backlog か」はプラットフォームの定義であり外部化できない。特に `num_pending` 単体は誤り（§5.2）。worker 0 台でも読めなければ scale-from-zero が原理的に不可能 |
| (ii) decision | backlog → 目標台数 | **in-repo で実装（純関数）** | 完了条件が「自動増減する」である以上、増減規則は本リポジトリの成果物。純関数にすれば CI（services 無し）でも毎コミット検証できる唯一の部分になる |
| (iii) actuation | 実際にプロセスを増減する | **in-repo に参照実装を 1 本だけ（`scripts/`）。プロダクション実装（KEDA/HPA）は非スコープ** | §5.5 |

**却下案 A: CP が worker 子プロセスを spawn する。** CP は Ed25519 署名秘密鍵を持つ発行系であり
（仕様書.md:246-248）、worker は鍵を持たない別トラストドメイン（同 245）。CP にプロセス起動能力を与えると
信頼境界が崩れる。加えて CP は「ステートレス x N」が前提（仕様書.md:723-728）で、
N 台が各自スケジューラを名乗ると台数が N 倍に暴走する。

**却下案 B: worker 自身が自己複製する。** 0 台からの起床が構造的に不可能。

**却下案 C: actuation を完全非スコープにする。** リポジトリ内のどのコードも worker 台数を変えないため、
chaos で「自動増減する」を 1 行も assert できない。M7 が確立した
「完了条件は必ず CI と chaos に割り付ける」作法（docs/M7-design.md:2023-2038）に反する。

**採用案: 単一インスタンスの外部プロセス（bash）。** KEDA/HPA が本番で果たす役割そのものを、
ローカルで最小コストで再現する。Rust の新 crate にしない理由は 3 つ:
(1) workspace members とビルド時間を増やさない、
(2)「これは製品コードではなく ops glue である」という位置づけがファイル形式で自明になる、
(3) `cargo build` を挟まずに編集→即実行できるため実測イテレーションが速い。

### 5.2 backlog シグナル（M8-8）

#### 5.2.1 定義

```
backlog = Σ over all lanes ( num_pending + num_ack_pending )
```

- **`num_pending` 単体を使ってはならない。** 1 worker が大量に claim した瞬間 `num_pending` は 0 に落ちるが
  仕事は終わっていない。`num_ack_pending` は「配送済みだが未 ack」= まさに未完了の仕事なので、
  両者の和が唯一正しい「未消化の仕事量」である。worker が突然死した場合も、抱えていたメッセージは
  `ack_wait` 経過まで `num_ack_pending` に残り backlog から消えない（堅牢）。
- **lane 別に取ることで、M8 の 2 つの目的が 1 本の走査で満たされる**: 合計は autoscale の入力になり、
  lane 別の値は「分離が配送層で効いている」ことの直接証拠になる（§6）。

#### 5.2.2 取得 API（async-nats 0.38.0 のソースで確認済み）

| 用途 | API | 実在確認 |
| --- | --- | --- |
| **採用**: 全 lane を 1 パス走査 | `Stream::consumers() -> Consumers`（`Item = Result<consumer::Info, _>`） | async-nats-0.38.0/src/jetstream/stream.rs:1058-1067 |
| 単一 lane の depth | `Stream::consumer_info<T: AsRef<str>>(&self, name: T) -> Result<consumer::Info, crate::Error>` | 同 stream.rs:887-890（`CONSUMER.INFO.{stream}.{name}` への RPC） |
| フィールド | `Info { num_pending: u64, num_ack_pending: usize, num_waiting: usize, num_redelivered: usize, config: Config, .. }` | src/jetstream/consumer/mod.rs:134-165 |
| 不採用: `Consumer::info()` | `&mut self` を要求する | 同 mod.rs:147。worker は immutable 束縛で持つので使えない |
| 不採用: `Consumer::cached_info()` | ネットワークに出ない作成時スナップショット | 同 mod.rs:177 |
| 明確に不適: `Stream::info_with_subjects()` | subject 単位の件数。WorkQueue では ack で消えるので使えなくはないが、consumer の未 ack を数えないため backlog にならない | stream.rs:216 付近 |

#### 5.2.3 ポーラの実装（`reaper::run_secret_kid_gauge`（reaper.rs:208-236）の派生）

```rust
pub async fn run_lane_depth_gauge(state: AppState, interval_secs: u64) {
    let period = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    tracing::info!(interval_secs = period.as_secs(), "jetstream lane depth poller started");

    let mut stream: Option<Stream> = None;
    let mut last_ok = std::time::Instant::now();
    let mut consecutive_failures: u64 = 0;

    loop {
        ticker.tick().await;
        let m = state.metrics();                 // ★ match の外で束縛する（両アームで使う）
        if stream.is_none() {
            match state.jetstream().get_stream(faas_shared::INVOKE_STREAM_NAME).await {
                Ok(s) => stream = Some(s),
                Err(e) => { note_failure(&mut consecutive_failures, &e); continue }
            }
        }
        let Some(s) = stream.as_ref() else { continue };

        match collect_all_lane_infos(s).await {
            Ok(infos) => {
                consecutive_failures = 0;
                last_ok = std::time::Instant::now();
                // lane が消えたら系列も消す（reaper.rs:221 と同じ理由で reset してから set）。
                m.lane_pending_messages.reset();
                m.lane_ack_pending.reset();
                let mut backlog: u64 = 0;
                for i in &infos {
                    m.lane_pending_messages.with_label_values(&[&i.name]).set(i.num_pending as i64);
                    m.lane_ack_pending.with_label_values(&[&i.name]).set(i.num_ack_pending as i64);
                    backlog += i.num_pending + i.num_ack_pending as u64;
                }
                m.scale_backlog.set(backlog as i64);
                m.scale_signal_age_seconds.set(0);
                state.scale().observe(backlog, infos.len());   // §5.3 の判断ロジックへ渡す
            }
            Err(e) => {
                // ★ run_secret_kid_gauge (reaper.rs:221) と決定的に違う点:
                //   backlog 側の gauge を reset しない。0 に落とすと「仕事が無い」と誤読され、
                //   NATS の一時的な瞬断がそのまま scale-to-zero を誘発する。
                m.scale_signal_age_seconds.set(last_ok.elapsed().as_secs() as i64);
                stream = None;                      // 次周期は get_stream からやり直す
                note_failure(&mut consecutive_failures, &e);
            }
        }
    }
}
```

**失敗時の挙動（設計上の要）**:
1. backlog gauge を**リセットしない**（前回値を保持）。この非対称性はコードコメントで明記する。
2. `faas_scale_signal_age_seconds` が伸びる。判断ロジックはこれを見て Hold に落ちる（§5.3 R1）。
3. ログは 1 回目 warn、以後は 60 周期に 1 回（新規スタックでは consumer 不在が続くため抑制が要る）。
4. best-effort（reaper.rs:65-71 / :230-233 の「観測の失敗で本流を止めない」規約）。

`SCALE_POLL_INTERVAL_SECS` は新 env（既定 **0 = ポーラを spawn しない**、`.env.example` は 5）。
reaper の 30 秒を再利用しない理由: 30 秒古い depth で判断すると scale-out が最大 30 秒遅れ、
それがそのままレイテンシの悪化になる。crates/control-plane/src/main.rs:258 のコメント
（「頻度を要さない観測なので専用 env は増やさない」）とは逆の判断であり、その差分を新 env の doc に書く。

> **scale-from-zero が成立する根拠（M8-3 の帰結）**: lane / legacy consumer を作るのは **CP** であり
> （§3.7）、durable consumer はクライアント接続と独立にサーバ側状態として残る。したがって
> **worker 0 台でも `CONSUMER.INFO` は成功する**。これが scale-from-zero の成立条件であり、
> §1.4 で述べた「弾力スケール単独先行ではブートストラップ・デッドロックする」問題の解消そのものである。
> **`inactive_threshold` を設定してはならない**（§8 不変条件 #4）。

### 5.3 判断ロジック — `crates/control-plane/src/scale.rs`（新規・純関数）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalePolicy {
    pub jobs_per_worker: u32,        // SCALE_JOBS_PER_WORKER
    pub min_workers: u32,            // SCALE_MIN_WORKERS（0 で scale-to-zero を許可）
    pub max_workers: u32,            // SCALE_MAX_WORKERS
    pub scale_in_cooldown_secs: u64, // SCALE_IN_COOLDOWN_SECS
    pub stale_after_secs: u64,       // SCALE_SIGNAL_STALE_SECS
}

#[derive(Debug, Clone, Copy)]
pub struct ScaleSignal { pub backlog: u64, pub age_secs: u64, pub ever_observed: bool }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason { NoSignalYet, StaleSignal, ScaleInCooldown }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision { pub target: u32, pub hold: Option<HoldReason> }

pub struct ScaleState { target: u32, below_since_secs: Option<u64> }

impl ScaleState {
    /// ★ `Default` を導出しない。初期 target は 0 ではなく **min_workers**。
    ///    0 から始めると「CP 再起動直後 + NATS 瞬断」で desired 0 が出て全台停止する。
    pub fn new(p: &ScalePolicy) -> Self { Self { target: p.min_workers, below_since_secs: None } }
}

/// 時計は引数（`now_secs`）で受け取り、内部で `Instant::now()` を呼ばない（CI で決定的）。
pub fn decide(p: &ScalePolicy, sig: &ScaleSignal, st: &mut ScaleState, now_secs: u64) -> Decision;
```

**規則（すべて全列挙で検証可能）**:

- **R0（境界は全経路の事後条件）**: `decide` の**あらゆる return 経路**で
  `target = target.clamp(min_workers, max_workers)` を通す。hold 経路も例外にしない。
  `min > max` の設定は `Config::from_env` で fail-fast。
- **R1（未観測 hold）**: `!sig.ever_observed` なら `hold = NoSignalYet`、`target = min_workers`。
  `/internal/scale` はこのとき **503** を返す（§5.4）。
- **R2（stale hold）**: `sig.age_secs > p.stale_after_secs` なら `st.target` を保持し `hold = StaleSignal`。
  **R0 により `min_workers` を下回らない**。`/internal/scale` はこのときも **503**（アクチュエータの
  「取れなければ現状維持」経路を必ず踏ませ、desired を出さない）。
- **R3（生の目標）**: `raw = backlog.div_ceil(jobs_per_worker)`、`want = raw.clamp(min, max)`。
- **R4（scale-out は即時）**: `want > st.target` なら cooldown 無しで即座に反映。
  遅らせることはレイテンシへの直撃であり、増やしすぎのコストは減らせば済む。
- **R5（scale-in はヒステリシス）**: `want < st.target` のとき `below_since` を記録し、
  `now - below_since >= scale_in_cooldown_secs` を満たして初めて反映。途中で `want` が戻ったら `below_since = None`。
  満たさない間は `hold = ScaleInCooldown`（desired は出す = 現在の target）。
- **R6（scale-to-zero）**: `target == 0` に到達できるのは `min_workers == 0` かつ `backlog == 0` のときのみ（R3 から自明）。

**CI の全列挙テスト**:

- `desired_is_monotone_in_backlog` — `backlog = 0..=4096` を全列挙し出力が単調非減少。
- `desired_never_leaves_bounds` — 全 `backlog` × 代表 policy × **hold 付きケースを含めて** `min ≤ out ≤ max`（R0）。
- `ceil_boundary_increments_at_each_multiple` — `backlog = k*jpw` と `k*jpw + 1` の境界で target が +1（全 `k`）。
- `scale_out_is_immediate` / `scale_in_requires_cooldown` — 合成時計で決定的。
- `initial_state_targets_min_workers` — `ScaleState::new` の初期 target が `min_workers`。
- `fresh_state_with_stale_signal_returns_min_not_zero` — **CP 再起動直後 + NATS 断が全台停止に化けない**回帰ガード。
- `stale_hold_never_returns_below_min_workers` — R0 と R2 の相互作用。
- `min_greater_than_max_is_rejected_at_config_load` — fail-fast。

**既定値と根拠**:

| env | 既定 | 根拠 |
| --- | --- | --- |
| `SCALE_JOBS_PER_WORKER` | `32` | `WORKER_MAX_CONCURRENCY` の既定と一致（§4.8） |
| `SCALE_MIN_WORKERS` | `1` | **scale-to-zero は opt-in**。M6 の同期 invoke はサーバ側 `SYNC_REPLY_TIMEOUT_MS=5000`（Makefile:78）で、from-zero coldstart（§5.6）はこの窓を容易に超えるため、既定 0 は M6 の完了条件を壊す |
| `SCALE_MAX_WORKERS` | `4` | worker 1 台が PG 接続を最大 8 本張る（crates/worker/src/main.rs:436）。postgres:16 の既定 `max_connections=100`（docker-compose.yml で上書きしていない）に対し CP も接続するので、ローカルでは 4 × 8 = 32 が安全圏 |
| `SCALE_IN_COOLDOWN_SECS` | `60` | `ack_wait = 30`（crates/shared/src/lib.rs:638）より長く取る。scale-in 直後に再配送が走ると backlog が跳ねて flapping する。§4.6.2 の NAK delay もこの値に揃える |
| `SCALE_SIGNAL_STALE_SECS` | `30` | poll 周期 5 秒の 6 倍 |

> **重要な相互作用**: backlog は CP の admission（in-flight ≤ `max_concurrent_executions`、既定 20）で頭打ちになる。
> 1 テナントのみの環境では `backlog ≤ 20` なので `desired ≤ ceil(20/32) = 1`。これは**バグではなく設計上の性質**で、
> 「プラットフォームは Σクォータを超えてスケールさせられない」という保証になる。
> 負荷試験時は CP を `QUOTA_MAX_CONCURRENT_EXECUTIONS=200` で起動するか、テナントの `quotas` を上書きする必要がある
> （§10.3 の chaos 前提に必須項目として明記する）。

### 5.4 露出面 `GET /internal/scale`

内部専用 listener（crates/control-plane/src/main.rs:271-286、`build_internal_router` は同 :317-321、
既定 `127.0.0.1:8081`）に 1 ルート追加する。

```json
{
  "stream": "FAAS_INVOKE",
  "lanes": 3, "unsubscribed_subjects": 0,
  "backlog": 144, "signal_age_secs": 2,
  "desired": 4, "hold_reason": null,
  "min_workers": 0, "max_workers": 4, "jobs_per_worker": 32,
  "poll_interval_secs": 5, "scale_in_cooldown_secs": 60, "stale_after_secs": 30
}
```

- **`poll_interval_secs` / `scale_in_cooldown_secs` / `stale_after_secs` を必ず含める**（レビュー指摘の反映）。
  chaos はタイムアウト予算を**この応答から算術で導出**し、値をハードコードしない。
  これらはテナントデータではないので不変条件 #5 に抵触しない。
- `SCALE_POLL_INTERVAL_SECS == 0`（ポーラ無効）/ `hold == NoSignalYet` / `hold == StaleSignal` のときは
  **503 + `{"error":"scaler_unavailable","reason":"..."}`** を返し、**`desired` を出さない**。
  `desired: 0` を返すとアクチュエータが全台停止させてしまう。
- **公開 listener（`BIND_ADDR`）に生やさない理由 / 認証を課さない理由**: 内容は stream 集計値のみで
  テナント名も execution_id も含まない。したがって `Principal` を必要とせず RLS の対象でもない。
  内部 listener をインターネットへ公開してはならない旨は main.rs:264-270 に既に MUST NOT として書かれており、
  同じ posture を継承する。
- **`build_internal_router` の doc コメント（main.rs:305-316）を更新する**（レビュー指摘）。
  現在の doc は「`POST /internal/job-env` だけを載せる」「認証は env-token の署名そのもの」を宣言しているが、
  M8 が足すのは署名を持たない GET である。**「ルート 2 種で認証根拠が異なる（job-env=署名 /
  scale=無認証だがテナントデータ非含有）」**ことと、不変条件 #5 を doc に転記する。
- **worker から到達可能であることを前提に設計する**（§8 不変条件 #5）。この listener は worker が
  `POST /internal/job-env` を叩く先であり、worker は Ed25519 鍵を持たない別トラストドメイン（仕様書.md:245）である。
  したがって「worker に見せてよい情報だけを載せる」。キュー深さと目標台数はこれを満たす。

### 5.5 参照アクチュエータ（M8-9）

#### 5.5.1 ローカルで N 個の worker を動かす際の障害物（すべて実測済み）

| 障害 | 事実 | 対処 |
| --- | --- | --- |
| `cargo run` の直列化 | Makefile:157-158 の `run-worker` は `cargo run -p faas-worker` 1 行。N 回叩くと `target/` のビルドロックで直列化する | 事前に `cargo build -p faas-worker` し、`target/debug/faas-worker` を直接 N 個起動する |
| **メトリクスポート衝突** | 既定 `0.0.0.0:9090`（crates/worker/src/main.rs:233）。bind 失敗は warn して task が `return` するだけで**プロセスは無言で稼働継続**（同 :376-380） | (1) `WORKER_METRICS_PORT_BASE` の既定を **9101** にする（9090 と衝突させない）。(2) supervisor が `METRICS_BIND_ADDR=127.0.0.1:$((BASE+i))` と `WORKER_SLOT=i` を割り当てる。(3) 起動後 `/readyz` を最大 10 秒ポーリングし、応答しなければ supervisor 側で loud に失敗させる（**worker の互換挙動を変えずに沈黙を潰す**） |
| **手動 worker の混入** | README の既存手順（README.md:805-810）は `make run-worker` を指示する。それが 9090 を掴んだまま `make autoscale` を回すと、supervisor が 1 台も起動していなくても `live_workers()` が 1 以上になりうる | (1) ポート基点を分離（上記）。(2) プローブは `/metrics` を読み `faas_worker_slot` の値が期待 slot と一致することまで確認する。(3) chaos の前提チェックで「9090 に応答する supervisor 管理外の worker が居ないこと」を確認し、居たら明示 panic |
| cwasm ディレクトリ共有 | 既定 `./worker-cache`（crates/worker/src/main.rs:231）。書き込みは `.{name}.{pid}.{nanos}.tmp` → rename のアトミック書き込みでファイルロックを取らない（同 :1774-1797 付近） | **共有して安全かつ有利**。2 台目以降は precompile を丸ごとスキップできる。k8s の emptyDir では共有されない点を注意書きする（§5.6） |
| env 不一致 | 全 worker に**同一 env** を渡さないと挙動が割れる | supervisor は自身の環境をそのまま継承して渡す。README に MUST として明記 |
| PG 接続数 | 1 worker = 最大 8 接続（crates/worker/src/main.rs:436） | `SCALE_MAX_WORKERS` 既定 4（§5.3） |

#### 5.5.2 docker-compose に worker サービスを足すべきか → **足さない**

1. **Dockerfile が 1 つも存在しない**。追加は「コンテナビルド」という新しい軸を持ち込み、
   内側ループが `編集 → イメージ再ビルド → compose up` になって実測サイクルが著しく遅くなる。
2. **`docker compose up --scale worker=N` は per-replica の固定ホストポートを公開できない**。
   これは chaos_m8 が「生きている worker 台数」を観測する唯一の非 shell-out 手段（§10.2）を破壊する。
   代替は `docker compose ps` の shell-out だが、chaos_m4.rs:81-85 が「compose の worker サービスが要るから」
   という理由で `#[ignore]` に退避した経緯があり、テストから `docker compose` を呼ぶ方針は
   chaos_m7.rs:52-55 が明示的に却下している（「環境依存が強すぎる」）。
3. compose 化しても第 3 層の一実装に過ぎず、第 1-2 層の設計は 1 ミリも変わらない。

> なお README.md:899 の `docker compose logs control-plane worker` は compose に該当サービスが無いため
> **現時点で既に空振りする**。M8-Z の README パスで修正する。

#### 5.5.3 スクリプトと Makefile

```
scripts/run-workers.sh   N            # 固定 N 台を起動（負荷試験・手動デバッグ用）
scripts/worker-autoscale.sh           # /internal/scale をポーリングして増減させる参照アクチュエータ
scripts/stop-workers.sh               # 全台を drain 停止
```

```sh
#!/usr/bin/env bash
# scripts/worker-autoscale.sh（骨子）
set -euo pipefail
set -a; [ -f ./.env ] && . ./.env; set +a     # 全 worker に同一 env を配る
INTERNAL_URL=${CONTROL_PLANE_INTERNAL_URL:-http://127.0.0.1:8081}
BIN=${WORKER_BIN:-target/debug/faas-worker}
RUNDIR=${WORKER_RUNDIR:-.workers}; PORT_BASE=${WORKER_METRICS_PORT_BASE:-9101}
: > "$RUNDIR/heartbeat"                        # chaos の前提 assert 用（§10.3）
while :; do
  # HTTP 200 のときだけ desired を読む。503（未観測 / stale / 無効）や取得失敗は「現状維持」。
  body=$(curl -sS --max-time 2 -w '\n%{http_code}' "$INTERNAL_URL/internal/scale" || true)
  code=$(printf '%s' "$body" | tail -n1)
  if [ "$code" = "200" ]; then
    desired=$(printf '%s' "$body" | sed -n 's/.*"desired":[[:space:]]*\([0-9]*\).*/\1/p')
    poll=$(printf '%s' "$body" | sed -n 's/.*"poll_interval_secs":[[:space:]]*\([0-9]*\).*/\1/p')
    if [ -n "$desired" ]; then
      reap_dead_pids
      while [ "$(live_count)" -lt "$desired" ]; do start_worker "$(next_free_slot)"; done
      while [ "$(live_count)" -gt "$desired" ]; do stop_worker "$(highest_slot)"; done
    fi
  fi
  date +%s > "$RUNDIR/heartbeat"
  sleep "${poll:-${SCALE_POLL_INTERVAL_SECS:-5}}"
done
```

`start_worker i`: `WORKER_MAX_CONCURRENCY` / `WORKER_LANE_CONCURRENCY` / `WORKER_DRAIN_TIMEOUT_SECS` /
`METRICS_BIND_ADDR=127.0.0.1:$((PORT_BASE+i))` / `WORKER_SLOT=i` / `WASM_CACHE_DIR` を渡して `nohup` 起動 →
PID を `$RUNDIR/worker-$i.pid` に記録 → `/readyz` を最大 10 秒待って来なければ loud fail。
`stop_worker i`: SIGTERM → `WORKER_DRAIN_TIMEOUT_SECS + 5` 秒待つ → まだ生きていれば SIGKILL + warn。

Makefile には `# --- M8: 弾力スケールとアイソレーション（§8 / §10 / §15 M8） ---` バナーの下に
`run-workers` / `autoscale` / `stop-workers` / `lane-status`（`jsz` を叩く）を追加し、
`.PHONY`（Makefile:97）と `## 日本語 1 行説明` を必ず付ける。
**ターゲット名に数字を入れない**（help の grep は `^[a-zA-Z_-]+:` なので `scale-m8` は `make help` に出ない、Makefile:102）。

### 5.6 scale-to-zero / from-zero coldstart（§3.6 キャッシュとの結合）

#### 5.6.1 worker 0 台のとき何が起きているか

- CP は通常どおり `invoke_subject(tenant)` へ publish する（crates/control-plane/src/enqueue.rs:298-316）。
  WorkQueue stream なのでメッセージは ack されるまで滞留する。
- lane consumer は **CP が作った**サーバ側状態として残り、`num_pending` が積み上がる（§5.2.3）。
- **`ack_wait` のタイマーは「配送された時点」から始まる**ので、滞留中は再配送カウントを消費しない。
- **ジョブトークンの exp は消費される。** `exp = iat + ack_wait*max_deliver + wall + margin`
  （crates/shared/src/lib.rs:653-664、既定 `210 + wall` 秒）。ただし仕様書 §3.3（仕様書.md:256）と
  subscriber.rs:391-395 により、**行が pending/running かつ署名内容が DB と整合すれば失効トークンでも
  結果を受理する**（「有効期限切れのみを理由に正規の結果を破棄してはならない (MUST NOT)」）。
  したがって実行喪失は起きない。失われるのは「リプレイ窓を絞る」効果のみ。
- **真の上限は stuck-execution sweeper。** `created_at < now() - interval` で pending/running を failed に落とす
  （crates/control-plane/src/db.rs:1610-1617 付近、既定 `STUCK_EXECUTION_DEADLINE_SECS=900`）。
  **起点が enqueue 時刻なので、誰も実行していないのに 900 秒で偽 failed になる。**

> **設計判断**: sweeper の起点を変えない（`started_at` 基準にすると「running のまま無音失踪した worker」を
> 救えなくなり §6.6 の MUST を壊す）。代わりに、アクチュエータが backlog > 0 を検出してから worker が立つまでを
> **1 ポーリング周期 + プロセス起動時間 ≒ 10〜20 秒**に抑えることで、900 秒には遠く及ばない構造にする。
> 900 秒に到達し得るのは「アクチュエータが死んでいる」場合だけで、それは `faas_scale_signal_age_seconds` と
> `faas_scale_backlog` の 2 gauge、および supervisor の heartbeat ファイルで検出できる（README のトラブルシュートに記載）。

#### 5.6.2 何が失われ、何が残るか

| 対象 | プロセス消滅時 | 根拠 |
| --- | --- | --- |
| in-memory Component LRU（cap 64） | **失われる** | crates/worker/src/main.rs:465-467 / :103。`Mutex<LruCache<String, Arc<Component>>>` はプロセスローカル |
| wasmtime `Engine` 上のコンパイル済みコード | **失われる** | 同 :449 付近（`build_engine`, :622） |
| NATS / PG 接続 | 失われる | 同 :429 / :436 |
| `{WASM_CACHE_DIR}/{sha}.cwasm` | **残る** | 同 :1774-1797 付近（tmp + rename、ロック無し）。ローカルでは全 worker で共有。k8s の emptyDir では Pod ごとに消える |
| stream 内の invoke メッセージ | 残る | WorkQueue: ack されるまで残る |
| lane consumer の ack floor / 配送状態 | 残る | サーバ側状態（CP が作成者、§3.7） |
| `executions` 行・in-flight カウンタ | 残る | DB / Redis |

したがって from-zero の coldstart は「**プロセス初期化 + cwasm deserialize**」であり、
S3 からの再ダウンロードと再コンパイルは **cwasm ファイルが残っている限り発生しない**。
この主張は既存の計器で直接検証できる: `wasmtime_component_cache_hits_total{tier="cwasm"}` が +1
（crates/worker/src/main.rs:1142-1150）かつ `wasmtime_component_cache_misses_total`（同 :1163）が +0。

#### 5.6.3 coldstart 短縮のために実装すること / しないこと

| 施策 | 判定 | 根拠 |
| --- | --- | --- |
| cwasm の**事前 warm**（起動時に cwasm を LRU へプリロード） | **実装しない（測ってから決める）** | `deserialize_file` 経路と LRU hit の差は mmap 相当で、プロセス初期化（NATS/PG 接続 + Engine 構築）に比べて桁が小さいと見込まれる。加えて「どの component を warm すべきか」を worker は知らない。§10.3 の U4 で `started_at - created_at` を実測し、cwasm hit の寄与が支配的だと判明した場合に初めて `WORKER_WARM_CACHE_MAX` を導入する |
| `WASM_CACHE_DIR` を worker 間で共有する運用の明文化 | **実装する（ドキュメントのみ）** | 既に安全。共有により 2 台目以降の scale-out は precompile を完全に回避できる。k8s では node ローカルボリューム（hostPath / local PV）に相当する、と README に書く |
| `InstancePre` / Pooling アロケータ | **M9 へ** | §含まない |

---

## 6. 観測 — 既存メトリクスを 1 つも変えない

docs/M7-design.md:1014 の規律（「既存メトリクスにラベルを足さず、新カウンタを別に足す」）と
crates/control-plane/src/metrics.rs:28-32 の方針（登録済みメトリクスは撤去しない）に従い、**新規追加のみ**とする。

### 6.1 命名規約の確定

worker の命名は現在 3 系統に割れている: `wasmtime_*`（crates/worker/src/metrics.rs:69/82/91/103 付近）、
無接頭辞（同 :112 `executions_total`, :124 `dlq_published_total`）、`faas_*`（同 :136 の
`faas_guest_stderr_dropped_bytes_total`、M7c 追加）。改名は不可能なので、**M8 は規約を確定して以後それに従う**:

> `wasmtime_` = wasmtime ランタイム固有の指標。`faas_worker_` = プラットフォーム横断の worker 指標。
> 無接頭辞は歴史的経緯であり新規には使わない。

4 系統目を作るのではなく、M7c が既に選んだ `faas_` へ寄せる統合である。

### 6.2 CP 側（crates/control-plane/src/metrics.rs）

`secret_versions_by_kid` の登録ブロック（metrics.rs:248-258）の直後に、同じ 3 行パターン
（new → register → 構造体フィールドは :111 の後ろ、`Arc::new(Self{..})` の列挙は :273 付近の後ろ）で追加する。

| 名前 | 型 / ラベル | 更新点 | 意味 |
| --- | --- | --- | --- |
| `faas_lane_pending_messages` | IntGaugeVec{lane} | `run_lane_depth_gauge`（§5.2.3） | lane 別の未配送数 |
| `faas_lane_ack_pending` | IntGaugeVec{lane} | 同上 | lane 別の配送済み未 ack |
| `faas_tenant_lanes` | IntGaugeVec{kind=dedicated\|overflow\|legacy} | `reconcile_lanes_once` 末尾 | lane 本数 |
| `faas_lane_overflow_tenants` | IntGauge | 同上 | overflow lane に相乗りしているテナント数（劣化モードの深さ） |
| `faas_lane_unsubscribed_subjects` | IntGauge | 同上 | **desired subject − 実 filter の差分件数。0 でなければ「誰も購読していない subject がある」** |
| `faas_lane_reconcile_total` | IntCounterVec{outcome=applied\|skipped_not_leader\|failed} | 同上 | 単一 writer が効いていることの証拠 |
| `faas_scale_backlog` | IntGauge | `run_lane_depth_gauge` | 全 lane 合計の未消化仕事量 |
| `faas_scale_desired_workers` | IntGauge | `decide` 呼び出し後 | 判断ロジックの出力 |
| `faas_scale_signal_age_seconds` | IntGauge | `run_lane_depth_gauge` | 最後に成功したポーリングからの経過秒 |

**カーディナリティ**: `lane` ラベルは **lane 数 = `min(T, MAX_DEDICATED_LANES) + 1` で有界**
（既定 65 系列が上限）。metrics.rs:9-12 の「tenant_id は付けない。テナント数 × ルート数で爆発する」という
方針は「テナント数 × **ルート数**」の積を問題にしており、lane gauge は積を作らない。
それでも上限を運用者が制御できるよう `METRICS_LANE_LABELS`（既定 true）で gate し、
false のときは `lane` ラベルを `"aggregate"` 1 値に畳む。
**既存の `faas_tenant_invoke_total`（metrics.rs:172）には一切触れない**
（README.md:1086-1087 の follow-up は M9 のまま据え置く）。

### 6.3 worker 側（crates/worker/src/metrics.rs、構造体は :38-64、`Metrics::init` の末尾に追加）

| 名前 | 型 | 更新点 | 意味 |
| --- | --- | --- | --- |
| `faas_worker_inflight_executions` | IntGauge | `InflightGuard`（§4.5） | 実行中ジョブ数 = 1 worker の実容量の実測値 |
| `faas_worker_concurrency_limit` | IntGauge | 起動時 | `WORKER_MAX_CONCURRENCY`（`SCALE_JOBS_PER_WORKER` とのズレ検出、§4.8） |
| `faas_worker_lanes` | IntGauge | discovery ループ | 提供中の lane 数 |
| `faas_worker_lanes_unserved` | IntGauge | 同上 | **提供できていない lane 数（§4.4 の劣化モード検出）** |
| `faas_worker_lane_credits` | IntGauge | 同上 | `effective_lane_concurrency` の現在値 |
| `faas_worker_lane_pull_blocked_total` | IntCounter | lane ループの `acquire_owned().await` が待った回数 | クレジットが binding していることの証拠 |
| `faas_worker_redelivered_total` | IntCounter | `delivered >= 2` を検出した回数（§4.3） | **二重実行の直接観測（§6.4）** |
| `faas_worker_drain_naked_total` | IntCounter | ドレイン時の NAK 件数 | §4.6.2 |
| `faas_worker_drain_abandoned_total` | IntCounter | ドレイン timeout 時の残件数 | 同上 |
| `faas_worker_slot` | IntGauge | 起動時（`WORKER_SLOT`） | supervisor 管理下であることの同一性検証（§5.5.1） |

### 6.4 二重実行を観測可能にする — 「なぜ result 側では検出できないか」

当初案は CP の `faas_result_finalized_total{outcome="stale"}`（`finalized.is_none()` かつ
`FinalizeOrigin::Result` の分岐、crates/control-plane/src/subscriber.rs:767-779）を
「二重実行ゼロの決定的な整数 assert」に使う設計だった。**これは検出力を持たないので撤回する。**

論証: worker は `handle_payload` が結果を publish した**後**に ack する（crates/worker/src/main.rs:559-592）。
scale-in で in-flight タスクが道連れになるケース（まさに §4.6.1 が問題視する経路）では、
1 回目の実行は結果を publish せずに死ぬ → 再配送 → 2 回目だけが publish する。
つまり **result は 1 通しか届かず `stale` は 0 のまま**で、ゲストは 2 回実行されている。
`stale` が立つのは「publish 成功後 ack 前に死んだ」極めて狭い窓だけである。
負の対照（`WORKER_DRAIN_TIMEOUT_SECS=0` なら > 0 になるはず）も同じ理由で成立しない。

> **採用する観測点は「配送側」**: worker の pull ループは `delivered` を既に取得している
> （crates/worker/src/main.rs:551）。`delivered >= 2` を `faas_worker_redelivered_total` で数える。
> これは「殺されたタスク由来の再配送」を直接捉える。加えて既存の `executions_total`
> （crates/worker/src/metrics.rs:112-122）の全 worker 合算 delta が投入件数 K と一致することを見る
> （K を超えたら再実行が起きている）。**どちらも整数カウンタの差分**であり、§10 の自己制約を保つ。
> `faas_result_finalized_total{outcome}` は観測値として有用なので追加してよい（`dlq_finalized_total`
> （metrics.rs:212）と対称）が、**完了条件の検証手段としては使わない**。

---

## 7. 縮退方針（fail-mode）

### 7.1 新しい失敗モードの分類

分類を型で宣言する場所を 1 箇所に保つという既存規律（crates/control-plane/src/store.rs:37-57）に従い、
`FailPolicy::TENANT_LANE = Open` を store.rs:56 の直後に置く（Store を経由しない分類だが、定数だけは同居させる）。
分類固定テスト（store.rs:1076 付近）に 1 行足す。

| 失敗 | 分類 | 挙動 | 記録（audit action） |
| --- | --- | --- | --- |
| **enqueue 時の lane ensure 失敗**（`CONSUMER.CREATE` の一時失敗） | **Open** | **publish を続行する** | warn + `audit_logs.action='lane_provision_degraded'`（**新設**。`admission_degraded` を再利用しない） |
| **enqueue 時 ensure を skip した（overflow 対象）** | 遅延契約 | reconcile が最大 `LANE_RECONCILE_INTERVAL_SECS`（既定 10 秒）以内に購読を張る | debug ログのみ。`faas_lane_unsubscribed_subjects` で可視 |
| **reconcile の advisory lock を取れない** | Static | そのラウンドを skip | `faas_lane_reconcile_total{skipped_not_leader}` |
| **reconcile の DB 列挙失敗** | Static | **既存 lane を一切変更しない**。パス全体を Err にして次周期（reaper.rs:79-86 の規約） | warn |
| **reconcile の個別 lane 操作失敗** | Static | そのテナントだけ skip し他は続行（reaper.rs:88-97 の規約） | per-tenant warn |
| **lane 削除条件（未処理 0）を満たさない** | Static | 削除を次周期へ繰り越す | warn（`num_pending` 付き） |
| **worker の consumer metadata が読めない / 壊れている** | Open | `WORKER_LANE_CONCURRENCY` の env 既定へ縮退 | warn |
| **backlog ポーラの失敗** | Static | gauge を保持し `signal_age` を伸ばす。判断ロジックは Hold（§5.3 R2）、`/internal/scale` は 503 | 1 回目 warn、以後 60 周期に 1 回 |
| **NATS publish 失敗** | 変更なし | 既存の `EnqueueError::PublishBackpressure`（enqueue.rs:314/326）→ 429（handlers.rs:828 付近） | 既存どおり |

**`fail_open_degraded` の一般化（レビュー指摘の反映）**: 現在の実体は
`async fn fail_open_degraded(state: &AppState, tenant: &str, class: &str, err: &StoreError)`
（crates/control-plane/src/admission.rs:129-146）で、(1) `pub` が付いておらず admission モジュール外
（enqueue.rs）から呼べない、(2) 第 4 引数が `&StoreError` で lane ensure の失敗（async-nats の consumer エラー）と
型が合わない、(3) 内部の `audit_degraded`（同 :149-174）が `action='admission_degraded'` を書くため、
NATS の consumer 作成失敗が **M4d の「Redis 縮退 = 上限が一時的に効いていない」証跡と同じ action に混入する**。
したがって:

1. `fail_open_degraded` を `pub(crate)` にする。
2. シグネチャを `(state, tenant, class, unavailable: bool, error: &dyn std::fmt::Display, action: &'static str)` へ
   一般化する（`StoreError::is_unavailable()` の呼び出しは admission 側の呼び出し元へ移す）。
3. `debug_assert` を「渡された class の `FailPolicy` が `Open` であること」を確認する形に一般化する
   （`INVOKE_RATE` / `INFLIGHT` / `TENANT_LANE`）。
4. lane は `action = "lane_provision_degraded"` で書く。M4d の縮退検知クエリ / ダッシュボードを汚さない。

**enqueue 時 lane ensure を fail-open にする理由**: 失敗しても
(1) メッセージは stream に残る（喪失しない）、(2) reconcile が lane を作れば配送される、
(3) 最悪でも stuck sweeper（既定 900 秒）が終端化する。逆に fail-closed（503）にすると、
NATS の一時的な RPC 失敗が invoke 全体を落とすことになり、**性能・分離のための機構が可用性インシデントになる**
（admission.rs:7-11 が invoke を fail-open にした理由と同一）。

**lane 削除とスケール判断を絶対に fail-open にしない理由**: lane を消すと配送が止まる。
台数判断を誤って 0 に倒すと全台が停止する。「観測できないときは現状維持（Static）」が正しい。

### 7.2 二重に効いて過剰に絞らないことの論証

受付層は `pending + running` を、配送層は「配送済み未 ack」を数える。両者の関係は:

> ack-after-publish（crates/worker/src/main.rs:547-566）により、配送済み未 ack のメッセージは
> **結果 publish がまだ成功していない**ものである。結果 publish が成功していない execution 行は
> `pending` か `running` のいずれかである。ゆえに **`ack_pending ⊆ (pending + running)`**。

したがって `max_ack_pending = max_concurrent_executions` に揃えれば、
**admission を通過した流量に対して配送層が先に binding することは無い**。

厳密には 1 つ例外がある。**再配送中のメッセージが、既に終端した行に対応しうる**
（stuck sweeper が先に `failed` に倒した後で再配送が届く等）。このとき `ack_pending` が瞬間的に
in-flight を上回る。これを吸収するために `max_ack_pending = max_concurrent_executions + LANE_ACK_PENDING_HEADROOM`
（既定 8）とする。headroom は「再配送スラック」であり、**配送層が受付層より先に binding しないための余白**である。
CI で `lane_ack_pending(r) == r.inflight.max + headroom` を固定する。

**バイパス経路（cron / chain）については逆に、配送層が binding するのが正しい。** 受付層を通らないので
in-flight が `max_concurrent_executions` を超えうるが、そのときは配送層が「そのテナント自身の枠」で
頭打ちにする。これは**過剰な絞りではなく、H2 という穴を塞ぐことそのもの**である。

**運用式（仕様書 §8:751 の Σ 目安式を置き換える）**:

> - **Σ を気にする必要は無くなった**（I2 により合算の頭打ちが消えたため）。
> - `worker 台数 W × WORKER_LANE_CONCURRENCY ≳ max_concurrent_executions`
>   （さもないと worker 側が binding して throughput が落ちる）。
> - `WORKER_MAX_CONCURRENCY ≳ アクティブ lane 数`（さもないと §4.4 の劣化モードに入る）。

### 7.3 M8 は新しい 429 を 1 つも作らない（意図的な設計判断）

- `faas_admission_rejections_total` の `kind` ラベル（metrics.rs:182-192 付近）に新しい値を足さない。
- `RateLimited` のバリアント（admission.rs:59/73/87 付近）を増やさない。
- `EnqueueError::PublishBackpressure` → 429 の写像も変えない。

理由: M8 の分離は**受付を絞ることではなく、受け付けたものを公平に捌くこと**で達成される。
新しい受付ゲートを足すと、M4d が固めた「429 は必ず `Retry-After` を持つ」等の契約
（admission.rs:287-315 付近の退行ガード）と、クライアントから見た 429 の意味論に触れる。
**M8 はクライアント可視の API 契約を 1 ビットも変えない。**

### 7.4 残る非対称（本書では直さないことを明示する）

cron / event / chain が受付層をバイパスするため、reaper の `count_inflight_executions` → resync が
バイパス経路の pending を後から Redis カウンタへ反映し、
**当該テナント自身の HTTP invoke が `concurrency_limit` で 429 になる**という非対称が残る。
これは**テナント内**の現象であり、**テナント間**の公平性（M8 の完了条件）とは別問題である。
直すには受付層に手を入れる必要があり §7.3 の方針と衝突するため、README の「含まない」に明示的に残す。

### 7.5 Redis 障害時 — M8 の最大の副次的利得

現状、公平性メカニズムは**すべて Redis 上にある**（store.rs の `RATE_LUA` / `RESERVE_LUA`）。
Redis 到達不能時は rate と in-flight が**同時に** fail-open し（admission.rs:192 / :219 付近）、
残る歯止めは全テナント合算の `MaxAckPending` だけになる。**つまり Redis は公平性の単一障害点である。**

M8 の分離は配送層（NATS consumer config）と実行層（worker のプロセス内クレジット）にあり、
**Redis に一切依存しない**。したがって:

> **Redis が落ちて admission が 2 ゲートとも fail-open しても、テナント間アイソレーションは保たれる。**
> M8 は §8 のリスク「fail-open は公平性の単一障害点」を**構造的に解消する**。
> これは副次的効果ではなく、採用案 (D) を選ぶ積極的な理由の 1 つである。

---

## 8. 不変条件の保全（明示セクション）

1. **ack セマンティクスを変えない。** ack-after-publish（crates/worker/src/main.rs:547-566）、
   `delivered >= max_deliver` での DLQ 自前 publish（同 :567-584）、un-ack による再配送は 1 行も変更しない。
   permit と `InflightGuard` は `tokio::spawn` の手前と中に入るだけである。
2. **I1（どの invoke subject もちょうど 1 consumer に属する）。** WorkQueue retention によりサーバが強制し、
   `assign_lanes` の全列挙テストが数学的に証明し、reconcile が「削除 → overflow 縮小 → 作成」の順序を
   advisory lock 下で守る。**設計・実装・サーバの 3 重で守られる。**
3. **stream の retention は起動時に検査する。** CP / worker とも `retention != WorkQueue` なら fail-fast（§3.4）。
   これが無いと I1 が「サーバ強制」から「たまたま」へ静かに退化する。
4. **`inactive_threshold` を設定しない。** durable consumer が worker 0 台のときに消えると
   scale-from-zero が壊れる（§5.2.3）。
5. **`/internal/scale` にテナント次元を足さない。** 認証を課していない根拠が「テナントデータを含まない」ことにあり、
   かつ **内部 listener 上のルートは worker（別トラストドメイン、仕様書.md:245）から到達可能**である。
   worker に見せてよい情報だけを載せる（§5.4）。
6. **stream に `max_msgs` / `max_bytes` / `max_age` を設定しない。** 設定する場合は
   `discard: DiscardPolicy::New` が必須（既定の `Old` は**最も古い = 他テナントのメッセージから捨てる**ため、
   完了条件 C2 を新設 config 自身が破る）（§3.4）。
7. **`WORKER_DRAIN_TIMEOUT_SECS < ACK_WAIT_SECS`。** 超えると実行中のジョブが再配送され二重実行になる。
   worker 起動時に fail-fast する（§4.6.2）。
8. **W1: pull ループの内側で permit を `await` しない。** メッセージを保持したまま待つと `ack_wait` を
   超えて二重実行になる（§4.1）。
9. **W2: 1 lane ループが保持する未使用 permit はすべて自 lane の専有資源。** 共有資源を待つ経路を作らないため
   hold-and-wait が原理的に成立しない（§4.1）。
10. **プロセス全体の上限は `effective_lane_concurrency` で構成的に閉じる。**
    `served_lanes * effective <= max(global, served_lanes)` を CI が全列挙で証明する（§4.2）。
11. **既存メトリクスを 1 つも変えない。** metrics.rs:28-32 の方針と M7 の作法（docs/M7-design.md:1014）。
    M8 は新規追加のみ（§6）。
12. **クライアント可視の API 契約を変えない。** 新しい 429 も新しい `kind` も作らない（§7.3）。
13. **TTL 結合の真実は 1 箇所。** `ACK_WAIT_SECS` / `MAX_DELIVER` / `BACKOFF_SECS` の所有を CP に一本化し、
    lane consumer config と `token_exp_offset_secs` の両方を同じフィールドから導出する（§3.5）。
14. **reconcile は単一 writer。** `pg_try_advisory_lock` を取れたインスタンスだけが lane を触る（§3.7.1）。

### 8.1 M4〜M7 の各不変条件への影響

| 不変条件 | 根拠 | M8 の影響 |
| --- | --- | --- |
| ack-after-publish（M3d/§8） | crates/worker/src/main.rs:547-566 | **無し**（不変条件 #1） |
| `max_deliver` 到達時の DLQ 自前 publish（M4c/§6.6） | 同 :567-584 | **無し**。park も pull 時 NAK もしないので配送回数を消費しない（§3.1）。ドレインの NAK だけが例外で、delay を cooldown 以上にして連鎖を断つ（§4.6.2） |
| TTL 結合式（§3.3） | crates/shared/src/lib.rs:638/641/653-664 | **所有が worker → CP へ移る**（§3.5）。移行を怠るとドリフトするため M8-1 の必須作業に含め、CI テストで固定する |
| 計量（M5）の冪等性 | subscriber.rs:718-746、reaper の sweeper rollup | **無し**（1 行も触らない）。U1 の C2 で `invocation_count` を実測固定する |
| canary / rollback（M7a） | 版解決は `enqueue::resolve_version_for_enqueue` の 1 箇所、計上も enqueue.rs:329-336 の 1 箇所 | **無し**。publish subject が不変なので lane 分割は版解決から完全に独立 |
| per-function config / secret 注入（M7b/M7c） | `env_token`（enqueue.rs:264-276 付近）、内部 listener（main.rs:271-286）、worker の引き換え | **無し**。lane 化は pull ループの**外側**だけを変え、`handle_payload` 以下には触れない |
| secret 非漏洩（§7 露出ガード） | `Redacted<T>`、`skip_all` の instrument | **無し**。新しいログ / メトリクスに tenant_id は載るが、**secret 名も値も載らない**（lane 名はテナント ID のみを含む） |
| RLS / GRANT（§3.2） | migrations/0004_rls.sql / 0011 の明示 GRANT | **無し**。新表ゼロ・新 GRANT ゼロ（§0） |
| M6 同期 invoke の上限レイテンシ | `SYNC_REPLY_TIMEOUT_MS=5000`、chaos_m6.rs:102-112 | **要対処 → 対処済み**。lane discovery の遅延が初回 invoke を 202 へ縮退させる回帰を、CP の `faas.lane.changed` 即時通知で消す（§3.7.4）。U3 で回帰ガードする |
| 「worker を 1 つだけ起動」前提（README.md:798-805 / :868） | M6/M7 chaos の前提 | **維持できる**（U1〜U3 は 1 プロセスで成立する）。U4（弾力スケール）だけが複数プロセスを要求するので、README に「chaos_m4〜m7 と U1〜U3 を回すときは `make stop-workers` でアクチュエータを止め、worker を 1 台だけにすること」と明記する |
| 冪等性三層（§6.6） | layer1 = `(tenant_id, idempotency_key)` 部分 UNIQUE / layer2 = `executions.id` PK / layer3 = `Nats-Msg-Id` dedup | layer1/2 は **無し**。layer3 は publish subject（enqueue.rs:302）も header も変更しないので**無し**。ただし **M8-1 の stream 再作成時に dedup 履歴がリセットされる**（§3.4 の MUST 手順で in-flight 0 を確認することで実害を消す） |

---

## 9. 移行手順と、既存を壊さないこと

### 9.1 移行手順

| 段 | 作業 | 停止 | ロールバック |
| --- | --- | --- | --- |
| M8-0 | 定数 / 純関数の `faas_shared` 移設 | 不要（挙動不変） | git revert |
| M8-1 | **CP / worker 停止 → depth 0 確認 → stream 削除 → 新 CP 起動（WorkQueue で再作成）** | **必要** | 旧バイナリで stream を削除して再作成（`Limits` に戻る） |
| M8-2 | CP の config / DB 読み取り拡張 | 不要（挙動不変） | git revert |
| M8-3 | CP と worker を**同時に**入れ替える（consumer 作成責務の移管 + discovery） | 必要（同一メンテ枠内） | 旧バイナリへ戻す。stream は WorkQueue のままだが legacy consumer の filter は 1 本なので I1 を満たし動作する |
| M8-4 | `TENANT_LANES_ENABLED=true` | 不要 | **`false` に戻すだけ**（§9.2） |
| M8-5 / M8-6 | worker の env を有効化 | 不要（既定は現行と同一） | env を戻す |
| M8-7 〜 M8-Z | component / 観測 / スクリプト / chaos / ドキュメント | 不要 | — |

**M8-1（WorkQueue 化）だけが唯一の不可逆点**である（`retention` は作成後に変更できない）。

### 9.2 緊急退避スイッチ（ロールバック経路の実体）

当初案は「`TENANT_LANES_ENABLED=false` にすれば legacy 共有 consumer が作られるので戻せる」としていた。
**そのままでは成立しない**（レビュー ops/blocker）。lane consumer はサーバ側 durable 状態として残り続けるので、
フラグを倒しても消えない。ワイルドカード filter は残存する全 lane の filter と重なるため、
WorkQueue stream では `CONSUMER.CREATE` がサーバに拒否され、CP も worker も consumer を得られない。

> **確定仕様**: **退避フラグの経路自身が「全 lane consumer を列挙して削除 → legacy 共有 consumer を作成」を
> 実装する**（§3.7.2 の (2)(3) がまさにこれである。reconcile は双方向に収束する単一のコードパスである）。
> このロールバック経路に限り、**「未処理 0 のときだけ削除」という MUST を明示的に免除する**。
> 免除で失われるのは consumer 側の ack floor / 配送状態であり、メッセージ自体は WorkQueue stream に残るので、
> 新しく作られた legacy consumer が `DeliverPolicy::All` で拾い直す。DB 行側は stuck sweeper が二重に保護する。
> **この免除の理由と失われるものを、コードコメントと README の両方に書く。**

worker 側は discovery が legacy 1 本を見つけて 1 lane として回すだけで、コードパスは共通である
（§4.3 の `mine` フィルタが `LEGACY_SHARED_DURABLE` を含む）。**したがってロールバックは env 1 個で完結する。**

### 9.3 「M8 未設定 = 現状と同じ」の範囲

| env | 既定 | 既定時の挙動 |
| --- | --- | --- |
| `TENANT_LANES_ENABLED` | `false` | legacy 共有 durable `workers` 1 本。**M7 までと同じトポロジ**（作成者だけが worker → CP に変わる） |
| `SCALE_POLL_INTERVAL_SECS` | `0` | ポーラを spawn しない → CP は `CONSUMER.INFO` を 1 度も叩かない。`GET /internal/scale` は 503 |
| `WORKER_DRAIN_TIMEOUT_SECS` | `0` | **シグナルハンドラを一切インストールしない** → SIGTERM / SIGINT はデフォルト disposition で即死（現行と同一）。`/readyz` も無条件 200 のまま |
| `WORKER_MAX_CONCURRENCY` | `32` | **ここだけは既定で挙動が変わる**（現行は無制限）。`0 = 無制限` を既定にしない理由は §4.8: それは「オートスケーラの既定構成が既知の壊れ方をする」ことを意味し、H3 を放置する |

追加で保証される互換性:

- **DB マイグレーションがゼロ**。`_sqlx_migrations` に触らないので旧 CP と新 CP を混在させても起動する。
- **メトリクスは追加のみ**。既存の系列は名前もラベルも不変 → dashboards が壊れない。
- **HTTP 公開 API は 1 本も増えない**。増えるのは内部専用 listener の 1 ルートのみ。
- `faas_shared` への定数移動は同一値の再配置であり、`INVOKE_STREAM_NAME = "FAAS_INVOKE"` /
  `LEGACY_SHARED_DURABLE = "workers"` が現行リテラル（crates/worker/src/main.rs:93/96、
  crates/control-plane/src/main.rs:533 付近）と一致することをユニットテストで固定する。

### 9.4 推奨する段階導入

1. M8-0 〜 M8-2 を land（挙動不変）。
2. M8-1 の停止メンテを実施し、WorkQueue へ移行。`jsz` で retention を目視確認。
3. M8-3 を land（CP と worker 同時）。`TENANT_LANES_ENABLED=false` のまま 1 日運用し、
   consumer 作成責務の移管だけで挙動が変わらないことを確認。
4. M8-5（`WORKER_MAX_CONCURRENCY=32`）を有効化し、`faas_worker_inflight_executions` と
   既存の `wasmtime_execution_duration_seconds` で劣化がないことを確認。
5. M8-4（`TENANT_LANES_ENABLED=true`）。`faas_lane_unsubscribed_subjects == 0` と
   `faas_tenant_lanes` を確認。U1〜U3 を回す。
6. M8-6（`WORKER_DRAIN_TIMEOUT_SECS=25`）。手動 SIGTERM で `faas_worker_redelivered_total` が
   増えないことを確認。
7. M8-8 / M8-9（`SCALE_POLL_INTERVAL_SECS=5` + `make autoscale`）。U4 を回す。
   `SCALE_MIN_WORKERS=1` のまま運用し、scale-to-zero は同期 invoke を使わない環境でのみ opt-in。

---

## 10. テスト方針 / 完了条件の自動検証

### 10.1 各ステップ共通のゲート（docs/M7-design.md:1678-1686 と同じ 4 点）

1. DB-free 単体が緑（`assign_lanes` / `effective_lane_concurrency` / `scale.rs` の全列挙テストはここに入る）
2. `chaos_m8.rs` は全て `#[ignore]`（既定 `cargo test` は緑のまま）
3. `cargo build --workspace --exclude echo` / `cargo test --workspace --exclude echo` /
   `cargo clippy --workspace --exclude echo --all-targets -- -D warnings`（`--all-targets` は `tests/` も
   対象なので chaos ファイルも完全準拠が必要）/ `cargo fmt --all -- --check`
4. `./scripts/rls-lint.sh`（M8 は DB スキーマを触らないので実質 no-op だが回帰確認として回す）

### 10.2 CI で守る部分（DB-free 単体）

`.github/workflows/ci.yml` には postgres / nats / redis / minio の services が無く、
`#[ignore]` は実行されない（docs/M7-design.md:1674-1676）。したがって CI が守れるのは純関数だけである。

| 対象 | テスト | 証明すること |
| --- | --- | --- |
| `assign_lanes`（§3.3） | `lanes_cover_every_subject_exactly_once` ほか 5 本 | **I1 の数学的証明**（二重配送が起きない） |
| `effective_lane_concurrency`（§4.2） | `sum_of_effective_never_exceeds_global` ほか 4 本 | **プロセス全体上限が構成的に閉じる**（デッドロック・permit 死蔵の不在） |
| `scale.rs::decide`（§5.3） | `desired_is_monotone_in_backlog` ほか 8 本 | **スケール判断の数学**（境界・ヒステリシス・stale が 0 に化けない） |
| `lane_ack_pending`（§7.2） | `lane_ack_pending_is_inflight_plus_headroom` | 配送層が受付層より先に binding しない |
| `resolve_for_tenant`（§4.7） | `lane_concurrency` のマージ / クランプ / 既定継承 | 既存の解決順序を壊さない |
| TTL 結合（§3.5） | `token_exp_covers_worst_case_redelivery` | consumer 設定の所有が CP へ移ってもドリフトしない |
| 定数の一致（§9.3） | `shared_constants_match_legacy_literals` | 移設が値を変えていない |
| `FailPolicy` 分類（§7.1） | store.rs:1076 付近の分類固定テストに 1 行 | `TENANT_LANE == Open` |

これらは**決定的入力**なのでフレークしない（docs/M7-design.md:1702 が分布 assert を許した唯一の条件と同じ性質）。

### 10.3 `crates/control-plane/tests/chaos_m8.rs`（全て `#[ignore]`）

ヘッダは chaos_m7.rs:1-58 の 5 ブロック構成（要約 + §15 完了条件の逐語引用 / 既存作法の宣言 /
「Un が検証すること（信頼境界の明示）」/ 実行方法の sh ブロック / env 一覧）をそのまま写す。
`#![allow(clippy::needless_return)]`（chaos_m7.rs:60）とヘルパ複製（同 :60-101）、
「後始末は assert より前」（同 :532-543）も同様。関数接頭辞は `chaos_u1_` 〜 `chaos_u4_`
（M4=a / M5=e / M6=s / M7=t の慣習を継ぐ）。

#### 10.3.1 前提（ヘッダと README に明記し、テスト冒頭で必ず assert する）

当初案の最大の欠陥は「A が BURST 件を stream に積める」ことを前提にしながら、
admission ゲート 2（handlers.rs:669-677）が A の in-flight を `max_concurrent_executions`（既定 20）に
制限する事実を考慮していなかった点である。**その状態では H4 が主張する「A の 300 件の後ろに B が並ぶ」が
再現不能で、C4 の閾値（`< BURST/2`）は A の実 execution が 20〜40 件しか無いため修正前でも自明に成立する。**
つまりテストが vacuous に通る。したがって:

| 前提 | 値 | 満たさないときの挙動 |
| --- | --- | --- |
| CP の起動 env | `QUOTA_MAX_CONCURRENT_EXECUTIONS=200`（config.rs の `DEFAULT_MAX_CONCURRENT` を上書き）、`TENANT_LANES_ENABLED=true`、`SCALE_POLL_INTERVAL_SECS=5` | テスト冒頭で `GET /internal/scale` と実測 enqueue 成功数から検出し、**明確なメッセージで panic** |
| A が実際に積めた件数 | **`enqueue 成功数 >= CHAOS_M8_BURST × 0.9`**（A の 429 件数と成功数を全件記録して前提 assert） | 「A の in-flight 上限が小さすぎる。CP を `QUOTA_MAX_CONCURRENT_EXECUTIONS=200` で起動するか、A テナントの `quotas` を上書きすること」と panic |
| worker | U1〜U3 は **1 台だけ**（`make run-worker`）。U4 は `make autoscale` | U4 は heartbeat ファイル（§5.5.3）を見て、アクチュエータ未起動なら**タイムアウトではなく即 panic** |
| supervisor 管理外の worker | U4 では**居てはならない** | `http://127.0.0.1:9090/healthz` が 200 を返したら panic（§5.5.1） |
| component | 両テナントに **`burn`** をデプロイ（同一 wasm = H5 を変数として固定） | `CHAOS_BURN` 未デプロイなら panic |
| 2 テナント目 | `make bootstrap SMOKE_TENANT_SLUG=chaos-b SMOKE_EMAIL=b@example.com` で作り、`CHAOS_TOKEN_B` にエクスポート | 未設定なら skip ではなく panic（黙って緑にしない） |

env（既定値つきでヘッダ doc に列挙する。chaos_m7.rs:35-38 の作法）:
`CHAOS_BASE_URL`(既定 http://127.0.0.1:8080) / `CHAOS_TOKEN` / **`CHAOS_TOKEN_B`** /
`CHAOS_INTERNAL_URL`(既定 http://127.0.0.1:8081) / **`CHAOS_BURN`**(既定 burn) / `CHAOS_POLL_SECS`(既定 60) /
**`CHAOS_M8_BURST`**(300) / **`CHAOS_M8_VICTIM`**(20) / **`CHAOS_M8_BURN_MS`**(300) /
**`CHAOS_M8_B_QUEUE_WAIT_MAX_MS`**(5000) / **`CHAOS_M8_SCALE_TIMEOUT_SECS`**(180) /
**`CHAOS_M8_COLDSTART_BUDGET_SECS`**(60) / `WORKER_METRICS_PORT_BASE`(9101)。

**`--test-threads=1` を要求する理由**（chaos_m7.rs:40-42 と同型の宣言をヘッダに書く）:
「U1〜U4 は共有 JetStream stream の depth、worker プロセス全体の実行スロット、worker プロセス台数という
**プロセス外の共有状態**を飽和させる。並行実行すると互いの backlog を測ってしまう」。

#### 10.3.2 観測ヘルパ（4 本。`/metrics` scrape の解禁を含む）

- `live_workers() -> Vec<usize>` — `http://127.0.0.1:{PORT_BASE+i}/metrics` を `i in 0..max_workers` で叩き、
  **`faas_worker_slot` の値が `i` と一致する**ものだけを数えて slot 番号を返す。
  `/healthz` ではなく `/metrics` を使うのは同一性検証のため（§5.5.1）。
  プロセス introspection も `docker compose` の shell-out も使わない。
- `scale_signal() -> ScaleJson` — `GET /internal/scale`。**タイムアウト予算はこの応答から算術で導出する**
  （`poll_interval_secs` / `scale_in_cooldown_secs` / `stale_after_secs` を読み、値をハードコードしない）。
- `scrape_counter(url, name, labels) -> u64` — Prometheus text から 1 行を読む。
  **契約: 該当系列が存在しなければ 0 を返す**（`IntCounterVec` は一度も inc されていないラベル組み合わせが
  テキストに現れない。`wasmtime_component_cache_hits_total` は Vec（crates/worker/src/metrics.rs:91-97）で
  `..._misses_total` は素の `IntCounter`（同 :103-106）という非対称があるため、この縮退規約を doc に固定する）。
- `queue_waits(token, ids) -> Vec<i64>` — `GET /executions/{id}` の `created_at` / `started_at` /
  `finished_at`（handlers.rs:1109-1113 付近、**すべてサーバ時計**。`started_at` は worker の `mark_running`,
  crates/worker/src/main.rs:1491-1511 が書く）から `started_at - created_at` を算出する。
  **クライアント `Instant` を測定に使わない。**

> **新しい前例の明示（設計書とヘッダの両方に書く）**: chaos_m4.rs:247 は「`/metrics` は内部ネット越し前提なので、
> まずは『終端化されている』だけを assert する」として `/metrics` の検証を回避し、M4〜M7 のどのテストも
> scrape していない。**M8 はこれを限定的に破る。** 破ってよい根拠は、対象が
> `faas_worker_slot` / `executions_total{outcome}` / `faas_worker_redelivered_total` /
> `wasmtime_component_cache_hits_total{tier}` という**整数カウンタ / gauge の差分**であって分布ではないこと、
> そして「worker が何台生きているか」「ゲストが何回実行されたか」「§3.6 のキャッシュ階層がどう効いたか」は
> 他に黒箱で観測する手段が存在しないことである。

#### 10.3.3 U1 — バーストテナント A が被害テナント B のクォータを劣化させない（C2）

```text
1. B の `GET /usage` の invocation_count と、worker の executions_total / redelivered_total、
   CP の faas_dlq_finalized_total を退避する（すべて整数のベースライン）。
2. A: CHAOS_M8_BURST(既定 300) 件を並行 invoke。input は {"burn_ms": CHAOS_M8_BURN_MS}。
   **429 / 成功をすべて記録する**（無視しない）。
3. 前提 assert: A の enqueue 成功数 >= BURST × 0.9（§10.3.1）。満たさなければ env の誤りとして panic。
4. A の enqueue 中に B: CHAOS_M8_VICTIM(既定 20) 件を **B のクォータの 1/5 のレート**で invoke。
5. C2-a: B の応答 status に 429 が **1 件も無い**こと（完全に決定的な整数 assert）。
   B の送信レートを B のクォータの 1/5 に抑えているので、B 自身に起因する
   rate_limited / concurrency_limit は構造的に 0。1 件でも出れば「A が B の何かを消費した」証明になる。
6. C2-b: B の全 execution を CHAOS_POLL_SECS まで poll → **全件 succeeded**（== M。失敗も timeout も 0）。
7. C2-c: `GET /usage` の B の invocation_count delta == M（M5 の会計不変条件。二重計上・欠落なし）。
8. 後始末（assert より前に実行）: A の残ジョブの終端待ち。
```

#### 10.3.4 U2 — B のキュー待ちがバーストの backlog に比例しない（C3 / C4）

```text
1. U1 と同じ負荷をかける（A の最終 invoke 時点のタイムスタンプを保持する）。
2. B の最終 invoke の直後に A の未終端件数を数える（GET /executions?status=... または個別 poll）。
3. C3-負の対照 (i): a_unterminated_at_b_last_invoke >= CHAOS_M8_BURST / 2
   → 「A が実際に滞留を起こしていた」ことの整数条件。
   満たさなければ「CHAOS_M8_BURST か CHAOS_M8_BURN_MS を上げよ」と明示メッセージで **fail**（成功ではない）。
4. 全件終端後、A / B の全 execution について queue_wait = started_at - created_at を算出（サーバ時計）。
5. C3-負の対照 (ii): max(A.queue_wait) >= CHAOS_M8_B_QUEUE_WAIT_MAX_MS。
   burn_ms × BURST / (worker の実容量) が閾値を大きく上回るよう既定値を選んである
   （300ms × 300 件 / 32 並列 ≒ 2.8 秒 …… 余裕を持たせるため BURN_MS を上げる調整余地を README に書く）。
6. C3: **max(B.queue_wait) <= CHAOS_M8_B_QUEUE_WAIT_MAX_MS**（既定 5000ms）。
   サーバ時計・絶対上限・env 上書き可という、chaos_m6.rs:50-58,107-112 が確立した唯一許容される時間 assert の型。
7. C4: |{a ∈ A : a.started_at < max(b.started_at)}| < CHAOS_M8_BURST / 2。
   FIFO 単一レーンなら構造的に == CHAOS_M8_BURST（B は stream 上で A の後ろ）。
   lane が分かれていれば数十のオーダー。分子・分母が同じマシン速度でスケールするので機種依存が消える。
8. 後始末は assert より前。
```

> **「修正前は red、修正後は green」の記録（MUST）**: U1 の C2-a と U2 の C4 は、
> `TENANT_LANES_ENABLED=false`（= M8-4 適用前と同じトポロジ）で回すと落ちるはずである。
> **この対比を README の「手で叩く最小手順」に記録し、落ちない assert は完了条件の証拠として採用しない。**
> レビューが指摘したとおり、修正前から緑になる assert は何も証明していない。

#### 10.3.5 U3 — admission をバイパスする経路でも分離が効き、M6 が壊れていない（H2 / M6 回帰）

```text
1. A に Cron を登録し、chain を大量に発火させる（M6 の chain 暴走ガードの範囲内で）。
2. 同時に B が HTTP invoke → B の 429 == 0 / B の max(queue_wait) <= 閾値（C2 / C3 と同型）。
3. **M6 回帰ガード**: `make bootstrap` で作りたての 3 つ目のテナント C の
   **初回 `POST /invoke?wait=1` が 200 を返す**こと（202 への縮退でないこと）。
   lane 作成の即時通知（§3.7.4）が効いていることの直接検証であり、
   これが無いと「新規テナントの初回同期 invoke が必ず 202」という M6 完了条件の回帰を見逃す。
4. 後始末: cron / trigger の削除を assert より前に（chaos_m6.rs:258-269 の作法）。
```

#### 10.3.6 U4 — 負荷に応じ worker が自動増減する（C1 / C5 / scale-to-zero）

当初案の `backlog == K` 厳密一致はレースする（アクチュエータが publish 完了前に worker を立てるため、
テストが読む backlog は `[0, K]` のどの値にもなりうる）。**したがって「ある瞬間の等値」を捨て、
「ポーリング中に観測した最大値」の整数 assert に置き換える。**

```text
前提: make autoscale が別ターミナルで走っていること（heartbeat ファイルで確認。無ければ即 panic）。
      SCALE_MIN_WORKERS=0 / SCALE_MAX_WORKERS=4 / SCALE_JOBS_PER_WORKER は /internal/scale から読む。

0. warm-up: worker を 1 台立てた状態で burn を 1 件 invoke して終端まで待つ
   （cwasm を確実に生成する。§10.3.7 の前提）。その後 live_workers() が空になるまで待つ。
1. ベースライン退避: 各 slot の executions_total / redelivered_total、CP の faas_dlq_finalized_total、
   B の usage.invocation_count。
2. live_workers() が空（0 台）になるまで待つ（上限 CHAOS_M8_SCALE_TIMEOUT_SECS）。
3. K = CHAOS_M8_BURST 件を invoke（input は burn_ms）。**publish と並行して**
   /internal/scale と live_workers() を poll_interval_secs 周期でサンプリングし、
   max_observed_desired と max_observed_live を記録する。
4. C1-out: max_observed_desired == max_workers かつ max_observed_live == max_workers。
   （「ある瞬間の backlog == K」ではなく「観測した最大値」なので、消費と publish の競走に影響されない）
5. 全件終端まで poll。**succeeded == K**（DLQ / timeout は 0）。
6. C5-a: 全 slot の executions_total の合算 delta == K（**再実行ゼロ**）。
   プロセス消滅で失われる分があるため、slot ごとの値は「停止前に読んだ最後の値」を supervisor が
   $RUNDIR/slot-{i}.executions に落としておき、テストはそれと生存中 slot の現在値を合算する。
7. C5-b: 全 slot の faas_worker_redelivered_total の合算 delta == 0（**ドレインが効いている直接証拠**）。
8. C5-c: CP の faas_dlq_finalized_total の delta == 0（**オートスケーラが正常ジョブを失敗させていない**）。
9. C1-in: backlog == 0 を確認したのち、live_workers() が空になるまで待つ
   （上限 = scale_in_cooldown_secs + CHAOS_M8_SCALE_TIMEOUT_SECS。**両方 /internal/scale から読む**）。
10. scale-from-zero / キャッシュ階層（§5.6.2 の直接検証）:
    a. live_workers() == [] を確認。
    b. burn を 1 件 invoke。終端まで待つ。
    c. started_at - created_at <= CHAOS_M8_COLDSTART_BUDGET_SECS（既定 60。絶対上限のみ。warm との比較はしない）。
    d. **実際に 200 を返した slot を live_workers() の走査で捕捉して**その /metrics を scrape:
       - wasmtime_component_cache_misses_total の delta == 0 —— S3 から再ダウンロードしていない
       - wasmtime_component_cache_hits_total{tier="cwasm"} の delta == 1 —— cwasm がプロセス寿命を越えて残った
       （0 の warm-up ステップにより、この 2 つは「クリーンなマシンで落ち、汚れたマシンで通る」テストにならない）
    e. 同じ component をもう 1 度 invoke → hits{tier="lru"} の delta == 1（LRU がプロセス内で効いている）。
11. 後始末は assert より前。
```

**所要時間の目安**（README に明記し、`CHAOS_M8_BURST` で縮小できるようにする）:
U1 が 3〜5 分、U2 が 3〜5 分、U3 が 3 分、U4 が 6〜10 分。

### 10.4 README の「手で叩く最小手順」へ退避する分

README.md:878-905 の形式（番号付き sh + 期待値コメント）で書く。テストには入れない。

1. **lane トポロジの目視**:
   `curl -s localhost:8222/jsz?consumers=1 | jq '.account_details[].stream_detail[].consumer_detail[] | {name, num_pending, num_ack_pending}'`
   → バースト中に A の lane だけ `num_pending` が大きく、B の lane が 0 に張り付くことを確認する。
   **「分離が配送層で効いている」ことの最も直接的な証拠**であり、Rust コードも Prometheus も要らない
   （監視ポート 8222 は docker-compose.yml:43-53 で公開済み）。
2. **I1 の目視**: 各 consumer の `config.filter_subject(s)` を列挙し、同じ subject が 2 回現れないこと。
3. **retention の目視**: `curl -s localhost:8222/jsz?streams=true | jq '.account_details[].stream_detail[] | {name, config: .config.retention}'`
   → `workqueue` であること（§3.4 の起動時 fail-fast の二重確認）。
4. **テナント別 p99 の参考値**（assert しない）:
   ```sql
   SELECT tenant_id,
          percentile_cont(0.99) WITHIN GROUP (
            ORDER BY EXTRACT(EPOCH FROM (started_at - created_at)) * 1000) AS p99_queue_wait_ms
   FROM executions
   WHERE created_at >= now() - interval '10 minutes' AND started_at IS NOT NULL
   GROUP BY tenant_id;
   ```
   `VERSION_STATS_SQL`（crates/control-plane/src/db.rs:446-457）と同型（`percentile_cont` を直近ウィンドウで
   生表から引く）。**スキーマ変更は不要**（`idx_executions_tenant_created`, migrations/0001_init.sql:59）。
   API として露出させると他テナントの統計が漏れるため、**運用者が psql で叩く手順に留める**。
5. **「修正前 red / 修正後 green」の対比**（§10.3.4 の MUST）。
6. **ドレイン無効時の対照**: `WORKER_DRAIN_TIMEOUT_SECS=0` で U4 を回すと
   `faas_worker_redelivered_total` が > 0 になることを手で確認する（テスト内で env を切り替えて
   worker を再起動する手段が無いため README へ退避）。

### 10.5 既存テストへの影響

- **README.md:798-805 / :868 の「worker を 1 つだけ起動」前提を書き換える**。
  「chaos_m4/m5/m6/m7 と chaos_m8 の U1〜U3 を回すときは `make stop-workers` でアクチュエータを止め、
  worker を 1 台だけにすること。U4 だけが `make autoscale` を要求する」と明示する。
- chaos_m6 の同期 invoke は `SCALE_MIN_WORKERS >= 1` でなければ通らない（§5.3）。既定 1 なので壊れない。
- 既定 env（`TENANT_LANES_ENABLED=false` / `SCALE_POLL_INTERVAL_SECS=0` / `WORKER_DRAIN_TIMEOUT_SECS=0`）では
  M8 の新機能パスがほぼ起動しないため、chaos_m4〜m7 は無変更で緑のまま。
  唯一の例外は `WORKER_MAX_CONCURRENCY=32`（§9.3）で、これは M4〜M7 の chaos が要求する並列度
  （最大でも数十件）を上回るので影響しない。M8-3 の land 後に chaos_m4〜m7 を通しで再実行して確認する。

---

## 11. 影響ファイル一覧（ステップ別）

挿入位置は**アンカー**で書く（docs/M7-design.md:1898-1900 の作法）。

### M8-0（挙動不変）
- `crates/shared/src/lib.rs`: `invoke_subject_wildcard()`(:249) の直後に
  `INVOKE_STREAM_NAME` / `LANE_DURABLE_PREFIX` / `OVERFLOW_LANE_DURABLE` / `LEGACY_SHARED_DURABLE` /
  `LANE_CHANGED_SUBJECT` / `lane_durable()` / `tenant_from_lane_durable()` / `assign_lanes()` /
  `effective_lane_concurrency()` / `served_lane_capacity()` / `parse_backoff_secs()`（worker main.rs:290-309 から移設）/
  `invoke_stream_config()` を追加 + 全列挙単体テスト
- `crates/worker/src/main.rs`: `DURABLE_NAME`(:93) / `STREAM_NAME`(:96) を `faas_shared` 参照に置換、
  `parse_backoff_secs`(:290-309) を削除して re-export を使う
- `crates/control-plane/src/main.rs`: `ensure_invoke_stream`(:525-541) の stream 名を `faas_shared` から取る

### M8-1（破壊的: stream 再作成）
- `crates/control-plane/src/main.rs:525-541`: `faas_shared::invoke_stream_config()`（WorkQueue + discard New）を使い、
  作成後に `Stream::info()` で `retention == WorkQueue` を検査して fail-fast
- `crates/worker/src/main.rs:674-681`（`ensure_consumer` 内）: `get_or_create_stream` → `get_stream` + 同じ retention 検査
- `crates/control-plane/src/config.rs`: `backoff_secs` フィールド追加（`ack_wait_secs`/`max_deliver` は :320-322 に既存）
- `crates/worker/src/main.rs:247-278`: `ACK_WAIT_SECS` / `MAX_DELIVER` / `BACKOFF_SECS` の consumer 設定用途を撤去
  （`ack_wait_secs` はドレイン上限検査のため読み取りのみ残す）
- `Makefile`: `lane-stream-reset` ターゲット（`## 日本語 1 行説明` + `.PHONY`(:97) 追記。数字入りの名前にしない）
- `README.md`: 停止メンテ手順（depth 0 確認 → stream 削除）

### M8-2（挙動不変）
- `crates/control-plane/src/db.rs:1478`: `TenantQuotaOverrides.lane_concurrency: Option<u64>`
- `crates/control-plane/src/db.rs:1535` の直後: `list_active_tenants_with_quotas`（既存 `list_active_tenant_ids` は残す）
- `crates/control-plane/src/state.rs:24` の直後: `QUOTA_MAX_LANE_CONCURRENCY = 32`
- `crates/control-plane/src/state.rs:79-88` の直後: `lane_concurrency` のマージ + `clamp_quota_u64`(:95 付近) 適用
- `crates/control-plane/src/state.rs:112-116`: `ResolvedAdmissionParams.lane_concurrency: u64`
- `crates/control-plane/src/config.rs`: §4.8 の CP 側 env を `DEFAULT_*` const（:20-53 の並び）→ フィールド →
  `env_u64` / `env_bool`（:320-360 の並び）の 3 点セットで追加。`min > max` の fail-fast もここ
- `crates/control-plane/src/store.rs:56` の直後: `FailPolicy::TENANT_LANE`（+ 分類固定テストに 1 行）
- `crates/control-plane/src/admission.rs:129-146`: `fail_open_degraded` の `pub(crate)` 化と引数一般化（§7.1）

### M8-3（CP + worker 同時入れ替え）
- `crates/control-plane/src/reaper.rs:236` の直後: `run_lane_reconcile` / `reconcile_lanes_once` /
  `collect_consumer_names`（`run_secret_kid_gauge`(:208-236) を型として複製）
- `crates/control-plane/src/state.rs`: advisory lock ヘルパ、lane ensure キャッシュ（`DashMap` + 世代 `AtomicU64`）、
  `tenant_lanes_enabled()` / `max_dedicated_lanes()` アクセサ（`jetstream()`(:236) / `metrics()`(:307) の隣）
- `crates/control-plane/src/enqueue.rs:298` の直前: `ensure_lane_for_tenant`（fail-open + `lane_provision_degraded`）
- `crates/control-plane/src/main.rs:261` の直後: `reaper::run_lane_reconcile` を spawn
- `crates/worker/src/main.rs:477-483`: `ensure_consumer` 呼び出しを discovery ループ起動に置換
- `crates/worker/src/main.rs:513-594`: 単一 pull ループを `lane_loop` へ再構成（**:544-592 の ack / DLQ ロジックは
  1 行も変えずに移設**）
- `crates/worker/src/main.rs:656-747`: `ensure_consumer` を削除（stream は `get_stream` のみ、consumer は CP が作る）
- `crates/worker/src/main.rs:247-278`: `LANE_DISCOVERY_INTERVAL_SECS` / `LANE_ROTATION_SECS` を追加

### M8-4（env フラグ）
- `.env.example` / `Makefile`: `TENANT_LANES_ENABLED=true`、`MAX_DEDICATED_LANES` ほか
- `crates/control-plane/src/reaper.rs`: lane トポロジ分岐（`LaneTopology::lanes` / `::legacy`）と
  ロールバック経路の未処理 0 免除（§9.2）

### M8-5（worker 実行クレジット）
- `crates/worker/src/main.rs:202-231`: `Settings` に `max_concurrency` / `lane_concurrency`
- `crates/worker/src/main.rs`（lane ループ）: §4.3 の permit 規律、`InflightGuard`（§4.5）
- `crates/worker/src/metrics.rs:38-64` / `Metrics::init` 末尾: §6.3 の gauge / counter
- `.env.example`: `WORKER_MAX_CONCURRENCY=32` / `WORKER_LANE_CONCURRENCY=4`

### M8-6（ドレイン）
- `crates/worker/src/main.rs:417-508`（`main`）: シグナル task、起動時 `drain_timeout < ack_wait` の fail-fast
- `crates/worker/src/main.rs`（lane ループ）: `select!` による pull 停止 + NAK(delay)
- `crates/worker/src/main.rs:351-387`（`spawn_metrics_server`）: `Arc<AtomicBool>` を渡し、
  `readyz`(:358-361) に 503 分岐 + 「これは依存 probe ではなくライフサイクル状態である」コメント
- `.env.example`: `WORKER_DRAIN_TIMEOUT_SECS=25`

### M8-7（component）
- `components/burn/{Cargo.toml,src/lib.rs,wit/}`（`components/slow` を雛形にコピー。
  `wit/` は同一内容、`Cargo.toml` は name だけ差し替え）
- `Cargo.toml:3-10`: workspace members に `components/burn` を追加
- `Makefile`: `build-component` / `deploy-chaos-components` に burn を追加

### M8-8（backlog シグナル + 判断）
- `crates/control-plane/src/metrics.rs`: §6.2 の 9 系列（登録は :258 の直後、構造体は :111 の後、
  `Arc::new(Self{..})` は :273 付近の後）
- `crates/control-plane/src/reaper.rs`: `run_lane_depth_gauge`（§5.2.3）
- `crates/control-plane/src/scale.rs`（新規: 純関数 + 全列挙ユニットテスト）
- `crates/control-plane/src/main.rs`: `mod scale;`（`mod routing;` の並び）、:261 の直後に spawn、
  `build_internal_router`(:317-321) に `GET /internal/scale` を 1 ルート、
  **doc コメント(:305-316) の更新**（§5.4）
- `crates/control-plane/src/handlers.rs`（または `handlers_scale.rs`）: `scale_status` ハンドラ
- `crates/control-plane/src/state.rs`: `scale()` アクセサと `Mutex<ScaleState>` 相当の共有状態

### M8-9（アクチュエータ）
- `scripts/run-workers.sh` / `scripts/worker-autoscale.sh` / `scripts/stop-workers.sh`
  （新規。`scripts/` には現在 `rls-lint.sh` の 1 本のみ）
- `Makefile`: M8 バナー + `run-workers` / `autoscale` / `stop-workers` / `lane-status` + `.PHONY`(:97) 追記

### M8-Z（検証 / ドキュメント）
- `crates/control-plane/tests/chaos_m8.rs`（新規、U1〜U4）
- `README.md`: `## M8 スコープ（と非スコープ）` を :66 の直前に新設、M7 節の「含まない（M8 以降の follow-up）」(:124) を更新、
  `### Chaos / M8 …` を M7 chaos 節の直後（:934 付近）に追加、環境変数表(:436)、起動手順(:513-568)、
  ディレクトリ構成(:1025-1053)、`worker を 1 つだけ` の前提(:798-805, :868)、
  `docker compose logs control-plane worker`(:899) の誤りの修正、トラブルシュート(:996)、
  次のマイルストーン(:1071-1093)、k8s / KEDA の 1 段落
- `.env.example`: 新 env 一式
- `仕様書.md`: §3.3(:269) / §8(:743 表, :751) / §9(:758) / §15 M8(:913-917)（§13）

---

## 12. 完了条件と、その検証手段の対応

| 完了条件（§1.2 の翻訳後） | 実現する設計 | 検証手段 |
| --- | --- | --- |
| **C1: 負荷に応じ worker が自動増減する** | backlog = 全 lane の `num_pending + num_ack_pending`（§5.2）、`Stream::consumers()` ポーラ（§5.2.3）、`scale.rs` の ceil 除算 + 即時 scale-out + ヒステリシス scale-in（§5.3）、参照アクチュエータ（§5.5） | **CI**: `desired_is_monotone_in_backlog` / `ceil_boundary_increments_at_each_multiple` / `scale_out_is_immediate` / `scale_in_requires_cooldown`（全列挙・決定的）。**chaos**: U4-4（`max_observed_desired == max_workers` かつ `max_observed_live == max_workers`）と U4-9（`live_workers()` が空へ）。いずれも整数の到達 assert で、上限時間は `/internal/scale` から算術導出する |
| **C1': scale-to-zero / from-zero が成立する** | consumer 作成責務を CP へ移し、worker 0 台でも `CONSUMER.INFO` が成功する（§5.2.3）。`inactive_threshold` を設定しない（§8 #4）。`SCALE_MIN_WORKERS=0` の opt-in（§5.3） | **chaos**: U4-2 / U4-10。`started_at - created_at <= CHAOS_M8_COLDSTART_BUDGET_SECS`（絶対上限のみ） |
| **§3.6 のキャッシュと正しく結合している** | cwasm はプロセス寿命と独立（crates/worker/src/main.rs:1774-1797 付近）、LRU はプロセスローカル（同 :465-467）、共有 `WASM_CACHE_DIR` の明文化（§5.6.3） | **chaos**: U4-10d/e（`misses` delta == 0、`hits{tier="cwasm"}` delta == 1、2 回目で `hits{tier="lru"}` delta == 1）。warm-up ステップ（U4-0）により前提が明示されている |
| **C2: 他テナントのクォータを劣化させない** | lane ごとの `max_ack_pending`（= そのテナントの `max_concurrent_executions` + headroom, I2）により、A の未 ack が B の配送を止める経路（§2.2）が消滅する。B の pending が滞留しないので B の Redis in-flight が上がらず `concurrency_limit` 429 が出ない | **CI**: `lanes_cover_every_subject_exactly_once`（I1）、`lane_ack_pending_is_inflight_plus_headroom`。**chaos**: U1-5（`b_429 == 0`、完全に決定的な整数）、U1-6（`succeeded == M`）、U1-7（`invocation_count` delta == M） |
| **C3 / C4: 他テナントのレイテンシを劣化させない** | 配送層の lane 分割が head-of-line blocking（H4）を解き、実行層の per-lane クレジットが CPU 競合（H3）を解く。H4 が subject 分離でしか解けないことは §3.1 で論証 | **chaos**: U2-6（`max(B.queue_wait) <= 5000ms`、サーバ時計・絶対上限）、U2-7（順序の個数 `< BURST/2`）、**U2-3 / U2-5 の負の対照**（A が実際に滞留し閾値を超えたこと。テストが vacuous に通れない） |
| **バイパス経路（cron / event / chain）でも成り立つ** | 配送層は subject に載る全メッセージに効き origin を問わない（§7.2） | **chaos**: U3-2 |
| **C5: スケールが実行の正しさを壊さない** | ack セマンティクス不変（§8 #1）、permit 規律 W1/W2（§4.1）、終端 CAS（db.rs:1833-1839）+ rollup の CAS ゲート、SIGTERM ドレイン + delay 付き NAK（§4.6.2） | **CI**: 既存の `finalize_execution_sql_is_cas_guarded`（db.rs:2528 付近）が回帰ガードとして機能し続けること、`sum_of_effective_never_exceeds_global`。**chaos**: U4-5（`succeeded == K`）、U4-6（`executions_total` 合算 delta == K = 再実行ゼロ）、U4-7（`redelivered_total` delta == 0）、U4-8（`faas_dlq_finalized_total` delta == 0） |
| **同一ジョブの多重実行が起きない（仕様書 §3.3 の MUST NOT）** | WorkQueue retention で filter の重なる consumer 作成を **NATS サーバが拒否**（I1）。reconcile が削除 → 更新 → 作成の順序を advisory lock 下で守る（§3.7）。起動時 retention 検査（§3.4） | **CI**: `lanes_cover_every_subject_exactly_once`。**chaos**: U4-7。**手順**: README の `jsz?consumers=1` で filter 重複ゼロと retention を目視 |
| **既存マイルストンを壊さない** | 全 M8 env の既定が「新機能パスを起動しない」値（§9.3）、DB マイグレーションゼロ、既存メトリクス不変（§8 #11）、API 契約不変（§7.3）、lane 作成の即時通知で M6 の初回同期 invoke を守る（§3.7.4） | **CI**: `cargo test --workspace --exclude echo` が緑。**chaos**: M8-3 land 後に chaos_m4/m5/m6/m7 を通しで再実行して緑。加えて U3-3（作りたてのテナントの初回 `?wait=1` が 200） |
| ~~k8s HPA / KEDA での水平増減~~ | **非スコープ**（§含まない） | 検証手段が無いため対応表に行を作らない。README に「参照アクチュエータと交換可能な外部実装」として記述するのみ |
| ~~アクティブテナント数 > `MAX_DEDICATED_LANES` の regime~~ | **非スコープ**（§3.3 で regime を明示） | overflow lane 内部では H1/H4 が復活することを宣言する。検出は `faas_lane_overflow_tenants` / `faas_worker_lanes_unserved` gauge |

> **CI と chaos の分担（docs/M7-design.md:1674-1676 の再掲）**: `.github/workflows/ci.yml` には
> postgres / nats / redis / minio の services が無く `#[ignore]` は実行されない。上表の「CI:」は毎コミットで
> 守られるが「chaos:」は手元 / 手動でのみ走る。
> **「分離と上限の数学」（どの subject もちょうど 1 lane に属する / Σ クレジットが global を超えない /
> スケール判断が境界と単調性を守る）は CI 側の全列挙テストに、
> 「それが実際に配線されていること」（lane 別 depth が取れる / アクチュエータがプロセスを増減させる /
> ドレインが再配送を防ぐ）は chaos 側に**責任を分ける。どちらか一方だけでは完了条件を満たしたとは言えない。
>
> **M8 固有の宣言（chaos_m7.rs:745 / docs/M7-design.md:1891-1892 への明示的な例外規定）**:
> M6/M7 は「分布や時間依存の assert を持たない」と宣言した。M8 は完了条件が本質的にレイテンシに
> 関するものであるため、**この方針に 1 箇所だけ例外を作る**: U2-6 の
> 「サーバ時計で測ったキュー待ちの絶対上限（env 上書き可）」である。これは chaos_m6.rs:50-58,107-112 が
> 既に確立した唯一の時間 assert の型（`CHAOS_SYNC_TIMEOUT_SECS`）と**完全に同型**であり、
> 新しい作法を導入するものではない。**「比率の比較」「分位点の比較」は依然として行わない**
> （README の psql 手順へ退避する）。

---

## 13. 仕様書の更新（実装より前に行う）

| 箇所 | 現状の記述 | M8 後 |
| --- | --- | --- |
| 仕様書.md:269（§3.3 Consumer セマンティクス） | 「`tenant.*.component.invoke` を購読する**単一の共有 Durable Pull Consumer**を用い…テナント動的追加時もワイルドカード購読のため Consumer の再構成は不要」 | 「テナント別 lane Consumer（+ 有界な overflow lane）を用いる。lane は Control Plane が enqueue 時と reconcile（単一 writer）で冪等に用意するため、テナント追加時の**運用手順**は不要（自動化されている）。stream は WorkQueue retention で、**どの subject もちょうど 1 consumer に属する**ことをサーバが強制する（多重実行の構造的禁止）」 |
| 仕様書.md:743 表（§8 クォータ表） | `MaxAckPending \| 未 ack メッセージ数 / Consumer \| 1000 \| — \| JetStream Consumer 設定` | `MaxAckPending \| 未 ack メッセージ数 / **lane（= テナント）** \| max_concurrent_executions + 8 \| 208 \| JetStream Consumer 設定（Control Plane が導出）`。overflow lane のみ固定 1000 |
| 仕様書.md:751（§8 公平性の責務分界） | 「テナント間の公平性は Axum の admission control で担う…`Σ(テナント数 × max_concurrent) ≲ MaxAckPending` を目安に…より強い分離が要るならテナント別 Consumer / 優先度分離を将来導入する（§10）」 | 「公平性は 3 層で担う: **受付層**（admission = テナント自身のクォータ）/ **配送層**（lane consumer の per-tenant `max_ack_pending`）/ **実行層**（worker の per-lane 実行クレジット）。`MaxAckPending` は lane ごとに導出されるため**合算の目安式は不要**。運用条件は `W × WORKER_LANE_CONCURRENCY ≳ max_concurrent_executions` と `WORKER_MAX_CONCURRENCY ≳ アクティブ lane 数`。優先度（重み付け）分離は引き続き §10」 |
| 仕様書.md:758（§9 プロビジョニング） | 「共有 Durable Pull Consumer を作成する…テナント追加時はワイルドカード購読のため Consumer の再構成は不要」 | 「stream を **WorkQueue retention** で作成する（Control Plane が唯一の作成者）。lane Consumer も Control Plane が自動で用意する（手動プロビジョニング不要）」 |
| 仕様書.md:913-917（§15 M8） | 「Worker オートスケール…テナント別 Consumer / 優先度分離…**完了条件**: 負荷に応じ worker が自動増減し、1 テナントのバースト負荷が他テナントの p99 レイテンシ / クォータを劣化させない」 | §1.3 の改訂提案文（実測可能な 3 条件 + p99 は psql 手順へ退避する旨の注記）に差し替える。あわせて「優先度分離」を M9 へ、「Result Ingestor 分離」を需要発火型のまま据え置く |

---

## 付録 A: `components/burn` の仕様

`components/slow`（components/slow/src/lib.rs）を雛形にする。相違は `handle` の中身だけ。

```rust
//! M8 chaos 用: 入力 JSON の `burn_ms` で指定されたミリ秒だけ busy-loop してから **succeeded で終端する**。
//!
//! `components/slow` は無限 tight loop で必ず timeout 終端するため、
//! 「N ミリ秒かかって成功する」ノブとしては使えない。M8 の chaos は
//! 「A の総仕事量 = BURST × 1 件あたり ms」を設計パラメータとして制御する必要があるので、
//! 本 component を新設する（テストの都合ではなく、完了条件を決定的に測るための設計要素）。
//!
//! 注意: 標準 handler world は host import を持たないので時刻を読めない。したがって
//! **反復回数**に翻訳する。`burn_ms` から `iterations = burn_ms * ITERS_PER_MS` を求め、
//! `#[inline(never)]` の補助関数を回す（slow と同じく epoch check を確実に挿入させるため）。
//! `ITERS_PER_MS` はマシン依存なので、**絶対的な実行時間の精度は要求しない**。
//! chaos が要求するのは「A の 1 件が echo より十分長い」ことだけであり、
//! U2 の負の対照（A が実際に滞留したこと）を整数条件で確認するので機種差は吸収される。
//!
//! 出力は `{"burned_ms": <要求値>, "iterations": <実行数>}`。
//! `burn_ms` が無い / 解釈できない場合は 0（= echo 相当の即時終端）。
//! 上限は `MAX_BURN_MS`（10_000）でクランプする（ゲスト側の防御。実際の打ち切りは
//! version ごとの `ResourceLimits` の `max_wall_time_ms` が epoch interruption で行う）。
```

`Cargo.toml` は components/slow のものを name だけ変えて流用（`wit-bindgen = "0.36"` +
`crate-type = ["cdylib"]` + `[profile.release]`）。`serde_json` は入力パースに要るので
components/echo/Cargo.toml と同じく依存に足す。`wit/` はワークスペース直下 `wit/world.wit` と同一内容をコピーする
（既存 component と同じ作法）。workspace members（Cargo.toml:3-10）に追加する。
CI の native ジョブは `--exclude echo` のみなので、`slow` / `always-trap` と同じ扱いでビルドされる。

## 付録 B: レビュー指摘のうち、採用しなかったもの

| 指摘 | 判断 | 理由 |
| --- | --- | --- |
| `faas_result_finalized_total{outcome="stale"}` を二重実行の検証手段に使う | **撤回**（観測値としては追加してよい） | 検出力が無い（§6.4 で論証）。代わりに `faas_worker_redelivered_total` と `executions_total` 合算を使う |
| lane 数超過時は起動時 fail-fast にする | **不採用**（gauge + warn + 回転にする） | lane 数は実行時に増える（テナント追加）ので起動時検査では守れない。守るべきは「静かに壊れないこと」であり、それは可観測性と縮退で足りる（§4.4） |
| `WORKER_MAX_CONCURRENCY == SCALE_JOBS_PER_WORKER` を MUST にする | **MUST から降格** | CP は worker の env を知らないので検査できない。既定を一致させ、gauge と chaos の前提 assert で静かなズレを潰す（§4.8） |
| stream に `max_age` / `max_msgs` を設定して暴走を有界化する | **不採用**（上限を設定しない） | 既定の `discard: Old` は**他テナントのメッセージから捨てる**ため完了条件 C2 を新設 config 自身が破る。WorkQueue なら ack で消えるので通常は不要。設定する場合は `discard: New` を必須の不変条件にする（§8 #6） |
| `pull` 時の余剰メッセージを `AckKind::Nak` で即返す | **不採用**（permit 規律で over-fetch しない） | NAK は `delivered` を進めて `MAX_DELIVER` を消費し、ゲストが失敗していないジョブを DLQ 化する。ドレイン時のみ、delay 付きで例外的に使う（§4.6.2） |
| worker の cwasm 事前 warm を実装する | **不採用（測ってから決める）** | プロセス初期化に比べ寄与が小さいと見込まれ、「どの component を warm すべきか」を worker は知らない。U4-10 の実測で支配的と判明したら `WORKER_WARM_CACHE_MAX` を M9 で導入（§5.6.3） |
| `METRICS_INCLUDE_TENANT_LABEL` を M8 で land する | **不採用**（M9 へ） | 完了条件を Prometheus 非依存で測る方針なので前提ではない。lane gauge のカーディナリティは `METRICS_LANE_LABELS` で独立に gate する（§6.2） |
