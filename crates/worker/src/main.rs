//! faas-worker — Component を invoke して結果を返す実行ワーカ。
//!
//! 仕様書 §3.4 / §3.6 / §6.3 / §6.5 / §15 M2。
//!
//! 流れ (§6.3 / §6.5):
//!   1. NATS JetStream の `invoke_subject_wildcard()`（`tenant.*.component.invoke`,
//!      §3.3）を共有 Pull Consumer (durable "workers") で購読する。水平スケール時は
//!      同一 durable を共有し JetStream がメッセージを分配する。新規テナント追加時も
//!      stream/consumer の再構成は不要（`*` は 1 トークン=テナント ID に一致）。
//!   2. `JobMessage` を受信 → executions を `running` へ更新 (M2 は worker が
//!      直接 DB 更新)。
//!   3. `job.wasm_sha256` をキーに Component を解決して wasmtime で実行する
//!      (§3.6 のキャッシュ階層。詳細は `Worker::resolve_component`)。
//!      - Engine: epoch_interruption + consume_fuel + async + cranelift。precompile /
//!        deserialize / 実行は同一 Config の Engine を共有する (deserialize は同一設定が必須)。
//!      - Store: StoreLimits(max_memory) を適用。
//!      - 別 OS スレッドの epoch ticker が max_wall_time 経過後に
//!        `engine.increment_epoch()` を周期的に呼び出し、実行を停止させる (Timeout)。
//!        OS スレッドにするのは tokio LIFO-slot 起因のスタベーション回避のため
//!        (`run_component` の設計メモ参照)。
//!      - M4b (§4.3): tokio::time::timeout(max_execution_time) で host+guest 総時間も覆う。
//!        任意 max_fuel が設定されていれば Store::set_fuel(n) を適用し、OutOfFuel 超過は
//!        `failed` に分類する。
//!      - world `handler` の `handle(input: list<u8>)` を呼び出す。
//!        入力は `serde_json::to_vec(&job.input)`。
//!   4. 結果 (succeeded | failed | timeout) を `ResultMessage` として
//!      `result_subject(job.tenant_id)`（具体テナント subject）へ publish する。
//!      CP の subscriber が `tenant.*.component.result` で受け、echo した署名トークンを
//!      検証してから executions を終端状態へ CAS 更新する。worker は終端状態を **直接
//!      DB に書かない**（鍵なし。唯一の終端 writer は検証付きの CP subscriber, §3.3）。
//!
//! Component キャッシュ (§3.6): `job.wasm_url`（短命 presigned GET URL）から本体を
//! 取得し、`job.wasm_sha256` をキーに in-memory LRU → ローカル cwasm →
//! ダウンロード+事前コンパイル の順で解決する。hit 経路で coldstart を短縮する。
//!
//! M3b (§3.2): DB アクセスは faas_app（NOBYPASSRLS）で接続し、各 tx 冒頭で
//! job 由来の tenant を `app.tenant_id` GUC に設定してから executions /
//! component_versions / components に触れる（FORCE RLS。未設定だと fail-closed）。
//!
//! M3c (§3.3): テナントの権威は **CP が署名した claim** であり、それは CP 内の
//! subscriber がトークン検証時に行 (execution_id/tenant_id/version_id) と突き合わせる。
//! worker は鍵を持たず、JobMessage.job_token を全 result に verbatim に echo するだけで
//! ある（DONE: 署名トークンの echo 経路）。job.tenant_id は CP が mint した claim と
//! 一致するので、worker は引き続き job.tenant_id で SET LOCAL し RLS 書き込みを tenant tx
//! の下で行う（GUC/tx は撤去しない）。consumer の ack_wait/max_deliver は CP の token exp と
//! 同一定数から導出する（TTL 結合, §3.3）。
//!
//! M4c (§6.6 MUST): JetStream pull consumer に `backoff` 配列を載せ、`delivered == max_deliver`
//! の最終配送試行で result publish に失敗したら、worker が自前で `.failed` (DLQ) を core NATS で
//! publish して CP の DLQ subscriber に即時 finalize+DECR させる経路を加えた。`.failed` の publish 自体
//! にも失敗した場合は最終手段として CP 側 reaper の stuck-execution sweeper が deadline で回収する
//! （二段救済）。また durable consumer の config drift（ack_wait/max_deliver/backoff）が検出されたら
//! 起動時に自動で delete+recreate して TTL 結合を保つ。
//!
//! スコープ外 (TODO):
//!   - result / failed の durable JetStream stream 化（M4c では core NATS のままで耐ブラスト半径を維持）
//!   - InstancePre 事前インスタンス化・Pooling アロケータ (§3.6 / 将来最適化)
//!
//! M4b (§4.3) 完了: tokio::time::timeout で max_execution_time（ホスト関数込み総時間）を、
//! Store::set_fuel で任意 max_fuel を、それぞれ epoch + StoreLimits に加えて適用する。
//! トラップ分類: epoch 中断 / 時間超過 → `timeout`、fuel 超過 → `failed`（§4.3）。

mod bindings;
mod env;
mod metrics;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use chrono::Utc;
use faas_shared::{
    failed_subject, result_subject, ExecutionStatus, FailedMessage, JobMessage, ResourceLimits,
    ResultMessage, UsageMetrics,
};
use futures::StreamExt;
use lru::LruCache;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::time::Instant;
use tracing::{error, info, warn};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};

use bindings::Handler;

/// JetStream stream 名。invoke subject を束ねる（真実は `faas_shared`）。
const STREAM_NAME: &str = faas_shared::INVOKE_STREAM_NAME;

/// 1 度の Pull で取りに行く最大メッセージ数。
const PULL_BATCH: usize = 16;

/// in-memory Component LRU キャッシュの最大エントリ数 (§3.6)。
/// hit 経路が coldstart 短縮の主経路。容量超過時は最古を退避する。
const COMPONENT_CACHE_CAP: usize = 64;

// M8-3: `MAX_ACK_PENDING`（全テナント合算の固定値 1000）はここから削除された。
// consumer の作成者が control-plane へ移り、`max_ack_pending` は **lane（= テナント）ごとに**
// `max_concurrent_executions + headroom` から導出されるようになったため（§8 / §3.5）。
// 合算の頭打ちこそが「1 件しか投げていないテナントが他テナントの負荷で配送されない」という
// クォータ劣化の本体であり、保存すべき不変条件ではなかった。

// ============================================================================
// Store のホスト状態
// ============================================================================

/// M5 (§15): linear memory のピーク使用量を計測する `ResourceLimiter` ラッパ。
///
/// wasmtime 29 の `StoreLimits` は上限の強制はするが「実際に何バイトまで伸びたか」を
/// 観測する getter を持たない。そこで `memory_growing` フックに相乗りし、各成長要求の
/// `desired`（その成長後の linear memory バイト数）の最大値を記録する。上限判定そのものは
/// 内側の `StoreLimits` にそのまま委譲するため、`max_memory_bytes` 強制（§4.3）は不変。
///
/// peak は `Arc<AtomicU64>` に持たせ、Store が future 内へ move されたあとでも
/// run_component 末尾で読み取れるようにする（Store を drop しても peak は残る）。
/// table 成長など他の `ResourceLimiter` メソッドは内側 `StoreLimits` へ素通しする。
struct MeteredLimits {
    inner: StoreLimits,
    /// これまでに観測した linear memory の最大バイト数（全 memory 横断の max）。
    peak_memory_bytes: Arc<AtomicU64>,
}

impl ResourceLimiter for MeteredLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        // 上限強制は内側 StoreLimits に委譲する（許可されたときだけ peak を更新する）。
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if allowed {
            // `desired` は成長後の総バイト数。AtomicU64 へ max でマージする。
            self.peak_memory_bytes
                .fetch_max(desired as u64, Ordering::Relaxed);
        }
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn memory_grow_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        self.inner.memory_grow_failed(error)
    }

    fn table_grow_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

/// wasmtime Store が保持するホスト側状態。
/// WASI コンテキスト・リソーステーブル・計量付き StoreLimits を 1 つにまとめる。
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: MeteredLimits,
}

// wasmtime-wasi 29: `WasiView` が `ctx()` と `table()` の両方を提供する。
impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

// ============================================================================
// 設定
// ============================================================================

struct Settings {
    database_url: String,
    nats_url: String,
    /// 取得した wasm 本体の事前コンパイル成果物 (cwasm) を置くローカルキャッシュ先 (§3.6)。
    /// 起動時に mkdir する。M2 で `COMPONENTS_DIR` 依存は撤去した。
    wasm_cache_dir: PathBuf,
    // M8-3: `ack_wait_secs` / `backoff_secs` はここから削除された。consumer の作成者が
    // control-plane へ移ったため、**TTL 結合（§3.3: トークン exp と再配送間隔）の所有者も CP** になった。
    // worker が同じ env を読んで別の値を持つと「どちらが効いているのか分からない」状態になるため、
    // 二重に持たない。`max_deliver` だけは worker が「今回が最終試行か」を判定するのに要るので残す
    // （CP と同じ env を読むので値はずれない）。
    /// M4c (§6.6): 最終配送試行の判定に使う。CP が consumer に設定する値と同じ env から読む。
    max_deliver: u64,
    /// M4a (§3.8): /metrics + /readyz を返す最小 axum サーバの bind 先。既定 `0.0.0.0:9090`。
    /// 内部ネット越しのみで露出させる前提。ノード LB の readinessProbe ターゲットでもある。
    metrics_bind_addr: String,
    /// M7c (§4.6): control-plane の**内部専用** listener の URL（`INTERNAL_BIND_ADDR` を指す）。
    /// secret を持つ component の実行時に `POST {url}/internal/job-env` で引き換える。
    control_plane_internal_url: String,
    /// M7c: 引き換え HTTP のタイムアウト（ミリ秒）。超過は **fail-closed**（実行を failed で終端）。
    job_env_fetch_timeout_ms: u64,
    /// M8 (§3.7): lane discovery の周期（秒）。`faas.lane.changed` 通知の取りこぼしと
    /// worker 再起動直後を吸収する収束用の保険であり、通常の即応性は通知が担う。
    lane_discovery_interval_secs: u64,
}

/// `WASM_CACHE_DIR` 未設定時の既定キャッシュ先。
const DEFAULT_WASM_CACHE_DIR: &str = "./worker-cache";
/// `METRICS_BIND_ADDR` 未設定時の既定。
const DEFAULT_METRICS_BIND_ADDR: &str = "0.0.0.0:9090";
/// `CONTROL_PLANE_INTERNAL_URL` 未設定時の既定（CP の `INTERNAL_BIND_ADDR` 既定に対応）。
const DEFAULT_CONTROL_PLANE_INTERNAL_URL: &str = "http://127.0.0.1:8081";
/// `JOB_ENV_FETCH_TIMEOUT_MS` 未設定時の既定。
const DEFAULT_JOB_ENV_FETCH_TIMEOUT_MS: u64 = 2000;
/// `LANE_DISCOVERY_INTERVAL_SECS` 未設定時の既定。
const DEFAULT_LANE_DISCOVERY_INTERVAL_SECS: u64 = 10;
/// ゲスト stderr を封じ込めるときのバッファ上限（バイト）。
///
/// secret を注入する実行では `inherit_stderr()` を**使わない**。inherit すると、ゲストが
/// うっかり（あるいは意図的に）env をダンプした内容が**全テナント共有のコンテナログ**へ
/// 直結してしまう（§5.5）。内容はログにも `executions.error` にも載せず捨て、捨てたバイト数
/// だけを観測する。
const GUEST_STDERR_CAPTURE_BYTES: usize = 64 * 1024;

