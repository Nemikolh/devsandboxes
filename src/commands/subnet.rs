//! IPv4 subnet checks for service networks (docs/architecture/instances/services.md,
//! *Networks*). A runtime picks a new network's subnet from its own pools,
//! and a rootless one can't see the host's routes: docker happily hands out
//! `192.168.0.0/20` on a `192.168.1.0/24` LAN, and the empty bridge's route
//! then swallows the host's gateway and DNS for every container. So the
//! host's view is read here, on the host, and compared after create.

use std::fmt;

/// A /24 of this range replaces a network whose subnet overlaps the host.
pub const FALLBACK: Net = Net { addr: 0x0AC8_0000, prefix: 16 }; // 10.200.0.0/16
const FALLBACK_PREFIX: u8 = 24;

/// An IPv4 network, address masked to its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Net {
    addr: u32,
    prefix: u8,
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) }
}

impl Net {
    fn new(addr: u32, prefix: u8) -> Net {
        Net { addr: addr & mask(prefix), prefix }
    }

    /// `a.b.c.d/n`, or a bare address as a /32. `None` for IPv6 and garbage.
    pub fn parse(s: &str) -> Option<Net> {
        let (ip, prefix) = match s.trim().split_once('/') {
            Some((ip, p)) => (ip, p.parse().ok().filter(|p| *p <= 32)?),
            None => (s.trim(), 32),
        };
        let ip: std::net::Ipv4Addr = ip.parse().ok()?;
        Some(Net::new(ip.into(), prefix))
    }

    pub fn overlaps(self, other: Net) -> bool {
        let m = mask(self.prefix.min(other.prefix));
        self.addr & m == other.addr & m
    }
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ip = std::net::Ipv4Addr::from(self.addr);
        if self.prefix == 32 { write!(f, "{ip}") } else { write!(f, "{ip}/{}", self.prefix) }
    }
}

/// Something the host reaches that a network must not cover.
#[derive(Clone, Debug, PartialEq)]
pub struct HostNet {
    pub net: Net,
    kind: Kind,
    /// Interface or file it came from, for the warning.
    via: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Route,
    Gateway,
    Nameserver,
}

impl fmt::Display for HostNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::Route => "route",
            Kind::Gateway => "gateway",
            Kind::Nameserver => "nameserver",
        };
        write!(f, "{kind} {} ({})", self.net, self.via)
    }
}

/// `/proc/net/route` (Linux, main table): every up route but the default,
/// and every gateway. Addresses are hex of the raw network-order bytes.
fn parse_proc_route(text: &str) -> Vec<HostNet> {
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().map(|v| u32::from_be_bytes(v.to_ne_bytes()));
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let [iface, dest, gateway, flags, _, _, _, netmask, ..] = cols[..] else {
            continue;
        };
        let (Some(dest), Some(gateway), Some(flags), Some(netmask)) =
            (hex(dest), hex(gateway), u32::from_str_radix(flags, 16).ok(), hex(netmask))
        else {
            continue;
        };
        if flags & 0x1 == 0 {
            continue; // RTF_UP
        }
        let via = iface.to_string();
        let prefix = netmask.leading_ones() as u8;
        if prefix > 0 {
            out.push(HostNet { net: Net::new(dest, prefix), kind: Kind::Route, via: via.clone() });
        }
        if gateway != 0 {
            out.push(HostNet { net: Net::new(gateway, 32), kind: Kind::Gateway, via });
        }
    }
    out
}

/// `nameserver` lines of a resolv.conf, IPv4, loopback ones (a local stub
/// like systemd-resolved's `127.0.0.53`) left out: no bridge covers them.
fn parse_resolv(text: &str, source: &str) -> Vec<HostNet> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|ip| Net::parse(ip.trim()))
        .filter(|n| n.addr >> 24 != 127)
        .map(|net| HostNet { net, kind: Kind::Nameserver, via: source.to_string() })
        .collect()
}

/// What this host reaches: its routes and gateways (Linux only) and its
/// nameservers, systemd-resolved's upstream list included (docker copies
/// that one when the stub is in `/etc/resolv.conf`). Unreadable files add
/// nothing.
pub fn host_nets() -> Vec<HostNet> {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let mut out = parse_proc_route(&read("/proc/net/route"));
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        for ns in parse_resolv(&read(path), path) {
            if !out.iter().any(|h| h.kind == Kind::Nameserver && h.net == ns.net) {
                out.push(ns);
            }
        }
    }
    out
}

