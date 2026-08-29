//! Secrets Manager の暗号層 (M7c, §10 / §15)。
//!
//! **このファイルは `scripts/rls-lint.sh` の検査 (4) の allowlist に入る**（平文を扱う唯一の場所の 1 つ）。
//!
//! ## `auth::hash_token` との違い（混同しないこと）
//!
//! `auth::hash_token`（sha256）と `crypto::hash_password`（argon2id）は**不可逆**ハッシュであり、
//! 「提示された値が正しいか」だけを判定する用途に使う（`0003_auth.sql` の一方向ハッシュ）。
//! secret は **復号して worker へ渡す必要がある**ため、まったく別系統の可逆暗号を使う。
//! この 2 つを取り違えると「secret をハッシュして保存し、二度と取り出せない」か、逆に
//! 「パスワードを可逆暗号で保存する」という重大な誤りになる。
//!
//! ## 封筒（envelope）構成
//!
//! ```text
//! SECRETS_MASTER_KEY (env, 32B) ── kid で選択 ──> KEK
//!                                                  │  AEAD(KEK, dek_nonce, DEK, aad=dek_aad)
//!                                                  ▼
//!                               行ごとにランダムな DEK (32B, OsRng)
//!                                                  │  AEAD(DEK, nonce, plaintext, aad=value_aad)
//!                                                  ▼
//!                                             ciphertext
//! ```
//!
//! **なぜ鍵導出（HKDF only）ではなく真の封筒か**: 版台帳が追記専用なので、KEK ローテーションは
//! 「新 version 行を INSERT」で表現する。封筒なら **値の平文をメモリに載せずに** KEK を回せる
//! （DEK を unwrap → 新 KEK で wrap するだけ）。これが封筒の実利である。
//!
//! **ただしこの再ラップは侵害復旧にはならない** (§4.7): DEK も ciphertext も不変なので、
//! 旧 KEK と旧 DB ダンプを持つ攻撃者は再ラップ後も全平文を復元できる。rekey は
//! **KEK の計画的ローテーション専用**であり、侵害時の唯一の復旧経路は**値そのものの rotate**である。
//!
//! ## AEAD の選定
//!
//! `XChaCha20Poly1305`（RustCrypto, pure-Rust）。24 バイト nonce なので毎回 `OsRng` で引いても
//! 衝突確率が無視でき、カウンタ管理（＝状態）が要らない（ステートレス×N の CP と噛み合う）。
//! ソフトウェア AES（AES-NI 非依存環境）と違い定数時間実装が素直、という点も選定理由。

use std::collections::HashMap;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;
use zeroize::Zeroizing;

/// 値本体の AAD ドメインタグ。
const VALUE_AAD_DOMAIN: &[u8] = b"faas-secret-value-v1";
/// DEK ラップの AAD ドメインタグ。
const DEK_AAD_DOMAIN: &[u8] = b"faas-secret-dek-v1";

/// XChaCha20-Poly1305 の nonce 長。
const NONCE_LEN: usize = 24;

/// 暗号層のエラー。**理由は列挙値としてのみ持ち、値や鍵素材は決して含めない**。
///
/// HTTP へは一律 500（`FaasError::Internal`）に写像し、ボディには reason も値も出さない
/// （`error.rs` の 5xx redaction と同じ思想）。`reason()` は**メトリクスラベル / 内部ログ専用**。
// 一部の variant / 関数は注入経路（M7c-3）と rekey（M7c-4）が着地するまで
// 本番コードから呼ばれない（現状はユニットテストのみが構築・呼び出しする）。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretError {
    /// 行が参照する kid がキーリングに無い（retired キーの早期撤去が典型）。
    UnknownKid,
    /// 封筒の形が壊れている（nonce 長違い等）。
    BadEnvelope,
    /// 復号失敗（AAD 不一致 = 貼り替え / 改竄、または鍵違い）。
    DecryptFailed,
    /// アクティブな KEK が解決できない（設定不備）。
    KeyMissing,
    /// execution 基準で世代を解決できなかった（§4.7）。
    VersionUnresolved,
}

