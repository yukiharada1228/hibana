//! 観測メトリクス (M4a, §3.8) — Prometheus exposition。
//!
//! Prometheus の pure-Rust crate（`prometheus`、protobuf feature 無し）を使う。
//! `prometheus::Registry` をプロセスで 1 つ持ち、各メトリクスは起動時に登録する。
//! `GET /metrics` ハンドラは `TextEncoder` で text/plain 形式に書き出して返す。
//!
//! 設計方針:
//! - 命名は `faas_` プレフィックスで namespace を切る（仕様書 §3.8 が要求する標準形式）。
//! - ラベルカーディナリティを抑える: `tenant_id` は **付けない**（テナント数 × ルート数で
//!   爆発する。テナント別観測は別途 audit/log で行う）。`component` のみ付ける execution
//!   メトリクスでも、未知 component は cardinality 攻撃の入口なので呼び出し側で
//!   既知の component_name にだけ載せる（不明時は "unknown" にバケツへ寄せる）。
//! - ヒストグラムは p50/p99 を導出できるバケットを 1ms..30s 帯で固定する（§3.8 が
//!   p50/p99 をハード要件にしているため）。
//!
//! このモジュールは「初期化」と「内部の Registry 取得」だけを公開する。各メトリクスは
//! `static` で持たず、`AppState` 経由で参照させる（初期化順を明示し、テストで差し替え可能に
//! するため）。Worker は別 crate なので、worker 側は worker/src/metrics.rs に同型の実装を置く。
//!
//! 注: 本モジュールは prometheus crate の薄い wrapper である。Prometheus exposition のみを
//! 提供し、OpenTelemetry exporter は持たない（M4a スコープ外）。
//!
//! このスライスではメトリクスを「登録 + render する経路」までを敷設し、実コードへの
//! 計装挿入は必要最小限（invoke カウンタ / executions 終端カウンタ / sweeper 件数）に
//! 留める。後続スライスで HTTP request 単位の duration や reaper の周期ヒストグラムなどを
//! 追加する余地を残す（registry はプロセス共有なので追加登録だけで拡張できる）。
//!
//! 注: 一部のメトリクス（http_requests_total / http_request_duration_seconds /
//! execution_duration_seconds、`observe_http` / `observe_secs` ヘルパ）は本スライスでは
//! 計装点を呼ばない（後続スライスの middleware と finalize-時計算が消費する）。dead-code 警告は
//! モジュール限定で許可する: 登録解除すると Prometheus exposition の安定性が下がる（dashboards
//! が break する）ので、登録した時点で /metrics に固定で見えるようにしておく。
#![allow(dead_code)]

use std::sync::Arc;

use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry, TextEncoder,
};

/// メトリクスの 1 セット（プロセスで 1 個。`AppState` が `Arc` で持ち、ハンドラから参照する）。
///
/// 個々のメトリクスは pub にしてハンドラ側で `.inc()` 等を直接呼ぶ（薄い wrapper を増やさず、
/// prometheus crate の API を素通しさせる）。
pub struct Metrics {
    /// プロセスの Registry。`/metrics` ハンドラがここから `gather()` してエンコードする。
    pub registry: Registry,

    // ---- HTTP（control-plane）------------------------------------------------
    /// HTTP リクエスト総数。labels: method, path, status。
    ///
    /// `path` は route パターン（`/components/{id}` 等）を入れる想定。生 URL を入れると
    /// クライアント側の自由入力でカーディナリティが爆発する。本スライスでは tower-http の
    /// `TraceLayer` の機構を再利用せず、まずは存在だけ登録し、後続スライスで axum middleware
    /// を 1 本足してここを inc / observe する（観測の核は本スライスで /metrics と /readyz を
    /// 通すことに集中する）。
    pub http_requests_total: IntCounterVec,
    /// HTTP リクエスト処理時間ヒストグラム（秒）。labels: method, path。
    pub http_request_duration_seconds: HistogramVec,

    // ---- 実行（control-plane が見える側） ------------------------------------
    /// 終端化された execution の総数。labels: status (succeeded/failed/timeout)。
    /// subscriber.rs の CAS finalize（唯一の終端 writer）でのみ inc する。
    pub executions_total: IntCounterVec,
    /// 実行時間ヒストグラム（秒、created_at→finished_at）。labels: status。
    /// subscriber.rs の CAS finalize 時、行から started_at/finished_at が取れたタイミングで
    /// observe する。本スライスでは初期化のみ（呼び出し点の追加は最小に留める）。
    pub execution_duration_seconds: HistogramVec,
    /// invoke API の呼び出し回数（テナント別）。labels: tenant_id。
    /// テナント数が多い環境では cardinality が増える。許容範囲はオペレーション上の
    /// テナント数で決まる（§3.8 の per-tenant 観測要件を満たすため敢えて付ける。
    /// 多テナント環境では別途集約規則で削るのが運用責任）。
    pub tenant_invoke_total: IntCounterVec,

