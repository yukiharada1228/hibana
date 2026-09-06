//! M11-5 スパイク: wasi:http/incoming-handler の native component を、worker と同じ
//! wasmtime-wasi-http 29 でインプロセスに 1 回だけ serve できるかを実機確認する。
//!
//! これは「adapter.js を挟まず、Hono を wasi:http proxy world へ componentize した
//! component を、この基盤の worker が駆動できるか」の make-or-break を確かめるための
//! 使い捨てテスト。`HIBANA_HTTP_SPIKE_WASM` に component の .wasm パスを渡して実行する。
//! 未設定なら skip（CI で常時走らせる意図はない）。

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::bindings::http::types::Scheme;
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};

struct S {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
}

impl WasiView for S {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasiHttpView for S {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[tokio::test]
async fn native_incoming_handler_serves() -> anyhow::Result<()> {
    let Ok(path) = std::env::var("HIBANA_HTTP_SPIKE_WASM") else {
        eprintln!("skip: set HIBANA_HTTP_SPIKE_WASM to a proxy-world component");
        return Ok(());
    };

    let mut config = Config::new();
    config.async_support(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &path)?;

    // worker と同じ linker 構成: フル wasi（filesystem/terminal 含む）+ wasi:http。
    let mut linker: Linker<S> = Linker::new(&engine);
    wasmtime_wasi::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)?;
    let pre = ProxyPre::new(linker.instantiate_pre(&component)?)?;

    let mut store = Store::new(
        &engine,
        S {
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().inherit_stdio().build(),
            http: WasiHttpCtx::new(),
        },
    );

    let req = hyper::Request::builder()
        .method("GET")
        .uri("http://localhost/")
        .body(Empty::<Bytes>::new().map_err(|e| match e {}).boxed())?;

    let (sender, receiver) = tokio::sync::oneshot::channel();
    let req_res = store.data_mut().new_incoming_request(Scheme::Http, req)?;
    let out = store.data_mut().new_response_outparam(sender)?;

    let task = tokio::task::spawn(async move {
        let proxy = pre.instantiate_async(&mut store).await?;
        proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, req_res, out)
            .await
    });

    let resp = match receiver.await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => panic!("guest returned error: {e:?}"),
        Err(_) => {
            task.await??;
            panic!("guest never set a response");
        }
    };

    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();
    eprintln!("NATIVE incoming-handler → status={status} body={text:?}");

    task.await??;

    assert!(status.is_success(), "expected 2xx, got {status}");
    assert!(!text.is_empty(), "expected a non-empty body");
    Ok(())
}