impl Settings {
    fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
        let nats_url = std::env::var("NATS_URL").context("NATS_URL must be set")?;
        // 未設定時は既定 (`./worker-cache`) を採用する。
        let wasm_cache_dir = std::env::var("WASM_CACHE_DIR")
            .unwrap_or_else(|_| DEFAULT_WASM_CACHE_DIR.to_string())
            .into();
        // M3c TTL 結合: 既定は faas_shared の定数。CP と値を揃えること（README 参照）。
        let max_deliver = env_u64("MAX_DELIVER", faas_shared::MAX_DELIVER)?;
        let metrics_bind_addr = std::env::var("METRICS_BIND_ADDR")
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|_| DEFAULT_METRICS_BIND_ADDR.to_string());
        Ok(Self {
            database_url,
            nats_url,
            wasm_cache_dir,
            max_deliver,
            metrics_bind_addr,
            control_plane_internal_url: std::env::var("CONTROL_PLANE_INTERNAL_URL")
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|_| DEFAULT_CONTROL_PLANE_INTERNAL_URL.to_string()),
            job_env_fetch_timeout_ms: env_u64(
                "JOB_ENV_FETCH_TIMEOUT_MS",
                DEFAULT_JOB_ENV_FETCH_TIMEOUT_MS,
            )?,
            lane_discovery_interval_secs: env_u64(
                "LANE_DISCOVERY_INTERVAL_SECS",
                DEFAULT_LANE_DISCOVERY_INTERVAL_SECS,
            )?,
        })
    }
}

/// u64 の任意 env。欠損は default、不正値はエラー。値は trim する。
fn env_u64(key: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .with_context(|| format!("env var {key} must be a non-negative integer")),
        Err(_) => Ok(default),
    }
}

/// `tracing` を初期化する。control-plane と同じ規約: `LOG_FORMAT=json` で構造化 JSON。
fn init_tracing(log_format: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if log_format.eq_ignore_ascii_case("json") {
        fmt()
            .with_env_filter(filter)
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

/// `/metrics` + `/readyz` + `/healthz` を返す最小 axum サーバを別 tokio task で起動する
/// （M4a, §3.8）。
///
/// JetStream pull ループから独立しているため、ジョブ消費が詰まっても liveness 応答は出る。
/// バインドエラーは fatal（プロセスを巻き込まずに warn して諦める）。bind が成功した後の
/// `axum::serve` の Err はループ内 backoff で再 listen するほどでもないので、終了時にログのみ。
///
/// readiness 判定:
/// - 本スライスでは「プロセスが立ち上がっている + 起動初期化が済んだ」をもって Ready とする。
///   NATS pull consumer の生死は pull ループ側のリトライで吸収するため readiness とは結合しない
///   （詰まり時の cascading restart 防止; §3.8 の liveness/readiness 分離方針）。
///   将来、NATS 接続性や DB pool 健全性を probe する場合はここで `state` を握って判定する。
fn spawn_metrics_server(metrics: Arc<metrics::Metrics>, bind_addr: String) {
    tokio::spawn(async move {
        use axum::{extract::State as AxState, routing::get, Router};

        async fn healthz() -> axum::http::StatusCode {
            axum::http::StatusCode::OK
        }
        async fn readyz() -> axum::http::StatusCode {
            // 本スライスでは依存疎通を probe しない（上記コメント参照）。
            axum::http::StatusCode::OK
        }
        async fn metrics_handler(
            AxState(m): AxState<Arc<metrics::Metrics>>,
        ) -> impl axum::response::IntoResponse {
            let (headers, body) = m.render();
            (axum::http::StatusCode::OK, headers, body)
        }

        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/readyz", get(readyz))
            .route("/metrics", get(metrics_handler))
            .with_state(metrics);

        let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(addr = %bind_addr, error = %e, "worker metrics server: failed to bind; metrics unavailable");
                return;
            }
        };
        info!(addr = %bind_addr, "worker metrics server listening");
        if let Err(e) = axum::serve(listener, app).await {
            warn!(error = %e, "worker metrics server: serve exited with error");
        }
    });
}

/// ランタイム接続ロールが RLS をバイパスしないことを起動時に検証する（M3b §3.2）。
///
/// FORCE RLS は SUPERUSER / BYPASSRLS ロールに対し無条件にバイパスされる。worker が
/// 特権ロールで接続していると、`SET LOCAL app.tenant_id` 越しの cross-tenant 書き込みガードが
/// 無効化される。`current_user` の `rolsuper`/`rolbypassrls` を確認し、特権なら fail-fast する。
async fn assert_non_privileged_runtime_role(pool: &PgPool) -> anyhow::Result<()> {
    let (rolname, rolsuper, rolbypassrls): (String, bool, bool) = sqlx::query_as(
        "SELECT rolname, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .context("probing runtime DB role privileges")?;

    if rolsuper || rolbypassrls {
        anyhow::bail!(
            "worker DATABASE_URL connects as privileged role '{rolname}' \
             (rolsuper={rolsuper}, rolbypassrls={rolbypassrls}); RLS would be bypassed. \
             Point DATABASE_URL at the non-privileged 'faas_app' role."
        );
    }
    info!(role = %rolname, "runtime DB role is non-privileged (RLS enforced)");
    Ok(())
}

// ============================================================================
// エントリポイント
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // M4a (§3.8): LOG_FORMAT=json で構造化 JSON ログに切り替える。span のフィールド
    // （execution_id 等）をイベントへ flatten する。既定の text は従来挙動と完全互換。
    let log_format = std::env::var("LOG_FORMAT")
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "text".to_string());
    init_tracing(&log_format);

    let settings = Settings::from_env()?;

    info!(nats = %settings.nats_url, "connecting to NATS");
    let nats = async_nats::connect(&settings.nats_url)
        .await
        .context("failed to connect to NATS")?;
    let jetstream = async_nats::jetstream::new(nats.clone());

    info!("connecting to Postgres");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&settings.database_url)
        .await
        .context("failed to connect to Postgres")?;
    // M3b §3.2: ランタイムは必ず非特権 faas_app（NOBYPASSRLS・非 SUPERUSER）で接続する。
    // superuser/owner だと FORCE RLS が無条件にバイパスされ、set_tenant_guc の fail-closed も
    // 働かず、worker の cross-tenant 書き込みガードが無効化される。起動時に検証して fail-fast。
    assert_non_privileged_runtime_role(&pool)
        .await
        .context("runtime DB role check failed")?;

    // 共有 Engine。precompile / deserialize / 実行で同一 Config を共有する。
    // (deserialize は同一 Engine 設定でないと拒否されるため §3.6 / §4)。
    let engine = build_engine().context("failed to build wasmtime engine")?;

    // cwasm キャッシュ先を起動時に用意する (§3.6)。
    std::fs::create_dir_all(&settings.wasm_cache_dir).with_context(|| {
        format!(
            "failed to create WASM_CACHE_DIR {}",
            settings.wasm_cache_dir.display()
        )
    })?;

    // presigned GET URL から本体を取得する HTTP クライアント (§3.4)。
    let http = reqwest::Client::builder()
        .build()
        .context("failed to build reqwest client")?;

    // in-memory Component LRU (§3.6)。sha256 -> Arc<Component>。
    let cache = Mutex::new(LruCache::new(
        NonZeroUsize::new(COMPONENT_CACHE_CAP).expect("cache cap must be non-zero"),
    ));

    // M4a (§3.8): メトリクスは pull ループから独立した tokio task で公開する。pull ループが
    // 詰まっても /metrics と /readyz が応答できるように分離する。
    let worker_metrics = metrics::Metrics::init();
    spawn_metrics_server(worker_metrics.clone(), settings.metrics_bind_addr.clone());

    // M8-1 / M8-3: stream と lane consumer の作成者は **control-plane 単独**である。
    // worker は取得と不変条件の検査だけを行い、購読すべき lane は NATS から発見する。
    let stream = open_invoke_stream(&jetstream).await?;

    // M4c: `.failed` (DLQ) 用の core NATS 経路は stream を貼らず、CP の subscriber が core
    // で購読する（result と同じトランスポート規約）。stream を作らない理由は、(a) DLQ メッセージ
    // は最終配送失敗時に worker が 1 度 publish するだけで JetStream の durable 保証が無くても
    // reaper の stuck-execution sweeper が二重安全網になっていること、(b) stream を増やすと
    // 観測・運用面の複雑度が増し M4c のブラスト半径が広がること、による。

    info!(
        stream = STREAM_NAME,
        cache_dir = %settings.wasm_cache_dir.display(),
        discovery_interval_secs = settings.lane_discovery_interval_secs,
        "worker started; discovering lanes"
    );

    let worker = Arc::new(Worker {
        engine,
        pool,
        nats: nats.clone(),
        http,
        wasm_cache_dir: settings.wasm_cache_dir,
        cache,
        metrics: worker_metrics,
        control_plane_internal_url: settings.control_plane_internal_url.clone(),
        job_env_fetch_timeout: Duration::from_millis(settings.job_env_fetch_timeout_ms),
    });

    run_lane_supervisor(
        worker,
        stream,
        nats,
        settings.max_deliver as i64,
        settings.lane_discovery_interval_secs,
    )
    .await
}

// ============================================================================
// lane supervisor / lane ループ (M8-3, §3.7)
// ============================================================================

/// 購読すべき lane を発見し、lane ごとの pull ループを起動 / 停止し続ける。
///
/// discovery の契機は 2 つ:
/// 1. **`faas.lane.changed` の core NATS 通知**（control-plane が lane を作成 / 削除したとき）。
///    周期 discovery だけだと「lane はあるが worker がまだ購読していない」窓が最大 1 周期残り、
///    新規テナントの初回 `POST /invoke?wait=1` が必ず 202 へ縮退して **M6 の完了条件が壊れる**
///    （`SYNC_REPLY_TIMEOUT_MS` は既定 5 秒）。
/// 2. **周期 tick**（`LANE_DISCOVERY_INTERVAL_SECS`）。通知の取りこぼしと worker 再起動直後を
///    吸収する収束用の保険。
async fn run_lane_supervisor(
    worker: Arc<Worker>,
    stream: async_nats::jetstream::stream::Stream,
    nats: async_nats::Client,
    max_deliver: i64,
    discovery_interval_secs: u64,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    // lane 名 -> 実行中タスク。発見されなくなった lane は abort する。
    let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    // lane 変更通知の購読（core NATS。取りこぼしても周期 tick が収束させるので durable 不要）。
    let mut changed = match nats.subscribe(faas_shared::LANE_CHANGED_SUBJECT).await {
        Ok(sub) => Some(sub),
        Err(e) => {
            // 購読できなくても周期 discovery で収束する。ただし新規テナントの初回同期 invoke が
            // 縮退しうるので loud に警告する。
            warn!(
                error = %e,
                "could not subscribe to lane change notifications;                  falling back to periodic discovery only (first sync invoke of a new tenant may degrade to 202)"
            );
            None
        }
    };

    let mut ticker = tokio::time::interval(Duration::from_secs(discovery_interval_secs.max(1)));

    loop {
        // (1) 現在の lane 一覧を取り、タスク集合を収束させる。
        match discover_lanes(&stream).await {
            Ok(lanes) => {
                // 消えた lane のタスクを止める。
                running.retain(|name, handle| {
                    if lanes.contains(name) {
                        true
                    } else {
                        info!(lane = %name, "lane disappeared; stopping its pull loop");
                        handle.abort();
                        false
                    }
                });
                // 新しい lane のタスクを起こす。
                for name in &lanes {
                    if running.contains_key(name) {
                        continue;
                    }
                    let consumer = match stream
                        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(name)
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(lane = %name, error = %e, "failed to open lane consumer");
                            continue;
                        }
                    };
                    info!(lane = %name, "subscribing to lane");
                    let w = Arc::clone(&worker);
                    let lane_name = name.clone();
                    running.insert(
                        name.clone(),
                        tokio::spawn(async move {
                            lane_loop(w, consumer, lane_name, max_deliver).await;
                        }),
                    );
                }
                worker.metrics.subscribed_lanes.set(running.len() as i64);
            }
            Err(e) => {
                // best-effort: 発見に失敗しても既存の lane ループは動き続ける。
                warn!(error = %e, "lane discovery failed; keeping current subscriptions");
            }
        }

        // (2) 次の契機を待つ。
        match changed.as_mut() {
            Some(sub) => {
                tokio::select! {
                    _ = ticker.tick() => {}
                    msg = sub.next() => {
                        if msg.is_none() {
                            // 購読が閉じた。以後は周期 discovery のみで収束させる。
                            warn!("lane change subscription closed; falling back to periodic discovery");
                            changed = None;
                        }
                    }
                }
            }
            None => {
                ticker.tick().await;
            }
        }
    }
}

