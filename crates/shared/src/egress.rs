//! Resolved-address egress policy shared by HTTP and WASI sockets.
//! Destination approval never permits private/special addresses. Operators may
//! separately approve exact RFC1918 TCP endpoints; local dev has its own policy.
use ipnet::{Ipv4Net, Ipv6Net};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// Policy data stays here; CIDR arithmetic belongs to ipnet. Keep in sync with
// IANA's IPv4 special-purpose registry and IPv6 address-space assignments.
const DENIED_V4: &[Ipv4Net] = &[
    Ipv4Net::new_assert(Ipv4Addr::new(0, 0, 0, 0), 8), // this network / unspecified
    Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10), // shared / CGNAT
    Ipv4Net::new_assert(Ipv4Addr::new(127, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16), // includes cloud metadata
    Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 0, 0), 24), // protocol assignments
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 2, 0), 24), // documentation
    Ipv4Net::new_assert(Ipv4Addr::new(192, 88, 99, 0), 24), // deprecated 6to4 relay
    Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15), // benchmarking
    Ipv4Net::new_assert(Ipv4Addr::new(198, 51, 100, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(203, 0, 113, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(224, 0, 0, 0), 4), // multicast
    Ipv4Net::new_assert(Ipv4Addr::new(240, 0, 0, 0), 4), // reserved / broadcast
];

// Native IPv6 egress uses the currently assigned global-unicast space only.
// Translation/tunnel ranges are deliberately unsupported: their encoded IPv4
// destination could otherwise bypass the private-address gate.
const GLOBAL_V6: Ipv6Net = Ipv6Net::new_assert(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3);
const DENIED_V6: &[Ipv6Net] = &[
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23), // protocol assignments, including Teredo
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16), // 6to4
    Ipv6Net::new_assert(Ipv6Addr::new(0x3ffe, 0, 0, 0, 0, 0, 0, 0), 16), // retired 6bone
    Ipv6Net::new_assert(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20), // documentation
];

/// Reject local/special destinations before checking the application's allowlist.
/// IPv4-mapped IPv6 addresses have exactly the same policy as their IPv4 address.
pub fn is_hard_denied(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => DENIED_V4.iter().any(|network| network.contains(&ip)),
        IpAddr::V6(ip) => {
            !GLOBAL_V6.contains(&ip) || DENIED_V6.iter().any(|network| network.contains(&ip))
        }
    }
}

/// allowlist の 1 エントリ（`host:port`）。ホスト名 or IP リテラル + ポート。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EgressEndpoint {
    pub host: String,
    pub port: u16,
}

/// `"host:port"` をパースする。ポート必須（`:443` を省略させない —— ポートまで縛るのが方針）。
///
/// IPv6 リテラルは `[::1]:443` 形式を受ける。ホスト名は空でないこと。
pub fn parse_egress_endpoint(raw: &str) -> Result<EgressEndpoint, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty endpoint".into());
    }
    // `[ipv6]:port`
    if let Some(rest) = raw.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| format!("malformed bracketed endpoint '{raw}' (want [ipv6]:port)"))?;
        let port = parse_port(port_part)?;
        if host.is_empty() {
            return Err(format!("empty host in '{raw}'"));
        }
        return Ok(EgressEndpoint {
            host: host.to_string(),
            port,
        });
    }
    // `host:port`（ホスト名 or IPv4）。IPv6 の裸表記（コロン多数）は明示的に弾く。
    let (host, port_part) = raw
        .rsplit_once(':')
        .ok_or_else(|| format!("endpoint '{raw}' must be host:port"))?;
    if host.contains(':') {
        return Err(format!(
            "bare IPv6 not allowed; use [addr]:port for '{raw}'"
        ));
    }
    if host.is_empty() {
        return Err(format!("empty host in '{raw}'"));
    }
    let port = parse_port(port_part)?;
    Ok(EgressEndpoint {
        host: host.to_string(),
        port,
    })
}

