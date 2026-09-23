//! Unicode normalization without networking or cryptographic dependencies.
use unicode_normalization::UnicodeNormalization;
wit_bindgen::generate!({ path: "wit", world: "unicode-component" });
struct Unicode;
impl exports::hibana::unicode_nfkc::api::Guest for Unicode {
    fn normalize_nfkc(value: String) -> Result<String, String> {
        if value.len() > 1024 * 1024 {
            return Err("Unicode input exceeds 1 MiB".into());
        }
        Ok(value.nfkc().collect())
    }
}
#[cfg(target_arch = "wasm32")]
export!(Unicode);

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::unicode_nfkc::api::Guest;
    #[test]
    fn compatibility_normalization_and_limit() {
        assert_eq!(Unicode::normalize_nfkc("Ａ\u{00a0}".into()).unwrap(), "A ");
        assert_eq!(Unicode::normalize_nfkc("\u{212b}".into()).unwrap(), "Å");
        assert!(Unicode::normalize_nfkc("a".repeat(1024 * 1024 + 1)).is_err());
    }
}
