//! ジョブ署名トークンの署名 / 検証 (M3c, §3.3「結果の出所認証」)。
//!
//! control-plane だけが Ed25519 署名鍵を持つ（worker は鍵なし）。invoke handler が
//! ジョブごとの [`JobClaims`] を署名して不透明な `job_token` を mint し、worker が
//! result に verbatim に echo、Control Plane がここで kid と署名を検証する。
//!
//! 設計上の最重要点:
//! - 署名対象バイトは **必ず** `hibana_shared::job_claims_signing_bytes` が組み立てる
//!   正準形（長さ前置・固定順・ドメイン分離）を使う。搬送 JSON のキー順・空白には
//!   一切依存しない（[`Signer::verify`] は受け取った JSON を一度 [`JobClaims`] に
//!   パースしてから正準バイトを **再導出** する）。
//! - 検証は `verify_strict`（Ed25519 の弱鍵/非正準 S を弾く厳格版）を使う。
//! - kid は設定済みの鍵 ID と照合し、未知の鍵は拒否する。
//!
//! wire 形: `job_token = b64url_nopad(json(JobClaims)) "." b64url_nopad(sig_64)`。

use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine as _,
};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use hibana_shared::{
    b64url_decode, b64url_encode, env_claims_signing_bytes, job_claims_signing_bytes, EnvClaims,
    JobClaims,
};

/// 署名 / 検証の失敗理由（audit detail の `reason` に載せる安定文字列）。
///
/// いずれも生トークン・秘密を含まない（理由ラベルのみ。§3.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// `payload.sig` の 2 セグメント形でない（`.` 区切りが 1 個でない）。
    MalformedToken,
    /// payload セグメントの base64url 復号に失敗。
    PayloadDecode,
    /// payload JSON を要求された用途の claim にパースできない。
    PayloadJson,
    /// signature セグメントの base64url 復号に失敗、または 64 バイトでない。
    SignatureDecode,
    /// claim の kid が設定済みの鍵 ID と一致しない。
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

/// Internal token signing and verification with the configured key and key ID.
/// The library's SigningKey already contains its corresponding verifying key.
pub struct Signer {
    key: SigningKey,
    kid: String,
}

impl Signer {
    pub fn from_seed(seed: [u8; 32], kid: String) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
            kid,
        }
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn sign(&self, claims: &JobClaims) -> String {
        self.sign_token(claims, &job_claims_signing_bytes(claims))
    }

    pub fn sign_env(&self, claims: &EnvClaims) -> String {
        self.sign_token(claims, &env_claims_signing_bytes(claims))
    }

    pub fn sign_preparation(&self, claims: &hibana_shared::preparation::Claims) -> String {
        self.sign_token(claims, &claims.signing_bytes())
    }

    fn sign_token(&self, claims: &impl serde::Serialize, signing_bytes: &[u8]) -> String {
        let signature: Signature = self.key.sign(signing_bytes);
        format!(
            "{}.{}",
            b64url_encode(&serde_json::to_vec(claims).expect("claims serialize")),
            b64url_encode(&signature.to_bytes())
        )
    }

    /// Verify authenticity only. Callers enforce expiry and execution provenance;
    /// admitted work can still complete after its admission token has expired.
    pub fn verify(&self, token: &str) -> Result<JobClaims, VerifyError> {
        self.verify_token(token, |claims: &JobClaims| {
            (&claims.kid, job_claims_signing_bytes(claims))
        })
    }

    pub fn verify_env(&self, token: &str) -> Result<EnvClaims, VerifyError> {
        self.verify_token(token, |claims: &EnvClaims| {
            (&claims.kid, env_claims_signing_bytes(claims))
        })
    }

    pub fn verify_preparation(
        &self,
        token: &str,
    ) -> Result<hibana_shared::preparation::Claims, VerifyError> {
        self.verify_token(token, |claims: &hibana_shared::preparation::Claims| {
            (&claims.kid, claims.signing_bytes())
        })
    }

    // The three typed entry points select distinct canonical signing domains.
    // JSON transports claims; its whitespace/key order is never signed directly.
    fn verify_token<C: serde::de::DeserializeOwned>(
        &self,
        token: &str,
        signing_input: fn(&C) -> (&str, Vec<u8>),
    ) -> Result<C, VerifyError> {
        let (payload, signature) = token
            .split_once('.')
            .filter(|(_, signature)| !signature.contains('.'))
            .ok_or(VerifyError::MalformedToken)?;
        let payload = b64url_decode(payload).ok_or(VerifyError::PayloadDecode)?;
        let claims: C = serde_json::from_slice(&payload).map_err(|_| VerifyError::PayloadJson)?;
        let signature = b64url_decode(signature).ok_or(VerifyError::SignatureDecode)?;
        let signature =
            Signature::from_slice(&signature).map_err(|_| VerifyError::SignatureDecode)?;
        let (kid, signing_bytes) = signing_input(&claims);
        if kid != self.kid {
            return Err(VerifyError::UnknownKid);
        }
        self.key
            .verify_strict(&signing_bytes, &signature)
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
        hex::decode_to_slice(raw, &mut out)
            .map_err(|_| anyhow::anyhow!("{env_name}: invalid hex"))?;
        return Ok(out);
    }

    // 2. base64url（no-pad）。
    if let Some(bytes) = b64url_decode(raw) {
        return to_seed32(bytes, env_name);
    }

    // 3. 標準 base64（'+'/'/'、パディングあり/なし）。不正な余剰ビットや末尾は拒否する。
    if let Ok(bytes) = STANDARD
        .decode(raw)
        .or_else(|_| STANDARD_NO_PAD.decode(raw))
    {
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

// ---------------------------------------------------------------------------
// M9a: Component 署名検証（§6.2 / §15 M9）
// ---------------------------------------------------------------------------

/// Component 署名の検証失敗理由（audit detail の `reason` に載せる安定文字列）。生の鍵・署名は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentSigError {
    /// 署名（base64url）が復号できない / 64 バイトでない。
    BadSignatureEncoding,
    /// 登録公開鍵が不正、または厳格な署名検証で拒否される弱い鍵。
    BadPublicKey,
    /// どの登録鍵でも検証に通らなかった。
    NoMatchingKey,
}

