//! OIDC code flow with independent PKCE protection for the provider and the
//! browser/CLI handoff. Provider tokens never become Hibana API credentials.
pub mod config;
mod http;
pub(crate) mod session;
use crate::{
    auth::hash_token, authz::resolve_login_scopes, crypto::generate_secret, db, error::AppError,
    extract::JsonBody, state::AppState,
};
use axum::{
    extract::{ConnectInfo, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hibana_shared::{FaasError, Redacted, Role, Scope};
use openidconnect::{
    core::{CoreAuthenticationFlow, CoreClient, CoreClientAuthMethod, CoreProviderMetadata},
    AccessTokenHash, AuthType, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl,
    Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse,
};
use sea_orm::TransactionTrait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// 平文 opaque secret。**一度だけ**返す。クライアントは Bearer に使う。
    ///
    /// M7-0 (§5.1): `Redacted` は `Serialize` を実装しないので、平文で返すには
    /// `expose_once` を**明示的に**書く必要がある。この属性の grep が「意図的に秘密を返す
    /// API」の全一覧になる。ログ・Debug 出力には `<redacted>` しか出ない。
    #[serde(serialize_with = "hibana_shared::expose_once")]
    pub token: hibana_shared::Redacted<String>,
    pub token_id: String,
    pub scopes: Vec<Scope>,
    /// RFC3339 失効時刻。
    pub expires_at: String,
}

const FLOW_TTL: u64 = 600;
const GRANT_TTL: u64 = 60;

type Client = CoreClient<
    openidconnect::EndpointSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointMaybeSet,
    openidconnect::EndpointMaybeSet,
>;

impl config::OidcConfig {
    async fn client(&self) -> Result<(Client, http::Http, bool), AppError> {
        let http = http::Http::new(self).map_err(|_| FaasError::Unavailable)?;
        // Discovery is repeated for each bounded login step so signing-key rotation
        // never requires restarting the platform. Only operator-configured issuers.
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(self.issuer.clone()).map_err(|_| FaasError::Unavailable)?,
            &http,
        )
        .await
        .map_err(|_| FaasError::Unavailable)?;
        // Email is optional. Do not break an openid-only provider with an
        // unsupported scope; it can still honor the voluntary claims request.
        let email_scope = metadata
            .scopes_supported()
            .is_some_and(|scopes| scopes.iter().any(|scope| scope.as_str() == "email"));
        for endpoint in [
            Some(metadata.authorization_endpoint().url()),
            metadata.token_endpoint().map(|u| u.url()),
            Some(metadata.jwks_uri().url()),
        ]
        .into_iter()
        .flatten()
        {
            config::validated_endpoint_url(endpoint.as_str(), self.allow_insecure_http)
                .map_err(|_| FaasError::Unavailable)?;
        }
        let auth_type = match metadata.token_endpoint_auth_methods_supported() {
            Some(methods)
                if !methods.contains(&CoreClientAuthMethod::ClientSecretBasic)
                    && methods.contains(&CoreClientAuthMethod::ClientSecretPost) =>
            {
                AuthType::RequestBody
            }
            _ => AuthType::BasicAuth,
        };
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret().to_owned())),
        )
        .set_auth_type(auth_type)
        .set_redirect_uri(
            RedirectUrl::new(self.callback_url.clone()).map_err(|_| FaasError::Unavailable)?,
        );
        Ok((client, http, email_scope))
    }
}

pub async fn configuration(State(state): State<AppState>) -> impl IntoResponse {
    no_store(
        Json(serde_json::json!({
            "console_url": state.auth_config().console_url,
        }))
        .into_response(),
    )
}

#[derive(Deserialize)]
pub struct StartRequest {
    tenant_slug: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
    #[serde(default)]
    scopes: Vec<Scope>,
}

#[derive(Serialize, Deserialize)]
struct Flow {
    tenant: String,
    redirect_uri: String,
    challenge: String,
    client_state: String,
    provider_verifier: String,
    nonce: String,
    issuer: String,
    client_id: String,
    scopes: Vec<Scope>,
}

#[derive(Serialize, Deserialize)]
struct Grant {
    tenant: String,
    user: String,
    auth_version: i64,
    challenge: String,
    issuer: String,
    subject: String,
    scopes: Vec<Scope>,
    console: bool,
    // Optional display metadata from the verified ID token, never an identity key.
    email: Option<String>,
}

