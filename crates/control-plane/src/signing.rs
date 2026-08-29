//! ジョブ署名トークンの署名 / 検証 (M3c, §3.3「結果の出所認証」)。
//!
//! control-plane だけが Ed25519 署名鍵を持つ（worker は鍵なし）。invoke handler が
//! ジョブごとの [`JobClaims`] を署名して不透明な `job_token` を mint し、worker が
//! result に verbatim に echo、subscriber がここで kid を選んで検証する。
//!
//! 設計上の最重要点:
//! - 署名対象バイトは **必ず** `faas_shared::job_claims_signing_bytes` が組み立てる
//!   正準形（長さ前置・固定順・ドメイン分離）を使う。搬送 JSON のキー順・空白には
//!   一切依存しない（[`Verifier::verify`] は受け取った JSON を一度 [`JobClaims`] に
//!   パースしてから正準バイトを **再導出** する）。
//! - 検証は `verify_strict`（Ed25519 の弱鍵/非正準 S を弾く厳格版）を使う。
//! - kid -> 公開鍵のマップで鍵を選ぶ（rotation-ready。現状は 1 エントリ）。
//!
//! wire 形: `job_token = b64url_nopad(json(JobClaims)) "." b64url_nopad(sig_64)`。

use std::collections::HashMap;

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use faas_shared::{b64url_decode, b64url_encode, job_claims_signing_bytes, JobClaims};

/// 署名 / 検証の失敗理由（audit detail の `reason` に載せる安定文字列）。
///
/// いずれも生トークン・秘密を含まない（理由ラベルのみ。§3.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// `payload.sig` の 2 セグメント形でない（`.` 区切りが 1 個でない）。
    MalformedToken,
    /// payload セグメントの base64url 復号に失敗。
    PayloadDecode,
    /// payload JSON を JobClaims にパースできない。
    PayloadJson,
    /// signature セグメントの base64url 復号に失敗、または 64 バイトでない。
    SignatureDecode,
    /// claim の kid が検証鍵マップに無い（未知 kid）。
    UnknownKid,
    /// Ed25519 署名検証に失敗（改竄・別鍵）。
    BadSignature,
}

impl VerifyError {
    /// audit / ログ用の安定文字列。
    pub fn reason(&self) -> &'static str {
        match self {
            VerifyError::MalformedToken => "malformed_token",
            VerifyError::PayloadDecode => "payload_decode",
            VerifyError::PayloadJson => "payload_json",
            VerifyError::SignatureDecode => "signature_decode",
            VerifyError::UnknownKid => "unknown_kid",
            VerifyError::BadSignature => "bad_signature",
        }
    }
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// Ed25519 署名器。単一の署名鍵と、それに紐づく active kid を持つ。
///
/// `verify` も兼ねられるよう、内部に [`Verifier`]（kid -> 公開鍵マップ）を保持する。
/// 現状はマップに 1 エントリ（active kid -> 自鍵の公開鍵）だけ入る（rotation-ready）。
pub struct Signer {
    key: SigningKey,
    kid: String,
    verifier: Verifier,
}

impl Signer {
    /// 32 バイトの Ed25519 seed と kid から署名器を構築する。
    ///
    /// seed から導出した公開鍵を `kid` で検証マップに登録する。
    pub fn from_seed(seed: [u8; 32], kid: String) -> Self {
        let key = SigningKey::from_bytes(&seed);
        let verifying = key.verifying_key();
        let mut map = HashMap::new();
        map.insert(kid.clone(), verifying);
        Signer {
            key,
            kid,
            verifier: Verifier { keys: map },
        }
    }

    /// active kid。
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// claim を署名し、不透明な `job_token` 文字列を返す。
    ///
    /// 署名対象は **正準バイト**（`job_claims_signing_bytes`）。wire には JSON を
    /// 載せるが、検証側はその JSON 順序を信用せず正準バイトを再導出する。
    pub fn sign(&self, claims: &JobClaims) -> String {
        let signing_bytes = job_claims_signing_bytes(claims);
        let sig: Signature = self.key.sign(&signing_bytes);
        // 搬送 JSON。serde_json の出力順は検証に影響しない（再正準化するため）。
        let payload = serde_json::to_vec(claims).expect("JobClaims serialize never fails");
        format!(
            "{}.{}",
            b64url_encode(&payload),
            b64url_encode(&sig.to_bytes())
        )
    }

