//! PostgreSQL-backed HTTP acceptance, provenance and accounting regression.
use crate::{db, direct_http, state::AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use hibana_shared::{ExecutionStatus, JobClaims, ResultMessage, UsageMetrics};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, ExecResult, Statement, TransactionTrait,
    TryGetable,
};
use serde_json::json;
use std::sync::Arc;
#[path = "http_tests/retention.rs"]
mod retention;
#[path = "http_tests/tail.rs"]
mod tail;
fn state(pool: DatabaseConnection, store: Arc<dyn crate::store::Store>) -> AppState {
    let cfg = crate::config::Config::from_env().unwrap();
    let storage = crate::storage::Storage::new(
        &cfg.s3_endpoint,
        &cfg.s3_region,
        &cfg.s3_bucket,
        &cfg.s3_access_key,
        cfg.s3_secret_key_plain(),
    );
    AppState::new(
        pool,
        storage,
        cfg.max_wasm_upload_bytes,
        cfg.presign_ttl_secs,
        "test-only".into(),
        Arc::new(crate::signing::Signer::from_seed(
            crate::signing::decode_seed(cfg.job_signing_key_plain()).unwrap(),
            cfg.job_signing_kid.clone(),
        )),
        cfg.token_margin_secs,
        store,
        cfg.admission(),
        crate::metrics::Metrics::init(),
        Arc::new(cfg.secret_keyring().unwrap()),
        cfg.job_env_exchange_rate_per_min,
        false,
        cfg.public_apps.clone(),
        cfg.auth.clone(),
    )
}

fn token(state: &AppState, id: &str, tenant: &str, version: &str) -> String {
    let iat = chrono::Utc::now().timestamp();
    state.signer().sign(&JobClaims {
        execution_id: id.into(),
        tenant_id: tenant.into(),
        version_id: version.into(),
        kid: state.signer().kid().into(),
        iat,
        exp: iat + 300,
    })
}
fn headers(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-hibana-job-token", token.parse().unwrap());
    h
}
async fn scalar(owner: &DatabaseConnection, query: &str) -> i64 {
    fixture_scalar(owner, query, vec![]).await.unwrap()
}

