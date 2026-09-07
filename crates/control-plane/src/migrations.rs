//! Schema maintenance and migration history checks.
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
/// `--migrate-only` モード: 起動時と同じ migrator を流して exit する。
///
/// `Config::from_env()` を経由しないため、BOOTSTRAP_ADMIN_TOKEN や JOB_SIGNING_KEY 等の
/// ランタイム用 env を要求しない（migrate に必要なのは DB URL だけ）。`MIGRATION_DATABASE_URL`
/// が未設定なら `DATABASE_URL` にフォールバックする（所有者 1 本運用の開発用途）。
///
/// 0004_rls.sql はテーブル所有者 + CREATEROLE 権限を要するため、本番では
/// `MIGRATION_DATABASE_URL` に所有者ロールの URL を渡すこと（ランタイムの faas_app では適用不可）。
pub(crate) async fn run_migrate_only() -> anyhow::Result<()> {
    use anyhow::Context;

    let migration_url = std::env::var("MIGRATION_DATABASE_URL")
        .ok()
        .and_then(|v| {
            let t = v.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        })
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .context("MIGRATION_DATABASE_URL or DATABASE_URL must be set for --migrate-only")?;

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&migration_url)
        .await
        .context("connecting to postgres for --migrate-only")?;
    run_migrations(&pool).await?;
    pool.close().await;
    tracing::info!("migrations applied (idempotent); exiting --migrate-only");
    Ok(())
}

/// 埋め込みマイグレーション（`migrations/`）。コンパイル時に取り込む。
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// 手動適用済みで、ランナー導入時点では `_sqlx_migrations` に未登録のバージョン。
/// これらは **スキーマが実在する場合に限り** baseline 行を挿入して再適用をスキップ
/// する（チェックサム不一致による起動中断も回避する）。0003 以降は通常どおりランナー
/// が適用する。
///
/// 各エントリの第 2 要素は「当該マイグレーションの DDL が実適用済みか」を判定する
/// `SELECT <bool>` プローブ。fresh DB（DDL 未適用）では false を返し、baseline を
/// 行わない → `Migrator::run` が 0001/0002 を通常適用する。これにより from-scratch
/// bootstrap でも 0003 が tenants 不在に当たって落ちる事故を防ぐ。
const BASELINE_VERSIONS: &[(i64, &str)] = &[
    // 0001_init: tenants テーブルの存在で実適用を判定する。
    (1, "SELECT to_regclass('public.tenants') IS NOT NULL"),
    // 0002_m2: component_versions.size_bytes 列の存在で実適用を判定する。
    (
        2,
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name = 'component_versions' AND column_name = 'size_bytes')",
    ),
];

/// pending マイグレーションを適用する。
///
/// 0001/0002 は M1/M2 で手動適用済みのため、まず `_sqlx_migrations` を初期化し、
/// 該当バージョンの DDL が **実在するときに限り** baseline 行（埋め込みファイルの
/// チェックサム付き）を挿入してから `Migrator::run` を呼ぶ。これにより:
/// - 既適用環境（M1/M2 手動適用済み）: 0003 以降のみが適用され、既適用分の再実行や
///   チェックサム検証失敗による中断が起こらない。
/// - fresh DB（DDL 未適用）: baseline をスキップし、`Migrator::run` が 0001/0002 から
///   順に適用する（0003 が tenants 不在で落ちない）。
///
/// 何度呼んでも安全（冪等）。
pub(crate) async fn run_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use anyhow::Context;

    // sqlx の管理テーブルを作成（存在すれば no-op）。Migrator と同一スキーマ。
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
            success BOOLEAN NOT NULL,
            checksum BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .context("creating _sqlx_migrations table")?;

    for mig in MIGRATOR.iter() {
        let Some((_, probe)) = BASELINE_VERSIONS.iter().find(|(v, _)| *v == mig.version) else {
            continue;
        };

        // DDL が実適用済みのときのみ baseline する。fresh DB では false → 通常適用に委ねる。
        let applied: bool = sqlx::query_scalar(probe)
            .fetch_one(pool)
            .await
            .with_context(|| format!("probing migration {} state", mig.version))?;
        if !applied {
            tracing::info!(
                version = mig.version,
                "schema not present; will apply migration normally (not baselining)"
            );
            continue;
        }

        // baseline 対象は埋め込みチェックサムで行を挿入（既存行があれば触らない）。
        let inserted = sqlx::query(
            r#"INSERT INTO _sqlx_migrations
                   (version, description, success, checksum, execution_time)
               VALUES ($1, $2, TRUE, $3, 0)
               ON CONFLICT (version) DO NOTHING"#,
        )
        .bind(mig.version)
        .bind(mig.description.as_ref())
        .bind(mig.checksum.as_ref())
        .execute(pool)
        .await
        .with_context(|| format!("baselining migration {}", mig.version))?;

        if inserted.rows_affected() > 0 {
            tracing::info!(version = mig.version, "baselined pre-applied migration");
        }
    }

    // 0003 以降の pending を適用する。baseline 済みは内容一致のためスキップされる。
    MIGRATOR
        .run(pool)
        .await
        .context("running pending migrations")?;
    tracing::info!("migrations up to date");

    Ok(())
}

