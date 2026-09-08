//! Wasmtime execution shared by fleet requests and local development.
use crate::{env, metrics};
mod egress;
mod http;
mod limits;
use anyhow::{anyhow, Context as _};
use egress::gated_send_request;
use hibana_shared::http::HttpRequest;
use hibana_shared::{ResourceLimits, UsageMetrics};
use http::into_request;
pub(crate) use http::HttpResponseReceipt;
pub(crate) use http::{response_channel, ResponseSender};
use limits::MeteredLimits;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::Instant;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    bindings::{http::types::Scheme, ProxyPre},
    WasiHttpView as _,
};
const GUEST_STDERR_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_HOST_RESOURCES: usize = 4096;
pub(crate) struct Runtime {
    pub engine: Engine,
    pub metrics: Arc<metrics::Metrics>,
    _epoch_ticker: EpochTickerGuard,
}

/// Import resolution is immutable and independent of request state. Keep it in
/// the same bounded cache as the compiled code; every invocation still gets a Store.
pub(crate) struct PreparedComponent(wasmtime::component::InstancePre<HostState>);

impl PreparedComponent {
    pub(crate) fn new(component: Component) -> anyhow::Result<Self> {
        let started = Instant::now();
        // On Linux, in-memory deserialization defers writing the memfd-backed
        // initial memory image until the first instance. Finish that host-only
        // work during preparation; do not instantiate or call guest code here.
        component
            .initialize_copy_on_write_image()
            .context("initializing component memory images")?;
        let images_ready = Instant::now();
        let mut linker = Linker::new(component.engine());
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)?;
        let prepared = linker.instantiate_pre(&component)?;
        tracing::debug!(target: "hibana_latency", stage = "component_preparation",
            copy_on_write_us = images_ready.duration_since(started).as_micros() as u64,
            link_us = images_ready.elapsed().as_micros() as u64, "Component preparation timing");
        Ok(Self(prepared))
    }

    #[cfg(test)]
    pub(crate) fn component(&self) -> &Component {
        self.0.component()
    }
}
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: MeteredLimits,
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    allowed_addrs: Arc<std::collections::HashSet<std::net::SocketAddr>>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_http::WasiHttpView for HostState {
    fn ctx(&mut self) -> &mut wasmtime_wasi_http::WasiHttpCtx {
        &mut self.http_ctx
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn send_request(
        &mut self,
        request: hyper::Request<wasmtime_wasi_http::body::HyperOutgoingBody>,
        config: wasmtime_wasi_http::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::HttpResult<wasmtime_wasi_http::types::HostFutureIncomingResponse> {
        let allowed = Arc::clone(&self.allowed_addrs);
        let handle = wasmtime_wasi::runtime::spawn(async move {
            Ok(gated_send_request(request, config, allowed).await)
        });
        Ok(wasmtime_wasi_http::types::HostFutureIncomingResponse::pending(handle))
    }
}

pub(crate) fn install_epoch_deadline<T: Send + 'static>(store: &mut Store<T>, deadline: Instant) {
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        if Instant::now() >= deadline {
            Err(wasmtime::Trap::Interrupt.into())
        } else {
            Ok(wasmtime::UpdateDeadline::Continue(1))
        }
    });
}

pub(crate) fn build_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);
    config.epoch_interruption(true);
    config.consume_fuel(true);
    // Cargo disables the `threads` feature: ResourceLimiter cannot account for
    // shared memories. A regression test requires shared modules to be rejected.
    Engine::new(&config).map_err(|e| anyhow!("Engine::new failed: {e}"))
}

pub(crate) struct Invocation {
    pub component: Arc<PreparedComponent>,
    pub request: HttpRequest,
    pub limits: ResourceLimits,
    pub built_env: env::BuiltEnv,
    pub allowed_addrs: std::collections::HashSet<std::net::SocketAddr>,
}

