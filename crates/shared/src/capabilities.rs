//! Persisted version capabilities shared by the Control Plane and Worker.
//!
//! Legacy import arrays never grant environment or network access. Missing or
//! malformed fields and invalid entries are ignored, leaving no implicit grants.
use std::collections::BTreeSet;

/// Server-managed imports, environment bindings and approved outbound targets.
/// Import permission alone does not grant access to environment values or sockets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    /// strict matching を通過した承認済み import 名。
    pub imports: Vec<String>,
    /// Environment names bound by the deployment. Empty means no injection.
    pub env: BTreeSet<String>,
    /// Administrator-approved `host:port` targets. Empty denies all outbound access.
    pub net_allow_outbound: BTreeSet<String>,
}

impl CapabilitySet {
    /// 保存形（JSONB）へ変換する。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "imports": self.imports,
            "env": self.env,
            "net_allow_outbound": self.net_allow_outbound,
        })
    }

    /// Parsed targets for the runtime's DNS and address checks. Invalid entries
    /// remain excluded even if a caller constructed the set without parsing JSON.
    pub fn outbound_endpoints(&self) -> Vec<crate::egress::EgressEndpoint> {
        self.net_allow_outbound
            .iter()
            .filter_map(|value| crate::egress::parse_egress_endpoint(value).ok())
            .collect()
    }
}

/// `capabilities` JSONB を読む（後方互換つき・**壊れた値は deny-all**）。純関数。
pub fn parse_capabilities(value: &serde_json::Value) -> CapabilitySet {
    fn string_list(v: Option<&serde_json::Value>) -> Vec<String> {
        v.and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    match value {
        // 旧形式（M6 まで）: 承認済み import 名の素の配列。env は空 = deny-all。
        serde_json::Value::Array(_) => CapabilitySet {
            imports: string_list(Some(value)),
            env: BTreeSet::new(),
            net_allow_outbound: BTreeSet::new(),
        },
        serde_json::Value::Object(map) => CapabilitySet {
            imports: string_list(map.get("imports")),
            // 許可リストは env 名として妥当なものだけを採る（DB が壊れていても不正名は入れない）。
            env: string_list(map.get("env"))
                .into_iter()
                .filter(|k| crate::is_valid_env_key(k))
                .collect(),
            // egress も「パースできる host:port だけ」を採る（壊れた値は落として fail-closed）。
            net_allow_outbound: string_list(map.get("net_allow_outbound"))
                .into_iter()
                .filter(|e| crate::egress::parse_egress_endpoint(e).is_ok())
                .collect(),
        },
        // null / 数値 / 文字列 / パース不能 → deny-all（fail-closed）。
        _ => CapabilitySet::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// egress allowlist は host:port として妥当なものだけを round-trip する（壊れた値は落とす）。
    #[test]
    fn egress_allowlist_round_trips_valid_entries_only() {
        let caps = CapabilitySet {
            imports: vec![],
            env: BTreeSet::new(),
            net_allow_outbound: ["api.example.com:443", "not a valid entry", "1.2.3.4:8080"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let parsed = parse_capabilities(&caps.to_json());
        assert!(parsed.net_allow_outbound.contains("api.example.com:443"));
        assert!(parsed.net_allow_outbound.contains("1.2.3.4:8080"));
        assert!(
            !parsed.net_allow_outbound.contains("not a valid entry"),
            "malformed egress entries must be dropped (fail-closed)"
        );
        assert_eq!(parsed.outbound_endpoints(), caps.outbound_endpoints());
        assert_eq!(
            parsed.outbound_endpoints(),
            vec![
                crate::egress::EgressEndpoint {
                    host: "1.2.3.4".into(),
                    port: 8080
                },
                crate::egress::EgressEndpoint {
                    host: "api.example.com".into(),
                    port: 443
                },
            ]
        );
    }

    /// 旧形式（承認済み import 名の素の配列）は `imports` として読め、`env` は空 = deny-all。
    /// 読み取り時に互換形式として解釈する。
    #[test]
    fn capabilities_legacy_array_is_read_as_imports_with_no_env() {
        let legacy = serde_json::json!(["wasi:cli/environment@0.2.0", "wasi:io/streams@0.2.6"]);
        let caps = parse_capabilities(&legacy);
        assert_eq!(caps.imports.len(), 2);
        assert!(
            caps.env.is_empty(),
            "legacy rows must never grant env injection"
        );
        assert!(caps.outbound_endpoints().is_empty());
    }

    /// 新形式はそのまま読める。
    #[test]
    fn capabilities_object_form_roundtrips() {
        let v = serde_json::json!({
            "imports": ["wasi:cli/environment@0.2.0", null, 42],
            "env": ["API_KEY", "API_KEY"],
            "net_allow_outbound": ["[2606:4700:4700::1111]:443", "[2606:4700:4700::1111]:443", null, 42]
        });
        let caps = parse_capabilities(&v);
        assert_eq!(caps.imports, vec!["wasi:cli/environment@0.2.0".to_string()]);
        assert!(caps.env.contains("API_KEY"));
        assert_eq!(caps.env.len(), 1);
        assert_eq!(
            caps.outbound_endpoints(),
            vec![crate::egress::EgressEndpoint {
                host: "2606:4700:4700::1111".into(),
                port: 443,
            }]
        );
        // to_json → parse で往復する。
        assert_eq!(parse_capabilities(&caps.to_json()), caps);
    }

    /// 壊れた値は **deny-all** に倒れる（fail-closed）。
    #[test]
    fn capabilities_broken_value_is_deny_all() {
        for v in [
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!(false),
            serde_json::json!("nope"),
            serde_json::json!({}),
            serde_json::json!({"imports": "not-a-list", "env": 7, "net_allow_outbound": "example.com:443"}),
            serde_json::json!({"env": [false, null, 42], "net_allow_outbound": [false, null, 42, "example.com:0"]}),
        ] {
            let caps = parse_capabilities(&v);
            assert!(caps.imports.is_empty(), "broken value must deny imports");
            assert!(caps.env.is_empty(), "broken value must deny env");
            assert!(
                caps.outbound_endpoints().is_empty(),
                "broken value must deny outbound access"
            );
        }
    }

    /// DB 側が壊れて不正な env 名が入っていても、読み取りで落とす。
    #[test]
    fn capabilities_drops_malformed_env_names() {
        let v = serde_json::json!({"imports": [], "env": ["OK_NAME", "bad-name", "", "9LEAD"]});
        let caps = parse_capabilities(&v);
        assert_eq!(caps.env.len(), 1);
        assert!(caps.env.contains("OK_NAME"));
    }
}
