//! Generate opaque Hibana session and API token secrets.
use rand::{rngs::OsRng, RngCore};

const SECRET_BYTES: usize = 32;

/// 不透明トークン secret を生成する（高エントロピー・URL-safe hex）。
///
/// 返り値は平文。呼び出し側は一度だけクライアントへ返し、DB には
/// `auth::hash_token`（sha256 hex）のみ保存する。
pub fn generate_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_secrets_are_unique_and_hex() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b);
        assert_eq!(a.len(), SECRET_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
