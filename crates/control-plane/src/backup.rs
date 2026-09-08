//! Offline restore verification. Input only on stdin; never emit keys or plaintext.
use crate::secrets::{self, Envelope};
use serde::Deserialize;
use std::io::BufRead;

#[derive(Deserialize)]
struct Keys {
    active_kid: String,
    active_key: String,
    retired: String,
}
#[derive(Deserialize)]
struct Row {
    tenant_id: String,
    component_id: String,
    secret_id: String,
    name: String,
    version: i32,
    kek_kid: String,
    ciphertext: String,
    nonce: String,
    wrapped_dek: String,
    dek_nonce: String,
    value_len: i32,
}
fn hex(raw: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        raw.len().is_multiple_of(2) && raw.is_ascii(),
        "invalid envelope encoding"
    );
    raw.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p)?, 16).map_err(Into::into))
        .collect()
}
fn line(input: &mut impl BufRead) -> anyhow::Result<zeroize::Zeroizing<String>> {
    use std::io::Read;
    let mut raw = zeroize::Zeroizing::new(String::new());
    input.take(1024 * 1024 + 1).read_line(&mut raw)?;
    anyhow::ensure!(raw.len() <= 1024 * 1024, "verification record too large");
    Ok(raw)
}
pub(crate) fn verify(mut input: impl BufRead) -> anyhow::Result<usize> {
    let keys: Keys = serde_json::from_str(&line(&mut input)?)?;
    let active = zeroize::Zeroizing::new(keys.active_key);
    let retired = zeroize::Zeroizing::new(keys.retired);
    let keyring = crate::config::parse_secret_keyring(&keys.active_kid, &active, &retired)?;
    let mut count = 0;
    loop {
        let raw = line(&mut input)?;
        if raw.is_empty() {
            break;
        }
        let row: Row = serde_json::from_str(&raw)?;
        let envelope = Envelope {
            kek_kid: row.kek_kid,
            ciphertext: hex(&row.ciphertext)?,
            nonce: hex(&row.nonce)?,
            wrapped_dek: hex(&row.wrapped_dek)?,
            dek_nonce: hex(&row.dek_nonce)?,
            value_len: row.value_len,
        };
        let value = secrets::decrypt(
            &keyring,
            &envelope,
            &row.tenant_id,
            &row.component_id,
            &row.secret_id,
            &row.name,
            row.version,
        )?;
        anyhow::ensure!(
            value.len() == usize::try_from(row.value_len)?,
            "secret length mismatch"
        );
        count += 1;
    }
    Ok(count)
}
pub(crate) fn run() -> anyhow::Result<()> {
    // Collapse parse errors too: serde diagnostics may include offending input.
    let count = verify(std::io::stdin().lock())
        .map_err(|_| anyhow::anyhow!("backup secret verification failed"))?;
    println!("{count}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> String {
        let keyring =
            secrets::SecretKeyring::new("rotated-k2".into(), [2; 32], vec![("k1".into(), [1; 32])]);
        let env = secrets::encrypt(&keyring, "t", "c", "s", "TOKEN", 1, b"private").unwrap();
        let hex = |v: &[u8]| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let keys = serde_json::json!({"active_kid":"rotated-k2","active_key":"02".repeat(32),"retired":format!("k1:{}", "01".repeat(32))});
        let row = serde_json::json!({"tenant_id":"t","component_id":"c","secret_id":"s","name":"TOKEN","version":1,
            "kek_kid":env.kek_kid,"ciphertext":hex(&env.ciphertext),"nonce":hex(&env.nonce),
            "wrapped_dek":hex(&env.wrapped_dek),"dek_nonce":hex(&env.dek_nonce),"value_len":env.value_len});
        format!("{keys}\n{row}\n")
    }
    #[test]
    fn restored_rotation_requires_matching_key_identity_and_material() {
        let data = fixture();
        assert_eq!(verify(data.as_bytes()).unwrap(), 1);
        // Only the key configuration changes: DB envelope keeps its original kid.
        assert!(verify(data.replacen("rotated-k2", "k1", 1).as_bytes()).is_err());
        assert!(verify(data.replace(&"02".repeat(32), &"03".repeat(32)).as_bytes()).is_err());
        assert!(verify(
            data.replace("\"component_id\":\"c\"", "\"component_id\":\"other\"")
                .as_bytes()
        )
        .is_err());
    }
}
