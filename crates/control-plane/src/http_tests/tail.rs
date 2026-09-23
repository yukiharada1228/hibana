use super::*;
use crate::{
    handlers::tail::{poll, TailQuery},
    store::{RedisStore, Store},
};
use axum::extract::{Path, Query};

#[derive(Default)]
struct PausedTailStore {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl Store for PausedTailStore {
    async fn rate_limit(
        &self,
        _: &str,
        _: crate::store::RateLimitParams,
    ) -> Result<crate::store::RateDecision, crate::store::StoreError> {
        unreachable!("poll fixture does not admit invocations")
    }
    async fn ping(&self) -> Result<(), crate::store::StoreError> {
        Ok(())
    }
    async fn read_tail(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<crate::store::tail::Page, crate::store::StoreError> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(crate::store::StoreError::Unavailable("fixture".into()))
    }
}

async fn database_is_available_while_tail_waits() {
    let pool = hibana_database::postgres::connect(&std::env::var("DATABASE_URL").unwrap(), 1, 0)
        .await
        .unwrap();
    let store = Arc::new(PausedTailStore::default());
    let waiting = state(pool.clone(), store.clone());
    let poll = tokio::spawn(async move {
        poll(
            State(waiting),
            principal("tail"),
            Path("tail-app".into()),
            Query(TailQuery::default()),
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), store.entered.notified())
        .await
        .unwrap();
    // A single-connection pool makes a retained transaction observable. Another
    // operation must complete before Redis recovers, not merely after its timeout.
    let available =
        tokio::time::timeout(std::time::Duration::from_secs(1), inflight(&pool, "tail")).await;
    store.release.notify_one();
    assert_eq!(poll.await.unwrap(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        available.is_ok(),
        "tail held the DB connection while waiting for Redis"
    );
    pool.close().await.unwrap();
}

fn principal(tenant: &str) -> crate::auth::Principal {
    crate::auth::Principal {
        tenant_id: tenant.into(),
        user_id: None,
        token_id: "fixture".into(),
        scopes: vec![hibana_shared::Scope::Read],
        role: hibana_shared::Role::Member,
    }
}

async fn read(
    state: &AppState,
    cursor: Option<&str>,
    filters: &[(&str, &str)],
) -> serde_json::Value {
    let mut value = json!({"cursor": cursor});
    for (key, field) in filters {
        value[*key] = json!(field);
    }
    let response = poll(
        State(state.clone()),
        principal("tail"),
        Path("tail-app".into()),
        Query(serde_json::from_value(value).unwrap()),
    )
    .await
    .unwrap()
    .into_response();
    assert_eq!(response.headers()["cache-control"], "no-store");
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

pub(super) async fn regression(owner: &DatabaseConnection, pool: &DatabaseConnection) {
    let store = Arc::new(
        RedisStore::connect(&std::env::var("REDIS_URL").unwrap())
            .await
            .unwrap(),
    );
    let live = state(pool.clone(), store.clone());
    fixture_execute(
        owner,
        r#"
        INSERT INTO tenants(id,slug,name,status) VALUES ('tail','tail','Tail','active');
        INSERT INTO components(id,tenant_id,name) VALUES ('tail-app','tail','app');
        INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256)
            VALUES ('tail-v1','tail','tail-app','1','unused','abcd');
        INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request,created_at)
            VALUES ('tail-slow','tail','tail-app','tail-v1','running',true,now()-interval '1 hour'),
                   ('tail-fast','tail','tail-app','tail-v1','running',true,now()),
                   ('tail-old','tail','tail-app','tail-v1','succeeded',true,now());
    "#,
        vec![],
    )
    .await
    .unwrap();
    database_is_available_while_tail_waits().await;
    let start = read(&live, None, &[]).await;
    assert_eq!(start["items"], json!([]));
    let initial = start["cursor"].as_str().unwrap();
    for (id, status, stdout) in [
        ("tail-fast", ExecutionStatus::Succeeded, ""),
        ("tail-slow", ExecutionStatus::Failed, "雪 100%_literal"),
    ] {
        let result = ResultMessage {
            execution_id: id.into(),
            tenant_id: "tail".into(),
            status,
            output: Some(json!({"status": 404})),
            error: Some("fixture diagnostic".into()),
            logs: Some(hibana_shared::application_logs::ApplicationLogs {
                stdout: stdout.into(),
                stderr: "stderr-only".into(),
                truncated: false,
            }),
            job_token: token(&live, id, "tail", "tail-v1"),
            usage: Some(UsageMetrics::default()),
        };
        for _ in 0..2 {
            crate::completion::complete(
                State(live.clone()),
                headers(&result.job_token),
                Json(result.clone()),
            )
            .await
            .unwrap();
        }
    }
    // Redis IDs are not authority: foreign execution notifications must not leak data.
    store
        .publish_tail("tail", "tail-app", "logs-001")
        .await
        .unwrap();
    let page = read(&live, Some(initial), &[]).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["execution_id"], "tail-fast");
    assert_eq!(items[1]["execution_id"], "tail-slow");
    assert_eq!(items[1]["logs"]["stdout"], "雪 100%_literal");
    assert_eq!(items[0]["http_status"], 404);
    for item in items {
        for field in ["input", "output", "input_ref", "output_ref"] {
            assert!(item.get(field).is_none());
        }
    }
    assert_eq!(
        read(&live, Some(initial), &[("status", "ok")]).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let errors = read(&live, Some(initial), &[("status", "error")]).await;
    assert_eq!(errors["items"][0]["execution_id"], "tail-slow");
    assert_eq!(
        read(&live, Some(initial), &[("status", "canceled")]).await["items"],
        json!([])
    );
    assert_eq!(
        read(&live, Some(initial), &[("search", "100%_literal")]).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        read(&live, Some(initial), &[("search", "STDERR")]).await["items"],
        json!([])
    );
    assert_eq!(
        read(&live, Some(initial), &[("search", "stderr-only")]).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        read(&live, Some(initial), &[("version_id", "other")]).await["items"],
        json!([])
    );
    assert_eq!(
        read(&live, Some(page["cursor"].as_str().unwrap()), &[]).await["items"],
        json!([])
    );
    assert_eq!(read(&live, None, &[]).await["items"], json!([]));
    let foreign = poll(
        State(live.clone()),
        principal("other"),
        Path("tail-app".into()),
        Query(TailQuery::default()),
    )
    .await
    .err()
    .unwrap()
    .into_response();
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    for invalid in [
        json!({"cursor": "1-0\n"}),
        json!({"status": "404"}),
        json!({"search": "x".repeat(1025)}),
    ] {
        assert_eq!(
            poll(
                State(live.clone()),
                principal("tail"),
                Path("tail-app".into()),
                Query(serde_json::from_value(invalid).unwrap())
            )
            .await
            .err()
            .unwrap()
            .into_response()
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    // Notification failure must not turn a successful durable completion into failure.
    fixture_execute(owner, "INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request) VALUES ('tail-offline','tail','tail-app','tail-v1','running',true)", vec![]).await.unwrap();
    let unavailable = state(pool.clone(), Arc::new(crate::store::FailingStore));
    let result = ResultMessage {
        execution_id: "tail-offline".into(),
        tenant_id: "tail".into(),
        status: ExecutionStatus::Succeeded,
        output: None,
        error: None,
        usage: None,
        logs: Some(Default::default()),
        job_token: token(&unavailable, "tail-offline", "tail", "tail-v1"),
    };
    assert_eq!(
        crate::completion::complete(
            State(unavailable.clone()),
            headers(&result.job_token),
            Json(result)
        )
        .await
        .unwrap(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        poll(
            State(unavailable),
            principal("tail"),
            Path("tail-app".into()),
            Query(TailQuery::default())
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    println!("PASS live tail: shared Redis feed / start boundary / late completion / idempotent publication / RLS / filters / unavailable notification isolation");
}
