wit_bindgen::generate!({path: "wit", world: "primitive"});
struct Primitive;
#[cfg(target_arch = "wasm32")]
export!(Primitive);

use sha2::Digest;
impl exports::hibana::sha512t224::api::Guest for Primitive {
    fn digest(data: Vec<u8>) -> Result<Vec<u8>, String> {
        if data.len() > 1024 * 1024 {
            return Err("Digest input exceeds 1 MiB".into());
        }
        Ok(sha2::Sha512_224::digest(data).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::sha512t224::api::Guest;
    #[test]
    fn known_vector_and_input_limit() {
        let bytes = Primitive::digest(b"abc".to_vec()).unwrap();
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa"
        );
        assert!(Primitive::digest(vec![0; 1024 * 1024 + 1]).is_err());
    }
}
