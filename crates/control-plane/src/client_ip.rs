//! Resolve proxy chains only across explicitly trusted network peers.
use axum::http::HeaderMap;
use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Default)]
pub struct TrustedProxies(Vec<IpNet>);

impl TrustedProxies {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        if value.trim().is_empty() {
            return Ok(Self::default());
        }
        let networks = value
            .split(',')
            .map(|entry| {
                let network: IpNet = entry.trim().parse().map_err(|_| {
                    anyhow::anyhow!("TRUSTED_PROXY_CIDRS must contain comma-separated IP CIDRs")
                })?;
                anyhow::ensure!(
                    network.prefix_len() != 0,
                    "TRUSTED_PROXY_CIDRS must not trust every address"
                );
                Ok(network)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self(networks))
    }

    fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(&ip))
    }

    pub fn client_ip(&self, headers: &HeaderMap, peer: SocketAddr) -> String {
        let mut current = canonical(peer.ip());
        // Each trusted proxy must append its immediate peer, or overwrite XFF
        // at the outside boundary. Stop at the first untrusted address, so an
        // attacker-controlled prefix (including malformed text) has no effect.
        let values: Vec<_> = headers.get_all("x-forwarded-for").iter().collect();
        for value in values.into_iter().rev() {
            if !self.contains(current) {
                break;
            }
            let Ok(value) = value.to_str() else { break };
            for entry in value.rsplit(',') {
                if !self.contains(current) {
                    return current.to_string();
                }
                let Ok(ip) = entry.trim().parse() else {
                    return current.to_string();
                };
                current = canonical(ip);
            }
        }
        current.to_string()
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(trusted: &str, peer: &str, forwarded: &[&str]) -> String {
        let mut headers = HeaderMap::new();
        for value in forwarded {
            headers.append("x-forwarded-for", value.parse().unwrap());
        }
        TrustedProxies::parse(trusted)
            .unwrap()
            .client_ip(&headers, peer.parse().unwrap())
    }

    #[test]
    fn untrusted_peers_cannot_supply_an_identity() {
        assert_eq!(
            resolve("", "203.0.113.1:80", &["198.51.100.1"]),
            "203.0.113.1"
        );
        assert_eq!(
            resolve("10.1.0.0/24", "203.0.113.1:80", &["198.51.100.1"]),
            "203.0.113.1"
        );
    }

    #[test]
    fn trusted_chains_stop_before_a_spoofed_prefix() {
        let trusted = "10.1.0.0/24,10.2.0.1/32";
        assert_eq!(
            resolve(trusted, "10.1.0.2:80", &["198.51.100.1, 10.2.0.1"]),
            "198.51.100.1"
        );
        assert_eq!(
            resolve(
                trusted,
                "10.1.0.2:80",
                &["spoofed, 203.0.113.7", "10.2.0.1"]
            ),
            "203.0.113.7"
        );
        assert_eq!(
            resolve(trusted, "10.1.0.2:80", &["198.51.100.1, 192.0.2.1"]),
            "192.0.2.1"
        );
    }

    #[test]
    fn missing_or_malformed_headers_do_not_invent_addresses() {
        for values in [
            vec![],
            vec![""],
            vec!["unknown"],
            vec!["198.51.100.1:80"],
            vec!["198.51.100.1, bad"],
        ] {
            assert_eq!(resolve("10.1.0.0/24", "10.1.0.2:80", &values), "10.1.0.2");
        }
    }

    #[test]
    fn ipv6_and_mapped_ipv4_have_stable_counter_keys() {
        assert_eq!(
            resolve("::1/128", "[::1]:80", &["2001:db8:0:0::1"]),
            "2001:db8::1"
        );
        assert_eq!(
            resolve(
                "127.0.0.1/32",
                "[::ffff:127.0.0.1]:80",
                &["::ffff:198.51.100.1"]
            ),
            "198.51.100.1"
        );
        assert_eq!(resolve("", "[::ffff:198.51.100.1]:80", &[]), "198.51.100.1");
    }

    #[test]
    fn invalid_or_universal_trust_is_rejected() {
        for value in [
            "true",
            "10.1.1.1",
            "10.1.1.1/33",
            "10.1.0.0/24,",
            "0.0.0.0/0",
            "::/0",
        ] {
            assert!(TrustedProxies::parse(value).is_err());
        }
    }
}
