use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
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