    // ---- admission / reaper -------------------------------------------------
    /// 429 を返した admission 拒否の総数。labels: kind (rate_limited / concurrency_limit)。
    pub admission_rejections_total: IntCounterVec,
    /// reaper の stuck-execution sweeper が回収（failed 化）した行数の累計。
    pub reaper_swept_total: IntCounter,
    /// reaper の 1 周期で扱った active テナント数（観測直近値）。
    pub reaper_tenants_last: IntGauge,
    /// DLQ subscriber が `.failed` 経路で finalize した execution の累計 (M4c, §6.6)。
    /// labels: outcome (`finalized` = CAS が遷移させた / `stale` = 既終端で no-op /
    /// `dropped` = 検証失敗・行不在等で drop)。`executions_total{status="failed"}` の合算には
    /// この `finalized` 数も含まれる（subscriber と DLQ の双方で同名カウンタを inc するため）。
    pub dlq_finalized_total: IntCounterVec,

    // ---- M7a: canary ルーティング -------------------------------------------
    /// canary ルーティングの選択結果のうち、**実際に JetStream へ publish された**もの (M7a, §15)。
    /// labels: reason (stable / canary)。
    ///
    /// version / tenant はラベルにしない（カーディナリティ）。既存 `faas_executions_total` にも
    /// version ラベルは足さない（既存ダッシュボードを壊さない）。
    ///
    /// **inc の位置**: `enqueue::enqueue_execution` が `Enqueued` を返す直前（publish ack 成功後）
    /// の 1 箇所のみ。解決時点（`resolve_version_for_enqueue`）で数えると、その後段にある
    /// admission の 429・presign 失敗・publish backpressure・cron の冪等ヒットまで canary として
    /// 数えてしまい、`executions.routing_reason` 由来の `GET /traffic` の値と乖離する。
    pub canary_routed_total: IntCounterVec,
}

