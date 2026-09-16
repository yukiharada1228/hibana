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
fn state(pool: DatabaseConnection, store: Arc<dyn crate::store::Store>) -> AppState {
    let cfg = crate::config::Config::from_env().unwrap();
    let storage = crate::storage::Storage::new(
        &cfg.s3_endpoint,
        &cfg.s3_region,
        &cfg.s3_bucket,
        &cfg.s3_access_key,
        cfg.s3_secret_key_plain(),
    );
    let exp_cfg = cfg.clone();
    AppState::new(
        pool,
        storage,
        cfg.max_wasm_upload_bytes,
        cfg.presign_ttl_secs,
        "test-only".into(),
        String::new(),
        Arc::new(crate::signing::Signer::from_seed(
            crate::signing::decode_seed(cfg.job_signing_key_plain()).unwrap(),
            cfg.job_signing_kid.clone(),
        )),
        Box::new(move |wall| exp_cfg.token_exp_offset_secs(wall)),
        store,
        cfg.admission(),
        crate::metrics::Metrics::init(),
        Arc::new(cfg.secret_keyring().unwrap()),
        cfg.job_env_exchange_rate_per_min,
        false,
        cfg.public_apps.clone(),
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
    let (first, second) = tokio::join!(
        crate::migrations::run_migrations(&owner),
        crate::migrations::run_migrations(&owner)
    );
    first.unwrap();
    second.unwrap();
    assert_fresh_schema(&owner).await;
    fixture_execute(&owner, r#"
        INSERT INTO tenants (id,slug,name,status,quotas) VALUES ('http','http','HTTP','active','{"max_concurrent_executions":1}'),('other','other','Other','active','{}');
        INSERT INTO components (id,tenant_id,name,ingress_enabled) VALUES ('source','http','source',true),('target','http','target',true);
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256,status)
            VALUES ('source-v1','http','source','1','source/1.wasm','abcd','active'),
                   ('source-v2','http','source','2','source/2.wasm','efab','active'),
                   ('target-v1','http','target','1','target/1.wasm','abcd','active');
        UPDATE components SET active_version_id=id||'-v1';
    "#, vec![]).await.unwrap();
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
    let store = Arc::new(crate::store::InProcStore::new());
    let state = state(pool.clone(), store.clone());

    let unavailable = self::state(pool.clone(), Arc::new(crate::store::FailingStore));
    assert_eq!(
        direct_http::accept(&unavailable, "http", "source", json!({}))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let params = unavailable
        .admission()
        .resolve_for_tenant("http", &Default::default());
    let (decision, reserved) =
        crate::admission::reserve_inflight(&unavailable, "http", &params).await;
    assert!(!reserved);
    match decision {
        crate::admission::Decision::Rejected(r) => {
            assert_eq!(r.into_response().status(), StatusCode::SERVICE_UNAVAILABLE)
        }
        _ => panic!("store failure must reject"),
    }
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions").await, 0);
    println!("PASS shared store failure rejects HTTP without accepting an execution");

    fixture_execute(
        &owner,
        "ALTER TABLE executions ADD CONSTRAINT reject_http_test CHECK(false) NOT VALID",
        vec![],
    )
    .await
    .unwrap();
    assert!(direct_http::accept(&state, "http", "source", json!({}))
        .await
        .is_err());
    assert_eq!(store.peek_inflight("http"), 0);
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
    let response =
        direct_http::accept(&state, "http", "source", json!({"method":"GET","path":"/"}))
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
    assert_eq!(store.peek_inflight("http"), 0);
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions WHERE status='failed' AND error->>'code'='dispatch_not_started'").await, 1);
    let rejected: String = fixture_scalar(&owner, "SELECT id FROM executions", vec![])
        .await
        .unwrap();
    assert!(!direct_http::cancel_pending(&state, "http", &rejected)
        .await
        .unwrap());
    assert_eq!(store.peek_inflight("http"), 0);
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
        "UPDATE executions SET status='pending',finished_at=NULL,error=NULL WHERE id=$1",
        vec![rejected.clone().into()],
    )
    .await
    .unwrap();
    use crate::store::Store as _;
    store.resync_inflight("http", 1, 3600).await.unwrap();
    println!("PASS unavailable Worker releases reservation once without invocation usage; late claim fails");
    assert_eq!(
        direct_http::accept(&state, "http", "source", json!({}))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // A stale Redis reconciliation must never admit more than the DB limit.
    store.resync_inflight("http", 0, 3600).await.unwrap();
    let (a, b) = tokio::join!(
        direct_http::accept(&state, "http", "source", json!({})),
        direct_http::accept(&state, "http", "source", json!({}))
    );
    assert_eq!(a.unwrap().status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(b.unwrap().status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        scalar(&owner, "SELECT count(*) FROM executions WHERE http_request").await,
        1
    );
    store.resync_inflight("http", 1, 3600).await.unwrap();
    println!("PASS HTTP admission / no outbox / active version only / DB concurrency despite stale Redis");

    let id: String = fixture_scalar(&owner, "SELECT id FROM executions", vec![])
        .await
        .unwrap();
    let valid = token(&state, &id, "http", "source-v1");
    let job = direct_http::redeem(State(state.clone()), headers(&valid))
        .await
        .unwrap()
        .0;
    assert_eq!(job.version, "1");
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
    assert_eq!(store.peek_inflight("http"), 1);
    let result = ResultMessage {
        execution_id: id.clone(),
        tenant_id: "http".into(),
        status: ExecutionStatus::Succeeded,
        output: Some(json!({"status":200,"streamed":true})),
        error: None,
        job_token: job.job_token.clone(),
        usage: Some(UsageMetrics {
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
    let mut forged = result.clone();
    forged.tenant_id = "other".into();
    assert!(
        direct_http::complete(State(state.clone()), headers(&job.job_token), Json(forged))
            .await
            .is_err()
    );
    let mut nonterminal = result.clone();
    nonterminal.status = ExecutionStatus::Running;
    assert!(direct_http::complete(
        State(state.clone()),
        headers(&job.job_token),
        Json(nonterminal)
    )
    .await
    .is_err());
    for _ in 0..2 {
        assert_eq!(
            direct_http::complete(
                State(state.clone()),
                headers(&job.job_token),
                Json(result.clone())
            )
            .await
            .unwrap(),
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(store.peek_inflight("http"), 0);
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

    for attempt in 0..16 {
        let raced = format!("dispatch-race-{attempt}");
        fixture_execute(&owner, "INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,http_request) VALUES ($1,'http','source','source-v1','pending','{}',true)", vec![raced.clone().into()]).await.unwrap();
        store.resync_inflight("http", 1, 3600).await.unwrap();
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
        assert_eq!(store.peek_inflight("http"), i64::from(claimed));
        assert!(!direct_http::cancel_pending(&state, "http", &raced)
            .await
            .unwrap());
        // Finish the synthetic claim without executing a guest; preserve no test reservations.
        fixture_execute(&owner, "UPDATE executions SET status='failed',finished_at=now() WHERE id=$1 AND status='running'", vec![raced.clone().into()]).await.unwrap();
        store.resync_inflight("http", 0, 3600).await.unwrap();
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
    assert!(direct_http::accept(&state, "http", "source", json!({}))
        .await
        .is_err());
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
    fixture_execute(&owner, "INSERT INTO components (id,tenant_id,name) VALUES ('delete-test','other','reusable'); INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256,status) VALUES ('delete-v1','other','delete-test','1','delete.wasm','abcd','active'); INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request) VALUES ('delete-pending','other','delete-test','delete-v1','pending',true);", vec![]).await.unwrap();
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
    println!("PASS admin deletion: authentication / RLS / active execution guard / suspended tenant inventory / name reuse / retained history");
}

async fn console_information_regression(state: &AppState) {
    use crate::handlers::{
        configuration::get_function_config,
        executions::{list_executions, ExecutionsQuery},
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
        "active",
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
        1
    );
    assert_eq!(scalar(owner,"SELECT count(*) FROM pg_class c JOIN pg_namespace n ON c.relnamespace=n.oid WHERE n.nspname='public' AND c.relkind='r' AND c.relrowsecurity AND c.relforcerowsecurity").await,13);
    assert_eq!(scalar(owner,"SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND column_name IN ('canary_weight','canary_version_id','chain_depth','routing_reason','idempotency_key')").await,0);
    println!("PASS fresh ORM schema: 15 platform tables, RLS, no removed feature tables, idempotent migration");
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
    fixture_execute(owner, "INSERT INTO users(id,tenant_id,email,password_hash,role) VALUES ('foreign-user','other','foreign@example.invalid','fixture','member')", vec![]).await.unwrap();
    for query in [
        "INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256) VALUES ('foreign-version','other','source','foreign','unused','unused')",
        "INSERT INTO executions(id,tenant_id,component_id,version_id) VALUES ('foreign-execution','other','source','source-v1')",
        "INSERT INTO usage_rollups(tenant_id,period_start,component_id) VALUES ('other',CURRENT_DATE,'source')",
        "INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at) VALUES ('foreign-token','http','foreign-user','fixture',ARRAY['read'],now()+interval '1 hour')",
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
