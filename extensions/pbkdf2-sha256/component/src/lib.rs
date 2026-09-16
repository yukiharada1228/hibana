wit_bindgen::generate!({path: "wit", world: "primitive"});
struct Primitive;
#[cfg(target_arch = "wasm32")]
export!(Primitive);

impl exports::hibana::pbkdf2_sha256::api::Guest for Primitive {
    fn derive(password: Vec<u8>, salt: Vec<u8>, iterations: u32) -> Result<Vec<u8>, String> {
        if password.len() > 1024 * 1024 || salt.len() > 1024 * 1024 {
            return Err("PBKDF2 input exceeds 1 MiB".into());
        }
        if !(1..=100_000).contains(&iterations) {
            return Err("PBKDF2 iterations must be 1..100000".into());
        }
        let mut key = vec![0; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&password, &salt, iterations, &mut key);
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::pbkdf2_sha256::api::Guest;
    #[test]
    fn known_vector_and_work_limits() {
        let bytes = Primitive::derive(b"password".to_vec(), b"salt".to_vec(), 1).unwrap();
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert!(Primitive::derive(vec![], vec![], 0).is_err());
        assert!(Primitive::derive(vec![], vec![], 100_001).is_err());
        assert!(Primitive::derive(vec![], vec![0; 1024 * 1024 + 1], 1).is_err());
    }
}
