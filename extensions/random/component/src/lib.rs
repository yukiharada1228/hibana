wit_bindgen::generate!({path: "wit", world: "primitive"});
struct Primitive;
#[cfg(target_arch = "wasm32")]
export!(Primitive);

impl exports::hibana::random::api::Guest for Primitive {
    fn bytes(length: u32) -> Result<Vec<u8>, String> {
        if length > 4096 {
            return Err("Random data exceeds 4096 bytes".into());
        }
        Ok(wasi::random::random::get_random_bytes(u64::from(length)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hibana::random::api::Guest;
    #[test]
    fn excessive_requests_are_rejected_before_host_access() {
        assert!(Primitive::bytes(4097).is_err());
    }
}
