//! Bound retained HTTP fields as well as Wasm linear memory. A per-field limit
//! alone is insufficient: a guest could retain thousands of full field resources.
use hyper::{
    header::{HeaderName, HeaderValue},
    HeaderMap,
};
use wasmtime::component::ResourceTable;
use wasmtime_wasi_http::WasiHttpCtx;

pub(super) const MAX_HOST_RESOURCES: usize = 128;
pub(super) const MAX_FIELD_BYTES: usize = 512 * 1024;
// Keep these bounds together: limiting only individual fields allows a guest to
// retain many full maps. This is separate from linear memory, and includes room
// for the JSON/base64 encoding of the bounded environment. Transport buffers and
// HeaderMap allocator overhead are not part of Wasmtime's field accounting.

pub(super) fn resource_table() -> ResourceTable {
    let mut table = ResourceTable::new();
    table.set_max_capacity(MAX_HOST_RESOURCES);
    table
}

pub(super) fn context() -> WasiHttpCtx {
    let mut ctx = WasiHttpCtx::new();
    ctx.set_field_size_limit(MAX_FIELD_BYTES);
    ctx
}

pub(super) fn check(headers: &HeaderMap) -> anyhow::Result<()> {
    // Match Wasmtime's field accounting (names/values plus their structures).
    // FieldMap::new accepts oversized input, so validate host-created fields too.
    let bytes: usize = headers
        .keys()
        .map(|name| name.as_str().len() + size_of::<HeaderName>())
        .sum::<usize>()
        + headers
            .values()
            .map(|value| value.len() + size_of::<HeaderValue>())
            .sum::<usize>();
    anyhow::ensure!(
        bytes <= MAX_FIELD_BYTES,
        "HTTP fields exceed host memory limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{limits::MeteredLimits, HostState};
    use std::sync::{atomic::AtomicU64, Arc};
    use wasmtime::component::Resource;
    use wasmtime_wasi::WasiCtxBuilder;
    use wasmtime_wasi_http::{
        bindings::http::types::{HostFields, HostOutgoingRequest},
        WasiHttpImpl,
    };

    fn host() -> HostState {
        HostState {
            ctx: WasiCtxBuilder::new().build(),
            table: resource_table(),
            limits: MeteredLimits::new(16 * 1024 * 1024, Arc::new(AtomicU64::new(0))),
            http_buffer_budget: crate::runtime::http_buffer::Budget::new(
                crate::runtime::http_buffer::worker_budget(),
            ),
            http_ctx: context(),
            approved_egress: Arc::default(),
        }
    }

    #[test]
    fn field_mutations_and_construction_have_the_same_bound() {
        let mut state = host();
        let mut host = WasiHttpImpl(&mut state);
        let fields = HostFields::new(&mut host).unwrap();
        let borrow = || Resource::new_borrow(fields.rep());
        let large = vec![b'x'; MAX_FIELD_BYTES];
        assert!(HostFields::append(&mut host, borrow(), "x-large".into(), large.clone()).is_err());
        assert!(
            HostFields::set(&mut host, borrow(), "x-large".into(), vec![large.clone()]).is_err()
        );
        assert!(HostFields::from_list(&mut host, vec![("x-large".into(), large)]).is_err());
        let chunk = vec![b'x'; 16 * 1024];
        for _ in 0..31 {
            HostFields::append(&mut host, borrow(), "x-small".into(), chunk.clone())
                .unwrap()
                .unwrap();
        }
        assert!(HostFields::append(&mut host, borrow(), "x-small".into(), chunk).is_err());
        assert_eq!(
            HostFields::get(&mut host, borrow(), "x-small".into())
                .unwrap()
                .len(),
            31
        );
        HostFields::drop(&mut host, fields).unwrap();
    }

    #[test]
    fn clones_and_transferred_fields_cannot_escape_the_resource_count() {
        let mut state = host();
        let mut host = WasiHttpImpl(&mut state);
        let fields =
            HostFields::from_list(&mut host, vec![("x-example".into(), vec![b'x'; 16 * 1024])])
                .unwrap()
                .unwrap();
        let mut requests = Vec::new();
        for _ in 1..MAX_HOST_RESOURCES {
            let copy = HostFields::clone(&mut host, Resource::new_borrow(fields.rep())).unwrap();
            requests.push(HostOutgoingRequest::new(&mut host, copy).unwrap());
        }
        assert!(HostFields::new(&mut host).is_err());
        assert!(HostFields::clone(&mut host, Resource::new_borrow(fields.rep())).is_err());
        HostOutgoingRequest::drop(&mut host, requests.pop().unwrap()).unwrap();
        let replacement = HostFields::new(&mut host).unwrap();
        assert!(HostFields::new(&mut host).is_err());
        HostFields::drop(&mut host, replacement).unwrap();
        HostFields::drop(&mut host, fields).unwrap();
        for request in requests {
            HostOutgoingRequest::drop(&mut host, request).unwrap();
        }
    }

    #[test]
    fn host_created_fields_are_checked_and_allow_the_largest_encoded_environment() {
        let mut headers = HeaderMap::new();
        // Control characters have the largest JSON expansion before base64.
        let map: std::collections::BTreeMap<_, _> = (0..8)
            .map(|i| {
                (
                    format!("KEY_{i}"),
                    "\0".repeat(hibana_shared::MAX_ENV_VALUE_BYTES - 64),
                )
            })
            .collect();
        let env = hibana_shared::b64url_encode(&serde_json::to_vec(&map).unwrap());
        headers.insert("x-hibana-env", HeaderValue::from_str(&env).unwrap());
        check(&headers).unwrap();
        headers.insert(
            "x-extra",
            HeaderValue::from_bytes(&vec![b'x'; MAX_FIELD_BYTES]).unwrap(),
        );
        assert!(check(&headers).is_err());
    }
}
