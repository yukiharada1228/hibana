//! faas-control-plane (bin) — M1 Walking Skeleton。
//!
//! 起動シーケンス (§15 M1):
//! 1. 設定読み込み (env)
//! 2. PgPool 接続
//! 3. NATS 接続
//! 4. result_subject 購読タスクを spawn (ResultMessage → executions 終端更新)
//! 5. axum サーバ起動
//!
//! ルート / 認証 / メッセージは crates/shared の契約に厳密準拠。

mod admission;
mod auth;
mod authz;
mod config;
mod crypto;
mod db;
mod error;
mod extract;
mod handlers;
mod login;
mod metrics;
mod reaper;
mod signing;
mod state;
mod storage;
mod store;
mod subscriber;
mod validation;

use std::time::Duration;

use axum::routing::{delete, get, post, put};
use axum::Router;
use faas_shared::Scope;
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::require_scope;
use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // M4a (§3.8): 起動最初に LOG_FORMAT を読んで tracing を初期化する。Config::from_env() より
    // 先に env から読むのは、Config が必須 env 欠損でエラーを返すケースでも JSON ログを吐ける
    // ようにするため（運用側の早期問題切り分けに使う）。trim() は env_or と同じ理由（Makefile
    // include 経由の末尾空白対策）。
    let log_format = std::env::var("LOG_FORMAT")
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "text".to_string());
    init_tracing(&log_format);

    // `--migrate-only`: 起動時マイグレーションと同じ冪等ロジック（baseline + pending）を流して exit。
    // `make migrate` から呼ばれる入口。`Config::from_env()` を経由しないため、BOOTSTRAP_ADMIN_TOKEN /
    // JOB_SIGNING_KEY などのランタイム用必須 env が無くても通る（migrate に必要なのは DB URL だけ）。
    if std::env::args().any(|a| a == "--migrate-only") {
        return run_migrate_only().await;
    }

    let config = Config::from_env()?;
    tracing::info!(bind = %config.bind_addr, "starting control-plane");

    // --- マイグレーション（特権ロールで適用）---
    // 0004_rls.sql は CREATE ROLE / FORCE RLS / CREATE FUNCTION SECURITY DEFINER を含み、
    // テーブル所有者かつ CREATEROLE 権限を要する。ランタイムの faas_app（NOBYPASSRLS）では
    // 適用できないため、専用の短命プールを所有者 URL で開いて適用し、適用後に閉じる。
    // 0001/0002 は手動適用済みのため baseline 行で再適用をスキップし、0003 以降のみ適用する
    // （冪等。serve 前に必ず完了させる）。
    {
        let migration_pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&config.migration_database_url)
            .await?;
        run_migrations(&migration_pool).await?;
        migration_pool.close().await;
    }

    // --- ランタイム DB（非特権 faas_app で接続）---
    // RLS が実効的であるには、ランタイムが必ず NOBYPASSRLS・非 SUPERUSER のロールで
    // 接続していなければならない。superuser/owner は FORCE RLS を無条件にバイパスし、
    // GUC 未設定時の fail-closed も働かない。起動時に self-check で検証して fail-fast する。
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await?;
    assert_non_privileged_runtime_role(&pool).await?;
    tracing::info!("connected to postgres");

    // --- NATS ---
    let nats = async_nats::connect(&config.nats_url).await?;
    tracing::info!(url = %config.nats_url, "connected to nats");

    // --- Object Storage (MinIO/S3 互換, §3.4) ---
    let storage = storage::Storage::new(
        &config.s3_endpoint,
        &config.s3_region,
        &config.s3_bucket,
        &config.s3_access_key,
        &config.s3_secret_key,
    );
    tracing::info!(endpoint = %config.s3_endpoint, bucket = %config.s3_bucket, "configured object storage");

    // login の no-user パスで使う固定ダミー argon2 ハッシュを起動時に一度だけ生成する
    // （timing oracle 防止: ユーザ不在でも常に verify を実行する）。
    let dummy_password_hash = crypto::dummy_password_hash();

    // --- ジョブ署名鍵 (M3c, §3.3) ---
    // Ed25519 seed を env から復号し、Signer（kid -> 公開鍵マップ付き）を構築する。
    // 鍵は control-plane だけが持つ。worker は鍵なし（不透明トークンを echo するのみ）。
    let seed = signing::decode_seed(&config.job_signing_key)?;
    let signer = std::sync::Arc::new(signing::Signer::from_seed(
        seed,
        config.job_signing_kid.clone(),
    ));
    tracing::info!(kid = %signer.kid(), "loaded job signing key (Ed25519)");

    // invoke の publish 先 JetStream stream を冪等に用意する。worker も同名 stream を
    // ensure_consumer で作るが、CP が先に起動して publish する場合に「no responders /
    // no stream」で落ちないよう、CP 側でも get_or_create_stream しておく（risks の起動順序）。
    {
        let js = async_nats::jetstream::new(nats.clone());
        ensure_invoke_stream(&js).await?;
    }

    // --- 共有 admission ストア（M3d, §8）---
    // 全 Axum インスタンスで共有する低レイテンシカウンタ（Redis）。invoke のレート制限 /
    // in-flight 同時実行 / login 失敗ロックアウトを集計する（§8 MUST: インスタンスローカルだと
    // 上限が実効 N 倍に緩む）。Lua アトミックで TOCTOU を避ける。
    //
    // 起動時に接続を試み、到達不能なら **縮退ストア**（[`store::DegradedStore`]）で起動を継続する
    // （ブートを Redis 単一障害点にしない。§8 の DB 縮退/HA 方針の暫定形）。縮退は警告で記録する。
    //
    // 重要（§8 fail-mode 保全）: フォールバックに素の `InProcStore` は使わない。InProcStore は
    // 決して `Err` を返さないため、login ロックアウトの fail-CLOSED 判定が **プロセス寿命の間
    // 発火できなくなり**、ブルートフォースが素通しする（login が事実上 fail-OPEN へ反転する）。
    // `DegradedStore` は login ロックアウト系を常に `Unavailable` にして fail-CLOSED を保ちつつ、
    // invoke 系（rate / in-flight）だけ in-proc で許可する（fail-OPEN; ただしインスタンス
    // ローカルで分散共有ではない —— §8 の分散カウンタ要件は復旧まで一時的に満たさない）。
    let store: std::sync::Arc<dyn store::Store> = match store::RedisStore::connect(
        &config.redis_url,
    )
    .await
    {
        Ok(s) => {
            tracing::info!(url = %config.redis_url, "connected to shared store (redis)");
            std::sync::Arc::new(s)
        }
        Err(e) => {
            tracing::warn!(
                url = %config.redis_url,
                error = %e,
                "shared store (redis) unreachable at startup; falling back to DEGRADED store \
                 (login lockout still fails CLOSED; invoke rate/in-flight fail OPEN per-instance, \
                 NOT cross-instance shared; §8 distributed-counter requirement temporarily unmet)"
            );
            std::sync::Arc::new(store::DegradedStore::new())
        }
    };

    // token exp 計算器をクロージャ化して AppState に渡す（Config を抱えない）。
    let exp_cfg = config.clone();
    let token_exp_offset_secs =
        Box::new(move |wall_ms: u64| exp_cfg.token_exp_offset_secs(wall_ms));

    // 観測メトリクス（M4a, §3.8）。プロセスで 1 つ。AppState 経由でハンドラ・タスクから参照する。
    let metrics = metrics::Metrics::init();

    let state = AppState::new(
        pool,
        nats,
        storage,
        config.max_wasm_upload_bytes,
        config.presign_ttl_secs,
        config.upload_presign_ttl_secs,
        config.bootstrap_admin_token.clone(),
        dummy_password_hash,
        signer,
        token_exp_offset_secs,
        store,
        config.admission(),
        metrics,
    );

    // --- result 購読タスク ---
    let sub_state = state.clone();
    tokio::spawn(async move {
        subscriber::run(sub_state).await;
    });

    // --- failed (DLQ) 購読タスク (M4c, §6.6 MUST) ---
    // `.result` も `.failed` も来ない無音失踪は reaper の stuck-execution sweeper が拾うが、
    // worker が最終配送失敗を検知して publish する `.failed` メッセージは DLQ subscriber が
    // 即時に finalize+DECR する。結果経路と独立 task で動かし、片方の遅延が他方を blocking
    // しないようにする（subscriber.rs::run_failed の doc 参照）。
    let dlq_state = state.clone();
    tokio::spawn(async move {
        subscriber::run_failed(dlq_state).await;
    });

    // --- in-flight reaper タスク (M3d, §8) ---
    // 共有カウンタは終端パスの DECR で減るが、取りこぼし／二重 DECR／プロセスクラッシュで
    // 真実（DB COUNT）からドリフトする。reaper が定期的に `SELECT COUNT(*) ... pending|running`
    // でテナントごとに再同期する（DB COUNT が唯一の真実; Redis は速い近似）。
    let reaper_state = state.clone();
    let reaper_interval = config.reaper_interval_secs;
    let inflight_ttl = config.inflight_ttl_secs;
    // stuck-execution sweeper の deadline。invoke ハンドラの post-commit publish 失敗や worker 側
    // 取りこぼしで孤立した pending/running 行を回収し、in-flight スロットの恒久リークを防ぐ（§8）。
    let stuck_deadline = config.stuck_execution_deadline_secs;
    tokio::spawn(async move {
        reaper::run(reaper_state, reaper_interval, inflight_ttl, stuck_deadline).await;
    });

    // --- ルータ ---
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");
    // login のロックアウト（§6.0）はクライアント IP を鍵の一方に使うため、接続元
    // SocketAddr をハンドラへ届ける必要がある。`into_make_service_with_connect_info`
    // で ConnectInfo<SocketAddr> を有効化する（これが無いと login の IP 抽出が機能しない）。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// ルータを組み立てる。
