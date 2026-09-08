//! Tenant-scoped execution claims and version configuration. Secrets are redeemed through the Control Plane.
use anyhow::Context as _;
use hibana_shared::ResourceLimits;
use sqlx::{Connection as _, Executor as _, PgPool};
use tracing::info;

const TENANT_SQL: &str = "SELECT set_config('app.tenant_id', $1, true)";
const CLAIM_SQL: &str =
    "UPDATE executions SET status='running', started_at=COALESCE(started_at,now()) \
     WHERE tenant_id=$1 AND id=$2 AND status='pending' AND http_request";
const RESOLVE_SQL: &str =
    "SELECT c.id AS component_id, cv.id AS version_id, cv.resource_limits AS resource_limits, \
            cv.capabilities AS capabilities \
     FROM executions e \
     JOIN component_versions cv ON cv.id = e.version_id \
       AND cv.tenant_id = e.tenant_id AND cv.component_id = e.component_id \
     JOIN components c ON c.id = e.component_id AND c.tenant_id = e.tenant_id \
     WHERE e.tenant_id = $1 AND e.id = $2 AND cv.wasm_sha256 = $3 \
       AND e.status = 'pending' AND e.http_request \
       AND c.deleted_at IS NULL AND cv.deleted_at IS NULL";
const CONFIG_SQL: &str = "SELECT key, value FROM version_configs \
      WHERE tenant_id = $1 AND component_id = $2 AND version_id = $3 ORDER BY key";

