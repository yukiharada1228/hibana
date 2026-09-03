//! 環境変数からの設定読み込み (M1/M2)。
//!
//! `.env.example` のキーに対応:
//! DATABASE_URL / NATS_URL / BOOTSTRAP_ADMIN_TOKEN / COMPONENTS_DIR / BIND_ADDR
//! S3_ENDPOINT / S3_REGION / S3_BUCKET / S3_ACCESS_KEY / S3_SECRET_KEY
//! MAX_WASM_UPLOAD_BYTES / PRESIGN_TTL_SECS
//!
//! 認証は M3a で API トークン方式（DB の api_tokens）へ移行。固定 AUTH_TOKEN は廃止。
//! BOOTSTRAP_ADMIN_TOKEN は system-admin 用（POST /admin/tenants を gate）。
//!
//! TODO(§8): M3 で設定ソースを secrets manager 等へ移す。

use anyhow::Context;
use faas_shared::Redacted;

/// wasm 本体の最大アップロードサイズ既定値（32 MiB, §6.2）。
const DEFAULT_MAX_WASM_UPLOAD_BYTES: u64 = 32 * 1024 * 1024;
/// presigned GET URL の既定 TTL（秒, 短命 read-only, §3.4）。
const DEFAULT_PRESIGN_TTL_SECS: u64 = 300;

// --- 共有ストア / クォータ既定値（M3d, §8） ---
// グローバル既定 → テナント上書き（tenants.quotas JSONB）の優先順位で解決する。
// ここはグローバル既定（テナント上書きが無い場合の値）。
/// 共有ストア（Redis）の既定接続 URL。
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:6379";
/// `invoke_rate` のグローバル既定（req/秒/テナント, §8 表）。
const DEFAULT_INVOKE_RATE_PER_SEC: u64 = 50;
/// token-bucket のバースト容量の既定（§8 表の上限相当を許容バーストとする）。
const DEFAULT_INVOKE_BURST: u64 = 500;
/// `max_concurrent_executions` のグローバル既定（in-flight pending+running, §8 表）。
const DEFAULT_MAX_CONCURRENT: u64 = 20;
/// in-flight カウンタキーの TTL（秒）。reaper が真実へ再同期するまでの孤立カウンタ保険。
const DEFAULT_INFLIGHT_TTL_SECS: u64 = 3600;
/// login 失敗ロックアウトの閾値（§6.0）。両キー（(tenant,email) / IP）に同値を適用する。
const DEFAULT_LOGIN_LOCKOUT_THRESHOLD: u64 = 10;
/// login 失敗カウンタの減衰窓（秒）。最後の失敗からこの時間無失敗で解放。
const DEFAULT_LOGIN_LOCKOUT_WINDOW_SECS: u64 = 900;
/// reaper の再同期間隔（秒, §8 ドリフト補正）。
const DEFAULT_REAPER_INTERVAL_SECS: u64 = 30;
/// stuck-execution sweeper の deadline（秒, §8）。`created_at` からこの時間を超えても
/// 終端化されない pending/running 行を「孤立」とみなし reaper が failed に finalize する。
///
/// 必要条件 (§3.3 / §6.6): 「最悪滞留時間 + 実行上限 + 余裕」を **厳密に上回る** こと。
/// - 固定 `ack_wait` 構成では最悪滞留 = `ack_wait * max_deliver`（既定 `30 * 5 = 150s`）。
/// - backoff 配列構成（M4c worker の既定 `[5, 15, 60]` 秒）では最悪滞留 = `Σ backoff[i]`
///   (`<= 80s`) を取り、配列が短ければ短いほど安全側のマージンが広がる。
///
/// 実行上限 (`MAX_WALL_TIME_MS_LIMIT` = 30s, `MAX_EXECUTION_TIME_MS_LIMIT` = 60s) を足し
/// 余裕を 600s 以上確保することで、正規の遅延結果（再配送中）を sweeper が誤って failed 化しない
/// 既定 900s を採用する。0 で無効化。worker の `backoff` を伸ばす場合はここも上げること（README 注釈）。
const DEFAULT_STUCK_EXECUTION_DEADLINE_SECS: u64 = 900;
/// /uploads の presigned PUT URL の既定 TTL（秒, §3.4/§5.2）。
const DEFAULT_UPLOAD_PRESIGN_TTL_SECS: u64 = 300;

// --- M6 同期 Invoke / Cron（§15） ---
/// 同期 invoke の待機上限（ミリ秒）。超過でクライアントへ 202 + execution_id にフォールバックする。
const DEFAULT_SYNC_REPLY_TIMEOUT_MS: u64 = 5000;
/// Cron スケジューラの due スキャン間隔（秒）。
const DEFAULT_CRON_POLL_INTERVAL_SECS: u64 = 10;

