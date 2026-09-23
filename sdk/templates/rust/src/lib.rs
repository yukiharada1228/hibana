wit_bindgen::generate!({ path: "wit", world: "http", generate_all });

use exports::wasi::http::incoming_handler::Guest;
use wasi::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use wasi::io::streams::StreamError;

struct App;
export!(App);

impl Guest for App {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let path = path.split('?').next().unwrap_or("/");
        let (status, content_type, bytes) =
            if path == "/echo" && matches!(request.method(), Method::Post) {
                match read_body(&request) {
                    Ok(bytes) => (200, "application/octet-stream", bytes),
                    Err(()) => (400, "text/plain", b"Invalid request body".to_vec()),
                }
            } else if path == "/" {
                let message = std::env::var("GREETING").unwrap_or_else(|_| "Hello Hibana".into());
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({ "message": message })).unwrap(),
                )
            } else {
                (404, "text/plain", b"Not found".to_vec())
            };
        let head = matches!(request.method(), Method::Head);
        drop(request);
        let headers = Fields::new();
        headers
            .set(&"content-type".into(), &[content_type.as_bytes().to_vec()])
            .unwrap();
        let response = OutgoingResponse::new(headers);
        response.set_status_code(status).unwrap();
        let body = response.body().unwrap();
        ResponseOutparam::set(response_out, Ok(response));
        {
            let stream = body.write().unwrap();
            if !head {
                for chunk in bytes.chunks(4096) {
                    if stream.blocking_write_and_flush(chunk).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = OutgoingBody::finish(body, None);
    }
}

fn read_body(request: &IncomingRequest) -> Result<Vec<u8>, ()> {
    let body = request.consume().map_err(|_| ())?;
    let stream = body.stream().map_err(|_| ())?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(4096) {
            Ok(chunk) => {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    return Err(());
                }
                bytes.extend(chunk);
            }
            Err(StreamError::Closed) => break,
            Err(_) => return Err(()),
        }
    }
    drop(stream);
    drop(IncomingBody::finish(body));
    Ok(bytes)
}