fn opaque(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

async fn throttle(state: &AppState, headers: &HeaderMap, peer: SocketAddr) -> Result<(), AppError> {
    let ip = state.admission().trusted_proxies.client_ip(headers, peer);
    // Requests already rejected for this IP must not consume the shared budget.
    for (key, rate, capacity) in [
        (format!("oidc:ip:{ip}"), 0.2, 10.0),
        ("oidc:global".to_owned(), 5.0, 30.0),
    ] {
        let result = state
            .store()
            .rate_limit(
                &key,
                crate::store::RateLimitParams {
                    refill_per_sec: rate,
                    capacity,
                },
            )
            .await
            .map_err(|_| FaasError::Unavailable)?;
        if !result.allowed {
            return Err(FaasError::Unavailable.into());
        }
    }
    Ok(())
}

pub async fn start(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<StartRequest>,
) -> Result<Response, AppError> {
    let cfg = state.auth_config();
    throttle(&state, &headers, peer).await?;
    if !opaque(&req.state, 32, 128)
        || !opaque(&req.code_challenge, 43, 43)
        || URL_SAFE_NO_PAD.decode(&req.code_challenge).is_err()
        || req.scopes.len() > Role::Admin.ceiling().len()
        || !crate::public_apps::valid_label(&req.tenant_slug)
    {
        return Err(FaasError::InvalidRequest("invalid login request".into()).into());
    }
    let redirect_uri = cfg
        .validate_return_url(&req.redirect_uri)
        .map_err(|_| FaasError::InvalidRequest("invalid login return URL".into()))?;
    let tenant = db::find_tenant_id_by_slug(state.pool(), &req.tenant_slug)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    let (client, _, email_scope) = cfg.client().await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut authorization = client.authorize_url(
        CoreAuthenticationFlow::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
    );
    if email_scope {
        authorization = authorization.add_scope(openidconnect::Scope::new("email".into()));
    }
    let (url, csrf, nonce) = authorization
        .add_extra_param("claims", r#"{"id_token":{"email":null}}"#)
        .set_pkce_challenge(challenge)
        .url();
    let flow = Flow {
        tenant,
        redirect_uri: redirect_uri.into(),
        challenge: req.code_challenge,
        client_state: req.state,
        provider_verifier: verifier.secret().clone(),
        nonce: nonce.secret().clone(),
        issuer: cfg.issuer.clone(),
        client_id: cfg.client_id.clone(),
        scopes: req.scopes,
    };
    state
        .store()
        .put_auth_state(
            &format!("flow:{}", hash_token(csrf.secret())),
            &serde_json::to_string(&flow)?,
            FLOW_TTL,
        )
        .await
        .map_err(|_| FaasError::Unavailable)?;
    Ok(no_store(
        Json(serde_json::json!({ "authorization_url": url.as_str(), "expires_in": FLOW_TTL }))
            .into_response(),
    ))
}

#[derive(Deserialize)]
pub struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

pub async fn callback(
    State(state): State<AppState>,
    Query(query): Query<Callback>,
) -> Result<Response, AppError> {
    let cfg = state.auth_config();
    if !opaque(&query.state, 16, 256) {
        return Err(FaasError::Unauthorized.into());
    }
    // Bound callback I/O as well as JSON start/exchange requests.
    let _permit = state
        .json_request_slots()
        .try_acquire_owned()
        .map_err(|_| FaasError::Unavailable)?;
    let raw = state
        .store()
        .take_auth_state(&format!("flow:{}", hash_token(&query.state)))
        .await
        .map_err(|_| FaasError::Unavailable)?
        .ok_or(FaasError::Unauthorized)?;
    let flow: Flow = serde_json::from_str(&raw)?;
    if flow.issuer != cfg.issuer || flow.client_id != cfg.client_id {
        return Err(FaasError::Unauthorized.into());
    }
    cfg.validate_return_url(&flow.redirect_uri)
        .map_err(|_| FaasError::Unauthorized)?;
    let grant = if query.error.is_some() {
        Err(FaasError::Unauthorized.into())
    } else {
        finish_provider(&state, cfg, &flow, query.code).await
    };
    // Never include provider messages, tokens or claims in URLs/logs. Even an
    // error return must originate from a consumed, verified state record.
    let mut target =
        reqwest::Url::parse(&flow.redirect_uri).map_err(|_| FaasError::Unauthorized)?;
    let params = match grant {
        Ok(code) => vec![("oidc_code", code), ("oidc_state", flow.client_state)],
        Err(_) => vec![
            ("oidc_error", "login_failed".into()),
            ("oidc_state", flow.client_state),
        ],
    };
    if target.as_str() == cfg.console_url {
        let mut query_url = reqwest::Url::parse("https://unused.invalid/").unwrap();
        query_url.query_pairs_mut().extend_pairs(&params);
        target.set_fragment(query_url.query());
    } else {
        target.query_pairs_mut().extend_pairs(&params);
    }
    Ok(no_store(Redirect::to(target.as_str()).into_response()))
}

async fn finish_provider(
    state: &AppState,
    cfg: &config::OidcConfig,
    flow: &Flow,
    code: Option<String>,
) -> Result<String, AppError> {
    let code = code
        .filter(|c| !c.is_empty() && c.len() <= 8192)
        .ok_or(FaasError::Unauthorized)?;
    let (client, http, _) = cfg.client().await?;
    let response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| FaasError::Unavailable)?
        .set_pkce_verifier(PkceCodeVerifier::new(flow.provider_verifier.clone()))
        .request_async(&http)
        .await
        .map_err(|_| FaasError::Unauthorized)?;
    let id_token = response.id_token().ok_or(FaasError::Unauthorized)?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(flow.nonce.clone()))
        .map_err(|_| FaasError::Unauthorized)?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            response.access_token(),
            id_token
                .signing_alg()
                .map_err(|_| FaasError::Unauthorized)?,
            id_token
                .signing_key(&client.id_token_verifier())
                .map_err(|_| FaasError::Unauthorized)?,
        )
        .map_err(|_| FaasError::Unauthorized)?;
        if *expected != actual {
            return Err(FaasError::Unauthorized.into());
        }
    }
    let subject = claims.subject().as_str();
    if subject.is_empty() || subject.len() > 255 {
        return Err(FaasError::Unauthorized.into());
    }
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &flow.tenant).await?;
    let user = db::find_oidc_user(&tx, &flow.tenant, &cfg.issuer, subject)
        .await?
        .ok_or(FaasError::Forbidden)?;
    tx.commit().await?;
    let code = generate_secret();
    let grant = Grant {
        tenant: flow.tenant.clone(),
        user: user.id,
        auth_version: user.auth_version,
        challenge: flow.challenge.clone(),
        issuer: cfg.issuer.clone(),
        subject: subject.into(),
        scopes: flow.scopes.clone(),
        console: flow.redirect_uri == cfg.console_url,
        email: display_email(claims.email().map(|email| email.as_str())).map(str::to_owned),
    };
    state
        .store()
        .put_auth_state(
            &format!("grant:{}", hash_token(&code)),
            &serde_json::to_string(&grant)?,
            GRANT_TTL,
        )
        .await
        .map_err(|_| FaasError::Unavailable)?;
    Ok(code)
}

