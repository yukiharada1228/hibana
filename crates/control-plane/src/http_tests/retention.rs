//! Real PostgreSQL retention, transaction, RLS and accounting boundaries.
use super::*;

async fn tenant_tx(state: &AppState, tenant: &str) -> sea_orm::DatabaseTransaction {
    let tx = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&tx, tenant).await.unwrap();
    tx
}

async fn retained_data(owner: &DatabaseConnection) -> String {
    fixture_scalar(owner, r#"
        SELECT jsonb_build_object(
            'usage', (SELECT jsonb_agg(to_jsonb(t) ORDER BY tenant_id, component_id, period_start) FROM usage_rollups t),
            'audit', (SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM audit_logs t),
            'versions', (SELECT jsonb_agg(to_jsonb(t) ORDER BY id) FROM component_versions t),
            'secrets', (SELECT jsonb_agg(to_jsonb(t) ORDER BY secret_id, version) FROM function_secret_versions t)
        )::text
    "#, vec![]).await.unwrap()
}

pub(super) async fn regression(owner: &DatabaseConnection, state: &AppState) {
    fixture_execute(owner, r#"
        INSERT INTO tenants (id,slug,name,status) VALUES
            ('history','history','History','active'),
            ('history-other','history-other','Other history','active');
        INSERT INTO components (id,tenant_id,name)
            SELECT id||'-app',id,'app' FROM tenants WHERE id IN ('history','history-other');
        INSERT INTO component_versions (id,tenant_id,component_id,version,storage_uri,wasm_sha256)
            SELECT id||'-v1',id,id||'-app','1','unused','abcd' FROM tenants WHERE id IN ('history','history-other');
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at)
            SELECT 'history-old-'||i,'history','history-app','history-v1',
                (ARRAY['succeeded','failed','timeout'])[1+i%3],true,
                now()-interval '41 days',now()-interval '40 days'
            FROM generate_series(1,501) AS i;
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at)
            VALUES ('history-other-old','history-other','history-other-app','history-other-v1','succeeded',true,
                    now()-interval '41 days',now()-interval '40 days');
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at)
            SELECT 'history-keep-'||id,'history','history-app','history-v1',status,true,now()-interval '41 days',finished
            FROM (VALUES
                ('pending','pending',NULL::timestamptz),
                ('running','running',NULL::timestamptz),
                ('running-with-timestamp','running',now()-interval '40 days'),
                ('missing-finish','succeeded',NULL::timestamptz),
                ('recent','failed',now()-interval '29 days'),
                ('just-finished','timeout',now()),
                ('future','succeeded',now()+interval '1 day')
            ) AS rows(id,status,finished);
        INSERT INTO usage_rollups (tenant_id,component_id,period_start,invocation_count,succeeded_count)
            VALUES ('history','history-app',current_date-40,501,501);
        INSERT INTO audit_logs (tenant_id,actor,action,target,created_at)
            VALUES ('history','fixture','retention.fixture','history-app',now()-interval '60 days');
    "#, vec![]).await.unwrap();
    let preserved = retained_data(owner).await;
    let keys = db::secrets_kek_kid_counts_all(state.pool()).await.unwrap();

    let tx = tenant_tx(state, "history-other").await;
    assert_eq!(
        db::purge_expired_executions(&tx, "history", 30)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
    let tx = tenant_tx(state, "history").await;
    assert_eq!(
        db::purge_expired_executions(&tx, "history", 0)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();

    // A completion holding a row lock must not stall cleanup of other history.
    let held = tenant_tx(state, "history").await;
    fixture_execute(
        &held,
        "SELECT id FROM executions WHERE id='history-old-1' FOR UPDATE",
        vec![],
    )
    .await
    .unwrap();
    let first = tenant_tx(state, "history").await;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            db::purge_expired_executions(&first, "history", 30)
        )
        .await
        .unwrap()
        .unwrap(),
        500
    );
    let second = tenant_tx(state, "history").await;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            db::purge_expired_executions(&second, "history", 30)
        )
        .await
        .unwrap()
        .unwrap(),
        0
    );
    second.commit().await.unwrap();
    first.rollback().await.unwrap();
    held.commit().await.unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE id LIKE 'history-old-%'"
        )
        .await,
        501
    );

    // Two CP transactions take disjoint batches without waiting for a commit.
    let first = tenant_tx(state, "history").await;
    let second = tenant_tx(state, "history").await;
    assert_eq!(
        db::purge_expired_executions(&first, "history", 30)
            .await
            .unwrap(),
        500
    );
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            db::purge_expired_executions(&second, "history", 30)
        )
        .await
        .unwrap()
        .unwrap(),
        1
    );
    second.rollback().await.unwrap();
    first.commit().await.unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE id LIKE 'history-old-%'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE id='history-other-old'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE id LIKE 'history-keep-%'"
        )
        .await,
        7
    );

    // Boundary uses completion time and DB time, not creation time or local clock.
    let tx = tenant_tx(state, "history").await;
    fixture_execute(&tx, r#"
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at)
        VALUES ('history-boundary','history','history-app','history-v1','succeeded',true,now()-interval '31 days',now()-interval '30 days');
        UPDATE executions SET finished_at=now()-interval '30 days'+interval '1 microsecond' WHERE id LIKE 'history-old-%';
    "#, vec![]).await.unwrap();
    assert_eq!(
        db::purge_expired_executions(&tx, "history", 30)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        fixture_scalar::<i64>(
            &tx,
            "SELECT count(*) FROM executions WHERE id LIKE 'history-old-%'",
            vec![]
        )
        .await
        .unwrap(),
        1
    );
    tx.rollback().await.unwrap();

    // Bounded passes, suspended tenants, and a disabled orphan sweeper.
    fixture_execute(owner, r#"
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at,finished_at)
            SELECT 'history-bulk-'||i,'history','history-app','history-v1','succeeded',true,
                now()-interval '41 days',now()-interval '40 days' FROM generate_series(1,5000) AS i;
        UPDATE tenants SET status='suspended' WHERE id='history';
    "#, vec![]).await.unwrap();
    let deleted = state.metrics().execution_history_deleted_total.get();
    crate::reaper::reconcile_once(state, 0, 0).await.unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE tenant_id='history'"
        )
        .await,
        5008
    );
    assert_eq!(
        state.metrics().execution_history_deleted_total.get(),
        deleted
    );
    crate::reaper::reconcile_once(state, 0, 30).await.unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE tenant_id='history'"
        )
        .await,
        8
    );
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE id='history-other-old'"
        )
        .await,
        0
    );
    assert_eq!(
        state.metrics().execution_history_deleted_total.get(),
        deleted + 5001
    );
    crate::reaper::reconcile_once(state, 0, 30).await.unwrap();
    assert_eq!(
        scalar(
            owner,
            "SELECT count(*) FROM executions WHERE tenant_id='history'"
        )
        .await,
        7
    );
    assert_eq!(
        state.metrics().execution_history_deleted_total.get(),
        deleted + 5002
    );
    assert_eq!(retained_data(owner).await, preserved);
    assert_eq!(
        db::secrets_kek_kid_counts_all(state.pool()).await.unwrap(),
        keys
    );

    // The history endpoint reports a removed execution as missing, not an error.
    let principal = crate::auth::Principal {
        tenant_id: "history".into(),
        user_id: None,
        token_id: "fixture".into(),
        scopes: vec![hibana_shared::Scope::Read],
        role: hibana_shared::Role::Member,
    };
    assert_eq!(
        crate::handlers::executions::get_execution(
            State(state.clone()),
            principal,
            axum::extract::Path("history-old-1".into())
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );
    completion_after_retention(owner, state).await;
    println!("PASS execution history retention: expiry boundary, 500-row transactions / 5000-row passes, SKIP LOCKED concurrency, rollback, RLS, suspension, disable switch, preserved usage/audit/versions/Secret keys and completion retries");
}