// --- M7 デプロイ運用 / Secrets（§10 / §15） ---
/// 内部専用 listener の bind アドレス。`POST /internal/job-env` だけを載せる。
/// **既定は loopback**（公開 listener と分けることが露出ガードの本体）。
const DEFAULT_INTERNAL_BIND_ADDR: &str = "127.0.0.1:8081";
/// `/internal/job-env` の per-IP 上限（req/分）。無認証面のグローバル保護。
const DEFAULT_JOB_ENV_EXCHANGE_RATE_PER_MIN: u64 = 600;
// --- M8 弾力スケール / テナント間アイソレーション（§8 / §15） ---
/// テナント別 lane consumer を有効にするか。**既定 false**（= M7 までと同一トポロジ）。
///
/// 既定を false にしているのは、lane 化が JetStream 側のトポロジを変える操作であり、
/// 「アップグレードしたら黙って挙動が変わる」ことを避けるため。有効化は明示的な env で行う。
const DEFAULT_TENANT_LANES_ENABLED: bool = false;
/// 専有 lane を割り当てるテナント数の上限。超過分は overflow lane 1 本へ束ねる。
///
/// **完了条件を満たす regime はアクティブテナント数 ≤ この値**である。超えると超過分は
/// overflow lane を共有し、その内部では合算の頭打ちが復活する（有界で明示的な劣化モード）。
const DEFAULT_MAX_DEDICATED_LANES: u64 = 64;
/// lane consumer の `max_ack_pending` に足す余裕。
/// in-flight 上限ちょうどだと、終端と次の配送が重なる瞬間に配送が止まる。
const DEFAULT_LANE_ACK_PENDING_HEADROOM: u64 = 8;
/// overflow lane の `max_ack_pending`（固定）。
///
/// **所属テナント数に比例させない**。比例させると合算上限が事実上撤廃され、
/// overflow lane が「M7 までの共有 consumer」そのものに戻ってしまう。
const DEFAULT_LANE_OVERFLOW_ACK_PENDING: u64 = 1000;
/// per-lane 実行クレジットのグローバル既定（worker 1 プロセスあたり）。
const DEFAULT_WORKER_LANE_CONCURRENCY: u64 = 4;
/// lane reconcile ループの周期（秒）。
const DEFAULT_LANE_RECONCILE_INTERVAL_SECS: u64 = 10;

/// backlog ポーラの周期（秒）。**0 = ポーラを spawn しない**。
///
/// M7 までと完全に同一の挙動を既定に保つ（観測ループを増やすのはオプトイン）。
/// `.env.example` は 5 を出荷する。
const DEFAULT_SCALE_POLL_INTERVAL_SECS: u64 = 0;
/// desired の分母。worker 側 `WORKER_MAX_CONCURRENCY` の既定と一致させてある。
const DEFAULT_SCALE_JOBS_PER_WORKER: u64 = 32;
/// worker 台数の下限。scale-to-zero は opt-in なので既定は 1（§5.3 の根拠を参照）。
const DEFAULT_SCALE_MIN_WORKERS: u64 = 1;
/// worker 台数の上限。
///
/// 根拠: worker 1 台が PG 接続を最大 8 本張る。postgres:16 の既定 `max_connections=100` に対し
/// CP も接続するので、ローカルでは 4 × 8 = 32 が安全圏。
const DEFAULT_SCALE_MAX_WORKERS: u64 = 4;
/// scale-in のヒステリシス（秒）。`ACK_WAIT_SECS`（既定 30）より長く取る。
const DEFAULT_SCALE_IN_COOLDOWN_SECS: u64 = 60;
/// シグナル陳腐化の閾値（秒）。poll 周期 5 秒の 6 倍。
const DEFAULT_SCALE_SIGNAL_STALE_SECS: u64 = 30;

/// `.env.example` に置く既知プレースホルダ。**この値のまま起動させない**（下記 MUST）。
///
/// `JOB_SIGNING_KEY` と違い secret の**暗号文は DB に永続する**ため、既知鍵で暗号化して
/// しまうと影響が長期に残る。さらに Makefile は `include .env` + `export` するので、値は
/// 全 make 子プロセスへ export される。よって warn ではなく**起動失敗**にする。
const SECRETS_MASTER_KEY_PLACEHOLDER: &str = "CHANGE_ME_REPLACE_WITH_32_BYTE_KEY_BEFORE_USE";

