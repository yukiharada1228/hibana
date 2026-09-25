use super::*;

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
