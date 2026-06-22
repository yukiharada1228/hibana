//! faas-shared — control-plane と worker が共有する唯一の契約 (M1)。
//!
//! ここが「共有契約の唯一の真実」であり、両 bin はこの型・subject・メッセージ・
//! エラー・ResourceLimits を読む。仕様書 §15 M1 に準拠。
//!
//! 原則(§15): M1 は単一テナント "default" でハードコード可だが、将来の M3
//! (テナント分離・RLS) に備え、関数引数には常に `tenant_id` を通す。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ============================================================================
// 既定テナント (M1)
// ============================================================================

/// M1 で固定使用する単一テナント ID。
/// TODO(§3.2 / §6.0): M3 で認証コンテキストから解決する。
pub const DEFAULT_TENANT: &str = "default";

// ============================================================================
// ID 採番ヘルパ（uuid 由来の不透明文字列）
// ============================================================================

/// `{prefix}_{uuid_simple}` 形式の不透明 ID を生成する。
fn new_prefixed_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

/// `cmp_*` Component ID を生成する。
pub fn new_component_id() -> String {
    new_prefixed_id("cmp")
}

/// `ver_*` Component Version ID を生成する。
pub fn new_version_id() -> String {
    new_prefixed_id("ver")
}

/// `exec_*` Execution ID を生成する。
pub fn new_execution_id() -> String {
    new_prefixed_id("exec")
}

/// `ten_*` Tenant ID を生成する (§3.2)。
///
/// uuid-simple 由来なので subject-safe（`.`/`*`/`>`/空白を含まない）であり、
/// NATS subject やキー空間にそのまま埋め込める。
pub fn new_tenant_id() -> String {
    new_prefixed_id("ten")
}

/// `usr_*` User ID を生成する (§3.2)。
pub fn new_user_id() -> String {
    new_prefixed_id("usr")
}

/// `tok_*` API Token ID を生成する (§3.3)。
pub fn new_token_id() -> String {
    new_prefixed_id("tok")
}

// ============================================================================
// 実行状態
// ============================================================================

/// 実行状態 (§3.2 / §6)。snake_case で (de)serialize され、DB の TEXT 値と一致する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Timeout,
}

impl ExecutionStatus {
    /// DB に格納する文字列表現（serde の snake_case と一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Succeeded => "succeeded",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Timeout => "timeout",
        }
    }

    /// 終端状態か（これ以上遷移しない）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ExecutionStatus::Succeeded | ExecutionStatus::Failed | ExecutionStatus::Timeout
        )
    }
}

impl std::fmt::Display for ExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// 認可: Scope / Role (§3.3 / §6.0)
// ============================================================================

/// API トークンに付与される権限スコープ (§3.3)。
///
/// snake_case で (de)serialize され、DB の TEXT[] 値と一致する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// 読み取り（一覧・取得）。
    Read,
    /// 関数の invoke。
    Invoke,
    /// component / version のデプロイ・削除。
    Deploy,
    /// テナント管理（トークン・ユーザ管理）。
    Admin,
}

impl Scope {
    /// DB / serde の文字列表現（snake_case と一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Invoke => "invoke",
            Scope::Deploy => "deploy",
            Scope::Admin => "admin",
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// テナント内ユーザのロール (§3.3)。
///
/// ロールは付与可能なスコープの上限（ceiling）を定める。トークン発行時、
/// 要求スコープは発行者のスコープ以下かつ対象ユーザのロール上限以下でなければならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// 一般メンバー（read / invoke / deploy）。
    Member,
    /// テナント管理者（member + admin）。
    Admin,
}

impl Role {
    /// DB / serde の文字列表現（snake_case と一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Member => "member",
            Role::Admin => "admin",
        }
    }

    /// このロールが付与しうるスコープ上限。
    ///
    /// member => read, invoke, deploy
    /// admin  => read, invoke, deploy, admin
    pub fn ceiling(&self) -> &'static [Scope] {
        match self {
            Role::Member => &[Scope::Read, Scope::Invoke, Scope::Deploy],
            Role::Admin => &[Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin],
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================================
// NATS subjects
// ============================================================================

/// invoke subject: `tenant.{tenant}.component.invoke`。
/// control-plane が publish し、worker が JetStream Pull Consumer で購読する。
pub fn invoke_subject(tenant: &str) -> String {
    format!("tenant.{tenant}.component.invoke")
}

/// result subject: `tenant.{tenant}.component.result`。
/// worker が publish し、control-plane が購読して executions を更新する。
pub fn result_subject(tenant: &str) -> String {
    format!("tenant.{tenant}.component.result")
}

/// invoke subject のテナントワイルドカード形 `tenant.*.component.invoke` (§3.3)。
///
/// worker の JetStream stream `subjects` / 共有 Pull Consumer の `filter_subject` に使う。
/// 単一の `*` は 1 トークン（テナント ID）のみに一致する（`>` のような多段ワイルドカードでは
/// ない）。テナント ID は subject-safe（`.`/`*`/`>`/空白を含まない）に採番されるため、
/// 1 トークンに必ず収まる。これにより新規テナント追加時も stream/consumer の再構成が不要。
pub fn invoke_subject_wildcard() -> &'static str {
    "tenant.*.component.invoke"
}

/// result subject のテナントワイルドカード形 `tenant.*.component.result` (§3.3)。
///
/// control-plane の subscriber が購読する。テナントは subject の第 2 トークンから導出し
/// （`subject_tenant`）、メッセージ本文の `tenant_id` を盲信しない（spoof 防止）。
pub fn result_subject_wildcard() -> &'static str {
    "tenant.*.component.result"
}

/// failed (DLQ) subject: `tenant.{tenant}.component.failed` (§3.3 / §6.6, M4c)。
///
/// worker が `max_deliver` 直前で結果を出せないと判断したとき（最終配送に失敗するケース）に
/// publish する DLQ サブジェクト。control-plane の DLQ subscriber が購読し、対象 execution を
/// CAS で `failed` に終端化して in-flight カウンタを DECR する。これは「result も failed も来ない
/// 無音失踪」を許さず、終端化の責務を必ず CP に集中させるためのチャネル分離である（§6.6 MUST）。
pub fn failed_subject(tenant: &str) -> String {
    format!("tenant.{tenant}.component.failed")
}