#[derive(Deserialize)]
pub struct ExchangeRequest {
    code: String,
    code_verifier: String,
}

pub async fn exchange(
    State(state): State<AppState>,
    JsonBody(req): JsonBody<ExchangeRequest>,
) -> Result<Response, AppError> {
    exchange_grant(&state, req, false).await
}

pub async fn browser_exchange(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<ExchangeRequest>,
) -> Result<Response, AppError> {
    session::require_console(state.auth_config(), &headers, &axum::http::Method::POST)?;
    exchange_grant(&state, req, true).await
}

async fn exchange_grant(
    state: &AppState,
    req: ExchangeRequest,
    browser: bool,
) -> Result<Response, AppError> {
    let cfg = state.auth_config();
    if !opaque(&req.code, 64, 64) || !opaque(&req.code_verifier, 43, 128) {
        return Err(FaasError::Unauthorized.into());
    }
    let raw = state
        .store()
        .take_auth_state(&format!("grant:{}", hash_token(&req.code)))
        .await
        .map_err(|_| FaasError::Unavailable)?
        .ok_or(FaasError::Unauthorized)?;
    let grant: Grant = serde_json::from_str(&raw)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(req.code_verifier.as_bytes()));
    if challenge != grant.challenge || grant.issuer != cfg.issuer || (browser && !grant.console) {
        return Err(FaasError::Unauthorized.into());
    }
    let tx = state.pool().begin().await?;
    db::set_tenant_guc(&tx, &grant.tenant).await?;
    let user = db::lock_user(&tx, &grant.tenant, &grant.user)
        .await?
        .ok_or(FaasError::Unauthorized)?;
    if user.auth_version != grant.auth_version
        || user.oidc_issuer.as_deref() != Some(&grant.issuer)
        || user.oidc_subject.as_deref() != Some(&grant.subject)
    {
        return Err(FaasError::Unauthorized.into());
    }
    if !matches!(db::load_tenant_status_and_quotas(&tx, &grant.tenant).await?, Some((status, _)) if status == "active")
    {
        return Err(FaasError::Unauthorized.into());
    }
    let role = db::parse_role(Some(&user.role)).ok_or(FaasError::Unauthorized)?;
    let scopes = resolve_login_scopes(&grant.scopes, role);
    // The PKCE handoff, current membership and revocation generation have all
    // been checked under the user lock. Failed/abandoned logins cannot mutate
    // the profile, and profile changes never rebind identities or revoke tokens.
    if let Some(email) = grant.email.as_deref().filter(|email| *email != user.email) {
        db::update_user_email(&tx, &grant.tenant, &user.id, email).await?;
        db::insert_audit_log(
            &tx,
            &grant.tenant,
            Some(&user.id),
            "user_profile_updated",
            Some(&user.id),
            Some(&serde_json::json!({ "via": "oidc", "fields": ["email"] })),
        )
        .await?;
    }
    let scope_names: Vec<String> = scopes.iter().map(|s| s.as_str().into()).collect();
    let secret = generate_secret();
    let token_id = hibana_shared::new_token_id();
    let expires = chrono::Utc::now() + chrono::Duration::seconds(cfg.session_ttl_secs);
    db::create_token(
        &tx,
        &token_id,
        &grant.tenant,
        Some(&user.id),
        &hash_token(&secret),
        &scope_names,
        Some("oidc login"),
        expires,
        db::TokenAuthMethod::Oidc,
    )
    .await?;
    db::insert_audit_log(
        &tx,
        &grant.tenant,
        Some(&user.id),
        "token_issued",
        Some(&token_id),
        Some(&serde_json::json!({ "via": "oidc", "scopes": scope_names })),
    )
    .await?;
    tx.commit().await?;
    if browser {
        let mut response = no_store(
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "token_id": token_id, "expires_at": expires.to_rfc3339(),
                })),
            )
                .into_response(),
        );
        response.headers_mut().insert(
            header::SET_COOKIE,
            session::cookie(cfg, &secret, cfg.session_ttl_secs),
        );
        return Ok(response);
    }
    Ok(no_store(
        (
            StatusCode::CREATED,
            Json(LoginResponse {
                token: Redacted::new(secret),
                token_id,
                scopes,
                expires_at: expires.to_rfc3339(),
            }),
        )
            .into_response(),
    ))
}

pub(crate) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
}

/// A bounded display label, not proof of mailbox ownership. `email_verified`
/// deliberately does not affect login, authorization, or this display cache.
fn display_email(email: Option<&str>) -> Option<&str> {
    email.filter(|value| {
        !value.trim().is_empty() && value.len() <= 320 && !value.chars().any(char::is_control)
    })
}

#[cfg(test)]
mod tests {
    use super::display_email;

    #[test]
    fn optional_display_email_is_bounded_without_normalizing_identity() {
        for email in ["Alice@Example.test", "利用者@example.test"] {
            assert_eq!(display_email(Some(email)), Some(email));
        }
        assert_eq!(display_email(None), None);
        for email in [
            "",
            "   ",
            "alice\n@example.test",
            "alice\0@example.test",
            "\u{0085}",
        ] {
            assert_eq!(display_email(Some(email)), None);
        }
        assert_eq!(display_email(Some(&"a".repeat(321))), None);
        assert_eq!(display_email(Some(&"あ".repeat(107))), None);
    }
}
