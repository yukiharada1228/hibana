//! PostgreSQL-backed HTTP acceptance, provenance and accounting regression.
use crate::{db, direct_http, state::AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use faas_shared::{ExecutionStatus, JobClaims, ResultMessage, UsageMetrics};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
fn state(pool: PgPool, store: Arc<dyn crate::store::Store>) -> AppState {
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
        None,
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
async fn scalar(owner: &PgPool, query: &str) -> i64 {
    sqlx::query_scalar(query).fetch_one(owner).await.unwrap()
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL; bash scripts/test-http.sh"]
async fn http_mvp_regression() {
    let url = std::env::var("HTTP_TEST_DATABASE_URL").unwrap();
    assert!(
        url.ends_with("/hibana_http"),
        "refusing a non-test database"
    );
    let owner = PgPool::connect(&url).await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('public.tenants') IS NULL")
            .fetch_one(&owner)
            .await
            .unwrap(),
        "refusing a nonempty database"
    );
    crate::migrations::run_migrations(&owner).await.unwrap();
    sqlx::raw_sql(r#"
        INSERT INTO tenants (id,slug,name,status,quotas) VALUES ('http','http','HTTP','active','{"max_concurrent_executions":1}'),('other','other','Other','active','{}');
        INSERT INTO components (id,tenant_id,name,ingress_enabled) VALUES ('source','http','source',true),('target','http','target',true);
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256,status)
            VALUES ('source-v1','http','source','1','source/1.wasm','abcd','active'),
                   ('source-v2','http','source','2','source/2.wasm','efab','active'),
                   ('target-v1','http','target','1','target/1.wasm','abcd','active');
        UPDATE components SET active_version_id=id||'-v1';
        UPDATE components SET canary_version_id='source-v2',canary_weight=100 WHERE id='source';
    "#).execute(&owner).await.unwrap();
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    crate::migrations::assert_non_privileged_runtime_role(&pool)
        .await
        .unwrap();
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

    sqlx::query("ALTER TABLE executions ADD CONSTRAINT reject_http_test CHECK(false) NOT VALID")
        .execute(&owner)
        .await
        .unwrap();
    assert!(direct_http::accept(&state, "http", "source", json!({}))
        .await
        .is_err());
    assert_eq!(store.peek_inflight("http"), 0);
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions").await, 0);
    sqlx::query("ALTER TABLE executions DROP CONSTRAINT reject_http_test")
        .execute(&owner)
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
        scalar(&owner, "SELECT count(*) FROM execution_outbox").await,
        0
    );
    assert_eq!(store.peek_inflight("http"), 1);
    assert_eq!(
        direct_http::accept(&state, "http", "source", json!({}))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // A stale Redis reconciliation must never admit more than the DB limit.
    use crate::store::Store as _;
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

    let id: String = sqlx::query_scalar("SELECT id FROM executions")
        .fetch_one(&owner)
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
    sqlx::query("UPDATE executions SET http_request=false WHERE id=$1")
        .bind(&id)
        .execute(&owner)
        .await
        .unwrap();
    assert!(direct_http::redeem(State(state.clone()), headers(&valid))
        .await
        .is_err());
    sqlx::query("UPDATE executions SET http_request=true WHERE id=$1")
        .bind(&id)
        .execute(&owner)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&mut tx, "other").await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM executions")
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
        0
    );
    assert!(
        sqlx::query("UPDATE executions SET tenant_id='other' WHERE id=$1")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected()
            == 0
    );
    tx.commit().await.unwrap();
    println!("PASS signed redemption / version pin / historical job rejection / FORCE RLS");

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

    let mut tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&mut tx, "http").await.unwrap();
    assert!(
        db::switch_active_version(&mut *tx, "http", "source", "source-v2")
            .await
            .unwrap()
    );
    assert!(
        db::switch_active_version(&mut *tx, "http", "source", "source-v2")
            .await
            .unwrap()
    );
    assert_eq!(
        db::rollback_active_version(&mut *tx, "http", "source", None)
            .await
            .unwrap()
            .unwrap()
            .0,
        "source-v1"
    );
    assert!(
        db::rollback_active_version(&mut *tx, "http", "source", Some("target-v1"))
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    println!("PASS version switch / no-op preserves previous / rollback rejects another component");

    sqlx::query("UPDATE tenants SET status='suspended' WHERE id='http'")
        .execute(&owner)
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
}
