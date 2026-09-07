//! Tenant-scoped execution claims and version configuration. Secrets are redeemed through the Control Plane.
use anyhow::Context as _;
use faas_shared::ResourceLimits;
use sqlx::PgPool;
use tracing::info;
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
    pub(crate) allow_outbound: Vec<faas_shared::egress::EgressEndpoint>,
    /// `function_configs` の平文キー・値。
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
                .filter(|k| faas_shared::is_valid_env_key(k))
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
) -> Vec<faas_shared::egress::EgressEndpoint> {
    capabilities
        .as_object()
        .and_then(|m| m.get("net_allow_outbound"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .filter_map(|s| faas_shared::egress::parse_egress_endpoint(s).ok())
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
        sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
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
        let mut tx = self.pool.begin().await?;
        Self::set_tenant_guc(&mut tx, tenant_id).await?;
        let updated = sqlx::query(
            "UPDATE executions SET status='running', started_at=COALESCE(started_at,now()) \
             WHERE tenant_id=$1 AND id=$2 AND status='pending' AND http_request",
        )
        .bind(tenant_id)
        .bind(execution_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(updated == 1)
    }

    /// component_versions の resource_limits / capabilities.env と function_configs を
    /// **同一 tx** で解決する（M7b, §3.4）。
    ///
    /// 平文 config は worker が DB を直読みする（HTTP 往復ゼロ）。既に worker は
    /// `assert_non_privileged_runtime_role` で `faas_app`（NOBYPASSRLS）であることを起動時に
    /// アサートしており、読み取りは `set_tenant_guc` 済み tx + `WHERE tenant_id = $1` の
    /// 二重防御下で行う（既存 `resolve_limits` と同じ作法）。
    ///
    /// **secret はここでは読まない**。secret は暗号化されており、worker は KEK を持たない
    /// （keyless by design, §3.3）。secret の注入は CP の内部エンドポイントとの引き換えで行う。
    pub(crate) async fn resolve_version(
        &self,
        tenant_id: &str,
        component: &str,
        version: &str,
    ) -> anyhow::Result<ResolvedVersion> {
        use sqlx::Row as _;

        // M3c: tenant の権威は CP-signed claim。job.tenant_id はその claim と一致する。
        //   RLS 読み取りは tenant tx の下で行う（GUC/tx は撤去しない）。
        let mut tx = self.pool.begin().await?;
        Self::set_tenant_guc(&mut tx, tenant_id).await?;
        let row = sqlx::query(
            "SELECT c.id AS component_id, cv.resource_limits AS resource_limits, \
                    cv.capabilities AS capabilities \
             FROM component_versions cv \
             JOIN components c ON c.id = cv.component_id \
             WHERE c.tenant_id = $1 AND c.name = $2 AND cv.version = $3 \
             LIMIT 1",
        )
        .bind(tenant_id)
        .bind(component)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(r) = row else {
            tx.commit().await?;
            // 承認情報がなければ、既定値で続行せず実行を拒否する。
            anyhow::bail!("version configuration missing");
        };

        let component_id: String = r.try_get("component_id")?;
        let limits: ResourceLimits = serde_json::from_value(r.try_get("resource_limits")?)?;
        let caps_json = r.try_get::<serde_json::Value, _>("capabilities")?;
        let allowed_env = parse_allowed_env(&caps_json);
        let allow_outbound = parse_allow_outbound(&caps_json);

        // 平文 config を同じ tx（同じ GUC）で引く。
        let config_rows = sqlx::query(
            "SELECT key, value FROM function_configs \
              WHERE tenant_id = $1 AND component_id = $2 ORDER BY key",
        )
        .bind(tenant_id)
        .bind(&component_id)
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
