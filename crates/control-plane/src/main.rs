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
mod cron;
mod crypto;
mod db;
mod enqueue;
mod error;
mod extract;
mod handlers;
mod handlers_secrets;
mod ingress;
mod lanes;
mod login;
mod metrics;
mod reaper;
mod routing;
mod scale;
mod scheduler;
mod secrets;
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
    // M10 (§3.8): OTel は opt-in（OTEL_EXPORTER_OTLP_ENDPOINT 設定時のみ）。未設定なら M4a の
    // fmt/json ログのみで挙動不変。guard は main の最後まで保持して終了時に span を flush する。
    let _otel_guard = faas_shared::otel::init_tracing(
        &log_format,
        "info,faas_control_plane=debug",
        "faas-control-plane",
    );

    // `--migrate-only`: 起動時マイグレーションと同じ冪等ロジック（baseline + pending）を流して exit。
    // `make migrate` から呼ばれる入口。`Config::from_env()` を経由しないため、BOOTSTRAP_ADMIN_TOKEN /
    // JOB_SIGNING_KEY などのランタイム用必須 env が無くても通る（migrate に必要なのは DB URL だけ）。
    if std::env::args().any(|a| a == "--migrate-only") {
        return run_migrate_only().await;
    }

    // M9b (§6.2): `--validate-stdin` は wasm 検証を **別プロセス**で行う子プロセスの入口。
    // control-plane 本体（親）が `spawn_validation_child` からこのフラグ付きで自分自身を起動する。
    // stdin から wasm を読み、結果 JSON を stdout に書いて exit する。Config::from_env() を
    // 経由しない（検証に必要なのは stdin のバイト列だけで、DB/NATS/鍵は要らない）。
    // 子プロセスは先頭で RLIMIT_AS を張り、悪性 wasm による OOM を自分 1 個に限局する。
    if std::env::args().any(|a| a == validation::VALIDATE_STDIN_FLAG) {
        return validation::run_validate_stdin();
    }

    // M8-1 (§3.4): `--recreate-invoke-stream` は invoke stream を WorkQueue retention で作り直す。
    // retention は NATS で**作成後に変更できない**ため、M8 以前の `Limits` stream が残っている環境では
    // 削除して作り直すしかない。手作業の手順書にすると「未消化 0 の確認」を飛ばす事故が起きるので、
    // **確認を機械にやらせる**（`--migrate-only` と同じ「運用手順を実行可能にする」作法）。
    if std::env::args().any(|a| a == "--recreate-invoke-stream") {
        let force = std::env::args().any(|a| a == "--force");
        return run_recreate_invoke_stream(force).await;
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
        config.s3_secret_key_plain(),
    );
    tracing::info!(endpoint = %config.s3_endpoint, bucket = %config.s3_bucket, "configured object storage");

    // login の no-user パスで使う固定ダミー argon2 ハッシュを起動時に一度だけ生成する
    // （timing oracle 防止: ユーザ不在でも常に verify を実行する）。
    let dummy_password_hash = crypto::dummy_password_hash();

    // --- ジョブ署名鍵 (M3c, §3.3) ---
    // Ed25519 seed を env から復号し、Signer（kid -> 公開鍵マップ付き）を構築する。
    // 鍵は control-plane だけが持つ。worker は鍵なし（不透明トークンを echo するのみ）。
    let seed = signing::decode_seed(config.job_signing_key_plain())?;
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

    // --- secret の KEK キーリング (M7c, §10) ---
    // active KEK（新規暗号化）と retired KEK（復号専用）を env から構築する。
    // control-plane だけがこれを持つ（worker は keyless by design, §3.3）。
    let secret_keyring = std::sync::Arc::new(config.secret_keyring()?);
    tracing::info!(kid = %secret_keyring.active_kid(), "loaded secrets KEK keyring");

    let state = AppState::new(
        pool,
        nats,
        storage,
        config.max_wasm_upload_bytes,
        config.presign_ttl_secs,
        config.upload_presign_ttl_secs,
        config.bootstrap_admin_token_plain().to_string(),
        dummy_password_hash,
        signer,
        token_exp_offset_secs,
        store,
        config.admission(),
        metrics,
        config.instance_id.clone(),
        config.sync_reply_timeout_ms,
        secret_keyring,
        config.job_env_exchange_rate_per_min,
        config.lanes(),
        config.scale_policy(),
        config.metrics_include_tenant_label,
        config.ingress_base_domain.clone(),
    );

    // --- result 購読タスク ---
    let sub_state = state.clone();
    tokio::spawn(async move {
        subscriber::run(sub_state).await;
    });

    // --- 同期 invoke reply 購読タスク (M6a, §15) ---
    // Core NATS で `reply.{instance_id}.*` を 1 本購読し、worker が同期 invoke の終端 result を
    // 追加 publish してきたら、subject 第 3 トークン（correlation_id）で per-instance waiter registry
    // の対応 sender を解決して結果を渡す。**当該インスタンスが送ったジョブの reply だけ** を受け取る
    // （reply subject に埋めた instance_id による per-instance 隔離＝ステートレス×N の鍵, §4 不変条件）。
    // 所有インスタンスが死ねば waiter は解決されず CP/クライアント timeout で 202 へ縮退する。
    let reply_state = state.clone();
    tokio::spawn(async move {
        subscriber::run_sync_reply(reply_state).await;
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

    // --- Cron スケジューラタスク (M6b, §11 / §15) ---
    // CRON_POLL_INTERVAL_SECS 周期で due な Cron ジョブを全テナント横断で引き（cron_due_tenant_jobs()
    // = SECURITY DEFINER）、各テナントの GUC 済み tx で FOR UPDATE SKIP LOCKED により single-flight で
    // 1 ジョブずつ掴んで HTTP invoke と同一 enqueue 正規パスへ合流させる。冪等性（cron slot キー +
    // UNIQUE）・provenance（job_token 署名）・計量（単一 finalize）は enqueue ヘルパが担保する。
    let scheduler_state = state.clone();
    let cron_poll_interval = config.cron_poll_interval_secs;
    tokio::spawn(async move {
        scheduler::run(scheduler_state, cron_poll_interval).await;
    });

    // --- lane reconciler（M8-3, §3.7）---
    // consumer の作成 / 更新 / 削除は **control-plane が唯一の書き手**である（M7 までは worker）。
    // 単一 writer は pg_try_advisory_lock で強制する（CP はステートレス × N が前提のため）。
    let lane_state = state.clone();
    let lane_interval = config.lane_reconcile_interval_secs;
    tokio::spawn(async move {
        lanes::run_lane_reconcile(lane_state, lane_interval).await;
    });

    // --- backlog ポーラ（M8-8, §5.2）---
    // **0 = spawn しない**（既定）。観測ループを増やすのはオプトインにする。
    //
    // reaper の 30 秒周期を再利用しない理由: 30 秒古い depth で判断すると scale-out が最大
    // 30 秒遅れ、それがそのままレイテンシの悪化になる。下の kid gauge が「頻度を要さない観測
    // なので専用 env は増やさない」としているのとは**逆の判断**であり、差は
    // 「観測の鮮度そのものが完了条件に効くかどうか」にある。
    if config.scale_poll_interval_secs > 0 {
        let scale_state = state.clone();
        let scale_interval = config.scale_poll_interval_secs;
        tokio::spawn(async move {
            lanes::run_backlog_poller(scale_state, scale_interval).await;
        });
    } else {
        tracing::info!(
            "backlog poller disabled (SCALE_POLL_INTERVAL_SECS=0); GET /internal/scale will return 503"
        );
    }

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
    {
        let internal_state = state.clone();
        let internal_addr = config.internal_bind_addr.clone();
        let internal_listener = tokio::net::TcpListener::bind(&internal_addr).await?;
        tracing::info!(addr = %internal_addr, "listening (internal: job-env exchange only)");
        tokio::spawn(async move {
            let internal_app = build_internal_router(internal_state);
            if let Err(e) = axum::serve(
                internal_listener,
                internal_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            {
                tracing::error!(error = %e, "internal listener terminated");
            }
        });
    }

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

/// **内部専用**ルータ (M7c §4.6.1 / M8 §5.4)。
///
/// # 載せる 2 ルートは、認証の根拠が異なる
///
/// | ルート | 認証の根拠 |
/// | --- | --- |
/// | `POST /internal/job-env` | **env-token の Ed25519 署名そのもの**。ハンドラ内でテナント停止も明示的に遮断する |
/// | `GET /internal/scale` | **無認証**。ただし応答は stream の集計値のみで、テナント名も execution_id も含まない |
///
/// `GET /internal/scale` に `Principal` を要求しない根拠は「テナントデータを含まないこと」で
/// あり、これは不変条件として維持しなければならない: **この応答に per-tenant の値を足しては
/// ならない**（足すなら同時に認証を課すこと）。この listener は worker が `job-env` を叩く先で
/// あり、worker は Ed25519 鍵を持たない**別トラストドメイン**である（仕様書 §3.3）。したがって
/// ここに載せてよいのは「worker に見せてよい情報」だけで、キュー深さと目標台数はそれを満たす。
///
/// # MUST NOT
///
/// このルータを公開 listener（`BIND_ADDR`）へマウントしてはならない。`build_router` が被せる
/// `auth::authenticate` / `require_scope` の外にあり、`job-env` の認証は署名そのものである。
/// 公開すると、署名鍵を持たない相手でも per-IP レート上限まで総当たりの試行ができる面が
/// インターネットに露出する（署名検証は破れないが、無用な攻撃面を作らない）。
///
/// TraceLayer は付けない —— `TraceLayer::new_for_http()` は URI を span に載せるが、ここは
/// body にトークンを載せる経路であり、リクエストの記録は監査ログ（値を載せない detail 構築点）に
/// 一本化するほうが漏洩面が狭い。
fn build_internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/job-env", post(handlers_secrets::job_env))
        .route("/internal/scale", get(handlers::internal_scale))
        .with_state(state)
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
        // GET /components/{id}/versions/{version}/capabilities: 現在の承認 env 名 / egress 先を返す
        // （M11-9。値は返さない。CLI が全置換 PUT 前にマージするための読み取り）。
        .route(
            "/components/{component_id}/versions/{version}/capabilities",
            get(handlers::get_capabilities),
        )
        // GET /usage: テナント利用量参照 (M5, §15 / §6.0)。principal.tenant_id を権威化し
        // cross-tenant path を持たない（/tenants/{id}/usage の IDOR 面を作らない）。
        .route("/usage", get(handlers::get_usage))
        // GET /cron-jobs: テナントの Cron ジョブ一覧 (M6b, §15)。
        .route("/cron-jobs", get(handlers::list_cron_jobs))
        // GET /triggers: テナントのトリガー一覧 (M6c, §15)。
        .route("/triggers", get(handlers::list_triggers))
        // GET /components/{id}/traffic: 現在の canary 配分 + version 別の直近成績 (M7a, §6.7 / §15)。
        // canary の go/no-go 判断の一次情報（executions 生表の直近 60 分を直読み）。
        .route(
            "/components/{component_id}/traffic",
            get(handlers::get_traffic_split),
        )
        // GET /components/{id}/secrets: secret の**メタデータのみ** (M7c, §10)。
        // 値を返す経路はコードに存在しない。value_len / kek_kid も返さない
        // （value_len は平文長のオラクルになるので admin 専用の /secrets/keys へ分離）。
        .route(
            "/components/{component_id}/secrets",
            get(handlers_secrets::list_secrets),
        )
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Read)));

    // --- Invoke スコープ ---
    let invoke_routes = Router::new()
        .route("/invoke", post(handlers::invoke))
        // POST /uploads: 大入力アップロード用の署名付き PUT URL 発行（§5.2 / §6.4。invoke スコープ）。
        .route("/uploads", post(handlers::create_upload))
        // POST /events/object-storage: Object Storage 通知の取込 (M6c, §15)。tenant_id は
        // オブジェクトキーの tenants/{tenant} プレフィックスから導出し principal と一致を要求する
        // （本文盲信せず anti-spoof）。component を起動する性質上 Invoke スコープに置く。
        .route(
            "/events/object-storage",
            post(handlers::object_storage_event),
        )
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Invoke)));

    // --- Deploy スコープ（component / version の作成） ---
    let deploy_routes = Router::new()
        .route("/components", post(handlers::create_component))
        .route(
            "/components/{component_id}/versions",
            // axum の DefaultBodyLimit（既定 2MiB）は multipart body 全体に効くため、
            // それを超える wasm（JS/Hono コンポーネントは数 MiB〜十数 MiB）は
            // ハンドラのストリーミング検査に届く前に弾かれてしまう。上限を
            // MAX_WASM_UPLOAD_BYTES（+ 他フィールド用の余白 1MiB）に引き上げる。
            // ハード上限の強制自体は upload_version 内のストリーミング検査が担う。
            post(handlers::upload_version).layer(axum::extract::DefaultBodyLimit::max(
                state.max_wasm_upload_bytes() as usize + 1024 * 1024,
            )),
        )
        // PUT /components/{id}/ingress: 公開 HTTP ingress の opt-in 切り替え (M11, §4.2。
        // component ライフサイクル相当の Deploy スコープ)。
        .route(
            "/components/{component_id}/ingress",
            put(handlers::set_component_ingress),
        )
        // POST /cron-jobs: Cron ジョブ登録 (M6b, §15。component ライフサイクル相当の Deploy スコープ)。
        .route("/cron-jobs", post(handlers::create_cron_job))
        // POST /triggers: トリガー登録 (M6c, §15。component ライフサイクル相当の Deploy スコープ)。
        .route("/triggers", post(handlers::create_trigger))
        // --- M7b: per-function 環境変数（平文 config, §15 / §4.4）---
        // 読み書きとも Deploy。GET を Read に置かないのは、config が平文で secret と同じ env
        // 名前空間に混ざるため（資格情報を誤って config へ入れた瞬間、最も広く配られる read
        // スコープが資格情報の読み取り権限になる）。
        .route(
            "/components/{component_id}/config",
            get(handlers::get_function_config),
        )
        .route(
            "/components/{component_id}/config",
            put(handlers::put_function_config),
        )
        .route(
            "/components/{component_id}/config/{key}",
            delete(handlers::delete_function_config),
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
        // DELETE /cron-jobs/{id}: Cron ジョブ削除 (M6b, §15。他 DELETE と整合の Admin スコープ)。
        .route("/cron-jobs/{id}", delete(handlers::delete_cron_job))
        // DELETE /triggers/{id}: トリガー削除 (M6c, §15。他 DELETE と整合の Admin スコープ)。
        .route("/triggers/{id}", delete(handlers::delete_trigger))
        // --- M7a: 段階移行と即時 rollback (§6.7 / §15) ---
        // 版の切替権限を 1 スコープ（Admin）に集約する。既存 PUT /active-version が Admin であり、
        // 分けると「Deploy で stable を動かせるが Admin でないと戻せない」非対称が生まれる。
        //
        // PUT /components/{id}/traffic: canary の版と重みを設定（絶対値・冪等）。
        .route(
            "/components/{component_id}/traffic",
            put(handlers::set_traffic_split),
        )
        // DELETE /components/{id}/traffic: canary を解除（weight=0 + ポインタ NULL）。
        .route(
            "/components/{component_id}/traffic",
            delete(handlers::clear_traffic_split),
        )
        // POST /components/{id}/promote: canary を stable へ昇格（CAS つき単一 UPDATE）。
        .route(
            "/components/{component_id}/promote",
            post(handlers::promote_version),
        )
        // POST /components/{id}/rollback: ワンクリック rollback（canary 破棄 + 直前 stable へ復帰）。
        .route(
            "/components/{component_id}/rollback",
            post(handlers::rollback_version),
        )
        // --- M7b: capability の env 許可リスト承認 (§4.4 / §15) ---
        // PUT /components/{id}/versions/{version}/capabilities:
        // 注入を許可する env 名を承認する。§4.4 MUST「付与は admin スコープを要する」に従い
        // アップロード（Deploy）から分離した専用経路にする（deploy トークンによる権限昇格の遮断）。
        .route(
            "/components/{component_id}/versions/{version}/capabilities",
            put(handlers::approve_capability_env),
        )
        // --- M9c: capability の egress allowlist 承認 (§4.4 / §15 M9) ---
        // PUT /components/{id}/versions/{version}/capabilities/egress:
        // 許可する outbound 先（host:port）を承認する。env と同じく admin 専用経路
        // （deploy トークンが自分で外部到達を承認できてはならない）。
        .route(
            "/components/{component_id}/versions/{version}/capabilities/egress",
            put(handlers::approve_capability_egress),
        )
        // --- M9a: Component 署名鍵の管理 + 署名必須ポリシー (§6.2 / §15 M9) ---
        // 供給網検証: deploy トークンが漏れても、テナント登録鍵で署名された wasm でなければ
        // active にできない。鍵管理とポリシーは admin 専用（deploy から分離）。
        .route("/admin/signing-keys", get(handlers::list_signing_keys))
        .route(
            "/admin/signing-keys/{key_id}",
            put(handlers::register_signing_key).delete(handlers::retire_signing_key),
        )
        .route("/admin/signing-policy", put(handlers::set_signing_policy))
        // --- M7c: Secrets Manager (§10 / §15) ---
        // 書き込み系はすべて admin スコープ + require_admin_role の二重ガード
        // （§4.4「付与（承認）は admin スコープを要する (MUST)」に従う）。
        .route(
            "/components/{component_id}/secrets/{name}",
            put(handlers_secrets::put_secret),
        )
        .route(
            "/components/{component_id}/secrets/{name}/rotate",
            post(handlers_secrets::rotate_secret),
        )
        .route(
            "/components/{component_id}/secrets/{name}",
            delete(handlers_secrets::delete_secret),
        )
        // GET /secrets/keys: 運用向け（kek_kid / value_len はここだけ）。
        .route(
            "/components/{component_id}/secrets/keys",
            get(handlers_secrets::list_secret_keys),
        )
        // POST /admin/secrets/rekey: **当該テナントのみ**を現行 KEK で再ラップする (M7c-4)。
        // 応答は件数のみ（kid 別の内訳は他テナントの総数が漏れるので返さない）。
        .route(
            "/admin/secrets/rekey",
            post(handlers_secrets::rekey_secrets),
        )
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
        // M10 follow-up: テナント status / quotas の platform 管理（bootstrap トークン gate。
        // create_tenant と同じ**非認証グループ**に置き、ハンドラ内で bootstrap トークンを照合する。
        // テナント admin スコープではない —— テナント自身が自分を再有効化 / 増枠できてはならない）。
        .route(
            "/admin/tenants/{tenant_id}/status",
            put(handlers::set_tenant_status),
        )
        .route(
            "/admin/tenants/{tenant_id}/quotas",
            put(handlers::set_tenant_quotas),
        )
        .merge(protected)
        // M11 (§4.2): 公開 HTTP ingress gateway。API ルートにマッチしなかったリクエストのうち
        // Host が `<app>.<tenant>.<INGRESS_BASE_DOMAIN>` のものだけを gateway として処理する
        // （それ以外は 404）。deny-by-default（ingress_enabled な component だけ到達可能）。
        .fallback(ingress::ingress_fallback)
        // M10 follow-up (§3.8): HTTP リクエストメトリクスを observe する。TraceLayer より内側に
        // 置くことで、routing 済み（MatchedPath が extensions に載った状態）で method/route/status を
        // 拾える。**path はルートテンプレート**（`/components/{id}/versions`）を使い、生 URI の
        // ID でカーディナリティを爆発させない。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            http_metrics_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// HTTP リクエストの件数（`faas_http_requests_total`）と処理時間
/// （`faas_http_request_duration_seconds`）を observe する middleware（M4a のメトリクスを配線）。
///
/// カーディナリティ対策として `path` は **MatchedPath**（ルートテンプレート）を使う。ルートに
/// マッチしなかった（404 等）リクエストは 1 つの `<unmatched>` に畳んで、任意 URI による
/// 系列の無限増殖を防ぐ。
async fn http_metrics_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().as_str().to_owned();
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_owned());
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    state
        .metrics()
        .observe_http(&method, &path, resp.status().as_u16(), start.elapsed());
    resp
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
    use async_nats::jetstream::stream::RetentionPolicy;

    // M8-1 (§3.4): **control-plane が stream の唯一の作成者**である（worker からは撤去した）。
    // config の真実は faas_shared::invoke_stream_config()。
    let mut stream = jetstream
        .get_or_create_stream(faas_shared::invoke_stream_config())
        .await
        .map_err(|e| anyhow!("get_or_create_stream(FAAS_INVOKE) failed: {e}"))?;

    // **不変条件の起動時検査 (MUST)**: `get_or_create_stream` は既存 stream があっても
    // config を更新せずそのまま返す。したがって M8 以前の `Limits` stream が残っている環境では、
    // 「どの subject もちょうど 1 consumer」というサーバ強制が**静かに退化する**
    // （lane の filter が重なっても NATS が拒否しなくなり、二重配送が起こりうる）。
    // 黙って劣化させるより起動を止める（assert_non_privileged_runtime_role と同じ作法）。
    let info = stream
        .info()
        .await
        .map_err(|e| anyhow!("stream info(FAAS_INVOKE) failed: {e}"))?;
    if info.config.retention != RetentionPolicy::WorkQueue {
        anyhow::bail!(
            "stream {} has retention {:?} but M8 requires WorkQueue. \
             retention cannot be changed after creation: stop the control-plane and all workers, \
             confirm the stream is drained (curl -s localhost:8222/jsz?streams=true shows messages=0), \
             delete the stream, then start the control-plane again. \
             See README トラブルシュート.",
            faas_shared::INVOKE_STREAM_NAME,
            info.config.retention
        );
    }

    tracing::info!(
        stream = faas_shared::INVOKE_STREAM_NAME,
        retention = ?info.config.retention,
        "ensured invoke jetstream stream"
    );
    Ok(())
}

