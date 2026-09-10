//! PostgreSQL-backed HTTP acceptance, provenance and accounting regression.
use crate::{db, direct_http, state::AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use hibana_shared::{ExecutionStatus, JobClaims, ResultMessage, UsageMetrics};
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
    crate::migrations::test_environment_upgrade(&owner).await;
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
    assert_eq!(store.peek_inflight("http"), 0);
    assert_eq!(scalar(&owner, "SELECT count(*) FROM executions WHERE status='failed' AND error->>'code'='dispatch_not_started'").await, 1);
    let rejected: String = sqlx::query_scalar("SELECT id FROM executions")
        .fetch_one(&owner)
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
        sqlx::query("UPDATE executions SET status='running' WHERE id=$1 AND status='pending'")
            .bind(&rejected)
            .execute(&owner)
            .await
            .unwrap()
            .rows_affected(),
        0
    );
    sqlx::query("UPDATE executions SET status='pending',finished_at=NULL,error=NULL WHERE id=$1")
        .bind(&rejected)
        .execute(&owner)
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

    // Once claimed, a transport failure must not release or rewrite the execution.
    sqlx::query(
        "UPDATE executions SET status='running',started_at=now() WHERE id=$1 AND status='pending'",
    )
    .bind(&id)
    .execute(&owner)
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
    sqlx::query("UPDATE platform_maintenance SET owner='regression' WHERE singleton")
        .execute(&owner)
        .await
        .unwrap();
    assert!(crate::maintenance::enabled(&pool).await.unwrap());
    assert!(
        sqlx::query("UPDATE platform_maintenance SET owner=NULL WHERE singleton")
            .execute(&pool)
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
    sqlx::query("UPDATE platform_maintenance SET owner=NULL WHERE singleton")
        .execute(&owner)
        .await
        .unwrap();
    println!("PASS durable completion remains available with admission closed; runtime role cannot change the gate");

    for attempt in 0..16 {
        let raced = format!("dispatch-race-{attempt}");
        sqlx::query("INSERT INTO executions (id,tenant_id,component_id,version_id,status,input,http_request) VALUES ($1,'http','source','source-v1','pending','{}',true)")
            .bind(&raced).execute(&owner).await.unwrap();
        store.resync_inflight("http", 1, 3600).await.unwrap();
        let claim = async {
            let mut tx = pool.begin().await.unwrap();
            db::set_tenant_guc(&mut tx, "http").await.unwrap();
            let changed = sqlx::query("UPDATE executions SET status='running',started_at=now() WHERE tenant_id='http' AND id=$1 AND status='pending' AND http_request")
                .bind(&raced).execute(&mut *tx).await.unwrap().rows_affected() == 1;
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
        sqlx::query("UPDATE executions SET status='failed',finished_at=now() WHERE id=$1 AND status='running'")
            .bind(&raced).execute(&owner).await.unwrap();
        store.resync_inflight("http", 0, 3600).await.unwrap();
    }
    println!(
        "PASS concurrent Worker claim / dispatch cancellation: one winner, one reservation release"
    );

    let mut tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&mut tx, "http").await.unwrap();
    assert!(db::lock_component(&mut tx, "http", "source").await.unwrap());
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
    assert!(
        !db::switch_active_version(&mut *tx, "http", "source", "target-v1")
            .await
            .unwrap(),
        "activation must reject another component's version"
    );
    sqlx::query("SAVEPOINT deleted_target")
        .execute(&mut *tx)
        .await
        .unwrap();
    db::soft_delete_version(&mut *tx, "http", "source", "source-v2")
        .await
        .unwrap();
    assert!(
        !db::switch_active_version(&mut *tx, "http", "source", "source-v2")
            .await
            .unwrap(),
        "publication must recheck the target after preparation"
    );
    assert_eq!(
        db::find_component_by_id(&mut *tx, "http", "source")
            .await
            .unwrap()
            .unwrap()
            .active_version_id
            .as_deref(),
        Some("source-v1")
    );
    sqlx::query("ROLLBACK TO SAVEPOINT deleted_target")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    println!("PASS version switch / no-op preserves previous / publication rejects deleted and foreign targets");
    secret_resolution_regression(&state).await;

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
    sqlx::raw_sql("INSERT INTO components (id,tenant_id,name) VALUES ('delete-test','other','reusable'); INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256,status) VALUES ('delete-v1','other','delete-test','1','delete.wasm','abcd','active'); INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request) VALUES ('delete-pending','other','delete-test','delete-v1','pending',true);")
        .execute(&owner).await.unwrap();
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
    sqlx::query(
        "UPDATE executions SET status='failed',finished_at=now() WHERE id='delete-pending'",
    )
    .execute(&owner)
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
    let mut tx = pool.begin().await.unwrap();
    db::set_tenant_guc(&mut tx, "other").await.unwrap();
    db::create_component(&mut *tx, "other", "delete-recreated", "reusable")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    println!("PASS admin deletion: authentication / RLS / active execution guard / suspended tenant inventory / name reuse / retained history");
}

async fn secret_resolution_regression(state: &AppState) {
    use crate::secrets::{encrypt, resolve_for_injection, SecretError};
    let mut tx = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&mut tx, "http").await.unwrap();
    db::insert_secret_meta(&mut *tx, "http", "sec-projection", "source", "TOKEN", 2)
        .await
        .unwrap();
    sqlx::query("INSERT INTO version_secret_bindings (tenant_id,component_id,version_id,secret_id,name) VALUES ('http','source','source-v1','sec-projection','TOKEN')")
        .execute(&mut *tx).await.unwrap();
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
        sqlx::query("INSERT INTO function_secret_versions (tenant_id,secret_id,version,kek_kid,wrapped_dek,dek_nonce,nonce,ciphertext,value_len,reason,created_at) VALUES ('http','sec-projection',$1,$2,$3,$4,$5,$6,$7,'rotate',$8)")
            .bind(version).bind(e.kek_kid).bind(e.wrapped_dek).bind(e.dek_nonce)
            .bind(e.nonce).bind(e.ciphertext).bind(e.value_len).bind(time(created))
            .execute(&mut *tx).await.unwrap();
    }
    let allowed = ["TOKEN".into()].into_iter().collect();
    for (at, version, value) in [
        ("2000-01-02T00:00:00Z", 1, "old"),
        ("2000-01-04T00:00:00Z", 2, "new"),
    ] {
        let resolved = resolve_for_injection(
            &mut tx,
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
                &mut tx,
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
        &mut tx,
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
    db::soft_delete_secret(&mut *tx, "http", "sec-projection")
        .await
        .unwrap();
    db::insert_secret_meta(&mut *tx, "http", "sec-reused", "source", "TOKEN", 1)
        .await
        .unwrap();
    assert!(
        resolve_for_injection(
            &mut tx,
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
    sqlx::query("INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request) VALUES ('secret-projection','http','source','source-v1','pending',true)").execute(&mut *tx).await.unwrap();
    let execution = db::secret_execution(&mut *tx, "http", "secret-projection")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.component_id, "source");
    assert_eq!(execution.version_id, "source-v1");
    assert_eq!(execution.status, "pending");
    db::set_tenant_guc(&mut tx, "other").await.unwrap();
    assert!(db::secret_execution(&mut *tx, "http", "secret-projection")
        .await
        .unwrap()
        .is_none());
    assert!(resolve_for_injection(
        &mut tx,
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