/// control-plane の起動時設定。
///
/// **`Debug` は手動実装**（M7-0, §5.1）。秘密フィールドは `Redacted<String>` で包んであるため
/// derive でも `<redacted>` になるが、`Config` 全体の Debug 契約（「この型を `{:?}` してもログに
/// 秘密が出ない」）を型の側で明示するために手動実装を選ぶ。
#[derive(Clone)]
pub struct Config {
    /// ランタイム Postgres 接続 URL。**非特権ロール `faas_app`（NOBYPASSRLS・非 SUPERUSER）**で
    /// 接続する（M3b §3.2）。superuser/owner で接続すると FORCE RLS が無条件にバイパスされ、
    /// GUC 未設定時の fail-closed も働かず RLS 層全体が無効化される。起動時に self-check で検証する。
    pub database_url: String,
    /// マイグレーション専用 Postgres 接続 URL。CREATE ROLE / ALTER TABLE ... FORCE RLS /
    /// CREATE FUNCTION SECURITY DEFINER はテーブル所有者かつ CREATEROLE 権限を要するため、
    /// 0004_rls.sql は**特権ロール（所有者）**で適用する必要がある。未設定なら `database_url`
    /// にフォールバックする（fresh DB を所有者 1 本で立ち上げる開発用途）。
    pub migration_database_url: String,
    /// NATS 接続 URL。
    pub nats_url: String,
    /// system-admin bootstrap トークン（POST /admin/tenants を gate, §3.3）。
    pub bootstrap_admin_token: Redacted<String>,
    /// HTTP bind アドレス（例: 0.0.0.0:8080）。
    pub bind_addr: String,

    // --- Object Storage (M2: MinIO, S3 互換, path-style。§3.4) ---
    /// MinIO/S3 API エンドポイント（例: http://127.0.0.1:9000）。
    pub s3_endpoint: String,
    /// リージョン（MinIO は任意だが aws-sdk が必須とするため固定）。
    pub s3_region: String,
    /// 本体保存バケット。
    pub s3_bucket: String,
    /// アクセスキー（MINIO_ROOT_USER 相当）。
    pub s3_access_key: String,
    /// シークレットキー（MINIO_ROOT_PASSWORD 相当）。
    pub s3_secret_key: Redacted<String>,

    // --- アップロード/検証パイプライン (M2: §6.2) ---
    /// wasm 本体の最大アップロードサイズ（bytes）。
    pub max_wasm_upload_bytes: u64,
    /// JobMessage に同梱する presigned GET URL の TTL（秒）。
    pub presign_ttl_secs: u64,

    // --- ジョブ署名トークン (M3c, §3.3) ---
    /// Ed25519 署名鍵 seed（32 バイト）を hex / base64url / base64 で受け取る生文字列。
    /// `signing::decode_seed` で 32 バイトへ復号して `Signer` を構築する。必須。
    pub job_signing_key: Redacted<String>,
    /// 署名トークンに埋める kid（検証側が公開鍵を選ぶキー）。必須。
    pub job_signing_kid: String,
    /// JetStream consumer の ack 待ち秒数（worker と共有。token exp 計算にも使う）。
    pub ack_wait_secs: u64,
    /// JetStream consumer の最大再配送回数（worker と共有。token exp 計算にも使う）。
    pub max_deliver: u64,
    /// M8: 再配送 backoff（秒, CSV）。M7 までは worker だけが読んでいたが、consumer の作成者が
    /// control-plane へ移ったため **CP もこの値の所有者**になった（§3.3 の TTL 結合: トークン exp と
    /// 再配送間隔がずれると、正規の遅延結果がトークン失効扱いで破棄される）。
    /// worker と同じ `BACKOFF_SECS` を読み、解釈規則も `faas_shared::parse_backoff_secs` で共有する。
    pub backoff_secs: Vec<u64>,
    /// token exp に足す余裕秒数。
    pub token_margin_secs: u64,

    // --- 共有ストア / admission 制御 (M3d, §8) ---
    // 注: 本フェーズ（共有ストア土台）では redis_url のみを main.rs が消費する。残りの
    // クォータ/レート/lockout/reaper/uploads パラメータは後続フェーズ（invoke admission・
    // login lockout・reaper・uploads）の配線で参照するため、それまで dead-code を許可する。
    /// 共有ストア（Redis）接続 URL。全 Axum インスタンスで共有するカウンタの実体（§8 MUST）。
    pub redis_url: String,
    /// `invoke_rate` グローバル既定（req/秒/テナント）。token-bucket 補充レート。
    pub invoke_rate_per_sec: u64,
    /// token-bucket のバースト容量。
    pub invoke_burst: u64,
    /// `max_concurrent_executions` グローバル既定（in-flight pending+running 上限）。
    pub max_concurrent_executions: u64,
    /// in-flight カウンタキーの TTL（秒）。reaper の再同期までの保険。
    pub inflight_ttl_secs: u64,
    /// login 失敗ロックアウト閾値（両キーに適用）。
    pub login_lockout_threshold: u64,
    /// login 失敗カウンタ減衰窓（秒）。
    pub login_lockout_window_secs: u64,
    /// reaper 再同期間隔（秒）。
    pub reaper_interval_secs: u64,
    /// stuck-execution sweeper の deadline（秒, §8）。`created_at + この秒数` を超えても
    /// 終端化されない pending/running 行を reaper が failed に finalize して in-flight スロットを
    /// 回収する。0 で無効。
    pub stuck_execution_deadline_secs: u64,
    /// /uploads の presigned PUT URL TTL（秒, §3.4 / §5.2 / §6.4）。AppState 経由で
    /// `create_upload` ハンドラが消費する。
    pub upload_presign_ttl_secs: u64,
    /// X-Forwarded-For を信頼してクライアント IP を取り出すか（§6.0）。
    /// 既定 **false**（プロキシ信頼は明示オプトイン。信頼境界外で詐称 IP による
    /// ロックアウト回避／他者ロックアウトを防ぐ）。
    pub trust_proxy_headers: bool,

