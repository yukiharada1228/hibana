//! Bounded, informational build declarations carried inside the signed artifact.
//! Neither extension names nor requested permissions grant host capabilities.
use std::collections::{BTreeMap, BTreeSet};

use hibana_shared::FaasError;
use serde::{Deserialize, Serialize};
use wasmparser::{Parser, Payload};

const SECTION: &str = "hibana:build";
const MAX_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildMetadata {
    schema_version: u32,
    input: BuildInput,
    roots: Vec<String>,
    extensions: Vec<Extension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BuildInput {
    Javascript,
    Component,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Extension {
    name: String,
    version: Option<String>,
    dependencies: Vec<String>,
    permissions: Vec<String>,
}

fn invalid() -> FaasError {
    // JSON parse errors may quote arbitrary user-supplied text.
    FaasError::InvalidRequest("invalid Hibana build metadata".into())
}

fn name_valid(name: &str) -> bool {
    if name.starts_with("./") {
        return name.len() <= 512
            && !name.chars().any(char::is_control)
            && !name.contains('\\')
            && !name.split('/').any(|part| part == "..");
    }
    let part_valid = |part: &str| {
        part.as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && part
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
    };
    name.len() <= 214
        && if let Some(scoped) = name.strip_prefix('@') {
            scoped
                .split_once('/')
                .is_some_and(|(scope, name)| part_valid(scope) && part_valid(name))
        } else {
            part_valid(name)
        }
}

fn unique(values: &[String]) -> bool {
    values.len() <= 64 && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

impl BuildMetadata {
    fn validate(&self) -> Result<(), FaasError> {
        if self.schema_version != 1 || self.extensions.len() > 64 || !unique(&self.roots) {
            return Err(invalid());
        }
        let nodes: BTreeMap<_, _> = self
            .extensions
            .iter()
            .map(|e| (e.name.as_str(), e))
            .collect();
        if nodes.len() != self.extensions.len() {
            return Err(invalid());
        }
        for extension in &self.extensions {
            if !name_valid(&extension.name)
                || !unique(&extension.dependencies)
                || extension.permissions.len() > 1
                || extension
                    .permissions
                    .iter()
                    .any(|p| p != "outbound-network")
                || extension.version.as_ref().is_some_and(|v| {
                    v.is_empty()
                        || v.len() > 128
                        || !v
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b".+-".contains(&c))
                })
            {
                return Err(invalid());
            }
        }
        fn visit<'a>(
            name: &'a str,
            nodes: &BTreeMap<&'a str, &'a Extension>,
            visiting: &mut BTreeSet<&'a str>,
            visited: &mut BTreeSet<&'a str>,
        ) -> Result<(), FaasError> {
            if visited.contains(name) {
                return Ok(());
            }
            let extension = nodes.get(name).ok_or_else(invalid)?;
            if !visiting.insert(name) {
                return Err(invalid());
            }
            for dependency in &extension.dependencies {
                visit(dependency, nodes, visiting, visited)?;
            }
            visiting.remove(name);
            visited.insert(name);
            Ok(())
        }
        let mut visited = BTreeSet::new();
        for root in &self.roots {
            visit(root, &nodes, &mut BTreeSet::new(), &mut visited)?;
        }
        if visited.len() != nodes.len() {
            return Err(invalid());
        }
        Ok(())
    }
}

/// Read only the outer component's declaration, after normal Wasm validation.
pub fn extract(bytes: &[u8]) -> Result<Option<BuildMetadata>, FaasError> {
    let mut depth = 0i32;
    let mut metadata = None;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|_| invalid())? {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => depth -= 1,
            Payload::CustomSection(section) if depth == 0 && section.name() == SECTION => {
                if metadata.is_some() || section.data().len() > MAX_BYTES {
                    return Err(invalid());
                }
                let parsed: BuildMetadata =
                    serde_json::from_slice(section.data()).map_err(|_| invalid())?;
                parsed.validate()?;
                metadata = Some(parsed);
            }
            _ => {}
        }
    }
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> serde_json::Value {
        json!({"schema_version":1,"input":"javascript","roots":["./database"],"extensions":[
            {"name":"./database","version":null,"dependencies":["@example/tcp"],"permissions":[]},
            {"name":"@example/tcp","version":"1.0.0","dependencies":[],"permissions":["outbound-network"]}
        ]})
    }
    fn validate(value: serde_json::Value) -> bool {
        serde_json::from_value::<BuildMetadata>(value).is_ok_and(|m| m.validate().is_ok())
    }
    #[test]
    fn graph_is_bounded_and_requires_resolved_reachable_acyclic_dependencies() {
        assert!(validate(sample()));
        let mut missing = sample();
        missing["extensions"][0]["dependencies"] = json!(["missing"]);
        assert!(!validate(missing));
        let mut cycle = sample();
        cycle["extensions"][1]["dependencies"] = json!(["./database"]);
        assert!(!validate(cycle));
        let mut duplicate = sample();
        duplicate["extensions"][1]["name"] = json!("./database");
        assert!(!validate(duplicate));
        let mut orphan = sample();
        orphan["roots"] = json!([]);
        assert!(!validate(orphan));
        let mut grant = sample();
        grant["extensions"][1]["permissions"] = json!(["filesystem"]);
        assert!(!validate(grant));
        let mut unknown = sample();
        unknown["net_allow_outbound"] = json!(["*:*"]);
        assert!(!validate(unknown));
        for name in [
            "/private/path",
            "../secret",
            "./../secret",
            "https://registry.invalid/pkg",
            "./bad\npath",
        ] {
            let mut unsafe_name = sample();
            unsafe_name["extensions"][0]["name"] = json!(name);
            unsafe_name["roots"] = json!([name]);
            assert!(!validate(unsafe_name));
        }
        assert!(validate(
            json!({"schema_version":1,"input":"javascript","roots":[],"extensions":[]})
        ));
    }

    fn length(mut n: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let next = n >> 7;
            bytes.push((n as u8 & 127) | if next != 0 { 128 } else { 0 });
            if next == 0 {
                return bytes;
            }
            n = next;
        }
    }
    fn section(data: &[u8]) -> Vec<u8> {
        let mut payload = length(SECTION.len());
        payload.extend_from_slice(SECTION.as_bytes());
        payload.extend_from_slice(data);
        let mut result = vec![0];
        result.extend(length(payload.len()));
        result.extend(payload);
        result
    }
    #[test]
    fn only_outer_metadata_is_recorded_and_bad_sections_are_rejected_without_echoing_values() {
        let header = [0, 97, 115, 109, 13, 0, 1, 0];
        let declaration = section(&serde_json::to_vec(&sample()).unwrap());
        let mut component = header.to_vec();
        component.extend(&declaration);
        assert!(extract(&component).unwrap().is_some());
        assert!(extract(&header).unwrap().is_none());
        let mut nested = header.to_vec();
        nested.push(4);
        nested.extend(length(component.len()));
        nested.extend(&component);
        assert!(extract(&nested).unwrap().is_none());
        nested.extend(&declaration);
        assert!(extract(&nested).unwrap().is_some());
        component.extend(&declaration);
        assert!(extract(&component).is_err());
        for data in [
            vec![b' '; MAX_BYTES + 1],
            b"{\"secret\":\"do-not-print\",broken}".to_vec(),
        ] {
            let mut bad = header.to_vec();
            bad.extend(section(&data));
            let error = extract(&bad).unwrap_err().to_string();
            assert!(error.contains("invalid Hibana build metadata"));
            assert!(!error.contains("do-not-print"));
        }
    }
}
