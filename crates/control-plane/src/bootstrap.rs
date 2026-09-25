//! Process composition: configuration, dependencies, background tasks and listeners.
use crate::metrics;
use crate::migrations::{run_migrate_only, run_migrations};
use crate::routes::{build_internal_router, build_router};
use crate::{
    config::Config, deployment, ingress, migrations, reaper, signing, state::AppState, storage,
    store,
};
use axum::{serve::ListenerExt as _, Router};

fn configure_http_socket(stream: &mut tokio::net::TcpStream) {
    // Headers, body chunks and the final EOF can be separate small writes.
    // Do not hold them behind Nagle's algorithm while waiting for an ACK.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(%error, "cannot enable TCP_NODELAY for HTTP connection");
    }
}
pub(crate) async fn run() -> anyhow::Result<()> {
    // M4a (§3.8): 起動最初に LOG_FORMAT を読んで tracing を初期化する。Config::from_env() より
    // 先に env から読むのは、Config が必須 env 欠損でエラーを返すケースでも JSON ログを吐ける
    // ようにするため（運用側の早期問題切り分けに使う）。trim() は env_or と同じ理由（Makefile
    // include 経由の末尾空白対策）。
    let log_format = std::env::var("LOG_FORMAT")
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "text".to_string());
    // M10 (§3.8): OTel は opt-in（OTEL_EXPORTER_OTLP_ENDPOINT 設定時のみ）。未設定なら M4a の
    // fmt/json ログのみで挙動不変。guard は main の最後まで保持して終了時に span を flush する。
    let _otel_guard = hibana_shared::otel::init_tracing(
        &log_format,
        "info,faas_control_plane=debug",
        "hibana-control-plane",
    );

    // `--migrate-only`: 起動時マイグレーションと同じバージョン管理されたマイグレーションを流して exit。
    // `make migrate` から呼ばれる入口。`Config::from_env()` を経由しないため、BOOTSTRAP_ADMIN_TOKEN /
    // JOB_SIGNING_KEY などのランタイム用必須 env が無くても通る（migrate に必要なのは DB URL だけ）。
    if std::env::args().any(|a| a == "--migrate-only") {
        return run_migrate_only().await;
    }

    let config = Config::from_env()?;
    tracing::info!(bind = %config.bind_addr, "starting control-plane");

    // --- マイグレーション（特権ロールで適用）---
    // RLS、ロール、SECURITY DEFINER 関数の作成には所有者権限が必要。
    // 専用の短命プールで適用し、非特権ランタイムの接続と分離する。
    // 初期マイグレーションは空の DB 専用。旧スキーマは別途切り替える。
    if deployment::run_migrations_on_start()? {
        let migration_pool =
            hibana_database::postgres::connect(&config.migration_database_url, 2, 0).await?;
        run_migrations(&migration_pool).await?;
        migration_pool.close().await?;
    }

    // --- ランタイム DB（非特権 faas_app で接続）---
    // RLS が実効的であるには、ランタイムが必ず NOBYPASSRLS・非 SUPERUSER のロールで
    // 接続していなければならない。superuser/owner は FORCE RLS を無条件にバイパスし、
    // GUC 未設定時の fail-closed も働かない。起動時に self-check で検証して fail-fast する。
    let pool = hibana_database::postgres::connect(&config.database_url, 10, 0).await?;
    migrations::assert_non_privileged_runtime_role(&pool).await?;
    hibana_database::postgres::assert_runtime_schema(&pool).await?;
    tracing::info!("connected to postgres");

    // --- Object Storage (MinIO/S3 互換, §3.4) ---
    let storage = storage::Storage::new(
        &config.s3_endpoint,
        &config.s3_region,
        &config.s3_bucket,
        &config.s3_access_key,
        config.s3_secret_key_plain(),
    )
    .with_compiled_cache(config.compiled_cache_auth.clone());
    tracing::info!(endpoint = %config.s3_endpoint, bucket = %config.s3_bucket, "configured object storage");

    let seed = signing::decode_seed(config.job_signing_key_plain())?;
    let signer = std::sync::Arc::new(signing::Signer::from_seed(
        seed,
        config.job_signing_kid.clone(),
    ));
    tracing::info!(kid = %signer.kid(), "loaded job signing key (Ed25519)");

    // Cross-instance quotas and login throttling always use the shared Redis store.
    // Never substitute a per-process counter when it cannot be reached.
    let store: std::sync::Arc<dyn store::Store> =
        std::sync::Arc::new(store::RedisStore::connect(&config.redis_url).await?);

    // 観測メトリクス（M4a, §3.8）。プロセスで 1 つ。AppState 経由でハンドラ・タスクから参照する。
    let metrics = metrics::Metrics::init();

    let secret_keyring = std::sync::Arc::new(config.secret_keyring()?);
    tracing::info!(kid = %secret_keyring.active_kid(), "loaded secrets KEK keyring");

    let state = AppState::new(
        pool,
        storage,
        config.max_wasm_upload_bytes,
        config.presign_ttl_secs,
        config.bootstrap_admin_token_plain().to_string(),
        signer,
        config.token_margin_secs,
        store,
        config.admission(),
        metrics,
        secret_keyring,
        config.job_env_exchange_rate_per_min,
        config.metrics_include_tenant_label,
        config.public_apps.clone(),
        config.auth.clone(),
    );

    // 完了済み入力の清掃と孤立した pending/running 行の回収。
    let reaper_state = state.clone();
    let reaper_interval = config.reaper_interval_secs;
    // stuck-execution sweeper の deadline。invoke ハンドラの post-commit publish 失敗や worker 側
    // 取りこぼしで孤立した pending/running 行を回収し、in-flight スロットの恒久リークを防ぐ（§8）。
    let stuck_deadline = config.stuck_execution_deadline_secs;
    let retention_days = config.execution_retention_days;
    tokio::spawn(async move {
        reaper::run(
            reaper_state,
            reaper_interval,
            stuck_deadline,
            retention_days,
        )
        .await;
    });

    // --- KEK ローテーション進捗の gauge 更新（M7c-4, §4.7.2）---
    // reaper と同じ周期で回す（頻度を要さない観測なので専用 env は増やさない）。
    let kid_gauge_state = state.clone();
    let kid_gauge_interval = config.reaper_interval_secs;
    tokio::spawn(async move {
        reaper::run_secret_kid_gauge(kid_gauge_state, kid_gauge_interval).await;
    });

    // --- 内部専用 listener (M7c, §4.6.1) ---
    // `POST /internal/job-env` だけを載せた 2 本目の axum サーバを立てる。**公開 listener
    // （BIND_ADDR）には生やさない**。認証 middleware の外にあるため、認証は env-token の署名
    // そのものであり、テナント停止の遮断はハンドラ内で明示的に行う。
    // 既定 bind は loopback（127.0.0.1:8081）。これをインターネット / 共有ネットワークへ
    // 公開してはならない (MUST NOT)。
    let (internal_stop, internal_stopped) = tokio::sync::oneshot::channel::<()>();
    let internal_server = {
        let internal_state = state.clone();
        let internal_addr = config.internal_bind_addr.clone();
        let internal_listener = tokio::net::TcpListener::bind(&internal_addr).await?;
        tracing::info!(addr = %internal_addr, "listening (internal: job-env exchange only)");
        tokio::spawn(async move {
            let internal_app = build_internal_router(internal_state);
            if let Err(e) = axum::serve(
                internal_listener.tap_io(configure_http_socket),
                internal_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = internal_stopped.await;
            })
            .await
            {
                tracing::error!(error = %e, "internal listener terminated");
            }
        })
    };

    crate::artifact_reservations::spawn(state.clone());

    // --- ルータ ---
    let app_server = if let Ok(addr) = std::env::var("APP_BIND_ADDR") {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        let apps = Router::new()
            .fallback(ingress::ingress_fallback)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::maintenance::public_gate,
            ))
            .with_state(state.clone());
        Some(tokio::spawn(async move {
            axum::serve(listener.tap_io(configure_http_socket), apps)
                .with_graceful_shutdown(deployment::shutdown_signal())
                .await
        }))
    } else {
        None
    };
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");
    // OIDC のレート制限はクライアント IP を鍵の一方に使うため、接続元
    // SocketAddr をハンドラへ届ける必要がある。`into_make_service_with_connect_info`
    // で ConnectInfo<SocketAddr> を有効化する（これが無いと OIDC の IP 抽出が機能しない）。
    axum::serve(
        listener.tap_io(configure_http_socket),
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(deployment::shutdown_signal())
    .await?;
    if let Some(server) = app_server {
        server.await??;
    }
    // Workers still redeem jobs/Secrets and persist results while public HTTP drains.
    // Kubernetes stop closes fleet admission before removing any Service endpoints.
    let _ = internal_stop.send(());
    internal_server.await?;

    Ok(())
}
