//! M9c (§4.4 / §15 M9): egress allowlist の**セキュリティ核**。
//!
//! outbound を承認された宛先だけに絞るとき、最大の敵は **DNS rebinding / SSRF** である。
//! `wasmtime-wasi` の `socket_addr_check` は接続時に**解決後の `SocketAddr`（IP:port）**だけを
//! 受け取り、元のホスト名は渡ってこない。したがって「ホスト名の allowlist」を素朴に信じると、
//! allowlist に載ったホスト名が内部 IP（`169.254.169.254` のクラウドメタデータや `10.x` の
//! 内部ネット）へ解決されると、名前で allowlist を通り実 IP は内部、という到達が起きる。
//!
//! これを防ぐ核が [`hard_denied_reason`] である。**allowlist に何が書いてあっても優先して拒否する
//! IP の集合**を、解決後の IP に対して機械的に判定する。これは純関数なので網羅的に単体テストでき、
//! 本番でしか壊れないことが無い（M9 の設計方針: セキュリティ核は全列挙で固める）。
//!
//! 実際の allowlist 照合（解決後 IP が承認集合に入っているか）は worker 側が担い、
//! 本モジュールは「絶対に触らせない IP か」の判定と、`host:port` のパースだけを持つ。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 解決後の IP が **allowlist より優先して拒否すべき**ものなら、その理由を返す。
///
/// 返り値が `Some` の宛先へは、たとえ allowlist に載っていても接続させてはならない。
/// これが SSRF / DNS rebinding に対する最後の砦である。
///
/// # 何を拒否するか（そしてなぜ）
///
/// - **ループバック / 未指定 / ブロードキャスト**: ホスト自身や無効宛先。
/// - **プライベート（RFC1918）/ CGNAT（100.64/10）**: 内部ネットワーク。
/// - **リンクローカル（169.254/16）**: **クラウドメタデータ `169.254.169.254` を含む**。
///   ここが SSRF の最重要ターゲット（IAM 資格情報の窃取）なので、範囲ごと拒否する。
/// - **マルチキャスト / ドキュメント用 / ベンチマーク用 / 予約**: 正当な egress 先ではない。
/// - **IPv6**: ループバック / 未指定 / ユニークローカル（fc00::/7）/ リンクローカル（fe80::/10）/
///   マルチキャスト、および **IPv4-mapped（::ffff:a.b.c.d）は中の IPv4 を取り出して再判定**
///   （マップ経由で上の IPv4 制限をすり抜けさせない）。
///
/// パブリックにルーティング可能な IP だけが `None`（= allowlist 照合へ進んでよい）を返す。
pub fn hard_denied_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => hard_denied_v4(v4),
        IpAddr::V6(v6) => hard_denied_v6(v6),
    }
}

/// [`hard_denied_reason`] の bool 版。
pub fn is_hard_denied(ip: IpAddr) -> bool {
    hard_denied_reason(ip).is_some()
}

fn hard_denied_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, _, _] = ip.octets();

    if ip.is_unspecified() {
        return Some("unspecified (0.0.0.0)");
    }
    if ip.is_loopback() {
        return Some("loopback (127.0.0.0/8)");
    }
    if ip.is_broadcast() {
        return Some("broadcast (255.255.255.255)");
    }
    if ip.is_private() {
        return Some("private (RFC1918)");
    }
    if ip.is_link_local() {
        // 169.254.0.0/16。クラウドメタデータ 169.254.169.254 を含む最重要拒否。
        return Some("link-local incl. cloud metadata (169.254.0.0/16)");
    }
    if ip.is_multicast() {
        return Some("multicast (224.0.0.0/4)");
    }
    if ip.is_documentation() {
        return Some("documentation (192.0.2/198.51.100/203.0.113)");
    }
    // 以下は std が安定 API を持たない範囲。オクテットで判定する。
    // CGNAT / shared address space（100.64.0.0/10）。
    if a == 100 && (64..=127).contains(&b) {
        return Some("shared/CGNAT (100.64.0.0/10)");
    }
    // ベンチマーク（198.18.0.0/15）。
    if a == 198 && (b == 18 || b == 19) {
        return Some("benchmarking (198.18.0.0/15)");
    }
    // IETF プロトコル割当（192.0.0.0/24）。
    if a == 192 && b == 0 && ip.octets()[2] == 0 {
        return Some("IETF protocol assignments (192.0.0.0/24)");
    }
    // 予約（240.0.0.0/4, 実質ルーティング不可）。
    if a >= 240 {
        return Some("reserved (240.0.0.0/4)");
    }
    None
}

fn hard_denied_v6(ip: Ipv6Addr) -> Option<&'static str> {
    // IPv4-mapped（::ffff:a.b.c.d）は中の v4 を取り出して v4 の制限で再判定する。
    // これをしないと、マップ表記で v4 の内部 IP へ到達できてしまう。
    if let Some(v4) = ip.to_ipv4_mapped() {
        return hard_denied_v4(v4);
    }
    // 互換表記（::a.b.c.d、非推奨だが存在しうる）も同様に扱う。
    if let Some(v4) = ip.to_ipv4() {
        // to_ipv4 は mapped と compat の両方を拾うが、mapped は上で処理済み。
        // ループバック ::1 が 0.0.0.1 に化けるのを避けるため、::1 は先に弾く。
        if !ip.is_loopback() && !ip.is_unspecified() {
            if let Some(r) = hard_denied_v4(v4) {
                return Some(r);
            }
        }
    }

    if ip.is_unspecified() {
        return Some("unspecified (::)");
    }
    if ip.is_loopback() {
        return Some("loopback (::1)");
    }
    if ip.is_multicast() {
        return Some("multicast (ff00::/8)");
    }
    let seg0 = ip.segments()[0];
    // ユニークローカル fc00::/7。
    if (seg0 & 0xfe00) == 0xfc00 {
        return Some("unique-local (fc00::/7)");
    }
    // リンクローカル fe80::/10。
    if (seg0 & 0xffc0) == 0xfe80 {
        return Some("link-local (fe80::/10)");
    }
    // ドキュメント用 2001:db8::/32。
    if ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8 {
        return Some("documentation (2001:db8::/32)");
    }
    None
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
            "240.0.0.1", // reserved
            "255.0.0.0",
        ] {
            assert!(is_hard_denied(v4(s)), "must deny {s}");
        }
        for s in [
            "::",
            "::1",
            "fc00::1", // unique-local
            "fd12:3456::1",
            "fe80::1",     // link-local
            "ff02::1",     // multicast
            "2001:db8::1", // documentation
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
            "239.0.0.0",      // ← これは multicast なので実は拒否。下で確認するため除外
        ] {
            if s == "239.0.0.0" {
                assert!(is_hard_denied(v4(s)), "239.0.0.0 は multicast なので拒否");
                continue;
            }
            assert!(!is_hard_denied(v4(s)), "must allow public {s}");
        }
        for s in ["2606:4700:4700::1111", "2001:4860:4860::8888"] {
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
