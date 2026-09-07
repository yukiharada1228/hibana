//! Application service: claim once, resolve permissions/artifact, execute, then persist the result.
use crate::{
    artifacts::ArtifactCache,
    config::Settings,
    control_plane::ControlPlaneClient,
    env, metrics,
    repository::{self, ExecutionRepository},
    runtime::{build_engine, duration_to_millis, ExecError, Runtime},
};
use anyhow::Context as _;
use faas_shared::{ExecutionStatus, JobMessage, ResultMessage, UsageMetrics};
use std::{sync::Arc, time::Duration};
use tracing::{info, warn};
pub(crate) struct Worker {
    runtime: Runtime,
    repository: ExecutionRepository,
    artifacts: ArtifactCache,
    pub(crate) control_plane: ControlPlaneClient,
    pub(crate) metrics: Arc<metrics::Metrics>,
    pub(crate) execution_slots: Arc<tokio::sync::Semaphore>,
}
impl Worker {
    pub(crate) async fn connect(
        settings: &Settings,
        metrics: Arc<metrics::Metrics>,
    ) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(32)
            .connect(&settings.database_url)
            .await
            .context("failed to connect to Postgres")?;
        repository::assert_non_privileged_runtime_role(&pool).await?;
        let engine = build_engine()?;
        let http = reqwest::Client::builder().build()?;
        let artifacts = ArtifactCache::new(
            engine.clone(),
            http.clone(),
            settings.wasm_cache_dir.clone(),
            metrics.clone(),
        )?;
        let control_plane = ControlPlaneClient::new(
            http,
            settings.control_plane_internal_url.clone(),
            Duration::from_millis(settings.job_env_fetch_timeout_ms),
        );
        Ok(Self {
            runtime: Runtime {
                engine,
                metrics: metrics.clone(),
            },
            repository: ExecutionRepository::new(pool),
            artifacts,
            control_plane,
            metrics,
            execution_slots: Arc::new(tokio::sync::Semaphore::new(
                settings.max_concurrency as usize,
            )),
        })
    }
    pub(crate) async fn handle_http(
        &self,
        job: JobMessage,
        stream: crate::runtime::ResponseSender,
    ) -> bool {
        let execution_id = job.execution_id.clone();
        let tenant_id = job.tenant_id.clone();
        // M4a: span に相関 ID を詰める（JSON ログでは各イベントのトップレベルに展開される）。
        let span = tracing::Span::current();
        span.record("execution_id", execution_id.as_str());
        span.record("tenant_id", tenant_id.as_str());
        span.record("component", job.component.as_str());
        let job_token = job.job_token.clone();
        info!(%execution_id, component = %job.component, "received job");

        let exec_started = std::time::Instant::now();

        match self
            .repository
            .mark_running(&tenant_id, &execution_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                info!(%execution_id, "execution already terminal or absent; acknowledging duplicate");
                return true;
            }
            Err(e) => {
                warn!(%execution_id, error = %e, "cannot check execution state; refusing to run without DB");
                return false;
            }
        }

        let finish = stream.body.clone();
        let outcome = self.execute(&job, stream).await;
        if outcome.is_err() {
            let _ = finish
                .send(Err(std::io::Error::other("Worker execution failed")))
                .await;
        }
        let exec_elapsed = exec_started.elapsed();
        // wall_time_ms は設定解決・ダウンロードを含む handle_http 全体を計測する。
        // 成功経路は run_http が組んだ usage の
        // wall_time_ms をこの値で上書きし、失敗/timeout 経路は wall のみ判る部分計量を組む。
        let wall_time_ms = duration_to_millis(exec_elapsed);

        let (status_label, result) = match outcome {
            Ok((output, mut usage)) => {
                info!(%execution_id, "job succeeded");
                // ランタイム実行前の準備時間も含める。
                usage.wall_time_ms = wall_time_ms;
                (
                    "succeeded",
                    ResultMessage {
                        execution_id: execution_id.clone(),
                        tenant_id: tenant_id.clone(),
                        status: ExecutionStatus::Succeeded,
                        output: Some(
                            serde_json::json!({"status": output.status, "streamed": true}),
                        ),
                        error: None,
                        job_token: job_token.clone(),
                        usage: Some(usage),
                    },
                )
            }
            Err(ExecError::Timeout) => {
                warn!(%execution_id, "job timed out");
                (
                    "timeout",
                    ResultMessage {
                        execution_id: execution_id.clone(),
                        tenant_id: tenant_id.clone(),
                        status: ExecutionStatus::Timeout,
                        output: None,
                        error: Some("execution timed out".to_string()),
                        job_token: job_token.clone(),
                        // M5 (§15): timeout/failed 経路は run_component が Err を返すため fuel/peak/
                        // output は判らない。判る計量（wall_time_ms）のみ載せ、残りは 0（未計測）。
                        usage: Some(UsageMetrics {
                            wall_time_ms,
                            ..UsageMetrics::default()
                        }),
                    },
                )
            }
            Err(ExecError::Failed(msg)) => {
                warn!(%execution_id, "job failed (diagnostic retained in the tenant execution record)");
                (
                    "failed",
                    ResultMessage {
                        execution_id: execution_id.clone(),
                        tenant_id: tenant_id.clone(),
                        status: ExecutionStatus::Failed,
                        output: None,
                        error: Some(msg),
                        job_token: job_token.clone(),
                        // M5 (§15): 同上。失敗経路は wall_time_ms のみ確定。
                        usage: Some(UsageMetrics {
                            wall_time_ms,
                            ..UsageMetrics::default()
                        }),
                    },
                )
            }
        };

        // M4a (§3.8): outcome 別に duration histogram + executions_total を更新する。
        self.metrics
            .wasmtime_execution_duration_seconds
            .with_label_values(&[status_label])
            .observe(exec_elapsed.as_secs_f64());
        self.metrics
            .executions_total
            .with_label_values(&[status_label])
            .inc();

        if let Err(e) = self.control_plane.complete(&result).await {
            warn!(%execution_id, error = %e, "HTTP result persistence failed; request will not be replayed");
            return false;
        }
        true
    }

    /// Component を解決して handle を呼び出す。Timeout / Failed / 出力を返す。
    async fn execute(
        &self,
        job: &JobMessage,
        stream: crate::runtime::ResponseSender,
    ) -> std::result::Result<(crate::runtime::HttpResponseReceipt, UsageMetrics), ExecError> {
        // resource_limits / env 許可リスト / 平文 config を DB から解決する。
        // 取得失敗・未設定時は実行を拒否する。
        let resolved = self
            .repository
            .resolve_version(&job.tenant_id, &job.component, &job.version)
            .await
            .map_err(|_| ExecError::Failed("Version configuration unavailable".into()))?;
        let limits = resolved.limits;

        let secrets = match job.env_token.as_deref() {
            Some(token) => self.control_plane.fetch_job_env(token).await?,
            None => std::collections::BTreeMap::new(),
        };

        // M7b (§4.4): 許可リストで畳んで注入する env を組み立てる。許可リストに無いキーは
        // 値が DB に存在しても注入しない（admin 承認が権威。CP 側の引き換えでも同じ許可リストで
        // 絞っており、この二重防御で片側の実装ミスが即漏洩にならないようにする）。
        let built_env = env::build_env(&resolved.config, &secrets, &resolved.allowed_env);
        if built_env.dropped_unapproved > 0 {
            // 「設定したのに入っていない」の切り分けを可能にする（キー名は出さない）。
            tracing::debug!(
                dropped = built_env.dropped_unapproved,
                "dropped env entries not present in the approved allowlist"
            );
        }

        // §3.6 のキャッシュ階層で Component を解決する。
        let component = self.artifacts.resolve_component(job).await?;

        let request = serde_json::from_value(job.input.clone())
            .map_err(|_| ExecError::Failed("Invalid HTTP request envelope".into()))?;

        let allowed_addrs = self
            .resolve_egress_allowlist(&resolved.allow_outbound)
            .await;

        self.runtime
            .run_http(
                crate::runtime::Invocation {
                    component,
                    request,
                    limits,
                    built_env,
                    allowed_addrs,
                },
                stream,
            )
            .await
    }

    async fn resolve_egress_allowlist(
        &self,
        endpoints: &[faas_shared::egress::EgressEndpoint],
    ) -> std::collections::HashSet<std::net::SocketAddr> {
        use std::collections::HashSet;
        let mut out: HashSet<std::net::SocketAddr> = HashSet::new();
        for ep in endpoints {
            match tokio::net::lookup_host((ep.host.as_str(), ep.port)).await {
                Ok(addrs) => {
                    for addr in addrs {
                        if faas_shared::egress::is_hard_denied(addr.ip()) {
                            tracing::warn!(
                                host = %ep.host, port = ep.port, ip = %addr.ip(),
                                "approved egress host resolved to a hard-denied IP; dropping (possible rebinding)"
                            );
                            continue;
                        }
                        out.insert(addr);
                    }
                }
                Err(e) => {
                    tracing::warn!(host = %ep.host, port = ep.port, error = %e,
                        "could not resolve approved egress host; skipping");
                }
            }
        }
        out
    }
}
pub(crate) struct InflightGuard(Arc<metrics::Metrics>);

impl InflightGuard {
    pub(crate) fn new(m: &Arc<metrics::Metrics>) -> Self {
        m.inflight_executions.inc();
        Self(Arc::clone(m))
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight_executions.dec();
    }
}