fn parse_port(s: &str) -> Result<u16, String> {
    s.trim()
        .parse::<u16>()
        .map_err(|_| format!("invalid port '{s}'"))
        .and_then(|p| {
            if p == 0 {
                Err("port 0 is not a valid egress target".into())
            } else {
                Ok(p)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    /// **最重要**: クラウドメタデータ IP は必ず拒否される（IAM 資格情報窃取の主経路）。
    #[test]
    fn cloud_metadata_ip_is_denied() {
        assert!(is_hard_denied(v4("169.254.169.254")));
        // IPv4-mapped 経由でもすり抜けない。
        assert!(is_hard_denied(v6("::ffff:169.254.169.254")));
    }

    /// 拒否すべき代表 IP を全部拒否する。
    #[test]
    fn denies_all_internal_ranges() {
        for s in [
            "0.0.0.0",
            "0.0.0.1",
            "0.255.255.255",
            "127.0.0.1",
            "127.5.5.5",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "169.254.169.254",
            "100.64.0.1", // CGNAT
            "100.127.255.1",
            "198.18.0.1", // benchmarking
            "224.0.0.1",  // multicast
            "239.255.255.255",
            "255.255.255.255", // broadcast
            "192.0.2.1",       // documentation
            "198.51.100.1",
            "203.0.113.1",
            "192.0.0.8",
            "192.88.99.1",
            "240.0.0.1", // reserved
            "255.0.0.0",
        ] {
            assert!(is_hard_denied(v4(s)), "must deny {s}");
            let mapped = s.parse::<Ipv4Addr>().unwrap().to_ipv6_mapped();
            assert!(is_hard_denied(mapped.into()), "must deny mapped {s}");
        }
        for s in [
            "::",
            "::1",
            "fc00::1", // unique-local
            "fd12:3456::1",
            "fe80::1",     // link-local
            "ff02::1",     // multicast
            "2001:db8::1", // documentation
            "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff::1",
            "3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fec0::1",            // deprecated site-local
            "::8.8.8.8",          // deprecated IPv4-compatible encoding
            "64:ff9b::a9fe:a9fe", // NAT64 encoding of cloud metadata
            "64:ff9b:1::a9fe:a9fe",
            "2002:a9fe:a9fe::1", // 6to4 encoding of cloud metadata
            "2001::1",           // Teredo
            "2001:2::1",         // benchmarking
            "3ffe::1",           // retired 6bone
            "100::1",            // discard-only
            "5f00::1",           // segment routing
            "4000::1",           // reserved
        ] {
            assert!(is_hard_denied(v6(s)), "must deny {s}");
        }
    }

    /// パブリックにルーティング可能な IP は通す（false positive で正規の egress を殺さない）。
    #[test]
    fn allows_public_ips() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",  // example.com
            "140.82.121.4",   // github
            "172.15.255.255", // 172.16 の直前（private の境界外）
            "172.32.0.1",     // 172.31 の直後
            "100.63.255.255", // CGNAT の直前
            "100.128.0.1",    // CGNAT の直後
            "198.17.255.255", // benchmarking の直前
            "198.20.0.1",     // benchmarking の直後
        ] {
            assert!(!is_hard_denied(v4(s)), "must allow public {s}");
            let mapped = s.parse::<Ipv4Addr>().unwrap().to_ipv6_mapped();
            assert!(!is_hard_denied(mapped.into()), "must allow mapped {s}");
        }
        for s in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2001:200::1",
            "2001:db7:ffff::1",
            "2001:db9::1",
            "3fff:1000::1",
        ] {
            assert!(!is_hard_denied(v6(s)), "must allow public {s}");
        }
    }

    /// 境界値: private 範囲のちょうど内/外。
    #[test]
    fn private_range_boundaries() {
        assert!(!is_hard_denied(v4("9.255.255.255")));
        assert!(is_hard_denied(v4("10.0.0.0")));
        assert!(is_hard_denied(v4("10.255.255.255")));
        assert!(!is_hard_denied(v4("11.0.0.0")));
    }

    #[test]
    fn parse_endpoint_ok() {
        assert_eq!(
            parse_egress_endpoint("api.example.com:443").unwrap(),
            EgressEndpoint {
                host: "api.example.com".into(),
                port: 443
            }
        );
        assert_eq!(
            parse_egress_endpoint("  1.2.3.4:8080 ").unwrap(),
            EgressEndpoint {
                host: "1.2.3.4".into(),
                port: 8080
            }
        );
        assert_eq!(
            parse_egress_endpoint("[2606:4700::1]:443").unwrap(),
            EgressEndpoint {
                host: "2606:4700::1".into(),
                port: 443
            }
        );
    }

    #[test]
    fn parse_endpoint_rejects_bad() {
        for bad in [
            "",
            "hostonly",         // ポート無し
            "host:0",           // ポート 0
            "host:99999",       // 範囲外
            "host:abc",         // 非数値
            "2606:4700::1:443", // 裸 IPv6（曖昧）
            ":443",             // ホスト空
        ] {
            assert!(parse_egress_endpoint(bad).is_err(), "must reject '{bad}'");
        }
    }
}