/// 1 本の lane（= consumer）を pull し続けるループ。
///
/// **M7 までの共有 consumer ループと、メッセージ処理の中身は 1 行も変えていない**
/// （ack-after-publish / DLQ / 再配送の規約は M3d / M4c で確立した不変条件そのものであり、
/// lane 分割はその外側の「どの consumer から引くか」だけを変える）。
async fn lane_loop(
    worker: Arc<Worker>,
    consumer: async_nats::jetstream::consumer::Consumer<
        async_nats::jetstream::consumer::pull::Config,
    >,
    lane: String,
    max_deliver: i64,
) {
    loop {
        let mut batch = match consumer
            .batch()
            .max_messages(PULL_BATCH)
            .expires(Duration::from_secs(30))
            .messages()
            .await
        {
            Ok(b) => b,
            Err(e) => {
                error!(lane = %lane, error = %e, "failed to pull batch; backing off");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        while let Some(item) = batch.next().await {
            let msg = match item {
                Ok(m) => m,
                Err(e) => {
                    warn!(lane = %lane, error = %e, "error reading pulled message");
                    continue;
                }
            };

            // M4c (§6.6): JetStream のメッセージメタデータから今回が何回目の配送かを取り出す。
            // `delivered` は 1 始まりで、今回の試行を含むカウント。`delivered >= max_deliver`
            // のときは「これが最後の試行」であり、ここで publish に失敗すると JetStream は
            // 二度と再配送しないため、worker は自前で `.failed` (DLQ) を publish しなければ
            // ならない（無音失踪を作らない）。info() が取れない（旧サーバ・形式不一致）場合は
            // 安全側で 1（=「最終ではない」扱い）とし、reaper の stuck sweeper に救済を委ねる。
            let delivered = msg.info().map(|i| i.delivered).unwrap_or(1);
            let is_final_attempt = max_deliver > 0 && delivered >= max_deliver;

            // M3d (§8 / §6.6): ack は **結果 publish の成功後** に返す（ack-after-publish）。
            // 以前は処理前に ack していたため、spawn 後のパニック／プロセスクラッシュ／
            // publish_result 失敗で ResultMessage が一度も出ず、subscriber が finalize できず、
            // pending/running 行が孤立して in-flight スロットが恒久リークした（reaper は DB COUNT
            // を真実とするため孤立行を「真実」として数え、回収できない）。AckExplicit + ack_wait +
            // max_deliver は設定済みなので、publish できなかったメッセージは ack せず JetStream に
            // 再配送させ、最終的に finalize+DECR へ収束させる。
            // M4c: ただし `delivered == max_deliver` の最終試行で publish に失敗したら、もう再配送が
            // 来ない（JetStream は終了済み扱い）。そのまま un-ack で放置すると CP の reaper の
            // stuck deadline (既定 900s) まで pending/running が残り続けるため、worker は `.failed`
            // (DLQ) を publish して CP の subscriber に即時 finalize+DECR を促す。`.failed` の
            // publish にも失敗したら最終手段として reaper に委ねる（保険）。
            let worker = Arc::clone(&worker);
            let payload = msg.payload.clone();
            tokio::spawn(async move {
                let published = worker.handle_payload(&payload).await;
                if published {
                    if let Err(e) = msg.ack().await {
                        warn!(error = %e, "failed to ack message after publishing result");
                    }
                } else if is_final_attempt {
                    // 最終配送で publish できなかった: `.failed` を出してから ack する。`.failed`
                    // が出れば CP の DLQ subscriber が即 finalize+DECR する（reaper を待たない）。
                    // 出せなくても ack はしない（保険として再配送可能性を残す。max_deliver 到達後は
                    // 実際には再配送されないため reaper が deadline で拾う）。
                    let dlq_ok = worker
                        .publish_failed_for_payload(&payload, "max_deliver exhausted")
                        .await;
                    if dlq_ok {
                        if let Err(e) = msg.ack().await {
                            warn!(error = %e, "failed to ack message after publishing DLQ");
                        }
                    } else {
                        warn!(
                            "final delivery attempt could not publish result or DLQ; \
                             leaving un-acked (reaper will reclaim via stuck-execution sweep)"
                        );
                    }
                } else {
                    // 通常の再配送経路。ack せず JetStream に再配送させる（backoff 待ち）。
                    warn!(
                        delivered,
                        "did not publish a result; leaving message un-acked for redelivery"
                    );
                }
            });
        }
    }
}

// ============================================================================
// wasmtime Engine 構築
// ============================================================================

/// epoch interruption + async + component-model + fuel を有効にした Engine を作る。
///
/// M4b (§4.3): fuel と epoch は **暴走中断の代替関係** にあり、spec は「両機構を常時同時適用は
/// しない」と明記する。しかし Wasmtime の `Config` は Engine 構築時に **immutable** で、cwasm
/// (precompile) も同一 Config の Engine で deserialize されることが必須（§3.6）。よって
/// per-job で epoch-only / fuel+epoch を切り替えるには Engine を 2 つ持ち cwasm キャッシュも
/// 分割する必要があり、複雑度が大きく増す（§3.6 LRU + cwasm パスのキー空間 + LRU を Engine
/// 種別で 2 系統化）。
///
/// 本スライス（M4b）は「単一 Engine で `consume_fuel(true)` を常時有効化し、per-Store では
/// `set_fuel` を **`max_fuel.is_some()` のときだけ** 呼ぶ」方針を取る。`set_fuel` を呼ばない
/// Store は fuel カウンタが 0 のままだが、`consume_fuel(true)` 自体は実行に影響しない（fuel が
/// 設定されていない Store は OutOfFuel に至らない、というのが Wasmtime の意味論ではないため、
/// 実際は **`set_fuel` を呼ばずに fuel mode の component を実行するとすぐ trap する**）。
/// したがって本スライスでは「fuel が `None` の Store では `set_fuel(u64::MAX)` 相当を入れ、
/// 実質無効化扱いにする」運用とし、コードレベルで明示する。
///
/// 計装オーバーヘッド（§4.3 注意点）: fuel 計装は Cranelift がコード生成時に挿入するため、
/// `consume_fuel(true)` を Engine レベルで有効化すると **全 component が計装される**。fuel を
/// 使わない component に対する非ゼロのオーバーヘッドが発生するが、Engine 分割の運用複雑度
/// より優先する（follow-up: M4c 以降で Engine 分割 + cwasm キャッシュ分離を再評価）。
fn build_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);
    // 別 task の ticker が増分する epoch で実行を中断できるようにする（§4.3 主機構）。
    config.epoch_interruption(true);
    // M4b (§4.3): fuel を Engine レベルで有効化する。実 set_fuel は per-Store で行う。
    // 詳細はこの関数の doc コメント参照。
    config.consume_fuel(true);
    Engine::new(&config).map_err(|e| anyhow!("Engine::new failed: {e}"))
}

// ============================================================================
// JetStream consumer 用意
// ============================================================================

/// invoke stream を取得し、**不変条件を検査する**（M8-1 / M8-3）。
///
/// M7 までは worker が stream と共有 durable consumer の両方を `get_or_create` していたが、
/// M8 で **どちらも control-plane が唯一の作成者**になった。理由は 2 つ:
///
/// 1. テナント別 lane consumer を作るにはテナント一覧とクォータが要り、それは CP の持ち物である。
/// 2. worker 0 台のとき consumer が存在しないと、backlog シグナル（オートスケールの入力）が
///    「未消化の仕事があるのに consumer が無いので読めない」というブートストラップ・
///    デッドロックを起こし、scale-from-zero が原理的に成立しない。
///
/// あわせて、M7 まで worker が持っていた drift 時の `delete_consumer` → 再 create も CP 側の
/// `update_consumer` へ置き換わった。旧実装のコメントは「durable の場合でも stream 側に
/// 再配送状態が残るため消費中ジョブは取りこぼさない」と書いていたが、**ack floor は consumer 側の
/// 状態**であり `delete_consumer` で失われる（`DeliverPolicy::All` と Limits retention の
/// 組み合わせでは stream 全履歴の再配送を招きうる）。
///
/// worker は取得と検査だけを行い、無ければ fail-fast する（CP を先に起動する運用）。
async fn open_invoke_stream(
    jetstream: &async_nats::jetstream::Context,
) -> anyhow::Result<async_nats::jetstream::stream::Stream> {
    use async_nats::jetstream::stream::RetentionPolicy;

    let mut stream = jetstream.get_stream(STREAM_NAME).await.map_err(|e| {
        anyhow!(
            "get_stream({STREAM_NAME}) failed: {e}. \
             start the control-plane first: it is the sole creator of the invoke stream and \
             of the lane consumers (M8-1 / M8-3)"
        )
    })?;

    // Limits のままの stream に lane consumer を作ると、filter の重なりをサーバが拒否しなくなり
    // 「どの subject もちょうど 1 consumer」の保証が静かに退化する。起動時に検査して止める。
    let info = stream
        .info()
        .await
        .map_err(|e| anyhow!("stream info({STREAM_NAME}) failed: {e}"))?;
    if info.config.retention != RetentionPolicy::WorkQueue {
        anyhow::bail!(
            "stream {STREAM_NAME} has retention {:?} but M8 requires WorkQueue \
             (run `make recreate-stream`; see README トラブルシュート)",
            info.config.retention
        );
    }

    Ok(stream)
}

