wit_bindgen::generate!({ path: "wit", world: "crypto" });

use sha2::{Digest, Sha256};
struct Crypto;
export!(Crypto);

impl exports::example::crypto::hash::Guest for Crypto {
    fn sha256(data: Vec<u8>) -> String {
        format!("{:x}", Sha256::digest(data))
    }
}
