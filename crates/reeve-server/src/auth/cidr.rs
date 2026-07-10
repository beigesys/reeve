//! Tiny CIDR matcher for proxy-mode trusted peers (docs/decisions/auth.md
//! D1). Hand-rolled (~60 lines) rather than a dependency — the need is
//! exactly "is this peer inside these prefixes".

use std::net::IpAddr;
use std::str::FromStr;

/// One CIDR prefix, e.g. `10.0.0.0/8`, `127.0.0.1` (implicit /32),
/// `fd00::/8`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid CIDR {0:?}")]
pub struct CidrParseError(String);

impl FromStr for Cidr {
    type Err = CidrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || CidrParseError(s.to_string());
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, p)) => {
                let addr: IpAddr = ip.parse().map_err(|_| err())?;
                let prefix: u8 = p.parse().map_err(|_| err())?;
                (addr, prefix)
            }
            None => {
                let addr: IpAddr = s.parse().map_err(|_| err())?;
                let full = if addr.is_ipv4() { 32 } else { 128 };
                (addr, full)
            }
        };
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(err());
        }
        Ok(Cidr { addr: ip, prefix })
    }
}

impl Cidr {
    /// Does `ip` fall inside this prefix? IPv4-mapped IPv6 addresses
    /// (`::ffff:a.b.c.d` — what a dual-stack listener reports) are
    /// canonicalized to IPv4 first; otherwise families must match.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr.to_canonical(), ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                (u128::from(net) & mask) == (u128::from(ip) & mask)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn v4_prefix_match() {
        let c: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(c.contains(ip("10.1.2.3")));
        assert!(!c.contains(ip("11.0.0.1")));
    }

    #[test]
    fn bare_ip_is_host_prefix() {
        let c: Cidr = "127.0.0.1".parse().unwrap();
        assert!(c.contains(ip("127.0.0.1")));
        assert!(!c.contains(ip("127.0.0.2")));
    }

    #[test]
    fn v6_prefix_and_mapped_v4() {
        let c6: Cidr = "fd00::/8".parse().unwrap();
        assert!(c6.contains(ip("fd12::1")));
        assert!(!c6.contains(ip("fe80::1")));
        // dual-stack listener reports ::ffff:10.0.0.5 for a v4 peer
        let c4: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(c4.contains(ip("::ffff:10.0.0.5")));
    }

    #[test]
    fn zero_prefix_matches_everything_in_family() {
        let c: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(c.contains(ip("203.0.113.9")));
        assert!(!c.contains(ip("fd00::1")), "family mismatch never matches");
    }

    #[test]
    fn rejects_garbage() {
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
        assert!("not-an-ip".parse::<Cidr>().is_err());
        assert!("10.0.0.0/x".parse::<Cidr>().is_err());
    }
}
