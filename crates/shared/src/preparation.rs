//! Preparation is authorized separately from invocation and never executes a guest.
use serde::{Deserialize, Serialize};

pub const TOKEN_HEADER: &str = "x-hibana-preparation-token";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct Claims {
    pub tenant_id: String,
    pub storage_uri: String,
    pub sha256: String,
    pub kid: String,
    pub iat: i64,
    pub exp: i64,
}

impl Claims {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut bytes = b"hibana-preparation-v1\0".to_vec();
        for value in [&self.tenant_id, &self.storage_uri, &self.sha256, &self.kid] {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.extend_from_slice(&self.iat.to_be_bytes());
        bytes.extend_from_slice(&self.exp.to_be_bytes());
        bytes
    }

    pub fn valid_at(&self, now: i64) -> bool {
        self.exp > now
            && self.iat <= now + 5
            && self.exp.checked_sub(self.iat).is_some_and(|ttl| ttl <= 120)
            && self.exp > self.iat
            && valid_digest(&self.sha256)
            && !self.tenant_id.is_empty()
            && self
                .storage_uri
                .starts_with(&format!("{}/versions/", self.tenant_id))
            && self.storage_uri.ends_with(".wasm")
            && !self.storage_uri.contains("..")
    }
}

pub fn valid_digest(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

// No Debug: the presigned URL is a short-lived storage credential.
#[derive(Serialize, Deserialize)]
pub struct Artifact {
    pub sha256: String,
    pub url: String,
}