impl SecretError {
    /// 安定した理由文字列（メトリクスラベル / 内部ログ用。**HTTP ボディには出さない**）。
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnknownKid => "unknown_kid",
            Self::BadEnvelope => "bad_envelope",
            Self::DecryptFailed => "decrypt_failed",
            Self::KeyMissing => "key_missing",
            Self::VersionUnresolved => "version_unresolved",
        }
    }
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for SecretError {}

/// KEK キーリング (§10)。**暗号化は常に `active_kid`、復号は行の kid で選ぶ**（rotation-ready）。
///
/// `signing.rs` の `Signer`（現行鍵 1 本 + kid）/ `Verifier`（kid -> 鍵）の 2 段構成をそのまま写す。
/// retired キーは復号専用で、再ラップが全行に行き渡るまで残す（早期撤去すると復号不能 ＝ データ喪失）。
pub struct SecretKeyring {
    active_kid: String,
    keys: HashMap<String, Zeroizing<[u8; 32]>>,
}

/// 鍵素材を絶対に出さない `Debug`（kid の一覧のみ。鍵長や先頭バイトも出さない）。
impl std::fmt::Debug for SecretKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretKeyring")
            .field("active_kid", &self.active_kid)
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SecretKeyring {
    /// active な KEK と retired KEK 群からキーリングを構築する。
    ///
    /// `retired` は `(kid, key)` の列。active と同じ kid が来たら active を優先する。
    pub fn new(active_kid: String, active_key: [u8; 32], retired: Vec<(String, [u8; 32])>) -> Self {
        let mut keys = HashMap::new();
        for (kid, key) in retired {
            keys.insert(kid, Zeroizing::new(key));
        }
        keys.insert(active_kid.clone(), Zeroizing::new(active_key));
        Self { active_kid, keys }
    }

    /// 新規暗号化に使う kid。
    pub fn active_kid(&self) -> &str {
        &self.active_kid
    }

    fn key_for(&self, kid: &str) -> Result<&[u8; 32], SecretError> {
        self.keys
            .get(kid)
            .map(|k| &**k)
            .ok_or(SecretError::UnknownKid)
    }
}

/// AEAD の AAD を正準化して組み立てる。
///
/// `faas_shared::job_claims_signing_bytes` と**同じ作法**（ドメインタグ + 4 バイト BE 長さ前置 +
/// UTF-8）。serde_json を使わないのは同じ理由 —— map/key 順序や空白が非正準で、書き手と読み手の
/// 間でドリフトすると復号が黙って壊れる（あるいは貼り替えを見逃す）。
fn aad_bytes(domain: &[u8], fields: &[&[u8]], trailing_be: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        4 + domain.len() + fields.iter().map(|f| 4 + f.len()).sum::<usize>() + trailing_be.len(),
    );
    write_len_prefixed(&mut out, domain);
    for f in fields {
        write_len_prefixed(&mut out, f);
    }
    out.extend_from_slice(trailing_be);
    out
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// 値本体の AAD。
///
/// `tenant_id` / `component_id` / `name` を束縛する。DB 書き込み権限を得た攻撃者が、他テナント /
/// 他 component / 他キー名の行へ暗号文を貼り替えても Poly1305 で復号失敗する（cut-and-paste 遮断）。
///
/// **`version` と `kek_kid` は意図的に含めない**: 再ラップ（`reason='rekey'`）で `ciphertext` を
/// そのままコピーできるようにするため。version の巻き戻し耐性は `dek_aad` 側が担う。
///
/// `component_id` を含めるため **secret を別 component へ移すことはできない**（移設は
/// 「新しい secret として登録し直す」）。
pub fn value_aad(tenant_id: &str, component_id: &str, name: &str) -> Vec<u8> {
    aad_bytes(
        VALUE_AAD_DOMAIN,
        &[
            tenant_id.as_bytes(),
            component_id.as_bytes(),
            name.as_bytes(),
        ],
        &[],
    )
}

/// DEK ラップの AAD。`tenant_id` / `secret_id` / `version` / `kek_kid` を束縛し、
/// ラップ済み DEK の他行への転用と **version の巻き戻し**を遮断する。
pub fn dek_aad(tenant_id: &str, secret_id: &str, version: i32, kek_kid: &str) -> Vec<u8> {
    aad_bytes(
        DEK_AAD_DOMAIN,
        &[
            tenant_id.as_bytes(),
            secret_id.as_bytes(),
            kek_kid.as_bytes(),
        ],
        &(version as u32).to_be_bytes(),
    )
}