impl Metrics {
    /// プロセスの Registry とメトリクスを初期化する。`main` で 1 回だけ呼ぶ。
    ///
    /// 失敗（典型は重複登録）は致命なので panic させる: メトリクスは process-wide に 1 つしか
    /// 存在しないことが前提であり、初期化失敗を握って起動を続けると `/metrics` が空のまま
    /// プロセスが立ち上がり、誤った成功判定を招くため fail-fast する。
    pub fn init() -> Arc<Self> {
        let registry = Registry::new();

        let http_requests_total = IntCounterVec::new(
            Opts::new("faas_http_requests_total", "Total HTTP requests received"),
            &["method", "path", "status"],
        )
        .expect("metric: http_requests_total");
        registry
            .register(Box::new(http_requests_total.clone()))
            .expect("register http_requests_total");

        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "faas_http_request_duration_seconds",
                "HTTP request handler duration in seconds",
            )
            .buckets(default_latency_buckets()),
            &["method", "path"],
        )
        .expect("metric: http_request_duration_seconds");
        registry
            .register(Box::new(http_request_duration_seconds.clone()))
            .expect("register http_request_duration_seconds");

        let executions_total = IntCounterVec::new(
            Opts::new(
                "faas_executions_total",
                "Total executions finalized (succeeded/failed/timeout)",
            ),
            &["status"],
        )
        .expect("metric: executions_total");
        registry
            .register(Box::new(executions_total.clone()))
            .expect("register executions_total");

        let execution_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "faas_execution_duration_seconds",
                "End-to-end execution duration (created_at to finished_at) in seconds",
            )
            .buckets(default_latency_buckets()),
            &["status"],
        )
        .expect("metric: execution_duration_seconds");
        registry
            .register(Box::new(execution_duration_seconds.clone()))
            .expect("register execution_duration_seconds");

        let tenant_invoke_total = IntCounterVec::new(
            Opts::new(
                "faas_tenant_invoke_total",
                "POST /invoke calls accepted by tenant",
            ),
            &["tenant_id"],
        )
        .expect("metric: tenant_invoke_total");
        registry
            .register(Box::new(tenant_invoke_total.clone()))
            .expect("register tenant_invoke_total");

        let admission_rejections_total = IntCounterVec::new(
            Opts::new(
                "faas_admission_rejections_total",
                "Admission rejections (429) by reason",
            ),
            &["kind"],
        )
        .expect("metric: admission_rejections_total");
        registry
            .register(Box::new(admission_rejections_total.clone()))
            .expect("register admission_rejections_total");

        let reaper_swept_total = IntCounter::new(
            "faas_reaper_swept_total",
            "Number of stuck pending/running rows finalized to 'failed' by reaper",
        )
        .expect("metric: reaper_swept_total");
        registry
            .register(Box::new(reaper_swept_total.clone()))
            .expect("register reaper_swept_total");

        let reaper_tenants_last = IntGauge::new(
            "faas_reaper_tenants_last",
            "Number of active tenants processed in the most recent reaper pass",
        )
        .expect("metric: reaper_tenants_last");
        registry
            .register(Box::new(reaper_tenants_last.clone()))
            .expect("register reaper_tenants_last");

        let dlq_finalized_total = IntCounterVec::new(
            Opts::new(
                "faas_dlq_finalized_total",
                "Executions handled via the .failed (DLQ) subject by outcome (finalized/stale/dropped)",
            ),
            &["outcome"],
        )
        .expect("metric: dlq_finalized_total");
        registry
            .register(Box::new(dlq_finalized_total.clone()))
            .expect("register dlq_finalized_total");

        let canary_routed_total = IntCounterVec::new(
            Opts::new(
                "faas_canary_routed_total",
                "Enqueued jobs by version routing decision (stable/canary)",
            ),
            &["reason"],
        )
        .expect("metric: canary_routed_total");
        registry
            .register(Box::new(canary_routed_total.clone()))
            .expect("register canary_routed_total");

        Arc::new(Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            executions_total,
            execution_duration_seconds,
            tenant_invoke_total,
            admission_rejections_total,
            reaper_swept_total,
            reaper_tenants_last,
            dlq_finalized_total,
            canary_routed_total,
        })
    }

    /// `/metrics` ハンドラ本体（text/plain Prometheus exposition）。
    ///
    /// `Registry::gather()` は登録された全 collector の現在値を集めて MetricFamily を返す。
    /// `TextEncoder` でそれを Prometheus text 形式に書き出す。`Content-Type` は
    /// `text/plain; version=0.0.4` を付ける（Prometheus 標準）。
    pub fn render(&self) -> (axum::http::HeaderMap, String) {
        let metric_families = self.registry.gather();
        let mut buf = Vec::new();
        let encoder = TextEncoder::new();
        // エンコード失敗は通常ありえない（メモリ書き出し）。失敗時は空ボディで返す（観測は best-effort）。
        if let Err(e) = encoder.encode(&metric_families, &mut buf) {
            tracing::warn!(error = %e, "failed to encode prometheus metrics");
        }
        let body = String::from_utf8(buf).unwrap_or_default();
        let mut headers = axum::http::HeaderMap::new();
        // TextEncoder::format_type() は "text/plain; version=0.0.4" を返す。固定値だが
        // crate API から取ることで将来の改訂に追従する。
        if let Ok(v) = axum::http::HeaderValue::from_str(TextEncoder::new().format_type()) {
            headers.insert(axum::http::header::CONTENT_TYPE, v);
        }
        (headers, body)
    }

    /// duration_seconds histogram の便利メソッド: 秒単位の Histogram を取り出す。
    pub fn observe_http(&self, method: &str, path: &str, status: u16, dur: std::time::Duration) {
        self.http_request_duration_seconds
            .with_label_values(&[method, path])
            .observe(dur.as_secs_f64());
        let status_str = status.to_string();
        self.http_requests_total
            .with_label_values(&[method, path, status_str.as_str()])
            .inc();
    }
}

/// p50/p99 を取れる汎用バケット（秒, §3.8）。
///
/// 1ms..30s 帯を log-ish に並べる。短命処理（admission ゲート）と長尺処理（execution finalize 時の
/// e2e duration）の両方を 1 セットで覆える幅にしてある。`HistogramOpts::buckets` は所有 Vec を要求する。
pub fn default_latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ]
}

/// 任意の Histogram に秒値を observe する小ヘルパ（呼び出し側の単位ミスを避ける）。
pub fn observe_secs(h: &Histogram, dur: std::time::Duration) {
    h.observe(dur.as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// init は重複登録なく成功し、render は Prometheus 形式の text を返す。
    #[test]
    fn init_and_render_round_trip() {
        let m = Metrics::init();
        m.executions_total.with_label_values(&["succeeded"]).inc();
        m.tenant_invoke_total.with_label_values(&["ten_a"]).inc();
        let (_headers, body) = m.render();
        // exposition には HELP / TYPE 行と inc 済みカウンタの値が現れる。
        assert!(body.contains("faas_executions_total"));
        assert!(body.contains("faas_tenant_invoke_total"));
        assert!(body.contains("status=\"succeeded\""));
    }

    /// バケットは要素を持ち単調増加（p50/p99 を取れる体裁）。
    #[test]
    fn latency_buckets_are_monotonic() {
        let b = default_latency_buckets();
        assert!(!b.is_empty());
        assert!(b.windows(2).all(|w| w[0] < w[1]));
    }
}