async fn completion_after_retention(owner: &DatabaseConnection, state: &AppState) {
    fixture_execute(owner, r#"
        INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request,created_at)
        VALUES ('history-completion','history','history-app','history-v1','running',true,now()-interval '40 days');
    "#, vec![]).await.unwrap();
    let result = ResultMessage {
        execution_id: "history-completion".into(),
        tenant_id: "history".into(),
        status: ExecutionStatus::Succeeded,
        output: None,
        error: None,
        usage: None,
        logs: None,
        job_token: token(state, "history-completion", "history", "history-v1"),
    };
    crate::completion::complete(
        State(state.clone()),
        headers(&result.job_token),
        Json(result.clone()),
    )
    .await
    .unwrap();
    let preserved = retained_data(owner).await;
    let tx = tenant_tx(state, "history").await;
    assert_eq!(
        db::purge_expired_executions(&tx, "history", 30)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
    // Even a very old admission is retained for the full period after completion.
    crate::completion::complete(
        State(state.clone()),
        headers(&result.job_token),
        Json(result.clone()),
    )
    .await
    .unwrap();
    fixture_execute(
        owner,
        "UPDATE executions SET finished_at=now()-interval '31 days' WHERE id='history-completion'",
        vec![],
    )
    .await
    .unwrap();
    let tx = tenant_tx(state, "history").await;
    assert_eq!(
        db::purge_expired_executions(&tx, "history", 30)
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
    assert_eq!(
        crate::completion::complete(
            State(state.clone()),
            headers(&result.job_token),
            Json(result)
        )
        .await
        .err()
        .unwrap()
        .into_response()
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(retained_data(owner).await, preserved);
}
