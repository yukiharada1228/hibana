//! Resolve only approved names, from the same snapshot used to authorize sockets.
use super::HostState;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
};
use wasmtime::component::{HasSelf, Linker, Resource};
use wasmtime_wasi::{
    p2::{
        bindings::sockets::{
            ip_name_lookup::{self, Host, HostResolveAddressStream, ResolveAddressStream},
            network::{self, ErrorCode, Network},
        },
        DynPollable, SocketError,
    },
    WasiView,
};

#[derive(Default)]
pub(crate) struct ApprovedEgress {
    pub(super) addresses: HashSet<SocketAddr>,
    hosts: HashMap<String, Vec<SocketAddr>>,
}

fn host_key(name: &str) -> String {
    let bare = name
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
        .unwrap_or(name);
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => bare.trim_end_matches('.').to_ascii_lowercase(),
    }
}

impl ApprovedEgress {
    // Call only after the operator's IP policy has accepted this destination.
    pub(crate) fn insert(&mut self, host: &str, addr: SocketAddr) {
        self.addresses.insert(addr);
        let addresses = self.hosts.entry(host_key(host)).or_default();
        if !addresses.contains(&addr) {
            addresses.push(addr);
        }
    }

    pub(crate) fn resolve(&self, name: &str) -> impl Iterator<Item = SocketAddr> + '_ {
        let key = host_key(name);
        // Literal IPs are safe to return without DNS. Socket policy still checks port.
        let literal = key.parse::<IpAddr>().ok();
        // Preserve the resolver's address preference for hostnames.
        self.hosts
            .get(&key)
            .filter(|_| literal.is_none())
            .into_iter()
            .flatten()
            .chain(
                self.addresses
                    .iter()
                    .filter(move |addr| literal == Some(addr.ip())),
            )
            .copied()
    }
}

pub(super) fn add_to_linker(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // Replace the entire resolver interface, retaining Wasmtime's stream/poll resources.
    // Keep the default OS resolver disabled in WasiCtx as a second barrier.
    linker.allow_shadowing(true);
    ip_name_lookup::add_to_linker::<HostState, HasSelf<HostState>>(linker, |state| state)?;
    linker.allow_shadowing(false);
    Ok(())
}

impl Host for HostState {
    fn resolve_addresses(
        &mut self,
        network: Resource<Network>,
        name: String,
    ) -> Result<Resource<ResolveAddressStream>, SocketError> {
        self.table.get(&network)?;
        let mut seen = HashSet::new();
        let addresses: Vec<_> = self
            .approved_egress
            .resolve(&name)
            .map(|addr| addr.ip())
            .filter(|ip| seen.insert(*ip))
            .collect();
        if addresses.is_empty() {
            return Err(ErrorCode::NameUnresolvable.into());
        }
        let stream = ResolveAddressStream::Done(Ok(addresses
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>()
            .into_iter()));
        Ok(self.table.push(stream)?)
    }
}

impl HostResolveAddressStream for HostState {
    fn resolve_next_address(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> Result<Option<ip_name_lookup::IpAddress>, SocketError> {
        self.ctx().resolve_next_address(resource)
    }

    fn subscribe(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> anyhow::Result<Resource<DynPollable>> {
        self.ctx().subscribe(resource)
    }

    fn drop(&mut self, resource: Resource<ResolveAddressStream>) -> anyhow::Result<()> {
        self.ctx().drop(resource)
    }
}

impl network::Host for HostState {
    fn convert_error_code(&mut self, error: SocketError) -> anyhow::Result<ErrorCode> {
        network::Host::convert_error_code(&mut self.ctx(), error)
    }

    fn network_error_code(
        &mut self,
        error: Resource<anyhow::Error>,
    ) -> anyhow::Result<Option<ErrorCode>> {
        network::Host::network_error_code(&mut self.ctx(), error)
    }
}

impl network::HostNetwork for HostState {
    fn drop(&mut self, resource: Resource<Network>) -> anyhow::Result<()> {
        network::HostNetwork::drop(&mut self.ctx(), resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicU64, Arc};
    use wasmtime_wasi::{ResourceTable, WasiCtxBuilder};

    #[tokio::test]
    async fn wasi_dns_returns_only_cached_approved_addresses_and_supports_polling() {
        let mut approved = ApprovedEgress::default();
        // These names cannot resolve through the OS; results must come from the snapshot.
        for addr in ["1.1.1.1:443", "1.1.1.1:8443", "[2606:4700:4700::1111]:443"] {
            approved.insert("approved.invalid", addr.parse().unwrap());
        }
        let mut host = HostState {
            ctx: WasiCtxBuilder::new().allow_ip_name_lookup(false).build(),
            table: ResourceTable::new(),
            limits: super::super::limits::MeteredLimits::new(65536, Arc::new(AtomicU64::new(0))),
            http_buffer_budget: crate::runtime::http_buffer::Budget::new(
                crate::runtime::http_buffer::worker_budget(),
            ),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            approved_egress: Arc::new(approved),
        };
        let network =
            wasmtime_wasi::p2::bindings::sockets::instance_network::Host::instance_network(
                &mut host.ctx(),
            )
            .unwrap();
        for name in [
            "unapproved.invalid",
            "localhost",
            "127.0.0.1",
            "[::1]",
            "sub.approved.invalid",
        ] {
            assert!(
                host.resolve_addresses(Resource::new_borrow(network.rep()), name.into())
                    .is_err(),
                "{name}"
            );
        }
        for (name, count) in [
            ("APPROVED.invalid.", 2),
            ("1.1.1.1", 1),
            ("[2606:4700:4700:0:0:0:0:1111]", 1),
        ] {
            let stream = host
                .resolve_addresses(Resource::new_borrow(network.rep()), name.into())
                .unwrap();
            let poll = host.subscribe(Resource::new_borrow(stream.rep())).unwrap();
            let mut addresses = Vec::new();
            while let Some(addr) = host
                .resolve_next_address(Resource::new_borrow(stream.rep()))
                .unwrap()
            {
                addresses.push(addr);
            }
            assert_eq!(addresses.len(), count, "{name}");
            if count == 2 {
                assert!(matches!(
                    addresses[0],
                    ip_name_lookup::IpAddress::Ipv4((1, 1, 1, 1))
                ));
            }
            wasmtime_wasi::p2::bindings::io::poll::HostPollable::drop(&mut host.table, poll)
                .unwrap();
            HostResolveAddressStream::drop(&mut host, stream).unwrap();
        }
        network::HostNetwork::drop(&mut host, network).unwrap();
    }
}