impl ComponentSigError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::BadSignatureEncoding => "bad_signature_encoding",
            Self::BadPublicKey => "bad_public_key",
            Self::NoMatchingKey => "no_matching_key",
        }
    }
}

impl std::fmt::Display for ComponentSigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason())
    }
}

/// wasm 本体の `wasm_sha256`（16 進小文字文字列）に対する detached Ed25519 署名を、
/// テナントが登録した**いずれかの**公開鍵（active / retired）で検証する（§6.2 M9a）。
///
/// - 署名対象は本体そのものではなく **sha256 のバイト列**（検証パイプラインが算出済みの
///   16 進文字列をそのまま UTF-8 バイトとして署名する。ダイジェストのテキスト表現に対して
///   署名することで、テナント側の署名ツールが `sha256_hex` だけを見て署名できる）。
/// - 複数鍵を順に試し、1 つでも通れば OK（ローテーション対応。retired 鍵でも検証は通す）。
/// - 生の署名 / 公開鍵はログにも戻り値にも載せない（理由ラベルのみ）。
///
/// `keys` は `(key_id, public_key_b64url)` の列。`signature_b64url` はテナントが付けた署名。
pub fn verify_component_signature(
    sha256_hex: &str,
    signature_b64url: &str,
    keys: &[(String, String)],
) -> Result<(), ComponentSigError> {
    let sig_bytes =
        b64url_decode(signature_b64url).ok_or(ComponentSigError::BadSignatureEncoding)?;
    let signature =
        Signature::from_slice(&sig_bytes).map_err(|_| ComponentSigError::BadSignatureEncoding)?;

    let message = sha256_hex.as_bytes();

    // 不正・弱い鍵をスキップし、有効な登録鍵をすべて試す。
    // 有効な鍵が無ければ BadPublicKey、署名が一致しなければ NoMatchingKey。
    let mut any_usable_key = false;
    for (_key_id, pk_b64) in keys {
        let Ok(vk) = decode_public_key(pk_b64) else {
            continue;
        };
        any_usable_key = true;
        if vk.verify_strict(message, &signature).is_ok() {
            return Ok(());
        }
    }

    if !any_usable_key {
        return Err(ComponentSigError::BadPublicKey);
    }
    Err(ComponentSigError::NoMatchingKey)
}

/// 登録用の公開鍵文字列（base64url, パディング無し, 32 バイト）を検証する。
///
/// admin が鍵を登録するときの入力バリデーション。復号できて 32 バイトかつ Ed25519 の
/// 妥当な点であり弱い鍵でないことを確認する（署名検証に使える鍵だけを DB に入れる）。
pub fn validate_public_key_b64url(public_key: &str) -> Result<(), ComponentSigError> {
    decode_public_key(public_key).map(|_| ())
}