/// Populate SQLx's per-connection statement metadata before the connection is
/// offered to requests, including replacement connections. Parse/describe only:
/// never set a tenant, read tenant data, or claim a real execution here.
pub(crate) async fn prepare_connection(
    connection: &mut sqlx::PgConnection,
) -> Result<(), sqlx::Error> {
    for sql in [TENANT_SQL, RESOLVE_SQL, CONFIG_SQL, CLAIM_SQL] {
        connection.prepare(sql).await?;
    }
    Ok(())
}
pub(crate) async fn assert_non_privileged_runtime_role(pool: &PgPool) -> anyhow::Result<()> {
    let (rolname, rolsuper, rolbypassrls): (String, bool, bool) = sqlx::query_as(
        "SELECT rolname, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .context("probing runtime DB role privileges")?;

    if rolsuper || rolbypassrls {
        anyhow::bail!(
            "worker DATABASE_URL connects as privileged role '{rolname}' \
             (rolsuper={rolsuper}, rolbypassrls={rolbypassrls}); RLS would be bypassed. \
             Point DATABASE_URL at the non-privileged 'faas_app' role."
        );
    }
    info!(role = %rolname, "runtime DB role is non-privileged (RLS enforced)");
    Ok(())
}

/// version 解決の結果（M7b: limits に加え env 許可リストと平文 config を同じ tx で引く）。
#[derive(Debug, Default)]
pub(crate) struct ResolvedVersion {
    pub(crate) limits: ResourceLimits,
    /// `component_versions.capabilities.env`（admin 承認済みの注入可能 env 名）。
    /// 行が引けない / 壊れている場合は空 ＝ **deny-all**（fail-closed）。
    pub(crate) allowed_env: std::collections::BTreeSet<String>,
    /// M9c: `component_versions.capabilities.net_allow_outbound`（admin 承認済みの outbound 先 host:port）。
    /// 空 = egress deny-all（fail-closed）。worker はこれを解決して `socket_addr_check` を組む。
    pub(crate) allow_outbound: Vec<hibana_shared::egress::EgressEndpoint>,
    /// `version_configs` の版ごとの平文キー・値。
    pub(crate) config: std::collections::BTreeMap<String, String>,
}

/// `component_versions.capabilities` から env 許可リストを読む（M7b, §4.4）。
///
/// CP 側 `validation::parse_capabilities` と**同じ規則**（後方互換 + fail-closed）を worker 側にも
/// 持つ。crate をまたぐので実装は複製になるが、どちらも「配列は旧形式で env 空 / 壊れた値は
/// deny-all」という 1 行の規則であり、テストで両側に固定する。
fn parse_allowed_env(capabilities: &serde_json::Value) -> std::collections::BTreeSet<String> {
    capabilities
        .as_object()
        .and_then(|m| m.get("env"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter(|k| hibana_shared::is_valid_env_key(k))
                .map(str::to_string)
                .collect()
        })
        // 旧形式（素の配列）/ null / 壊れた値 → deny-all。
        .unwrap_or_default()
}

/// M9c (§4.4): `component_versions.capabilities.net_allow_outbound` から egress allowlist を読む。
///
/// CP 側 `validation::parse_capabilities` と同じく **パースできる host:port だけ** を採り、
/// 壊れた値は落とす（fail-closed）。空 = egress deny-all。
fn parse_allow_outbound(
    capabilities: &serde_json::Value,
) -> Vec<hibana_shared::egress::EgressEndpoint> {
    capabilities
        .as_object()
        .and_then(|m| m.get("net_allow_outbound"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter_map(|s| hibana_shared::egress::parse_egress_endpoint(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) struct ExecutionRepository {
    pool: PgPool,
}
impl ExecutionRepository {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    async fn set_tenant_guc(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(TENANT_SQL)
            .bind(tenant_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub(crate) async fn mark_running(
        &self,
        tenant_id: &str,
        execution_id: &str,
    ) -> anyhow::Result<bool> {
        let started = std::time::Instant::now();
        let connections_before = self.pool.size();
        let idle_before = self.pool.num_idle();
        let mut connection = self.pool.acquire().await?;
        let acquired = std::time::Instant::now();
        let mut tx = connection.begin().await?;
        let begun = std::time::Instant::now();
        Self::set_tenant_guc(&mut tx, tenant_id).await?;
        let scoped = std::time::Instant::now();
        let updated = sqlx::query(CLAIM_SQL)
            .bind(tenant_id)
            .bind(execution_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let updated_at = std::time::Instant::now();
        tx.commit().await?;
        tracing::debug!(target: "hibana_latency", %execution_id, stage = "worker_claim_db",
            connections_before, idle_before, connections_after = self.pool.size(),
            acquire_us = acquired.duration_since(started).as_micros() as u64,
            begin_us = begun.duration_since(acquired).as_micros() as u64,
            tenant_us = scoped.duration_since(begun).as_micros() as u64,
            query_us = updated_at.duration_since(scoped).as_micros() as u64,
            commit_us = updated_at.elapsed().as_micros() as u64, "HTTP phase timing");
        Ok(updated == 1)
    }

    /// Resolve the immutable version pinned by the authenticated execution, never
    /// reusable component names or caller-provided version labels. Bind the artifact
    /// digest too so a mismatched redeemed payload cannot inherit other grants.
    /// executionが固定した版のresource_limits / capabilities.envとversion_configsを
    /// **同一 tx** で解決する（M7b, §3.4）。
    ///
    /// 平文 config は worker が DB を直読みする（HTTP 往復ゼロ）。既に worker は
    /// `assert_non_privileged_runtime_role` で `faas_app`（NOBYPASSRLS）であることを起動時に
    /// アサートしており、読み取りは `set_tenant_guc` 済み tx + `WHERE tenant_id = $1` の
    /// 二重防御下で行う（既存 `resolve_limits` と同じ作法）。
    ///
    /// **secret はここでは読まない**。secret は暗号化されており、worker は KEK を持たない
    /// （keyless by design, §3.3）。secret の注入は CP の内部エンドポイントとの引き換えで行う。
    pub(crate) async fn resolve_execution(
        &self,
        tenant_id: &str,
        execution_id: &str,
        wasm_sha256: &str,
    ) -> anyhow::Result<ResolvedVersion> {
        use sqlx::Row as _;

        // M3c: tenant の権威は CP-signed claim。job.tenant_id はその claim と一致する。
        //   RLS 読み取りは tenant tx の下で行う（GUC/tx は撤去しない）。
        let mut tx = self.pool.begin().await?;
        Self::set_tenant_guc(&mut tx, tenant_id).await?;
        let row = sqlx::query(RESOLVE_SQL)
            .bind(tenant_id)
            .bind(execution_id)
            .bind(wasm_sha256)
            .fetch_optional(&mut *tx)
            .await?;

        let Some(r) = row else {
            tx.commit().await?;
            // 承認情報がなければ、既定値で続行せず実行を拒否する。
            anyhow::bail!("version configuration missing");
        };

        let component_id: String = r.try_get("component_id")?;
        let version_id: String = r.try_get("version_id")?;
        let limits: ResourceLimits = serde_json::from_value(r.try_get("resource_limits")?)?;
        let caps_json = r.try_get::<serde_json::Value, _>("capabilities")?;
        let allowed_env = parse_allowed_env(&caps_json);
        let allow_outbound = parse_allow_outbound(&caps_json);

        // 平文 config を同じ tx（同じ GUC）で引く。
        let config_rows = sqlx::query(CONFIG_SQL)
            .bind(tenant_id)
            .bind(&component_id)
            .bind(&version_id)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;

        let mut config = std::collections::BTreeMap::new();
        for row in config_rows {
            config.insert(
                row.try_get::<String, _>("key")?,
                row.try_get::<String, _>("value")?,
            );
        }

        Ok(ResolvedVersion {
            limits,
            allowed_env,
            allow_outbound,
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL; bash scripts/test-http.sh"]
    async fn execution_configuration_survives_reused_names() {
        let url = std::env::var("HTTP_TEST_DATABASE_URL").unwrap();
        assert!(
            url.ends_with("/hibana_http"),
            "requires disposable test database"
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        // Session-local tables shadow real names, including when another test has
        // populated the disposable DB. All assertions execute the production query.
        sqlx::raw_sql(r#"
          CREATE TEMP TABLE components(id text, tenant_id text, name text, deleted_at timestamptz);
          CREATE TEMP TABLE component_versions(id text, tenant_id text, component_id text, version text,
            wasm_sha256 text, resource_limits jsonb, capabilities jsonb, deleted_at timestamptz);
          CREATE TEMP TABLE executions(id text, tenant_id text, component_id text, version_id text, status text, http_request boolean, started_at timestamptz);
          CREATE TEMP TABLE version_configs(tenant_id text, component_id text, version_id text, key text, value text);
          INSERT INTO components VALUES ('old','t','hello',now()), ('new','t','hello',NULL);
          INSERT INTO component_versions VALUES
            ('old-v','t','old','1','old-hash','{"max_memory_bytes":123}',
             '{"env":["OLD_SECRET"],"net_allow_outbound":["old.example:443"]}',NULL),
            ('new-v','t','new','1','new-hash','{}','{}',NULL);
          INSERT INTO executions VALUES ('accepted','t','new','new-v','pending',true,NULL);
          INSERT INTO version_configs VALUES ('t','old','old-v','MESSAGE','old'),('t','new','new-v','MESSAGE','new');
        "#).execute(&pool).await.unwrap();
        {
            let mut connection = pool.acquire().await.unwrap();
            let before: Option<String> =
                sqlx::query_scalar("SELECT current_setting('app.tenant_id', true)")
                    .fetch_one(&mut *connection)
                    .await
                    .unwrap();
            prepare_connection(&mut connection).await.unwrap();
            let after: Option<String> =
                sqlx::query_scalar("SELECT current_setting('app.tenant_id', true)")
                    .fetch_one(&mut *connection)
                    .await
                    .unwrap();
            assert_eq!(before, after, "preparation must not set tenant context");
            let status: String =
                sqlx::query_scalar("SELECT status FROM executions WHERE id='accepted'")
                    .fetch_one(&mut *connection)
                    .await
                    .unwrap();
            assert_eq!(status, "pending", "preparation must not claim executions");
            let cached: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_prepared_statements WHERE statement = ANY($1)",
            )
            .bind(vec![TENANT_SQL, RESOLVE_SQL, CONFIG_SQL, CLAIM_SQL])
            .fetch_one(&mut *connection)
            .await
            .unwrap();
            assert_eq!(cached, 4, "every production query must be prepared");
        }
        let repo = ExecutionRepository::new(pool.clone());
        let resolved = repo
            .resolve_execution("t", "accepted", "new-hash")
            .await
            .unwrap();
        assert_eq!(
            resolved.limits.max_memory_bytes,
            ResourceLimits::default().max_memory_bytes
        );
        assert!(resolved.allowed_env.is_empty());
        assert!(resolved.allow_outbound.is_empty());
        assert_eq!(resolved.config["MESSAGE"], "new");
        // Another deployment of the same app must not change an already accepted execution.
        sqlx::raw_sql(r#"
          INSERT INTO component_versions VALUES ('later-v','t','new','2','later-hash','{}','{}',NULL);
          INSERT INTO version_configs VALUES ('t','new','later-v','MESSAGE','later');
          INSERT INTO executions VALUES ('later','t','new','later-v','pending',true,NULL);
        "#).execute(&pool).await.unwrap();
        assert_eq!(
            repo.resolve_execution("t", "accepted", "new-hash")
                .await
                .unwrap()
                .config["MESSAGE"],
            "new"
        );
        assert_eq!(
            repo.resolve_execution("t", "later", "later-hash")
                .await
                .unwrap()
                .config["MESSAGE"],
            "later"
        );
        assert!(repo
            .resolve_execution("other", "accepted", "new-hash")
            .await
            .is_err());
        assert!(repo
            .resolve_execution("t", "accepted", "old-hash")
            .await
            .is_err());
        assert!(!repo.mark_running("other", "accepted").await.unwrap());
        assert!(repo.mark_running("t", "accepted").await.unwrap());
        assert!(!repo.mark_running("t", "accepted").await.unwrap());
        sqlx::query("UPDATE executions SET status='running'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(repo
            .resolve_execution("t", "accepted", "new-hash")
            .await
            .is_err());
        pool.close().await;
    }
}
