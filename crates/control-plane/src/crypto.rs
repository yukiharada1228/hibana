//! パスワードハッシュ（argon2id, pure-Rust）と不透明トークン secret 生成 (§3.3)。
//!
//! - `password_hash`: users.password_hash は argon2id（cc/aws-lc 不要）。
//! - `verify_password`: login の照合。no-user パスでも常に呼んで timing oracle を潰す。
//! - `generate_secret`: opaque トークン secret（URL-safe な高エントロピー文字列）。
//!
//! token_hash 側（sha256）は `auth::hash_token` を使う（ハッシュ分割: §3.3）。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;
use rand::RngCore;

use hibana_shared::FaasError;

/// 不透明トークン secret のバイト長（256bit）。
const SECRET_BYTES: usize = 32;

/// argon2id でパスワードをハッシュする（ランダム salt）。PHC 文字列を返す。
pub fn hash_password(password: &str) -> Result<String, FaasError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| FaasError::Internal(format!("password hash failed: {e}")))
}

/// パスワードを PHC ハッシュ文字列と照合する。一致で `true`。
///
/// ハッシュ自体が壊れている（パース不能）場合は照合不能として `false`。例外を投げず
/// 一律に false を返すことで、login の失敗形状を統一する（timing/oracle を作らない）。
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// 不透明トークン secret を生成する（高エントロピー・URL-safe hex）。
///
/// 返り値は平文。呼び出し側は一度だけクライアントへ返し、DB には
/// `auth::hash_token`（sha256 hex）のみ保存する。
pub fn generate_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(SECRET_BYTES * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// login の no-user / no-tenant パスで使う固定ダミーハッシュ。
///
/// ユーザが存在しなくても**必ず** argon2 verify を一回実行し、応答時間を
/// 存在ありパスと揃える（timing oracle 防止）。プロセス起動時に一度だけ生成する。
pub fn dummy_password_hash() -> String {
    // 固定パスワードから生成。salt はランダムだが値自体は秘密でなくてよい
    // （照合は常に失敗する想定のダミー）。
    hash_password("dummy-password-for-uniform-failure")
        .unwrap_or_else(|_| String::from("$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAA$"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash_password("hunter2").unwrap();
        assert!(verify_password("hunter2", &phc));
        assert!(!verify_password("hunter3", &phc));
    }

    #[test]
    fn verify_against_garbage_hash_is_false_not_panic() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn dummy_hash_parses_and_never_matches_random_input() {
        let dummy = dummy_password_hash();
        // ダミーハッシュは PHC としてパース可能で、任意入力に一致しない。
        assert!(!verify_password(
            "dummy-password-for-uniform-failure-wrong",
            &dummy
        ));
    }

    #[test]
    fn generated_secrets_are_unique_and_hex() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b);
        assert_eq!(a.len(), SECRET_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
