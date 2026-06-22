//! echo Component — M1 Walking Skeleton のサンプルゲスト。
//!
//! `wit/world.wit` の `world handler` を実装し、`handle` をエクスポートする。
//! ホスト(worker)側の規約 (§4.2 / 仕様書 §15 M1):
//!   - 入力 `input: list<u8>` は `serde_json::to_vec(input)` の結果。
//!     つまり JobMessage.input(serde_json::Value) を JSON エンコードしたバイト列。
//!   - 出力 `list<u8>` も同様に JSON エンコードされたバイト列を返す。
//!     worker はこれを `serde_json::from_slice` して ResultMessage.output に詰める。
//!
//! 本実装は入力 JSON 値 `v` を `{"echo": v}` に包んで返すだけの最小ハンドラ。
//! 入力が JSON として解釈できない場合は、バイト列を UTF-8 文字列とみなして包む。

// wit-bindgen 0.36 の generate! で guest バインディングを生成する。
// `path` は本クレート内にコピーした WIT を参照（ワークスペース直下 wit/world.wit と同一内容）。
wit_bindgen::generate!({
    world: "handler",
    path: "wit",
});

use serde_json::{json, Value};

// `handler-error` は world が `use types.{handler-error}` するため、生成バインディングが
// すでにクレート直下へ取り込んでいる（`HandlerError`）。重複 import を避けここでは
// 取り込まない。`error-kind` は world が直接 `use` しないため、パッケージ名を辿る
// `crate::faas::component::types` から明示的に取り込む（`exports::` 配下ではない）。
use crate::faas::component::types::ErrorKind;

/// 生成された `world handler` を実装するゼロサイズの型。
struct Component;

impl Guest for Component {
    fn handle(input: Vec<u8>) -> Result<Vec<u8>, HandlerError> {
        // 入力バイト列を JSON 値として解釈。失敗時は UTF-8 文字列にフォールバック。
        let value: Value = match serde_json::from_slice::<Value>(&input) {
            Ok(v) => v,
            Err(_parse_err) => match std::str::from_utf8(&input) {
                Ok(s) => Value::String(s.to_string()),
                Err(utf8_err) => {
                    return Err(HandlerError {
                        kind: ErrorKind::InvalidInput,
                        message: format!("input is neither JSON nor valid UTF-8: {utf8_err}"),
                    });
                }
            },
        };

        // {"echo": <input value>} に包んで JSON エンコードして返す。
        let output = json!({ "echo": value });
        serde_json::to_vec(&output).map_err(|e| HandlerError {
            kind: ErrorKind::Runtime,
            message: format!("failed to serialize output: {e}"),
        })
    }
}

// 生成された export! マクロで Component を world のエクスポート実装として登録。
export!(Component);