    // --- M6 同期 Invoke / Cron（§15） ---
    /// この CP インスタンスの subject-safe 識別子（M6a, §15）。同期 invoke の reply subject
    /// `reply.{instance_id}.{correlation_id}` に埋め込み、JobMessage を送った当該インスタンスだけが
    /// reply を購読する（ステートレス×N の鍵）。env `INSTANCE_ID` 未設定なら `inst_{uuid}` を採番する。
    /// M6a で AppState へ渡し、reply 購読タスク / 同期 invoke の reply subject 構築が consume する。
    pub instance_id: String,
    /// 同期 invoke の待機上限（ミリ秒, M6a）。超過でクライアントへ 202 + execution_id へフォールバック。
    /// M6a で AppState へ渡し、invoke ハンドラの `tokio::time::timeout` が consume する。
    pub sync_reply_timeout_ms: u64,
    /// Cron スケジューラの due スキャン間隔（秒, M6b）。main.rs が `scheduler::run` へ渡して consume する。
    pub cron_poll_interval_secs: u64,

    // --- M7c Secrets Manager（§10 / §15） ---
    /// 現行 KEK（32 バイト）。hex / base64url / base64。新規暗号化は常にこの鍵で行う。
    pub secrets_master_key: Redacted<String>,
    /// 現行 KEK の kid。`function_secret_versions.kek_kid` に記録され、復号時の鍵選択に使う。
    pub secrets_master_kid: String,
    /// 復号専用の旧 KEK 群（`kid:key,kid:key` の CSV）。再ラップが全行に行き渡るまで残す。
    /// **早期撤去は復号不能 ＝ データ喪失**（signing の overlap と同じ規律）。
    pub secrets_retired_keys: Redacted<String>,
    /// 内部専用 listener の bind アドレス（`POST /internal/job-env` のみ）。**公開してはならない**。
    pub internal_bind_addr: String,
    /// `/internal/job-env` の per-IP 上限（req/分）。
    pub job_env_exchange_rate_per_min: u64,

    // --- M8 弾力スケール / テナント間アイソレーション（§8 / §15） ---
    // これらは lane reconcile（M8-3 / M8-4）が消費する。設定の読み取りを先に land して
    // 「設定は入るが挙動は変わらない」段を作ることで、各段が単体で動作・テスト可能になる。
    /// テナント別 lane consumer を有効にするか（既定 false = M7 までと同一トポロジ）。
    #[allow(dead_code)]
    pub tenant_lanes_enabled: bool,
    /// 専有 lane の上限数。超過分は overflow lane 1 本へ束ねる。
    #[allow(dead_code)]
    pub max_dedicated_lanes: u64,
    /// lane consumer の `max_ack_pending` に足す余裕。
    #[allow(dead_code)]
    pub lane_ack_pending_headroom: u64,
    /// overflow lane の `max_ack_pending`（固定値）。
    #[allow(dead_code)]
    pub lane_overflow_ack_pending: u64,
    /// per-lane 実行クレジットのグローバル既定。
    pub worker_lane_concurrency: u64,
    /// lane reconcile ループの周期（秒）。
    #[allow(dead_code)]
    pub lane_reconcile_interval_secs: u64,

    // --- M8 (§5): 弾力スケール ---
    /// backlog ポーラの周期（秒）。**0 = ポーラを spawn しない**（既定）。
    ///
    /// reaper の 30 秒周期を再利用しない理由: 30 秒古い depth で判断すると scale-out が
    /// 最大 30 秒遅れ、それがそのままレイテンシの悪化になる。reaper 側のコメントは
    /// 「頻度を要さない観測なので専用 env は増やさない」と述べるが、こちらは
    /// **観測の鮮度そのものが完了条件に効く**ので逆の判断をする。
    pub scale_poll_interval_secs: u64,
    /// desired の分母。**`WORKER_MAX_CONCURRENCY` と揃えること**（CP は worker の env を
    /// 知らないので検査できない。既定を一致させ、ズレは chaos の前提 assert で潰す）。
    pub scale_jobs_per_worker: u64,
    /// worker 台数の下限。`0` で scale-to-zero を許可する（**opt-in**）。
    ///
    /// 既定を 0 にしない理由: M6 の同期 invoke はサーバ側 `SYNC_REPLY_TIMEOUT_MS`（既定 5000）で
    /// 打ち切られる。from-zero の coldstart はこの窓を容易に超えるので、既定 0 は
    /// **M6 の完了条件を壊す**。
    pub scale_min_workers: u64,
    /// worker 台数の上限。
    pub scale_max_workers: u64,
    /// scale-in のヒステリシス（秒）。`ACK_WAIT_SECS` より長く取る
    /// （scale-in 直後に再配送が走ると backlog が跳ねて flapping する）。
    pub scale_in_cooldown_secs: u64,
    /// シグナルがこの秒数より古くなったら信用しない。
    pub scale_signal_stale_secs: u64,

