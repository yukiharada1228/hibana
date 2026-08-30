//! POST /auth/login — メール+パスワードで opaque API トークンを発行する (§3.3)。
//!
//! 手順:
//! 1. `tenant_slug` -> tenant_id 解決（tenants.slug は UNIQUE）。
//! 2. (tenant_id, email) でユーザ解決。
//! 3. argon2id でパスワード照合。
//! 4. 成功なら opaque secret を採番し、sha256(secret) を api_tokens に保存して
//!    平文 secret を一度だけ返す。scopes は min(要求, role.ceiling)。
//!
//! UNIFORM FAILURE (MUST): テナント不在/ユーザ不在でも**必ず**固定ダミーハッシュで
//! argon2 verify を一度実行し、応答時間と 401 のボディ形状を均一化する（timing/
//! oracle 防止）。どの要因で失敗しても同一の 401 を返す。パスワードはログに出さない。
//!
//! ロックアウト (M3d, §6.0 / §8): [`crate::store::Store`] ベースの分散ロックアウト。失敗を
//! **2 つの鍵**でカウントする —— `(tenant_slug, email)` と **クライアント IP** —— どちらかが
//! 閾値を超えたら 401（fail-CLOSED）。fail-closed の意味は **ストア到達不能でも拒否** であり、
//! Redis 障害時にブルートフォースを素通しさせない（[`crate::store::FailPolicy::LOGIN_LOCKOUT`]）。
//!
//! TIMING-ORACLE 不変条件 (MUST): ロックアウト棄却の早期 return は、資格情報失敗の 401 が
//! 経由する argon2 verify を踏まないため、そのままだとロックアウト 401 のほうが速くなる
//! （タイミングオラクル）。これを防ぐため、ロックアウトで return する前に必ずダミー
//! `verify_password` を 1 回走らせ、両 401 経路の所要時間を揃える。

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use faas_shared::{new_token_id, FaasError, Scope};

use crate::auth::hash_token;
use crate::authz::resolve_login_scopes;
use crate::crypto::{generate_secret, verify_password};
use crate::db;
use crate::error::AppError;
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::store::LockoutDecision;

/// 発行トークンの有効期間（短命）。
const LOGIN_TOKEN_TTL_HOURS: i64 = 12;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// テナント slug（人間可読・グローバル一意）。
    pub tenant_slug: String,
    pub email: String,
    pub password: String,
    /// 要求スコープ（任意）。未指定は role 上限を付与。role 上限超は黙って切詰め。
    #[serde(default)]
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// 平文 opaque secret。**一度だけ**返す。クライアントは Bearer に使う。
    ///
    /// M7-0 (§5.1): `Redacted` は `Serialize` を実装しないので、平文で返すには
    /// `expose_once` を**明示的に**書く必要がある。この属性の grep が「意図的に秘密を返す
    /// API」の全一覧になる。ログ・Debug 出力には `<redacted>` しか出ない。
    #[serde(serialize_with = "faas_shared::expose_once")]
    pub token: faas_shared::Redacted<String>,
    pub token_id: String,
    pub scopes: Vec<Scope>,
    /// RFC3339 失効時刻。
    pub expires_at: String,
}