///
/// レイヤリング:
/// - スコープ要件はルート群ごとに `route_layer(require_scope(..))` で被せる
///   （GET=>Read / POST /invoke=>Invoke / component・version 作成=>Deploy /
///   全 DELETE・active-version 切替・user/token/tenant 管理=>Admin）。
/// - `authenticate` は /healthz・/auth/login・POST /admin/tenants を**除く**
///   全ルートに適用し、`Principal` を確立する。
///
/// メソッド単位でスコープが異なる（例 /components の POST=Deploy, GET=Read）ため、
/// スコープ別の小ルータに分割して各々へ route_layer を被せてから merge する。
fn build_router(state: AppState) -> Router {
    // --- Read スコープ（一覧・取得） ---
    let read_routes = Router::new()
        .route("/components", get(handlers::list_components))
        .route(
            "/components/{component_id}/versions",
            get(handlers::list_versions),
        )
        .route("/executions/{id}", get(handlers::get_execution))
        // GET /usage: テナント利用量参照 (M5, §15 / §6.0)。principal.tenant_id を権威化し
        // cross-tenant path を持たない（/tenants/{id}/usage の IDOR 面を作らない）。
        .route("/usage", get(handlers::get_usage))
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Read)));

    // --- Invoke スコープ ---
    let invoke_routes = Router::new()
        .route("/invoke", post(handlers::invoke))
        // POST /uploads: 大入力アップロード用の署名付き PUT URL 発行（§5.2 / §6.4。invoke スコープ）。
        .route("/uploads", post(handlers::create_upload))
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Invoke)));

    // --- Deploy スコープ（component / version の作成） ---
    let deploy_routes = Router::new()
        .route("/components", post(handlers::create_component))
        .route(
            "/components/{component_id}/versions",
            post(handlers::upload_version),
        )
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Deploy)));

    // --- Admin スコープ（全 DELETE・active-version 切替・user/token 管理） ---
    let admin_routes = Router::new()
        .route(
            "/components/{component_id}",
            delete(handlers::delete_component),
        )
        .route(
            "/components/{component_id}/versions/{version}",
            delete(handlers::delete_version),
        )
        .route(
            "/components/{component_id}/active-version",
            put(handlers::set_active_version),
        )
        .route("/tenants/{tenant_id}/users", post(handlers::create_user))
        .route("/tokens", post(handlers::create_token))
        .route("/tokens/{token_id}", delete(handlers::revoke_token))
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Admin)));

    // 認証必須ルート（スコープ別ルータを統合し、authenticate で principal を確立）。
    let protected = read_routes
        .merge(invoke_routes)
        .merge(deploy_routes)
        .merge(admin_routes)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ));

    // 認証不要 / 認証 middleware の対象外ルート。
    // - /healthz: liveness。プロセス生存のみ（DB/NATS/Store に触れない, §3.8）。
    // - /readyz: readiness。DB / NATS / Store の 3 つに軽量 ping して 200 か 503。
    // - /metrics: Prometheus exposition（text/plain）。認証や rate-limit の対象外。
    //   内部ネット越しのみで露出させる前提（M4a の scaffolding; 認可は後続スライスで足す）。
    // - /auth/login: 資格情報からトークンを発行する（Bearer 不要）。
    // - POST /admin/tenants: bootstrap トークンでハンドラ内 gate（principal 不要）。
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .route("/metrics", get(handlers::metrics))
        .route("/auth/login", post(login::login))
        .route("/admin/tenants", post(handlers::create_tenant))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// invoke を JetStream へ publish するための stream を冪等に用意する (M3c, §6.6)。
