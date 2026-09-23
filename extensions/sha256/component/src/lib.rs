wit_bindgen::generate!({path: "wit", world: "primitive"});
struct Primitive;
#[cfg(target_arch = "wasm32")]
export!(Primitive);

use sha2::Digest;
impl exports::hibana::sha256::api::Guest for Primitive {
    fn digest(data: Vec<u8>) -> Result<Vec<u8>, String> {
        if data.len() > 1024 * 1024 {
            return Err("Digest input exceeds 1 MiB".into());
        }
        Ok(sha2::Sha256::digest(data).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::sha256::api::Guest;
    #[test]
    fn known_vector_and_input_limit() {
        let bytes = Primitive::digest(b"abc".to_vec()).unwrap();
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(Primitive::digest(vec![0; 1024 * 1024 + 1]).is_err());
    }
}
