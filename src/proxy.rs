//! The address of the client behind trusted reverse proxies.
//!
//! Behind a proxy every connection comes from the proxy, so failed logins
//! of one client would lock out everyone. A proxy that the configuration
//! trusts names the client in `X-Forwarded-For`; any other caller can write
//! that header, so it is read only for connections from a trusted proxy.

use std::net::IpAddr;
use std::str::FromStr;

/// An address range such as `10.0.0.0/8`; a bare address is one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                same_prefix(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                same_prefix(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

/// Whether the first `prefix` bits of `a` and `b` are equal.
fn same_prefix(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    let rest = prefix % 8;
    if a[..full] != b[..full] {
        return false;
    }
    rest == 0 || (a[full] ^ b[full]) >> (8 - rest) == 0
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let addr: IpAddr = addr
            .parse()
            .map_err(|_| format!("{text} is not an address or a range"))?;
        let addr = addr.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|bits| *bits <= max)
                .ok_or_else(|| format!("{text} has an invalid prefix length"))?,
        };
        Ok(Self { addr, prefix })
    }
}

/// Reads ranges separated by commas or spaces.
pub fn parse_list(text: &str) -> Result<Vec<Cidr>, String> {
    text.split([',', ' '])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::parse)
        .collect()
}

/// The client of a connection from `peer`. When `peer` is a trusted proxy,
/// `X-Forwarded-For` is read from the right: each proxy appends the address
/// it got the request from, and the first address that is not a trusted
/// proxy is the client. A value that is not an address stops the walk at
/// the last trusted proxy, because the text left of it is not checked.
pub fn client_ip<'a>(
    peer: IpAddr,
    forwarded_for: impl Iterator<Item = &'a str>,
    trusted: &[Cidr],
) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|range| range.contains(ip));
    let mut client = peer.to_canonical();
    if !is_trusted(client) {
        return client;
    }
    let chain: Vec<&str> = forwarded_for.flat_map(|value| value.split(',')).collect();
    for entry in chain.iter().rev() {
        let Ok(ip) = entry.trim().parse::<IpAddr>() else {
            break;
        };
        client = ip.to_canonical();
        if !is_trusted(client) {
            break;
        }
    }
    client
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn ranges_parse_and_match() {
        let list = parse_list("10.0.0.0/8, 192.168.1.5 fd00::/8").unwrap();
        assert!(list[0].contains(ip("10.200.3.4")));
        assert!(!list[0].contains(ip("11.0.0.1")));
        assert!(list[1].contains(ip("192.168.1.5")));
        assert!(!list[1].contains(ip("192.168.1.6")));
        assert!(list[2].contains(ip("fd12::1")));
        // An IPv4 address seen through a dual-stack socket.
        assert!(list[0].contains(ip("::ffff:10.0.0.1")));
        assert!(
            "172.16.0.0/12"
                .parse::<Cidr>()
                .unwrap()
                .contains(ip("172.31.255.1"))
        );
        assert!(
            !"172.16.0.0/12"
                .parse::<Cidr>()
                .unwrap()
                .contains(ip("172.32.0.1"))
        );
        assert!("0.0.0.0/0".parse::<Cidr>().unwrap().contains(ip("8.8.8.8")));
        for bad in ["10.0.0.0/33", "proxy", "10.0.0.0/x", "::/129"] {
            assert!(parse_list(bad).is_err(), "{bad}");
        }
        assert!(parse_list("").unwrap().is_empty());
    }

    #[test]
    fn only_a_trusted_proxy_names_the_client() {
        let trusted = parse_list("10.0.0.0/8").unwrap();
        let header = |value: &'static str| std::iter::once(value);
        // A direct client cannot pick its address with the header.
        assert_eq!(
            client_ip(ip("203.0.113.9"), header("198.51.100.1"), &trusted),
            ip("203.0.113.9")
        );
        // Through the proxy: the rightmost address that is not a proxy.
        assert_eq!(
            client_ip(
                ip("10.0.0.2"),
                header("1.2.3.4, 198.51.100.1, 10.0.0.3"),
                &trusted
            ),
            ip("198.51.100.1")
        );
        // Several header lines form one list.
        let lines = ["1.2.3.4", "198.51.100.7"];
        assert_eq!(
            client_ip(ip("10.0.0.2"), lines.into_iter(), &trusted),
            ip("198.51.100.7")
        );
        // Without the header, or with garbage, the proxy is the client.
        assert_eq!(
            client_ip(ip("10.0.0.2"), std::iter::empty(), &trusted),
            ip("10.0.0.2")
        );
        assert_eq!(
            client_ip(ip("10.0.0.2"), header("1.2.3.4, unknown"), &trusted),
            ip("10.0.0.2")
        );
        // Nothing trusted: the header never counts.
        assert_eq!(
            client_ip(ip("10.0.0.2"), header("198.51.100.1"), &[]),
            ip("10.0.0.2")
        );
    }
}
