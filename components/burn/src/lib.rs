//! M8 chaos 用: 入力 JSON の `burn_ms` で指定されたぶんだけ busy-loop してから
//! **succeeded で終端する** component。
//!
//! なぜ新設するのか（テストの都合ではなく、完了条件を決定的に測るための設計要素）:
//! `components/slow` は無限 tight loop なので **必ず timeout で終端する**。M8 の chaos は
//! 「テナント A の総仕事量 = 件数 × 1 件あたりの実行時間」を設計パラメータとして制御し、
//! その上で「B のレイテンシが劣化しない」を測る。したがって
//! 「N ミリ秒かかって **成功する**」ノブが要る。slow ではそれが作れない。
//!
//! # 時間を読まずに時間を作る
//!
//! 標準 handler world（wit/world.wit）は host import を 1 つも持たないので、ゲストは
//! **時刻を読めない**。よって `burn_ms` を **反復回数**に翻訳する
//! （`iterations = burn_ms * ITERS_PER_MS`）。`components/slow` と同じく `#[inline(never)]`
//! の補助関数をループ内で呼び、関数入口の epoch check が確実に挿入される形にする
//! （tight inline ループだと back-edge の check タイミングが期待通りに来ないことがある）。
//!
//! `ITERS_PER_MS` はマシン依存であり、**絶対的な実行時間の精度は要求しない**。
//! chaos が要求するのは「1 件が echo より十分長い」ことだけで、実際に滞留したかどうかは
//! 整数条件（件数・順序）で確認するので機種差は吸収される。
//!
//! # 入出力
//!
//! - 入力: `{"burn_ms": <整数>}`。キーが無い / 数値でない / JSON でない場合は 0 として扱い、
//!   echo 相当の即時終端になる（入力不正でも failed にしない。ここで failed にすると、
//!   chaos が「負荷をかけたつもりが全件エラーで即終端していた」ことに気づけない）。
//! - 出力: `{"burned_ms": <クランプ後の要求値>, "iterations": <実際に回した回数>}`。
//!
//! `MAX_BURN_MS` はゲスト側の防御であって打ち切り機構ではない。実際の打ち切りは version ごとの
//! `ResourceLimits.max_wall_time_ms` に対する epoch interruption が行う（§4.3）。

wit_bindgen::generate!({
    world: "handler",
    path: "wit",
});

// `generate!` は `HandlerError` をクレート直下へ取り込むが、`ErrorKind` はしない
// （echo と同じ作法）。
use crate::faas::component::types::ErrorKind;

struct Component;

/// 1 ミリ秒あたりの反復回数の目安。マシン依存なので概算でよい（上のドキュメント参照）。
///
/// 実測で較正した値（Apple Silicon / `--release`）: 6.5e8 反復 ≈ 1.0 秒。
/// 較正の手順は `make deploy-chaos-components` 後に
/// `burn_ms` を変えて `POST /invoke?wait=1` の実時間を測るだけでよい。
/// 大きくずれるマシンでは chaos の待ち窓が合わなくなるので、その時はここを直す。
const ITERS_PER_MS: u64 = 650_000;

/// ゲスト側の自衛上限。これを超える要求は黙ってクランプする。
const MAX_BURN_MS: u64 = 10_000;

#[inline(never)]
fn burn_step(acc: u64) -> u64 {
    // 単純な加算 + black_box。関数呼び出し境界を確実に作るためだけのヘルパー
    // （slow と同じ理由。epoch check の挿入箇所を確定させる）。
    let next = acc.wrapping_add(1);
    std::hint::black_box(next)
}

/// 入力から `burn_ms` を取り出す。解釈できないものはすべて 0 に倒す。
fn requested_burn_ms(input: &[u8]) -> u64 {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(input) else {
        return 0;
    };
    value
        .get("burn_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(MAX_BURN_MS)
}

impl Guest for Component {
    fn handle(input: Vec<u8>) -> Result<Vec<u8>, HandlerError> {
        let burn_ms = requested_burn_ms(&input);
        let iterations = burn_ms.saturating_mul(ITERS_PER_MS);

        let mut acc: u64 = 0;
        for _ in 0..iterations {
            acc = burn_step(acc);
        }
        // acc を出力に混ぜない代わりに black_box で保持する。ループ全体が
        // 最適化で消えると「burn_ms を指定したのに一瞬で返る」という静かな嘘になる。
        std::hint::black_box(acc);

        let body = serde_json::json!({
            "burned_ms": burn_ms,
            "iterations": iterations,
        });
        serde_json::to_vec(&body).map_err(|e| HandlerError {
            kind: ErrorKind::Runtime,
            message: format!("failed to serialize burn result: {e}"),
        })
    }
}

export!(Component);
