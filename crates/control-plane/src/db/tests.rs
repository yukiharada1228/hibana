use super::{saturating_i64, TenantQuotaOverrides};

#[test]
fn tenant_quota_overrides_empty_object_is_all_none() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(v.invoke_rate_per_sec.is_none());
    assert!(v.invoke_burst.is_none());
    assert!(v.max_concurrent_executions.is_none());
}

#[test]
fn tenant_quota_overrides_reads_known_ignores_unknown() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
        "invoke_rate_per_sec": 100,
        "invoke_burst": 1000,
        "max_concurrent_executions": 50,
        "future_field_we_dont_know": "ignored",
    }))
    .unwrap();
    assert_eq!(v.invoke_rate_per_sec, Some(100));
    assert_eq!(v.invoke_burst, Some(1000));
    assert_eq!(v.max_concurrent_executions, Some(50));
}

#[test]
fn tenant_quota_overrides_partial_override() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
        "invoke_rate_per_sec": 200,
    }))
    .unwrap();
    assert_eq!(v.invoke_rate_per_sec, Some(200));
    assert!(v.invoke_burst.is_none());
    assert!(v.max_concurrent_executions.is_none());
}

#[test]
fn tenant_quota_overrides_null_means_inherit() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
        "invoke_rate_per_sec": null,
        "max_concurrent_executions": 10,
    }))
    .unwrap();
    assert!(v.invoke_rate_per_sec.is_none());
    assert_eq!(v.max_concurrent_executions, Some(10));
}

#[test]
fn saturating_i64_clamps_at_i64_max() {
    assert_eq!(saturating_i64(0), 0);
    assert_eq!(saturating_i64(123), 123);
    assert_eq!(saturating_i64(i64::MAX as u64), i64::MAX);
    assert_eq!(saturating_i64(u64::MAX), i64::MAX);
}
