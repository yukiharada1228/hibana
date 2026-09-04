//! M9c 検証用 component: 入力 `{"target":"host:port"}` へ TCP 接続を試み、あわせて
//! **filesystem 到達も試す**。egress allowlist と fs deny-by-default の実効を実測するため。
//!
//! `wasi:sockets` / `wasi:filesystem` を import する（後者は std が推移的に焼き込む）。
//! 実際に到達できるかは worker の WasiCtx が決める（socket_addr_check + preopen 無し）。
//! 出力: `{"target":..,"net":"connected|denied:<kind>|no-target","fs":"<path>=OK(n)|DENIED(kind)"}`。
wit_bindgen::generate!({ world: "handler", path: "wit" });
use crate::faas::component::types::ErrorKind;
struct Component;

fn probe_fs() -> String {
    // preopen が無ければ、どのパスも開けないはず（fs deny-by-default の実測）。
    let mut out = Vec::new();
    for t in ["/etc/passwd", "/", "."] {
        match std::fs::read_dir(t) {
            Ok(rd) => out.push(format!("{t}=OPENDIR_OK({})", rd.count())),
            Err(e) => match std::fs::read(t) {
                Ok(b) => out.push(format!("{t}=READ_OK({}B)", b.len())),
                Err(e2) => out.push(format!("{t}=DENIED({}/{})", e.kind(), e2.kind())),
            },
        }
    }
    out.join(" ")
}

impl Guest for Component {
    fn handle(input: Vec<u8>) -> Result<Vec<u8>, HandlerError> {
        let target = serde_json::from_slice::<serde_json::Value>(&input)
            .ok()
            .and_then(|v| v.get("target").and_then(|t| t.as_str()).map(String::from))
            .unwrap_or_default();
        let net = if target.is_empty() {
            "no-target".to_string()
        } else {
            match std::net::TcpStream::connect(target.as_str()) {
                Ok(_) => "connected".to_string(),
                Err(e) => format!("denied:{}", e.kind()),
            }
        };
        let body = serde_json::json!({ "target": target, "net": net, "fs": probe_fs() });
        serde_json::to_vec(&body).map_err(|e| HandlerError {
            kind: ErrorKind::Runtime,
            message: format!("{e}"),
        })
    }
}
export!(Component);