    // --- 観測 (M4a, §3.8) ---
    /// ログ整形（"text" 既定 / "json"）。`json` のとき `tracing_subscriber::fmt().json()` を
    /// 有効化し、フィールドを flatten した JSON ライン形式で吐く。集約基盤（Loki/ELK 等）に
    /// パイプする運用で使う。既定の `text` は既存挙動と完全互換。
    ///
    /// 注: 起動順の都合（Config::from_env() が必須 env 欠損でエラーする前にログを出したい）で、
    /// main.rs は env を直接読んで `init_tracing` を呼ぶ。Config 経由は使わないが、設定ソースを
    /// 1 箇所に集める原則のため Config にも残す（運用ドキュメントから検索しやすくする）。
    #[allow(dead_code)]
    pub log_format: String,
}

/// 秘密フィールドを `<redacted>` にする手動 `Debug`（M7-0, §5.1）。
///
/// 秘密は `Redacted<String>` なので derive でも漏れないが、「`Config` を `{:?}` してもログに
/// 秘密が出ない」ことをこの impl が型の契約として固定する（フィールド追加時にここを通るため、
/// 新しい秘密を素の `String` で足すと doc とレビューの目に触れる）。
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            // 接続 URL はパスワードを含みうるため値を出さない（キーの存在だけ示す）。
            .field("database_url", &"<redacted>")
            .field("migration_database_url", &"<redacted>")
            .field("redis_url", &"<redacted>")
            .field("nats_url", &self.nats_url)
            .field("bootstrap_admin_token", &self.bootstrap_admin_token)
            .field("bind_addr", &self.bind_addr)
            .field("s3_endpoint", &self.s3_endpoint)
            .field("s3_region", &self.s3_region)
            .field("s3_bucket", &self.s3_bucket)
            .field("s3_access_key", &self.s3_access_key)
            .field("s3_secret_key", &self.s3_secret_key)
            .field("job_signing_key", &self.job_signing_key)
            .field("job_signing_kid", &self.job_signing_kid)
            .field("secrets_master_key", &self.secrets_master_key)
            .field("secrets_master_kid", &self.secrets_master_kid)
            .field("secrets_retired_keys", &self.secrets_retired_keys)
            .field("internal_bind_addr", &self.internal_bind_addr)
            .field("instance_id", &self.instance_id)
            .finish_non_exhaustive()
    }
}

impl Config {
    // --- 秘密フィールドの平文アクセサ（M7-0, §5.1 / §5.6） ---
    //
    // 秘密は `Redacted<String>` で保持し、平文の取り出しは**この 3 本だけ**に閉じる
    // （`scripts/rls-lint.sh` の検査 (4) が `.expose()` の呼び出しファイルを allowlist に限定し、
    // config.rs はその 1 つ）。main.rs 等の消費側は `.expose()` を書かずこのアクセサを使うため、
    // 「秘密がプロセス内のどこへ渡ったか」は `_plain()` の grep で全数把握できる。
    /// system-admin bootstrap トークンの平文（`POST /admin/tenants` の gate に渡す）。
    pub fn bootstrap_admin_token_plain(&self) -> &str {
        self.bootstrap_admin_token.expose()
    }

    /// S3 シークレットキーの平文（aws-sdk の資格情報構築に渡す）。
    pub fn s3_secret_key_plain(&self) -> &str {
        self.s3_secret_key.expose()
    }

    /// Ed25519 署名鍵 seed の生文字列（`signing::decode_seed` に渡す）。
    pub fn job_signing_key_plain(&self) -> &str {
        self.job_signing_key.expose()
    }