/// 封筒 1 件分（`function_secret_versions` の 1 行に対応する暗号材料）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// 値本体の暗号文（AEAD tag 込み）。
    pub ciphertext: Vec<u8>,
    /// 値本体の nonce（24 バイト）。
    pub nonce: Vec<u8>,
    /// KEK でラップした DEK（AEAD tag 込み）。
    pub wrapped_dek: Vec<u8>,
    /// DEK ラップの nonce（24 バイト）。
    pub dek_nonce: Vec<u8>,
    /// ラップに使った KEK の kid。
    pub kek_kid: String,
    /// 平文のバイト長（運用の目安。**値そのものではない**）。
    pub value_len: i32,
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(Key::from_slice(key))
}

#[allow(dead_code)] // 復号経路（M7c-3）で使う。
fn to_nonce(bytes: &[u8]) -> Result<&XNonce, SecretError> {
    if bytes.len() != NONCE_LEN {
        return Err(SecretError::BadEnvelope);
    }
    Ok(XNonce::from_slice(bytes))
}

/// 平文を封筒暗号化する。
///
/// `version` は保存先の版番号（`dek_aad` に束縛される）。呼び出し側は INSERT する版と同じ値を渡すこと
/// （ずれると復号できない ＝ 書いた直後に読めないので、テストと e2e で必ず検出される）。
pub fn encrypt(
    keyring: &SecretKeyring,
    tenant_id: &str,
    component_id: &str,
    secret_id: &str,
    name: &str,
    version: i32,
    plaintext: &[u8],
) -> Result<Envelope, SecretError> {
    let kek_kid = keyring.active_kid().to_string();
    let kek = keyring.key_for(&kek_kid)?;

    // 行ごとにランダムな DEK。
    let dek = Zeroizing::new(random_bytes::<32>());

    // (1) 値本体を DEK で暗号化する。
    let nonce = random_bytes::<NONCE_LEN>();
    let ciphertext = cipher(&dek)
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &value_aad(tenant_id, component_id, name),
            },
        )
        .map_err(|_| SecretError::DecryptFailed)?;

    // (2) DEK を KEK でラップする。
    let dek_nonce = random_bytes::<NONCE_LEN>();
    let wrapped_dek = cipher(kek)
        .encrypt(
            XNonce::from_slice(&dek_nonce),
            Payload {
                msg: &dek[..],
                aad: &dek_aad(tenant_id, secret_id, version, &kek_kid),
            },
        )
        .map_err(|_| SecretError::DecryptFailed)?;

    Ok(Envelope {
        ciphertext,
        nonce: nonce.to_vec(),
        wrapped_dek,
        dek_nonce: dek_nonce.to_vec(),
        kek_kid,
        value_len: plaintext.len() as i32,
    })
}

/// 封筒を復号して平文を取り出す。返り値は `Zeroizing`（Drop でゼロ化）。
#[allow(dead_code)] // job-env 引き換え（M7c-3）が唯一の呼び出し元になる。
pub fn decrypt(
    keyring: &SecretKeyring,
    env: &Envelope,
    tenant_id: &str,
    component_id: &str,
    secret_id: &str,
    name: &str,
    version: i32,
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let kek = keyring.key_for(&env.kek_kid)?;

    // (1) DEK を unwrap する。
    let dek_vec = Zeroizing::new(
        cipher(kek)
            .decrypt(
                to_nonce(&env.dek_nonce)?,
                Payload {
                    msg: &env.wrapped_dek,
                    aad: &dek_aad(tenant_id, secret_id, version, &env.kek_kid),
                },
            )
            .map_err(|_| SecretError::DecryptFailed)?,
    );
    let dek: [u8; 32] = dek_vec
        .as_slice()
        .try_into()
        .map_err(|_| SecretError::BadEnvelope)?;
    let dek = Zeroizing::new(dek);

    // (2) 値本体を復号する。
    let plaintext = cipher(&dek)
        .decrypt(
            to_nonce(&env.nonce)?,
            Payload {
                msg: &env.ciphertext,
                aad: &value_aad(tenant_id, component_id, name),
            },
        )
        .map_err(|_| SecretError::DecryptFailed)?;

    Ok(Zeroizing::new(plaintext))
}