/// failed (DLQ) subject のテナントワイルドカード形 `tenant.*.component.failed` (§3.3 / §6.6)。
///
/// control-plane の DLQ subscriber が購読する。テナントは subject の第 2 トークンから導出し
/// メッセージ本文の `tenant_id` を盲信しない（result と同じ spoof 防止規約）。新規テナント追加時も
/// stream/consumer の再構成は不要（`*` は 1 トークン=テナント ID に一致）。
pub fn failed_subject_wildcard() -> &'static str {
    "tenant.*.component.failed"
}

/// `tenant.{tenant}.component.{kind}` 形の subject から第 2 トークン（テナント ID）を取り出す。
///
/// subscriber が `tenant.*.component.result` のワイルドカード購読で受けた実際の subject から
/// テナントを導出するために使う。形が一致しなければ `None`（防御的に drop する）。
pub fn tenant_from_subject(subject: &str) -> Option<&str> {
    let mut parts = subject.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("tenant"), Some(tenant), Some("component"), Some(_kind))
            if !tenant.is_empty() && parts.next().is_none() =>
        {
            Some(tenant)
        }
        _ => None,
    }
}

// ============================================================================
// Object Storage キーレイアウト (§3.4)
// ============================================================================

/// Object Storage 上の Component 本体オブジェクトキー (§3.4)。
///
/// テナントプレフィックスレイアウト
/// `tenants/{tenant_id}/components/{component_name}/{version}/component.wasm`
/// を採用し、テナント境界をキー空間で明示する。
/// M2 は単一テナント "default" 固定だが、将来の M3 分離に備えてレイアウトを固定する。
pub fn component_object_key(tenant_id: &str, component_name: &str, version: &str) -> String {
    format!("tenants/{tenant_id}/components/{component_name}/{version}/component.wasm")
}

/// 大入力の退避先オブジェクトキー (§3.4 / §5.2 / §6.4)。
///
/// `tenants/{tenant_id}/io/{execution_id}/input` に固定する。`POST /uploads` はこのキーへの
/// 短命 presigned PUT を発行し、`POST /invoke` の `input_ref` はこのキーに **完全一致** する
/// 場合のみ受理される（§3.4 MUST: prefix 一致では不十分。同一テナント内の別 execution /
/// 別ユーザの入力を指す `input_ref` を拒否する）。テナントと execution の両方をキー空間で
/// 明示し、execution_id 単位で隔離する。
pub fn io_input_key(tenant_id: &str, execution_id: &str) -> String {
    format!("tenants/{tenant_id}/io/{execution_id}/input")
}

/// 大出力の退避先オブジェクトキー (§3.4 / §5.2 / §6.4)。
///
/// `tenants/{tenant_id}/io/{execution_id}/output` に固定する。worker が大出力を退避する際に
/// 使う（output_ref）。本スライスでは worker 側の write は最小実装（TODO）だが、キーレイアウトは
/// input と対称に固定しておく。
pub fn io_output_key(tenant_id: &str, execution_id: &str) -> String {
    format!("tenants/{tenant_id}/io/{execution_id}/output")
}

// ============================================================================
// NATS メッセージ
// ============================================================================

/// invoke subject に流れるジョブ (§6.3)。
///
/// M2: worker は `wasm_url`（短命 presigned GET URL）から本体を取得し、
/// `wasm_sha256` をキャッシュ／事前コンパイルのキーにする (§3.6)。
/// `component` / `version` はログ・解決・観測用に残す。
/// M3c: `job_token` は control-plane が署名した不透明トークン (§3.3)。
/// worker は中身を解釈せず result にそのまま echo する（鍵を持たない）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMessage {
    /// `exec_*`。冪等の最終キー。
    pub execution_id: String,
    /// M1 は "default"。
    pub tenant_id: String,
    /// Component 名。ログ・解決・観測用。
    pub component: String,
    /// semver。ログ・解決・観測用。
    pub version: String,
    /// 本体の sha256（16進）。worker のキャッシュキー（§3.6）。
    pub wasm_sha256: String,
    /// worker が本体を取得する短命 presigned GET URL（read-only, 既定 TTL 300秒）。
    pub wasm_url: String,
    /// インライン入力。worker は `serde_json::to_vec(input)` をハンドラへ渡す。
    /// 大入力時（`input_url` が在るとき）は `null` を入れ、worker は `input_url` を優先する。
    pub input: Value,
    /// M3d (§3.4 / §5.2): 大入力がある場合に、退避済み入力オブジェクト
    /// （`tenants/{tenant}/io/{execution_id}/input`）への **そのキー限定** の短命 read-only
    /// presigned GET URL。CP は invoke 受付時に `input_ref` を当該キーへ完全一致検証してから
    /// このフィールドへ presign を載せる。worker はこの URL から入力を取得し、**他のキーは
    /// 読み取らない** (§3.4 MUST NOT)。インライン invoke では `None`（その場合 `input` を使う）。
    /// 旧 CP のメッセージとの互換のため serde default（None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_url: Option<String>,
    /// M3c: control-plane が署名したジョブトークン (§3.3)。worker は不透明文字列として
    /// 扱い、result にそのまま echo する。旧 CP のメッセージとの互換のため serde default。
    #[serde(default)]
    pub job_token: String,
}

/// result subject に流れる実行結果 (§6.5)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    /// `exec_*`。
    pub execution_id: String,
    /// M1 は "default"。
    pub tenant_id: String,
    /// 終端状態 (succeeded | failed | timeout)。
    pub status: ExecutionStatus,
    /// 成功時の出力（インライン）。
    pub output: Option<Value>,
    /// 失敗時のエラーメッセージ。
    pub error: Option<String>,
    /// M3c: worker が JobMessage から verbatim に echo した署名トークン (§3.3)。
    /// subscriber が kid で検証し、claim を execution 行と突き合わせて出所を認証する。
    /// 旧 worker のメッセージとの互換のため serde default（空 -> drop+audit）。
    #[serde(default)]
    pub job_token: String,
}

