//! Wasmtime execution shared by fleet requests and local development.
use crate::{env, metrics};
mod dns;
mod egress;
pub(crate) use dns::ApprovedEgress;
mod http;
mod http_buffer;
mod http_limits;
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
pub(crate) struct Runtime {
    tcp_policy: crate::network::TcpPolicy,
    http_buffer_budget: Arc<tokio::sync::Semaphore>,
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
        dns::add_to_linker(&mut linker)?;
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
    approved_egress: Arc<ApprovedEgress>,
    http_buffer_budget: http_buffer::Budget,
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
        let allowed = Arc::clone(&self.approved_egress);
        let budget = self.http_buffer_budget.clone();
        let handle = wasmtime_wasi::runtime::spawn(async move {
            Ok(gated_send_request(request, config, allowed, budget).await)
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
            // Tokio's deferred wakeup gives its I/O/timer driver a turn too.
            // Wasmtime's generic self-wake can keep a busy guest at the front
            // of the executor queue across many epoch ticks.
            Ok(wasmtime::UpdateDeadline::YieldCustom(
                1,
                Box::pin(tokio::task::yield_now()),
            ))
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
    pub approved_egress: ApprovedEgress,
    /// Includes environment/DNS preparation; never restart the budget at instantiation.
    pub deadline: Instant,
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
            tcp_policy: crate::network::TcpPolicy::default(),
            http_buffer_budget: http_buffer::worker_budget(),
            engine,
            metrics,
            _epoch_ticker: EpochTickerGuard(stop),
        })
    }

    pub(crate) fn with_tcp_policy(mut self, policy: crate::network::TcpPolicy) -> Self {
        self.tcp_policy = policy;
        self
    }

    pub(crate) fn tcp_destination_allowed(&self, addr: std::net::SocketAddr) -> bool {
        self.tcp_policy.allows_destination(addr)
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
            approved_egress,
            deadline,
        } = invocation;
        if setup_started >= deadline {
            return Err(ExecError::Timeout);
        }
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

        let allowed_arc = std::sync::Arc::new(approved_egress);
        if !allowed_arc.addresses.is_empty() {
            let allowed = std::sync::Arc::clone(&allowed_arc);
            wasi_builder.allow_tcp(true);
            wasi_builder.allow_udp(false);
            let tcp_policy = self.tcp_policy.clone();
            wasi_builder.socket_addr_check(move |addr, use_| {
                let allowed = std::sync::Arc::clone(&allowed);
                let tcp_policy = tcp_policy.clone();
                Box::pin(async move { tcp_policy.allows_connect(&allowed.addresses, addr, use_) })
            });
        }

        let host = HostState {
            ctx: wasi_builder.build(),
            table: http_limits::resource_table(),
            limits: metered_limits,
            http_ctx: http_limits::context(),
            approved_egress: allowed_arc,
            http_buffer_budget: http_buffer::Budget::new(self.http_buffer_budget.clone()),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|state| &mut state.limits);

        install_epoch_deadline(
            &mut store,
            deadline.min(Instant::now() + limits.max_wall_time()),
        );

        let fuel_to_set = limits.max_fuel.unwrap_or(u64::MAX);
        store
            .set_fuel(fuel_to_set)
            .map_err(|e| ExecError::failed(format_args!("failed to set fuel: {e}")))?;

        let wall = limits.max_wall_time();
        let started = Instant::now();

        let streamed_bytes = Arc::clone(&stream.bytes);
        let (req_res, out, receiver) = {
            let mut req = into_request(request)
                .map_err(|e| ExecError::failed(format_args!("bad ingress request: {e}")))?;
            req.headers_mut()
                .remove(hyper::header::HeaderName::from_static("x-hibana-env"));
            if !built_env.pairs.is_empty() {
                let map: serde_json::Map<String, serde_json::Value> = built_env
                    .pairs
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                let json = serde_json::to_vec(&serde_json::Value::Object(map))
                    .map_err(|e| ExecError::failed(format_args!("env encode: {e}")))?;
                let encoded = hibana_shared::b64url_encode(&json);
                if let Ok(hv) = hyper::header::HeaderValue::from_str(&encoded) {
                    req.headers_mut()
                        .insert(hyper::header::HeaderName::from_static("x-hibana-env"), hv);
                }
            }
            req.headers_mut().remove("x-hibana-event");
            http_limits::check(req.headers()).map_err(ExecError::failed)?;
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let scheme = if req.uri().scheme_str() == Some("https") {
                Scheme::Https
            } else {
                Scheme::Http
            };
            let req_res = store
                .data_mut()
                .new_incoming_request(scheme, req)
                .map_err(|e| ExecError::failed(format_args!("new_incoming_request: {e}")))?;
            let out = store
                .data_mut()
                .new_response_outparam(sender)
                .map_err(|e| ExecError::failed(format_args!("new_response_outparam: {e}")))?;
            (req_res, out, receiver)
        };

        let setup_us = setup_started.elapsed().as_micros() as u64;
        let exec_future = async {
            let link_started = Instant::now();
            let ppre = ProxyPre::new(component.0.clone())
                .map_err(|e| ExecError::failed(format_args!("not a proxy component: {e}")))?;
            let link_us = link_started.elapsed().as_micros() as u64;
            let instantiate_started = Instant::now();
            let proxy = ppre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| ExecError::failed(format_args!("failed to instantiate proxy: {e}")))?;
            let instantiate_us = instantiate_started.elapsed().as_micros() as u64;
            let handler_started = Instant::now();

            let call = proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, req_res, out);
            let recv = async {
                match receiver.await {
                    Ok(Ok(resp)) => stream.send(resp).await,
                    _ => Err(anyhow!("guest did not set a response")),
                }
            };
            // A trapped handler leaves its response-outparam in the Store. Do
            // not wait for that sender to drop before reporting the original trap.
            // Likewise, a failed/disconnected response must stop guest work.
            let result = tokio::try_join!(call, recv);
            tracing::debug!(target: "hibana_latency", stage = "wasm_runtime", setup_us, link_us, instantiate_us,
                handler_us = handler_started.elapsed().as_micros() as u64, "HTTP phase timing");

            Ok::<_, ExecError>(result.map(|((), receipt)| receipt))
        };

        let timed = tokio::time::timeout_at(deadline, exec_future).await;

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
                        Err(ExecError::failed(format_args!("fuel exhausted: {trap}")))
                    } else if is_interrupt_trap(&trap) || started.elapsed() >= wall {
                        Err(ExecError::Timeout)
                    } else {
                        Err(ExecError::failed(format_args!("wasm trap: {trap}")))
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

impl ExecError {
    pub(crate) fn failed(message: impl std::fmt::Display) -> Self {
        Self::Failed(hibana_shared::diagnostics::format_error(message))
    }
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