/// **wrap-only の再ラップ**（KEK ローテーション, §4.7）。
///
/// DEK を旧 KEK で unwrap し、active KEK で wrap し直すだけ。`ciphertext` / `nonce` / `value_len` は
/// **そのままコピー**する（値の平文をメモリに載せない）。新しい版番号 `new_version` を `dek_aad` に
/// 束縛するので、呼び出し側は INSERT する版と同じ値を渡すこと。
///
/// **これは侵害復旧ではない**: DEK も ciphertext も不変なので、旧 KEK + 旧 DB ダンプがあれば
/// 再ラップ後も全平文を復元できる。侵害時の唯一の復旧経路は**値そのものの rotate**。
#[allow(dead_code)] // KEK ローテーション（M7c-4 の POST /admin/secrets/rekey）で使う。
pub fn rewrap(
    keyring: &SecretKeyring,
    env: &Envelope,
    tenant_id: &str,
    secret_id: &str,
    old_version: i32,
    new_version: i32,
) -> Result<Envelope, SecretError> {
    let old_kek = keyring.key_for(&env.kek_kid)?;
    let dek = Zeroizing::new(
        cipher(old_kek)
            .decrypt(
                to_nonce(&env.dek_nonce)?,
                Payload {
                    msg: &env.wrapped_dek,
                    aad: &dek_aad(tenant_id, secret_id, old_version, &env.kek_kid),
                },
            )
            .map_err(|_| SecretError::DecryptFailed)?,
    );

    let new_kid = keyring.active_kid().to_string();
    let new_kek = keyring.key_for(&new_kid)?;
    let dek_nonce = random_bytes::<NONCE_LEN>();
    let wrapped_dek = cipher(new_kek)
        .encrypt(
            XNonce::from_slice(&dek_nonce),
            Payload {
                msg: &dek[..],
                aad: &dek_aad(tenant_id, secret_id, new_version, &new_kid),
            },
        )
        .map_err(|_| SecretError::DecryptFailed)?;

    Ok(Envelope {
        // 値そのものは触らない（平文をメモリに載せずに KEK だけ回すのが封筒の実利）。
        ciphertext: env.ciphertext.clone(),
        nonce: env.nonce.clone(),
        wrapped_dek,
        dek_nonce: dek_nonce.to_vec(),
        kek_kid: new_kid,
        value_len: env.value_len,
    })
}

/// **execution 基準**で解決した secret 1 件（注入用）。
pub struct ResolvedSecret {
    pub name: String,
    /// 実際に注入した世代（execution 基準で固定された値）。監査・デバッグ用。
    #[allow(dead_code)]
    pub version: i32,
    pub value: faas_shared::Redacted<String>,
}