/// `tenant.*.component.failed` (DLQ) に流れる失敗通知 (M4c, §3.3 / §6.6)。
///
/// worker が JetStream の最終配送試行で結果を publish できないと判断したとき
/// （= 一過性ではない失敗 / `delivered >= max_deliver` の最終再配送が失敗した場合）に publish する。
/// control-plane の DLQ subscriber がこれを受けて execution を `failed` に CAS 終端化し、
/// in-flight カウンタを DECR する（result も failed も来ない無音失踪を防ぐ唯一の自動経路）。
///
/// `ResultMessage` と分けるのは、(a) wire 形でも DLQ 由来であることが明示され audit/log で
/// 取り違えないため、(b) `output` のような成功側フィールドを持たない最小スキーマで「失敗のみ」を
/// 表現するため、である。`status` は明示せず、subscriber 側で常に `ExecutionStatus::Failed` に
/// 倒すことで「DLQ 経路から succeeded に終端化される」事故をスキーマレベルで不可能にする（§6.6）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedMessage {
    /// `exec_*`。
    pub execution_id: String,
    /// 権威テナント（CP-signed claim と一致するはず。subscriber は本文を盲信せず subject 由来と照合する）。
    pub tenant_id: String,
    /// DLQ 化の理由（人間可読; subscriber は executions.error に `{"message": reason}` で保存）。
    pub reason: String,
    /// M3c: worker が JobMessage から verbatim に echo した署名トークン (§3.3)。
    /// DLQ 経路でも結果認証は同一ルール: kid で署名検証し、claim を execution 行と突き合わせる。
    /// 旧 worker のメッセージとの互換のため serde default（空 -> drop+audit）。
    #[serde(default)]
    pub job_token: String,
}

// ============================================================================
// ジョブ署名トークン TTL 定数 (§3.3)
// ============================================================================
//
// TTL 結合の唯一の真実: control-plane のトークン mint と worker の JetStream
// consumer 設定は、ここの定数から同一に導出しなければならない (§3.3
// 「有効期限 ≥ ack_wait × max_deliver + 実行上限 + 余裕」)。両 bin が env で
// 上書きする場合も既定はここを共有することでドリフトを抑える。

/// JetStream consumer の ack 待ち秒数（既定）。
pub const ACK_WAIT_SECS: u64 = 30;

/// JetStream consumer の最大再配送回数（既定）。
pub const MAX_DELIVER: u64 = 5;

/// トークン有効期限に足す余裕秒数（既定）。
pub const TOKEN_MARGIN_SECS: u64 = 60;

/// トークン有効期限のオフセット秒数を計算する (§3.3)。
///
/// `exp = iat + token_exp_offset_secs(...)` で用いる。最悪ケースの再配送
/// (`ack_wait * max_deliver`) に実行壁時計上限と余裕を足したもの。これにより
/// 通常の再配送がトークン失効より先に届く（= 正規の遅延結果を取りこぼさない）。
///
/// 既定値では `30*5 + wall + 60 = 210 + wall`(秒)。
pub fn token_exp_offset_secs(
    wall_time_secs: u64,
    ack_wait: u64,
    max_deliver: u64,
    margin: u64,
) -> i64 {
    let total = (ack_wait.saturating_mul(max_deliver))
        .saturating_add(wall_time_secs)
        .saturating_add(margin);
    // u64 -> i64 を飽和的に行う（`as i64` は wrap して負になりうる）。
    total.min(i64::MAX as u64) as i64
}

// ============================================================================
// ジョブ署名トークン (§3.3)
// ============================================================================
//
// 目的: 結果の出所認証 (§3.3「結果の出所認証」)。control-plane がジョブごとの
// claim を Ed25519 秘密鍵で署名し、worker は不透明トークンを verbatim に echo、
// subscriber(CP 内) が kid で公開鍵を選んで検証する。
//
// crypto 依存はここには入れない。shared はバイト列の組み立てと base64url の
// みを行う。ed25519-dalek は control-plane だけが持つ。worker は job_token を
// 解析しない（opaque String）。

/// ジョブトークンに載せる claim (§3.3)。
///
/// `Serialize`/`Deserialize` は wire 上の搬送（JSON セグメント）のためだけに
/// あり、署名対象バイトには **使わない**。署名バイトは必ず
/// [`job_claims_signing_bytes`] が組み立てる正準形を用いる（serde_json は
/// map/key 順序や空白が非正準で、signer/verifier 間でドリフトすると検証が
/// 黙って壊れる／偽造の余地が生まれる）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobClaims {
    /// `exec_*`。
    pub execution_id: String,
    /// `ten_*`（権威的テナント）。
    pub tenant_id: String,
    /// `ver_*`。
    pub version_id: String,
    /// 検証側の公開鍵を選ぶ key id。
    pub kid: String,
    /// 発行時刻（unix 秒）。
    pub iat: i64,
    /// 有効期限（unix 秒）。TTL 定数から計算する。
    pub exp: i64,
}

/// 署名バイトのドメインタグ（バージョン付き）。レイアウトを将来変更する際に
/// 曖昧さなく進化させるための分離タグ。
const JOB_TOKEN_DOMAIN: &[u8] = b"faas-job-token-v1";

/// claim から **正準** 署名バイト列を組み立てる（§3.3、最高リスク部分）。
///
/// 性質:
/// - 固定フィールド順・長さ前置・ドメイン分離で、serde_json を **使わない**。
/// - 可変長フィールドはすべて 4 バイト BE の長さ前置 + UTF-8 バイトで、
///   連結が単射（injective）になる。これにより異なる claim タプルが同一バイト
///   列を生むこと（フィールド境界の取り違え攻撃、例: tenant "a"+exec "bc" と
///   tenant "ab"+exec "c"）を防ぐ。
/// - 整数 (iat, exp) は i64 BE 固定 8 バイト（長さ前置なし、幅が一意）。
///
/// signer と verifier は **必ず同じこの関数** を呼ぶ。どちらか一方でも再実装
/// したり serde_json を使うと検証が黙って失敗するか、最悪偽造可能になる。
pub fn job_claims_signing_bytes(c: &JobClaims) -> Vec<u8> {
    // 事前に概算容量を確保（厳密でなくてよい）。
    let mut out = Vec::with_capacity(
        4 + JOB_TOKEN_DOMAIN.len()
            + 4 * 4
            + c.execution_id.len()
            + c.tenant_id.len()
            + c.version_id.len()
            + c.kid.len()
            + 16,
    );

    // ドメインタグ（文字列フィールドと同じ規則: 長さ前置）。
    write_len_prefixed(&mut out, JOB_TOKEN_DOMAIN);
    // 6 フィールドを固定順で。
    write_len_prefixed(&mut out, c.execution_id.as_bytes());
    write_len_prefixed(&mut out, c.tenant_id.as_bytes());
    write_len_prefixed(&mut out, c.version_id.as_bytes());
    write_len_prefixed(&mut out, c.kid.as_bytes());
    out.extend_from_slice(&c.iat.to_be_bytes());
    out.extend_from_slice(&c.exp.to_be_bytes());
    out
}