/// POST /auth/login。認証不要ルート（middleware から除外される）。
///
/// `ConnectInfo<SocketAddr>` は main.rs の `into_make_service_with_connect_info` で有効化される
/// 接続元アドレス。ロックアウトの IP 鍵に使う（信頼境界外の詐称を防ぐため、既定では
/// X-Forwarded-For を信頼しない —— `TRUST_PROXY_HEADERS=true` で明示オプトイン）。
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<LoginRequest>,
) -> Result<impl IntoResponse, AppError> {
    // ロックアウトの 2 鍵を組み立てる（§6.0）: (tenant_slug, email) と クライアント IP。
    let id_key = format!("{}\u{0}{}", req.tenant_slug, req.email);
    let client_ip = client_ip(&headers, peer, state.admission().trust_proxy_headers);
    let ip_key = format!("ip:{client_ip}");
    let lockout_params = state.admission().lockout;

    // --- ロックアウト判定（fail-CLOSED, 両鍵 OR, §8）---
    // どちらかの鍵がロック中、**または**ストア到達不能なら拒否する。
    let id_locked = check_lockout(&state, &id_key, lockout_params).await;
    let ip_locked = check_lockout(&state, &ip_key, lockout_params).await;
    if is_locked_out(id_locked, ip_locked) {
        // TIMING-ORACLE 不変条件 (MUST): 資格情報失敗 401 と所要時間を揃えるため、ここで
        // return する前に必ずダミー verify を 1 回走らせる（argon2 のコストを両経路に課す）。
        let _ = verify_password(&req.password, state.dummy_password_hash());
        // §3.7: ロックアウト発火を監査する。テナントが解決できる場合のみ（audit_logs は
        // FORCE RLS + tenant_id FK のため、紐づけられる tenant_id が要る）。生パスワードは載せない。
        if let Ok(Some(tid)) = db::find_tenant_id_by_slug(state.pool(), &req.tenant_slug).await {
            write_login_audit(
                &state,
                &tid,
                None,
                "auth_lockout",
                None,
                serde_json::json!({
                    "id_key_locked": matches!(id_locked, LockoutCheck::Locked),
                    "ip_key_locked": matches!(ip_locked, LockoutCheck::Locked),
                    "store_unavailable":
                        matches!(id_locked, LockoutCheck::Unavailable)
                        || matches!(ip_locked, LockoutCheck::Unavailable),
                }),
            )
            .await;
        }
        return Err(FaasError::Unauthorized.into());
    }

    // (1) テナント解決。(2) ユーザ解決。どちらが欠けても user を None にして
    //     ダミーハッシュで verify する（uniform failure）。テナント id は成功時に使う。
    let tenant_id = db::find_tenant_id_by_slug(state.pool(), &req.tenant_slug).await?;
    let user = match &tenant_id {
        Some(tid) => db::find_user_by_email(state.pool(), tid, &req.email).await?,
        None => None,
    };

    // (3) argon2 verify。no-user パスでも必ず固定ダミーで verify を実行する。
    let (verified, resolved) = match &user {
        Some(u) => {
            let ok = verify_password(&req.password, &u.password_hash);
            (ok, Some(u))
        }
        None => {
            // ダミー verify（結果は捨てる）。応答時間を存在ありパスと揃える。
            let _ = verify_password(&req.password, state.dummy_password_hash());
            (false, None)
        }
    };

    if !verified {
        // 失敗を **両鍵** に記録する（§6.0）。fail-closed クラスだが record 失敗で 401 応答を
        // 妨げない（best-effort: Redis 障害時は check 側が既に fail-closed で守っている）。
        record_failure(&state, &id_key, lockout_params).await;
        record_failure(&state, &ip_key, lockout_params).await;
        // §3.7: 認証失敗を audit_logs に記録する。audit_logs は FORCE RLS + tenant_isolation
        // かつ tenant_id NOT NULL REFERENCES tenants(id) なので、テナントが解決できた失敗
        // （ユーザ不在/パスワード不一致）だけ当該テナント配下に追記する。テナント自体が
        // 不在の失敗は、紐づけられる正当な tenant_id が無く（FK/GUC を通せない）audit 行を
        // 作れないため記録しない（uniform 401 応答は保つ; 生パスワード/ハッシュは載せない）。
        if let Some(tid) = tenant_id.as_deref() {
            let actor = user.as_ref().map(|u| u.id.as_str());
            let reason = if user.is_some() {
                "bad_password"
            } else {
                "unknown_user"
            };
            write_login_audit(
                &state,
                tid,
                actor,
                "auth_failed",
                None,
                serde_json::json!({ "reason": reason }),
            )
            .await;
        }
        // どの要因でも同一の 401（ボディ形状はエラーエンベロープで統一）。
        return Err(FaasError::Unauthorized.into());
    }

    let user = resolved.expect("verified implies user present");
    // verified=true は user=Some を含意し、user=Some は tenant_id=Some を含意する。
    let tenant_id = tenant_id.expect("verified implies tenant resolved");
    let role = db::parse_role(Some(&user.role)).ok_or(FaasError::Unauthorized)?;

    // (4) scopes = 要求 ∩ role 上限（昇格は不可。本人発行なので caller 制約は無し）。
    let scopes = resolve_login_scopes(&req.scopes, role);
    let scope_strs: Vec<String> = scopes.iter().map(|s| s.as_str().to_string()).collect();

    let secret = generate_secret();
    let token_hash = hash_token(&secret);
    let token_id = new_token_id();
    let expires_at = Utc::now() + Duration::hours(LOGIN_TOKEN_TTL_HOURS);

    // api_tokens は FORCE RLS 下にあるため、INSERT は解決済み tenant_id を GUC に
    // セットした tx 上で行う（WITH CHECK を通す）。slug/user の解決は SECURITY DEFINER
    // 関数経由なので GUC 不要だが、トークン INSERT はテナントコンテキストを要する。
    let mut tx = state.pool().begin().await?;
    db::set_tenant_guc(&mut tx, &tenant_id).await?;
    db::create_token(
        &mut *tx,
        &token_id,
        &tenant_id,
        Some(&user.id),
        &token_hash,
        &scope_strs,
        Some("login"),
        expires_at,
    )
    .await?;
    // §3.7: トークン発行を audit_logs に記録する（GUC は設定済み → WITH CHECK を通す。
    // 同一 tx で token 行とアトミックに commit する）。生 secret/hash は載せない。
    db::insert_audit_log(
        &mut *tx,
        &tenant_id,
        Some(user.id.as_str()),
        "token_issued",
        Some(&token_id),
        Some(&serde_json::json!({ "scopes": scope_strs, "via": "login" })),
    )
    .await?;
    tx.commit().await?;

    // 成功で **両鍵** の失敗カウンタをリセットする（§6.0）。best-effort（クリア失敗は無害:
    // TTL 減衰でいずれ解放され、正規ユーザは次回成功でまたクリアされる）。
    clear_failures(&state, &id_key).await;
    clear_failures(&state, &ip_key).await;

    Ok((
        StatusCode::CREATED,
        Json(LoginResponse {
            token: faas_shared::Redacted::new(secret),
            token_id,
            scopes,
            expires_at: expires_at.to_rfc3339(),
        }),
    ))
}