/// job-env 引き換え専用の復号 API (§5.2)。**secrets.rs が公開する復号系はこれ 1 本だけ**。
///
/// # 世代を execution に固定する理由 (§4.7.1 MUST)
///
/// 「引き換え時点の `current_version` を解決する」実装だと、worker がクラッシュして再配送される
/// 間に `POST .../rotate` が走ったとき、同一 `execution_id` の 1 回目の試行は旧値・2 回目は新値で
/// 走る。at-least-once なので **両方の資格情報が外部 API へ到達しうる**（rotation の目的である
/// 「旧鍵を止める」が保証されず、旧鍵の最終使用時刻も特定できない）。版台帳が追記専用で旧世代を
/// 保持しているのだから、注入側もその利点を使う。
///
/// → `executions.created_at` 以前に作られた**最大 version** を `JOIN LATERAL` で取る。
/// `ON TRUE` は INNER 相当なので、**世代を 1 つも解決できない secret は行が返らない**。
/// 生存 secret があるのに世代が解決できないケースは呼び出し側が検出して fail-closed に倒す。
/// `reason='rekey'` 行は同一平文なので選ばれても等価。
///
/// `allowed` は `capabilities.env`（admin 承認）の許可リスト。**許可リストが権威**であり、
/// 値が DB に存在しても載っていない名前は返さない（worker 側にも同じフィルタがある二重防御）。
pub async fn resolve_for_injection(
    tx: &mut sqlx::PgConnection,
    keyring: &SecretKeyring,
    tenant_id: &str,
    component_id: &str,
    execution_created_at: chrono::DateTime<chrono::Utc>,
    allowed: &std::collections::BTreeSet<String>,
) -> Result<Vec<ResolvedSecret>, SecretError> {
    use sqlx::Row as _;

    if allowed.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = allowed.iter().cloned().collect();

    let rows = sqlx::query(SECRET_INJECTION_SQL)
        .bind(tenant_id)
        .bind(component_id)
        .bind(execution_created_at)
        .bind(&names)
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| SecretError::VersionUnresolved)?;

    // 生存 secret の件数と解決できた世代の件数が食い違ったら fail-closed（§4.7.1）。
    let live: i64 = sqlx::query(
        "SELECT count(*) AS n FROM function_secrets \
          WHERE tenant_id = $1 AND component_id = $2 AND deleted_at IS NULL AND name = ANY($3)",
    )
    .bind(tenant_id)
    .bind(component_id)
    .bind(&names)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| SecretError::VersionUnresolved)?
    .try_get("n")
    .map_err(|_| SecretError::VersionUnresolved)?;

    if live != rows.len() as i64 {
        return Err(SecretError::VersionUnresolved);
    }

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let secret_id: String = r
            .try_get("secret_id")
            .map_err(|_| SecretError::BadEnvelope)?;
        let name: String = r.try_get("name").map_err(|_| SecretError::BadEnvelope)?;
        let version: i32 = r.try_get("version").map_err(|_| SecretError::BadEnvelope)?;
        let envelope = Envelope {
            kek_kid: r.try_get("kek_kid").map_err(|_| SecretError::BadEnvelope)?,
            wrapped_dek: r
                .try_get("wrapped_dek")
                .map_err(|_| SecretError::BadEnvelope)?,
            dek_nonce: r
                .try_get("dek_nonce")
                .map_err(|_| SecretError::BadEnvelope)?,
            nonce: r.try_get("nonce").map_err(|_| SecretError::BadEnvelope)?,
            ciphertext: r
                .try_get("ciphertext")
                .map_err(|_| SecretError::BadEnvelope)?,
            value_len: r
                .try_get("value_len")
                .map_err(|_| SecretError::BadEnvelope)?,
        };

        let plaintext = decrypt(
            keyring,
            &envelope,
            tenant_id,
            component_id,
            &secret_id,
            &name,
            version,
        )?;
        let value = String::from_utf8(plaintext.to_vec()).map_err(|_| SecretError::BadEnvelope)?;

        out.push(ResolvedSecret {
            name,
            version,
            value: faas_shared::Redacted::new(value),
        });
    }
    Ok(out)
}

/// 注入クエリ (§4.7.1)。**`current_version` を参照しない**（execution 基準に固定する）。
const SECRET_INJECTION_SQL: &str = "SELECT s.id AS secret_id, s.name, \
        v.version, v.kek_kid, v.wrapped_dek, v.dek_nonce, v.nonce, v.ciphertext, v.value_len \
   FROM function_secrets s \
   JOIN LATERAL ( \
        SELECT * FROM function_secret_versions v2 \
         WHERE v2.tenant_id = s.tenant_id AND v2.secret_id = s.id \
           AND v2.created_at <= $3 \
         ORDER BY v2.version DESC LIMIT 1 \
   ) v ON TRUE \
  WHERE s.tenant_id = $1 AND s.component_id = $2 \
    AND s.deleted_at IS NULL AND s.name = ANY($4) \
  ORDER BY s.name";