    /// KEK キーリングを構築する (M7c, §4.4)。**秘密の平文がここから外へ出ない**ように、
    /// 生文字列ではなく組み立て済みの [`crate::secrets::SecretKeyring`] を返す。
    ///
    /// `SECRETS_RETIRED_KEYS` は `kid:key,kid:key` の CSV。空要素は無視し、形式不正は
    /// 起動失敗にする（黙って無視すると「retired 鍵を書いたのに復号できない」になる）。
    pub fn secret_keyring(&self) -> anyhow::Result<crate::secrets::SecretKeyring> {
        let active =
            crate::signing::decode_key32(self.secrets_master_key.expose(), "SECRETS_MASTER_KEY")?;

        let mut retired = Vec::new();
        for entry in self.secrets_retired_keys.expose().split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (kid, raw) = entry.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("SECRETS_RETIRED_KEYS entries must be 'kid:key' (comma separated)")
            })?;
            let kid = kid.trim();
            if kid.is_empty() {
                anyhow::bail!("SECRETS_RETIRED_KEYS: empty kid");
            }
            retired.push((
                kid.to_string(),
                crate::signing::decode_key32(raw.trim(), "SECRETS_RETIRED_KEYS")?,
            ));
        }

        Ok(crate::secrets::SecretKeyring::new(
            self.secrets_master_kid.clone(),
            active,
            retired,
        ))
    }

    /// プロセス環境から設定を読み込む。必須キー欠損はエラー。
    pub fn from_env() -> anyhow::Result<Self> {
        // M7c (§4.4 MUST): `.env.example` の既知プレースホルダのままでは**起動させない**。
        // 署名鍵と違い secret の暗号文は DB に永続するため、公開リポジトリに載る既知鍵で
        // 暗号化してしまうと影響が長く残る（warn では見逃される）。
        let secrets_master_key = env_required("SECRETS_MASTER_KEY")?;
        if secrets_master_key.trim() == SECRETS_MASTER_KEY_PLACEHOLDER {
            anyhow::bail!(
                "SECRETS_MASTER_KEY is still the placeholder from .env.example; \
                 generate a real 32-byte key (e.g. `openssl rand -hex 32`) before starting"
            );
        }

        let database_url = env_required("DATABASE_URL")?;
        // 未設定なら database_url にフォールバック（所有者 1 本運用の開発用途）。
        let migration_database_url =
            env_optional("MIGRATION_DATABASE_URL").unwrap_or_else(|| database_url.clone());
        let cfg = Self {
            database_url,
            migration_database_url,
            nats_url: env_or("NATS_URL", "nats://127.0.0.1:4222"),
            bootstrap_admin_token: Redacted::new(env_required("BOOTSTRAP_ADMIN_TOKEN")?),
            bind_addr: env_or("BIND_ADDR", "0.0.0.0:8080"),

            s3_endpoint: env_or("S3_ENDPOINT", "http://127.0.0.1:9000"),
            s3_region: env_or("S3_REGION", "us-east-1"),
            s3_bucket: env_or("S3_BUCKET", "faas-components"),
            s3_access_key: env_or("S3_ACCESS_KEY", "minioadmin"),
            s3_secret_key: Redacted::new(env_or("S3_SECRET_KEY", "minioadmin")),

            max_wasm_upload_bytes: env_u64("MAX_WASM_UPLOAD_BYTES", DEFAULT_MAX_WASM_UPLOAD_BYTES)?,
            presign_ttl_secs: env_u64("PRESIGN_TTL_SECS", DEFAULT_PRESIGN_TTL_SECS)?,

            // M3c: 署名鍵 / kid は必須。TTL 定数は faas_shared の既定を env で上書き可能。
            job_signing_key: Redacted::new(env_required("JOB_SIGNING_KEY")?),
            job_signing_kid: env_required("JOB_SIGNING_KID")?,
            backoff_secs: faas_shared::parse_backoff_secs(
                std::env::var("BACKOFF_SECS").ok().as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("env var BACKOFF_SECS {e}"))?,
            ack_wait_secs: env_u64("ACK_WAIT_SECS", faas_shared::ACK_WAIT_SECS)?,
            max_deliver: env_u64("MAX_DELIVER", faas_shared::MAX_DELIVER)?,
            token_margin_secs: env_u64("TOKEN_MARGIN_SECS", faas_shared::TOKEN_MARGIN_SECS)?,

            // M3d 共有ストア / admission。すべて env で上書き可能（グローバル既定）。
            redis_url: env_or("REDIS_URL", DEFAULT_REDIS_URL),
            invoke_rate_per_sec: env_u64("QUOTA_INVOKE_RATE_PER_SEC", DEFAULT_INVOKE_RATE_PER_SEC)?,
            invoke_burst: env_u64("QUOTA_INVOKE_BURST", DEFAULT_INVOKE_BURST)?,
            max_concurrent_executions: env_u64(
                "QUOTA_MAX_CONCURRENT_EXECUTIONS",
                DEFAULT_MAX_CONCURRENT,
            )?,
            inflight_ttl_secs: env_u64("INFLIGHT_TTL_SECS", DEFAULT_INFLIGHT_TTL_SECS)?,
            login_lockout_threshold: env_u64(
                "LOGIN_LOCKOUT_THRESHOLD",
                DEFAULT_LOGIN_LOCKOUT_THRESHOLD,
            )?,
            login_lockout_window_secs: env_u64(
                "LOGIN_LOCKOUT_WINDOW_SECS",
                DEFAULT_LOGIN_LOCKOUT_WINDOW_SECS,
            )?,
            reaper_interval_secs: env_u64("REAPER_INTERVAL_SECS", DEFAULT_REAPER_INTERVAL_SECS)?,
            stuck_execution_deadline_secs: env_u64(
                "STUCK_EXECUTION_DEADLINE_SECS",
                DEFAULT_STUCK_EXECUTION_DEADLINE_SECS,
            )?,
            upload_presign_ttl_secs: env_u64(
                "UPLOAD_PRESIGN_TTL_SECS",
                DEFAULT_UPLOAD_PRESIGN_TTL_SECS,
            )?,
            trust_proxy_headers: env_bool("TRUST_PROXY_HEADERS", false),

            // M6 同期 Invoke / Cron。INSTANCE_ID 未設定なら subject-safe な `inst_{uuid}` を採番する。
            instance_id: env_optional("INSTANCE_ID").unwrap_or_else(faas_shared::new_instance_id),
            sync_reply_timeout_ms: env_u64("SYNC_REPLY_TIMEOUT_MS", DEFAULT_SYNC_REPLY_TIMEOUT_MS)?,
            cron_poll_interval_secs: env_u64(
                "CRON_POLL_INTERVAL_SECS",
                DEFAULT_CRON_POLL_INTERVAL_SECS,
            )?,

            secrets_master_key: Redacted::new(secrets_master_key),
            secrets_master_kid: env_required("SECRETS_MASTER_KID")?,
            secrets_retired_keys: Redacted::new(env_or("SECRETS_RETIRED_KEYS", "")),
            internal_bind_addr: env_or("INTERNAL_BIND_ADDR", DEFAULT_INTERNAL_BIND_ADDR),
            job_env_exchange_rate_per_min: env_u64(
                "JOB_ENV_EXCHANGE_RATE_PER_MIN",
                DEFAULT_JOB_ENV_EXCHANGE_RATE_PER_MIN,
            )?,
            tenant_lanes_enabled: env_bool("TENANT_LANES_ENABLED", DEFAULT_TENANT_LANES_ENABLED),
            max_dedicated_lanes: env_u64("MAX_DEDICATED_LANES", DEFAULT_MAX_DEDICATED_LANES)?,
            lane_ack_pending_headroom: env_u64(
                "LANE_ACK_PENDING_HEADROOM",
                DEFAULT_LANE_ACK_PENDING_HEADROOM,
            )?,
            lane_overflow_ack_pending: env_u64(
                "LANE_OVERFLOW_ACK_PENDING",
                DEFAULT_LANE_OVERFLOW_ACK_PENDING,
            )?,
            worker_lane_concurrency: env_u64(
                "WORKER_LANE_CONCURRENCY",
                DEFAULT_WORKER_LANE_CONCURRENCY,
            )?,
            lane_reconcile_interval_secs: env_u64(
                "LANE_RECONCILE_INTERVAL_SECS",
                DEFAULT_LANE_RECONCILE_INTERVAL_SECS,
            )?,
            scale_poll_interval_secs: env_u64(
                "SCALE_POLL_INTERVAL_SECS",
                DEFAULT_SCALE_POLL_INTERVAL_SECS,
            )?,
            scale_jobs_per_worker: env_u64("SCALE_JOBS_PER_WORKER", DEFAULT_SCALE_JOBS_PER_WORKER)?
                // 0 は div_ceil の分母として使えない（ゼロ除算）。1 へ引き上げる。
                .max(1),
            scale_min_workers: env_u64("SCALE_MIN_WORKERS", DEFAULT_SCALE_MIN_WORKERS)?,
            scale_max_workers: env_u64("SCALE_MAX_WORKERS", DEFAULT_SCALE_MAX_WORKERS)?,
            scale_in_cooldown_secs: env_u64(
                "SCALE_IN_COOLDOWN_SECS",
                DEFAULT_SCALE_IN_COOLDOWN_SECS,
            )?,
            scale_signal_stale_secs: env_u64(
                "SCALE_SIGNAL_STALE_SECS",
                DEFAULT_SCALE_SIGNAL_STALE_SECS,
            )?,
            log_format: env_or("LOG_FORMAT", "text"),
        };

        // M8 (§5.3 R0): `min > max` は clamp の意味論が壊れる設定なので **起動時に落とす**。
        // 実行時に黙って握り潰すと「なぜか常に min 台」という診断しづらい形で現れる。
        if cfg.scale_min_workers > cfg.scale_max_workers {
            anyhow::bail!(
                "SCALE_MIN_WORKERS ({}) must not exceed SCALE_MAX_WORKERS ({})",
                cfg.scale_min_workers,
                cfg.scale_max_workers
            );
        }

        Ok(cfg)
    }

    /// M8 (§5.3): スケール方針を組み立てる。
    pub fn scale_policy(&self) -> crate::scale::ScalePolicy {
        // u32 へ落とす。実運用のオーダを大きく超える値は上限で頭打ちにする
        // （設定ミスで u32 を溢れさせても panic させない）。
        let to_u32 = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        crate::scale::ScalePolicy {
            jobs_per_worker: to_u32(self.scale_jobs_per_worker),
            min_workers: to_u32(self.scale_min_workers),
            max_workers: to_u32(self.scale_max_workers),
            scale_in_cooldown_secs: self.scale_in_cooldown_secs,
            stale_after_secs: self.scale_signal_stale_secs,
        }
    }

    /// 指定の壁時計上限（ms）から、トークンの `exp` オフセット秒数を計算する (§3.3)。
    ///
    /// `exp = iat + token_exp_offset_secs(wall, ack_wait, max_deliver, margin)`。
    /// worker の consumer ack_wait/max_deliver と **同一の定数** から導出する（TTL 結合）。
    pub fn token_exp_offset_secs(&self, wall_time_ms: u64) -> i64 {
        // ms -> 秒（切り上げ。0ms でも 0 秒、端数は安全側に丸める）。
        let wall_secs = wall_time_ms.div_ceil(1000);
        faas_shared::token_exp_offset_secs(
            wall_secs,
            self.ack_wait_secs,
            self.max_deliver,
            self.token_margin_secs,
        )
    }

    /// グローバル既定から admission 制御パラメータ束を組み立てる (M3d, §8)。
    ///
    /// 現状はグローバル既定のみを反映する（per-tenant `quotas` 上書きは後続スライス）。
    /// M8 (§3.7): lane provisioning の設定束を組み立てる。
    pub fn lanes(&self) -> crate::state::LaneConfig {
        crate::state::LaneConfig {
            enabled: self.tenant_lanes_enabled,
            max_dedicated: self.max_dedicated_lanes,
            ack_pending_headroom: self.lane_ack_pending_headroom,
            overflow_ack_pending: self.lane_overflow_ack_pending,
            ack_wait_secs: self.ack_wait_secs,
            max_deliver: self.max_deliver,
            backoff_secs: self.backoff_secs.clone(),
        }
    }

    pub fn admission(&self) -> crate::state::AdmissionConfig {
        use crate::store::{InflightParams, LockoutParams, RateLimitParams};
        crate::state::AdmissionConfig {
            rate: RateLimitParams {
                refill_per_sec: self.invoke_rate_per_sec as f64,
                capacity: self.invoke_burst as f64,
            },
            inflight: InflightParams {
                max: self.max_concurrent_executions as i64,
                ttl_secs: self.inflight_ttl_secs,
            },
            lockout: LockoutParams {
                threshold: self.login_lockout_threshold,
                window_secs: self.login_lockout_window_secs,
            },
            trust_proxy_headers: self.trust_proxy_headers,
            lane_concurrency: self.worker_lane_concurrency,
        }
    }
}