    /// 検証器への参照（subscriber が `state.verifier()` 経由で使う）。
    pub fn verifier(&self) -> &Verifier {
        &self.verifier
    }
}

/// kid -> 公開鍵のマップで `job_token` を検証する（rotation-ready）。
pub struct Verifier {
    keys: HashMap<String, VerifyingKey>,
}

impl Verifier {
    /// `job_token` を検証し、成功時に正準パース済みの [`JobClaims`] を返す。
    ///
    /// 手順（§3.3）:
    /// 1. `.` で 2 セグメントに分割。
    /// 2. seg0 を b64url 復号 -> JSON -> [`JobClaims`]。
    /// 3. `job_claims_signing_bytes(&claims)` で **正準バイトを再導出**（JSON 順序非依存）。
    /// 4. seg1 を b64url 復号 -> 64 バイト署名。
    /// 5. claims.kid で公開鍵を選び `verify_strict`。
    ///
    /// claim 内容（execution_id / tenant_id / version_id の行突き合わせ・exp 判定）は
    /// 署名検証の **後** に subscriber 側で行う（ここは署名の真正性のみ判定する）。
    pub fn verify(&self, token: &str) -> Result<JobClaims, VerifyError> {
        let mut parts = token.split('.');
        let seg0 = parts.next().ok_or(VerifyError::MalformedToken)?;
        let seg1 = parts.next().ok_or(VerifyError::MalformedToken)?;
        if parts.next().is_some() {
            return Err(VerifyError::MalformedToken);
        }

        let payload = b64url_decode(seg0).ok_or(VerifyError::PayloadDecode)?;
        let claims: JobClaims =
            serde_json::from_slice(&payload).map_err(|_| VerifyError::PayloadJson)?;

        let sig_bytes = b64url_decode(seg1).ok_or(VerifyError::SignatureDecode)?;
        let sig_arr: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| VerifyError::SignatureDecode)?;
        let signature = Signature::from_bytes(&sig_arr);

        let key = self.keys.get(&claims.kid).ok_or(VerifyError::UnknownKid)?;

        // 検証も signer と同一の正準バイトを再導出して行う（JSON 順序を信用しない）。
        let signing_bytes = job_claims_signing_bytes(&claims);
        key.verify_strict(&signing_bytes, &signature)
            .map_err(|_| VerifyError::BadSignature)?;

        Ok(claims)
    }
}

/// `JOB_SIGNING_KEY` env 値（base64url / base64 / hex のいずれか）を 32 バイト
/// Ed25519 seed に復号する。
///
/// 受理形式（順に試す）:
/// - 64 文字の hex（小文字/大文字）
/// - base64url（no-pad）で 32 バイトにデコードされるもの
/// - base64（標準、`+`/`/`、`=` パディング可）で 32 バイトにデコードされるもの
///
/// いずれも 32 バイトちょうどでなければエラー。
pub fn decode_seed(raw: &str) -> anyhow::Result<[u8; 32]> {
    decode_key32(raw, "JOB_SIGNING_KEY")
}

/// 32 バイト鍵素材を hex / base64url / base64 のいずれかから復号する（M7c で一般化）。
///
/// `env_name` はエラーメッセージに埋める env 変数名。`decode_seed` は
/// `decode_key32(raw, "JOB_SIGNING_KEY")` の薄いラッパであり、M7c の `SECRETS_MASTER_KEY` /
/// `SECRETS_RETIRED_KEYS` も同じ復号規則を共有する（鍵素材の受理形式を 1 箇所に保つ）。
pub fn decode_key32(raw: &str, env_name: &str) -> anyhow::Result<[u8; 32]> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("{env_name} is empty");
    }

    // 1. hex（64 文字）。
    if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            let hi = hex_val(raw.as_bytes()[i * 2]);
            let lo = hex_val(raw.as_bytes()[i * 2 + 1]);
            match (hi, lo) {
                (Some(h), Some(l)) => *byte = (h << 4) | l,
                _ => anyhow::bail!("{env_name}: invalid hex"),
            }
        }
        return Ok(out);
    }

    // 2. base64url（no-pad）。
    if let Some(bytes) = b64url_decode(raw) {
        return to_seed32(bytes, env_name);
    }

    // 3. 標準 base64（'+'/'/' と '=' パディング）。手実装（外部 base64 依存を避ける）。
    if let Some(bytes) = std_base64_decode(raw) {
        return to_seed32(bytes, env_name);
    }

    anyhow::bail!("{env_name}: not valid hex / base64url / base64 of a 32-byte seed")
}