/// `--recreate-invoke-stream` モード: invoke stream を WorkQueue retention で作り直して exit する (M8-1)。
///
/// # 安全確認（既定。`--force` で省略できる）
///
/// 全 consumer の `num_pending + num_ack_pending` が **0** であることを確認してから削除する。
/// **`messages` が 0 であることは要求しない** —— M8 以前の `Limits` retention では ack 済みの
/// メッセージも stream に残るため、`messages == 0` は正常な運用状態でも達成できない条件である
/// （未消化の仕事があるかどうかを表すのは consumer 側の `num_pending` / `num_ack_pending`）。
///
/// # 失われるもの
///
/// - **`Nats-Msg-Id` の重複排除ウィンドウ**（既定 2 分）がリセットされる。冪等性三層（§6.6）のうち
///   layer 3 が一時的に消えることを意味する。layer 1（`executions` の
///   `(tenant_id, idempotency_key)` 部分 UNIQUE）と layer 2（`executions.id` PK）は DB 側なので
///   効き続けるが、**三層のうち 1 層を意図的に落とす瞬間がある**ことを認識して実行すること。
/// - ack 済みメッセージの履歴（`Limits` retention で溜まっていた分）。実行結果は DB にあるため会計は壊れない。
///
/// # 手順
///
/// 1. control-plane と worker をすべて停止する。
/// 2. `cargo run -p faas-control-plane -- --recreate-invoke-stream`（= `make recreate-stream`）。
/// 3. control-plane → worker の順に起動する。
async fn run_recreate_invoke_stream(force: bool) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context as _};
    use async_nats::jetstream::stream::RetentionPolicy;
    use futures::StreamExt as _;

    let nats_url = std::env::var("NATS_URL").context("NATS_URL must be set")?;
    let nats = async_nats::connect(&nats_url).await?;
    let jetstream = async_nats::jetstream::new(nats);
    let name = faas_shared::INVOKE_STREAM_NAME;

    match jetstream.get_stream(name).await {
        Err(_) => {
            tracing::info!(stream = name, "stream does not exist; will create fresh");
        }
        Ok(mut stream) => {
            // info() は &mut self を取るので、後段の consumers()（&self）と借用が重ならないよう
            // 必要な値だけ先にコピーしてから借用を落とす。
            let (retention, messages) = {
                let info = stream
                    .info()
                    .await
                    .map_err(|e| anyhow!("stream info({name}) failed: {e}"))?;
                (info.config.retention, info.state.messages)
            };
            if retention == RetentionPolicy::WorkQueue {
                tracing::info!(stream = name, "stream is already WorkQueue; nothing to do");
                return Ok(());
            }

            // 未消化の仕事が残っていないことを確認する（残っていれば消すと実行喪失になる）。
            if !force {
                let mut consumers = stream.consumers();
                let mut undelivered = 0u64;
                while let Some(c) = consumers.next().await {
                    let c = c.map_err(|e| anyhow!("consumer info failed: {e}"))?;
                    let outstanding = c.num_pending + c.num_ack_pending as u64;
                    if outstanding > 0 {
                        tracing::warn!(
                            consumer = %c.name,
                            num_pending = c.num_pending,
                            num_ack_pending = c.num_ack_pending,
                            "consumer still has undelivered work"
                        );
                    }
                    undelivered += outstanding;
                }
                if undelivered > 0 {
                    anyhow::bail!(
                        "refusing to delete stream {name}: {undelivered} message(s) are still \
                         unprocessed. start the workers, let them drain, then retry. \
                         (pass --force to delete anyway; in-flight jobs would be lost and their \
                          DB rows finalized as failed by the stuck-execution sweeper)"
                    );
                }
            }

            tracing::warn!(
                stream = name,
                retention = ?retention,
                messages,
                "deleting stream to change retention to WorkQueue \
                 (the Nats-Msg-Id dedup window resets; idempotency layers 1 and 2 remain in the DB)"
            );
            jetstream
                .delete_stream(name)
                .await
                .map_err(|e| anyhow!("delete_stream({name}) failed: {e}"))?;
        }
    }

    let mut created = jetstream
        .create_stream(faas_shared::invoke_stream_config())
        .await
        .map_err(|e| anyhow!("create_stream({name}) failed: {e}"))?;
    let info = created
        .info()
        .await
        .map_err(|e| anyhow!("stream info({name}) failed after create: {e}"))?;
    anyhow::ensure!(
        info.config.retention == RetentionPolicy::WorkQueue,
        "stream {name} was created but retention is {:?}",
        info.config.retention
    );

    tracing::info!(
        stream = name,
        retention = ?info.config.retention,
        "invoke stream recreated with WorkQueue retention; start the control-plane and workers now"
    );
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

