//! The address check: which resolved addresses the broker dials. It runs on
//! the addresses `connect` is handed, never on a name, so a name that
//! resolves public at policy time and private at dial time (DNS rebinding)
//! gets nothing.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::UpstreamError;

/// Whether `ip` is a public unicast address: none of loopback, private,
/// link-local, CGNAT, documentation, benchmarking or reserved ranges, and
/// for IPv6 none of ULA, site-local or an embedded IPv4 (mapped, NAT64,
/// 6to4) that fails the IPv4 check.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

/// What may be dialed: public addresses, or with `private` (an upstream an
/// approved claim marks private, e.g. a self-hosted Sentry on the LAN) any
/// unicast address. Unspecified, multicast and broadcast never are.
pub fn allowed(ip: IpAddr, private: bool) -> bool {
    if private {
        unicast(ip)
    } else {
        public(ip)
    }
}

/// `addrs` (a resolution, in its order) cut to what `allowed` lets through;
/// none left is a refusal naming the first address dropped.
pub fn checked(addrs: Vec<SocketAddr>, private: bool) -> Result<Vec<SocketAddr>, UpstreamError> {
    let first = addrs.first().copied().ok_or(UpstreamError::Unresolved)?;
    let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| allowed(a.ip(), private)).collect();
    if ok.is_empty() {
        Err(UpstreamError::Refused(first.ip()))
    } else {
        Ok(ok)
    }
}

fn unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast()),
        IpAddr::V6(v6) => match embedded_v4(v6) {
            Some(v4) => unicast(IpAddr::V4(v4)),
            None => !(v6.is_unspecified() || v6.is_multicast()),
        },
    }
}

fn in_v4(ip: Ipv4Addr, net: [u8; 4], len: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - len).unwrap_or(0);
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(net)) & mask
}

fn public_v4(ip: Ipv4Addr) -> bool {
    const REFUSED: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([100, 64, 0, 0], 10),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        // Multicast, then reserved (broadcast included).
        ([224, 0, 0, 0], 4),
        ([240, 0, 0, 0], 4),
    ];
    !REFUSED.iter().any(|&(net, len)| in_v4(ip, net, len))
}

fn in_v6(ip: Ipv6Addr, net: [u16; 8], len: u32) -> bool {
    let mask = u128::MAX.checked_shl(128 - len).unwrap_or(0);
    u128::from(ip) & mask == u128::from(Ipv6Addr::from(net)) & mask
}

/// The IPv4 address an IPv6 one carries where traffic to it ends up at that
/// IPv4 address: mapped (`::ffff:0:0/96`), NAT64 (`64:ff9b::/96`), 6to4
/// (`2002::/16`).
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let o = ip.octets();
    if let Some(v4) = ip.to_ipv4_mapped() {
        Some(v4)
    } else if in_v6(ip, [0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96) {
        Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
    } else if in_v6(ip, [0x2002, 0, 0, 0, 0, 0, 0, 0], 16) {
        Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]))
    } else {
        None
    }
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = embedded_v4(ip) {
        return public_v4(v4);
    }
    const REFUSED: &[([u16; 8], u32)] = &[
        // Unspecified, loopback and the deprecated IPv4-compatible range.
        ([0, 0, 0, 0, 0, 0, 0, 0], 96),
        ([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
        ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
        ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),
        ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),
        ([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10),
        ([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
    ];
    !REFUSED.iter().any(|&(net, len)| in_v6(ip, net, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn v4_ranges() {
        let refused = [
            "0.0.0.0",
            "0.255.255.255",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.0",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
        ];
        for s in refused {
            assert!(!public(ip(s)), "{s} should be refused");
        }
        let public_ = [
            "1.1.1.1",
            "8.8.8.8",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.0.1.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
        ];
        for s in public_ {
            assert!(public(ip(s)), "{s} should be public");
        }
    }

    #[test]
    fn v6_ranges() {
        let refused = [
            "::",
            "::1",
            "::127.0.0.1",
            "::8.8.8.8",
            "100::1",
            "2001:db8::1",
            "fc00::1",
            "fdff:ffff::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "64:ff9b::192.168.0.1",
            "2002:c0a8:0101::1",
            "2002:7f00:0001::",
        ];
        for s in refused {
            assert!(!public(ip(s)), "{s} should be refused");
        }
        let public_ = [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450::1",
            "fbff::1",
            "fe00::1",
            "::ffff:8.8.8.8",
            "64:ff9b::1.1.1.1",
            "2002:0808:0808::1",
            "2001:db9::1",
        ];
        for s in public_ {
            assert!(public(ip(s)), "{s} should be public");
        }
    }

    #[test]
    fn private_allows_unicast_only() {
        for s in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "169.254.1.1", "::1", "fd00::1", "fe80::1", "8.8.8.8"] {
            assert!(allowed(ip(s), true), "{s}");
        }
        for s in ["0.0.0.0", "224.0.0.1", "255.255.255.255", "::", "ff02::1", "::ffff:0.0.0.0", "::ffff:224.0.0.1"] {
            assert!(!allowed(ip(s), true), "{s}");
            assert!(!allowed(ip(s), false), "{s}");
        }
        assert!(!allowed(ip("127.0.0.1"), false));
        assert!(allowed(ip("8.8.8.8"), false));
    }

    #[test]
    fn checked_filters_in_order() {
        let a = |s: &str| SocketAddr::new(ip(s), 443);
        assert_eq!(
            checked(vec![a("10.0.0.1"), a("8.8.8.8"), a("::1"), a("1.1.1.1")], false).unwrap(),
            vec![a("8.8.8.8"), a("1.1.1.1")]
        );
        assert_eq!(
            checked(vec![a("::1"), a("127.0.0.1")], false).unwrap_err(),
            UpstreamError::Refused(ip("::1"))
        );
        assert_eq!(checked(vec![a("::1")], true).unwrap(), vec![a("::1")]);
        assert_eq!(checked(vec![], true).unwrap_err(), UpstreamError::Unresolved);
    }
}
