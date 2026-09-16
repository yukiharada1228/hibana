//! Operator-approved private TCP destinations. Version capabilities are still required.
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
};
use wasmtime_wasi::SocketAddrUse;

#[derive(Clone, Default)]
pub(crate) struct TcpPolicy {
    private_endpoints: HashSet<SocketAddr>,
}

impl TcpPolicy {
    pub(crate) fn parse_private_endpoints(value: &str) -> anyhow::Result<Self> {
        let mut private_endpoints = HashSet::new();
        if !value.trim().is_empty() {
            for entry in value.split(',') {
                let addr: SocketAddr = entry.trim().parse().map_err(|_| {
                    anyhow::anyhow!(
                        "WORKER_PRIVATE_TCP_ENDPOINTS requires comma-separated IPv4:port literals"
                    )
                })?;
                anyhow::ensure!(
                    matches!(addr.ip(), IpAddr::V4(ip) if ip.is_private()) && addr.port() != 0,
                    "WORKER_PRIVATE_TCP_ENDPOINTS only accepts RFC1918 IPv4 addresses with nonzero ports"
                );
                private_endpoints.insert(addr);
                anyhow::ensure!(
                    private_endpoints.len() <= 64,
                    "WORKER_PRIVATE_TCP_ENDPOINTS accepts at most 64 endpoints"
                );
            }
        }
        Ok(Self { private_endpoints })
    }

    pub(crate) fn allows_destination(&self, addr: SocketAddr) -> bool {
        !hibana_shared::egress::is_hard_denied(addr.ip()) || self.private_endpoints.contains(&addr)
    }

    pub(crate) fn allows_connect(
        &self,
        approved: &HashSet<SocketAddr>,
        addr: SocketAddr,
        use_: SocketAddrUse,
    ) -> bool {
        matches!(use_, SocketAddrUse::TcpConnect)
            && approved.contains(&addr)
            && self.allows_destination(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    #[test]
    fn private_tcp_requires_operator_and_version_approval() {
        let db = addr("172.26.0.10:5432");
        let policy = TcpPolicy::parse_private_endpoints("172.26.0.10:5432").unwrap();
        let approved = HashSet::from([db]);
        assert!(!TcpPolicy::default().allows_connect(&approved, db, SocketAddrUse::TcpConnect));
        assert!(!policy.allows_connect(&HashSet::new(), db, SocketAddrUse::TcpConnect));
        assert!(policy.allows_connect(&approved, db, SocketAddrUse::TcpConnect));
    }

    #[test]
    fn private_tcp_pins_both_ip_and_port_and_rejects_other_socket_uses() {
        let db = addr("172.26.0.10:5432");
        let other_ip = addr("172.26.0.11:5432");
        let other_port = addr("172.26.0.10:5433");
        let approved = HashSet::from([db, other_ip, other_port]);
        let policy = TcpPolicy::parse_private_endpoints("172.26.0.10:5432").unwrap();
        assert!(!policy.allows_connect(&approved, other_ip, SocketAddrUse::TcpConnect));
        assert!(!policy.allows_connect(&approved, other_port, SocketAddrUse::TcpConnect));
        assert!(!policy.allows_connect(&approved, db, SocketAddrUse::TcpBind));
        assert!(!policy.allows_connect(&approved, db, SocketAddrUse::UdpBind));
        assert!(!policy.allows_connect(&approved, db, SocketAddrUse::UdpConnect));
    }

    #[test]
    fn private_tcp_does_not_open_metadata_loopback_or_mapped_addresses() {
        let policy = TcpPolicy::parse_private_endpoints("172.26.0.10:5432").unwrap();
        for value in [
            "169.254.169.254:80",
            "127.0.0.1:5432",
            "0.0.0.0:5432",
            "[::1]:5432",
            "[::ffff:172.26.0.10]:5432",
            "100.64.0.1:5432",
        ] {
            let target = addr(value);
            assert!(!policy.allows_connect(
                &HashSet::from([target]),
                target,
                SocketAddrUse::TcpConnect
            ));
            assert!(TcpPolicy::parse_private_endpoints(value).is_err());
        }
    }

    #[test]
    fn private_tcp_configuration_is_explicit_and_bounded() {
        assert!(TcpPolicy::parse_private_endpoints(" ")
            .unwrap()
            .private_endpoints
            .is_empty());
        assert_eq!(
            TcpPolicy::parse_private_endpoints(" 10.0.0.1:5432,192.168.1.1:5432,10.0.0.1:5432 ")
                .unwrap()
                .private_endpoints
                .len(),
            2
        );
        for value in [
            "db.internal:5432",
            "10.0.0.0/8:5432",
            "10.0.0.1:0",
            "8.8.8.8:5432",
            "10.0.0.1:5432,",
            "[fc00::1]:5432",
        ] {
            assert!(
                TcpPolicy::parse_private_endpoints(value).is_err(),
                "{value}"
            );
        }
        let oversized = (1..=65)
            .map(|i| format!("10.0.0.{i}:5432"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(TcpPolicy::parse_private_endpoints(&oversized).is_err());
    }

    #[test]
    fn private_tcp_preserves_public_destination_capabilities() {
        let public = addr("1.1.1.1:443");
        let policy = TcpPolicy::default();
        assert!(policy.allows_connect(&HashSet::from([public]), public, SocketAddrUse::TcpConnect));
        assert!(!policy.allows_connect(&HashSet::new(), public, SocketAddrUse::TcpConnect));
    }
}