#[cfg(test)]
mod migration_tests {
    use super::MIGRATOR;

    // 0007_usage_metering.sql のソースを**コンパイル時**に埋め込む（DB-free 検査用）。
    // MIGRATOR と同じ migrations/ ディレクトリを参照する（main.rs から見た相対パス）。
    const USAGE_METERING_SQL: &str = include_str!("../../../migrations/0007_usage_metering.sql");

    // 0008_m6.sql のソースも同様にコンパイル時埋め込み（DB-free 文字列不変条件検査用）。
    const M6_SQL: &str = include_str!("../../../migrations/0008_m6.sql");

    // 0009_m7a_traffic_split.sql も同様。M7 の migration は**サブマイルストンごとに別ファイル**
    // （0009/0010/0011）にするため、const も 1 ファイル 1 include_str! に分ける。統合ファイルに
    // すると m7a_creates_no_new_table（0009 は新表を作らない）が M7b/M7c の CREATE TABLE で必ず落ちる。
    const M7A_SQL: &str = include_str!("../../../migrations/0009_m7a_traffic_split.sql");
    const M7B_SQL: &str = include_str!("../../../migrations/0010_m7b_function_configs.sql");
    const M7C_SQL: &str = include_str!("../../../migrations/0011_m7c_secrets.sql");

