//! Teredo addresses.
//!
//! The console often offers a Teredo IPv6 address. Its real public IPv4
//! address and UDP port are hidden inside it (stored inverted). Greenlight
//! digs them out and adds them as extra candidates, and so do we.
//! Ported from Greenlight's `teredo.ts` (MIT, UnknownSKL).

use std::net::{Ipv4Addr, Ipv6Addr};

/// The public IPv4 address and UDP port hidden in a Teredo address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TeredoClient {
    pub ipv4: Ipv4Addr,
    pub port: u16,
}

/// Returns the hidden client address if `address` is a Teredo address (2001:0::/32).
pub fn parse(address: &str) -> Option<TeredoClient> {
    let ip: Ipv6Addr = address.parse().ok()?;
    let s = ip.segments();
    if s[0] != 0x2001 || s[1] != 0 {
        return None;
    }
    let port = !s[5];
    let hi = (!s[6]).to_be_bytes();
    let lo = (!s[7]).to_be_bytes();
    Some(TeredoClient {
        ipv4: Ipv4Addr::new(hi[0], hi[1], lo[0], lo[1]),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4380_example() {
        // The worked example from RFC 4380: client 192.0.2.45, port 40000.
        let client = parse("2001:0000:4136:e378:8000:63bf:3fff:fdd2").unwrap();
        assert_eq!(client.ipv4, Ipv4Addr::new(192, 0, 2, 45));
        assert_eq!(client.port, 40000);
    }

    #[test]
    fn short_form_address() {
        let client = parse("2001:0:284a:364:1c05:25cb:a3fd:9c10").unwrap();
        // !0xa3fd = 0x5c02, !0x9c10 = 0x63ef, !0x25cb = 0xda34
        assert_eq!(client.ipv4, Ipv4Addr::new(0x5c, 0x02, 0x63, 0xef));
        assert_eq!(client.port, 0xda34);
    }

    #[test]
    fn other_addresses_are_not_teredo() {
        assert_eq!(parse("192.168.1.20"), None);
        assert_eq!(parse("fe80::1"), None);
        assert_eq!(parse("2001:db8::1"), None);
        assert_eq!(parse("not an address"), None);
    }
}