///
/// worker の `ensure_consumer` と **同名・同 subject** の stream を get_or_create する。
/// CP が worker より先に起動して `publish_with_headers(Nats-Msg-Id=execution_id)` する場合に
/// 「stream 不在 / no responders」で publish が落ちないようにするための前提作りである。
/// JetStream は per-stream の重複排除ウィンドウで Nats-Msg-Id をデデュープするため、
/// CP のリトライによる同一 execution_id の二重 enqueue を防げる。
async fn ensure_invoke_stream(jetstream: &async_nats::jetstream::Context) -> anyhow::Result<()> {
    use anyhow::anyhow;
    use async_nats::jetstream::stream::Config as StreamConfig;

    // worker 側と一致: name=FAAS_INVOKE / subjects=[tenant.*.component.invoke]。
    let subject = faas_shared::invoke_subject_wildcard().to_string();
    jetstream
        .get_or_create_stream(StreamConfig {
            name: "FAAS_INVOKE".to_string(),
            subjects: vec![subject],
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow!("get_or_create_stream(FAAS_INVOKE) failed: {e}"))?;
    tracing::info!(stream = "FAAS_INVOKE", "ensured invoke jetstream stream");
    Ok(())
}

/// `--migrate-only` モード: 起動時と同じ migrator を流して exit する。
///
/// `Config::from_env()` を経由しないため、BOOTSTRAP_ADMIN_TOKEN や JOB_SIGNING_KEY 等の
/// ランタイム用 env を要求しない（migrate に必要なのは DB URL だけ）。`MIGRATION_DATABASE_URL`
/// が未設定なら `DATABASE_URL` にフォールバックする（所有者 1 本運用の開発用途）。
///
/// 0004_rls.sql はテーブル所有者 + CREATEROLE 権限を要するため、本番では
/// `MIGRATION_DATABASE_URL` に所有者ロールの URL を渡すこと（ランタイムの faas_app では適用不可）。
async fn run_migrate_only() -> anyhow::Result<()> {
    use anyhow::Context;

    let migration_url = std::env::var("MIGRATION_DATABASE_URL")
        .ok()
        .and_then(|v| {
            let t = v.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        })
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .context("MIGRATION_DATABASE_URL or DATABASE_URL must be set for --migrate-only")?;

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&migration_url)
        .await
        .context("connecting to postgres for --migrate-only")?;
    run_migrations(&pool).await?;
    pool.close().await;
    tracing::info!("migrations applied (idempotent); exiting --migrate-only");
    Ok(())
}

