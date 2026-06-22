//! M4 chaos_c 用: 常時 trap する component。`handle` の呼び出しで wasm unreachable を踏み、
//! Wasmtime が Trap を返す。worker はこれを `ExecError::Failed` として publish し、再配送が
//! `max_deliver` 回繰り返された時点で `.failed` (DLQ) へ落ちる経路を検証する。

wit_bindgen::generate!({
    world: "handler",
    path: "wit",
});

struct Component;

impl Guest for Component {
    fn handle(_input: Vec<u8>) -> Result<Vec<u8>, HandlerError> {
        // 入口で必ず panic させる。wasm では std::panic は unreachable へ降りるので、
        // Wasmtime はこれを Trap として観測する。
        panic!("always-trap component: intentional panic for M4 DLQ chaos test");
    }
}

export!(Component);