/// ランタイム接続ロールが RLS をバイパスしないことを起動時に検証する（M3b §3.2）。
///
/// FORCE RLS は SUPERUSER と BYPASSRLS ロールに対しては無条件にバイパスされる。その場合
/// テナント分離は WHERE 述語のみに退化し、GUC 未設定時の fail-closed も働かない。ランタイムが
/// 誤って特権ロール（典型的には postgres イメージの SUPERUSER な POSTGRES_USER）で接続して
/// いたら、ここで fail-fast して RLS 層が「飾り」になる事故を防ぐ。
///
/// `pg_roles` の `rolsuper` / `rolbypassrls` を `current_user` について確認する。
pub(crate) async fn assert_non_privileged_runtime_role(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    use anyhow::{bail, Context};

    let row: (String, bool, bool) = sqlx::query_as(
        "SELECT rolname, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .context("probing runtime DB role privileges")?;
    let (rolname, rolsuper, rolbypassrls) = row;

    if rolsuper || rolbypassrls {
        bail!(
            "runtime DATABASE_URL connects as privileged role '{rolname}' \
             (rolsuper={rolsuper}, rolbypassrls={rolbypassrls}); RLS would be bypassed. \
             Point DATABASE_URL at the non-privileged 'faas_app' role and keep the privileged \
             role only in MIGRATION_DATABASE_URL."
        );
    }
    tracing::info!(role = %rolname, "runtime DB role is non-privileged (RLS enforced)");
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::MIGRATOR;

    // 0007_usage_metering.sql のソースを**コンパイル時**に埋め込む（DB-free 検査用）。
    // MIGRATOR と同じ migrations/ ディレクトリを参照する（main.rs から見た相対パス）。
    const USAGE_METERING_SQL: &str = include_str!("../../../migrations/0007_usage_metering.sql");

    // 0008_m6.sql のソースも同様にコンパイル時埋め込み（DB-free 文字列不変条件検査用）。
    const M6_SQL: &str = include_str!("../../../migrations/0008_m6.sql");

    // 0009_m7a_traffic_split.sql も同様。M7 の migration は**サブマイルストンごとに別ファイル**
    // （0009/0010/0011）にするため、const も 1 ファイル 1 include_str! に分ける。統合ファイルに
    // すると m7a_creates_no_new_table（0009 は新表を作らない）が M7b/M7c の CREATE TABLE で必ず落ちる。
    const M7A_SQL: &str = include_str!("../../../migrations/0009_m7a_traffic_split.sql");
    const M7B_SQL: &str = include_str!("../../../migrations/0010_m7b_function_configs.sql");
    const M7C_SQL: &str = include_str!("../../../migrations/0011_m7c_secrets.sql");

    /// 連続空白を 1 個に潰す（DDL の桁揃えに依存しない部分文字列照合のため）。
    fn squeeze_spaces(sql: &str) -> String {
        let mut s = String::with_capacity(sql.len());
        let mut prev_space = false;
        for c in sql.chars() {
            if c == ' ' || c == '\t' {
                if !prev_space {
                    s.push(' ');
                }
                prev_space = true;
            } else {
                s.push(c);
                prev_space = false;
            }
        }
        s
    }

    /// 新表が ENABLE + FORCE RLS されていること（0007/0008 と同型の DDL 不変条件）。
    fn assert_force_rls(sql: &str, tables: &[&str]) {
        let normalized = squeeze_spaces(sql);
        for table in tables {
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY")),
                "{table} must FORCE ROW LEVEL SECURITY"
            );
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY")),
                "{table} must ENABLE ROW LEVEL SECURITY"
            );
        }
    }

    /// tenant_isolation が fail-closed（`current_setting('app.tenant_id')` の第 2 引数なし）。
    fn assert_fail_closed_isolation(sql: &str, tables: &[&str]) {
        assert!(
            sql.contains("current_setting('app.tenant_id')"),
            "tenant_isolation must gate on current_setting('app.tenant_id')"
        );
        assert!(
            !sql.contains("current_setting('app.tenant_id', true)")
                && !sql.contains("current_setting('app.tenant_id', TRUE)"),
            "tenant_isolation must NOT use the second-arg fallback (must fail closed)"
        );
        for table in tables {
            assert!(
                sql.contains(&format!("CREATE POLICY tenant_isolation ON {table}")),
                "{table} must have a tenant_isolation policy"
            );
        }
    }

    /// 新表に明示 GRANT / REVOKE があること。0004_rls.sql の GRANT はテーブル名の列挙なので
    /// 新表を含まない。書き忘れると RLS 以前に権限エラーで faas_app から一切触れなくなる
    /// （新表追加時の最頻の退行）ため CI で固定する。
    fn assert_explicit_grants(sql: &str, tables: &[&str]) {
        let normalized = squeeze_spaces(sql);
        for table in tables {
            assert!(
                normalized.contains(&format!("REVOKE ALL ON {table} FROM PUBLIC")),
                "{table} must REVOKE ALL FROM PUBLIC"
            );
            assert!(
                normalized.contains(&format!("ON {table} TO faas_app")),
                "{table} must GRANT explicitly to faas_app"
            );
        }
    }

    // ---- 0010_m7b_function_configs.sql の DB-free 文字列不変条件 --------------

    #[test]
    fn migrator_includes_version_10() {
        let v10 = MIGRATOR
            .iter()
            .find(|m| m.version == 10)
            .expect("migration version 10 (0010_m7b_function_configs) must be collected");
        assert!(
            v10.description.contains("m7b") || v10.description.contains("function"),
            "unexpected 0010 description: {}",
            v10.description
        );
    }

    #[test]
    fn version_10_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 10),
            "0010 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    #[test]
    fn m7b_tables_are_force_rls() {
        assert_force_rls(M7B_SQL, &["function_configs"]);
    }

    #[test]
    fn m7b_tenant_isolation_is_fail_closed() {
        assert_fail_closed_isolation(M7B_SQL, &["function_configs"]);
    }

    #[test]
    fn m7b_tables_have_explicit_grants() {
        assert_explicit_grants(M7B_SQL, &["function_configs"]);
    }

    /// component 参照 FK は **2 列の複合 FK** であること。
    ///
    /// 単一列 FK（`components(id)`）はテナント一致を強制しない。RLS の WITH CHECK は
    /// 「自分の tenant_id を書くこと」しか要求しないため、テナント A が
    /// 「tenant_id=A, component_id=（B の cmp_*）」という行を作れてしまう。
    #[test]
    fn m7b_foreign_keys_are_composite() {
        let normalized = squeeze_spaces(M7B_SQL);
        assert!(
            normalized.contains(
                "FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)"
            ),
            "the component reference must be a composite FK so the DB enforces tenant match"
        );
    }

    // ---- 0011_m7c_secrets.sql の DB-free 文字列不変条件 ----------------------

    #[test]
    fn migrator_includes_version_11() {
        let v11 = MIGRATOR
            .iter()
            .find(|m| m.version == 11)
            .expect("migration version 11 (0011_m7c_secrets) must be collected");
        assert!(
            v11.description.contains("m7c") || v11.description.contains("secrets"),
            "unexpected 0011 description: {}",
            v11.description
        );
    }

    #[test]
    fn version_11_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 11),
            "0011 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    #[test]
    fn m7c_tables_are_force_rls() {
        assert_force_rls(M7C_SQL, &["function_secrets", "function_secret_versions"]);
    }

    #[test]
    fn m7c_tenant_isolation_is_fail_closed() {
        assert_fail_closed_isolation(M7C_SQL, &["function_secrets", "function_secret_versions"]);
    }

    #[test]
    fn m7c_tables_have_explicit_grants() {
        assert_explicit_grants(M7C_SQL, &["function_secrets", "function_secret_versions"]);
    }

    /// 版台帳は **追記専用**（暗号文の改竄・消去を faas_app から不可能にする）。
    #[test]
    fn m7c_secret_versions_is_append_only_for_faas_app() {
        let normalized = squeeze_spaces(M7C_SQL);
        assert!(
            normalized.contains("GRANT SELECT, INSERT ON function_secret_versions TO faas_app"),
            "the version ledger must be granted only SELECT/INSERT"
        );
        assert!(
            normalized.contains("REVOKE UPDATE, DELETE ON function_secret_versions FROM faas_app"),
            "UPDATE/DELETE must be revoked from faas_app on the version ledger"
        );
        for forbidden in [
            "GRANT SELECT, INSERT, UPDATE ON function_secret_versions",
            "GRANT SELECT, INSERT, UPDATE, DELETE ON function_secret_versions",
            "GRANT ALL ON function_secret_versions",
        ] {
            assert!(
                !normalized.contains(forbidden),
                "the version ledger must never be granted {forbidden}"
            );
        }
    }

    /// name の一意性は **生存行のみ**（部分 UNIQUE index）。
    ///
    /// テーブル制約にすると soft delete 後に同名で作り直せず 23505 になる。
    /// 「侵害された資格情報を削除して同名で入れ直す」はインシデント対応の最も基本の操作であり、
    /// これを不可能にしてはならない。
    #[test]
    fn m7c_secret_name_uniqueness_is_soft_delete_aware() {
        let normalized = squeeze_spaces(M7C_SQL);
        assert!(
            normalized.contains(
                "CREATE UNIQUE INDEX IF NOT EXISTS uq_function_secrets_live_name \
                 ON function_secrets (tenant_id, component_id, name) WHERE deleted_at IS NULL"
            ) || (normalized.contains("uq_function_secrets_live_name")
                && normalized.contains("WHERE deleted_at IS NULL")),
            "secret name uniqueness must be a partial index over live rows only"
        );
        // コメント行を除いて判定する（本ファイルの設計メモが「テーブル制約にしない理由」を
        // 説明するために同じ字面を含むため）。
        let code_only: String = M7C_SQL
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !squeeze_spaces(&code_only).contains("UNIQUE (tenant_id, component_id, name)"),
            "a table-level UNIQUE would make same-name re-creation after deletion impossible"
        );
    }

    /// HTTP から呼ぶ SECURITY DEFINER 関数は **必ずテナント引数を取る**。
    ///
    /// 本リポジトリの admin は**テナント管理者**であってプラットフォーム管理者ではない。
    /// 全テナント版は `_all` 接尾辞のものだけ（背景ジョブ専用）。
    #[test]
    fn m7c_definer_functions_are_tenant_scoped() {
        assert!(
            M7C_SQL.contains("CREATE FUNCTION secrets_stale_kek(p_tenant text, p_active_kid text)"),
            "the HTTP-facing rekey helper must take a tenant argument"
        );
        assert!(
            M7C_SQL.contains("s.tenant_id = p_tenant"),
            "secrets_stale_kek must filter by the supplied tenant"
        );
        // 全テナント版は _all 接尾辞のものだけ。
        for f in ["secrets_stale_kek_all", "secrets_kek_kid_counts_all"] {
            assert!(
                M7C_SQL.contains(&format!("CREATE FUNCTION {f}(")),
                "{f} must exist as the explicitly-named cross-tenant variant"
            );
        }
        // SECURITY DEFINER 関数はすべて PUBLIC から EXECUTE を剥奪する。
        for f in [
            "secrets_stale_kek(text, text)",
            "secrets_stale_kek_all(text)",
            "secrets_kek_kid_counts_all()",
        ] {
            assert!(
                M7C_SQL.contains(&format!("REVOKE EXECUTE ON FUNCTION {f} FROM PUBLIC")),
                "{f} EXECUTE must be revoked from PUBLIC"
            );
        }
        assert!(
            M7C_SQL.contains("SECURITY DEFINER"),
            "the cross-tenant helpers must be SECURITY DEFINER (faas_app is under FORCE RLS)"
        );
    }

    /// secret 側の FK も 2 列の複合 FK であること（0010 と同じ理由）。
    #[test]
    fn m7c_foreign_keys_are_composite() {
        let normalized = squeeze_spaces(M7C_SQL);
        assert!(
            normalized.contains(
                "FOREIGN KEY (tenant_id, component_id) REFERENCES components (tenant_id, id)"
            ),
            "function_secrets must reference components with a composite FK"
        );
        assert!(
            normalized.contains(
                "FOREIGN KEY (tenant_id, secret_id) REFERENCES function_secrets (tenant_id, id)"
            ),
            "the version ledger must reference function_secrets with a composite FK"
        );
    }

    /// BIGSERIAL を使わない（0004 の ALL SEQUENCES GRANT が新シーケンスに効かないため、
    /// GRANT 漏れという退行を構造的に避ける）。
    #[test]
    fn m7c_uses_no_sequences() {
        for forbidden in ["BIGSERIAL", "SERIAL", "GENERATED"] {
            assert!(
                !M7C_SQL
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("--"))
                    .any(|l| l.to_ascii_uppercase().contains(forbidden)),
                "0011 must not introduce a sequence ({forbidden}); composite PKs avoid GRANT drift"
            );
        }
    }

    // MIGRATOR が 0007 を**コンパイル時収集**していること（sqlx::migrate! の埋め込み検証）。
    // live DB を要さない（iter() は埋め込み済みメタデータを走査するだけ）。
    #[test]
    fn migrator_includes_version_7() {
        let v7 = MIGRATOR
            .iter()
            .find(|m| m.version == 7)
            .expect("migration version 7 (0007_usage_metering) must be collected by MIGRATOR");
        // ファイル名由来の description（usage_metering）が拾えていること。
        assert!(
            v7.description.contains("usage") || v7.description.contains("metering"),
            "unexpected 0007 description: {}",
            v7.description
        );
    }

    // 0007 を BASELINE_VERSIONS に入れていないこと（= MIGRATOR.run が通常適用する。
    // baseline は手動適用済み 0001/0002 のみに限定し続ける）。
    #[test]
    fn version_7_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 7),
            "0007 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    // usage_rollups の DDL 不変条件（DB-free 文字列検査）: FORCE RLS + fail-closed tenant_isolation。
    #[test]
    fn usage_rollups_is_force_rls_and_fail_closed() {
        assert!(
            USAGE_METERING_SQL.contains("usage_rollups FORCE  ROW LEVEL SECURITY")
                || USAGE_METERING_SQL.contains("usage_rollups FORCE ROW LEVEL SECURITY"),
            "usage_rollups must FORCE ROW LEVEL SECURITY"
        );
        assert!(
            USAGE_METERING_SQL.contains("current_setting('app.tenant_id')"),
            "tenant_isolation must gate on current_setting('app.tenant_id')"
        );
        // fail-closed: 第 2 引数フォールバック（', true)' / ', TRUE)'）を持たないこと（未設定 GUC は ERROR）。
        assert!(
            !USAGE_METERING_SQL.contains("current_setting('app.tenant_id', true)")
                && !USAGE_METERING_SQL.contains("current_setting('app.tenant_id', TRUE)"),
            "tenant_isolation must NOT use the second-arg fallback (must fail closed)"
        );
    }

    // faas_app に DELETE を付与しないこと（集計の改竄/消去を不可にする）。
    #[test]
    fn usage_rollups_does_not_grant_delete_to_faas_app() {
        // GRANT 句は SELECT/INSERT/UPDATE のみ（DELETE を含まない）。
        assert!(
            USAGE_METERING_SQL
                .contains("GRANT  SELECT, INSERT, UPDATE ON usage_rollups TO   faas_app;")
                || USAGE_METERING_SQL
                    .contains("GRANT SELECT, INSERT, UPDATE ON usage_rollups TO faas_app;"),
            "faas_app must be granted only SELECT/INSERT/UPDATE on usage_rollups"
        );
        // DELETE は防御的に REVOKE され、GRANT ... DELETE ... は存在しないこと。
        assert!(
            USAGE_METERING_SQL.contains("REVOKE DELETE")
                && !USAGE_METERING_SQL.contains("GRANT  SELECT, INSERT, UPDATE, DELETE")
                && !USAGE_METERING_SQL.contains("GRANT SELECT, INSERT, UPDATE, DELETE"),
            "DELETE must not be granted to faas_app on usage_rollups"
        );
    }

    // ---- 0008_m6.sql の DB-free 文字列不変条件 -------------------------------

    // MIGRATOR が 0008 を**コンパイル時収集**していること（sqlx::migrate! の埋め込み検証）。
    #[test]
    fn migrator_includes_version_8() {
        let v8 = MIGRATOR
            .iter()
            .find(|m| m.version == 8)
            .expect("migration version 8 (0008_m6) must be collected by MIGRATOR");
        assert!(
            v8.description.contains("m6"),
            "unexpected 0008 description: {}",
            v8.description
        );
    }

    // 0008 を BASELINE_VERSIONS に入れていないこと（= MIGRATOR.run が通常適用する）。
    #[test]
    fn version_8_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 8),
            "0008 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    // 0008 の 3 表が ENABLE + FORCE RLS であること（0007 と同型の DDL 不変条件）。
    // 桁揃え（空白数）に依存しないよう、行を正規化して `ALTER TABLE {table} ... ROW LEVEL SECURITY` を探す。
    #[test]
    fn m6_tables_are_force_rls() {
        // 連続空白を 1 個に潰した正規化版で部分文字列照合する。
        let normalized: String = {
            let mut s = String::with_capacity(M6_SQL.len());
            let mut prev_space = false;
            for c in M6_SQL.chars() {
                if c == ' ' || c == '\t' {
                    if !prev_space {
                        s.push(' ');
                    }
                    prev_space = true;
                } else {
                    s.push(c);
                    prev_space = false;
                }
            }
            s
        };
        for table in ["cron_jobs", "triggers", "trigger_deliveries"] {
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY")),
                "{table} must FORCE ROW LEVEL SECURITY"
            );
            assert!(
                normalized.contains(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY")),
                "{table} must ENABLE ROW LEVEL SECURITY"
            );
        }
    }

    // 0008 の tenant_isolation が fail-closed（current_setting('app.tenant_id') 第 2 引数なし）。
    #[test]
    fn m6_tenant_isolation_is_fail_closed() {
        assert!(
            M6_SQL.contains("current_setting('app.tenant_id')"),
            "tenant_isolation must gate on current_setting('app.tenant_id')"
        );
        assert!(
            !M6_SQL.contains("current_setting('app.tenant_id', true)")
                && !M6_SQL.contains("current_setting('app.tenant_id', TRUE)"),
            "tenant_isolation must NOT use the second-arg fallback (must fail closed)"
        );
        // 3 表それぞれに tenant_isolation ポリシーが定義されていること。
        for table in ["cron_jobs", "triggers", "trigger_deliveries"] {
            assert!(
                M6_SQL.contains(&format!("CREATE POLICY tenant_isolation ON {table}")),
                "{table} must have a tenant_isolation policy"
            );
        }
    }

    #[test]
    fn m6_trigger_deliveries_does_not_grant_update_or_delete() {
        // SELECT/INSERT のみが付与される。
        assert!(
            M6_SQL.contains(
                "GRANT  SELECT, INSERT                 ON trigger_deliveries TO   faas_app;"
            ) || M6_SQL.contains("GRANT SELECT, INSERT ON trigger_deliveries TO faas_app;"),
            "trigger_deliveries must be granted only SELECT/INSERT"
        );
        assert!(
            M6_SQL.contains(
                "REVOKE UPDATE, DELETE                 ON trigger_deliveries FROM faas_app;"
            ) || M6_SQL.contains("REVOKE UPDATE, DELETE ON trigger_deliveries FROM faas_app;"),
            "trigger_deliveries must REVOKE UPDATE, DELETE from faas_app"
        );
        assert!(
            !M6_SQL.contains("DELETE ON trigger_deliveries TO")
                && !M6_SQL.contains("DELETE                 ON trigger_deliveries TO"),
            "DELETE must not be granted to faas_app on trigger_deliveries"
        );
    }

    // executions に chain_depth 列が additive（DEFAULT 0, backfill 不要）で追加されること（M6c 暴走防止）。
    #[test]
    fn m6_adds_executions_chain_depth() {
        assert!(
            M6_SQL.contains("ALTER TABLE executions ADD COLUMN IF NOT EXISTS chain_depth")
                && M6_SQL.contains("INTEGER NOT NULL DEFAULT 0"),
            "executions.chain_depth must be added additively with DEFAULT 0"
        );
    }

    #[test]
    fn m6_cron_and_triggers_grant_crud() {
        for table in ["cron_jobs", "triggers"] {
            assert!(
                M6_SQL.contains(&format!("GRANT  SELECT, INSERT, UPDATE, DELETE ON {table}"))
                    || M6_SQL.contains(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table}")),
                "{table} must be granted SELECT/INSERT/UPDATE/DELETE (CRUD)"
            );
        }
    }

    // PUBLIC から剥奪され faas_app にのみ付与されること（reaper の認証前参照と同型）。
    #[test]
    fn m6_cron_due_function_is_security_definer() {
        assert!(
            M6_SQL.contains("CREATE FUNCTION cron_due_tenant_jobs()"),
            "cron_due_tenant_jobs() must be defined"
        );
        assert!(
            M6_SQL.contains("SECURITY DEFINER"),
            "cron_due_tenant_jobs() must be SECURITY DEFINER (cross-tenant scan under owner)"
        );
        assert!(
            M6_SQL.contains("REVOKE EXECUTE ON FUNCTION cron_due_tenant_jobs() FROM PUBLIC;"),
            "cron_due_tenant_jobs() EXECUTE must be revoked from PUBLIC"
        );
        assert!(
            M6_SQL.contains("GRANT  EXECUTE ON FUNCTION cron_due_tenant_jobs() TO   faas_app;")
                || M6_SQL.contains("GRANT EXECUTE ON FUNCTION cron_due_tenant_jobs() TO faas_app;"),
            "cron_due_tenant_jobs() EXECUTE must be granted to faas_app"
        );
    }

    // ---- 0009_m7a_traffic_split.sql の DB-free 文字列不変条件 -----------------

    // MIGRATOR が 0009 を**コンパイル時収集**していること（sqlx::migrate! の埋め込み検証）。
    #[test]
    fn migrator_includes_version_9() {
        let v9 = MIGRATOR
            .iter()
            .find(|m| m.version == 9)
            .expect("migration version 9 (0009_m7a_traffic_split) must be collected by MIGRATOR");
        assert!(
            v9.description.contains("m7a") || v9.description.contains("traffic"),
            "unexpected 0009 description: {}",
            v9.description
        );
    }

    // 0009 を BASELINE_VERSIONS に入れていないこと（= MIGRATOR.run が通常適用する）。
    #[test]
    fn version_9_is_not_baselined() {
        assert!(
            !super::BASELINE_VERSIONS.iter().any(|(v, _)| *v == 9),
            "0009 must not be baselined; MIGRATOR.run applies it normally"
        );
    }

    // M7a は**新規テーブルを 1 つも作らない**（GRANT 漏れ / RLS ポリシー漏れという最大の退行リスクを
    // 設計段階で消したことの回帰ガード）。新表を足したくなったら 0010 以降で作り、RLS + GRANT を
    // 明示すること。
    #[test]
    fn m7a_creates_no_new_table() {
        assert!(
            !M7A_SQL.contains("CREATE TABLE"),
            "0009 must not create any table (canary state lives on components; \
             new tables belong in 0010+ with explicit RLS and GRANT)"
        );
    }

    // 同じ理由で、権限 / ポリシー DDL も 0009 には現れない（既存 components / executions の
    // FORCE RLS + tenant_isolation + GRANT をそのまま継承する）。
    #[test]
    fn m7a_grants_nothing_new() {
        for forbidden in ["GRANT", "REVOKE", "CREATE POLICY", "ROW LEVEL SECURITY"] {
            assert!(
                !M7A_SQL
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("--"))
                    .any(|l| l.contains(forbidden)),
                "0009 must not contain {forbidden} outside comments \
                 (it adds columns to already-protected tables)"
            );
        }
    }

    #[test]
    fn m7a_columns_are_additive() {
        for col in [
            "canary_version_id",
            "canary_weight",
            "previous_active_version_id",
            "canary_updated_at",
        ] {
            assert!(
                M7A_SQL.contains(&format!("ADD COLUMN IF NOT EXISTS {col}")),
                "components.{col} must be added with ADD COLUMN IF NOT EXISTS"
            );
        }
        assert!(
            M7A_SQL.contains("ADD COLUMN IF NOT EXISTS canary_weight SMALLINT NOT NULL DEFAULT 0"),
            "canary_weight must default to 0 so that an un-configured component routes 100% stable"
        );
        // M7b/M7c の複合 FK の被参照側。
        assert!(
            M7A_SQL.contains("components_tenant_id_id_key UNIQUE (tenant_id, id)"),
            "components must expose UNIQUE (tenant_id, id) for the composite FKs in 0010/0011"
        );
    }

    // 「配分先の無い重み」と値域外を DB で不可能にする 2 本の CHECK。
    #[test]
    fn m7a_canary_weight_is_range_checked() {
        assert!(
            M7A_SQL.contains("CHECK (canary_weight >= 0 AND canary_weight <= 100)"),
            "canary_weight must be range-checked in the DB (0..=100)"
        );
        assert!(
            M7A_SQL.contains("CHECK (canary_weight = 0 OR canary_version_id IS NOT NULL)"),
            "a non-zero weight must be impossible without a canary target"
        );
    }

    // executions は最大テーブル。ADD COLUMN のインライン CHECK（既存全行の検証走査を誘発する）を
    // 書かず、値域は NOT VALID 制約で前方だけ守る（ロック窓の最小化。VALIDATE は保守窓で手動）。
    #[test]
    fn m7a_executions_check_is_not_valid() {
        assert!(
            M7A_SQL
                .contains("ADD COLUMN IF NOT EXISTS routing_reason TEXT NOT NULL DEFAULT 'stable'"),
            "executions.routing_reason must be additive with DEFAULT 'stable'"
        );
        assert!(
            M7A_SQL.contains("CHECK (routing_reason IN ('stable', 'canary')) NOT VALID"),
            "the routing_reason CHECK must be added NOT VALID (no full-table verification scan)"
        );
        // インライン CHECK（ADD COLUMN ... CHECK ...）になっていないこと。
        let add_column_line = M7A_SQL
            .lines()
            .find(|l| l.contains("ADD COLUMN IF NOT EXISTS routing_reason"))
            .expect("routing_reason ADD COLUMN line must exist");
        assert!(
            !add_column_line.contains("CHECK"),
            "routing_reason must not carry an inline CHECK (it would scan every existing row)"
        );
    }
}
