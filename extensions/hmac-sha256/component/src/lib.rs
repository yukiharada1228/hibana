wit_bindgen::generate!({path: "wit", world: "primitive"});
struct Primitive;
#[cfg(target_arch = "wasm32")]
export!(Primitive);

use hmac::{Hmac, Mac};
impl exports::hibana::hmac_sha256::api::Guest for Primitive {
    fn sign(key: Vec<u8>, data: Vec<u8>) -> Result<Vec<u8>, String> {
        if key.len() > 1024 * 1024 || data.len() > 1024 * 1024 {
            return Err("HMAC input exceeds 1 MiB".into());
        }
        let mut mac =
            <Hmac<sha2::Sha256> as Mac>::new_from_slice(&key).map_err(|e| e.to_string())?;
        mac.update(&data);
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::hmac_sha256::api::Guest;
    #[test]
    fn rfc4231_vector_and_input_limit() {
        let bytes = Primitive::sign(vec![0x0b; 20], b"Hi There".to_vec()).unwrap();
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert!(Primitive::sign(vec![0; 1024 * 1024 + 1], vec![]).is_err());
        assert!(Primitive::sign(vec![], vec![0; 1024 * 1024 + 1]).is_err());
    }
}