/// この worker が購読すべき lane（= consumer 名）の一覧を NATS から発見する (M8-3, §3.7)。
///
/// **権威は NATS の consumer 一覧**であり、worker は DB も CP の HTTP も触らない
/// （テナント一覧を知る必要が無い ＝ 責務分離）。CP が作った consumer のうち、
/// この worker が扱うべき 3 種（テナント専有 lane / overflow lane / legacy 共有）だけを拾う。
async fn discover_lanes(
    stream: &async_nats::jetstream::stream::Stream,
) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut it = stream.consumer_names();
    while let Some(n) = it.next().await {
        let n = n.map_err(|e| anyhow!("listing consumer names failed: {e}"))?;
        let mine = n == faas_shared::LEGACY_SHARED_DURABLE
            || n == faas_shared::OVERFLOW_LANE_DURABLE
            || faas_shared::tenant_from_lane_durable(&n).is_some();
        if mine {
            names.push(n);
        }
    }
    // 決定的な順序にしておく（ログとメトリクスの読みやすさのため）。
    names.sort();
    Ok(names)
}

// ============================================================================
// Worker 本体
// ============================================================================

/// version 解決の結果（M7b: limits に加え env 許可リストと平文 config を同じ tx で引く）。
#[derive(Debug, Default)]
struct ResolvedVersion {
    limits: ResourceLimits,
    /// `component_versions.capabilities.env`（admin 承認済みの注入可能 env 名）。
    /// 行が引けない / 壊れている場合は空 ＝ **deny-all**（fail-closed）。
    allowed_env: std::collections::BTreeSet<String>,
    /// `function_configs` の平文キー・値。
    config: std::collections::BTreeMap<String, String>,
}

/// `component_versions.capabilities` から env 許可リストを読む（M7b, §4.4）。
///
/// CP 側 `validation::parse_capabilities` と**同じ規則**（後方互換 + fail-closed）を worker 側にも
/// 持つ。crate をまたぐので実装は複製になるが、どちらも「配列は旧形式で env 空 / 壊れた値は
/// deny-all」という 1 行の規則であり、テストで両側に固定する。
fn parse_allowed_env(capabilities: &serde_json::Value) -> std::collections::BTreeSet<String> {
    capabilities
        .as_object()
        .and_then(|m| m.get("env"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter(|k| faas_shared::is_valid_env_key(k))
                .map(str::to_string)
                .collect()
        })
        // 旧形式（素の配列）/ null / 壊れた値 → deny-all。
        .unwrap_or_default()
}

struct Worker {
    engine: Engine,
    pool: PgPool,
    /// result は core NATS で publish する (CP は core subscribe; subscriber.rs)。
    nats: async_nats::Client,
    /// presigned GET URL から本体を取得する HTTP クライアント (§3.4)。
    http: reqwest::Client,
    /// cwasm 事前コンパイル成果物のローカルキャッシュ先 (§3.6)。
    wasm_cache_dir: PathBuf,
    /// in-memory Component キャッシュ。`wasm_sha256` -> 解決済み `Component` (§3.6)。
    /// hit なら即実行でき、coldstart を短縮できる主経路。
    cache: Mutex<LruCache<String, Arc<Component>>>,
    /// 観測メトリクス（M4a, §3.8）。LRU / cwasm のキャッシュヒット率、execute 時間、終端化件数。
    metrics: Arc<metrics::Metrics>,
    /// M7c (§4.6): CP の内部専用エンドポイント URL（secret 引き換え先）。
    control_plane_internal_url: String,
    /// M7c: 引き換え HTTP のタイムアウト。超過は fail-closed。
    job_env_fetch_timeout: Duration,
}