/// The first host entry `subnet` covers or overlaps. `created`: `subnet` is
/// an existing network's, whose own bridge route (rootful runtimes add it
/// to the host) equals it exactly and isn't a conflict.
pub fn conflict(subnet: Net, host: &[HostNet], created: bool) -> Option<&HostNet> {
    host.iter()
        .filter(|h| !(created && h.kind == Kind::Route && h.net == subnet))
        .find(|h| h.net.overlaps(subnet))
}

/// The /24s of [`FALLBACK`] clear of `taken` (the runtime's networks) and
/// of the host, in order.
pub fn free_subnets<'a>(taken: &'a [Net], host: &'a [HostNet]) -> impl Iterator<Item = Net> + 'a {
    let count = 1u32 << (FALLBACK_PREFIX - FALLBACK.prefix);
    (0..count)
        .map(|i| Net::new(FALLBACK.addr + (i << (32 - FALLBACK_PREFIX)), FALLBACK_PREFIX))
        .filter(|n| !taken.iter().any(|t| t.overlaps(*n)) && conflict(*n, host, false).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Net {
        Net::parse(s).unwrap()
    }

    #[test]
    fn parse_masks_and_rejects() {
        assert_eq!(net("192.168.1.7/24").to_string(), "192.168.1.0/24");
        assert_eq!(net("10.0.0.1").to_string(), "10.0.0.1");
        assert_eq!(net("0.0.0.0/0").to_string(), "0.0.0.0/0");
        for bad in ["fd00::/64", "10.0.0.0/33", "x", "10.0.0/8", ""] {
            assert_eq!(Net::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn overlap_is_containment_either_way() {
        assert!(net("192.168.0.0/20").overlaps(net("192.168.1.0/24")));
        assert!(net("192.168.1.0/24").overlaps(net("192.168.0.0/20")));
        assert!(net("192.168.0.0/20").overlaps(net("192.168.1.1")));
        assert!(!net("192.168.16.0/20").overlaps(net("192.168.1.0/24")));
        assert!(!net("172.18.0.0/16").overlaps(net("172.17.0.0/16")));
    }

    /// The report's host, as the kernel prints it on little-endian.
    const ROUTE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eno2\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eno2\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
br-2ade\t0000A8C0\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0
down0\t00000A0A\t00000000\t0000\t0\t0\t0\t0000FFFF\t0\t0\t0
";

    #[test]
    #[cfg(target_endian = "little")]
    fn proc_route_lists_routes_and_gateways() {
        let got: Vec<String> = parse_proc_route(ROUTE).iter().map(ToString::to_string).collect();
        assert_eq!(
            got,
            [
                "gateway 192.168.1.1 (eno2)",
                "route 192.168.1.0/24 (eno2)",
                "route 192.168.0.0/20 (br-2ade)",
            ]
        );
    }

    #[test]
    fn resolv_keeps_ipv4_non_loopback_nameservers() {
        let text = "# c\nnameserver 127.0.0.53\nnameserver 192.168.1.1\nnameserver fe80::1\nsearch lan\n";
        let got = parse_resolv(text, "/etc/resolv.conf");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].to_string(), "nameserver 192.168.1.1 (/etc/resolv.conf)");
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn conflict_finds_the_lan_but_not_a_networks_own_route() {
        let host = parse_proc_route(ROUTE);
        let bad = net("192.168.0.0/20");
        // Its own bridge route is skipped; the LAN's gateway is not.
        assert_eq!(conflict(bad, &host, true).unwrap().to_string(), "gateway 192.168.1.1 (eno2)");
        let own: Vec<HostNet> = host.iter().filter(|h| h.via == "br-2ade").cloned().collect();
        assert_eq!(conflict(bad, &own, true), None);
        assert_eq!(conflict(bad, &own, false).unwrap().to_string(), "route 192.168.0.0/20 (br-2ade)");
        assert_eq!(conflict(net("192.168.16.0/20"), &host, true), None);
        assert_eq!(conflict(net("172.20.0.0/16"), &host, true), None);
    }

    #[test]
    fn free_subnets_skip_taken_and_host() {
        let host = parse_resolv("nameserver 10.200.1.53\n", "r");
        let taken = [net("10.200.0.0/24"), net("10.200.2.0/23")];
        let got: Vec<String> = free_subnets(&taken, &host).take(2).map(|n| n.to_string()).collect();
        assert_eq!(got, ["10.200.4.0/24", "10.200.5.0/24"]);
        assert_eq!(free_subnets(&[net("10.0.0.0/8")], &[]).count(), 0);
        assert_eq!(free_subnets(&[], &[]).count(), 256);
        assert_eq!(free_subnets(&[], &[]).last().unwrap().to_string(), "10.200.255.0/24");
    }
}