/// 失敗パス用の best-effort audit 追記（§3.7）。
///
/// audit_logs は FORCE RLS + tenant_isolation 下なので専用の短命 tx を開き、先に
/// `set_tenant_guc(tenant)` してから INSERT する（WITH CHECK を通すため tenant は
/// 解決済み tenant_id にする）。audit の失敗で 401 応答や処理を巻き込まないよう、
/// 失敗はログのみ（best-effort）。
async fn write_login_audit(
    state: &AppState,
    tenant: &str,
    actor: Option<&str>,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    let res: anyhow::Result<()> = async {
        let mut tx = state.pool().begin().await?;
        db::set_tenant_guc(&mut tx, tenant).await?;
        db::insert_audit_log(&mut *tx, tenant, actor, action, target, Some(&detail)).await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, action, "failed to write login audit_logs row");
    }
}

// ---------------------------------------------------------------------------
// ロックアウト helpers (§6.0 / §8) — fail-closed・両鍵 OR・純関数化してテスト可能に
// ---------------------------------------------------------------------------

/// 1 鍵分のロックアウト判定結果。fail-closed の OR ロジック（[`is_locked_out`]）に渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockoutCheck {
    /// 閾値未満 = この鍵では許可。
    Ok,
    /// 閾値到達 = この鍵でロック中。
    Locked,
    /// ストア到達不能。**fail-closed**: ロック扱い（ブルートフォース素通し防止, §8）。
    Unavailable,
}

/// 1 鍵のロックアウトをストアに問い合わせる（カウントは増やさない）。
///
/// `Err`（到達不能）は [`LockoutCheck::Unavailable`] にし、呼び出し側で fail-closed（拒否）扱い
/// にする（§8: login=FailPolicy::Closed）。
async fn check_lockout(
    state: &AppState,
    key: &str,
    params: crate::store::LockoutParams,
) -> LockoutCheck {
    match state.store().check_login_lockout(key, params).await {
        Ok(LockoutDecision { locked: true, .. }) => LockoutCheck::Locked,
        Ok(_) => LockoutCheck::Ok,
        Err(e) => {
            // fail-closed: ストア障害時は拒否側へ倒す（誤って fail-open しない, §8）。
            tracing::warn!(error = %e, "login lockout store unavailable; failing CLOSED (denying)");
            LockoutCheck::Unavailable
        }
    }
}

/// 両鍵の OR ロジック（§6.0）。**どちらか**がロック中 or 到達不能なら true（拒否）。
///
/// 純関数（DB/ストア非依存）にしてユニットテストする。fail-closed: `Unavailable` は
/// `Locked` と同じく拒否に倒す。
fn is_locked_out(id: LockoutCheck, ip: LockoutCheck) -> bool {
    !matches!((id, ip), (LockoutCheck::Ok, LockoutCheck::Ok))
}

/// 1 鍵の失敗を記録する（best-effort; 失敗はログのみ）。
async fn record_failure(state: &AppState, key: &str, params: crate::store::LockoutParams) {
    if let Err(e) = state.store().record_login_failure(key, params).await {
        tracing::warn!(error = %e, "failed to record login failure (lockout counter)");
    }
}

/// 1 鍵の失敗カウンタをクリアする（成功時; best-effort）。
async fn clear_failures(state: &AppState, key: &str) {
    if let Err(e) = state.store().clear_login_failures(key).await {
        tracing::warn!(error = %e, "failed to clear login failures");
    }
}