fn decode_public_key(public_key: &str) -> Result<VerifyingKey, ComponentSigError> {
    let bytes = b64url_decode(public_key).ok_or(ComponentSigError::BadPublicKey)?;
    let key =
        VerifyingKey::try_from(bytes.as_slice()).map_err(|_| ComponentSigError::BadPublicKey)?;
    if key.is_weak() {
        return Err(ComponentSigError::BadPublicKey);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    #[test]
    fn preparation_tokens_bind_artifact_and_cannot_authorize_execution() {
        use hibana_shared::preparation::Claims;
        let signer = super::Signer::from_seed([7; 32], "prepare-test".into());
        let claims = Claims {
            tenant_id: "tenant".into(),
            storage_uri: "tenant/versions/ver_1.wasm".into(),
            sha256: "a".repeat(64),
            kid: "prepare-test".into(),
            iat: 1000,
            exp: 1120,
        };
        let token = signer.sign_preparation(&claims);
        assert_eq!(signer.verify_preparation(&token).unwrap(), claims);
        assert!(claims.valid_at(1000));
        assert!(!claims.valid_at(1120));
        assert!(!claims.valid_at(994));
        assert!(signer.verify(&token).is_err());
        assert!(signer.verify_env(&token).is_err());
        let job = hibana_shared::JobClaims {
            execution_id: "exec".into(),
            tenant_id: "tenant".into(),
            version_id: "version".into(),
            kid: "prepare-test".into(),
            iat: 1000,
            exp: 1120,
        };
        assert!(signer.verify_preparation(&signer.sign(&job)).is_err());
        let (_, signature) = token.split_once('.').unwrap();
        for changed in [
            Claims {
                sha256: "b".repeat(64),
                ..claims.clone()
            },
            Claims {
                storage_uri: "tenant/versions/ver_other.wasm".into(),
                ..claims.clone()
            },
            Claims {
                tenant_id: "other".into(),
                ..claims.clone()
            },
        ] {
            let forged = format!(
                "{}.{}",
                hibana_shared::b64url_encode(&serde_json::to_vec(&changed).unwrap()),
                signature
            );
            assert!(signer.verify_preparation(&forged).is_err());
        }
    }
    use super::*;

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

    // --- M9a: Component 署名検証 ---

    /// テスト用に (public_key_b64url, sha256_hex に対する署名 b64url) を作る。
    fn sign_component(seed: [u8; 32], sha256_hex: &str) -> (String, String) {
        let sk = SigningKey::from_bytes(&seed);
        let vk = sk.verifying_key();
        let sig = sk.sign(sha256_hex.as_bytes());
        (b64url_encode(vk.as_bytes()), b64url_encode(&sig.to_bytes()))
    }

    #[test]
    fn component_signature_roundtrip() {
        let sha = "abcd1234".repeat(8); // 64 hex chars
        let (pk, sig) = sign_component([9u8; 32], &sha);
        assert!(verify_component_signature(&sha, &sig, &[("k1".into(), pk)]).is_ok());
    }

    #[test]
    fn component_signature_rejects_wrong_digest() {
        let (pk, sig) = sign_component([9u8; 32], &"a".repeat(64));
        // 別のダイジェストに対しては通らない（本体を差し替えたら検出される）。
        let err =
            verify_component_signature(&"b".repeat(64), &sig, &[("k1".into(), pk)]).unwrap_err();
        assert_eq!(err, ComponentSigError::NoMatchingKey);
    }

    #[test]
    fn component_signature_rejects_wrong_key() {
        let sha = "c".repeat(64);
        let (_pk_real, sig) = sign_component([9u8; 32], &sha);
        let (pk_other, _) = sign_component([1u8; 32], &sha);
        // 別の鍵で検証しようとすると通らない。
        let err = verify_component_signature(&sha, &sig, &[("k1".into(), pk_other)]).unwrap_err();
        assert_eq!(err, ComponentSigError::NoMatchingKey);
    }

    #[test]
    fn component_signature_tries_all_keys_incl_retired() {
        let sha = "d".repeat(64);
        let (pk_real, sig) = sign_component([9u8; 32], &sha);
        let (pk_other, _) = sign_component([2u8; 32], &sha);
        // 正しい鍵が 2 番目でも見つかる（ローテーション: 複数鍵を順に試す）。
        assert!(verify_component_signature(
            &sha,
            &sig,
            &[("old".into(), pk_other), ("new".into(), pk_real)],
        )
        .is_ok());
    }

    #[test]
    fn component_signature_bad_encodings() {
        let sha = "e".repeat(64);
        let (pk, _) = sign_component([9u8; 32], &sha);
        // 署名が壊れている。
        assert_eq!(
            verify_component_signature(&sha, "!!!not-b64!!!", &[("k1".into(), pk.clone())]),
            Err(ComponentSigError::BadSignatureEncoding)
        );
        // 登録鍵が全部壊れている。
        let (_pk, sig) = sign_component([9u8; 32], &sha);
        assert_eq!(
            verify_component_signature(&sha, &sig, &[("k1".into(), "not-a-key".into())]),
            Err(ComponentSigError::BadPublicKey)
        );
    }

    #[test]
    fn public_key_validation() {
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let pk = b64url_encode(sk.verifying_key().as_bytes());
        assert!(validate_public_key_b64url(&pk).is_ok());
        assert!(validate_public_key_b64url("short").is_err());
        assert!(validate_public_key_b64url(&b64url_encode(&[0u8; 10])).is_err());
    }

    #[test]
    fn weak_public_keys_are_rejected_before_registration() {
        let mut identity = [0; 32];
        identity[0] = 1;
        for bytes in [[0; 32], identity] {
            assert!(VerifyingKey::from_bytes(&bytes).unwrap().is_weak());
            let encoded = b64url_encode(&bytes);
            assert_eq!(
                validate_public_key_b64url(&encoded),
                Err(ComponentSigError::BadPublicKey)
            );
        }
    }

    #[test]
    fn token_domains_stay_distinct_even_when_the_payload_fits_every_claim_type() {
        let s = signer();
        let job = claims(1_700_000_120);
        let env = EnvClaims {
            execution_id: job.execution_id.clone(),
            tenant_id: job.tenant_id.clone(),
            version_id: job.version_id.clone(),
            component_id: "cmp_1".into(),
            aud: hibana_shared::ENV_TOKEN_AUDIENCE.into(),
            kid: job.kid.clone(),
            iat: job.iat,
            exp: job.exp,
        };
        let preparation = hibana_shared::preparation::Claims {
            tenant_id: job.tenant_id.clone(),
            storage_uri: "ten_xyz/versions/ver_123.wasm".into(),
            sha256: "a".repeat(64),
            kid: job.kid.clone(),
            iat: job.iat,
            exp: job.exp,
        };
        let mut combined = serde_json::Map::new();
        for value in [
            serde_json::to_value(&job).unwrap(),
            serde_json::to_value(&env).unwrap(),
            serde_json::to_value(&preparation).unwrap(),
        ] {
            combined.extend(value.as_object().unwrap().clone());
        }
        // A hybrid object fits all schemas. Only the signing domain can reject
        // substitution; JSON key order and whitespace must not affect it.
        let payload = b64url_encode(&serde_json::to_vec_pretty(&combined).unwrap());
        fn verify_all(s: &Signer, token: &str) -> [Result<(), VerifyError>; 3] {
            [
                s.verify(token).map(|_| ()),
                s.verify_env(token).map(|_| ()),
                s.verify_preparation(token).map(|_| ()),
            ]
        }
        for (domain, token) in [
            s.sign(&job),
            s.sign_env(&env),
            s.sign_preparation(&preparation),
        ]
        .iter()
        .enumerate()
        {
            let (original_payload, signature) = token.split_once('.').unwrap();
            let hybrid = format!("{payload}.{signature}");
            for (checked_domain, outcome) in verify_all(&s, &hybrid).into_iter().enumerate() {
                assert_eq!(
                    outcome,
                    if checked_domain == domain {
                        Ok(())
                    } else {
                        Err(VerifyError::BadSignature)
                    }
                );
            }
            assert_eq!(
                verify_all(&Signer::from_seed([8; 32], "k1".into()), &hybrid)[domain],
                Err(VerifyError::BadSignature)
            );
            assert_eq!(
                verify_all(&Signer::from_seed([7; 32], "other".into()), &hybrid)[domain],
                Err(VerifyError::UnknownKid)
            );
            for (bad, expected) in [
                ("nodot".into(), VerifyError::MalformedToken),
                (format!("{token}.extra"), VerifyError::MalformedToken),
                (format!("a+b.{signature}"), VerifyError::PayloadDecode),
                (
                    format!("{original_payload}.{}", b64url_encode(&[0; 63])),
                    VerifyError::SignatureDecode,
                ),
            ] {
                assert_eq!(verify_all(&s, &bad)[domain], Err(expected));
            }
        }
    }

    /// sign -> verify の往復が一致する。
    #[test]
    fn sign_verify_roundtrip() {
        let s = signer();
        let c = claims(1_700_000_210);
        let token = s.sign(&c);
        let back = s.verify(&token).expect("verify ok");
        assert_eq!(back, c);
    }

    /// kid が token の中に入っており、active kid と一致する。
    #[test]
    fn token_carries_active_kid() {
        let s = signer();
        let c = claims(1_700_000_210);
        let token = s.sign(&c);
        let back = s.verify(&token).unwrap();
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
        assert_eq!(s.verify(&tampered), Err(VerifyError::BadSignature));
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
        assert_eq!(s.verify(&tampered), Err(VerifyError::BadSignature));
    }

    /// 別鍵で署名されたトークンは（同じ kid でも）BadSignature。
    #[test]
    fn wrong_signing_key_rejected() {
        let signer_a = signer();
        // 別の seed・同じ kid "k1" で署名する攻撃者。
        let attacker = Signer::from_seed([9u8; 32], "k1".to_string());
        let token = attacker.sign(&claims(1_700_000_210));
        // verifier は signer_a の公開鍵を kid "k1" に持つので検証は失敗する。
        assert_eq!(signer_a.verify(&token), Err(VerifyError::BadSignature));
    }

    /// 未知 kid は UnknownKid。
    #[test]
    fn unknown_kid_rejected() {
        let s = signer();
        let c = JobClaims {
            kid: "k99".into(),
            ..claims(1_700_000_210)
        };
        // claim を直接署名（kid=k99）するが、設定済みの鍵 ID は k1。
        let token = s.sign(&c);
        assert_eq!(s.verify(&token), Err(VerifyError::UnknownKid));
    }

    /// 形が壊れたトークンは MalformedToken。
    #[test]
    fn malformed_token_rejected() {
        let s = signer();
        assert_eq!(s.verify("nodot"), Err(VerifyError::MalformedToken));
        assert_eq!(s.verify("a.b.c"), Err(VerifyError::MalformedToken));
    }

    /// 不正な base64url payload / signature。
    #[test]
    fn bad_segments_rejected() {
        let s = signer();
        // payload が不正 base64url（'+' は url-safe では無効）。
        assert_eq!(s.verify("a+b.AAAA"), Err(VerifyError::PayloadDecode));
        // payload は復号できるが JSON でない。
        let not_json = b64url_encode(b"not json");
        let r = s.verify(&format!("{not_json}.AAAA"));
        assert_eq!(r, Err(VerifyError::PayloadJson));
        // payload は正しい JobClaims JSON だが、署名が 64 バイトでない。
        let payload = b64url_encode(&serde_json::to_vec(&claims(1)).unwrap());
        let short_sig = b64url_encode(&[0u8; 10]);
        assert_eq!(
            s.verify(&format!("{payload}.{short_sig}")),
            Err(VerifyError::SignatureDecode)
        );
    }

    /// 期限切れでも署名自体は有効。受付の期限と受付済み実行の完了条件は呼び出し側が判断する。
    #[test]
    fn expired_token_still_verifies_signature() {
        let s = signer();
        let c = claims(1); // 大昔の exp。
        let token = s.sign(&c);
        let back = s.verify(&token).expect("signature still valid");
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
        // Both standard alphabet characters, with and without padding.
        let seed = [0xfb; 32];
        let encoded = STANDARD.encode(seed);
        assert!(encoded.contains('+') && encoded.contains('/'));
        assert_eq!(decode_seed(&encoded).unwrap(), seed);
        assert_eq!(decode_seed(encoded.trim_end_matches('=')).unwrap(), seed);
    }

    #[test]
    fn decode_seed_rejects_malformed_base64() {
        let raw = "A".repeat(43); // 32 zero bytes, before padding.
        for invalid in [
            format!("{raw}=garbage"),
            format!("{raw}=="),
            format!("{raw}A="), // Wrong padding after a complete group.
            format!("{}B=", "A".repeat(42)), // Nonzero trailing bits.
            format!("{}B", "A".repeat(42)),
        ] {
            assert!(decode_seed(&invalid).is_err());
        }
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
        let iat = 1_700_000_000i64;
        let exp = iat + 180;
        let s = signer();
        let c = JobClaims {
            iat,
            exp,
            ..claims(0)
        };
        let token = s.sign(&c);
        assert_eq!(s.verify(&token).unwrap().exp, exp);
    }
}
