wit_bindgen::generate!({ path: "wit", world: "postgres-crypto" });

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

struct Crypto;
#[cfg(target_arch = "wasm32")]
export!(Crypto);

fn bounded(data: &[u8]) -> Result<(), String> {
    if data.len() > 1024 * 1024 {
        return Err("PostgreSQL authentication input exceeds 1 MiB".into());
    }
    Ok(())
}

impl exports::hibana::postgres::crypto::Guest for Crypto {
    fn normalize_nfkc(value: String) -> Result<String, String> {
        bounded(value.as_bytes())?;
        Ok(value.nfkc().collect())
    }

    fn random_bytes(length: u32) -> Result<Vec<u8>, String> {
        if length > 4096 {
            return Err("Random authentication data exceeds 4096 bytes".into());
        }
        Ok(wasi::random::random::get_random_bytes(u64::from(length)))
    }

    fn sha256(data: Vec<u8>) -> Result<Vec<u8>, String> {
        bounded(&data)?;
        Ok(Sha256::digest(data).to_vec())
    }

    fn md5(data: Vec<u8>) -> Result<Vec<u8>, String> {
        bounded(&data)?;
        Ok(md5::Md5::digest(data).to_vec())
    }

    fn hmac_sha256(key: Vec<u8>, data: Vec<u8>) -> Result<Vec<u8>, String> {
        bounded(&key)?;
        bounded(&data)?;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).map_err(|e| e.to_string())?;
        mac.update(&data);
        Ok(mac.finalize().into_bytes().to_vec())
    }

    fn derive_key(password: Vec<u8>, salt: Vec<u8>, iterations: u32) -> Result<Vec<u8>, String> {
        bounded(&password)?;
        bounded(&salt)?;
        if !(1..=100_000).contains(&iterations) {
            return Err("SCRAM iterations must be 1..100000".into());
        }
        let mut key = vec![0; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(&password, &salt, iterations, &mut key);
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::postgres::crypto::Guest;

    #[test]
    fn pbkdf2_known_vector_and_work_limits() {
        let key = Crypto::derive_key(b"password".to_vec(), b"salt".to_vec(), 1).unwrap();
        let hex: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert!(Crypto::derive_key(vec![], vec![], 0).is_err());
        assert!(Crypto::derive_key(vec![], vec![], 100_001).is_err());
        assert!(Crypto::sha256(vec![0; 1024 * 1024 + 1]).is_err());
        assert_eq!(Crypto::normalize_nfkc("Ａ\u{00a0}".into()).unwrap(), "A ");
    }
}
