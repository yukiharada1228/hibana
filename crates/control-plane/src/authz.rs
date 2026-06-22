//! 認可の純ロジック（DB 非依存）。トークン発行時のスコープ上限計算 (§3.3)。
//!
//! 発行されるトークンのスコープは
//!   要求スコープ ∩ 呼び出し主体のスコープ ∩ 対象ユーザのロール上限
//! でなければならない（default-open は禁止＝要求を無検証で通さない）。

use faas_shared::{FaasError, Role, Scope};

/// 管理操作（ユーザ/トークン作成・失効）に admin **ロール**を要求する。
///
/// `require_scope(Admin)` 層に加えた多層防御。スコープではなくロールを権限境界に
/// する設計判断（§3.7）。service トークン（`user_id` NULL）は `Role::Admin` に
/// マップされるため通過する。admin 未満は `FaasError::Forbidden`（→ 403）。
pub fn require_admin_role(role: Role) -> Result<(), FaasError> {
    if role == Role::Admin {
        Ok(())
    } else {
        Err(FaasError::Forbidden)
    }
}

/// 要求スコープを「呼び出し主体のスコープ」と「対象ロール上限」の両方で絞り込む。
///
/// MUST: 要求スコープが呼び出し主体 or ロール上限を超える場合は拒否する
/// （`FaasError::Forbidden`）。これにより権限昇格（escalation）を防ぐ。
/// 要求が空（未指定）の場合は、呼び出し主体 ∩ ロール上限を既定付与する。
///
/// 戻り値は重複を除いた `Scope` のベクタ（入力順を保持）。
pub fn resolve_token_scopes(
    requested: &[Scope],
    caller_scopes: &[Scope],
    target_role: Role,
) -> Result<Vec<Scope>, FaasError> {
    let ceiling = target_role.ceiling();

    // 要求未指定: 呼び出し主体 ∩ ロール上限を既定とする。
    if requested.is_empty() {
        let mut out = Vec::new();
        for s in caller_scopes {
            if ceiling.contains(s) && !out.contains(s) {
                out.push(*s);
            }
        }
        return Ok(out);
    }

    // 要求指定あり: すべての要求が呼び出し主体 AND ロール上限以下であること。
    let mut out = Vec::new();
    for s in requested {
        if !caller_scopes.contains(s) {
            return Err(FaasError::Forbidden);
        }
        if !ceiling.contains(s) {
            return Err(FaasError::Forbidden);
        }
        if !out.contains(s) {
            out.push(*s);
        }
    }
    Ok(out)
}

/// ログイン時のスコープ決定: 要求 ∩ ロール上限。
///
/// login は「ユーザ本人」がトークンを作るため caller 制約は無い（本人の上限のみ）。
/// 要求未指定なら全ロール上限を付与する。要求がロール上限を超えても**昇格はせず**、
/// 上限内に黙って切り詰める（login の利便性優先。MUST の昇格防止は満たす）。
pub fn resolve_login_scopes(requested: &[Scope], role: Role) -> Vec<Scope> {
    let ceiling = role.ceiling();
    if requested.is_empty() {
        return ceiling.to_vec();
    }
    let mut out = Vec::new();
    for s in requested {
        if ceiling.contains(s) && !out.contains(s) {
            out.push(*s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_admin_role_only_admin_passes() {
        assert!(require_admin_role(Role::Admin).is_ok());
        assert!(matches!(
            require_admin_role(Role::Member),
            Err(FaasError::Forbidden)
        ));
    }

    #[test]
    fn requested_within_caller_and_ceiling_is_granted() {
        let got = resolve_token_scopes(
            &[Scope::Read, Scope::Invoke],
            &[Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin],
            Role::Admin,
        )
        .unwrap();
        assert_eq!(got, vec![Scope::Read, Scope::Invoke]);
    }

    #[test]
    fn requested_above_caller_is_forbidden() {
        // 呼び出し主体は admin を持たない → admin 要求は昇格になり拒否。
        let err = resolve_token_scopes(
            &[Scope::Admin],
            &[Scope::Read, Scope::Invoke, Scope::Deploy],
            Role::Admin,
        )
        .unwrap_err();
        assert!(matches!(err, FaasError::Forbidden));
    }

    #[test]
    fn requested_above_target_role_ceiling_is_forbidden() {
        // 対象ユーザは member（admin 不可）→ caller が admin を持っていても拒否。
        let err = resolve_token_scopes(
            &[Scope::Admin],
            &[Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin],
            Role::Member,
        )
        .unwrap_err();
        assert!(matches!(err, FaasError::Forbidden));
    }

    #[test]
    fn empty_request_defaults_to_caller_intersect_ceiling() {
        // caller は全権、対象は member → admin は落ちる。
        let got = resolve_token_scopes(
            &[],
            &[Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin],
            Role::Member,
        )
        .unwrap();
        assert_eq!(got, vec![Scope::Read, Scope::Invoke, Scope::Deploy]);
    }

    #[test]
    fn empty_request_limited_by_caller() {
        // caller が read のみ → 対象が admin でも read だけ。
        let got = resolve_token_scopes(&[], &[Scope::Read], Role::Admin).unwrap();
        assert_eq!(got, vec![Scope::Read]);
    }

    #[test]
    fn duplicate_requests_deduped() {
        let got = resolve_token_scopes(&[Scope::Read, Scope::Read], &[Scope::Read], Role::Member)
            .unwrap();
        assert_eq!(got, vec![Scope::Read]);
    }

    #[test]
    fn login_scopes_default_to_full_ceiling() {
        assert_eq!(
            resolve_login_scopes(&[], Role::Member),
            vec![Scope::Read, Scope::Invoke, Scope::Deploy]
        );
        assert_eq!(
            resolve_login_scopes(&[], Role::Admin),
            vec![Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin]
        );
    }

    #[test]
    fn login_scopes_clamp_to_ceiling() {
        // member が admin を要求しても黙って切り捨て（昇格しない）。
        assert_eq!(
            resolve_login_scopes(&[Scope::Read, Scope::Admin], Role::Member),
            vec![Scope::Read]
        );
    }
}
