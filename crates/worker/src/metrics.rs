//! Worker メトリクス (M4a, §3.8) — Prometheus exposition + /readyz。
//!
//! Worker は HTTP サーバを持たないため、観測のために最小 axum サーバを 1 本 spawn する
//! （`METRICS_BIND_ADDR`、既定 `0.0.0.0:9090`）。これは JetStream pull ループから完全に独立した
//! tokio task で動かす。pull ループが詰まっても liveness/metrics が応答できるようにするための
//! 分離である（§3.8）。
//!
//! 命名は control-plane 側と整合させ、worker 固有のメトリクスは `wasmtime_` プレフィックスを
//! つける（仕様書 §3.8 の例示に合わせる）。
//!
//! このスライスでは:
//! - `wasmtime_execution_duration_seconds`（histogram, labels: outcome）
//! - `wasmtime_memory_pages`（gauge; 最終実行の Wasm linear memory ページ数）
//! - `wasmtime_component_cache_hits_total` / `..._misses_total`（counter; LRU + cwasm）
//! - `executions_total`（counter, labels: outcome=succeeded|failed|timeout）
//!
//! を登録する。実コードへの計装は最小限（run_component 終了時 / cache hit/miss 経路）。
//!
//! 注: `wasmtime_memory_pages` は本スライスでは observe しない（Wasmtime の Store から実 page を
//! 取り出す経路の追加が後続）。登録だけは残して /metrics に固定で見えるようにする（dashboards 安定性）。
#![allow(dead_code)]

use std::sync::Arc;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

/// p50/p99 を取れる汎用バケット（秒, §3.8）。control-plane と同帯。
fn default_latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ]
}

/// Worker の観測メトリクス一式。
pub struct Metrics {
    pub registry: Registry,
    /// 1 実行の wall-clock duration（秒）。labels: outcome (succeeded/failed/timeout)。
    pub wasmtime_execution_duration_seconds: HistogramVec,
    /// 直近実行の linear memory ページ数（gauge）。1 page = 64KiB。後続で `Store` の現在
    /// メモリサイズから取り出して set する。本スライスでは登録のみ（observe は将来配線）。
    pub wasmtime_memory_pages: IntGauge,
    /// in-memory LRU + cwasm hit 数。labels: tier ("lru" / "cwasm")。
    pub wasmtime_component_cache_hits_total: IntCounterVec,
    /// in-memory LRU miss → ダウンロード + 事前コンパイル発生数。
    pub wasmtime_component_cache_misses_total: IntCounter,
    /// 終端化件数（worker 視点）。labels: outcome (succeeded/failed/timeout)。
    /// CP 側の executions_total と命名は同じだが、こちらは「worker が結果 publish に到達した」
    /// 視点での計上（subscriber の CAS 確定とはタイミングが違う点に注意）。
    pub executions_total: IntCounterVec,
    /// DLQ (`.failed`) に publish した最終配送失敗の累計 (M4c, §6.6)。labels: outcome
    /// (`published` = `.failed` への publish に成功 / `publish_failed` = publish 自体が失敗、
    /// reaper の stuck-execution sweeper に救済を委ねる)。
    pub dlq_published_total: IntCounterVec,
    /// M7c (§5.5): secret を注入した実行で**捨てた**ゲスト stderr のバイト数の累計。
    ///
    /// secret 注入時はゲスト stderr を共有コンテナログへ流さない（inherit_stderr を使わない）。
    /// 内容は一切見ずに捨てるが、「ゲストが何か書いている」ことだけは観測できるようにする
    /// （デバッグの手掛かりを残しつつ、何を書いたかは分からない状態を保つ）。
    pub guest_stderr_dropped_bytes_total: IntCounter,
    /// M8 (§3.7): この worker が現に購読している lane 数。
    ///
    /// lane discovery が収束しているかを見るための gauge。0 のまま張り付いていれば
    /// 「control-plane が lane を作っていない」か「discovery が失敗し続けている」ことが分かる。
    pub subscribed_lanes: IntGauge,
    /// M8 (§4.4): lane 数がこの worker のプロセス容量を超えたため、**今この瞬間は購読していない**
    /// lane の数。
    ///
    /// 0 より大きい状態は「完了条件のスコープ外の縮退 regime」に入ったことを意味する。
    /// メッセージは WorkQueue に残るので喪失はしないが、未提供 lane のテナントは最大
    /// `LANE_ROTATION_SECS` 待たされる。静かに壊れることだけは避けたいので gauge に晒す
    /// （対処は `WORKER_MAX_CONCURRENCY` を上げるか worker 台数を増やすか）。
    pub lanes_unserved: IntGauge,
    /// M8 (§4.3): 現在この worker で実行中（spawn 済み・未完了）のジョブ数。
    ///
    /// `InflightGuard` が inc/dec するので **panic 経路でもリークしない**。
    /// ドレイン（§4.6）はこれが 0 に落ちることを完了条件にするため、リークすると
    /// 「ドレインが必ずタイムアウトする」という形で壊れる。
    pub inflight_executions: IntGauge,
    /// M8 (§6.4): `delivered >= 2`（= JetStream が再配送した）メッセージを引いた回数の累計。
    ///
    /// 二重実行の**直接の観測点**。scale-in のたびに増えるなら、ドレイン（§4.6）が効いていない。
    pub redelivered_total: IntCounter,
    /// M8 (§4.6): ドレイン中に NAK で差し戻したメッセージ数の累計。
    ///
    /// §4.3 の permit 規律により over-fetch しないので、**定常状態ではほぼ 0 のはず**である。
    /// 増え続けるなら「引いたのに走らせられていない」= 規律が破れているサイン。
    pub drain_naked_total: IntCounter,
    /// M8 (§4.6): ドレインのタイムアウトで**待ちきれずに見捨てた** in-flight ジョブ数の累計。
    ///
    /// 見捨てても喪失はしない（JetStream の再配送が保険として効く）が、そのジョブは
    /// ゲストが 2 回実行される。増えているなら `WORKER_DRAIN_TIMEOUT_SECS` が実行時間に
    /// 対して短すぎる。
    pub drain_abandoned_total: IntCounter,
    /// M8 (§5.5): supervisor が注入した slot 番号（`WORKER_SLOT`）。未設定なら -1。
    ///
    /// **同一性の検証にだけ使う**。参照アクチュエータは「起動したはずの slot i の worker が
    /// 本当に `PORT_BASE+i` で応答しているか」を、この gauge の値が i と一致することで確認する。
    /// これが無いと、手動で起動した worker がポートを掴んでいる状況で
    /// 「supervisor は 1 台も起動していないのに live 判定が 1 になる」ことに気づけない。
    pub worker_slot: IntGauge,
}