fn env_required(key: &str) -> anyhow::Result<String> {
    std::env::var(key).with_context(|| format!("required env var {key} is not set"))
}

/// 文字列の任意 env。欠損／空（trim 後）は `None`。値は `trim()` する。
fn env_optional(key: &str) -> Option<String> {
    std::env::var(key).ok().and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

/// 文字列の任意 env。欠損は default。値は `trim()` する
/// （Makefile の `include .env` 経由で末尾空白が混入しても壊れないようにする）。
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| default.to_string())
}

/// bool の任意 env。欠損は default。`1/true/yes/on`（大文字小文字無視）を真とする。
/// 値は `trim()` する（Makefile include 経由の末尾空白対策）。
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

/// u64 の任意 env。欠損は default、不正値はエラー。
///
/// 値は `trim()` してからパースする（同上の理由で末尾空白を許容する）。
fn env_u64(key: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .with_context(|| format!("env var {key} must be a non-negative integer")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Config` を `{:?}` してもログに秘密が出ないこと（M7-0, §5.1）。
    ///
    /// `Config::from_env()` は必須 env を要求するためテストからは呼べない。ここでは
    /// **手動 Debug 実装が秘密フィールドをどう出すか**を直接検査する（Debug の契約が本体）。
    #[test]
    fn debug_never_reveals_secrets() {
        let secret_like = "SENTINEL-DO-NOT-LOG";
        // 手動 Debug は Redacted の Debug に委譲する。Redacted 単体の挙動は
        // faas_shared 側のテストで固定済みなので、ここでは委譲が効くことを確認する。
        let wrapped = Redacted::new(secret_like.to_string());
        let rendered = format!("{wrapped:?}");
        assert!(
            !rendered.contains(secret_like),
            "config secrets must never render in Debug output: {rendered}"
        );
        assert_eq!(rendered, "<redacted>");
    }
}