/// 4 バイト BE 長さ前置 + バイト列を書き込む。
fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    // id 類は短いので u32 で十分。サーバ採番のため上限超過は想定しない。
    let len = bytes.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

// ----------------------------------------------------------------------------
// base64url (RFC 4648 §5) パディングなし。pure-Rust 手実装（外部依存なし）。
// ----------------------------------------------------------------------------

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// 任意バイト列を base64url（パディングなし）に符号化する。
pub fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(B64URL_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        _ => {}
    }
    out
}

/// base64url（パディングなし）を復号する。
///
/// パディング文字 `=` は受け付けない（no-pad 前提）。不正な文字・不正な長さは
/// `None` を返す（検証側が drop+audit できるよう全失敗を一様に表す）。
pub fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    // base64 の最終グループは 1 文字だけ（= 端数 6bit のみ）にはならない。
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 + 2);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for &b in bytes {
        let v = b64url_decode_char(b)?;
        acc = (acc << 6) | u32::from(v);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    // 末尾に残るビット（< 8）はすべて 0 でなければならない（正準性）。
    if nbits > 0 && (acc & ((1 << nbits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

/// base64url 1 文字 -> 6bit 値。無効文字は `None`。
fn b64url_decode_char(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

// ============================================================================
// リソース制限 (§4.3)
// ============================================================================

const DEFAULT_MAX_MEMORY_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB
const DEFAULT_MAX_WALL_TIME_MS: u64 = 1000; // 1s
/// M4b (§4.3): tokio タイムアウト（ホスト関数込み総時間）の既定。
const DEFAULT_MAX_EXECUTION_TIME_MS: u64 = 5000; // 5s

/// §4.3 表の上限値。upload_version で検証してこの値を超える制限値は 422 で拒否する。
/// max_memory: 1 GiB。
pub const MAX_MEMORY_BYTES_LIMIT: u64 = 1024 * 1024 * 1024;
/// max_wall_time: 30 000 ms。
pub const MAX_WALL_TIME_MS_LIMIT: u64 = 30_000;
/// max_execution_time: 60 000 ms。
pub const MAX_EXECUTION_TIME_MS_LIMIT: u64 = 60_000;

/// Component version ごとのリソース制限 (§4.3)。
/// component_versions.resource_limits (JSONB) に格納される。
///
/// M1 は worker で `max_memory_bytes` を `StoreLimits` に、
/// `max_wall_time_ms` を epoch interruption に適用する。
///
/// M4b (§4.3) で以下を追加:
/// - `max_execution_time_ms`: tokio タイムアウト（ホスト関数込み総経過時間。epoch は
///   ゲスト内ループは中断できるがホスト関数中のブロッキングは止められないため両者を併用）。
///   既定 5000ms、上限 60000ms。
/// - `max_fuel`: 任意・決定性用の fuel 単位。`None` のとき epoch のみを適用（既定挙動）。
///   `Some(n)` のとき worker は `Store::set_fuel(n)` を呼び、`OutOfFuel` trap は `failed` 分類。
///
/// 後方互換: 既存行（`{"max_memory_bytes":..., "max_wall_time_ms":...}`）も `#[serde(default)]`
/// で問題なく round-trip する（欠損フィールドは既定値で補完される）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// 最大メモリ (bytes)。既定 128 MiB。
    #[serde(default = "default_max_memory_bytes")]
    pub max_memory_bytes: u64,
    /// ゲスト実行壁時計上限 (ms)。epoch interruption の停止しきい値。既定 1000。
    #[serde(default = "default_max_wall_time_ms")]
    pub max_wall_time_ms: u64,
    /// M4b (§4.3): ホスト関数込み総実行時間上限 (ms)。tokio::time::timeout で適用する。
    /// 既定 5000、上限 60000。
    #[serde(default = "default_max_execution_time_ms")]
    pub max_execution_time_ms: u64,
    /// M4b (§4.3): 任意の fuel 上限。`None`（または欠損）で fuel は無効＝ epoch のみ。
    /// `Some(n)` で `Store::set_fuel(n)` を呼び、超過は `failed` として分類する。
    /// fuel と epoch は代替関係（§4.3）にあり、本既定では fuel は無効。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fuel: Option<u64>,
}

fn default_max_memory_bytes() -> u64 {
    DEFAULT_MAX_MEMORY_BYTES
}

fn default_max_wall_time_ms() -> u64 {
    DEFAULT_MAX_WALL_TIME_MS
}

fn default_max_execution_time_ms() -> u64 {
    DEFAULT_MAX_EXECUTION_TIME_MS
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_wall_time_ms: DEFAULT_MAX_WALL_TIME_MS,
            max_execution_time_ms: DEFAULT_MAX_EXECUTION_TIME_MS,
            max_fuel: None,
        }
    }
}

impl ResourceLimits {
    /// `max_wall_time_ms` を `Duration` として返す（epoch 用）。
    pub fn max_wall_time(&self) -> Duration {
        Duration::from_millis(self.max_wall_time_ms)
    }

    /// M4b (§4.3): `max_execution_time_ms` を `Duration` として返す（tokio タイムアウト用）。
    pub fn max_execution_time(&self) -> Duration {
        Duration::from_millis(self.max_execution_time_ms)
    }

    /// §4.3 表の上限値を超えていないか検証する（upload_version の入口で呼ぶ）。
    ///
    /// 違反は [`FaasError::InvalidRequest`]（→ 422 相当）。上限超過は拒否し、ゼロ
    /// （`max_memory_bytes==0` / `max_wall_time_ms==0` / `max_execution_time_ms==0`）も
    /// 実行不能な構成として拒否する（既定 0 はサーバが許可しない）。max_fuel は `None` を
    /// 既定とし、`Some(0)` は「即 fuel 切れ」になるため拒否する（仕様の上限は無いので
    /// 上限チェックは行わない）。
    pub fn validate(&self) -> Result<()> {
        if self.max_memory_bytes == 0 {
            return Err(FaasError::InvalidRequest(
                "max_memory_bytes must be > 0".into(),
            ));
        }
        if self.max_memory_bytes > MAX_MEMORY_BYTES_LIMIT {
            return Err(FaasError::InvalidRequest(format!(
                "max_memory_bytes ({}) exceeds upper bound {} (§4.3)",
                self.max_memory_bytes, MAX_MEMORY_BYTES_LIMIT
            )));
        }
        if self.max_wall_time_ms == 0 {
            return Err(FaasError::InvalidRequest(
                "max_wall_time_ms must be > 0".into(),
            ));
        }
        if self.max_wall_time_ms > MAX_WALL_TIME_MS_LIMIT {
            return Err(FaasError::InvalidRequest(format!(
                "max_wall_time_ms ({}) exceeds upper bound {} (§4.3)",
                self.max_wall_time_ms, MAX_WALL_TIME_MS_LIMIT
            )));
        }
        if self.max_execution_time_ms == 0 {
            return Err(FaasError::InvalidRequest(
                "max_execution_time_ms must be > 0".into(),
            ));
        }
        if self.max_execution_time_ms > MAX_EXECUTION_TIME_MS_LIMIT {
            return Err(FaasError::InvalidRequest(format!(
                "max_execution_time_ms ({}) exceeds upper bound {} (§4.3)",
                self.max_execution_time_ms, MAX_EXECUTION_TIME_MS_LIMIT
            )));
        }
        // §4.3: max_execution_time は max_wall_time 以上であるべき（ホスト + ゲスト ≥ ゲスト）。
        // 入れ替えミスを防ぐため整合性も検証する。
        if self.max_execution_time_ms < self.max_wall_time_ms {
            return Err(FaasError::InvalidRequest(format!(
                "max_execution_time_ms ({}) must be >= max_wall_time_ms ({}) (§4.3)",
                self.max_execution_time_ms, self.max_wall_time_ms
            )));
        }
        if let Some(fuel) = self.max_fuel {
            if fuel == 0 {
                return Err(FaasError::InvalidRequest(
                    "max_fuel must be > 0 when set (omit the field to disable fuel)".into(),
                ));
            }
        }
        Ok(())
    }
}

// ============================================================================
// エラー
// ============================================================================

/// 共有エラー型。両 bin がこれを横断的に使用する。
/// TODO(§6.5): M3/M4 でエラー分類を拡張する（リトライ可否など）。
#[derive(Debug, thiserror::Error)]
pub enum FaasError {
    /// メッセージの (de)serialize 失敗。
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// 認証失敗（トークン欠損・不一致・失効・期限切れ）。
    #[error("unauthorized")]
    Unauthorized,

    /// 認可失敗（認証は成立したがスコープ/ロールが不足。§3.3）。
    #[error("forbidden")]
    Forbidden,

    /// リソースが見つからない（component / execution など）。
    #[error("not found: {0}")]
    NotFound(String),

    /// 不正なリクエスト。
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// 状態の競合（参照中の実行があり削除不可・active version は削除不可など, §6.7）。
    #[error("conflict: {0}")]
    Conflict(String),

    /// ハンドラ実行失敗。
    #[error("execution failed: {0}")]
    Execution(String),

    /// 実行が wall-time を超過した。
    #[error("execution timed out")]
    Timeout,

    /// その他内部エラー。
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, FaasError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_format() {
        assert_eq!(invoke_subject("default"), "tenant.default.component.invoke");
        assert_eq!(result_subject("default"), "tenant.default.component.result");
    }

    #[test]
    fn wildcard_subjects_format() {
        assert_eq!(invoke_subject_wildcard(), "tenant.*.component.invoke");
        assert_eq!(result_subject_wildcard(), "tenant.*.component.result");
        assert_eq!(failed_subject_wildcard(), "tenant.*.component.failed");
        // 具体 subject はワイルドカードの 1 トークンに収まる（テナント ID は subject-safe）。
        assert_eq!(
            invoke_subject("ten_abc").split('.').count(),
            invoke_subject_wildcard().split('.').count()
        );
        assert_eq!(
            failed_subject("ten_abc").split('.').count(),
            failed_subject_wildcard().split('.').count()
        );
    }

    /// M4c: failed (DLQ) subject の形と、`tenant_from_subject` が `.failed` でも第 2 トークンを
    /// 抽出できることを担保する（subscriber は subject 由来テナントを唯一の権威にする）。
    #[test]
    fn failed_subject_format_and_extraction() {
        assert_eq!(failed_subject("ten_abc"), "tenant.ten_abc.component.failed");
        assert_eq!(
            tenant_from_subject("tenant.ten_abc.component.failed"),
            Some("ten_abc")
        );
        // result と failed のワイルドカードは subject 末尾だけが違う。
        assert_ne!(failed_subject_wildcard(), result_subject_wildcard());
    }

    /// M4c: `FailedMessage` の round-trip と、job_token 欠落時の fail-closed defaults を確認する。
    #[test]
    fn failed_message_roundtrip_and_default_token() {
        let msg = FailedMessage {
            execution_id: "exec_1".into(),
            tenant_id: "ten_a".into(),
            reason: "max_deliver exhausted".into(),
            job_token: "hdr.sig".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: FailedMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.execution_id, "exec_1");
        assert_eq!(back.tenant_id, "ten_a");
        assert_eq!(back.reason, "max_deliver exhausted");
        assert_eq!(back.job_token, "hdr.sig");

        // 旧 worker（job_token なし）も復号でき、token は空文字（subscriber が drop+audit する）。
        let legacy = r#"{"execution_id":"exec_1","tenant_id":"ten_a","reason":"crashed"}"#;
        let parsed: FailedMessage = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.job_token, "");
    }

    #[test]
    fn tenant_from_subject_extracts_second_token() {
        assert_eq!(
            tenant_from_subject("tenant.ten_abc.component.result"),
            Some("ten_abc")
        );
        assert_eq!(
            tenant_from_subject(&result_subject("default")),
            Some("default")
        );
        // 形が一致しないものは None（防御的に drop）。
        assert_eq!(tenant_from_subject("tenant.ten_abc.component"), None);
        assert_eq!(
            tenant_from_subject("tenant.ten_abc.component.result.extra"),
            None
        );
        assert_eq!(tenant_from_subject("other.ten_abc.component.result"), None);
        assert_eq!(tenant_from_subject("tenant..component.result"), None);
        // ワイルドカード文字そのものはテナントとして導出されない（具体 subject のみ想定）。
        assert_eq!(
            tenant_from_subject(result_subject_wildcard()),
            Some("*"),
            "literal wildcard parses structurally; real subjects from NATS are concrete"
        );
    }

    #[test]
    fn status_snake_case_roundtrip() {
        let json = serde_json::to_string(&ExecutionStatus::Succeeded).unwrap();
        assert_eq!(json, "\"succeeded\"");
        let s: ExecutionStatus = serde_json::from_str("\"timeout\"").unwrap();
        assert_eq!(s, ExecutionStatus::Timeout);
        assert_eq!(ExecutionStatus::Running.as_str(), "running");
    }

    #[test]
    fn resource_limits_defaults() {
        let d = ResourceLimits::default();
        assert_eq!(d.max_memory_bytes, 128 * 1024 * 1024);
        assert_eq!(d.max_wall_time_ms, 1000);
        // M4b: execution_time の既定 5s、fuel は無効（None）。
        assert_eq!(d.max_execution_time_ms, 5000);
        assert_eq!(d.max_fuel, None);
        // 欠損フィールドは serde default で補完される。
        let parsed: ResourceLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, d);
    }

    /// M4b 後方互換: 旧 JSON（max_memory_bytes + max_wall_time_ms のみ）も round-trip し、
    /// 追加フィールドは既定で補完される。これは DB に既存行（M3 以前のアップロード）が
    /// 残っているケースでも resolve_limits が落ちないことを担保する（§4.3）。
    #[test]
    fn resource_limits_legacy_payload_roundtrips() {
        let legacy = r#"{"max_memory_bytes": 1048576, "max_wall_time_ms": 500}"#;
        let parsed: ResourceLimits = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.max_memory_bytes, 1_048_576);
        assert_eq!(parsed.max_wall_time_ms, 500);
        assert_eq!(parsed.max_execution_time_ms, 5000);
        assert_eq!(parsed.max_fuel, None);
    }

    /// M4b (§4.3): validate() は表の上限を超える値を 422 相当（InvalidRequest）で弾く。
    #[test]
    fn resource_limits_validate_upper_bounds() {
        // 既定は妥当。
        assert!(ResourceLimits::default().validate().is_ok());

        // max_memory 上限超過。
        let l = ResourceLimits {
            max_memory_bytes: MAX_MEMORY_BYTES_LIMIT + 1,
            ..Default::default()
        };
        assert!(matches!(l.validate(), Err(FaasError::InvalidRequest(_))));

        // max_wall_time 上限超過。
        let l = ResourceLimits {
            max_wall_time_ms: MAX_WALL_TIME_MS_LIMIT + 1,
            // execution_time も合わせて上限内に揃える（>= wall_time の整合性検証を回避するため）。
            max_execution_time_ms: MAX_EXECUTION_TIME_MS_LIMIT,
            ..Default::default()
        };
        assert!(matches!(l.validate(), Err(FaasError::InvalidRequest(_))));

        // max_execution_time 上限超過。
        let l = ResourceLimits {
            max_execution_time_ms: MAX_EXECUTION_TIME_MS_LIMIT + 1,
            ..Default::default()
        };
        assert!(matches!(l.validate(), Err(FaasError::InvalidRequest(_))));

        // execution_time < wall_time は逆転として拒否（入れ替えミス防止）。
        let l = ResourceLimits {
            max_wall_time_ms: 5000,
            max_execution_time_ms: 1000,
            ..Default::default()
        };
        assert!(matches!(l.validate(), Err(FaasError::InvalidRequest(_))));

        // max_fuel = Some(0) は拒否（即 fuel 切れになる構成は無効）。
        let l = ResourceLimits {
            max_fuel: Some(0),
            ..Default::default()
        };
        assert!(matches!(l.validate(), Err(FaasError::InvalidRequest(_))));

        // ゼロ値は拒否。
        let l = ResourceLimits {
            max_memory_bytes: 0,
            ..Default::default()
        };
        assert!(matches!(l.validate(), Err(FaasError::InvalidRequest(_))));
    }

    /// M4b: 上限ぴったり（境界値）は許容される。
    #[test]
    fn resource_limits_validate_accepts_upper_bound_inclusive() {
        let l = ResourceLimits {
            max_memory_bytes: MAX_MEMORY_BYTES_LIMIT,
            max_wall_time_ms: MAX_WALL_TIME_MS_LIMIT,
            max_execution_time_ms: MAX_EXECUTION_TIME_MS_LIMIT,
            max_fuel: Some(1_000_000),
        };
        assert!(l.validate().is_ok());
    }

    #[test]
    fn object_key_layout() {
        assert_eq!(
            component_object_key("default", "echo", "1.0.0"),
            "tenants/default/components/echo/1.0.0/component.wasm"
        );
    }

    /// 大入力 I/O キーは `tenants/{tenant}/io/{exec}/input` に固定される (§3.4)。
    /// invoke の input_ref 完全一致検証の権威レイアウト。
    #[test]
    fn io_key_layout() {
        assert_eq!(
            io_input_key("ten_abc", "exec_123"),
            "tenants/ten_abc/io/exec_123/input"
        );
        assert_eq!(
            io_output_key("ten_abc", "exec_123"),
            "tenants/ten_abc/io/exec_123/output"
        );
        // テナント / execution の取り違えは別キーになる（境界の混同で衝突しない）。
        assert_ne!(
            io_input_key("ten_a", "exec_bc"),
            io_input_key("ten_ab", "exec_c")
        );
    }

    #[test]
    fn job_message_roundtrip_includes_m2_fields() {
        let job = JobMessage {
            execution_id: "exec_1".into(),
            tenant_id: "default".into(),
            component: "echo".into(),
            version: "1.0.0".into(),
            wasm_sha256: "abc123".into(),
            wasm_url: "http://localhost:9000/faas-components/x?sig".into(),
            input: serde_json::json!({"k": "v"}),
            input_url: None,
            job_token: "payload.sig".into(),
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: JobMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wasm_sha256, "abc123");
        assert_eq!(back.wasm_url, "http://localhost:9000/faas-components/x?sig");
        assert_eq!(back.job_token, "payload.sig");
    }

    /// 旧 CP/worker が出した job_token なしのメッセージも serde(default) で復号でき、
    /// job_token は空文字になる（フェイルクローズ: subscriber が drop+audit する）。
    #[test]
    fn messages_decode_without_job_token_field() {
        let job_json = r#"{"execution_id":"exec_1","tenant_id":"default","component":"echo","version":"1.0.0","wasm_sha256":"abc","wasm_url":"http://x","input":{}}"#;
        let job: JobMessage = serde_json::from_str(job_json).unwrap();
        assert_eq!(job.job_token, "");

        let res_json = r#"{"execution_id":"exec_1","tenant_id":"default","status":"succeeded","output":null,"error":null}"#;
        let res: ResultMessage = serde_json::from_str(res_json).unwrap();
        assert_eq!(res.job_token, "");
    }

    #[test]
    fn result_message_roundtrip_includes_job_token() {
        let res = ResultMessage {
            execution_id: "exec_1".into(),
            tenant_id: "ten_a".into(),
            status: ExecutionStatus::Succeeded,
            output: Some(serde_json::json!({"ok": true})),
            error: None,
            job_token: "hdr.sig".into(),
        };
        let json = serde_json::to_string(&res).unwrap();
        let back: ResultMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.job_token, "hdr.sig");
        assert_eq!(back.status, ExecutionStatus::Succeeded);
    }

    fn sample_claims() -> JobClaims {
        JobClaims {
            execution_id: "exec_abc".into(),
            tenant_id: "ten_xyz".into(),
            version_id: "ver_123".into(),
            kid: "k1".into(),
            iat: 1_700_000_000,
            exp: 1_700_000_210,
        }
    }

    // ---- TTL 定数 / exp 算術 -------------------------------------------------

    #[test]
    fn token_exp_offset_uses_constants() {
        // 既定: 30*5 + wall + 60。wall=1 で 211。
        assert_eq!(
            token_exp_offset_secs(1, ACK_WAIT_SECS, MAX_DELIVER, TOKEN_MARGIN_SECS),
            211
        );
        // wall=0 で 210。
        assert_eq!(
            token_exp_offset_secs(0, ACK_WAIT_SECS, MAX_DELIVER, TOKEN_MARGIN_SECS),
            210
        );
        // env 上書き値でも算術が一貫している。
        assert_eq!(token_exp_offset_secs(5, 10, 3, 7), 10 * 3 + 5 + 7);
    }

    #[test]
    fn token_exp_offset_saturates_instead_of_overflow() {
        // u64 巨大値でもパニックせず i64 に飽和的に収める。
        let off = token_exp_offset_secs(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        assert!(off >= 0);
    }

    // ---- 正準署名バイト ------------------------------------------------------

    /// 決定性: 同じ claim は何度呼んでも同一バイト列。
    #[test]
    fn signing_bytes_are_deterministic() {
        let c = sample_claims();
        assert_eq!(job_claims_signing_bytes(&c), job_claims_signing_bytes(&c));
        // clone でも同一。
        let c2 = c.clone();
        assert_eq!(job_claims_signing_bytes(&c), job_claims_signing_bytes(&c2));
    }

    /// ドメインタグが先頭に長さ前置で入っている。
    #[test]
    fn signing_bytes_start_with_domain_tag() {
        let bytes = job_claims_signing_bytes(&sample_claims());
        let tag = JOB_TOKEN_DOMAIN;
        assert_eq!(&bytes[0..4], &(tag.len() as u32).to_be_bytes());
        assert_eq!(&bytes[4..4 + tag.len()], tag);
    }

    /// 各フィールドを変えるとバイト列が必ず変わる。
    #[test]
    fn changing_any_field_changes_bytes() {
        let base = job_claims_signing_bytes(&sample_claims());

        let mut c = sample_claims();
        c.execution_id = "exec_other".into();
        assert_ne!(job_claims_signing_bytes(&c), base);

        let mut c = sample_claims();
        c.tenant_id = "ten_other".into();
        assert_ne!(job_claims_signing_bytes(&c), base);

        let mut c = sample_claims();
        c.version_id = "ver_other".into();
        assert_ne!(job_claims_signing_bytes(&c), base);

        let mut c = sample_claims();
        c.kid = "k2".into();
        assert_ne!(job_claims_signing_bytes(&c), base);

        let mut c = sample_claims();
        c.iat += 1;
        assert_ne!(job_claims_signing_bytes(&c), base);

        let mut c = sample_claims();
        c.exp += 1;
        assert_ne!(job_claims_signing_bytes(&c), base);
    }

    /// 長さ前置により、隣接する文字列フィールドの境界取り違えで衝突しない。
    /// tenant="a", exec="bc" と tenant="ab", exec="c" は別バイト列でなければならない。
    /// (注: ここでは execution_id が先、tenant_id が次なので exec/tenant で検証)
    #[test]
    fn length_prefix_prevents_boundary_collision() {
        let mut a = sample_claims();
        a.execution_id = "a".into();
        a.tenant_id = "bc".into();

        let mut b = sample_claims();
        b.execution_id = "ab".into();
        b.tenant_id = "c".into();

        assert_ne!(
            job_claims_signing_bytes(&a),
            job_claims_signing_bytes(&b),
            "boundary-confusion: distinct field splits must not collide"
        );

        // 空文字を挟むエッジケースも単射。
        let mut x = sample_claims();
        x.execution_id = "".into();
        x.tenant_id = "abc".into();
        let mut y = sample_claims();
        y.execution_id = "abc".into();
        y.tenant_id = "".into();
        assert_ne!(job_claims_signing_bytes(&x), job_claims_signing_bytes(&y));
    }

    /// 整数フィールドが固定 8 バイト BE で末尾に並ぶ（iat の後に exp）。
    #[test]
    fn signing_bytes_end_with_fixed_width_ints() {
        let c = sample_claims();
        let bytes = job_claims_signing_bytes(&c);
        let n = bytes.len();
        assert_eq!(&bytes[n - 8..], &c.exp.to_be_bytes());
        assert_eq!(&bytes[n - 16..n - 8], &c.iat.to_be_bytes());
    }

    /// iat/exp の入れ替えは検出される（順序固定）。
    #[test]
    fn iat_exp_order_is_fixed() {
        let mut a = sample_claims();
        a.iat = 100;
        a.exp = 200;
        let mut b = sample_claims();
        b.iat = 200;
        b.exp = 100;
        assert_ne!(job_claims_signing_bytes(&a), job_claims_signing_bytes(&b));
    }

    // ---- base64url -----------------------------------------------------------

    #[test]
    fn b64url_roundtrip_all_lengths() {
        // 0..=300 バイト、内容も変化させて全長で往復一致。
        for len in 0usize..=300 {
            let data: Vec<u8> = (0..len).map(|i| (i as u32 * 31 + 7) as u8).collect();
            let enc = b64url_encode(&data);
            // パディング文字は出ない。
            assert!(!enc.contains('='), "no-pad expected, got {enc}");
            // url-safe 文字のみ。
            assert!(
                enc.bytes()
                    .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_')),
                "non-url-safe char in {enc}"
            );
            let dec = b64url_decode(&enc).expect("decode");
            assert_eq!(dec, data, "roundtrip mismatch at len {len}");
        }
    }

    #[test]
    fn b64url_known_vectors() {
        // 標準 base64url の既知ベクタ（no-pad）。
        assert_eq!(b64url_encode(b""), "");
        assert_eq!(b64url_encode(b"f"), "Zg");
        assert_eq!(b64url_encode(b"fo"), "Zm8");
        assert_eq!(b64url_encode(b"foo"), "Zm9v");
        assert_eq!(b64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(b64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url_encode(b"foobar"), "Zm9vYmFy");
        // url-safe: 0xff,0xff,0xfe -> 標準では //- だが url-safe では __-... 検証。
        assert_eq!(b64url_encode(&[0xff, 0xff, 0xff]), "____");
        assert_eq!(b64url_encode(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn b64url_decode_rejects_invalid() {
        // パディング文字は拒否。
        assert!(b64url_decode("Zg==").is_none());
        // 標準 base64 の '+' / '/' は url-safe では無効。
        assert!(b64url_decode("ab+d").is_none());
        assert!(b64url_decode("ab/d").is_none());
        // 不正長（mod 4 == 1）。
        assert!(b64url_decode("A").is_none());
        assert!(b64url_decode("ABCDE").is_none());
        // 末尾の非ゼロ余剰ビットは非正準として拒否。
        // "Zh" は 'Z'=25, 'h'=33 -> 0b011001 100001 -> 1バイト 0b01100110=0x66='f',
        // 残り 0b0001 != 0 なので拒否。
        assert!(b64url_decode("Zh").is_none());
        // 空白・非 ASCII。
        assert!(b64url_decode("Zg ").is_none());
        assert!(b64url_decode("Zg\n").is_none());
    }

    /// トークンの wire 形（payload.sig）を shared だけで組み立て・分解できる
    /// （署名自体は control-plane の責務だが、搬送コンテナの往復は shared で完結）。
    #[test]
    fn job_token_wire_container_roundtrip() {
        let claims = sample_claims();
        let payload = serde_json::to_vec(&claims).unwrap();
        let sig = [0x5au8; 64]; // ダミー署名（64 バイト）。
        let token = format!("{}.{}", b64url_encode(&payload), b64url_encode(&sig));

        // verifier 側の分解手順を模倣。
        let mut parts = token.split('.');
        let seg0 = parts.next().unwrap();
        let seg1 = parts.next().unwrap();
        assert!(parts.next().is_none());

        let payload_back = b64url_decode(seg0).unwrap();
        let claims_back: JobClaims = serde_json::from_slice(&payload_back).unwrap();
        assert_eq!(claims_back, claims);

        let sig_back = b64url_decode(seg1).unwrap();
        assert_eq!(sig_back.len(), 64);
        assert_eq!(sig_back, sig);

        // 検証側は搬送 JSON の順序を信用せず、必ず正準バイトを再導出する。
        assert_eq!(
            job_claims_signing_bytes(&claims_back),
            job_claims_signing_bytes(&claims)
        );
    }

    /// 搬送 JSON のキー順が違っても、再パース後の正準バイトは一致する
    /// （= verifier が JSON 順序に依存しない設計の保証）。
    #[test]
    fn canonical_bytes_independent_of_json_key_order() {
        let a = r#"{"execution_id":"exec_abc","tenant_id":"ten_xyz","version_id":"ver_123","kid":"k1","iat":1700000000,"exp":1700000210}"#;
        let b = r#"{"exp":1700000210,"iat":1700000000,"kid":"k1","version_id":"ver_123","tenant_id":"ten_xyz","execution_id":"exec_abc"}"#;
        let ca: JobClaims = serde_json::from_str(a).unwrap();
        let cb: JobClaims = serde_json::from_str(b).unwrap();
        assert_eq!(job_claims_signing_bytes(&ca), job_claims_signing_bytes(&cb));
    }

    #[test]
    fn id_prefixes() {
        assert!(new_component_id().starts_with("cmp_"));
        assert!(new_version_id().starts_with("ver_"));
        assert!(new_execution_id().starts_with("exec_"));
        assert!(new_tenant_id().starts_with("ten_"));
        assert!(new_user_id().starts_with("usr_"));
        assert!(new_token_id().starts_with("tok_"));
    }

    /// 採番した ID は subject / キー空間に安全に埋め込めること。
    /// `.` `*` `>`（NATS subject ワイルドカード/区切り）や空白を含まない。
    #[test]
    fn ids_are_subject_safe() {
        let ids = [
            new_tenant_id(),
            new_user_id(),
            new_token_id(),
            new_component_id(),
            new_version_id(),
            new_execution_id(),
        ];
        for id in ids {
            assert!(
                !id.chars()
                    .any(|c| matches!(c, '.' | '*' | '>') || c.is_whitespace()),
                "id contains an unsafe character: {id:?}"
            );
        }
    }

    #[test]
    fn scope_snake_case_and_as_str() {
        assert_eq!(serde_json::to_string(&Scope::Invoke).unwrap(), "\"invoke\"");
        let s: Scope = serde_json::from_str("\"deploy\"").unwrap();
        assert_eq!(s, Scope::Deploy);
        assert_eq!(Scope::Admin.as_str(), "admin");
        assert_eq!(Scope::Read.as_str(), "read");
    }

    #[test]
    fn role_ceiling() {
        assert_eq!(serde_json::to_string(&Role::Member).unwrap(), "\"member\"");
        assert_eq!(
            Role::Member.ceiling(),
            &[Scope::Read, Scope::Invoke, Scope::Deploy]
        );
        assert_eq!(
            Role::Admin.ceiling(),
            &[Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin]
        );
        // member の上限に admin は含まれない。
        assert!(!Role::Member.ceiling().contains(&Scope::Admin));
    }
}