    /// 連続空白を 1 個に潰す（DDL の桁揃えに依存しない部分文字列照合のため）。
    fn squeeze_spaces(sql: &str) -> String {
        let mut s = String::with_capacity(sql.len());
        let mut prev_space = false;
        for c in sql.chars() {
            if c == ' ' || c == '\t' {
                if !prev_space {
                    s.push(' ');
                }
                prev_space = true;
            } else {
                s.push(c);
                prev_space = false;
            }
        }
        s
    }

    /// 新表が ENABLE + FORCE RLS されていること（0007/0008 と同型の DDL 不変条件）。
    fn assert_force_rls(sql: &str, tables: &[&str]) {
        let normalized = squeeze_spaces(sql);
        for table in tables {
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY")),
                "{table} must FORCE ROW LEVEL SECURITY"
            );
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY")),
                "{table} must ENABLE ROW LEVEL SECURITY"
            );
        }
    }

    /// tenant_isolation が fail-closed（`current_setting('app.tenant_id')` の第 2 引数なし）。
    fn assert_fail_closed_isolation(sql: &str, tables: &[&str]) {
        assert!(
            sql.contains("current_setting('app.tenant_id')"),
            "tenant_isolation must gate on current_setting('app.tenant_id')"
        );
        assert!(
            !sql.contains("current_setting('app.tenant_id', true)")
                && !sql.contains("current_setting('app.tenant_id', TRUE)"),
            "tenant_isolation must NOT use the second-arg fallback (must fail closed)"
        );
        for table in tables {
            assert!(
                sql.contains(&format!("CREATE POLICY tenant_isolation ON {table}")),
                "{table} must have a tenant_isolation policy"
            );
        }
    }

    /// 新表に明示 GRANT / REVOKE があること。0004_rls.sql の GRANT はテーブル名の列挙なので
    /// 新表を含まない。書き忘れると RLS 以前に権限エラーで faas_app から一切触れなくなる
    /// （新表追加時の最頻の退行）ため CI で固定する。
    fn assert_explicit_grants(sql: &str, tables: &[&str]) {
        let normalized = squeeze_spaces(sql);
        for table in tables {
            assert!(
                normalized.contains(&format!("REVOKE ALL ON {table} FROM PUBLIC")),
                "{table} must REVOKE ALL FROM PUBLIC"
            );
            assert!(
                normalized.contains(&format!("ON {table} TO faas_app")),
                "{table} must GRANT explicitly to faas_app"
            );
        }
    }

    // ---- 0010_m7b_function_configs.sql の DB-free 文字列不変条件 --------------

    #[test]
    fn migrator_includes_version_10() {
        let v10 = MIGRATOR
            .iter()
            .find(|m| m.version == 10)
            .expect("migration version 10 (0010_m7b_function_configs) must be collected");
        assert!(
            v10.description.contains("m7b") || v10.description.contains("function"),
            "unexpected 0010 description: {}",
            v10.description
        );
    }

    #[test]
    fn version_10_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 10),
            "0010 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    #[test]
    fn m7b_tables_are_force_rls() {
        assert_force_rls(M7B_SQL, &["function_configs"]);
    }

    #[test]
    fn m7b_tenant_isolation_is_fail_closed() {
        assert_fail_closed_isolation(M7B_SQL, &["function_configs"]);
    }

    #[test]
    fn m7b_tables_have_explicit_grants() {
        assert_explicit_grants(M7B_SQL, &["function_configs"]);
    }

    /// component 参照 FK は **2 列の複合 FK** であること。
    ///
    /// 単一列 FK（`components(id)`）はテナント一致を強制しない。RLS の WITH CHECK は
    /// 「自分の tenant_id を書くこと」しか要求しないため、テナント A が
    /// 「tenant_id=A, component_id=（B の cmp_*）」という行を作れてしまう。
    #[test]
    fn m7b_foreign_keys_are_composite() {
        let normalized = squeeze_spaces(M7B_SQL);
        assert!(
            normalized.contains(
                "FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)"
            ),
            "the component reference must be a composite FK so the DB enforces tenant match"
        );
    }

    // ---- 0011_m7c_secrets.sql の DB-free 文字列不変条件 ----------------------

    #[test]
    fn migrator_includes_version_11() {
        let v11 = MIGRATOR
            .iter()
            .find(|m| m.version == 11)
            .expect("migration version 11 (0011_m7c_secrets) must be collected");
        assert!(
            v11.description.contains("m7c") || v11.description.contains("secrets"),
            "unexpected 0011 description: {}",
            v11.description
        );
    }

    #[test]
    fn version_11_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 11),
            "0011 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    #[test]
    fn m7c_tables_are_force_rls() {
        assert_force_rls(M7C_SQL, &["function_secrets", "function_secret_versions"]);
    }

    #[test]
    fn m7c_tenant_isolation_is_fail_closed() {
        assert_fail_closed_isolation(M7C_SQL, &["function_secrets", "function_secret_versions"]);
    }

    #[test]
    fn m7c_tables_have_explicit_grants() {
        assert_explicit_grants(M7C_SQL, &["function_secrets", "function_secret_versions"]);
    }

    /// 版台帳は **追記専用**（暗号文の改竄・消去を faas_app から不可能にする）。
    /// `audit_logs` / `trigger_deliveries` と同型の不変条件。
    #[test]
    fn m7c_secret_versions_is_append_only_for_faas_app() {
        let normalized = squeeze_spaces(M7C_SQL);
        assert!(
            normalized.contains("GRANT SELECT, INSERT ON function_secret_versions TO faas_app"),
            "the version ledger must be granted only SELECT/INSERT"
        );
        assert!(
            normalized.contains("REVOKE UPDATE, DELETE ON function_secret_versions FROM faas_app"),
            "UPDATE/DELETE must be revoked from faas_app on the version ledger"
        );
        for forbidden in [
            "GRANT SELECT, INSERT, UPDATE ON function_secret_versions",
            "GRANT SELECT, INSERT, UPDATE, DELETE ON function_secret_versions",
            "GRANT ALL ON function_secret_versions",
        ] {
            assert!(
                !normalized.contains(forbidden),
                "the version ledger must never be granted {forbidden}"
            );
        }
    }

    /// name の一意性は **生存行のみ**（部分 UNIQUE index）。
    ///
    /// テーブル制約にすると soft delete 後に同名で作り直せず 23505 になる。
    /// 「侵害された資格情報を削除して同名で入れ直す」はインシデント対応の最も基本の操作であり、
    /// これを不可能にしてはならない。
    #[test]
    fn m7c_secret_name_uniqueness_is_soft_delete_aware() {
        let normalized = squeeze_spaces(M7C_SQL);
        assert!(
            normalized.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_function_secrets_live_name \
                 ON function_secrets (tenant_id, component_id, name) WHERE deleted_at IS NULL"
            ) || (normalized.contains("uq_function_secrets_live_name")
                && normalized.contains("WHERE deleted_at IS NULL")),
            "secret name uniqueness must be a partial index over live rows only"
        );
        // コメント行を除いて判定する（本ファイルの設計メモが「テーブル制約にしない理由」を
        // 説明するために同じ字面を含むため）。
        let code_only: String = M7C_SQL
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !squeeze_spaces(&code_only).contains("UNIQUE (tenant_id, component_id, name)"),
            "a table-level UNIQUE would make same-name re-creation after deletion impossible"
        );
    }

    /// HTTP から呼ぶ SECURITY DEFINER 関数は **必ずテナント引数を取る**。
    ///
    /// 本リポジトリの admin は**テナント管理者**であってプラットフォーム管理者ではない。
    /// 全テナント版は `_all` 接尾辞のものだけ（背景ジョブ専用）。
    #[test]
    fn m7c_definer_functions_are_tenant_scoped() {
        assert!(
            M7C_SQL.contains("CREATE FUNCTION secrets_stale_kek(p_tenant text, p_active_kid text)"),
            "the HTTP-facing rekey helper must take a tenant argument"
        );
        assert!(
            M7C_SQL.contains("s.tenant_id = p_tenant"),
            "secrets_stale_kek must filter by the supplied tenant"
        );
        // 全テナント版は _all 接尾辞のものだけ。
        for f in ["secrets_stale_kek_all", "secrets_kek_kid_counts_all"] {
            assert!(
                M7C_SQL.contains(&format!("CREATE FUNCTION {f}(")),
                "{f} must exist as the explicitly-named cross-tenant variant"
            );
        }
        // SECURITY DEFINER 関数はすべて PUBLIC から EXECUTE を剥奪する。
        for f in [
            "secrets_stale_kek(text, text)",
            "secrets_stale_kek_all(text)",
            "secrets_kek_kid_counts_all()",
        ] {
            assert!(
                M7C_SQL.contains(&format!("REVOKE EXECUTE ON FUNCTION {f} FROM PUBLIC")),
                "{f} EXECUTE must be revoked from PUBLIC"
            );
        }
        assert!(
            M7C_SQL.contains("SECURITY DEFINER"),
            "the cross-tenant helpers must be SECURITY DEFINER (faas_app is under FORCE RLS)"
        );
    }

    /// secret 側の FK も 2 列の複合 FK であること（0010 と同じ理由）。
    #[test]
    fn m7c_foreign_keys_are_composite() {
        let normalized = squeeze_spaces(M7C_SQL);
        assert!(
            normalized.contains(
                "FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)"
            ),
            "function_secrets must reference components with a composite FK"
        );
        assert!(
            normalized.contains(
                "FOREIGN KEY (tenant_id, secret_id) REFERENCES function_secrets (tenant_id, id)"
            ),
            "the version ledger must reference function_secrets with a composite FK"
        );
    }

    /// BIGSERIAL を使わない（0004 の ALL SEQUENCES GRANT が新シーケンスに効かないため、
    /// GRANT 漏れという退行を構造的に避ける）。
    #[test]
    fn m7c_uses_no_sequences() {
        for forbidden in ["BIGSERIAL", "SERIAL", "GENERATED"] {
            assert!(
                !M7C_SQL
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("--"))
                    .any(|l| l.to_ascii_uppercase().contains(forbidden)),
                "0011 must not introduce a sequence ({forbidden}); composite PKs avoid GRANT drift"
            );
        }
    }

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
            USAGE_METERING_SQL
                .contains("GRANT  SELECT, INSERT, UPDATE ON usage_rollups TO   faas_app;")
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

    // ---- 0008_m6.sql の DB-free 文字列不変条件 -------------------------------

    // MIGRATOR が 0008 を**コンパイル時収集**していること（sqlx::migrate! の埋め込み検証）。
    #[test]
    fn migrator_includes_version_8() {
        let v8 = MIGRATOR
            .iter()
            .find(|m| m.version == 8)
            .expect("migration version 8 (0008_m6) must be collected by MIGRATOR");
        assert!(
            v8.description.contains("m6"),
            "unexpected 0008 description: {}",
            v8.description
        );
    }

    // 0008 を BASELINE_VERSIONS に入れていないこと（= MIGRATOR.run が通常適用する）。
    #[test]
    fn version_8_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 8),
            "0008 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    // 0008 の 3 表が ENABLE + FORCE RLS であること（0007 と同型の DDL 不変条件）。
    // 桁揃え（空白数）に依存しないよう、行を正規化して `ALTER TABLE {table} ... ROW LEVEL SECURITY` を探す。
    #[test]
    fn m6_tables_are_force_rls() {
        // 連続空白を 1 個に潰した正規化版で部分文字列照合する。
        let normalized: String = {
            let mut s = String::with_capacity(M6_SQL.len());
            let mut prev_space = false;
            for c in M6_SQL.chars() {
                if c == ' ' || c == '\t' {
                    if !prev_space {
                        s.push(' ');
                    }
                    prev_space = true;
                } else {
                    s.push(c);
                    prev_space = false;
                }
            }
            s
        };
        for table in ["cron_jobs", "triggers", "trigger_deliveries"] {
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY")),
                "{table} must FORCE ROW LEVEL SECURITY"
            );
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY")),
                "{table} must ENABLE ROW LEVEL SECURITY"
            );
        }
    }

    // 0008 の tenant_isolation が fail-closed（current_setting('app.tenant_id') 第 2 引数なし）。
    #[test]
    fn m6_tenant_isolation_is_fail_closed() {
        assert!(
            M6_SQL.contains("current_setting('app.tenant_id')"),
            "tenant_isolation must gate on current_setting('app.tenant_id')"
        );
        assert!(
            !M6_SQL.contains("current_setting('app.tenant_id', true)")
                && !M6_SQL.contains("current_setting('app.tenant_id', TRUE)"),
            "tenant_isolation must NOT use the second-arg fallback (must fail closed)"
        );
        // 3 表それぞれに tenant_isolation ポリシーが定義されていること。
        for table in ["cron_jobs", "triggers", "trigger_deliveries"] {
            assert!(
                M6_SQL.contains(&format!("CREATE POLICY tenant_isolation ON {table}")),
                "{table} must have a tenant_isolation policy"
            );
        }
    }

    // trigger_deliveries は SELECT/INSERT のみ付与し、UPDATE/DELETE は付与しない（配送台帳の改竄不能）。
    #[test]
    fn m6_trigger_deliveries_does_not_grant_update_or_delete() {
        // SELECT/INSERT のみが付与される。
        assert!(
            M6_SQL.contains(
                "GRANT  SELECT, INSERT                 ON trigger_deliveries TO   faas_app;"
            ) || M6_SQL.contains("GRANT SELECT, INSERT ON trigger_deliveries TO faas_app;"),
            "trigger_deliveries must be granted only SELECT/INSERT"
        );
        // UPDATE/DELETE は防御的に REVOKE され、trigger_deliveries への DELETE/UPDATE GRANT は無いこと。
        assert!(
            M6_SQL.contains(
                "REVOKE UPDATE, DELETE                 ON trigger_deliveries FROM faas_app;"
            ) || M6_SQL.contains("REVOKE UPDATE, DELETE ON trigger_deliveries FROM faas_app;"),
            "trigger_deliveries must REVOKE UPDATE, DELETE from faas_app"
        );
        assert!(
            !M6_SQL.contains("DELETE ON trigger_deliveries TO")
                && !M6_SQL.contains("DELETE                 ON trigger_deliveries TO"),
            "DELETE must not be granted to faas_app on trigger_deliveries"
        );
    }

    // executions に chain_depth 列が additive（DEFAULT 0, backfill 不要）で追加されること（M6c 暴走防止）。
    #[test]
    fn m6_adds_executions_chain_depth() {
        assert!(
            M6_SQL.contains("ALTER TABLE executions ADD COLUMN IF NOT EXISTS chain_depth")
                && M6_SQL.contains("INTEGER NOT NULL DEFAULT 0"),
            "executions.chain_depth must be added additively with DEFAULT 0"
        );
    }

    // cron_jobs / triggers は CRUD（DELETE を含む）が付与されること（CRUD API が DELETE する）。
    #[test]
    fn m6_cron_and_triggers_grant_crud() {
        for table in ["cron_jobs", "triggers"] {
            assert!(
                M6_SQL.contains(&format!("GRANT  SELECT, INSERT, UPDATE, DELETE ON {table}"))
                    || M6_SQL.contains(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table}")),
                "{table} must be granted SELECT/INSERT/UPDATE/DELETE (CRUD)"
            );
        }
    }

    // 全テナント巡回用 cron_due_tenant_jobs() が SECURITY DEFINER で定義され、EXECUTE が
    // PUBLIC から剥奪され faas_app にのみ付与されること（reaper の認証前参照と同型）。
    #[test]
    fn m6_cron_due_function_is_security_definer() {
        assert!(
            M6_SQL.contains("CREATE FUNCTION cron_due_tenant_jobs()"),
            "cron_due_tenant_jobs() must be defined"
        );
        assert!(
            M6_SQL.contains("SECURITY DEFINER"),
            "cron_due_tenant_jobs() must be SECURITY DEFINER (cross-tenant scan under owner)"
        );
        assert!(
            M6_SQL.contains("REVOKE EXECUTE ON FUNCTION cron_due_tenant_jobs() FROM PUBLIC;"),
            "cron_due_tenant_jobs() EXECUTE must be revoked from PUBLIC"
        );
        assert!(
            M6_SQL.contains("GRANT  EXECUTE ON FUNCTION cron_due_tenant_jobs() TO   faas_app;")
                || M6_SQL.contains("GRANT EXECUTE ON FUNCTION cron_due_tenant_jobs() TO faas_app;"),
            "cron_due_tenant_jobs() EXECUTE must be granted to faas_app"
        );
    }

    // ---- 0009_m7a_traffic_split.sql の DB-free 文字列不変条件 -----------------

    // MIGRATOR が 0009 を**コンパイル時収集**していること（sqlx::migrate! の埋め込み検証）。
    #[test]
    fn migrator_includes_version_9() {
        let v9 = MIGRATOR
            .iter()
            .find(|m| m.version == 9)
            .expect("migration version 9 (0009_m7a_traffic_split) must be collected by MIGRATOR");
        assert!(
            v9.description.contains("m7a") || v9.description.contains("traffic"),
            "unexpected 0009 description: {}",
            v9.description
        );
    }

    // 0009 を BASELINE_VERSIONS に入れていないこと（= MIGRATOR.run が通常適用する）。
    #[test]
    fn version_9_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 9),
            "0009 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    // M7a は**新規テーブルを 1 つも作らない**（GRANT 漏れ / RLS ポリシー漏れという最大の退行リスクを
    // 設計段階で消したことの回帰ガード）。新表を足したくなったら 0010 以降で作り、RLS + GRANT を
    // 明示すること。
    #[test]
    fn m7a_creates_no_new_table() {
        assert!(
            !M7A_SQL.contains("CREATE TABLE"),
            "0009 must not create any table (canary state lives on components; \
             new tables belong in 0010+ with explicit RLS and GRANT)"
        );
    }

    // 同じ理由で、権限 / ポリシー DDL も 0009 には現れない（既存 components / executions の
    // FORCE RLS + tenant_isolation + GRANT をそのまま継承する）。
    #[test]
    fn m7a_grants_nothing_new() {
        for forbidden in ["GRANT", "REVOKE", "CREATE POLICY", "ROW LEVEL SECURITY"] {
            assert!(
                !M7A_SQL
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("--"))
                    .any(|l| l.contains(forbidden)),
                "0009 must not contain {forbidden} outside comments \
                 (it adds columns to already-protected tables)"
            );
        }
    }

    // canary 4 列が additive（IF NOT EXISTS・nullable か NOT NULL DEFAULT）で足されること。
    #[test]
    fn m7a_columns_are_additive() {
        for col in [
            "canary_version_id",
            "canary_weight",
            "previous_active_version_id",
            "canary_updated_at",
        ] {
            assert!(
                M7A_SQL.contains(&format!("ADD COLUMN IF NOT EXISTS {col}")),
                "components.{col} must be added with ADD COLUMN IF NOT EXISTS"
            );
        }
        // canary_weight だけは NOT NULL。既定 0 ＝ M6 までと同一の解決（全量 stable）。
        assert!(
            M7A_SQL.contains("ADD COLUMN IF NOT EXISTS canary_weight SMALLINT NOT NULL DEFAULT 0"),
            "canary_weight must default to 0 so that an un-configured component routes 100% stable"
        );
        // M7b/M7c の複合 FK の被参照側。
        assert!(
            M7A_SQL.contains("components_tenant_id_id_key UNIQUE (tenant_id, id)"),
            "components must expose UNIQUE (tenant_id, id) for the composite FKs in 0010/0011"
        );
    }

    // 「配分先の無い重み」と値域外を DB で不可能にする 2 本の CHECK。
    #[test]
    fn m7a_canary_weight_is_range_checked() {
        assert!(
            M7A_SQL.contains("CHECK (canary_weight >= 0 AND canary_weight <= 100)"),
            "canary_weight must be range-checked in the DB (0..=100)"
        );
        assert!(
            M7A_SQL.contains("CHECK (canary_weight = 0 OR canary_version_id IS NOT NULL)"),
            "a non-zero weight must be impossible without a canary target"
        );
    }

    // executions は最大テーブル。ADD COLUMN のインライン CHECK（既存全行の検証走査を誘発する）を
    // 書かず、値域は NOT VALID 制約で前方だけ守る（ロック窓の最小化。VALIDATE は保守窓で手動）。
    #[test]
    fn m7a_executions_check_is_not_valid() {
        assert!(
            M7A_SQL
                .contains("ADD COLUMN IF NOT EXISTS routing_reason TEXT NOT NULL DEFAULT 'stable'"),
            "executions.routing_reason must be additive with DEFAULT 'stable'"
        );
        assert!(
            M7A_SQL.contains("CHECK (routing_reason IN ('stable', 'canary')) NOT VALID"),
            "the routing_reason CHECK must be added NOT VALID (no full-table verification scan)"
        );
        // インライン CHECK（ADD COLUMN ... CHECK ...）になっていないこと。
        let add_column_line = M7A_SQL
            .lines()
            .find(|l| l.contains("ADD COLUMN IF NOT EXISTS routing_reason"))
            .expect("routing_reason ADD COLUMN line must exist");
        assert!(
            !add_column_line.contains("CHECK"),
            "routing_reason must not carry an inline CHECK (it would scan every existing row)"
        );
    }
}
