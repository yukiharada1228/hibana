//! Environment grants shared by the Control Plane and Worker.
//!
//! Missing or malformed fields and invalid entries are ignored, leaving no
//! implicit grants. Only the current object representation is accepted.
use serde_json::Value;
use std::collections::BTreeSet;

/// Read only valid environment bindings fixed at deployment. Import metadata
/// does not grant environment access; outbound targets belong to the application.
pub fn allowed_env(capabilities: &Value) -> BTreeSet<String> {
    capabilities
        .get("env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| crate::is_valid_env_key(key))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_grants_are_deduplicated_and_ignore_other_metadata() {
        let v = serde_json::json!({
            "imports": ["wasi:cli/environment@0.2.0", null, 42],
            "env": ["API_KEY", "API_KEY"],
            "net_allow_outbound": ["[2606:4700:4700::1111]:443", "[2606:4700:4700::1111]:443", null, 42]
        });
        assert_eq!(allowed_env(&v), BTreeSet::from(["API_KEY".into()]));
    }

    /// 壊れた値は **deny-all** に倒れる（fail-closed）。
    #[test]
    fn capabilities_broken_value_is_deny_all() {
        for v in [
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!(false),
            serde_json::json!("nope"),
            serde_json::json!(["wasi:cli/environment@0.2.0"]),
            serde_json::json!({}),
            serde_json::json!({"imports": "not-a-list", "env": 7, "net_allow_outbound": "example.com:443"}),
            serde_json::json!({"env": [false, null, 42], "net_allow_outbound": [false, null, 42, "example.com:0"]}),
        ] {
            assert!(allowed_env(&v).is_empty(), "broken value must deny env");
        }
    }

    /// DB 側が壊れて不正な env 名が入っていても、読み取りで落とす。
    #[test]
    fn capabilities_drops_malformed_env_names() {
        let v = serde_json::json!({"env": ["OK_NAME", "bad-name", "", "9LEAD", null, false, 42]});
        assert_eq!(allowed_env(&v), BTreeSet::from(["OK_NAME".into()]));
    }
}