impl Runtime {
    pub(crate) fn new(engine: Engine, metrics: Arc<metrics::Metrics>) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let tick_engine = engine.clone();
        std::thread::Builder::new()
            .name("hibana-epoch".into())
            .spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    tick_engine.increment_epoch();
                }
            })?;
        Ok(Self {
            engine,
            metrics,
            _epoch_ticker: EpochTickerGuard(stop),
        })
    }

    pub(crate) async fn run_http(
        &self,
        invocation: Invocation,
        stream: ResponseSender,
    ) -> std::result::Result<(HttpResponseReceipt, UsageMetrics), ExecError> {
        let setup_started = Instant::now();
        let Invocation {
            component,
            request,
            limits,
            built_env,
            allowed_addrs,
        } = invocation;
        let peak_memory = Arc::new(AtomicU64::new(0));
        let metered_limits =
            MeteredLimits::new(limits.max_memory_bytes as usize, Arc::clone(&peak_memory));

        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.max_random_size(1024 * 1024);
        wasi_builder
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false);
        let captured_stderr = {
            let pipe = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(GUEST_STDERR_CAPTURE_BYTES);
            wasi_builder.stderr(pipe.clone());
            Some(pipe)
        };
        for (k, v) in &built_env.pairs {
            wasi_builder.env(k, v);
        }

        let allowed_arc = std::sync::Arc::new(allowed_addrs);
        if !allowed_arc.is_empty() {
            let allowed = std::sync::Arc::clone(&allowed_arc);
            wasi_builder.allow_tcp(true);
            wasi_builder.allow_udp(false);
            wasi_builder.allow_ip_name_lookup(true);
            wasi_builder.socket_addr_check(move |addr, use_| {
                let allowed = std::sync::Arc::clone(&allowed);
                Box::pin(async move {
                    if !matches!(use_, wasmtime_wasi::SocketAddrUse::TcpConnect) {
                        return false;
                    }
                    if hibana_shared::egress::is_hard_denied(addr.ip()) {
                        return false;
                    }
                    allowed.contains(&addr)
                })
            });
        }

        let mut table = ResourceTable::new();
        table.set_max_capacity(MAX_HOST_RESOURCES);
        let host = HostState {
            ctx: wasi_builder.build(),
            table,
            limits: metered_limits,
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            allowed_addrs: allowed_arc,
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|state| &mut state.limits);

        install_epoch_deadline(&mut store, Instant::now() + limits.max_wall_time());

        let fuel_to_set = limits.max_fuel.unwrap_or(u64::MAX);
        store
            .set_fuel(fuel_to_set)
            .map_err(|e| ExecError::Failed(format!("failed to set fuel: {e}")))?;

        let wall = limits.max_wall_time();
        let started = Instant::now();

        let streamed_bytes = Arc::clone(&stream.bytes);
        let (req_res, out, receiver) = {
            let mut req = into_request(request)
                .map_err(|e| ExecError::Failed(format!("bad ingress request: {e}")))?;
            req.headers_mut()
                .remove(hyper::header::HeaderName::from_static("x-hibana-env"));
            if !built_env.pairs.is_empty() {
                let map: serde_json::Map<String, serde_json::Value> = built_env
                    .pairs
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                let json = serde_json::to_vec(&serde_json::Value::Object(map))
                    .map_err(|e| ExecError::Failed(format!("env encode: {e}")))?;
                let encoded = hibana_shared::b64url_encode(&json);
                if let Ok(hv) = hyper::header::HeaderValue::from_str(&encoded) {
                    req.headers_mut()
                        .insert(hyper::header::HeaderName::from_static("x-hibana-env"), hv);
                }
            }
            req.headers_mut().remove("x-hibana-event");
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let req_res = store
                .data_mut()
                .new_incoming_request(Scheme::Http, req)
                .map_err(|e| ExecError::Failed(format!("new_incoming_request: {e}")))?;
            let out = store
                .data_mut()
                .new_response_outparam(sender)
                .map_err(|e| ExecError::Failed(format!("new_response_outparam: {e}")))?;
            (req_res, out, receiver)
        };

        let exec_timeout = limits.max_execution_time();
        let setup_us = setup_started.elapsed().as_micros() as u64;
        let exec_future = async {
            let link_started = Instant::now();
            let ppre = ProxyPre::new(component.0.clone())
                .map_err(|e| ExecError::Failed(format!("not a proxy component: {e}")))?;
            let link_us = link_started.elapsed().as_micros() as u64;
            let instantiate_started = Instant::now();
            let proxy = ppre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| ExecError::Failed(format!("failed to instantiate proxy: {e}")))?;
            let instantiate_us = instantiate_started.elapsed().as_micros() as u64;
            let handler_started = Instant::now();

            let call = proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, req_res, out);
            let recv = async {
                match receiver.await {
                    Ok(Ok(resp)) => Some(stream.send(resp).await),
                    _ => None,
                }
            };
            let (call_res, recv_res) = tokio::join!(call, recv);
            tracing::debug!(target: "hibana_latency", stage = "wasm_runtime", setup_us, link_us, instantiate_us,
                handler_us = handler_started.elapsed().as_micros() as u64, "HTTP phase timing");

            let call_result: std::result::Result<HttpResponseReceipt, anyhow::Error> =
                match call_res {
                    Err(e) => Err(e),
                    Ok(()) => match recv_res {
                        Some(Ok(bytes)) => Ok(bytes),
                        Some(Err(e)) => Err(anyhow!("failed to encode response: {e}")),
                        None => Err(anyhow!("guest did not set a response")),
                    },
                };
            Ok::<_, ExecError>(call_result)
        };

        let timed = tokio::time::timeout(exec_timeout, exec_future).await;

        if let Some(pipe) = captured_stderr {
            let dropped = pipe.contents().len() as u64;
            if dropped > 0 {
                self.metrics
                    .guest_stderr_dropped_bytes_total
                    .inc_by(dropped);
            }
        }

        match timed {
            Err(_elapsed) => Err(ExecError::Timeout),
            Ok(Err(e)) => Err(e),
            Ok(Ok(call_result)) => match call_result {
                Ok(output) => {
                    let remaining = store.get_fuel().unwrap_or(fuel_to_set);
                    let cpu_fuel_used =
                        fuel_consumed(fuel_to_set, remaining, limits.max_fuel.is_some());
                    let peak_memory_bytes = peak_memory.load(Ordering::Relaxed);
                    let output_bytes = streamed_bytes.load(Ordering::Relaxed);
                    let wall_time_ms = duration_to_millis(started.elapsed());
                    let usage = UsageMetrics {
                        cpu_fuel_used,
                        wall_time_ms,
                        peak_memory_bytes,
                        output_bytes,
                    };
                    Ok((output, usage))
                }
                Err(trap) => {
                    if is_out_of_fuel_trap(&trap) {
                        Err(ExecError::Failed(format!("fuel exhausted: {trap}")))
                    } else if is_interrupt_trap(&trap) || started.elapsed() >= wall {
                        Err(ExecError::Timeout)
                    } else {
                        Err(ExecError::Failed(format!("wasm trap: {trap}")))
                    }
                }
            },
        }
    }
}
pub(crate) enum ExecError {
    Timeout,
    Failed(String),
}

pub(crate) fn is_interrupt_trap(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::Interrupt)
    )
}

pub(crate) fn is_out_of_fuel_trap(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::OutOfFuel)
    )
}

pub(crate) fn fuel_consumed(set: u64, remaining: u64, fuel_enabled: bool) -> u64 {
    if !fuel_enabled {
        return 0;
    }
    set.saturating_sub(remaining)
}

pub(crate) fn duration_to_millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

// One ticker per runtime, shared by all request Stores. Each Store independently
// checks its absolute deadline; an expired request cannot time out another one.
struct EpochTickerGuard(Arc<AtomicBool>);
impl Drop for EpochTickerGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