impl Metrics {
    pub fn init() -> Arc<Self> {
        let registry = Registry::new();

        let wasmtime_execution_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "wasmtime_execution_duration_seconds",
                "Wasm component execution wall-clock duration in seconds",
            )
            .buckets(default_latency_buckets()),
            &["outcome"],
        )
        .expect("metric: wasmtime_execution_duration_seconds");
        registry
            .register(Box::new(wasmtime_execution_duration_seconds.clone()))
            .expect("register wasmtime_execution_duration_seconds");

        let wasmtime_memory_pages = IntGauge::new(
            "wasmtime_memory_pages",
            "Last observed Wasm linear memory size in pages (1 page = 64KiB)",
        )
        .expect("metric: wasmtime_memory_pages");
        registry
            .register(Box::new(wasmtime_memory_pages.clone()))
            .expect("register wasmtime_memory_pages");

        let wasmtime_component_cache_hits_total = IntCounterVec::new(
            Opts::new(
                "wasmtime_component_cache_hits_total",
                "Component cache hits broken down by tier (lru / cwasm)",
            ),
            &["tier"],
        )
        .expect("metric: wasmtime_component_cache_hits_total");
        registry
            .register(Box::new(wasmtime_component_cache_hits_total.clone()))
            .expect("register wasmtime_component_cache_hits_total");

        let wasmtime_component_cache_misses_total = IntCounter::new(
            "wasmtime_component_cache_misses_total",
            "Component cache misses (forced download + precompile)",
        )
        .expect("metric: wasmtime_component_cache_misses_total");
        registry
            .register(Box::new(wasmtime_component_cache_misses_total.clone()))
            .expect("register wasmtime_component_cache_misses_total");

        let executions_total = IntCounterVec::new(
            Opts::new(
                "executions_total",
                "Total executions handled by this worker, by outcome",
            ),
            &["outcome"],
        )
        .expect("metric: executions_total");
        registry
            .register(Box::new(executions_total.clone()))
            .expect("register executions_total");

        let dlq_published_total = IntCounterVec::new(
            Opts::new(
                "dlq_published_total",
                "Final-delivery failures emitted by worker to the .failed (DLQ) subject (M4c)",
            ),
            &["outcome"],
        )
        .expect("metric: dlq_published_total");
        registry
            .register(Box::new(dlq_published_total.clone()))
            .expect("register dlq_published_total");

        let guest_stderr_dropped_bytes_total = IntCounter::new(
            "faas_guest_stderr_dropped_bytes_total",
            "Guest stderr bytes discarded because the execution had secrets injected",
        )
        .expect("metric: guest_stderr_dropped_bytes_total");
        registry
            .register(Box::new(guest_stderr_dropped_bytes_total.clone()))
            .expect("register guest_stderr_dropped_bytes_total");

        let subscribed_lanes = IntGauge::new(
            "wasmtime_subscribed_lanes",
            "Number of JetStream lane consumers this worker is currently pulling from",
        )
        .expect("metric: subscribed_lanes");
        registry
            .register(Box::new(subscribed_lanes.clone()))
            .expect("register subscribed_lanes");

        let lanes_unserved = IntGauge::new(
            "wasmtime_lanes_unserved",
            "Lanes this worker discovered but is not currently pulling from (process capacity exceeded)",
        )
        .expect("metric: lanes_unserved");
        registry
            .register(Box::new(lanes_unserved.clone()))
            .expect("register lanes_unserved");

        let inflight_executions = IntGauge::new(
            "wasmtime_inflight_executions",
            "Executions currently running in this worker process",
        )
        .expect("metric: inflight_executions");
        registry
            .register(Box::new(inflight_executions.clone()))
            .expect("register inflight_executions");

        let redelivered_total = IntCounter::new(
            "wasmtime_redelivered_total",
            "Messages pulled with delivered >= 2 (JetStream redelivery; direct signal of re-execution)",
        )
        .expect("metric: redelivered_total");
        registry
            .register(Box::new(redelivered_total.clone()))
            .expect("register redelivered_total");

        let drain_naked_total = IntCounter::new(
            "wasmtime_drain_naked_total",
            "Messages explicitly NAK'd back to JetStream during graceful drain",
        )
        .expect("metric: drain_naked_total");
        registry
            .register(Box::new(drain_naked_total.clone()))
            .expect("register drain_naked_total");

        let drain_abandoned_total = IntCounter::new(
            "wasmtime_drain_abandoned_total",
            "In-flight executions abandoned because the drain timeout elapsed",
        )
        .expect("metric: drain_abandoned_total");
        registry
            .register(Box::new(drain_abandoned_total.clone()))
            .expect("register drain_abandoned_total");

        let worker_slot = IntGauge::new(
            "wasmtime_worker_slot",
            "Supervisor-assigned slot number for this worker process (-1 when unmanaged)",
        )
        .expect("metric: worker_slot");
        // 未設定は -1。0 を既定にすると「slot 0 として管理されている」と区別がつかない。
        worker_slot.set(
            std::env::var("WORKER_SLOT")
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(-1),
        );
        registry
            .register(Box::new(worker_slot.clone()))
            .expect("register worker_slot");

        Arc::new(Self {
            registry,
            wasmtime_execution_duration_seconds,
            wasmtime_memory_pages,
            wasmtime_component_cache_hits_total,
            wasmtime_component_cache_misses_total,
            executions_total,
            dlq_published_total,
            guest_stderr_dropped_bytes_total,
            subscribed_lanes,
            lanes_unserved,
            inflight_executions,
            redelivered_total,
            drain_naked_total,
            drain_abandoned_total,
            worker_slot,
        })
    }

    /// `/metrics` ハンドラ本体（text/plain Prometheus exposition）。
    pub fn render(&self) -> (axum::http::HeaderMap, String) {
        let metric_families = self.registry.gather();
        let mut buf = Vec::new();
        let encoder = TextEncoder::new();
        if let Err(e) = encoder.encode(&metric_families, &mut buf) {
            tracing::warn!(error = %e, "failed to encode prometheus metrics");
        }
        let body = String::from_utf8(buf).unwrap_or_default();
        let mut headers = axum::http::HeaderMap::new();
        if let Ok(v) = axum::http::HeaderValue::from_str(TextEncoder::new().format_type()) {
            headers.insert(axum::http::header::CONTENT_TYPE, v);
        }
        (headers, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_metrics_init_and_render() {
        let m = Metrics::init();
        m.executions_total.with_label_values(&["succeeded"]).inc();
        m.wasmtime_component_cache_hits_total
            .with_label_values(&["lru"])
            .inc();
        let (_h, body) = m.render();
        assert!(body.contains("executions_total"));
        assert!(body.contains("wasmtime_component_cache_hits_total"));
    }
}