fn to_seed32(bytes: Vec<u8>, env_name: &str) -> anyhow::Result<[u8; 32]> {
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("{env_name} must decode to exactly 32 bytes"))?;
    Ok(arr)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 標準 base64（RFC 4648 §4、`+`/`/`、`=` パディング許容）の最小デコーダ。
/// dev 用の seed 受理のためだけに使う（厳密な正準性検査はしない）。
fn std_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faas_shared::{ACK_WAIT_SECS, MAX_DELIVER, TOKEN_MARGIN_SECS};

    fn signer() -> Signer {
        // 決定的な dev seed（テスト専用）。
        let seed = [7u8; 32];
        Signer::from_seed(seed, "k1".to_string())
    }

    fn claims(exp: i64) -> JobClaims {
        JobClaims {
            execution_id: "exec_abc".into(),
            tenant_id: "ten_xyz".into(),
            version_id: "ver_123".into(),
            kid: "k1".into(),
            iat: 1_700_000_000,
            exp,
        }
    }

    /// sign -> verify の往復が一致する。
    #[test]
    fn sign_verify_roundtrip() {
        let s = signer();
        let c = claims(1_700_000_210);
        let token = s.sign(&c);
        let back = s.verifier().verify(&token).expect("verify ok");
        assert_eq!(back, c);
    }

    /// kid が token の中に入っており、active kid と一致する。
    #[test]
    fn token_carries_active_kid() {
        let s = signer();
        let c = claims(1_700_000_210);
        let token = s.sign(&c);
        let back = s.verifier().verify(&token).unwrap();
        assert_eq!(back.kid, s.kid());
    }

    /// 署名を改竄すると BadSignature。
    #[test]
    fn tampered_signature_rejected() {
        let s = signer();
        let token = s.sign(&claims(1_700_000_210));
        let (payload, sig) = token.split_once('.').unwrap();
        // 署名の最後の 1 文字を別の url-safe 文字に変える。
        let mut sig_chars: Vec<char> = sig.chars().collect();
        let last = sig_chars.last_mut().unwrap();
        *last = if *last == 'A' { 'B' } else { 'A' };
        let bad: String = sig_chars.into_iter().collect();
        let tampered = format!("{payload}.{bad}");
        assert_eq!(
            s.verifier().verify(&tampered),
            Err(VerifyError::BadSignature)
        );
    }

    /// payload(claim) を差し替えると、署名は元 claim に対するものなので BadSignature。
    #[test]
    fn tampered_claim_rejected() {
        let s = signer();
        let token = s.sign(&claims(1_700_000_210));
        let (_payload, sig) = token.split_once('.').unwrap();
        // 別 claim（tenant を変えた）の payload を載せる。
        let forged = JobClaims {
            tenant_id: "ten_attacker".into(),
            ..claims(1_700_000_210)
        };
        let forged_payload = b64url_encode(&serde_json::to_vec(&forged).unwrap());
        let tampered = format!("{forged_payload}.{sig}");
        assert_eq!(
            s.verifier().verify(&tampered),
            Err(VerifyError::BadSignature)
        );
    }

    /// 別鍵で署名されたトークンは（同じ kid でも）BadSignature。
    #[test]
    fn wrong_signing_key_rejected() {
        let signer_a = signer();
        // 別の seed・同じ kid "k1" で署名する攻撃者。
        let attacker = Signer::from_seed([9u8; 32], "k1".to_string());
        let token = attacker.sign(&claims(1_700_000_210));
        // verifier は signer_a の公開鍵を kid "k1" に持つので検証は失敗する。
        assert_eq!(
            signer_a.verifier().verify(&token),
            Err(VerifyError::BadSignature)
        );
    }

    /// 未知 kid は UnknownKid。
    #[test]
    fn unknown_kid_rejected() {
        let s = signer();
        let c = JobClaims {
            kid: "k99".into(),
            ..claims(1_700_000_210)
        };
        // claim を直接署名（kid=k99）するが、verifier マップには k1 しか無い。
        let token = s.sign(&c);
        assert_eq!(s.verifier().verify(&token), Err(VerifyError::UnknownKid));
    }

    /// 形が壊れたトークンは MalformedToken。
    #[test]
    fn malformed_token_rejected() {
        let s = signer();
        assert_eq!(
            s.verifier().verify("nodot"),
            Err(VerifyError::MalformedToken)
        );
        assert_eq!(
            s.verifier().verify("a.b.c"),
            Err(VerifyError::MalformedToken)
        );
    }

    /// 不正な base64url payload / signature。
    #[test]
    fn bad_segments_rejected() {
        let s = signer();
        // payload が不正 base64url（'+' は url-safe では無効）。
        assert_eq!(
            s.verifier().verify("a+b.AAAA"),
            Err(VerifyError::PayloadDecode)
        );
        // payload は復号できるが JSON でない。
        let not_json = b64url_encode(b"not json");
        let r = s.verifier().verify(&format!("{not_json}.AAAA"));
        assert_eq!(r, Err(VerifyError::PayloadJson));
        // payload は正しい JobClaims JSON だが、署名が 64 バイトでない。
        let payload = b64url_encode(&serde_json::to_vec(&claims(1)).unwrap());
        let short_sig = b64url_encode(&[0u8; 10]);
        assert_eq!(
            s.verifier().verify(&format!("{payload}.{short_sig}")),
            Err(VerifyError::SignatureDecode)
        );
    }

    /// expired なトークンも署名自体は有効（検証は通る）。exp は subscriber が
    /// 「pending/running なら受理」の判定に使うのであって、ここでは弾かない。
    #[test]
    fn expired_token_still_verifies_signature() {
        let s = signer();
        let c = claims(1); // 大昔の exp。
        let token = s.sign(&c);
        let back = s.verifier().verify(&token).expect("signature still valid");
        assert_eq!(back.exp, 1);
    }

    /// decode_seed は hex / base64url / base64 を受理し、いずれも 32 バイトを要求する。
    #[test]
    fn decode_seed_accepts_formats() {
        // hex 64 文字。
        let hex = "00".repeat(32);
        assert_eq!(decode_seed(&hex).unwrap(), [0u8; 32]);
        // base64url（no-pad）の 32 ゼロバイト。
        let b64url = b64url_encode(&[0u8; 32]);
        assert_eq!(decode_seed(&b64url).unwrap(), [0u8; 32]);
        // 標準 base64（パディング付き）。
        // 32 ゼロバイトの標準 base64 = "AAAA...=" 形。手で作る。
        let std_b64 = {
            // 簡便に: url-safe を標準へ変換（このベクタには +// が無いのでそのまま + '='）。
            let raw = b64url_encode(&[0u8; 32]);
            // no-pad -> pad（43 文字 -> 44 文字、'=' を 1 つ補う）。
            format!("{raw}=")
        };
        assert_eq!(decode_seed(&std_b64).unwrap(), [0u8; 32]);
    }

    /// 32 バイトにならない seed は拒否。
    #[test]
    fn decode_seed_rejects_wrong_length() {
        assert!(decode_seed("00").is_err());
        assert!(decode_seed("").is_err());
        assert!(decode_seed(&b64url_encode(&[0u8; 16])).is_err());
    }

    /// TTL 定数から計算した exp を持つ claim を sign->verify できる（結合の健全性）。
    #[test]
    fn ttl_derived_exp_roundtrips() {
        use faas_shared::token_exp_offset_secs;
        let iat = 1_700_000_000i64;
        let exp = iat + token_exp_offset_secs(1, ACK_WAIT_SECS, MAX_DELIVER, TOKEN_MARGIN_SECS);
        let s = signer();
        let c = JobClaims {
            iat,
            exp,
            ..claims(0)
        };
        let token = s.sign(&c);
        assert_eq!(s.verifier().verify(&token).unwrap().exp, exp);
    }
}