impl Worker {
    /// 1 メッセージ分のペイロードを処理する。失敗してもパニックさせず、
    /// 可能な限り failed の ResultMessage を返す。
    ///
    /// 戻り値（M3d, §8）: 終端 ResultMessage を **publish できたか**。`true` のとき呼び出し側は
    /// メッセージを ack する。`false`（decode 不能 / publish 失敗）のときは ack せず、JetStream の
    /// 再配送（ack_wait + max_deliver）に委ねる —— これにより worker 側の取りこぼしでも最終的に
    /// 結果が出て subscriber の finalize+DECR が走り、in-flight スロットがリークしない。
    ///
    /// M4a (§3.8): `worker.handle_payload` span で包む。JobMessage を decode 後に execution_id /
    /// tenant_id / component を span へ記録し、以降のすべてのログを相関させる。
    #[tracing::instrument(
        name = "worker.handle_payload",
        skip_all,
        fields(
            execution_id = tracing::field::Empty,
            tenant_id = tracing::field::Empty,
            component = tracing::field::Empty,
        )
    )]
    async fn handle_payload(&self, payload: &[u8]) -> bool {
        let job: JobMessage = match serde_json::from_slice(payload) {
            Ok(j) => j,
            Err(e) => {
                // execution_id が取れないため DB / result publish は不能。デコード不能な毒メッセージは
                // 再配送しても無駄なので、結果は出ないが ack させて DLQ 化を避ける（true を返す）。
                error!(error = %e, "failed to decode JobMessage; dropping (acking poison message)");
                return true;
            }
        };

        let execution_id = job.execution_id.clone();
        let tenant_id = job.tenant_id.clone();
        // M4a: span に相関 ID を詰める（JSON ログでは各イベントのトップレベルに展開される）。
        let span = tracing::Span::current();
        span.record("execution_id", execution_id.as_str());
        span.record("tenant_id", tenant_id.as_str());
        span.record("component", job.component.as_str());
        // M3c: CP が署名した不透明トークン。worker は中身を解釈せず、全 result に verbatim に echo する
        // （鍵を持たない。検証は CP の subscriber が kid で行う, §3.3）。
        let job_token = job.job_token.clone();
        info!(%execution_id, component = %job.component, "received job");

        // M4a (§3.8): execute の wall-clock を計測する。outcome ラベルに応じて histogram + counter を
        // 動かす（subscriber 側の finalize と二重計上にはなるが、worker 視点の「実行完了率」を取りたい
        // のと、CP がダウンしていても worker で計測できるので両方で残す）。
        let exec_started = std::time::Instant::now();

        // running へ遷移 (M1: worker が直接 DB 更新)。
        if let Err(e) = self.mark_running(&tenant_id, &execution_id).await {
            warn!(%execution_id, error = %e, "failed to mark running");
            // 続行する: 実行結果側で最終状態を書く。
        }

        let outcome = self.execute(&job).await;
        let exec_elapsed = exec_started.elapsed();
        // M5 (§15): wall_time_ms は resolve_limits/download 込みの handle_payload スコープの
        // 壁時計を一次ソースにする（不変条件 #5）。成功経路は run_component が組んだ usage の
        // wall_time_ms をこの値で上書きし、失敗/timeout 経路は wall のみ判る部分計量を組む。
        let wall_time_ms = duration_to_millis(exec_elapsed);

        let (status_label, result) = match outcome {
            Ok((output, mut usage)) => {
                info!(%execution_id, "job succeeded");
                // run_component スコープより広い handle_payload 壁時計で上書きする。
                usage.wall_time_ms = wall_time_ms;
                (
                    "succeeded",
                    ResultMessage {
                        execution_id: execution_id.clone(),
                        tenant_id: tenant_id.clone(),
                        status: ExecutionStatus::Succeeded,
                        output: Some(output),
                        error: None,
                        job_token: job_token.clone(),
                        // M5 (§15): per-execution 計量。subscriber が finalize 時に clamp_usage で
                        // resource_limits 上限へ切り詰めてから永続化する（信頼境界外, §3.3）。
                        usage: Some(usage),
                    },
                )
            }
            Err(ExecError::Timeout) => {
                warn!(%execution_id, "job timed out");
                (
                    "timeout",
                    ResultMessage {
                        execution_id: execution_id.clone(),
                        tenant_id: tenant_id.clone(),
                        status: ExecutionStatus::Timeout,
                        output: None,
                        error: Some("execution timed out".to_string()),
                        job_token: job_token.clone(),
                        // M5 (§15): timeout/failed 経路は run_component が Err を返すため fuel/peak/
                        // output は判らない。判る計量（wall_time_ms）のみ載せ、残りは 0（未計測）。
                        usage: Some(UsageMetrics {
                            wall_time_ms,
                            ..UsageMetrics::default()
                        }),
                    },
                )
            }
            Err(ExecError::Failed(msg)) => {
                warn!(%execution_id, error = %msg, "job failed");
                (
                    "failed",
                    ResultMessage {
                        execution_id: execution_id.clone(),
                        tenant_id: tenant_id.clone(),
                        status: ExecutionStatus::Failed,
                        output: None,
                        error: Some(msg),
                        job_token: job_token.clone(),
                        // M5 (§15): 同上。失敗経路は wall_time_ms のみ確定。
                        usage: Some(UsageMetrics {
                            wall_time_ms,
                            ..UsageMetrics::default()
                        }),
                    },
                )
            }
        };

        // M4a (§3.8): outcome 別に duration histogram + executions_total を更新する。
        self.metrics
            .wasmtime_execution_duration_seconds
            .with_label_values(&[status_label])
            .observe(exec_elapsed.as_secs_f64());
        self.metrics
            .executions_total
            .with_label_values(&[status_label])
            .inc();

        // TODO(§3.4 / §6.4 大出力退避): 出力がインライン上限（256 KiB）を超える場合は、
        // CP が同梱する **出力キー限定の write 資格**（io_output_key, §3.4）で Object Storage へ
        // 書き出し、ResultMessage に output_ref を載せて subscriber が executions.output_ref を
        // 確定する経路を加える。本スライスは DB 列（executions.output_ref）+ GET 応答 + キー
        // レイアウト（faas_shared::io_output_key）までを敷設し、worker 側の write と
        // ResultMessage.output_ref フィールドは後続（最小実装で広げない方針）に残す。
        //
        // M3c (§3.3「結果の出所認証」): worker は終端状態を **直接 DB に書かない**。
        // 終端状態の唯一の writer は CP の subscriber であり、echo された署名トークンを
        // 検証（kid 署名 + execution_id/tenant_id/version_id を行と突き合わせ + exp/CAS）
        // してから finalize する。worker は鍵を持たない（keyless by design）ので、ここで
        // 直接 UPDATE すると検証を一切経ずに任意 execution を終端化でき、§3.3 が防ぐべき
        // forgery を許してしまう。よって worker は result を publish するだけにする。
        if let Err(e) = self.publish_result(&tenant_id, &result).await {
            // publish 失敗 → 呼び出し側に ack させない（false）。再配送で再試行され、最終的に
            // subscriber が finalize+DECR する。ここで ack してしまうと結果が永久に出ず、
            // pending/running 行と in-flight スロットがリークする（§8）。
            warn!(%execution_id, error = %e, "failed to publish ResultMessage; will NOT ack (redeliver)");
            return false;
        }

        // M6a (§15): 同期 invoke の reply 先が同梱されていれば、終端 result を **追加で** Core NATS の
        // reply subject へも publish する（job_token を verbatim に echo 保持）。これは「速い通知」で
        // あって正の終端化経路ではない: 終端化・計量は依然 result subject の subscriber が単一 finalize
        // パスで担う。よって reply の publish 失敗は best-effort で握りつぶし、ack 戦略を変えない
        // （false を返さない＝再配送を誘発しない）。所有 CP インスタンスの reply 購読タスクが
        // correlation で待機中のハンドラを解決する。未待機（timeout 済み）なら CP 側で drop される。
        if let Some(reply_to) = job.reply_to.as_deref() {
            if let Err(e) = self.publish_reply(reply_to, &result).await {
                warn!(
                    %execution_id,
                    reply_to,
                    error = %e,
                    "failed to publish sync reply (best-effort; result already published, ack unaffected)"
                );
            }
        }
        true
    }

    /// Component を解決して handle を呼び出す。Timeout / Failed / 出力を返す。
    async fn execute(
        &self,
        job: &JobMessage,
    ) -> std::result::Result<(serde_json::Value, UsageMetrics), ExecError> {
        // resource_limits / env 許可リスト / 平文 config を DB から解決する（M7b）。
        // 失敗・未設定時は既定（limits は既定値、env は **deny-all**）。
        let resolved = self
            .resolve_version(&job.tenant_id, &job.component, &job.version)
            .await
            .unwrap_or_default();
        let limits = resolved.limits;

        // M7c (§4.6): secret を持つ component だけ CP の内部エンドポイントから引き換える。
        // **worker は KEK を持たない**（keyless by design, §3.3）ので、平文は「CP のメモリ →
        // TLS 上の HTTP レスポンス → worker のメモリ → WasiCtx」だけを通り、NATS にも DB にも
        // S3 にも永続化されない。`env_token` が None（= secret 無し）なら往復ゼロ。
        //
        // **fail-closed**: 引き換えに失敗したら secret 欠損のまま実行してはならない。
        // ExecError::Failed で終端させ、JetStream の backoff 再配送が自然にリトライになる。
        let secrets = match job.env_token.as_deref() {
            Some(token) => self.fetch_job_env(token).await?,
            None => std::collections::BTreeMap::new(),
        };

        // M7b (§4.4): 許可リストで畳んで注入する env を組み立てる。許可リストに無いキーは
        // 値が DB に存在しても注入しない（admin 承認が権威。CP 側の引き換えでも同じ許可リストで
        // 絞っており、この二重防御で片側の実装ミスが即漏洩にならないようにする）。
        let built_env = env::build_env(&resolved.config, &secrets, &resolved.allowed_env);
        if built_env.dropped_unapproved > 0 {
            // 「設定したのに入っていない」の切り分けを可能にする（キー名は出さない）。
            tracing::debug!(
                dropped = built_env.dropped_unapproved,
                "dropped env entries not present in the approved allowlist"
            );
        }

        // §3.6 のキャッシュ階層で Component を解決する。
        let component = self.resolve_component(job).await?;

        // 入力を取得する (§3.4 / §5.2)。大入力時は CP が同梱した **そのキー限定** の短命
        // presigned GET URL（input_url）から取得する。worker はこの URL 以外のオブジェクトを
        // 読まない（CP が input_ref を当該 execution の入力キーへ完全一致検証済み, §3.4 MUST NOT）。
        // インライン invoke では input_url=None で、JobMessage.input をそのまま使う。
        let input_bytes = match job.input_url.as_deref() {
            Some(url) => self.download_input(url).await?,
            None => serde_json::to_vec(&job.input)
                .map_err(|e| ExecError::Failed(format!("failed to encode input: {e}")))?,
        };

        self.run_component(component, input_bytes, limits, built_env)
            .await
    }

    /// M7c (§4.6): env-token と引き換えに復号済み secret を CP から受け取る。
    ///
    /// **fail-closed**: どの失敗（CP 不達 / タイムアウト / 401 / 403 / 500 / 応答が壊れている）でも
    /// `ExecError::Failed` を返し、secret 欠損のまま実行させない。エラーメッセージには
    /// **HTTP ステータスしか載せない**（応答 body には値が含まれうるため、ログにも
    /// executions.error にも転記しない）。
    ///
    /// `env_token` は `ResultMessage` / `FailedMessage` / reply へ **echo しない** (MUST NOT)。
    /// 本関数はトークンをリクエスト body にのみ載せ、返り値にも保持しない。
    async fn fetch_job_env(
        &self,
        env_token: &str,
    ) -> std::result::Result<
        std::collections::BTreeMap<String, faas_shared::Redacted<String>>,
        ExecError,
    > {
        let url = format!(
            "{}/internal/job-env",
            self.control_plane_internal_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .timeout(self.job_env_fetch_timeout)
            .json(&serde_json::json!({ "env_token": env_token }))
            .send()
            .await
            .map_err(|e| {
                // reqwest のエラー表示には URL しか出ない（body は含まれない）。
                ExecError::Failed(format!("secret material unavailable: {}", e.without_url()))
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ExecError::Failed(format!(
                "secret material unavailable: control-plane returned {status}"
            )));
        }

        #[derive(serde::Deserialize)]
        struct JobEnvResponse {
            env: std::collections::BTreeMap<String, String>,
        }
        let body: JobEnvResponse = resp.json().await.map_err(|_| {
            ExecError::Failed("secret material unavailable: malformed response".into())
        })?;

        Ok(body
            .env
            .into_iter()
            .map(|(k, v)| (k, faas_shared::Redacted::new(v)))
            .collect())
    }

    /// 大入力を presigned GET URL（input_url）から取得する (§3.4)。
    ///
    /// CP が `input_ref` を当該 execution の入力キーへ完全一致検証してから presign した URL であり、
    /// worker はそのキー以外を読まない。退避入力は **生バイト列** をそのままハンドラへ渡す
    /// （インライン入力が `serde_json::to_vec(input)` であるのと対称: クライアントが PUT した
    /// バイト列がハンドラの `list<u8>` 入力になる）。
    async fn download_input(&self, url: &str) -> std::result::Result<Vec<u8>, ExecError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| ExecError::Failed(format!("failed to GET input: {e}")))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| ExecError::Failed(format!("input GET returned error status: {e}")))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ExecError::Failed(format!("failed to read input body: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// `job.wasm_sha256` をキーに Component を解決する (§3.6)。
    ///
    /// キャッシュ階層:
    ///   a. in-memory LRU が hit すれば即返す (coldstart 短縮の主経路)。
    ///   b. miss 時、ローカル cwasm `{WASM_CACHE_DIR}/{sha256}.cwasm` が在れば、
    ///      自プラットフォームが生成した信頼できる成果物として deserialize する。
    ///      バージョン不一致等で失敗したら c へフォールバックする。
    ///   c. cwasm が無い/壊れている場合、`job.wasm_url` から本体を取得し sha256 を
    ///      照合 → `precompile_component` で cwasm を生成 → アトミックに書き込み →
    ///      deserialize する。
    ///   d. 解決した Component を in-memory LRU に格納する。
    async fn resolve_component(
        &self,
        job: &JobMessage,
    ) -> std::result::Result<Arc<Component>, ExecError> {
        let sha = &job.wasm_sha256;

        // a. in-memory LRU hit。
        if let Some(component) = self.cache_get(sha) {
            // M4a (§3.8): LRU hit を計上。
            self.metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .inc();
            return Ok(component);
        }

        // b. ローカル cwasm を試す。
        let cwasm_path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));
        if cwasm_path.exists() {
            // SAFETY: deserialize_file は信頼できない入力に対して未定義動作になりうる。
            // ここで読むのは「自分が precompile_component で生成した cwasm」のみであり、
            // 他テナント由来の cwasm は決して読み込まない (§3.6 MUST NOT)。
            // キーは sha256 で、cwasm 自体も自プラットフォーム生成物に限定している。
            match unsafe { Component::deserialize_file(&self.engine, &cwasm_path) } {
                Ok(component) => {
                    let component = Arc::new(component);
                    self.cache_put(sha.clone(), Arc::clone(&component));
                    // M4a (§3.8): cwasm hit を計上。
                    self.metrics
                        .wasmtime_component_cache_hits_total
                        .with_label_values(&["cwasm"])
                        .inc();
                    info!(%sha, "component cache: cwasm hit");
                    return Ok(component);
                }
                Err(e) => {
                    // バージョン不一致・破損など。c へフォールバックする。
                    warn!(%sha, error = %e, "cwasm deserialize failed; recompiling");
                }
            }
        }

        // c. ダウンロード → sha256 照合 → precompile → cwasm 書き込み → deserialize。
        // M4a (§3.8): miss を計上（download + precompile に進む経路）。
        self.metrics.wasmtime_component_cache_misses_total.inc();
        info!(%sha, "component cache: miss; downloading and precompiling");
        let bytes = self.download_wasm(&job.wasm_url).await?;
        self.verify_sha256(&bytes, sha)?;

        let cwasm = self
            .engine
            .precompile_component(&bytes)
            .map_err(|e| ExecError::Failed(format!("precompile_component failed: {e}")))?;

        // アトミックに書き込む (tmp -> rename)。失敗しても実行自体は続行する。
        if let Err(e) = write_atomic(&cwasm_path, &cwasm) {
            warn!(%sha, error = %e, "failed to persist cwasm cache (continuing)");
        }

        // SAFETY: 直前に同一 Engine で生成した cwasm を読み込む。信頼できる自前生成物。
        let component = unsafe { Component::deserialize(&self.engine, &cwasm) }
            .map_err(|e| ExecError::Failed(format!("Component::deserialize failed: {e}")))?;
        let component = Arc::new(component);

        // d. in-memory LRU に格納する。
        self.cache_put(sha.clone(), Arc::clone(&component));
        Ok(component)
    }

    /// in-memory LRU から取得する (hit で参照順を更新)。
    fn cache_get(&self, sha: &str) -> Option<Arc<Component>> {
        let mut cache = self.cache.lock().expect("component cache mutex poisoned");
        cache.get(sha).map(Arc::clone)
    }

    /// in-memory LRU へ格納する。
    fn cache_put(&self, sha: String, component: Arc<Component>) {
        let mut cache = self.cache.lock().expect("component cache mutex poisoned");
        cache.put(sha, component);
    }

    /// presigned GET URL から wasm 本体を取得する (§3.4)。
    async fn download_wasm(&self, url: &str) -> std::result::Result<Vec<u8>, ExecError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| ExecError::Failed(format!("failed to GET wasm: {e}")))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| ExecError::Failed(format!("wasm GET returned error status: {e}")))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ExecError::Failed(format!("failed to read wasm body: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// ダウンロード本体の sha256 (16進) を期待値と照合する。不一致は Failed (§3.6)。
    fn verify_sha256(&self, bytes: &[u8], expected: &str) -> std::result::Result<(), ExecError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let actual = hex_encode(&hasher.finalize());
        if actual.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(ExecError::Failed(format!(
                "wasm sha256 mismatch: expected {expected}, got {actual}"
            )))
        }
    }

    /// 解決済み Component を 1 回実行する。
    ///
    /// 時間・暴走制御 (§4.3):
    /// - **epoch ticker (max_wall_time)**: ゲスト計算の壁時計上限。別 task で `sleep(wall)` →
    ///   `engine.increment_epoch()` を 1 回呼び、ゲスト内ループを `Trap::Interrupt` で停止させる。
    /// - **tokio::time::timeout (max_execution_time)**: ホスト関数（WASI のブロッキング呼び出し
    ///   など）込みの総経過時間上限。epoch はゲスト内コードのみを中断するため、ホスト関数中で
    ///   詰まると epoch では止まらない。よって `instantiate_async + call_handle` 全体を tokio
    ///   タイムアウトで包む。タイムアウトはどちらの原因でも `ExecError::Timeout` に収束する
    ///   （subscriber は `status=timeout` で finalize する）。
    /// - **max_fuel (M4b, 任意)**: `Some(n)` のとき `Store::set_fuel(n)` で fuel を設定する。
    ///   fuel 超過は `Trap::OutOfFuel` として現れ、§4.3 の規定どおり `failed` 分類にする
    ///   （wall-time / execution_time 超過の `timeout` とは別物。決定性が要件の component に
    ///   限り fuel を有効化し、暴走中の fuel 切れは「リソース超過」として失敗扱い）。
    ///   `None` のときは fuel を **無効化** する（後述のため `u64::MAX` 相当を流し込むダミー）。
    async fn run_component(
        &self,
        component: Arc<Component>,
        input: Vec<u8>,
        limits: ResourceLimits,
        built_env: env::BuiltEnv,
    ) -> std::result::Result<(serde_json::Value, UsageMetrics), ExecError> {
        // StoreLimits: max_memory を適用する (§4.3)。
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes as usize)
            .build();

        // M5 (§15): linear memory のピークを観測するため StoreLimits を MeteredLimits で包む。
        // peak は Arc<AtomicU64> に持たせ、Store が future へ move されても末尾で読める。
        let peak_memory = Arc::new(AtomicU64::new(0));
        let metered_limits = MeteredLimits {
            inner: store_limits,
            peak_memory_bytes: Arc::clone(&peak_memory),
        };

        // M7b (§3.5 / §4.4): per-function 環境変数を注入する唯一の場所。
        //
        // `Component` は sha256 キーで LRU 共有されるが `WasiCtx` は **実行ごとに構築される**ため、
        // テナント混線は構造的に起きない（この不変条件は M7c で secret を載せる際の前提でもある）。
        // ゲスト側は `wasi:cli/environment` を import する。これは capability baseline で承認済み
        // （`validation.rs` の `BASELINE_APPROVED_PREFIXES` に `wasi:cli/`）なので baseline の変更は不要。
        let mut wasi_builder = WasiCtxBuilder::new();
        // M7c (§5.5): secret を 1 件でも注入する実行では `inherit_stderr()` を**使わない**。
        // inherit はゲスト stderr を**全テナント共有のコンテナログ**へ直結させるため、ゲストが
        // env をダンプすれば secret がそのままログに載る（`docker compose logs` で読める）。
        // 代わりに上限付きのメモリパイプへ流し、内容はログにも executions.error にも載せず
        // Drop で捨てる（捨てたバイト数だけをメトリクスで観測する）。
        // 平文 config だけの実行は従来どおり inherit する（運用のデバッグ性を落とさない）。
        let captured_stderr = if built_env.has_secret {
            let pipe = wasmtime_wasi::pipe::MemoryOutputPipe::new(GUEST_STDERR_CAPTURE_BYTES);
            wasi_builder.stderr(pipe.clone());
            Some(pipe)
        } else {
            wasi_builder.inherit_stderr();
            None
        };
        for (k, v) in &built_env.pairs {
            wasi_builder.env(k, v);
        }
        let wasi = wasi_builder.build();
        let host = HostState {
            ctx: wasi,
            table: ResourceTable::new(),
            limits: metered_limits,
        };

        let mut store = Store::new(&self.engine, host);
        store.limiter(|state| &mut state.limits);

        // epoch deadline を 1 に設定し、ticker が 1 増分すると中断される。
        store.set_epoch_deadline(1);
        // 既定の epoch deadline 動作は Trap (Trap::Interrupt) で、ticker が `increment_epoch` を
        // 呼んだ直後の guest 内 epoch check で発火する。Cranelift がループ back-edge と関数入口に
        // 自動挿入する。
        //
        // 設計メモ (chaos_d 対策, LIFO-slot 起因): epoch ticker は tokio::spawn では動かさず、
        // 必ず OS スレッドで回す（後段の std::thread::spawn ブロック参照）。tokio 1.x は task 内から
        // spawn された新規 task をその worker の LIFO slot に積み、LIFO slot は他 worker から steal
        // できない。standard handler world は host import を持たず、tight loop の guest が
        // `Future::poll` 内で同期的に走り続けるため、spawning worker は次の scheduling iteration に
        // 戻れず ticker future が永久にスロットに留まる（chaos_d 再現で確認: ticker tokio::spawn 版は
        // increment_epoch が一度も呼ばれず 5s exec_timeout 全域で hang）。
        // OS スレッドに分離することで wasm-busy な tokio worker から独立して必ず ticker が発火し、
        // 50ms 周期で epoch を継続的に bump するため Cranelift の epoch-check 挿入密度や
        // wasmtime 側の yield 経路に依存せず Trap::Interrupt が surface する。

        // M4b (§4.3): fuel の per-Store 設定。
        // - `Some(n)`: 上限 n を設定。超過時は `Trap::OutOfFuel`（後段で `failed` 分類）。
        // - `None`:    Engine レベルで `consume_fuel(true)` を常時有効にしているため
        //              **何も set しないと即 OutOfFuel になりうる**。fuel を使わない component
        //              でも実行を阻害しないよう、`u64::MAX` を流し込んで実質無効化する。
        //              これにより既定 component（fuel なし）の挙動は M4a 以前と同一になる。
        // 注意: set_fuel は consume_fuel(false) の Engine では Err を返すが、ここは Engine で
        // 常時 true にしているため Ok を期待する。失敗は `Failed` として上に伝える（決定的に
        // 検知できるよう ExecError 化）。
        let fuel_to_set = limits.max_fuel.unwrap_or(u64::MAX);
        store
            .set_fuel(fuel_to_set)
            .map_err(|e| ExecError::Failed(format!("failed to set fuel: {e}")))?;

        // Linker: WASI を非同期で配線する。
        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        wasmtime_wasi::add_to_linker_async(&mut linker)
            .map_err(|e| ExecError::Failed(format!("failed to link wasi: {e}")))?;

        // 別 OS スレッドで epoch ticker を起動する。max_wall_time 経過で epoch を継続増分する。
        // ticker は execution 全体（instantiate + call_handle）に対して張る。
        // tokio::spawn ではなく std::thread::spawn を使う理由は上述の LIFO-slot 設計メモ参照。
        // 50ms 周期で経過時間を確認し、wall を超えたら毎周期 increment_epoch を呼ぶ。
        // - 単発ではなく繰り返しにすることで、wasmtime 側の epoch-check 挿入密度に左右されず
        //   ゲスト/ホストが次の check に到達した瞬間に確実に deadline を超過させる。
        // - cleanup path で stop フラグを立てることでスレッドは自然に終了する（最大 1 tick 遅延）。
        let engine = self.engine.clone();
        let wall = limits.max_wall_time();
        let started = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let engine_t = engine.clone();
        std::thread::spawn(move || {
            let tick = Duration::from_millis(50);
            let thread_started = std::time::Instant::now();
            while !stop_t.load(Ordering::Relaxed) {
                std::thread::sleep(tick);
                if thread_started.elapsed() >= wall {
                    // wall 経過後は毎 tick 増分し続ける。host blocked / LIFO race / async yield
                    // upgrade などで一発では届かないケースでも次の epoch check で必ず Trap させる。
                    engine_t.increment_epoch();
                }
            }
        });

        // M4b (§4.3): instantiate + call_handle 全体を tokio タイムアウトで包む。
        // epoch だけではホスト関数（WASI）内の sleep / blocking I/O を中断できないため、
        // ホスト経過時間込みの総時間で wall-clock 上限をかぶせる二重防御。タイムアウト時は
        // future が drop され、worker task は次の job へ進む（Wasmtime ランタイムは Store と
        // ともに drop される）。
        let exec_timeout = limits.max_execution_time();
        let exec_future = async {
            // TODO(§3.6): 将来最適化として、ここを `InstancePre` による事前
            // インスタンス化（リンク済み Component を再利用）や Pooling アロケータへ
            // 引き上げ、coldstart をさらに短縮する。今回は Component キャッシュ +
            // 事前コンパイル (cwasm) までを実装範囲とする。
            let instance = Handler::instantiate_async(&mut store, &component, &linker)
                .await
                .map_err(|e| ExecError::Failed(format!("failed to instantiate handler: {e}")))?;
            // handle 呼び出し (async)。epoch 中断時は Err(trap) になる。
            Ok::<_, ExecError>(instance.call_handle(&mut store, &input).await)
        };

        let timed = tokio::time::timeout(exec_timeout, exec_future).await;

        // ticker を停止する (正常終了でも timeout でも不要)。
        // OS スレッドは次の 50ms tick で stop を観測して終了する（detached, join しない）。
        stop.store(true, Ordering::Relaxed);

        // M7c (§5.5): secret を注入した実行のゲスト stderr は**内容を一切見ずに捨てる**。
        // ログにも executions.error にも載せない（載せた瞬間、共有ログ経由の漏洩になる）。
        // 捨てたバイト数だけをメトリクスで観測し、「ゲストが何か書いている」ことは分かるが
        // 「何を書いたか」は分からない状態にする。
        if let Some(pipe) = captured_stderr {
            let dropped = pipe.contents().len() as u64;
            if dropped > 0 {
                self.metrics
                    .guest_stderr_dropped_bytes_total
                    .inc_by(dropped);
            }
        }

        match timed {
            // tokio タイムアウト（max_execution_time 超過）。host+guest 総時間上限を超えた。
            // §4.3: 時間超過は `timeout` 分類。subscriber は ExecutionStatus::Timeout で finalize する。
            Err(_elapsed) => Err(ExecError::Timeout),
            // 内側で instantiate に失敗した（Failed）。
            Ok(Err(e)) => Err(e),
            // call_handle が完了（成功・ハンドラエラー・trap のいずれか）。
            Ok(Ok(call_result)) => match call_result {
                Ok(Ok(output)) => {
                    // M5 (§15): 成功経路でのみ完全な per-execution 計量を組む。
                    // - cpu_fuel_used: 設定 fuel − 残 fuel。fuel 無効化時（max_fuel=None →
                    //   fuel_to_set=u64::MAX）は意味を持たないので 0 に倒す（一次防御。
                    //   subscriber 側の clamp_usage が二次防御）。store はここでまだ生存している
                    //   ので get_fuel() が読める。
                    let remaining = store.get_fuel().unwrap_or(fuel_to_set);
                    let cpu_fuel_used =
                        fuel_consumed(fuel_to_set, remaining, limits.max_fuel.is_some());
                    // - peak_memory_bytes: MeteredLimits が memory_growing で記録した最大値。
                    //   一度も成長要求が無ければ 0（未計測扱い）。上限は StoreLimits が保証する。
                    let peak_memory_bytes = peak_memory.load(Ordering::Relaxed);
                    // - output_bytes: decode 前の raw バイト長（最も正確な計上点。JSON 化で
                    //   桁が変わらない）。
                    let output_bytes = output.len() as u64;
                    // - wall_time_ms: run_component スコープの経過時間。handle_payload 側の
                    //   exec_started.elapsed() が resolve_limits/download 込みのより広い壁時計で
                    //   あり、最終的に handle_payload がそちらで上書きする（§5 不変条件 #5）。
                    let wall_time_ms = duration_to_millis(started.elapsed());
                    let usage = UsageMetrics {
                        cpu_fuel_used,
                        wall_time_ms,
                        peak_memory_bytes,
                        output_bytes,
                    };
                    Ok((self.decode_output(output), usage))
                }
                Ok(Err(handler_err)) => Err(ExecError::Failed(format!(
                    "handler error [{:?}]: {}",
                    handler_err.kind, handler_err.message
                ))),
                Err(trap) => {
                    // §4.3: fuel 切れは `failed`（リソース超過）。epoch 中断 / wall-time 経過は
                    // `timeout`。trap 種別を優先的に見て分類し、種別不明（プレーンな trap）の
                    // ときに限り `started.elapsed()` で wall 越えを補助判定する（ticker race
                    // 対策: epoch を増分した直後にゲストが別 trap を出すコーナーを timeout に倒す）。
                    if is_out_of_fuel_trap(&trap) {
                        Err(ExecError::Failed(format!("fuel exhausted: {trap}")))
                    } else if is_interrupt_trap(&trap) || started.elapsed() >= wall {
                        Err(ExecError::Timeout)
                    } else {
                        Err(ExecError::Failed(format!("wasm trap: {trap}")))
                    }
                }
            },
        }
    }

    /// 出力バイト列を JSON として解釈する。JSON でなければ生バイト配列にフォールバック。
    fn decode_output(&self, bytes: Vec<u8>) -> serde_json::Value {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => v,
            Err(_) => {
                serde_json::Value::Array(bytes.into_iter().map(serde_json::Value::from).collect())
            }
        }
    }

    // ------------------------------------------------------------------------
    // DB アクセス (sqlx ランタイム API; query! マクロ不使用)
    // ------------------------------------------------------------------------
    //
    // M3b (§3.2): executions / components / component_versions は FORCE RLS 下にある
    // （migrations/0004_rls.sql）。worker は faas_app（NOBYPASSRLS）で接続するため、
    // これらのクエリは事前に `app.tenant_id` GUC が設定された tx 内でしか成立しない
    // （未設定だと RLS ポリシーが ERROR で fail-closed する）。
    // 各 DB アクセスを tx で包み、冒頭で job 由来の tenant を GUC に設定する。
    // WHERE tenant_id=$1 等の述語は belt-and-suspenders として残す。

    /// tx にテナントコンテキストを設定する (§3.2)。control-plane の `db::set_tenant_guc`
    /// と同型: パラメータバインドのみ・`set_config(...,true)`（SET LOCAL 相当）。
    /// SET ステートメント文字列結合は禁止（injection 防止）。
    async fn set_tenant_guc(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
            .bind(tenant_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn mark_running(&self, tenant_id: &str, execution_id: &str) -> anyhow::Result<()> {
        // M3c: tenant の権威は CP-signed claim（subscriber が検証時に行と突き合わせる）。
        //   job.tenant_id はその claim と一致する。worker は鍵なしのまま、引き続き
        //   job.tenant_id で SET LOCAL し RLS 書き込みを tenant tx の下で行う。
        let mut tx = self.pool.begin().await?;
        Self::set_tenant_guc(&mut tx, tenant_id).await?;
        // pending -> running のみ遷移させる (CAS 的)。
        sqlx::query(
            "UPDATE executions \
             SET status = $1, started_at = $2 \
             WHERE id = $3 AND status = $4",
        )
        .bind(ExecutionStatus::Running.as_str())
        .bind(Utc::now())
        .bind(execution_id)
        .bind(ExecutionStatus::Pending.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// component_versions の resource_limits / capabilities.env と function_configs を
    /// **同一 tx** で解決する（M7b, §3.4）。
    ///
    /// 平文 config は worker が DB を直読みする（HTTP 往復ゼロ）。既に worker は
    /// `assert_non_privileged_runtime_role` で `faas_app`（NOBYPASSRLS）であることを起動時に
    /// アサートしており、読み取りは `set_tenant_guc` 済み tx + `WHERE tenant_id = $1` の
    /// 二重防御下で行う（既存 `resolve_limits` と同じ作法）。
    ///
    /// **secret はここでは読まない**。secret は暗号化されており、worker は KEK を持たない
    /// （keyless by design, §3.3）。secret の注入は CP の内部エンドポイントとの引き換えで行う。
    async fn resolve_version(
        &self,
        tenant_id: &str,
        component: &str,
        version: &str,
    ) -> anyhow::Result<ResolvedVersion> {
        use sqlx::Row as _;

        // M3c: tenant の権威は CP-signed claim。job.tenant_id はその claim と一致する。
        //   RLS 読み取りは tenant tx の下で行う（GUC/tx は撤去しない）。
        let mut tx = self.pool.begin().await?;
        Self::set_tenant_guc(&mut tx, tenant_id).await?;
        let row = sqlx::query(
            "SELECT c.id AS component_id, cv.resource_limits AS resource_limits, \
                    cv.capabilities AS capabilities \
             FROM component_versions cv \
             JOIN components c ON c.id = cv.component_id \
             WHERE c.tenant_id = $1 AND c.name = $2 AND cv.version = $3 \
             LIMIT 1",
        )
        .bind(tenant_id)
        .bind(component)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(r) = row else {
            tx.commit().await?;
            // 行が引けない = 承認情報が無い。limits は既定、env は **deny-all**（fail-closed）。
            return Ok(ResolvedVersion::default());
        };

        let component_id: String = r.try_get("component_id")?;
        let limits: ResourceLimits =
            serde_json::from_value(r.try_get("resource_limits")?).unwrap_or_default();
        let allowed_env = parse_allowed_env(&r.try_get::<serde_json::Value, _>("capabilities")?);

        // 平文 config を同じ tx（同じ GUC）で引く。
        let config_rows = sqlx::query(
            "SELECT key, value FROM function_configs \
              WHERE tenant_id = $1 AND component_id = $2 ORDER BY key",
        )
        .bind(tenant_id)
        .bind(&component_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut config = std::collections::BTreeMap::new();
        for row in config_rows {
            config.insert(
                row.try_get::<String, _>("key")?,
                row.try_get::<String, _>("value")?,
            );
        }

        Ok(ResolvedVersion {
            limits,
            allowed_env,
            config,
        })
    }

    // ------------------------------------------------------------------------
    // NATS publish
    // ------------------------------------------------------------------------

    async fn publish_result(&self, tenant_id: &str, result: &ResultMessage) -> anyhow::Result<()> {
        let subject = result_subject(tenant_id);
        let payload = serde_json::to_vec(result)?;
        // M1: result は core NATS で publish する (CP は core subscribe; subscriber.rs)。
        // JetStream stream は invoke 用のみ存在するため、result は素の publish で配送する。
        self.nats
            .publish(subject, payload.into())
            .await
            .map_err(|e| anyhow!("nats publish failed: {e}"))?;
        // 配送の確実性を高めるため flush する。
        self.nats
            .flush()
            .await
            .map_err(|e| anyhow!("nats flush failed: {e}"))?;
        Ok(())
    }

    /// M6a (§15): 同期 invoke の reply subject へ終端 `ResultMessage` を **追加で** publish する。
    ///
    /// `job.reply_to`（`reply.{instance_id}.{correlation_id}`）へ Core NATS で送る。`publish_result`
    /// と同一の `ResultMessage`（job_token を verbatim に echo 保持）を送るため、CP のハンドラは
    /// クライアントへ返す前にこれを署名検証できる（provenance 規約は result subject と同一, §3.3）。
    /// これは速い通知経路であり、終端化・計量は依然 result subject の subscriber が単一 finalize パスで
    /// 担う（reply の成否は worker の ack 戦略に影響しない＝呼び出し側で best-effort 扱い）。
    async fn publish_reply(&self, reply_to: &str, result: &ResultMessage) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(result)?;
        self.nats
            .publish(reply_to.to_string(), payload.into())
            .await
            .map_err(|e| anyhow!("nats publish (reply) failed: {e}"))?;
        self.nats
            .flush()
            .await
            .map_err(|e| anyhow!("nats flush (reply) failed: {e}"))?;
        Ok(())
    }

    /// `.failed` (DLQ) subject へ最終配送失敗通知を publish する (M4c, §6.6 MUST)。
    ///
    /// 呼び出し条件: pull consumer のメタデータで「今回が `delivered == max_deliver` の最終試行」と
    /// 判定でき、かつ `.result` の publish が失敗した場合のみ。これより前の試行は backoff 再配送で
    /// 拾うべきなので呼ばない（早すぎる DLQ 化を防ぐ）。
    ///
    /// 経路: core NATS で `tenant.{tenant_id}.component.failed` に publish する（result と同じ
    /// トランスポート規約）。CP の subscriber::run_failed が purchase 検証してから `failed` 終端化する。
    /// `job_token` は CP がジョブ時に mint した不透明トークンを verbatim に echo する（DLQ 経路の
    /// 出所認証も result と同じ kid + claim 突き合わせを通る、§3.3）。
    async fn publish_failed(&self, tenant_id: &str, failed: &FailedMessage) -> anyhow::Result<()> {
        let subject = failed_subject(tenant_id);
        let payload = serde_json::to_vec(failed)?;
        self.nats
            .publish(subject, payload.into())
            .await
            .map_err(|e| anyhow!("nats publish (failed) failed: {e}"))?;
        self.nats
            .flush()
            .await
            .map_err(|e| anyhow!("nats flush (failed) failed: {e}"))?;
        Ok(())
    }

    /// 失敗 envelope を payload から組み立てて DLQ に publish するベストエフォート (M4c)。
    ///
    /// payload が JobMessage として decode できないとき（毒メッセージ）は execution_id が取れず
    /// 救済不能なので何もせず false を返す（呼び出し側は無音 ack で DLQ ループを断ち切る）。
    /// decode できれば `FailedMessage { execution_id, tenant_id, reason, job_token }` を作って
    /// publish する。publish 自体の失敗も false を返し、最終的な救済は reaper の stuck deadline
    /// に委ねる（DLQ は冗長な高速救済経路で、reaper が真の安全網）。
    async fn publish_failed_for_payload(&self, payload: &[u8], reason: &str) -> bool {
        let job: JobMessage = match serde_json::from_slice(payload) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "DLQ publish skipped: payload is not a JobMessage");
                return false;
            }
        };
        let failed = FailedMessage {
            execution_id: job.execution_id.clone(),
            tenant_id: job.tenant_id.clone(),
            reason: reason.to_string(),
            job_token: job.job_token.clone(),
        };
        match self.publish_failed(&job.tenant_id, &failed).await {
            Ok(()) => {
                warn!(
                    execution_id = %job.execution_id,
                    tenant_id = %job.tenant_id,
                    reason,
                    "published DLQ (.failed) envelope after final delivery attempt"
                );
                self.metrics
                    .dlq_published_total
                    .with_label_values(&["published"])
                    .inc();
                true
            }
            Err(e) => {
                warn!(
                    execution_id = %job.execution_id,
                    tenant_id = %job.tenant_id,
                    error = %e,
                    "failed to publish DLQ; reaper stuck-execution sweeper will reclaim slot"
                );
                self.metrics
                    .dlq_published_total
                    .with_label_values(&["publish_failed"])
                    .inc();
                false
            }
        }
    }
}

