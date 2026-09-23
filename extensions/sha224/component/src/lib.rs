wit_bindgen::generate!({path: "wit", world: "primitive"});
struct Primitive;
#[cfg(target_arch = "wasm32")]
export!(Primitive);

use sha2::Digest;
impl exports::hibana::sha224::api::Guest for Primitive {
    fn digest(data: Vec<u8>) -> Result<Vec<u8>, String> {
        if data.len() > 1024 * 1024 {
            return Err("Digest input exceeds 1 MiB".into());
        }
        Ok(sha2::Sha224::digest(data).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::sha224::api::Guest;
    #[test]
    fn known_vector_and_input_limit() {
        let bytes = Primitive::digest(b"abc".to_vec()).unwrap();
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert!(Primitive::digest(vec![0; 1024 * 1024 + 1]).is_err());
    }
}