/// 埋め込みマイグレーション（`migrations/`）。コンパイル時に取り込む。
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// 手動適用済みで、ランナー導入時点では `_sqlx_migrations` に未登録のバージョン。
/// これらは **スキーマが実在する場合に限り** baseline 行を挿入して再適用をスキップ
/// する（チェックサム不一致による起動中断も回避する）。0003 以降は通常どおりランナー
/// が適用する。
///
/// 各エントリの第 2 要素は「当該マイグレーションの DDL が実適用済みか」を判定する
/// `SELECT <bool>` プローブ。fresh DB（DDL 未適用）では false を返し、baseline を
/// 行わない → `Migrator::run` が 0001/0002 を通常適用する。これにより from-scratch
/// bootstrap でも 0003 が tenants 不在に当たって落ちる事故を防ぐ。
const BASELINE_VERSIONS: &[(i64, &str)] = &[
    // 0001_init: tenants テーブルの存在で実適用を判定する。
    (1, "SELECT to_regclass('public.tenants') IS NOT NULL"),
    // 0002_m2: component_versions.size_bytes 列の存在で実適用を判定する。
    (
        2,
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name = 'component_versions' AND column_name = 'size_bytes')",
    ),
];

/// pending マイグレーションを適用する。
///
/// 0001/0002 は M1/M2 で手動適用済みのため、まず `_sqlx_migrations` を初期化し、
/// 該当バージョンの DDL が **実在するときに限り** baseline 行（埋め込みファイルの
/// チェックサム付き）を挿入してから `Migrator::run` を呼ぶ。これにより:
/// - 既適用環境（M1/M2 手動適用済み）: 0003 以降のみが適用され、既適用分の再実行や
///   チェックサム検証失敗による中断が起こらない。
/// - fresh DB（DDL 未適用）: baseline をスキップし、`Migrator::run` が 0001/0002 から
///   順に適用する（0003 が tenants 不在で落ちない）。
///
/// 何度呼んでも安全（冪等）。
async fn run_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use anyhow::Context;

    // sqlx の管理テーブルを作成（存在すれば no-op）。Migrator と同一スキーマ。
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
            success BOOLEAN NOT NULL,
            checksum BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .context("creating _sqlx_migrations table")?;

    for mig in MIGRATOR.iter() {
        let Some((_, probe)) = BASELINE_VERSIONS.iter().find(|(v, _)| *v == mig.version) else {
            continue;
        };

        // DDL が実適用済みのときのみ baseline する。fresh DB では false → 通常適用に委ねる。
        let applied: bool = sqlx::query_scalar(probe)
            .fetch_one(pool)
            .await
            .with_context(|| format!("probing migration {} state", mig.version))?;
        if !applied {
            tracing::info!(
                version = mig.version,
                "schema not present; will apply migration normally (not baselining)"
            );
            continue;
        }

        // baseline 対象は埋め込みチェックサムで行を挿入（既存行があれば触らない）。
        let inserted = sqlx::query(
            r#"INSERT INTO _sqlx_migrations
                   (version, description, success, checksum, execution_time)
               VALUES ($1, $2, TRUE, $3, 0)
               ON CONFLICT (version) DO NOTHING"#,
        )
        .bind(mig.version)
        .bind(mig.description.as_ref())
        .bind(mig.checksum.as_ref())
        .execute(pool)
        .await
        .with_context(|| format!("baselining migration {}", mig.version))?;

        if inserted.rows_affected() > 0 {
            tracing::info!(version = mig.version, "baselined pre-applied migration");
        }
    }

    // 0003 以降の pending を適用する。baseline 済みは内容一致のためスキップされる。
    MIGRATOR
        .run(pool)
        .await
        .context("running pending migrations")?;
    tracing::info!("migrations up to date");

    Ok(())
}