// ============================================================================
// 実行結果の内部表現
// ============================================================================

enum ExecError {
    /// wall-time 超過 (epoch 中断)。
    Timeout,
    /// その他の実行失敗。
    Failed(String),
}

/// trap が epoch 中断由来か判定する。wasmtime 29 では `Trap::Interrupt`。
fn is_interrupt_trap(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::Interrupt)
    )
}

/// M4b (§4.3): trap が fuel 切れ由来か判定する。wasmtime 29 では `Trap::OutOfFuel`。
///
/// 仕様: 時間超過は `timeout`、メモリ/fuel 超過は `failed` として記録する（§4.3 末尾）。
/// よって OutOfFuel は `ExecError::Failed` に分類し、subscriber は `status=failed` で finalize する。
fn is_out_of_fuel_trap(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::OutOfFuel)
    )
}

// ============================================================================
// M5 (§15) 計量ヘルパ（純関数; live wasm 不要でユニットテスト可能）
// ============================================================================

/// 消費 fuel を算出する (§15)。`set` は run_component が `set_fuel` した値、`remaining` は
/// `Store::get_fuel()` の残量。消費 = `set - remaining`。
///
/// - `fuel_enabled == false`（`max_fuel=None` → `set=u64::MAX` のダミー）のときは fuel 計量に
///   意味が無いため **0 に倒す**（一次防御。subscriber の clamp_usage が二次防御）。
/// - `remaining > set`（理論上起きないが防御的に）は `saturating_sub` で 0 を返す。
fn fuel_consumed(set: u64, remaining: u64, fuel_enabled: bool) -> u64 {
    if !fuel_enabled {
        return 0;
    }
    set.saturating_sub(remaining)
}

