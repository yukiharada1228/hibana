//! wasmtime component-model バインディング。
//!
//! `wit/world.wit` の world `handler`（`package faas:component@1.0.0`）から
//! ホスト側の型付き呼び出しコードを生成する。
//!
//! - `async: true`  : `handle` を async で呼べるようにする（epoch 停止と協調）。
//! - 生成される型:
//!     - `Handler`               … インスタンス化済み world
//!     - `HandlerError`          … `result<list<u8>, handler-error>` のエラー側
//!     - `ErrorKind`             … `enum error-kind`
//!
//! `path` はこのファイル(crates/worker/src/bindings.rs)からの相対で wit/ を指す。
wasmtime::component::bindgen!({
    world: "handler",
    path: "../../wit/world.wit",
    async: true,
});
