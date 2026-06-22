//! M4 chaos_d 用: handle が長時間処理を返さない component。タイトループで進み続けて
//! `max_wall_time_ms`（既定 1s）を超過させ、worker の epoch interruption が
//! `Trap::Interrupt` を出して status=timeout で finalize させる経路を検証する。
//!
//! 注: 標準 handler world は host import を持たないため、純粋なゲストループで時間を
//! 消費するしかない。wasmtime の epoch interruption は Cranelift がコード生成時に
//! 関数入口とループ back-edge に epoch check を挿入する。`#[inline(never)]` の補助関数を
//! ループ内で呼び出し、関数入口での epoch check が確実に発火する形にする
//! （tight inline ループだと back-edge での check タイミングが期待通りに来ないケースがある）。

wit_bindgen::generate!({
    world: "handler",
    path: "wit",
});

struct Component;

#[inline(never)]
fn busy_step(acc: u64) -> u64 {
    // 単純な加算 + black_box。関数呼び出し境界を確実に作るためだけのヘルパー。
    let next = acc.wrapping_add(1);
    std::hint::black_box(next)
}

impl Guest for Component {
    fn handle(_input: Vec<u8>) -> Result<Vec<u8>, HandlerError> {
        let mut acc: u64 = 0;
        loop {
            // 関数呼び出しで epoch check 挿入箇所を確実にする。
            acc = busy_step(acc);
        }
    }
}

export!(Component);
