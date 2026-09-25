use super::*;

pub(super) async fn retention_serializes_with_publication(
    state: &AppState,
    owner: &DatabaseConnection,
) {
    let gc = owner.begin().await.unwrap();
    fixture_execute(&gc, "SELECT pg_advisory_xact_lock(1212760641, 1)", vec![])
        .await
        .unwrap();
    let copied = state.clone();
    let mut pin = tokio::spawn(async move {
        crate::artifact_reservations::Reservation::new(
            &copied,
            "http",
            "cache-pin-race",
            "unused",
            &"ab".repeat(32),
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut pin)
            .await
            .is_err(),
        "pin creation must wait for GC before preparing any code"
    );
    gc.commit().await.unwrap();
    let reservation = pin.await.unwrap().unwrap();
    let hashes = hibana_database::postgres::function_rows(
        state.pool(),
        "hibana_protected_artifact_hashes",
        vec![],
    )
    .await
    .unwrap();
    assert!(hashes
        .iter()
        .any(|row| row.try_get::<String>("", "sha256").unwrap() == "ab".repeat(32)));

    let publication = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&publication, "http").await.unwrap();
    reservation.lock(&publication).await.unwrap();
    let concurrent_gc = owner.begin().await.unwrap();
    assert!(
        !fixture_scalar::<bool>(
            &concurrent_gc,
            "SELECT pg_try_advisory_xact_lock(1212760641, 1)",
            vec![]
        )
        .await
        .unwrap(),
        "GC cannot cross the reservation-to-publication handoff"
    );
    publication.commit().await.unwrap();
    assert!(fixture_scalar::<bool>(
        &concurrent_gc,
        "SELECT pg_try_advisory_xact_lock(1212760641, 1)",
        vec![]
    )
    .await
    .unwrap());
    concurrent_gc.commit().await.unwrap();
    // A transaction begun while the reservation was live must not revive it
    // after waiting for the GC guard past its expiry.
    let expired = state.pool().begin().await.unwrap();
    db::set_tenant_guc(&expired, "http").await.unwrap();
    fixture_execute(owner, "UPDATE artifact_reservations SET expires_at=clock_timestamp()+interval '50 milliseconds' WHERE version_id='cache-pin-race'", vec![]).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(reservation.lock(&expired).await.is_err());
    expired.rollback().await.unwrap();
    reservation.finish().await;
    fixture_execute(
        owner,
        "DELETE FROM artifact_reservations WHERE version_id='cache-pin-race'",
        vec![],
    )
    .await
    .unwrap();
    println!("PASS shared GC serializes with deployment pin creation and publication commit across DB connections");
}

pub(super) async fn recovery(state: &AppState, owner: &DatabaseConnection, id: &str) {
    fixture_execute(owner, "UPDATE component_versions SET storage_uri='http/versions/source-v1.wasm',wasm_sha256=repeat('a',64) WHERE id='source-v1'", vec![]).await.unwrap();
    let valid = token(state, id, "http", "source-v1");
    let result = crate::preparation::redeem_execution(State(state.clone()), headers(&valid))
        .await
        .unwrap()
        .0;
    assert_eq!(result.artifact.sha256, "a".repeat(64));
    let claims = state.signer().verify_preparation(&result.token).unwrap();
    assert_eq!(claims.storage_uri, "http/versions/source-v1.wasm");
    assert_eq!(claims.tenant_id, "http");
    assert!(claims.valid_at(chrono::Utc::now().timestamp()));
    for invalid in [
        token(state, id, "other", "source-v1"),
        token(state, id, "http", "source-v2"),
        token(state, "missing", "http", "source-v1"),
        format!("{valid}tampered"),
    ] {
        assert!(
            crate::preparation::redeem_execution(State(state.clone()), headers(&invalid))
                .await
                .is_err()
        );
    }
    for (change, undo) in [
        (
            "UPDATE executions SET status='failed'",
            "UPDATE executions SET status='pending'",
        ),
        (
            "UPDATE executions SET status='running'",
            "UPDATE executions SET status='pending'",
        ),
        (
            "UPDATE executions SET http_request=false",
            "UPDATE executions SET http_request=true",
        ),
        (
            "UPDATE components SET deleted_at=now() WHERE id='source'",
            "UPDATE components SET deleted_at=NULL WHERE id='source'",
        ),
        (
            "UPDATE component_versions SET deleted_at=now() WHERE id='source-v1'",
            "UPDATE component_versions SET deleted_at=NULL WHERE id='source-v1'",
        ),
        (
            "UPDATE tenants SET status='suspended' WHERE id='http'",
            "UPDATE tenants SET status='active' WHERE id='http'",
        ),
    ] {
        fixture_execute(owner, change, vec![]).await.unwrap();
        assert!(
            crate::preparation::redeem_execution(State(state.clone()), headers(&valid))
                .await
                .is_err()
        );
        fixture_execute(owner, undo, vec![]).await.unwrap();
    }
    fixture_execute(owner,"UPDATE component_versions SET storage_uri='source/1.wasm',wasm_sha256='abcd' WHERE id='source-v1'",vec![]).await.unwrap();
    println!("PASS on-demand artifact authorization: pinned version, RLS, tamper, cancellation, claim, deletion and suspension");
}
