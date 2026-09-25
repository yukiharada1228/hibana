//! Authenticated, disposable native-code cache. Verify before unsafe deserialization.
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const RUNTIME_HEADER: &str = "x-hibana-cache-runtime";
pub const PREFIX: &str = "_hibana/compiled/v1/";
pub const MAX_NATIVE_BYTES: usize = 128 * 1024 * 1024;
pub const TAG_BYTES: usize = 32;
pub const MAX_OBJECT_BYTES: usize = MAX_NATIVE_BYTES + TAG_BYTES;
pub const BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 256;
const DOMAIN: &[u8] = b"hibana-compiled-cache-v1\0";

#[derive(Clone, Debug)]
pub struct Auth(crate::Redacted<Vec<u8>>);

impl Auth {
    pub fn from_env() -> Result<Option<Self>, &'static str> {
        match std::env::var("COMPILED_CACHE_KEY") {
            Ok(value) => Self::from_hex(&value).map(Some),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err("COMPILED_CACHE_KEY must be 32 bytes encoded as hex"),
        }
    }

    pub fn from_hex(value: &str) -> Result<Self, &'static str> {
        let key =
            hex::decode(value).map_err(|_| "COMPILED_CACHE_KEY must be 32 bytes encoded as hex")?;
        if key.len() != 32 {
            return Err("COMPILED_CACHE_KEY must be 32 bytes encoded as hex");
        }
        Ok(Self(crate::Redacted::new(key)))
    }

    fn mac(&self, runtime: &str, wasm: &str) -> Result<Hmac<Sha256>, &'static str> {
        if !valid_id(runtime) || !valid_id(wasm) {
            return Err("Invalid compiled cache identity");
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.expose()).unwrap();
        mac.update(DOMAIN);
        mac.update(runtime.as_bytes());
        mac.update(wasm.as_bytes());
        Ok(mac)
    }

    pub fn seal(&self, runtime: &str, wasm: &str, bytes: &mut Vec<u8>) -> Result<(), &'static str> {
        if bytes.is_empty() || bytes.len() > MAX_NATIVE_BYTES {
            return Err("Invalid compiled cache size");
        }
        let mut mac = self.mac(runtime, wasm)?;
        mac.update(bytes);
        bytes.extend_from_slice(&mac.finalize().into_bytes());
        Ok(())
    }

    pub fn verify<'a>(
        &self,
        runtime: &str,
        wasm: &str,
        bytes: &'a [u8],
    ) -> Result<&'a [u8], &'static str> {
        if bytes.len() <= TAG_BYTES || bytes.len() > MAX_OBJECT_BYTES {
            return Err("Invalid compiled cache size");
        }
        let (native, tag) = bytes.split_at(bytes.len() - TAG_BYTES);
        let mut mac = self.mac(runtime, wasm)?;
        mac.update(native);
        mac.verify_slice(tag)
            .map_err(|_| "Compiled cache authentication failed")?;
        Ok(native)
    }
}

pub fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn object_key(runtime: &str, wasm: &str) -> Result<String, &'static str> {
    if !valid_id(runtime) || !valid_id(wasm) {
        return Err("Invalid compiled cache identity");
    }
    Ok(format!("{PREFIX}{runtime}/{wasm}.cwasm"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_bytes_are_bound_to_key_runtime_and_source() {
        let auth = Auth::from_hex(&"ab".repeat(32)).unwrap();
        let runtime = "12".repeat(32);
        let wasm = "34".repeat(32);
        let mut bytes = b"test native bytes, never deserialized".to_vec();
        auth.seal(&runtime, &wasm, &mut bytes).unwrap();
        assert_eq!(
            auth.verify(&runtime, &wasm, &bytes).unwrap(),
            b"test native bytes, never deserialized"
        );
        assert!(auth.verify(&"56".repeat(32), &wasm, &bytes).is_err());
        assert!(auth.verify(&runtime, &"56".repeat(32), &bytes).is_err());
        assert!(Auth::from_hex(&"cd".repeat(32))
            .unwrap()
            .verify(&runtime, &wasm, &bytes)
            .is_err());
        bytes[0] ^= 1;
        assert!(auth.verify(&runtime, &wasm, &bytes).is_err());
        assert!(auth.verify(&runtime, &wasm, &[0; TAG_BYTES]).is_err());
        assert!(object_key("../outside", &wasm).is_err());
        assert!(object_key(&"AB".repeat(32), &wasm).is_err());
        assert!(!format!("{auth:?}").contains(&"ab".repeat(32)));
    }
}