/// 監査ログ `detail` の**唯一の構築点** (§5.3)。
///
/// `db::insert_audit_log` の `detail: Option<&Value>` は任意 JSON を受け取れてしまうため、
/// secret 系はこの関数だけを通す。**値を受け取らないシグネチャ**にすることで、生値が
/// `insert_audit_log` に到達する型経路そのものを消す。
pub fn audit_detail(name: &str, version: i32, reason: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "version": version, "reason": reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring() -> SecretKeyring {
        SecretKeyring::new("k1".into(), [7u8; 32], vec![])
    }

    fn enc(k: &SecretKeyring, v: &[u8]) -> Envelope {
        encrypt(k, "ten_a", "cmp_a", "sec_a", "API_KEY", 1, v).expect("encrypt")
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let k = keyring();
        let e = enc(&k, b"hunter2");
        let out = decrypt(&k, &e, "ten_a", "cmp_a", "sec_a", "API_KEY", 1).expect("decrypt");
        assert_eq!(&out[..], b"hunter2");
        assert_eq!(e.value_len, 7);
        // 暗号文に平文が現れない（当然だが、AEAD の配線ミスの一次検出になる）。
        assert!(!e.ciphertext.windows(7).any(|w| w == b"hunter2"));
    }

    /// **cut-and-paste 遮断の証明**: 同じ暗号文を別テナント / 別 component / 別キー名の AAD で
    /// 復号すると必ず失敗する（DB 書き込み権限を得た攻撃者による行の貼り替えを潰す）。
    #[test]
    fn aad_tampering_is_rejected() {
        let k = keyring();
        let e = enc(&k, b"hunter2");
        for (t, c, n) in [
            ("ten_OTHER", "cmp_a", "API_KEY"),
            ("ten_a", "cmp_OTHER", "API_KEY"),
            ("ten_a", "cmp_a", "OTHER_NAME"),
        ] {
            assert_eq!(
                decrypt(&k, &e, t, c, "sec_a", n, 1).unwrap_err(),
                SecretError::DecryptFailed,
                "pasting the ciphertext into ({t}, {c}, {n}) must fail"
            );
        }
    }

    /// version の巻き戻し（古い版の wrapped_dek を新しい版として読む）が遮断される。
    #[test]
    fn version_rollback_is_rejected() {
        let k = keyring();
        let e = enc(&k, b"v1-value");
        assert_eq!(
            decrypt(&k, &e, "ten_a", "cmp_a", "sec_a", "API_KEY", 2).unwrap_err(),
            SecretError::DecryptFailed
        );
        // secret_id の付け替えも同様に遮断される。
        assert_eq!(
            decrypt(&k, &e, "ten_a", "cmp_a", "sec_OTHER", "API_KEY", 1).unwrap_err(),
            SecretError::DecryptFailed
        );
    }

    /// 行が参照する kid がキーリングに無ければ `UnknownKid`（retired キーの早期撤去の典型）。
    #[test]
    fn unknown_kid_is_reported_as_such() {
        let k = keyring();
        let mut e = enc(&k, b"x");
        e.kek_kid = "k-gone".into();
        let err = decrypt(&k, &e, "ten_a", "cmp_a", "sec_a", "API_KEY", 1).unwrap_err();
        assert_eq!(err, SecretError::UnknownKid);
        assert_eq!(err.reason(), "unknown_kid");
    }

    /// nonce は呼び出しごとに異なる（OsRng）。同じ平文でも暗号文が一致しない。
    #[test]
    fn nonces_are_unique_per_call() {
        let k = keyring();
        let a = enc(&k, b"same");
        let b = enc(&k, b"same");
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.dek_nonce, b.dek_nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    /// 壊れた封筒（nonce 長違い）は `BadEnvelope`。
    #[test]
    fn malformed_envelope_is_reported() {
        let k = keyring();
        let mut e = enc(&k, b"x");
        e.nonce = vec![0u8; 12];
        assert_eq!(
            decrypt(&k, &e, "ten_a", "cmp_a", "sec_a", "API_KEY", 1).unwrap_err(),
            SecretError::BadEnvelope
        );
    }

    /// `Debug for SecretKeyring` は kid だけを出し、鍵素材を出さない。
    #[test]
    fn keyring_debug_never_reveals_key_material() {
        let k = SecretKeyring::new("k2".into(), [0xABu8; 32], vec![("k1".into(), [0xCDu8; 32])]);
        let out = format!("{k:?}");
        assert!(
            out.contains("k1") && out.contains("k2"),
            "kids must be visible: {out}"
        );
        assert!(
            !out.contains("171") && !out.contains("205"),
            "key bytes leaked: {out}"
        );
        assert!(
            !out.contains("ab") && !out.contains("cd"),
            "key bytes leaked: {out}"
        );
    }

    /// wrap-only rekey のラウンドトリップ: 新 kid で再ラップした DEK で ciphertext が復号できる。
    #[test]
    fn rewrap_roundtrips_without_touching_the_ciphertext() {
        let old = SecretKeyring::new("k1".into(), [1u8; 32], vec![]);
        let e1 = encrypt(&old, "ten_a", "cmp_a", "sec_a", "API_KEY", 1, b"rotate-me").unwrap();

        // 新 KEK を active にし、旧 KEK は復号専用として残す。
        let rotated = SecretKeyring::new("k2".into(), [2u8; 32], vec![("k1".into(), [1u8; 32])]);
        let e2 = rewrap(&rotated, &e1, "ten_a", "sec_a", 1, 2).expect("rewrap");

        assert_eq!(e2.kek_kid, "k2");
        // 値そのものは触っていない（平文をメモリに載せずに KEK だけ回した証拠）。
        assert_eq!(e2.ciphertext, e1.ciphertext);
        assert_eq!(e2.nonce, e1.nonce);
        assert_ne!(e2.wrapped_dek, e1.wrapped_dek);

        let out = decrypt(&rotated, &e2, "ten_a", "cmp_a", "sec_a", "API_KEY", 2).expect("decrypt");
        assert_eq!(&out[..], b"rotate-me");

        // 旧 KEK を撤去すると復号できなくなる（早期撤去 = データ喪失、の実証）。
        let only_new = SecretKeyring::new("k2".into(), [2u8; 32], vec![]);
        assert!(rewrap(&only_new, &e1, "ten_a", "sec_a", 1, 3).is_err());
    }

    /// AAD は正準化されており、フィールド境界が曖昧にならない（長さ前置の効果）。
    #[test]
    fn aad_is_unambiguous_across_field_boundaries() {
        // ("ab", "c") と ("a", "bc") が同じバイト列にならないこと。
        assert_ne!(value_aad("ab", "c", "X"), value_aad("a", "bc", "X"));
        assert_ne!(dek_aad("ab", "c", 1, "k"), dek_aad("a", "bc", 1, "k"));
        // version が違えば別の AAD。
        assert_ne!(dek_aad("t", "s", 1, "k"), dek_aad("t", "s", 2, "k"));
        // ドメインタグで用途が分離されている。
        assert_ne!(value_aad("t", "c", "n"), dek_aad("t", "c", 0, "n"));
    }

    /// 注入クエリは **execution 基準**（`current_version` を参照しない）。
    ///
    /// 参照してしまうと、worker 再配送中に rotate が走ったとき同一 execution の 1 回目と 2 回目で
    /// 別の資格情報が外部へ出る（at-least-once なので両方到達しうる）。
    #[test]
    fn secret_injection_sql_is_execution_pinned() {
        assert!(
            SECRET_INJECTION_SQL.contains("v2.created_at <= $3"),
            "the generation must be pinned to executions.created_at"
        );
        assert!(
            !SECRET_INJECTION_SQL.contains("current_version"),
            "resolving via current_version would break redelivery determinism"
        );
        assert!(
            SECRET_INJECTION_SQL.contains("JOIN LATERAL")
                && SECRET_INJECTION_SQL.contains("ON TRUE"),
            "an INNER-equivalent LATERAL makes unresolvable generations disappear (fail-closed)"
        );
        assert!(
            SECRET_INJECTION_SQL.contains("s.deleted_at IS NULL"),
            "soft-deleted secrets must never be injected"
        );
        assert!(
            SECRET_INJECTION_SQL.contains("s.tenant_id = $1"),
            "the injection query must be tenant-scoped in the predicate as well as under RLS"
        );
    }

    /// 監査 detail に値を載せる型経路が存在しない（シグネチャが値を受け取らない）。
    #[test]
    fn audit_detail_carries_no_value() {
        let d = audit_detail("API_KEY", 3, "rotate");
        assert_eq!(d["name"], "API_KEY");
        assert_eq!(d["version"], 3);
        assert_eq!(d["reason"], "rotate");
        assert_eq!(d.as_object().map(|m| m.len()), Some(3));
    }
}
