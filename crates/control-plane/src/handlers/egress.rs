//! Administrator-approved application egress, shared by every version.
use crate::{
    auth::Principal, authz::require_admin_role, db, error::AppError, extract::JsonBody,
    state::AppState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use hibana_shared::FaasError;
use sea_orm::TransactionTrait as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeSet, net::Ipv6Addr};

const MAX_ENDPOINTS: usize = 64;

#[derive(Serialize)]
pub struct EgressPolicy {
    allow_outbound: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeEgress {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

pub fn policy(component: &db::ComponentRow) -> Result<BTreeSet<String>, AppError> {
    BTreeSet::deserialize(&component.egress_policy).map_err(Into::into)
}

pub(crate) fn normalize(endpoints: &[String]) -> Result<BTreeSet<String>, FaasError> {
    if endpoints.len() > MAX_ENDPOINTS {
        return Err(FaasError::InvalidRequest(
            "At most 64 egress destinations are allowed".into(),
        ));
    }
    endpoints
        .iter()
        .map(|raw| {
            let invalid = || {
                FaasError::InvalidRequest(
                    "Use a hostname:port or [IPv6]:port, without a URL, credentials or wildcard"
                        .into(),
                )
            };
            if raw.len() > 300 {
                return Err(invalid());
            }
            let ep = hibana_shared::egress::parse_egress_endpoint(raw).map_err(|_| invalid())?;
            if ep.host.contains(':') || raw.trim().starts_with('[') {
                let ip = ep.host.parse::<Ipv6Addr>().map_err(|_| invalid())?;
                return Ok(format!("[{ip}]:{}", ep.port));
            }
            let host = ep.host.trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty()
                || host.len() > 253
                || !host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err(invalid());
            }
            Ok(format!("{host}:{}", ep.port))
        })
        .collect()
}

pub async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<Json<EgressPolicy>, AppError> {
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &principal.tenant_id).await?;
    let component = db::find_component_by_id(&tx, &principal.tenant_id, &id)
        .await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let allow_outbound = policy(&component)?;
    tx.commit().await?;
    Ok(Json(EgressPolicy { allow_outbound }))
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    JsonBody(change): JsonBody<ChangeEgress>,
) -> Result<Json<EgressPolicy>, AppError> {
    require_admin_role(principal.role)?;
    let allow = normalize(&change.allow)?;
    let deny = normalize(&change.deny)?;
    if (allow.is_empty() && deny.is_empty()) || !allow.is_disjoint(&deny) {
        return Err(FaasError::InvalidRequest(
            "Specify destinations to allow or deny; a destination cannot appear in both".into(),
        )
        .into());
    }
    let tenant = &principal.tenant_id;
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, tenant).await?;
    if !db::lock_component(&tx, tenant, &id).await? {
        return Err(FaasError::NotFound("component".into()).into());
    }
    let component = db::find_component_by_id(&tx, tenant, &id)
        .await?
        .ok_or_else(|| FaasError::NotFound("component".into()))?;
    let before = policy(&component)?;
    let mut approved = before.clone();
    approved.extend(allow);
    approved.retain(|endpoint| !deny.contains(endpoint));
    if approved.len() > MAX_ENDPOINTS {
        return Err(
            FaasError::InvalidRequest("At most 64 egress destinations are allowed".into()).into(),
        );
    }
    db::set_component_egress(&tx, tenant, &id, &approved).await?;
    db::insert_audit_log(
        &tx,
        tenant,
        principal.actor(),
        "component_egress_updated",
        Some(&id),
        Some(&json!({"before": before, "allow_outbound": approved})),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(EgressPolicy {
        allow_outbound: approved,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_are_bounded_and_canonical_without_echoing_credentials() {
        let values = [
            " DB.Example.COM.:05432 ",
            "db.example.com:5432",
            "[2606:4700:4700:0:0:0:0:1111]:443",
        ]
        .map(str::to_owned);
        assert_eq!(
            normalize(&values).unwrap(),
            ["db.example.com:5432", "[2606:4700:4700::1111]:443"]
                .map(str::to_owned)
                .into()
        );
        for value in [
            "",
            "https://db.example:443",
            "user:secret@db:5432",
            "*.example:443",
            "a..b:443",
            "[example]:443",
            "[::g]:443",
            "db:0",
            "db:65536",
            "db/path:443",
            "a b:443",
        ] {
            let error = normalize(&[value.into()]).unwrap_err().to_string();
            assert!(!error.contains("secret@"));
        }
        assert!(normalize(&vec!["a:443".into(); 65]).is_err());
        assert!(normalize(&[]).unwrap().is_empty());
    }
}