/// ランタイム接続ロールが RLS をバイパスしないことを起動時に検証する（M3b §3.2）。
///
/// FORCE RLS は SUPERUSER と BYPASSRLS ロールに対しては無条件にバイパスされる。その場合
/// テナント分離は WHERE 述語のみに退化し、GUC 未設定時の fail-closed も働かない。ランタイムが
/// 誤って特権ロール（典型的には postgres イメージの SUPERUSER な POSTGRES_USER）で接続して
/// いたら、ここで fail-fast して RLS 層が「飾り」になる事故を防ぐ。
///
/// `pg_roles` の `rolsuper` / `rolbypassrls` を `current_user` について確認する。
async fn assert_non_privileged_runtime_role(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use anyhow::{bail, Context};

    let row: (String, bool, bool) = sqlx::query_as(
        "SELECT rolname, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .context("probing runtime DB role privileges")?;
    let (rolname, rolsuper, rolbypassrls) = row;

    if rolsuper || rolbypassrls {
        bail!(
            "runtime DATABASE_URL connects as privileged role '{rolname}' \
             (rolsuper={rolsuper}, rolbypassrls={rolbypassrls}); RLS would be bypassed. \
             Point DATABASE_URL at the non-privileged 'faas_app' role and keep the privileged \
             role only in MIGRATION_DATABASE_URL."
        );
    }
    tracing::info!(role = %rolname, "runtime DB role is non-privileged (RLS enforced)");
    Ok(())
}

/// `tracing` を初期化する。`log_format` が "json" の時は構造化 JSON を吐く（M4a, §3.8）。
///
/// 既定（"text" / 未設定）は従来どおりの人間可読フォーマット。`json` 経路では
/// `tracing_subscriber::fmt().json().flatten_event(true)` で event フィールド（`execution_id` など）を
/// トップレベルにフラット化し、集約基盤の検索が楽になるようにする。span フィールドも一緒に出る
/// ように `with_current_span(true)` を有効化する（後続スライスで invoke ハンドラに開く
/// `info_span!("invoke", execution_id=..., tenant_id=...)` の値が各イベントに付くようにする）。
///
/// `EnvFilter::try_from_default_env()` は `RUST_LOG` 由来。未設定時は info + 自分のクレートを
/// debug、にしてあるが（既存挙動）、JSON 経路でも同じ既定にする（運用切替で意図しない静音化を
/// 起こさないため）。
fn init_tracing(log_format: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,faas_control_plane=debug"));
    if log_format.eq_ignore_ascii_case("json") {
        fmt()
            .with_env_filter(filter)
            .json()
            // event フィールドをトップレベル化（{"execution_id": ..., "message": ...}）。
            .flatten_event(true)
            // 現在の span をイベントに添える（execution_id を span 経由で全ログに自動付与する）。
            .with_current_span(true)
            .with_span_list(false)
            .init();
    } else {
        fmt().with_env_filter(filter).init();
    }
}

#[cfg(test)]
mod migration_tests {
    use super::MIGRATOR;

    // 0007_usage_metering.sql のソースを**コンパイル時**に埋め込む（DB-free 検査用）。
    // MIGRATOR と同じ migrations/ ディレクトリを参照する（main.rs から見た相対パス）。
    const USAGE_METERING_SQL: &str = include_str!("../../../migrations/0007_usage_metering.sql");

    // MIGRATOR が 0007 を**コンパイル時収集**していること（sqlx::migrate! の埋め込み検証）。
    // live DB を要さない（iter() は埋め込み済みメタデータを走査するだけ）。
    #[test]
    fn migrator_includes_version_7() {
        let v7 = MIGRATOR
            .iter()
            .find(|m| m.version == 7)
            .expect("migration version 7 (0007_usage_metering) must be collected by MIGRATOR");
        // ファイル名由来の description（usage_metering）が拾えていること。
        assert!(
            v7.description.contains("usage") || v7.description.contains("metering"),
            "unexpected 0007 description: {}",
            v7.description
        );
    }

    // 0007 を BASELINE_VERSIONS に入れていないこと（= MIGRATOR.run が通常適用する。
    // baseline は手動適用済み 0001/0002 のみに限定し続ける）。
    #[test]
    fn version_7_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 7),
            "0007 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    // usage_rollups の DDL 不変条件（DB-free 文字列検査）: FORCE RLS + fail-closed tenant_isolation。
    #[test]
    fn usage_rollups_is_force_rls_and_fail_closed() {
        assert!(
            USAGE_METERING_SQL.contains("usage_rollups FORCE  ROW LEVEL SECURITY")
                || USAGE_METERING_SQL.contains("usage_rollups FORCE ROW LEVEL SECURITY"),
            "usage_rollups must FORCE ROW LEVEL SECURITY"
        );
        assert!(
            USAGE_METERING_SQL.contains("current_setting('app.tenant_id')"),
            "tenant_isolation must gate on current_setting('app.tenant_id')"
        );
        // fail-closed: 第 2 引数フォールバック（', true)' / ', TRUE)'）を持たないこと（未設定 GUC は ERROR）。
        assert!(
            !USAGE_METERING_SQL.contains("current_setting('app.tenant_id', true)")
                && !USAGE_METERING_SQL.contains("current_setting('app.tenant_id', TRUE)"),
            "tenant_isolation must NOT use the second-arg fallback (must fail closed)"
        );
    }

    // faas_app に DELETE を付与しないこと（集計の改竄/消去を不可にする）。
    #[test]
    fn usage_rollups_does_not_grant_delete_to_faas_app() {
        // GRANT 句は SELECT/INSERT/UPDATE のみ（DELETE を含まない）。
        assert!(
            USAGE_METERING_SQL.contains("GRANT  SELECT, INSERT, UPDATE ON usage_rollups TO   faas_app;")
                || USAGE_METERING_SQL
                    .contains("GRANT SELECT, INSERT, UPDATE ON usage_rollups TO faas_app;"),
            "faas_app must be granted only SELECT/INSERT/UPDATE on usage_rollups"
        );
        // DELETE は防御的に REVOKE され、GRANT ... DELETE ... は存在しないこと。
        assert!(
            USAGE_METERING_SQL.contains("REVOKE DELETE")
                && !USAGE_METERING_SQL.contains("GRANT  SELECT, INSERT, UPDATE, DELETE")
                && !USAGE_METERING_SQL.contains("GRANT SELECT, INSERT, UPDATE, DELETE"),
            "DELETE must not be granted to faas_app on usage_rollups"
        );
    }
}