/// クライアント IP を解決する（§6.0）。
///
/// 既定では接続元 `SocketAddr`（`ConnectInfo`）の IP を使う。`trust_proxy`（env
/// `TRUST_PROXY_HEADERS`、既定 false）が真のときに限り `X-Forwarded-For` の **先頭** エントリ
/// を信頼する。プロキシ信頼を明示オプトインにするのは、信頼境界外で X-Forwarded-For を
/// 信じると攻撃者が IP を詐称してロックアウトを回避（毎回別 IP を名乗る）したり、他者を
/// ロックアウト（被害者 IP を名乗って失敗を積む）できてしまうため（fail-safe な既定）。
///
/// 純関数（DB/ストア非依存）にしてユニットテストする。
fn client_ip(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            // X-Forwarded-For: client, proxy1, proxy2 ... 先頭が originating client。
            if let Some(first) = xff.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    // ポートは鍵に含めない（同一 IP の別ポートを別 client 扱いしない）。
    peer.ip().to_string()
}

#[cfg(test)]
mod tests {
    use super::{client_ip, is_locked_out, LockoutCheck};
    use crate::authz::resolve_login_scopes;
    use crate::crypto::{dummy_password_hash, verify_password};
    use axum::http::HeaderMap;
    use faas_shared::{Role, Scope};
    use std::net::SocketAddr;

    /// uniform-failure: ダミーハッシュ verify は決して成功しない（no-user パスで
    /// 実行しても認証が通ることはない）。
    #[test]
    fn dummy_hash_verify_never_succeeds() {
        let dummy = dummy_password_hash();
        assert!(!verify_password("any-password", &dummy));
        assert!(!verify_password("", &dummy));
    }

    /// login で発行されるスコープは role 上限を超えない。
    #[test]
    fn login_scopes_never_exceed_role() {
        let got = resolve_login_scopes(&[Scope::Admin, Scope::Read], Role::Member);
        assert!(!got.contains(&Scope::Admin));
        assert!(got.contains(&Scope::Read));
    }

    // ---- M3d ロックアウト: 両鍵 OR ロジック (§6.0) -----------------------------

    /// 両鍵とも Ok のときだけ許可（false）。それ以外は拒否（true）。
    #[test]
    fn lockout_or_logic_both_ok_allows() {
        assert!(!is_locked_out(LockoutCheck::Ok, LockoutCheck::Ok));
    }

    /// どちらか一方でも Locked なら拒否（(tenant,email) か IP のどちらか, OR)。
    #[test]
    fn lockout_or_logic_either_locked_denies() {
        assert!(is_locked_out(LockoutCheck::Locked, LockoutCheck::Ok));
        assert!(is_locked_out(LockoutCheck::Ok, LockoutCheck::Locked));
        assert!(is_locked_out(LockoutCheck::Locked, LockoutCheck::Locked));
    }

    /// fail-CLOSED: ストア到達不能（Unavailable）はロック扱いで拒否する（§8）。
    #[test]
    fn lockout_or_logic_unavailable_fails_closed() {
        assert!(is_locked_out(LockoutCheck::Unavailable, LockoutCheck::Ok));
        assert!(is_locked_out(LockoutCheck::Ok, LockoutCheck::Unavailable));
        assert!(is_locked_out(
            LockoutCheck::Unavailable,
            LockoutCheck::Unavailable
        ));
    }

    // ---- M3d ロックアウト: クライアント IP 抽出 (§6.0) -------------------------

    fn peer(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// 既定（trust_proxy=false）は接続元 SocketAddr の IP を使い、ポートは含めない。
    #[test]
    fn client_ip_uses_peer_when_proxy_untrusted() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        // 信頼しない → XFF は無視して接続元を使う（詐称防止）。
        assert_eq!(
            client_ip(&h, peer("203.0.113.9:55555"), false),
            "203.0.113.9"
        );
    }

    /// trust_proxy=true のとき X-Forwarded-For の先頭エントリ（originating client）を使う。
    #[test]
    fn client_ip_uses_xff_first_when_trusted() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            "9.9.9.9, 10.0.0.1, 10.0.0.2".parse().unwrap(),
        );
        assert_eq!(client_ip(&h, peer("203.0.113.9:55555"), true), "9.9.9.9");
    }

    /// trust_proxy=true でも XFF が無ければ接続元へフォールバックする。
    #[test]
    fn client_ip_falls_back_to_peer_without_xff() {
        let h = HeaderMap::new();
        assert_eq!(
            client_ip(&h, peer("198.51.100.7:40000"), true),
            "198.51.100.7"
        );
    }
}
