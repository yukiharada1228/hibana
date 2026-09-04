//! M10 (§3.8 / §10): 分散トレーシングの初期化と、サービス境界（NATS）を跨ぐ
//! trace context の伝搬。
//!
//! # opt-in（既定 off = 完全 no-op）
//!
//! OpenTelemetry は **`OTEL_EXPORTER_OTLP_ENDPOINT` が設定されているときだけ**有効になる。
//! 未設定なら OTel layer を一切足さず、[`init_tracing`] は M4a までの fmt/json ログと
//! 1 ビットも変わらない。これが「アップグレードしても既定挙動が変わらない」ことの唯一の条件。
//!
//! # なぜ CP / worker で共有するか
//!
//! 両 bin の `init_tracing` はほぼ同一（既定 EnvFilter 文字列だけ違う）で、OTel を足すと
//! layer 合成・exporter 構築・終了時 flush という非自明な処理が増える。2 箇所に複製すると
//! 片方だけ直す事故になるので、ここへ集約する。

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{global, Context};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, EnvFilter, Layer};

/// プロセス生存中は保持し、終了時に **batch exporter を flush** するためのガード。
///
/// batch 送信なので、drop でフラッシュしないと短命プロセス / テストで span が送られず消える。
/// `main` の最後まで束縛し続けること（`let _guard = init_tracing(...)`）。
#[must_use = "hold this guard until process exit; dropping it flushes pending spans"]
pub struct OtelGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            // best-effort。exporter 先が死んでいても終了処理は止めない。
            let _ = p.shutdown();
        }
    }
}

/// tracing subscriber を初期化する（M4a の fmt/json + M10 の OTel opt-in）。
///
/// - `log_format`: "json" なら JSON 行、それ以外は text（M4a と同一）。
/// - `default_filter`: `RUST_LOG` 等が無いときの既定 EnvFilter（bin ごとに違う）。
/// - `service_name`: OTel の `service.name`（CP / worker を区別する）。
///
/// 戻り値の [`OtelGuard`] は OTel 有効時のみ意味を持つ（無効時は中身 None）。
pub fn init_tracing(
    log_format: &str,
    default_filter: &str,
    service_name: &'static str,
) -> OtelGuard {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    // fmt レイヤ（text / json）。型を揃えるため boxed する。設定は M4a と同一。
    let fmt_layer = if log_format.eq_ignore_ascii_case("json") {
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .boxed()
    } else {
        fmt::layer().boxed()
    };

    // OTel レイヤ（endpoint 設定時のみ）。失敗しても fmt だけで起動を続ける（観測の失敗で
    // 本流を止めない）。
    let (otel_layer, provider) = match otel_endpoint() {
        Some(endpoint) => match build_provider(service_name) {
            Ok(provider) => {
                let tracer = provider.tracer(service_name);
                let layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();
                (Some(layer), Some(provider))
            }
            Err(e) => {
                eprintln!(
                    "otel: failed to init OTLP exporter for {endpoint} ({e}); \
                     continuing with logs only"
                );
                (None, None)
            }
        },
        None => (None, None),
    };

    // Option<Layer> も Layer を実装する（None = 何もしない）。
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    // サービス間伝搬は W3C TraceContext（traceparent）。有効/無効に関わらず伝搬器は入れておく
    // （無効時は current context が空なので inject しても何も乗らない = 無害）。
    global::set_text_map_propagator(TraceContextPropagator::new());

    OtelGuard { provider }
}

/// `OTEL_EXPORTER_OTLP_ENDPOINT` を読む（空白のみ / 未設定は None）。
fn otel_endpoint() -> Option<String> {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// OTLP(HTTP/protobuf) exporter を張った batch tracer provider を構築する。
fn build_provider(
    service_name: &'static str,
) -> Result<SdkTracerProvider, Box<dyn std::error::Error>> {
    // exporter は reqwest **blocking** ベースの HTTP/protobuf（gRPC=tonic を避ける, 設計 §3）。
    // blocking にする理由: SDK 0.32 の BatchSpanProcessor は tokio 外の専用 OS スレッドで送信するため、
    // async reqwest だと「no reactor running」で panic する。blocking client はスレッド上で完結する。
    //
    // **`with_endpoint` は呼ばない**。それを使うと値が verbatim で使われ `/v1/traces` が付かず、
    // ルートに POST して失敗する（opentelemetry-otlp の仕様: プログラム指定 endpoint は無加工）。
    // 代わりに exporter に `OTEL_EXPORTER_OTLP_ENDPOINT`（base URL）を自分で読ませると、
    // OTel 仕様どおり signal path（`/v1/traces`）を付けてくれる。有効/無効の判定は [`otel_endpoint`]。
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()?;

    let resource = Resource::builder().with_service_name(service_name).build();

    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

// ---------------------------------------------------------------------------
// サービス境界を跨ぐ trace context の伝搬（NATS ヘッダ）
// ---------------------------------------------------------------------------

/// `async_nats::HeaderMap` へ W3C traceparent を書き込む [`opentelemetry::propagation::Injector`]。
struct NatsHeaderInjector<'a>(&'a mut async_nats::HeaderMap);

impl opentelemetry::propagation::Injector for NatsHeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        // key はプロパゲータ由来の固定文字列（"traceparent" 等）なので変換は安全。
        self.0.insert(key, value.as_str());
    }
}

/// `async_nats::HeaderMap` から W3C traceparent を読む [`opentelemetry::propagation::Extractor`]。
struct NatsHeaderExtractor<'a>(&'a async_nats::HeaderMap);

impl opentelemetry::propagation::Extractor for NatsHeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|v| v.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.iter().map(|(k, _)| k.as_ref()).collect()
    }
}

/// **現在の span の trace context** を NATS ヘッダへ inject する（CP の publish 側）。
///
/// OTel 無効時は current context が空なので何も書かれない（無害）。`traceparent` を足すだけで
/// 既存ヘッダ（`Nats-Msg-Id` 等）は触らない。
pub fn inject_trace_context(headers: &mut async_nats::HeaderMap) {
    let cx = tracing_current_context();
    global::get_text_map_propagator(|prop| {
        prop.inject_context(&cx, &mut NatsHeaderInjector(headers));
    });
}

/// NATS ヘッダから親 trace context を取り出す（worker の受信側）。
///
/// 取り出したコンテキストは `tracing::Span::set_parent`（`tracing_opentelemetry::OpenTelemetrySpanExt`）
/// へ渡して、worker の実行 span を CP の invoke span の子にする。
pub fn extract_trace_context(headers: &async_nats::HeaderMap) -> Context {
    global::get_text_map_propagator(|prop| prop.extract(&NatsHeaderExtractor(headers)))
}

/// 現在の `tracing` span に紐づく OTel コンテキストを得る。
///
/// `tracing_opentelemetry` は tracing span ↔ OTel span を対応づけるので、
/// `Span::current().context()` で「今の span の OTel context」が取れる。
fn tracing_current_context() -> Context {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    tracing::Span::current().context()
}

/// 与えた `tracing` span の親を、抽出した OTel コンテキストにする（worker の受信側）。
///
/// `OpenTelemetrySpanExt` を呼び出し側の crate に import させずに済むよう、ここに包む。
/// OTel 無効時に空コンテキストを渡しても無害（親無しになるだけ）。
pub fn set_span_parent(span: &tracing::Span, cx: Context) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    // 戻り値は「span が OTel 非対応 subscriber 下」等のときの Err。OTel 無効時は普通に起きるので
    // best-effort で無視する（親付けができないだけで実行に影響しない）。
    let _ = span.set_parent(cx);
}
