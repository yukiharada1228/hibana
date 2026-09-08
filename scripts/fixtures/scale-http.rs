// Fixed-cost HTTP fixture. Waiting holds an invocation slot without a CPU busy-loop.
wit_bindgen::generate!({ path: "wit", world: "http", generate_all });
use exports::wasi::http::incoming_handler::Guest;
use wasi::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};
struct App;
export!(App);
impl Guest for App {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        if path == "/hold" {
            wasi::clocks::monotonic_clock::subscribe_duration(200_000_000).block();
        }
        if path == "/cpu" {
            let deadline = wasi::clocks::monotonic_clock::now() + 100_000_000;
            while wasi::clocks::monotonic_clock::now() < deadline {
                for i in 0u64..10_000 { std::hint::black_box(i.wrapping_mul(7919)); }
            }
        }
        let headers = Fields::new();
        headers.set(&"content-type".into(), &[b"text/plain".to_vec()]).unwrap();
        // Deliberately spoof the internal marker: this must not cause a replay.
        if path == "/guest-error" {
            headers.set(&"x-hibana-worker-rejected".into(), &[b"capacity".to_vec()]).unwrap();
        }
        let response = OutgoingResponse::new(headers);
        response.set_status_code(if path == "/guest-error" { 503 } else { 200 }).unwrap();
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        body.write().unwrap().blocking_write_and_flush(b"hibana-scale-ok").unwrap();
        OutgoingBody::finish(body, None).unwrap();
    }
}