async fn inflight(pool: &DatabaseConnection, tenant: &str) -> i64 {
    let tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&tx, tenant).await.unwrap();
    let count = db::count_inflight_executions(&tx, tenant).await.unwrap();
    tx.commit().await.unwrap();
    count
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; bash scripts/test-http.sh"]
async fn http_mvp_regression() {
    let url = std::env::var("HTTP_TEST_DATABASE_URL").unwrap();
    assert!(
        url.ends_with("/hibana_http"),
        "refusing a non-test database"
    );
    let owner = hibana_database::postgres::connect(&url, 10, 0)
        .await
        .unwrap();
    assert!(
        fixture_scalar::<bool>(
            &owner,
            "SELECT to_regclass('public.tenants') IS NULL",
            vec![]
        )
        .await
        .unwrap(),
        "refusing a nonempty database"
    );
    assert_legacy_database_rejected(&owner).await;
    migrate_before_oidc_cutover(&owner).await;
    fixture_execute(&owner, r#"
        INSERT INTO tenants (id,slug,name,status,quotas) VALUES ('http','http','HTTP','active','{"max_concurrent_executions":1}'),('other','other','Other','active','{}');
        INSERT INTO components (id,tenant_id,name,ingress_enabled) VALUES ('source','http','source',true),('target','http','target',true);
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256,status)
            VALUES ('source-v1','http','source','1','source/1.wasm','abcd','active'),
                   ('source-v2','http','source','2','source/2.wasm','efab','active'),
                   ('target-v1','http','target','1','target/1.wasm','abcd','active');
        UPDATE components SET active_version_id=id||'-v1';
    "#, vec![]).await.unwrap();
    assert_build_metadata_upgrade(&owner).await;
    assert_execution_input_upgrade(&owner).await;
    assert_secret_key_retention_upgrade(&owner).await;
    assert_oidc_cutover(&owner).await;
    assert_fresh_schema(&owner).await;
    assert_compound_identities(&owner).await;
    let pool = hibana_database::postgres::connect(&std::env::var("DATABASE_URL").unwrap(), 10, 0)
        .await
        .unwrap();
    crate::migrations::assert_non_privileged_runtime_role(&pool)
        .await
        .unwrap();
    hibana_database::postgres::assert_runtime_schema(&pool)
        .await
        .unwrap();
    assert_tenant_context_is_transaction_local().await;
    assert_inflight_queries(&owner, &pool).await;
    assert_usage_boundaries(&pool).await;
    let store = Arc::new(crate::store::InProcStore::new());
    let state = state(pool.clone(), store.clone());

    let unavailable = self::state(pool.clone(), Arc::new(crate::store::FailingStore));
    assert_eq!(
        direct_http::accept(
            &unavailable,
            "http",
            "source",
            std::future::ready(Ok(json!({})))
        )
        .await
        .unwrap()
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions").await, 0);
    println!("PASS shared store failure rejects HTTP without accepting an execution");

    fixture_execute(
        &owner,
        "ALTER TABLE executions ADD CONSTRAINT reject_http_test CHECK(false) NOT VALID",
        vec![],
    )
    .await
    .unwrap();
    assert!(
        direct_http::accept(&state, "http", "source", std::future::ready(Ok(json!({}))))
            .await
            .is_err()
    );
    assert_eq!(inflight(&pool, "http").await, 0);
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions").await, 0);
    fixture_execute(
        &owner,
        "ALTER TABLE executions DROP CONSTRAINT reject_http_test",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS failed acceptance rolls back the row and releases the reservation");

    // An unreachable Worker must yield an error, never queue or replay the HTTP request.
    let response = direct_http::accept(
        &state,
        "http",
        "source",
        std::future::ready(Ok(json!({"method":"GET","path":"/"}))),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        scalar(
            &owner,
            "SELECT count(*) FROM executions WHERE http_request AND version_id='source-v1'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&owner, "SELECT count(*) FROM information_schema.tables WHERE table_schema='public' AND table_name='execution_outbox'").await,
        0
    );
    assert_eq!(inflight(&pool, "http").await, 0);
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions WHERE status='failed' AND error->>'code'='dispatch_not_started'").await, 1);
    assert_eq!(
        scalar(
            &owner,
            "SELECT count(*) FROM executions WHERE input IS NOT NULL OR input_ref IS NOT NULL"
        )
        .await,
        0
    );
    let rejected: String = fixture_scalar(&owner, "SELECT id FROM executions", vec![])
        .await
        .unwrap();
    assert!(!direct_http::cancel_pending(&state, "http", &rejected)
        .await
        .unwrap());
    assert_eq!(inflight(&pool, "http").await, 0);
    assert_eq!(
        scalar(&owner, "SELECT count(*) FROM usage_rollups").await,
        0
    );
    // Reuse the pinned fixture to exercise pending admission/provenance below.
    // A late Worker cannot claim a cancelled record in normal operation.
    assert_eq!(
        fixture_execute(
            &owner,
            "UPDATE executions SET status='running' WHERE id=$1 AND status='pending'",
            vec![rejected.clone().into()]
        )
        .await
        .unwrap()
        .rows_affected(),
        0
    );
    fixture_execute(
        &owner,
        "UPDATE executions SET status='pending',finished_at=NULL,error=NULL,input='{\"method\":\"GET\",\"path\":\"/\"}' WHERE id=$1",
        vec![rejected.clone().into()],
    )
    .await
    .unwrap();
    println!("PASS unavailable Worker releases reservation once without invocation usage; late claim fails");
    assert_eq!(
        direct_http::accept(&state, "http", "source", std::future::ready(Ok(json!({}))))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // Separate Control Plane instances must observe the same DB limit.
    let peer = self::state(pool.clone(), store.clone());
    let (a, b) = tokio::join!(
        direct_http::accept(&state, "http", "source", std::future::ready(Ok(json!({})))),
        direct_http::accept(&peer, "http", "source", std::future::ready(Ok(json!({}))))
    );
    assert_eq!(a.unwrap().status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(b.unwrap().status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        scalar(&owner, "SELECT count(*) FROM executions WHERE http_request").await,
        1
    );
    println!("PASS HTTP admission / no outbox / active version only / DB concurrency across Control Planes");

    let id: String = fixture_scalar(&owner, "SELECT id FROM executions", vec![])
        .await
        .unwrap();
    let valid = token(&state, &id, "http", "source-v1");
    let job = direct_http::redeem(State(state.clone()), headers(&valid))
        .await
        .unwrap()
        .0;
    assert_eq!(
        state.signer().verify(&job.job_token).unwrap().version_id,
        "source-v1"
    );
    assert_eq!(job.input["path"], "/");
    for invalid in [
        token(&state, &id, "other", "source-v1"),
        token(&state, &id, "http", "target-v1"),
        format!("{valid}tampered"),
    ] {
        assert!(direct_http::redeem(State(state.clone()), headers(&invalid))
            .await
            .is_err());
    }
    // Retained historical byte/async records cannot be redeemed, even with a valid signature.
    fixture_execute(
        &owner,
        "UPDATE executions SET http_request=false WHERE id=$1",
        vec![id.clone().into()],
    )
    .await
    .unwrap();
    assert!(direct_http::redeem(State(state.clone()), headers(&valid))
        .await
        .is_err());
    fixture_execute(
        &owner,
        "UPDATE executions SET http_request=true WHERE id=$1",
        vec![id.clone().into()],
    )
    .await
    .unwrap();
    let tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&tx, "other").await.unwrap();
    assert_eq!(
        fixture_scalar::<i64>(&tx, "SELECT count(*) FROM executions", vec![])
            .await
            .unwrap(),
        0
    );
    assert!(
        fixture_execute(
            &tx,
            "UPDATE executions SET tenant_id='other' WHERE id=$1",
            vec![id.clone().into()]
        )
        .await
        .unwrap()
        .rows_affected()
            == 0
    );
    tx.commit().await.unwrap();
    println!("PASS signed redemption / version pin / historical job rejection / FORCE RLS");

    // Once claimed, a transport failure must not release or rewrite the execution.
    fixture_execute(
        &owner,
        "UPDATE executions SET status='running',started_at=now() WHERE id=$1 AND status='pending'",
        vec![id.clone().into()],
    )
    .await
    .unwrap();
    assert!(!direct_http::cancel_pending(&state, "http", &id)
        .await
        .unwrap());
    assert_eq!(inflight(&pool, "http").await, 1);
    let mut result = ResultMessage {
        logs: None,
        execution_id: id.clone(),
        tenant_id: "http".into(),
        status: ExecutionStatus::Succeeded,
        output: Some(json!({"status":200,"streamed":true})),
        error: None,
        job_token: job.job_token.clone(),
        usage: Some(UsageMetrics {
            cpu_fuel_used: u64::MAX,
            wall_time_ms: u64::MAX,
            peak_memory_bytes: u64::MAX,
            ..Default::default()
        }),
    };
    fixture_execute(
        &owner,
        "UPDATE platform_maintenance SET owner='regression' WHERE singleton",
        vec![],
    )
    .await
    .unwrap();
    assert!(crate::maintenance::enabled(&pool).await.unwrap());
    assert!(
        fixture_execute(
            &pool,
            "UPDATE platform_maintenance SET owner=NULL WHERE singleton",
            vec![]
        )
        .await
        .is_err(),
        "runtime role cannot reopen admission"
    );
    // Suspending admission must still allow the in-flight result to settle,
    // even if a transport delay has taken it beyond its admission token expiry.
    fixture_execute(
        &owner,
        "UPDATE tenants SET status='suspended' WHERE id='http'",
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        direct_http::redeem(State(state.clone()), headers(&job.job_token))
            .await
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut expired = state.signer().verify(&job.job_token).unwrap();
    expired.exp = chrono::Utc::now().timestamp() - 1;
    result.job_token = state.signer().sign(&expired);
    assert_eq!(
        direct_http::redeem(State(state.clone()), headers(&result.job_token))
            .await
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for bad_token in [
        "invalid".to_owned(),
        token(&state, &id, "http", "source-v2"),
    ] {
        let mut forged = result.clone();
        forged.job_token = bad_token.clone();
        assert_eq!(
            crate::completion::complete(State(state.clone()), headers(&bad_token), Json(forged))
                .await
                .unwrap_err()
                .into_response()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        crate::completion::complete(
            State(state.clone()),
            headers(&job.job_token),
            Json(result.clone())
        )
        .await
        .unwrap_err()
        .into_response()
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut forged = result.clone();
    forged.tenant_id = "other".into();
    assert!(crate::completion::complete(
        State(state.clone()),
        headers(&result.job_token),
        Json(forged)
    )
    .await
    .is_err());
    let mut nonterminal = result.clone();
    nonterminal.status = ExecutionStatus::Running;
    assert!(crate::completion::complete(
        State(state.clone()),
        headers(&result.job_token),
        Json(nonterminal)
    )
    .await
    .is_err());
    for _ in 0..2 {
        assert_eq!(
            crate::completion::complete(
                State(state.clone()),
                headers(&result.job_token),
                Json(result.clone())
            )
            .await
            .unwrap(),
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(inflight(&pool, "http").await, 0);
    assert_eq!(
        scalar(
            &owner,
            "SELECT sum(cpu_fuel_used)::bigint FROM usage_rollups"
        )
        .await,
        0,
        "a Worker cannot report fuel use for an unmetered version"
    );
    assert_eq!(
        scalar(
            &owner,
            "SELECT count(*) FROM executions WHERE http_request AND input IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &owner,
            "SELECT sum(invocation_count)::bigint FROM usage_rollups"
        )
        .await,
        1
    );
    assert!(
        scalar(
            &owner,
            "SELECT wall_time_ms FROM executions WHERE http_request"
        )
        .await
            < 100_000
    );
    assert!(direct_http::redeem(State(state.clone()), headers(&valid))
        .await
        .is_err());
    println!("PASS completion provenance / terminal CAS / bounded usage / one reservation release");
    fixture_execute(
        &owner,
        "UPDATE platform_maintenance SET owner=NULL WHERE singleton",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS durable completion remains available with admission closed; runtime role cannot change the gate");
    fixture_execute(
        &owner,
        "UPDATE tenants SET status='active' WHERE id='http'",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS suspended/expired completion persists once; expired or suspended admission and forged results remain denied");

    for attempt in 0..16 {
        let raced = format!("dispatch-race-{attempt}");
        fixture_execute(&owner, "INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,http_request) VALUES ($1,'http','source','source-v1','pending','{}',true)", vec![raced.clone().into()]).await.unwrap();
        let claim = async {
            let tx = pool.begin().await.unwrap();
            db::set_tenant_guc(&tx, "http").await.unwrap();
            let changed = fixture_execute(&tx, "UPDATE executions SET status='running',started_at=now() WHERE tenant_id='http' AND id=$1 AND status='pending' AND http_request", vec![raced.clone().into()]).await.unwrap().rows_affected() == 1;
            tx.commit().await.unwrap();
            changed
        };
        let (claimed, cancelled) =
            tokio::join!(claim, direct_http::cancel_pending(&state, "http", &raced));
        let cancelled = cancelled.unwrap();
        assert_ne!(claimed, cancelled, "exactly one transition must win");
        assert_eq!(inflight(&pool, "http").await, i64::from(claimed));
        assert!(!direct_http::cancel_pending(&state, "http", &raced)
            .await
            .unwrap());
        // Finish the synthetic claim without executing a guest; preserve no test reservations.
        fixture_execute(&owner, "UPDATE executions SET status='failed',finished_at=now() WHERE id=$1 AND status='running'", vec![raced.clone().into()]).await.unwrap();
    }
    println!(
        "PASS concurrent Worker claim / dispatch cancellation: one winner, one reservation release"
    );

    let tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&tx, "http").await.unwrap();
    assert!(db::lock_component(&tx, "http", "source").await.unwrap());
    assert!(
        db::switch_active_version(&tx, "http", "source", "source-v2")
            .await
            .unwrap()
    );
    assert!(
        db::switch_active_version(&tx, "http", "source", "source-v2")
            .await
            .unwrap()
    );
    assert_eq!(
        db::rollback_active_version(&tx, "http", "source", None)
            .await
            .unwrap()
            .unwrap()
            .0,
        "source-v1"
    );
    assert!(
        db::rollback_active_version(&tx, "http", "source", Some("target-v1"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !db::switch_active_version(&tx, "http", "source", "target-v1")
            .await
            .unwrap(),
        "activation must reject another component's version"
    );
    fixture_execute(&tx, "SAVEPOINT deleted_target", vec![])
        .await
        .unwrap();
    db::soft_delete_version(&tx, "http", "source", "source-v2")
        .await
        .unwrap();
    assert!(
        !db::switch_active_version(&tx, "http", "source", "source-v2")
            .await
            .unwrap(),
        "publication must recheck the target after preparation"
    );
    assert_eq!(
        db::find_component_by_id(&tx, "http", "source")
            .await
            .unwrap()
            .unwrap()
            .active_version_id
            .as_deref(),
        Some("source-v1")
    );
    fixture_execute(&tx, "ROLLBACK TO SAVEPOINT deleted_target", vec![])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    println!("PASS version switch / no-op preserves previous / publication rejects deleted and foreign targets");
    secret_resolution_regression(&state).await;

    fixture_execute(
        &owner,
        "UPDATE tenants SET status='suspended' WHERE id='http'",
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        direct_http::redeem(State(state.clone()), headers(&valid))
            .await
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        direct_http::accept(&state, "http", "source", std::future::ready(Ok(json!({}))))
            .await
            .is_err()
    );
    println!("PASS suspended tenant cannot accept or redeem HTTP requests");

    use crate::handlers::components::{admin_delete_component, admin_list_components};
    use axum::extract::Path;
    let mut admin = HeaderMap::new();
    admin.insert("authorization", "Bearer test-only".parse().unwrap());
    assert_eq!(
        admin_list_components(State(state.clone()), HeaderMap::new())
            .await
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    fixture_execute(&owner, "INSERT INTO components (id,tenant_id,name) VALUES ('delete-test','other','reusable'); INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256) VALUES ('delete-v1','other','delete-test','1','delete.wasm','abcd'); INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request) VALUES ('delete-pending','other','delete-test','delete-v1','pending',true);", vec![]).await.unwrap();
    let Json(inventory) = admin_list_components(State(state.clone()), admin.clone())
        .await
        .unwrap();
    assert!(
        inventory.iter().any(|c| c["tenant_id"] == "http"),
        "suspended tenants remain visible to the platform administrator"
    );
    assert!(inventory.iter().any(|c| c["component_id"] == "delete-test"));
    let delete_path = || Path(("other".to_string(), "delete-test".to_string()));
    assert_eq!(
        admin_delete_component(State(state.clone()), HeaderMap::new(), delete_path())
            .await
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        admin_delete_component(
            State(state.clone()),
            admin.clone(),
            Path(("http".into(), "delete-test".into()))
        )
        .await
        .unwrap_err()
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        admin_delete_component(State(state.clone()), admin.clone(), delete_path())
            .await
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );
    fixture_execute(
        &owner,
        "UPDATE executions SET status='failed',finished_at=now() WHERE id='delete-pending'",
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(
        admin_delete_component(State(state.clone()), admin.clone(), delete_path())
            .await
            .unwrap(),
        StatusCode::NO_CONTENT
    );
    let Json(inventory) = admin_list_components(State(state.clone()), admin)
        .await
        .unwrap();
    assert!(!inventory.iter().any(|c| c["component_id"] == "delete-test"));
    assert_eq!(
        scalar(
            &owner,
            "SELECT count(*) FROM components WHERE id='delete-test' AND deleted_at IS NOT NULL"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &owner,
            "SELECT count(*) FROM executions WHERE component_id='delete-test'"
        )
        .await,
        1
    );
    let tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&tx, "other").await.unwrap();
    db::create_component(&tx, "other", "delete-recreated", "reusable")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    console_information_regression(&state).await;
    direct_http_dispatch_regression(&owner, &pool).await;
    completion_diagnostic_regression(&owner, &state).await;
    application_logs_regression(&owner, &state).await;
    retention::regression(&owner, &state).await;
    tail::regression(&owner, &pool).await;
    completion_concurrency_regression(&owner, &state).await;
    // Keep identities used by the subsequent cross-tenant RLS checks, but do
    // not ask the real fleet to prepare these fake, unstored Wasm versions.
    fixture_execute(
        &owner,
        "UPDATE components SET active_version_id = NULL",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS admin deletion: authentication / RLS / active execution guard / suspended tenant inventory / name reuse / retained history");
}

async fn completion_diagnostic_regression(owner: &DatabaseConnection, state: &AppState) {
    use hibana_database::prelude::*;

    fixture_execute(owner, r#"
        INSERT INTO tenants (id,slug,name,status) VALUES ('diagnostics','diagnostics','Diagnostics','active');
        INSERT INTO components (id,tenant_id,name) VALUES ('diagnostics-app','diagnostics','app');
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256)
            VALUES ('diagnostics-v1','diagnostics','diagnostics-app','1','unused','abcd');
    "#, vec![]).await.unwrap();

    // Wasmtime preserves NUL in debug names when formatting a trap backtrace.
    // Exercise the signed completion handler and JSONB storage, not just JSON encoding.
    let long_error = "雪".repeat(32 * 1024);
    let marker = "\n[diagnostic truncated]";
    let truncated_error = format!(
        "{}{marker}",
        "雪".repeat((hibana_shared::diagnostics::MAX_DIAGNOSTIC_BYTES - marker.len()) / "雪".len())
    );
    let cases = [
        (
            Some("wasm trap: error while executing at wasm backtrace:\n    0: module\0fixture!func\0fixture"),
            Some("wasm trap: error while executing at wasm backtrace:\n    0: module\\u0000fixture!func\\u0000fixture"),
        ),
        (Some("\0雪\0\0"), Some("\\u0000雪\\u0000\\u0000")),
        (Some("失敗\n\t原因\r\n"), Some("失敗\n\t原因\r\n")),
        (Some(r"literal \u0000"), Some(r"literal \u0000")),
        (Some(long_error.as_str()), Some(truncated_error.as_str())),
        (Some(""), Some("")),
        (None, None),
    ];
    for (index, (message, expected)) in cases.into_iter().enumerate() {
        let id = format!("diagnostics-{index}");
        fixture_execute(owner,
            "INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,input_ref,http_request,started_at) VALUES ($1,'diagnostics','diagnostics-app','diagnostics-v1','running','{}','fixture-input',true,now())",
            vec![id.clone().into()]).await.unwrap();
        assert_eq!(inflight(state.pool(), "diagnostics").await, 1);
        let result = ResultMessage {
            logs: Some(hibana_shared::application_logs::ApplicationLogs {
                stdout: "\0雪".repeat(20_000),
                stderr: "fixture stderr".into(),
                truncated: false,
            }),
            execution_id: id.clone(),
            tenant_id: "diagnostics".into(),
            status: ExecutionStatus::Failed,
            output: None,
            error: message.map(str::to_owned),
            job_token: token(state, &id, "diagnostics", "diagnostics-v1"),
            usage: Some(UsageMetrics::default()),
        };
        for _ in 0..2 {
            assert_eq!(
                crate::completion::complete(
                    State(state.clone()),
                    headers(&result.job_token),
                    Json(result.clone()),
                )
                .await
                .unwrap(),
                StatusCode::NO_CONTENT,
            );
            assert_eq!(inflight(state.pool(), "diagnostics").await, 0);
        }
        let tx = state.pool().begin().await.unwrap();
        db::set_tenant_guc(&tx, "diagnostics").await.unwrap();
        let saved = executions::Entity::find_by_id(&id)
            .one(&tx)
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(saved.status, "failed");
        let expected_logs = result.logs.as_ref().unwrap().bounded();
        assert_eq!(saved.application_logs, Some(json!(expected_logs)));
        let mut retry = result.clone();
        retry.logs = Some(Default::default());
        crate::completion::complete(State(state.clone()), headers(&retry.job_token), Json(retry))
            .await
            .unwrap();
        let tx = state.pool().begin().await.unwrap();
        db::set_tenant_guc(&tx, "diagnostics").await.unwrap();
        let after = executions::Entity::find_by_id(&id)
            .one(&tx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.application_logs, saved.application_logs);
        assert_eq!(after.finished_at, saved.finished_at);
        tx.commit().await.unwrap();
        assert_eq!(
            saved.error,
            expected.map(|message| json!({ "message": message }))
        );
        assert!(saved.finished_at.is_some());
        assert!(saved.input.is_none());
        assert!(saved.input_ref.is_none());
        for field in ["invocation_count", "failed_count"] {
            assert_eq!(
                scalar(owner, &format!("SELECT sum({field})::bigint FROM usage_rollups WHERE tenant_id='diagnostics'")).await,
                (index + 1) as i64,
                "completion retries must not double-count usage",
            );
        }
    }
    println!("PASS completion diagnostics: bounded UTF-8 / NUL-safe JSONB / unchanged text / terminal state / input cleanup / immediate slot release / idempotent usage");
}

async fn application_logs_regression(owner: &DatabaseConnection, state: &AppState) {
    use crate::handlers::executions::{get_execution, list_executions, list_logs, ExecutionsQuery};
    use axum::extract::{Path, Query};
    let principal = |tenant: &str| crate::auth::Principal {
        tenant_id: tenant.into(),
        user_id: None,
        token_id: "fixture".into(),
        scopes: vec![hibana_shared::Scope::Read],
        role: hibana_shared::Role::Member,
    };
    async fn body(response: impl IntoResponse) -> serde_json::Value {
        let response = response.into_response();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    fixture_execute(owner, r#"
        INSERT INTO tenants (id,slug,name,status) VALUES ('logs','logs','Logs','active');
        INSERT INTO components (id,tenant_id,name) VALUES ('logs-app','logs','app');
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256)
            VALUES ('logs-v1','logs','logs-app','1','unused','abcd');
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at,application_logs)
            SELECT 'logs-'||lpad(i::text,3,'0'),'logs','logs-app','logs-v1','succeeded',true,now(),now(),
                '{"stdout":"tenant-private 雪","stderr":"","truncated":false}'::jsonb FROM generate_series(1,26) AS i;
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,finished_at,application_logs)
            SELECT 'expired-'||i,'logs','logs-app','logs-v1','failed',true,now()-interval '25 hours',
                '{"stdout":"expired private output","stderr":"","truncated":false}'::jsonb FROM generate_series(1,501) AS i;
    "#, vec![]).await.unwrap();
    // Expiry is enforced before cleanup. Old runtimes and pending executions also have no logs.
    let expired = body(
        get_execution(
            State(state.clone()),
            principal("logs"),
            Path("expired-1".into()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(expired["logs"].is_null());
    let detail = body(
        get_execution(
            State(state.clone()),
            principal("logs"),
            Path("logs-001".into()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(detail["logs"]["stdout"], "tenant-private 雪");
    assert_eq!(detail["version_id"], "logs-v1");
    for key in ["input", "output", "input_ref", "output_ref"] {
        assert!(detail.get(key).is_none());
    }
    assert_eq!(
        get_execution(
            State(state.clone()),
            principal("other"),
            Path("logs-001".into())
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        list_logs(
            State(state.clone()),
            principal("other"),
            Path("logs-app".into()),
            Query(ExecutionsQuery::default())
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );

    let page = body(
        list_logs(
            State(state.clone()),
            principal("logs"),
            Path("logs-app".into()),
            Query(ExecutionsQuery::default()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 20);
    // Cursor ordering must also work when creation timestamps are identical.
    let next = body(
        list_logs(
            State(state.clone()),
            principal("logs"),
            Path("logs-app".into()),
            Query(ExecutionsQuery {
                before: Some(page["next_cursor"].as_str().unwrap().into()),
                errors_only: false,
            }),
        )
        .await
        .unwrap(),
    )
    .await;
    let ids: std::collections::HashSet<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["execution_id"].as_str().unwrap())
        .collect();
    assert!(next["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| !ids.contains(r["execution_id"].as_str().unwrap())));
    // The small history endpoint must not automatically download application output.
    let history = body(
        list_executions(
            State(state.clone()),
            principal("logs"),
            Path("logs-app".into()),
            Query(ExecutionsQuery::default()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(history["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.get("logs").is_none()));
    assert!(list_logs(
        State(state.clone()),
        principal("logs"),
        Path("logs-app".into()),
        Query(ExecutionsQuery {
            before: Some("invalid".into()),
            errors_only: false
        })
    )
    .await
    .is_err());

    // Explicit tenant predicates and RLS both apply, including the cleanup worker.
    let tx = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&tx, "other").await.unwrap();
    assert_eq!(
        db::purge_expired_application_logs(&tx, "logs")
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
    let tx = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&tx, "logs").await.unwrap();
    assert_eq!(
        db::purge_expired_application_logs(&tx, "logs")
            .await
            .unwrap(),
        500
    );
    tx.commit().await.unwrap();
    fixture_execute(
        owner,
        "UPDATE tenants SET status='suspended' WHERE id='logs'",
        vec![],
    )
    .await
    .unwrap();
    fixture_execute(
        owner,
        "UPDATE executions SET application_logs='{\"stdout\":\"expired\"}' WHERE tenant_id='logs' AND id LIKE 'expired-%'; UPDATE executions SET input_ref='legacy-fixture' WHERE id='expired-501'",
        vec![],
    )
    .await
    .unwrap();
    crate::reaper::reconcile_once(state, 0, crate::config::DEFAULT_EXECUTION_RETENTION_DAYS)
        .await
        .unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE tenant_id='logs' AND input_ref IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE tenant_id='logs' AND application_logs IS NOT NULL").await, 26);
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE tenant_id='logs'"
        )
        .await,
        527
    );
    println!("PASS application logs: tenant isolation / expiry before cleanup / bounded cleanup / suspended tenants / stable pagination / unchanged execution history");
}

async fn completion_concurrency_regression(owner: &DatabaseConnection, state: &AppState) {
    fixture_execute(owner, r#"
        INSERT INTO tenants (id,slug,name,status) VALUES ('completion','completion','Completion','active');
        INSERT INTO components (id,tenant_id,name) VALUES ('completion-app','completion','app');
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256)
            VALUES ('completion-v1','completion','completion-app','1','unused','abcd');
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,http_request)
            SELECT id,'completion','completion-app','completion-v1','running','{}',true
            FROM unnest(ARRAY['completion-duplicate','completion-conflict','completion-rollback']) AS id;
    "#, vec![]).await.unwrap();
    let result = |id: &str, status| ResultMessage {
        logs: None,
        execution_id: id.into(),
        tenant_id: "completion".into(),
        status,
        output: None,
        error: None,
        job_token: token(state, id, "completion", "completion-v1"),
        usage: Some(UsageMetrics::default()),
    };
    let complete = |result: ResultMessage| async move {
        crate::completion::complete(
            State(state.clone()),
            headers(&result.job_token),
            Json(result),
        )
        .await
        .unwrap_or_else(|error| error.into_response().status())
    };
    for (id, second_status) in [
        ("completion-duplicate", ExecutionStatus::Succeeded),
        ("completion-conflict", ExecutionStatus::Failed),
    ] {
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                complete(result(id, ExecutionStatus::Succeeded)),
                complete(result(id, second_status)),
            )
        })
        .await
        .expect("concurrent completions must not deadlock");
        if second_status == ExecutionStatus::Succeeded {
            assert_eq!(
                (first, second),
                (StatusCode::NO_CONTENT, StatusCode::NO_CONTENT)
            );
        } else {
            assert!(matches!(
                (first, second),
                (StatusCode::NO_CONTENT, StatusCode::CONFLICT)
                    | (StatusCode::CONFLICT, StatusCode::NO_CONTENT)
            ));
        }
    }
    assert_eq!(
        scalar(
            owner,
            "SELECT sum(invocation_count)::bigint FROM usage_rollups WHERE tenant_id='completion'"
        )
        .await,
        2
    );
    assert_eq!(inflight(state.pool(), "completion").await, 1);

    // Fail at COMMIT, after finalization and the rollup both succeeded. Neither
    // the execution nor metrics may record a transition that was rolled back.
    fixture_execute(
        owner,
        r#"
        CREATE FUNCTION reject_completion_commit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'fixture commit failure'; END $$;
        CREATE CONSTRAINT TRIGGER reject_completion_commit AFTER UPDATE ON executions
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
            WHEN (NEW.id = 'completion-rollback') EXECUTE FUNCTION reject_completion_commit();
    "#,
        vec![],
    )
    .await
    .unwrap();
    let durations = state
        .metrics()
        .execution_duration_seconds
        .with_label_values(&["succeeded"]);
    let count = state
        .metrics()
        .executions_total
        .with_label_values(&["succeeded"]);
    let before = (durations.get_sample_count(), count.get());
    assert_eq!(
        complete(result("completion-rollback", ExecutionStatus::Succeeded)).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!((durations.get_sample_count(), count.get()), before);
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id='completion-rollback' AND status='running' AND input IS NOT NULL").await, 1);
    assert_eq!(
        scalar(
            owner,
            "SELECT sum(invocation_count)::bigint FROM usage_rollups WHERE tenant_id='completion'"
        )
        .await,
        2
    );
    fixture_execute(owner, "DROP TRIGGER reject_completion_commit ON executions; DROP FUNCTION reject_completion_commit()", vec![]).await.unwrap();
    assert_eq!(
        complete(result("completion-rollback", ExecutionStatus::Succeeded)).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        (durations.get_sample_count(), count.get()),
        (before.0 + 1, before.1 + 1)
    );
    assert_eq!(inflight(state.pool(), "completion").await, 0);
    println!("PASS concurrent duplicate/conflicting completions: one committed result, one usage count; failed commit rolls back execution, usage and metrics");
}

async fn direct_http_dispatch_regression(owner: &DatabaseConnection, pool: &DatabaseConnection) {
    use axum::{routing::post, Router};
    use hibana_shared::http::HttpRequest;
    use std::time::Duration;
    use tokio::sync::{mpsc, Semaphore};
    use tokio::time::timeout;

    fixture_execute(owner, r#"
        INSERT INTO tenants (id,slug,name,status,quotas)
            VALUES ('slots','slots','Slots','active','{"max_concurrent_executions":1}');
        INSERT INTO components (id,tenant_id,name,ingress_enabled) VALUES ('slots-app','slots','app',true);
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256)
            VALUES ('slots-v1','slots','slots-app','1','unused','abcd');
        UPDATE components SET active_version_id='slots-v1' WHERE id='slots-app';
    "#, vec![]).await.unwrap();

    let store = Arc::new(crate::store::InProcStore::new());
    // Separate process-local request capacity, sharing only DB and rate limits.
    let first = state(pool.clone(), store.clone());
    let second = state(pool.clone(), store);
    // Completing and cleaning executions must not require a healthy Redis.
    let finalizer = state(pool.clone(), Arc::new(crate::store::FailingStore));
    let worker_state = finalizer.clone();
    let gate = Arc::new(Semaphore::new(0));
    let worker_gate = gate.clone();
    let (started, mut started_rx) = mpsc::channel::<()>(4);
    let worker = Router::new().route("/invoke", post(move |request_headers: HeaderMap| {
        let state = worker_state.clone();
        let gate = worker_gate.clone();
        let started = started.clone();
        async move {
            let Json(job) = direct_http::redeem(State(state.clone()), request_headers).await.unwrap();
            let request: HttpRequest = serde_json::from_value(job.input).unwrap();
            let tx = state.pool().begin().await.unwrap();
            db::set_tenant_guc(&tx, "slots").await.unwrap();
            let claimed = fixture_execute(&tx,
                "UPDATE executions SET status='running',started_at=now() WHERE tenant_id='slots' AND id=$1 AND status='pending'",
                vec![job.execution_id.clone().into()]).await.unwrap();
            assert_eq!(claimed.rows_affected(), 1);
            tx.commit().await.unwrap();
            started.send(()).await.unwrap();
            gate.acquire().await.unwrap().forget();
            let result = ResultMessage {
                logs: None,
                execution_id: job.execution_id,
                tenant_id: job.tenant_id,
                status: ExecutionStatus::Succeeded,
                output: Some(json!({"status":200,"streamed":true})),
                error: None,
                job_token: job.job_token.clone(),
                usage: Some(UsageMetrics::default()),
            };
            assert_eq!(crate::completion::complete(State(state), headers(&job.job_token), Json(result)).await.unwrap(),
                StatusCode::NO_CONTENT);
            (StatusCode::OK, request.into_body_bytes().unwrap())
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, worker).await.unwrap() });
    // This ignored regression runs alone against a disposable DB in its own process.
    let previous_endpoint = std::env::var("WORKER_HTTP_URL").unwrap();
    std::env::set_var("WORKER_HTTP_URL", endpoint);

    for _ in 0..3 {
        let invoke = |state: AppState| {
            tokio::spawn(async move {
                direct_http::accept(&state, "slots", "app", std::future::ready(Ok(json!({}))))
                    .await
                    .unwrap()
                    .status()
            })
        };
        let mut a = invoke(first.clone());
        let mut b = invoke(second.clone());
        timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let a_rejected = timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut a => {
                    assert_eq!(result.unwrap(), StatusCode::TOO_MANY_REQUESTS);
                    true
                },
                result = &mut b => {
                    assert_eq!(result.unwrap(), StatusCode::TOO_MANY_REQUESTS);
                    false
                },
            }
        })
        .await
        .unwrap();
        assert_eq!(inflight(pool, "slots").await, 1);
        let winner = if a_rejected { b } else { a };
        gate.add_permits(1);
        let (completed, reaped) = tokio::join!(
            timeout(Duration::from_secs(5), winner),
            crate::reaper::reconcile_once(
                &finalizer,
                0,
                crate::config::DEFAULT_EXECUTION_RETENTION_DAYS
            ),
        );
        assert_eq!(completed.unwrap().unwrap(), StatusCode::OK);
        reaped.unwrap();
        assert_eq!(inflight(pool, "slots").await, 0);
        // The next round must be admitted immediately, without a later reaper pass.
    }
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE tenant_id='slots' AND status='succeeded'"
        )
        .await,
        3
    );
    println!("PASS shared DB concurrency: two Control Planes enforce one slot, completion/reaper release immediately without Redis");

    // Echo the stored envelope through the Worker handoff, including UTF-8
    // bodies that JSONB cannot represent directly because they contain NUL.
    for body in [
        b"hello".as_slice(),
        b"hello\0world",
        "雪\0".as_bytes(),
        &[0, 255, 128],
    ] {
        let (parts, ()) = axum::http::Request::builder()
            .method("POST")
            .uri("/echo")
            .body(())
            .unwrap()
            .into_parts();
        let input = serde_json::to_value(HttpRequest::from_parts(&parts, body)).unwrap();
        gate.add_permits(1);
        let response = timeout(
            Duration::from_secs(5),
            direct_http::accept(&first, "slots", "app", std::future::ready(Ok(input))),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "body: {body:?}");
        let echoed = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(echoed.as_ref(), body);
        started_rx.recv().await.unwrap();
    }
    std::env::set_var("WORKER_HTTP_URL", previous_endpoint);
    server.abort();
    println!("PASS HTTP body: text, NUL and binary survive JSONB persistence and Worker handoff byte-for-byte");
}

async fn assert_usage_boundaries(pool: &DatabaseConnection) {
    let tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&tx, "http").await.unwrap();
    db::create_component(&tx, "http", "usage-boundary", "usage-boundary")
        .await
        .unwrap();
    let from = chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    let to = from.succ_opt().unwrap();
    let usage = UsageMetrics {
        cpu_fuel_used: u64::MAX,
        wall_time_ms: u64::MAX,
        peak_memory_bytes: u64::MAX,
        output_bytes: u64::MAX,
    };
    for day in [from, to] {
        db::upsert_usage_rollup(
            &tx,
            "http",
            "usage-boundary",
            day,
            ExecutionStatus::Succeeded,
            &usage,
        )
        .await
        .unwrap();
    }
    // Exercise every additive column, including counters normally reached only
    // after many invocations. The next completion must still be able to commit.
    fixture_execute(&tx, "UPDATE usage_rollups SET invocation_count=9223372036854775807, succeeded_count=9223372036854775807, failed_count=9223372036854775807, timeout_count=9223372036854775807 WHERE component_id='usage-boundary'", vec![]).await.unwrap();
    for status in [
        ExecutionStatus::Succeeded,
        ExecutionStatus::Failed,
        ExecutionStatus::Timeout,
    ] {
        db::upsert_usage_rollup(&tx, "http", "usage-boundary", from, status, &usage)
            .await
            .unwrap();
    }
    // PostgreSQL SUM(bigint) returns numeric; spanning days must also stay bounded.
    let rows = db::get_usage_rollups(&tx, "http", from, to, Some("usage-boundary"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0].usage;
    for value in [
        row.invocation_count,
        row.cpu_fuel_used,
        row.wall_time_ms,
        row.peak_memory_bytes_max,
        row.output_bytes,
        row.succeeded_count,
        row.failed_count,
        row.timeout_count,
    ] {
        assert_eq!(value, i64::MAX);
    }
    db::set_tenant_guc(&tx, "other").await.unwrap();
    assert!(db::get_usage_rollups(&tx, "http", from, to, None)
        .await
        .unwrap()
        .is_empty());
    tx.rollback().await.unwrap();
    println!("PASS usage accumulation and date aggregation saturate without blocking completion or bypassing RLS");
}

async fn assert_inflight_queries(owner: &DatabaseConnection, pool: &DatabaseConnection) {
    fixture_execute(
        owner,
        r#"
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,created_at)
        SELECT 'existence-'||status,'http','source','source-v1',status,true,now()-interval '1 day'
        FROM unnest(ARRAY['pending','running','succeeded','failed','timeout']) AS status;
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request)
        VALUES ('existence-non-http','http','source','source-v2','pending',false);
    "#,
        vec![],
    )
    .await
    .unwrap();
    for context in ["http", "other"] {
        let tx = pool.begin().await.unwrap();
        db::set_tenant_guc(&tx, context).await.unwrap();
        let visible = context == "http";
        assert_eq!(
            db::has_active_executions_for_component(&tx, "http", "source")
                .await
                .unwrap(),
            visible
        );
        assert_eq!(
            db::has_active_executions_for_version(&tx, "http", "source-v1")
                .await
                .unwrap(),
            visible
        );
        assert!(
            !db::has_active_executions_for_component(&tx, "http", "target")
                .await
                .unwrap()
        );
        assert!(
            !db::has_active_executions_for_version(&tx, "http", "source-v2")
                .await
                .unwrap()
        );
        assert!(
            !db::has_active_executions_for_component(&tx, "other", "source")
                .await
                .unwrap()
        );
        let versions = db::active_execution_version_ids(&tx, "http", "source")
            .await
            .unwrap();
        assert_eq!(
            versions,
            if visible {
                vec!["source-v1".to_string()]
            } else {
                vec![]
            }
        );
        assert_eq!(
            db::count_inflight_executions(&tx, "http").await.unwrap(),
            if visible { 2 } else { 0 }
        );
        let swept = db::finalize_stuck_executions(&tx, "http", 60)
            .await
            .unwrap();
        assert_eq!(swept.len(), if visible { 2 } else { 0 });
        assert!(
            !db::has_active_executions_for_component(&tx, "http", "source")
                .await
                .unwrap()
        );
        assert_eq!(db::count_inflight_executions(&tx, "http").await.unwrap(), 0);
        tx.rollback().await.unwrap();
    }
    fixture_execute(
        owner,
        "DELETE FROM executions WHERE id LIKE 'existence-%'",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS execution existence, capacity and recovery share HTTP state filters and preserve tenant RLS");
}

async fn console_information_regression(state: &AppState) {
    use crate::handlers::{
        configuration::get_function_config,
        executions::{get_execution, list_executions, ExecutionsQuery},
    };
    use axum::extract::{Path, Query};
    use hibana_database::prelude::*;
    let principal = |tenant: &str| crate::auth::Principal {
        tenant_id: tenant.into(),
        user_id: None,
        token_id: "test".into(),
        scopes: vec![hibana_shared::Scope::Read, hibana_shared::Scope::Deploy],
        role: hibana_shared::Role::Admin,
    };
    let tx = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&tx, "http").await.unwrap();
    db::create_component(&tx, "http", "console", "console")
        .await
        .unwrap();
    db::insert_version(
        &tx,
        "http",
        "console",
        "console-v1",
        "1.0.0",
        "console.wasm",
        "abcd",
        4,
        &json!({}),
        &json!(hibana_shared::ResourceLimits::default()),
        None,
    )
    .await
    .unwrap();
    db::switch_active_version(&tx, "http", "console", "console-v1")
        .await
        .unwrap();
    let now = chrono::Utc::now();
    for i in 0..27 {
        executions::Entity::insert(executions::ActiveModel {
            id: Set(format!("console-{i:02}")),
            tenant_id: Set("http".into()),
            component_id: Set("console".into()),
            version_id: Set("console-v1".into()),
            status: Set(if i == 25 { "succeeded" } else { "failed" }.into()),
            created_at: Set(if i == 26 {
                now - chrono::Duration::hours(25)
            } else {
                now
            }),
            finished_at: Set(Some(now)),
            error: Set(Some(json!({"code":"fixture_error"}))),
            input: Set(Some(json!("private-input"))),
            output: Set(Some(json!("private-output"))),
            ..Default::default()
        })
        .exec(&tx)
        .await
        .unwrap();
    }
    db::insert_secret_meta(&tx, "http", "console-old", "console", "TOKEN", 1)
        .await
        .unwrap();
    version_secret_bindings::Entity::insert(version_secret_bindings::ActiveModel {
        tenant_id: Set("http".into()),
        component_id: Set("console".into()),
        version_id: Set("console-v1".into()),
        secret_id: Set("console-old".into()),
        name: Set("TOKEN".into()),
    })
    .exec(&tx)
    .await
    .unwrap();
    function_secrets::Entity::update_many()
        .col_expr(function_secrets::Column::DeletedAt, Expr::val(now))
        .filter(function_secrets::Column::Id.eq("console-old"))
        .exec(&tx)
        .await
        .unwrap();
    db::insert_secret_meta(&tx, "http", "console-new", "console", "TOKEN", 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    async fn body(response: impl IntoResponse) -> serde_json::Value {
        let response = response.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    let page = body(
        list_executions(
            State(state.clone()),
            principal("http"),
            Path("console".into()),
            Query(ExecutionsQuery {
                errors_only: true,
                before: None,
            }),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 20);
    let detail = body(
        get_execution(
            State(state.clone()),
            principal("http"),
            Path("console-00".into()),
        )
        .await
        .unwrap(),
    )
    .await;
    for key in ["input", "output", "input_ref", "output_ref"] {
        assert!(
            detail.get(key).is_none(),
            "execution details must not expose {key}"
        );
    }
    assert_eq!(
        get_execution(
            State(state.clone()),
            principal("other"),
            Path("console-00".into())
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(page["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["status"] == "failed"
            && e.get("input").is_none()
            && e.get("output").is_none()
            && e.get("input_ref").is_none()));
    let next = body(
        list_executions(
            State(state.clone()),
            principal("http"),
            Path("console".into()),
            Query(ExecutionsQuery {
                errors_only: true,
                before: Some(page["next_cursor"].as_str().unwrap().into()),
            }),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(next["items"].as_array().unwrap().len(), 5, "ties paginate without duplicate or missing rows, and old/successful executions are excluded");
    assert_eq!(next["next_cursor"], serde_json::Value::Null);
    assert!(next["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| !page["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["execution_id"] == e["execution_id"])));

    // HTTP errors are operational failures even when Wasm completed successfully.
    // Project only the status, including if old records contain response bodies.
    for (status, expected, is_error) in [
        (json!(200), json!(200), false),
        (json!(404), json!(404), true),
        (json!(503), json!(503), true),
        (json!(599), json!(599), true),
        (json!(600), json!(null), false),
        (json!("500"), json!(null), false),
        (json!(null), json!(null), false),
    ] {
        let tx = state.pool().begin().await.unwrap();
        db::set_tenant_guc(&tx, "http").await.unwrap();
        executions::Entity::update_many()
            .col_expr(executions::Column::HttpRequest, Expr::val(true))
            .col_expr(
                executions::Column::Output,
                Expr::val(json!({"status": status, "body": "private-response-body"})),
            )
            .filter(executions::Column::Id.eq("console-25"))
            .exec(&tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let detail = body(
            get_execution(
                State(state.clone()),
                principal("http"),
                Path("console-25".into()),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(detail["http_status"], expected);
        assert_eq!(detail["status"], "succeeded");
        assert!(!detail.to_string().contains("private-response-body"));
        let errors = body(
            list_executions(
                State(state.clone()),
                principal("http"),
                Path("console".into()),
                Query(ExecutionsQuery {
                    errors_only: true,
                    before: None,
                }),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(
            errors["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["execution_id"] == "console-25"),
            is_error
        );
        assert!(!errors.to_string().contains("private-response-body"));
    }
    let denied = list_executions(
        State(state.clone()),
        principal("other"),
        Path("console".into()),
        Query(ExecutionsQuery::default()),
    )
    .await
    .err()
    .unwrap()
    .into_response();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let malformed = list_executions(
        State(state.clone()),
        principal("http"),
        Path("console".into()),
        Query(ExecutionsQuery {
            before: Some("invalid".into()),
            ..Default::default()
        }),
    )
    .await
    .err()
    .unwrap()
    .into_response();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    let settings = body(
        get_function_config(
            State(state.clone()),
            principal("http"),
            Path("console".into()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(settings["version_id"], "console-v1");
    assert_eq!(
        settings["secrets"],
        json!([{"name":"TOKEN", "available":false}]),
        "recreating a Secret name does not restore an old binding"
    );
    assert!(settings["resource_limits"]["max_memory_bytes"].is_number());
    assert_eq!(settings["net_allow_outbound"], json!([]));
    println!("PASS console execution pagination, 24h window, RLS, body exclusion and immutable Secret references");
}

async fn secret_resolution_regression(state: &AppState) {
    use crate::secrets::{encrypt, resolve_for_injection, SecretError};
    let tx = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&tx, "http").await.unwrap();
    db::insert_secret_meta(&tx, "http", "sec-projection", "source", "TOKEN", 2)
        .await
        .unwrap();
    fixture_execute(&tx, "INSERT INTO version_secret_bindings (tenant_id,component_id,version_id,secret_id,name) VALUES ('http','source','source-v1','sec-projection','TOKEN')", vec![]).await.unwrap();
    let time = |day: &str| {
        chrono::DateTime::parse_from_rfc3339(day)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    for (version, value, created) in [
        (1, b"old".as_slice(), "2000-01-01T00:00:00Z"),
        (2, b"new".as_slice(), "2000-01-03T00:00:00Z"),
    ] {
        let e = encrypt(
            state.secret_keyring(),
            "http",
            "source",
            "sec-projection",
            "TOKEN",
            version,
            value,
        )
        .unwrap();
        fixture_execute(&tx, "INSERT INTO function_secret_versions (tenant_id,secret_id,version,kek_kid,wrapped_dek,dek_nonce,nonce,ciphertext,value_len,reason,created_at) VALUES ('http','sec-projection',$1,$2,$3,$4,$5,$6,$7,'rotate',$8)", vec![(version).to_owned().into(), (e.kek_kid).to_owned().into(), (e.wrapped_dek).to_owned().into(), (e.dek_nonce).to_owned().into(), (e.nonce).to_owned().into(), (e.ciphertext).to_owned().into(), (e.value_len).to_owned().into(), (time(created)).to_owned().into()]).await.unwrap();
    }
    let allowed = ["TOKEN".into()].into_iter().collect();
    for (at, version, value) in [
        ("2000-01-02T00:00:00Z", 1, "old"),
        ("2000-01-04T00:00:00Z", 2, "new"),
    ] {
        let resolved = resolve_for_injection(
            &tx,
            state.secret_keyring(),
            "http",
            "source",
            "source-v1",
            time(at),
            &allowed,
        )
        .await
        .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].version, version);
        crate::secrets::assert_secret_eq(&resolved[0].value, value);
    }
    assert!(
        matches!(
            resolve_for_injection(
                &tx,
                state.secret_keyring(),
                "http",
                "source",
                "source-v1",
                time("1999-01-01T00:00:00Z"),
                &allowed
            )
            .await,
            Err(SecretError::VersionUnresolved)
        ),
        "a live Secret without a generation must fail closed"
    );
    assert!(resolve_for_injection(
        &tx,
        state.secret_keyring(),
        "http",
        "source",
        "source-v2",
        time("2000-01-04T00:00:00Z"),
        &allowed
    )
    .await
    .unwrap()
    .is_empty());
    db::soft_delete_secret(&tx, "http", "sec-projection")
        .await
        .unwrap();
    db::insert_secret_meta(&tx, "http", "sec-reused", "source", "TOKEN", 1)
        .await
        .unwrap();
    assert!(
        resolve_for_injection(
            &tx,
            state.secret_keyring(),
            "http",
            "source",
            "source-v1",
            time("2000-01-04T00:00:00Z"),
            &allowed
        )
        .await
        .unwrap()
        .is_empty(),
        "a reused name is not the pinned Secret identity"
    );
    fixture_execute(&tx, "INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request) VALUES ('secret-projection','http','source','source-v1','pending',true)", vec![]).await.unwrap();
    let execution = db::secret_execution(&tx, "http", "secret-projection")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.component_id, "source");
    assert_eq!(execution.version_id, "source-v1");
    assert_eq!(execution.status, "pending");
    db::set_tenant_guc(&tx, "other").await.unwrap();
    assert!(db::secret_execution(&tx, "http", "secret-projection")
        .await
        .unwrap()
        .is_none());
    assert!(resolve_for_injection(
        &tx,
        state.secret_keyring(),
        "http",
        "source",
        "source-v1",
        time("2000-01-04T00:00:00Z"),
        &allowed
    )
    .await
    .unwrap()
    .is_empty());
    tx.rollback().await.unwrap();
    println!("PASS Secret generation / unresolved generation denied / version binding / name reuse / narrow execution projection / FORCE RLS");
}

// Raw statements are restricted to adversarial fixtures and schema assertions.
// Production behavior exercised above uses entity queries on the same transaction.
async fn fixture_execute(
    db: &impl ConnectionTrait,
    sql: &str,
    values: Vec<sea_orm::Value>,
) -> Result<ExecResult, DbErr> {
    if values.is_empty() {
        db.execute_unprepared(sql).await
    } else {
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await
    }
}
async fn fixture_scalar<T: TryGetable>(
    db: &impl ConnectionTrait,
    sql: &str,
    values: Vec<sea_orm::Value>,
) -> Result<T, DbErr> {
    db.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        sql,
        values,
    ))
    .await?
    .ok_or_else(|| DbErr::Custom("fixture result missing".into()))?
    .try_get_by_index(0)
}
async fn assert_fresh_schema(owner: &DatabaseConnection) {
    assert_eq!(scalar(owner,"SELECT count(*) FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE'").await,16);
    assert_eq!(
        scalar(owner, "SELECT count(*) FROM seaql_migrations").await,
        13
    );
    assert_eq!(scalar(owner,"SELECT count(*) FROM pg_class c JOIN pg_namespace n ON c.relnamespace=n.oid WHERE n.nspname='public' AND c.relkind='r' AND c.relrowsecurity AND c.relforcerowsecurity").await,13);
    assert_eq!(scalar(owner,"SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND column_name IN ('canary_weight','canary_version_id','chain_depth','routing_reason','idempotency_key')").await,0);
    println!("PASS fresh ORM schema: 15 platform tables, RLS, no removed feature tables, idempotent migration");
}

// Exercise historical data migrations before the irreversible OIDC cutover.
async fn migrate_before_oidc_cutover(owner: &DatabaseConnection) {
    use hibana_migration::MigratorTrait as _;
    let applied = if fixture_scalar::<bool>(
        owner,
        "SELECT to_regclass('public.seaql_migrations') IS NOT NULL",
        vec![],
    )
    .await
    .unwrap()
    {
        scalar(owner, "SELECT count(*) FROM seaql_migrations").await
    } else {
        0
    };
    assert!(applied <= 7);
    if applied < 7 {
        hibana_migration::Migrator::up(owner, Some((7 - applied) as u32))
            .await
            .unwrap();
    }
}

async fn assert_oidc_cutover(owner: &DatabaseConnection) {
    use hibana_migration::MigratorTrait as _;
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    fixture_execute(owner, r#"
        INSERT INTO users(id,tenant_id,email,password_hash,role,oidc_issuer,oidc_subject,auth_version)
        VALUES ('cutover-user','http','cutover@example.invalid','unused','admin','https://fixture-idp.invalid','cutover-subject',4);
        INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at,auth_method,user_auth_version)
        SELECT 'cutover-'||method,'http','cutover-user','cutover-'||method,ARRAY['read'],now()+interval '1 hour',method,4
        FROM unnest(ARRAY['api','oidc','password']) AS method;
        INSERT INTO api_tokens(id,tenant_id,token_hash,scopes,expires_at,auth_method,user_auth_version,revoked_at)
        VALUES ('cutover-service','http','cutover-service',ARRAY['read'],now()+interval '1 hour','api',0,NULL),
               ('cutover-revoked','http','cutover-revoked',ARRAY['read'],now()+interval '1 hour','api',0,'2026-01-01T00:00:00Z');
    "#, vec![]).await.unwrap();
    hibana_migration::Migrator::up(owner, Some(1))
        .await
        .unwrap();
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    fixture_execute(owner, "INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at,auth_method,user_auth_version) VALUES ('profile-migration-token','http','cutover-user','profile-migration-token',ARRAY['read','invoke','deploy','admin'],now()+interval '1 hour','api',5), ('profile-migration-invoke','http','cutover-user','profile-migration-invoke',ARRAY['invoke'],now()+interval '1 hour','api',5)", vec![]).await.unwrap();
    fixture_execute(owner, r#"
        UPDATE component_versions SET capabilities='{"imports":["wasi:cli/environment@0.2.0"],"env":["KEY"],"net_allow_outbound":["legacy.example:443"]}' WHERE id='source-v1';
        UPDATE components SET egress_policy='["approved.example:443"]' WHERE id='target';
        INSERT INTO components(id,tenant_id,name,deleted_at) VALUES ('egress-cutover','other','egress-cutover',now());
        INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256,status,capabilities,deleted_at)
        VALUES ('egress-cutover-v1','other','egress-cutover','1','unused','abcd','active','{"net_allow_outbound":["deleted.example:443"]}',now());
    "#, vec![]).await.unwrap();
    hibana_migration::Migrator::up(owner, Some(3))
        .await
        .unwrap();
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    fixture_execute(
        owner,
        "UPDATE components SET previous_active_version_id='source-v2' WHERE id='source'",
        vec![],
    )
    .await
    .unwrap();
    let publication_sql = "SELECT jsonb_agg(jsonb_build_object('id',id,'active',active_version_id,'previous',previous_active_version_id) ORDER BY id) FROM components";
    let publication: serde_json::Value = fixture_scalar(owner, publication_sql, vec![])
        .await
        .unwrap();
    let versions: serde_json::Value = fixture_scalar(
        owner,
        "SELECT jsonb_agg(to_jsonb(v) - 'status' ORDER BY id) FROM component_versions v",
        vec![],
    )
    .await
    .unwrap();
    let (first, second) = tokio::join!(
        crate::migrations::run_migrations(owner),
        crate::migrations::run_migrations(owner)
    );
    first.unwrap();
    second.unwrap();
    hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .unwrap();
    assert_eq!(scalar(owner, "SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND table_name='component_versions' AND column_name='status'").await, 0);
    assert_eq!(
        fixture_scalar::<serde_json::Value>(owner, publication_sql, vec![])
            .await
            .unwrap(),
        publication
    );
    assert_eq!(
        fixture_scalar::<serde_json::Value>(
            owner,
            "SELECT jsonb_agg(to_jsonb(v) ORDER BY id) FROM component_versions v",
            vec![]
        )
        .await
        .unwrap(),
        versions
    );
    fixture_execute(
        owner,
        "UPDATE components SET previous_active_version_id=NULL WHERE id='source'",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS version publication migration: removed redundant status, preserved active/previous pointers and version data across tenants and deleted rows");
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM components WHERE egress_policy IS NULL"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM component_versions WHERE capabilities ? 'net_allow_outbound'"
        )
        .await,
        0
    );
    assert!(fixture_scalar::<bool>(
        owner,
        "SELECT egress_policy='[]'::jsonb FROM components WHERE id='source'",
        vec![]
    )
    .await
    .unwrap());
    assert!(fixture_scalar::<bool>(owner, "SELECT egress_policy='[\"approved.example:443\"]'::jsonb FROM components WHERE id='target'", vec![]).await.unwrap());
    assert!(fixture_scalar::<bool>(owner, r#"SELECT capabilities='{"imports":["wasi:cli/environment@0.2.0"],"env":["KEY"]}'::jsonb FROM component_versions WHERE id='source-v1'"#, vec![]).await.unwrap());
    for invalid in ["NULL", "'null'::jsonb", "'{}'::jsonb"] {
        assert!(fixture_execute(
            owner,
            &format!("UPDATE components SET egress_policy={invalid} WHERE id='source'"),
            vec![]
        )
        .await
        .is_err());
    }
    fixture_execute(owner, "UPDATE components SET egress_policy='[]' WHERE id='target'; UPDATE component_versions SET capabilities='{}' WHERE id='source-v1'; DELETE FROM component_versions WHERE id='egress-cutover-v1'; DELETE FROM components WHERE id='egress-cutover'", vec![]).await.unwrap();
    println!("PASS application egress migration: default deny, preserved application grants, removed version grants across tenants and deleted rows");
    assert!(fixture_scalar::<bool>(
        owner,
        "SELECT to_regclass('public.users_tenant_id_email_key') IS NULL",
        vec![]
    )
    .await
    .unwrap());
    assert!(fixture_scalar::<bool>(
        owner,
        "SELECT revoked_at IS NULL FROM api_tokens WHERE id='profile-migration-token'",
        vec![]
    )
    .await
    .unwrap());
    assert!(fixture_scalar::<bool>(owner, "SELECT scopes=ARRAY['read','deploy','admin'] FROM api_tokens WHERE id='profile-migration-token'", vec![]).await.unwrap());
    assert!(fixture_scalar::<bool>(owner, "SELECT scopes=ARRAY[]::text[] AND revoked_at IS NULL FROM api_tokens WHERE id='profile-migration-invoke'", vec![]).await.unwrap());
    assert!(fixture_execute(
        owner,
        "UPDATE api_tokens SET scopes=ARRAY['invoke'] WHERE id='profile-migration-token'",
        vec![]
    )
    .await
    .is_err());
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM api_tokens WHERE id LIKE 'cutover-%'"
        )
        .await,
        5
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM api_tokens WHERE id LIKE 'cutover-%' AND revoked_at IS NULL"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT auth_version FROM users WHERE id='cutover-user'"
        )
        .await,
        5
    );
    assert!(fixture_scalar::<bool>(owner, "SELECT revoked_at='2026-01-01T00:00:00Z'::timestamptz FROM api_tokens WHERE id='cutover-revoked'", vec![]).await.unwrap());
    assert_eq!(scalar(owner, "SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND table_name='users' AND column_name='password_hash'").await, 0);
    for function in [
        "auth_lookup_user_by_email(text,text)",
        "auth_lookup_token_by_hash_v2(text)",
        "legacy_token_auth_method(text,text,text)",
        "classify_legacy_token_auth()",
        "snapshot_legacy_token_generation()",
    ] {
        assert!(
            fixture_scalar::<bool>(
                owner,
                "SELECT to_regprocedure($1) IS NULL",
                vec![format!("public.{function}").into()]
            )
            .await
            .unwrap(),
            "{function}"
        );
    }
    assert_eq!(scalar(owner, "SELECT count(*) FROM pg_trigger WHERE tgname IN ('classify_legacy_token_auth','snapshot_legacy_token_generation')").await, 0);
    assert_eq!(scalar(owner, "SELECT count(*) FROM pg_proc p, LATERAL aclexplode(p.proacl) a WHERE p.oid='public.auth_lookup_token_by_hash(text)'::regprocedure AND a.grantee=0 AND a.privilege_type='EXECUTE'").await, 0);
    // Current issuance and authentication still operate with the restricted DB role.
    let runtime = hibana_database::postgres::connect(&std::env::var("DATABASE_URL").unwrap(), 1, 0)
        .await
        .unwrap();
    assert!(db::find_token_by_hash(&runtime, "cutover-api")
        .await
        .unwrap()
        .is_none());
    assert!(db::find_token_by_hash(&runtime, "cutover-service")
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    for columns in [
        "",
        ",auth_method",
        ",user_auth_version",
        ",auth_method,user_auth_version",
    ] {
        let extra = match columns {
            "" => "",
            ",auth_method" => ",'api'",
            ",user_auth_version" => ",5",
            _ => ",'password',5",
        };
        let tx = runtime.begin().await.unwrap();
        db::set_tenant_guc(&tx, "http").await.unwrap();
        let query = format!("INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at{columns}) VALUES ('cutover-invalid','http','cutover-user','cutover-invalid',ARRAY['read'],now()+interval '1 hour'{extra})");
        assert!(
            fixture_execute(&tx, &query, vec![]).await.is_err(),
            "{columns}"
        );
        tx.rollback().await.unwrap();
    }
    fixture_execute(owner, "INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at,auth_method,user_auth_version) VALUES ('cutover-new','http','cutover-user','cutover-new',ARRAY['read'],now()+interval '1 hour','api',5)", vec![]).await.unwrap();
    assert!(db::find_token_by_hash(&runtime, "cutover-new")
        .await
        .unwrap()
        .unwrap()
        .into_principal()
        .is_some());
    crate::migrations::run_migrations(owner).await.unwrap();
    assert!(
        db::find_token_by_hash(&runtime, "cutover-new")
            .await
            .unwrap()
            .unwrap()
            .into_principal()
            .is_some(),
        "repeated migration must not revoke newly issued credentials"
    );
    assert!(hibana_migration::Migrator::down(owner, Some(1))
        .await
        .unwrap_err()
        .to_string()
        .contains("cannot be rolled back"));
    hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .unwrap();
    runtime.close().await.unwrap();
    fixture_execute(owner, "DELETE FROM api_tokens WHERE id LIKE 'cutover-%' OR id LIKE 'profile-migration-%'; DELETE FROM users WHERE id='cutover-user'", vec![]).await.unwrap();
    println!("PASS OIDC cutover removes password storage and legacy writers, revokes existing credentials, preserves rows, and permits current token issuance");
}

async fn assert_build_metadata_upgrade(owner: &DatabaseConnection) {
    use hibana_migration::MigratorTrait as _;
    // Exercise an existing baseline with real versions; never recreate the DB.
    hibana_migration::Migrator::down(owner, Some(6))
        .await
        .unwrap();
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    migrate_before_oidc_cutover(owner).await;
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM component_versions WHERE build_metadata IS NULL"
        )
        .await,
        3
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM components WHERE active_version_id=id||'-v1'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM component_versions WHERE id='source-v1' AND wasm_sha256='abcd'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM components WHERE egress_policy IS NOT NULL"
        )
        .await,
        0
    );
    println!("PASS additive build metadata and egress migrations preserves versions, hashes and publication; old versions remain unrecorded");
}

async fn assert_execution_input_upgrade(owner: &DatabaseConnection) {
    use hibana_migration::MigratorTrait as _;
    hibana_migration::Migrator::down(owner, Some(4))
        .await
        .unwrap();
    fixture_execute(owner, r#"
        INSERT INTO components(id,tenant_id,name) VALUES ('retention-other','other','retention');
        INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256,status)
            VALUES ('retention-other-v1','other','retention-other','1','unused','abcd','active');
        UPDATE tenants SET status='suspended' WHERE id='other';
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,input,input_ref)
            SELECT 'retention-upgrade-'||c.tenant_id||'-'||s.status,c.tenant_id,c.id,v.id,s.status,true,
                '{"headers":{"authorization":"dummy-token"},"body":"dummy-password"}'::jsonb,'dummy-ref'
            FROM components c JOIN component_versions v ON v.component_id=c.id
            CROSS JOIN (VALUES ('pending'),('running'),('succeeded'),('failed'),('timeout')) AS s(status)
            WHERE v.id IN ('source-v1','retention-other-v1');
    "#, vec![]).await.unwrap();
    migrate_before_oidc_cutover(owner).await;
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id LIKE 'retention-upgrade-%' AND status IN ('pending','running') AND input IS NOT NULL AND input_ref IS NOT NULL").await, 4);
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id LIKE 'retention-upgrade-%' AND status NOT IN ('pending','running') AND input IS NULL AND input_ref IS NULL").await, 6);

    let tx = owner.begin().await.unwrap();
    db::set_tenant_guc(&tx, "http").await.unwrap();
    for status in [
        ExecutionStatus::Succeeded,
        ExecutionStatus::Failed,
        ExecutionStatus::Timeout,
    ] {
        fixture_execute(&tx, "UPDATE executions SET status='running',input='{}',input_ref='dummy-ref' WHERE id='retention-upgrade-http-running'", vec![]).await.unwrap();
        assert!(db::finalize_execution(
            &tx,
            "http",
            "retention-upgrade-http-running",
            status,
            None,
            None,
            None
        )
        .await
        .unwrap()
        .is_some());
        assert!(fixture_scalar::<bool>(&tx, "SELECT input IS NULL AND input_ref IS NULL FROM executions WHERE id='retention-upgrade-http-running'", vec![]).await.unwrap());
    }
    fixture_execute(&tx, "UPDATE executions SET created_at=now()-interval '1 day' WHERE id='retention-upgrade-http-pending'", vec![]).await.unwrap();
    assert_eq!(
        db::finalize_stuck_executions(&tx, "http", 60)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(fixture_scalar::<bool>(&tx, "SELECT input IS NULL AND input_ref IS NULL FROM executions WHERE id='retention-upgrade-http-pending'", vec![]).await.unwrap());
    tx.commit().await.unwrap();
    // Simulate an older Pod finishing after migration, including a suspended tenant.
    fixture_execute(owner, "UPDATE executions SET status='succeeded',input='{\"headers\":{\"authorization\":\"legacy-fixture\"}}',input_ref='legacy-ref' WHERE id IN ('retention-upgrade-http-running','retention-upgrade-other-running')", vec![]).await.unwrap();
    migrate_before_oidc_cutover(owner).await;
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id LIKE 'retention-upgrade-%' AND status='succeeded' AND input IS NOT NULL").await, 2);
    let runtime = hibana_database::postgres::connect(&std::env::var("DATABASE_URL").unwrap(), 2, 0)
        .await
        .unwrap();
    // This fixture intentionally stops before the OIDC/log schema migrations.
    // Exercise the input query here; the full reaper is tested after all migrations.
    for tenant in ["http", "other"] {
        let tx = runtime.begin().await.unwrap();
        db::set_tenant_guc(&tx, tenant).await.unwrap();
        db::purge_terminal_execution_inputs(&tx, tenant)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id LIKE 'retention-upgrade-%' AND status='succeeded' AND (input IS NOT NULL OR input_ref IS NOT NULL)").await, 0);
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id='retention-upgrade-other-pending' AND input IS NOT NULL AND input_ref IS NOT NULL").await, 1);
    // More than one batch must progress without touching live or foreign inputs.
    fixture_execute(owner, r#"
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,input_ref)
        SELECT 'retention-upgrade-batch-'||n,'http','source','source-v1','succeeded',true,'legacy-ref'
        FROM generate_series(1,501) AS n;
        UPDATE executions SET input_ref='foreign-ref' WHERE id='retention-upgrade-other-running';
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,input_ref)
        VALUES ('retention-upgrade-live-running','http','source','source-v1','running',true,'live-ref'),
            ('retention-upgrade-non-http','http','source','source-v1','succeeded',false,'non-http-ref');
    "#, vec![]).await.unwrap();
    let tx = runtime.begin().await.unwrap();
    db::set_tenant_guc(&tx, "http").await.unwrap();
    assert_eq!(
        db::purge_terminal_execution_inputs(&tx, "other")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        db::purge_terminal_execution_inputs(&tx, "http")
            .await
            .unwrap(),
        500
    );
    assert_eq!(
        db::purge_terminal_execution_inputs(&tx, "http")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db::purge_terminal_execution_inputs(&tx, "http")
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
    assert_eq!(scalar(owner, "SELECT count(*) FROM executions WHERE id IN ('retention-upgrade-other-running','retention-upgrade-live-running','retention-upgrade-non-http') AND input_ref IS NOT NULL").await, 3);
    runtime.close().await.unwrap();
    fixture_execute(owner, "DELETE FROM executions WHERE id LIKE 'retention-upgrade-%'; DELETE FROM component_versions WHERE id='retention-other-v1'; DELETE FROM components WHERE id='retention-other'; UPDATE tenants SET status='active' WHERE id='other'", vec![]).await.unwrap();
    println!("PASS request retention migration preserves live jobs, scrubs completed/suspended-tenant history; completion and sweeper discard input atomically");
}

async fn assert_secret_key_retention_upgrade(owner: &DatabaseConnection) {
    use hibana_migration::MigratorTrait as _;
    hibana_migration::Migrator::down(owner, Some(2))
        .await
        .unwrap();
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    fixture_execute(owner, r#"
        INSERT INTO components(id,tenant_id,name)
            SELECT 'key-'||id,id,'key-retention' FROM tenants WHERE id IN ('http','other');
        INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256,status)
            SELECT c.id||'-v'||n,c.tenant_id,c.id,n::text,'unused','abcd','active'
            FROM components c CROSS JOIN generate_series(1,2) AS n WHERE c.id IN ('key-http','key-other');
        UPDATE components SET active_version_id=id||'-v2' WHERE id IN ('key-http','key-other');
        INSERT INTO function_secrets(id,tenant_id,component_id,name,current_version)
            SELECT c.id||'-'||s.name,c.tenant_id,c.id,s.name,3 FROM components c
            CROSS JOIN (VALUES ('BOUND'),('UNUSED')) AS s(name) WHERE c.id IN ('key-http','key-other');
        INSERT INTO function_secret_versions(tenant_id,secret_id,version,kek_kid,wrapped_dek,dek_nonce,nonce,ciphertext,value_len,reason,created_at)
            SELECT s.tenant_id,s.id,n,
                CASE WHEN n=3 THEN 'key-current' WHEN n=1 THEN 'key-obsolete'
                    WHEN s.name='BOUND' THEN 'key-required' ELSE 'key-unbound' END,
                '\x00'::bytea,'\x00'::bytea,'\x00'::bytea,'\x00'::bytea,1,'rotate',
                '2026-01-01'::timestamptz+(n-1)*interval '1 day'
            FROM function_secrets s CROSS JOIN generate_series(1,3) AS n WHERE s.id LIKE 'key-%';
        INSERT INTO version_secret_bindings(tenant_id,component_id,version_id,name,secret_id)
            SELECT tenant_id,component_id,component_id||CASE WHEN name='BOUND' THEN '-v1' ELSE '-v2' END,name,id
            FROM function_secrets WHERE id LIKE 'key-%';
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,created_at)
            SELECT c.id||'-'||s.status,c.tenant_id,c.id,c.id||'-v1',s.status,true,'2026-01-02 12:00:00+00'
            FROM components c CROSS JOIN (VALUES ('pending'),('running')) AS s(status)
            WHERE c.id IN ('key-http','key-other');
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,created_at)
            SELECT c.id||'-current',c.tenant_id,c.id,c.id||'-v1','running',true,'2026-01-04'
            FROM components c WHERE c.id IN ('key-http','key-other');
        UPDATE tenants SET status='suspended' WHERE id='other';
    "#, vec![]).await.unwrap();
    // Prove the old function misses generations required by accepted jobs.
    assert_eq!(scalar(owner, "SELECT COALESCE(sum(n),0)::bigint FROM secrets_kek_kid_counts_all() WHERE kek_kid='key-required'").await, 0);
    migrate_before_oidc_cutover(owner).await;
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    let runtime = hibana_database::postgres::connect(&std::env::var("DATABASE_URL").unwrap(), 1, 0)
        .await
        .unwrap();
    // No tenant GUC: the restricted aggregate must include suspended tenants too.
    let counts: std::collections::BTreeMap<_, _> = db::secrets_kek_kid_counts_all(&runtime)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        counts,
        std::collections::BTreeMap::from([
            ("key-current".to_owned(), 4),
            ("key-required".to_owned(), 2),
        ]),
        "count each required generation once, using each execution's immutable version bindings"
    );
    assert_eq!(scalar(owner, "SELECT count(*) FROM pg_proc p, LATERAL aclexplode(p.proacl) a WHERE p.oid='public.secrets_kek_kid_counts_all()'::regprocedure AND a.grantee=0 AND a.privilege_type='EXECUTE'").await, 0);
    fixture_execute(owner, "UPDATE executions SET status='succeeded' WHERE id IN ('key-http-pending','key-other-pending')", vec![]).await.unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT n FROM secrets_kek_kid_counts_all() WHERE kek_kid='key-required'"
        )
        .await,
        2,
        "remaining running jobs still need the same old generations"
    );
    fixture_execute(owner, "UPDATE executions SET status=CASE WHEN tenant_id='http' THEN 'failed' ELSE 'timeout' END WHERE id IN ('key-http-running','key-other-running')", vec![]).await.unwrap();
    assert_eq!(scalar(owner, "SELECT COALESCE(sum(n),0)::bigint FROM secrets_kek_kid_counts_all() WHERE kek_kid='key-required'").await, 0);
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM function_secret_versions WHERE secret_id LIKE 'key-%'"
        )
        .await,
        12,
        "migration and counting leave the encrypted generation history intact"
    );
    runtime.close().await.unwrap();
    fixture_execute(
        owner,
        r#"
        DELETE FROM executions WHERE id LIKE 'key-%';
        DELETE FROM version_secret_bindings WHERE component_id IN ('key-http','key-other');
        DELETE FROM function_secret_versions WHERE secret_id LIKE 'key-%';
        DELETE FROM function_secrets WHERE id LIKE 'key-%';
        UPDATE components SET active_version_id=NULL WHERE id IN ('key-http','key-other');
        DELETE FROM component_versions WHERE component_id IN ('key-http','key-other');
        DELETE FROM components WHERE id IN ('key-http','key-other');
        UPDATE tenants SET status='active' WHERE id='other';
    "#,
        vec![],
    )
    .await
    .unwrap();
    println!("PASS key retention migration counts current and in-flight generations once, across tenants and immutable version bindings, until jobs finish");
}

async fn assert_legacy_database_rejected(owner: &DatabaseConnection) {
    assert!(hibana_database::postgres::assert_runtime_schema(owner)
        .await
        .is_err());
    fixture_execute(owner, "CREATE TABLE tenants(id text); INSERT INTO tenants VALUES ('keep-existing'); CREATE TABLE _sqlx_migrations(version bigint); INSERT INTO _sqlx_migrations VALUES (30)", vec![]).await.unwrap();
    assert!(crate::migrations::run_migrations(owner)
        .await
        .unwrap_err()
        .to_string()
        .contains("empty database"));
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM tenants WHERE id='keep-existing'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(owner, "SELECT version FROM _sqlx_migrations").await,
        30
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'"
        )
        .await,
        2
    );
    fixture_execute(
        owner,
        "DROP TABLE tenants, _sqlx_migrations; CREATE TABLE unrelated_fixture(id text)",
        vec![],
    )
    .await
    .unwrap();
    assert!(crate::migrations::run_migrations(owner).await.is_err());
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'"
        )
        .await,
        1
    );
    fixture_execute(owner, "DROP TABLE unrelated_fixture", vec![])
        .await
        .unwrap();
    println!("PASS legacy and unrelated databases rejected without changing existing schema/data");
}

async fn assert_tenant_context_is_transaction_local() {
    use hibana_database::prelude::*;
    let pool = hibana_database::postgres::connect(&std::env::var("DATABASE_URL").unwrap(), 1, 1)
        .await
        .unwrap();
    for commit in [true, false] {
        let tx = pool.begin().await.unwrap();
        set_tenant_guc(&tx, "http").await.unwrap();
        assert_eq!(components::Entity::find().all(&tx).await.unwrap().len(), 2);
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        // Reusing the sole physical connection must not inherit the prior tenant.
        assert!(components::Entity::find()
            .all(&pool)
            .await
            .map(|rows| rows.is_empty())
            .unwrap_or(true));
        let tx = pool.begin().await.unwrap();
        set_tenant_guc(&tx, "http'; SELECT 'other").await.unwrap();
        assert!(components::Entity::find()
            .all(&tx)
            .await
            .unwrap()
            .is_empty());
        set_tenant_guc(&tx, "other").await.unwrap();
        assert!(components::Entity::find()
            .all(&tx)
            .await
            .unwrap()
            .is_empty());
        tx.commit().await.unwrap();
    }
    pool.close().await.unwrap();
    println!(
        "PASS tenant context cleared after commit/rollback on reused connection; values stay bound"
    );
}

async fn assert_compound_identities(owner: &DatabaseConnection) {
    fixture_execute(owner, "INSERT INTO users(id,tenant_id,email,role) VALUES ('foreign-user','other','foreign@example.invalid','member')", vec![]).await.unwrap();
    for query in [
        "INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256) VALUES ('foreign-version','other','source','foreign','unused','unused')",
        "INSERT INTO executions(id,tenant_id,component_id,version_id) VALUES ('foreign-execution','other','source','source-v1')",
        "INSERT INTO usage_rollups(tenant_id,period_start,component_id) VALUES ('other',CURRENT_DATE,'source')",
        "INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at,auth_method,user_auth_version) VALUES ('foreign-token','http','foreign-user','fixture',ARRAY['read'],now()+interval '1 hour','api',0)",
        "UPDATE components SET active_version_id='target-v1' WHERE id='source'",
        "UPDATE components SET previous_active_version_id='target-v1' WHERE id='source'",
    ] {
        let error = fixture_execute(owner, query, vec![]).await.unwrap_err();
        assert!(matches!(error.sql_err(), Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_))), "composite identity must be enforced even for the owner role: {error}");
    }
    fixture_execute(owner, "DELETE FROM users WHERE id='foreign-user'", vec![])
        .await
        .unwrap();
    println!("PASS cross-tenant user/version/usage and cross-component active/previous references rejected by foreign keys");
}