/// `Duration` をミリ秒の `u64` へ飽和変換する (§15 wall_time_ms)。
/// `u128` のミリ秒が `u64` を超える非現実的なケースでは `u64::MAX` に飽和させる。
fn duration_to_millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

// ============================================================================
// ファイル / ハッシュ ヘルパ
// ============================================================================

/// バイト列を小文字 16進文字列へエンコードする (sha256 照合用)。
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `path` へアトミックに書き込む (同一ディレクトリの tmp に書いてから rename)。
///
/// 複数 worker / 同一 worker の並行ダウンロードが同じ cwasm を書いても、
/// 最終的な可視ファイルが部分書き込みにならないようにする (§3.6)。
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // tmp 名は衝突を避けるため pid + nanos を含める。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("cwasm"),
        std::process::id(),
        nanos
    ));
    std::fs::write(&tmp, data)?;
    // rename は同一ファイルシステム内でアトミック。既存があっても置き換える。
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 失敗時は tmp を掃除しておく。
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M4b (§4.3): `ResourceLimits::max_execution_time()` は `max_execution_time_ms` を
    /// `Duration` として返し、worker が `tokio::time::timeout(...)` でホスト+ゲスト総時間を
    /// 覆うときの境界を決める。既定（5000ms）と任意値の両方で正しいことを担保する。
    #[test]
    fn resource_limits_max_execution_time_matches_ms() {
        // 既定は ResourceLimits の DEFAULT_MAX_EXECUTION_TIME_MS（5000ms）由来。
        let d = ResourceLimits::default();
        assert_eq!(d.max_execution_time(), Duration::from_millis(5000));
        // 任意値も同じ単位で。
        let custom = ResourceLimits {
            max_execution_time_ms: 250,
            ..ResourceLimits::default()
        };
        assert_eq!(custom.max_execution_time(), Duration::from_millis(250));
    }

    /// M4b (§4.3): tokio::time::timeout(max_execution_time) で「ホスト関数中で詰まる」
    /// パスをモデル化する。WASI のブロッキングホスト関数を呼んだまま帰ってこない future を
    /// `tokio::time::sleep` で代用し、worker の `run_component` が `Err(_elapsed)` 経路を
    /// 辿って `ExecError::Timeout` を返すことを future の合成で再現する。
    ///
    /// 実 wasm を要さずに timeout 分岐を行使できるのが要点（M4b の e2e は live worker +
    /// 永久ブロックする component が必要なため #[ignore] で別途用意する）。
    #[tokio::test(start_paused = true)]
    async fn tokio_timeout_short_circuits_host_blocking_future() {
        let limits = ResourceLimits {
            max_wall_time_ms: 10,
            max_execution_time_ms: 50,
            ..ResourceLimits::default()
        };
        // host 側 blocking の代理: 100ms スリープする future（exec_timeout=50ms を超える）。
        let blocking = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, ExecError>(serde_json::Value::Null)
        };
        let timed = tokio::time::timeout(limits.max_execution_time(), blocking).await;
        // 超過は `Err(_elapsed)`。run_component はこれを `ExecError::Timeout` へ写像する。
        assert!(
            timed.is_err(),
            "tokio::time::timeout must trip on a host-blocking future longer than max_execution_time"
        );
    }

    /// M4b (§4.3): 実時間より短い処理は当然 timeout 内に収まる（false-positive 回帰ガード）。
    #[tokio::test(start_paused = true)]
    async fn tokio_timeout_passes_fast_future() {
        let limits = ResourceLimits {
            max_wall_time_ms: 10,
            max_execution_time_ms: 100,
            ..ResourceLimits::default()
        };
        let fast = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<serde_json::Value, ExecError>(serde_json::json!({"ok": true}))
        };
        let timed = tokio::time::timeout(limits.max_execution_time(), fast).await;
        assert!(timed.is_ok(), "fast future must not trip the timeout");
        match timed.unwrap() {
            Ok(v) => assert_eq!(v, serde_json::json!({"ok": true})),
            Err(_) => panic!("inner future must succeed"),
        }
    }

    /// M4b (§4.3) trap 分類: epoch 中断は `Trap::Interrupt` で `Timeout` に倒れる。
    #[test]
    fn interrupt_trap_is_recognized() {
        let err: anyhow::Error = anyhow::Error::new(wasmtime::Trap::Interrupt);
        assert!(is_interrupt_trap(&err));
        assert!(!is_out_of_fuel_trap(&err));
    }

    /// M4b (§4.3) trap 分類: fuel 超過は `Trap::OutOfFuel` で `Failed` に倒れる
    /// （timeout ではない。決定性 fuel 切れは「リソース超過」分類）。
    #[test]
    fn out_of_fuel_trap_is_recognized() {
        let err: anyhow::Error = anyhow::Error::new(wasmtime::Trap::OutOfFuel);
        assert!(is_out_of_fuel_trap(&err));
        assert!(!is_interrupt_trap(&err));
    }

    /// M4b: 関係のない trap（メモリ越境等）はどちらの分類にも該当しない。
    /// `run_component` はこのケースで「elapsed >= wall なら timeout / それ以外は failed」と
    /// 補助判定する（ticker race 対策）。
    #[test]
    fn unrelated_trap_is_not_interrupt_or_fuel() {
        let err: anyhow::Error = anyhow::Error::new(wasmtime::Trap::MemoryOutOfBounds);
        assert!(!is_interrupt_trap(&err));
        assert!(!is_out_of_fuel_trap(&err));
    }

    /// M5 (§15): `fuel_consumed` は fuel 有効時に `set - remaining` を返す。
    /// fuel 無効化時（`max_fuel=None` → `set=u64::MAX`）は意味を持たないので 0 に倒す
    /// （rollup の cpu_fuel_used が天文学的値で汚染されるのを防ぐ一次防御）。`remaining > set`
    /// は理論上起きないが `saturating_sub` で 0 を返すことを担保する。
    #[test]
    fn fuel_consumed_disabled_enabled_and_saturating() {
        // fuel 無効化（fuel_enabled=false）: set が u64::MAX でも 0。
        assert_eq!(fuel_consumed(u64::MAX, 12_345, false), 0);
        assert_eq!(fuel_consumed(1_000_000, 0, false), 0);
        // fuel 有効: 消費 = set - remaining。
        assert_eq!(fuel_consumed(1_000_000, 250_000, true), 750_000);
        // 全消費（残 0）。
        assert_eq!(fuel_consumed(1_000_000, 0, true), 1_000_000);
        // 一切消費せず（残 == set）。
        assert_eq!(fuel_consumed(1_000_000, 1_000_000, true), 0);
        // remaining > set（防御的）: saturating_sub で 0。
        assert_eq!(fuel_consumed(100, 500, true), 0);
    }

    /// M5 (§15): `duration_to_millis` は `Duration` をミリ秒へ飽和変換する。
    /// 非現実的に巨大な Duration（u64 ミリ秒上限超え）でも `u64::MAX` に飽和し、
    /// オーバーフローや wrap を起こさない。
    #[test]
    fn duration_to_millis_saturates() {
        assert_eq!(duration_to_millis(Duration::from_millis(0)), 0);
        assert_eq!(duration_to_millis(Duration::from_millis(250)), 250);
        assert_eq!(duration_to_millis(Duration::from_secs(5)), 5_000);
        // u64::MAX ミリ秒を超える Duration は u64::MAX に飽和する。
        let huge = Duration::from_secs(u64::MAX);
        assert_eq!(duration_to_millis(huge), u64::MAX);
    }

    /// M5 (§15): output_bytes は decode 前の raw バイト長から取得され、`decode_output` の
    /// JSON 化（成功時は JSON 値、非 JSON 時はバイト配列フォールバック）には影響されない。
    /// JSON バイト列・非 JSON バイナリ列のどちらでも raw `.len()` がそのまま計上点になることを示す。
    #[test]
    fn output_bytes_uses_raw_len_independent_of_decode() {
        // JSON 出力: raw len と decode 後の値は別物。計上は raw len。
        let json_raw = br#"{"ok":true}"#.to_vec();
        let json_len = json_raw.len() as u64;
        // 非 JSON バイナリ: decode_output はバイト配列へフォールバックするが len は raw のまま。
        let bin_raw: Vec<u8> = vec![0x00, 0xff, 0x10, 0x20, 0x7f];
        let bin_len = bin_raw.len() as u64;
        assert_eq!(json_len, 11);
        assert_eq!(bin_len, 5);
        // run_component は `output.len() as u64` を decode 前に取るため、両者とも raw 長で一致する。
        assert_eq!(json_raw.len() as u64, json_len);
        assert_eq!(bin_raw.len() as u64, bin_len);
    }

    /// M5 (§15): `MeteredLimits` の `memory_growing` は許可された成長の `desired` の最大値を
    /// `peak_memory_bytes` に記録する。上限判定は内側 `StoreLimits` に委譲するため、上限以下の
    /// 成長は許可され peak が更新され、上限超過は拒否され peak を汚さないことを確認する。
    #[test]
    fn metered_limits_records_peak_memory() {
        let peak = Arc::new(AtomicU64::new(0));
        let inner = StoreLimitsBuilder::new().memory_size(1024).build();
        let mut limits = MeteredLimits {
            inner,
            peak_memory_bytes: Arc::clone(&peak),
        };
        // 上限内の成長: 許可され peak=512。
        assert!(limits.memory_growing(0, 512, Some(1024)).unwrap());
        assert_eq!(peak.load(Ordering::Relaxed), 512);
        // さらに大きい成長（上限ちょうど）: 許可され peak=1024 に更新。
        assert!(limits.memory_growing(512, 1024, Some(1024)).unwrap());
        assert_eq!(peak.load(Ordering::Relaxed), 1024);
        // 上限超過の成長: 拒否（false）され peak は 1024 のまま（fetch_max は呼ばれない）。
        assert!(!limits.memory_growing(1024, 2048, Some(1024)).unwrap());
        assert_eq!(peak.load(Ordering::Relaxed), 1024);
    }

    // M8-3: `parse_backoff_secs` の単体テストは faas_shared へ移った
    // （`parse_backoff_secs_matches_legacy_rules`）。consumer の作成者が control-plane へ移り、
    // backoff の解釈規則を CP と worker が共有する必要が生じたため、規則ごと共有契約へ移設した。
}
