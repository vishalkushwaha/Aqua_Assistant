//! Which network addresses are not ordinary public internet addresses.
//!
//! One list for every tool that decides where a request or a browser may go: the HTTP tools (`aqua-native-tools`, `http::policy`) and the browser's site policy
//! (`aqua-mcp-adapters`, `browser::policy`) both ask it, so a name or a number that one refuses is refused by the other (audit item SE-09: the browser had its own, shorter copy).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Anything that is not an ordinary public internet address: this computer, a private or link-local network, shared (carrier NAT) space, multicast, reserved and documentation
/// ranges, the unspecified address, and IPv6 forms that carry such an IPv4 address (IPv4-mapped, NAT64, 6to4).
pub fn is_non_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => non_public_v4(v),
        IpAddr::V6(v) => non_public_v6(v),
    }
}

/// This computer itself: a loopback address or the unspecified address (`0.0.0.0`, `::`), also written as an IPv4-mapped IPv6 address.
pub fn is_loopback_or_unspecified(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_unspecified(),
        IpAddr::V6(v) => v.is_loopback() || v.is_unspecified() || v.to_ipv4_mapped().map(|m| m.is_loopback() || m.is_unspecified()).unwrap_or(false),
    }
}

fn non_public_v4(v: Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_loopback()
        || v.is_private()
        || v.is_link_local()
        || v.is_unspecified()
        || v.is_broadcast()
        || v.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 carrier-grade NAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 protocol assignments
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // documentation
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18.0.0/15 benchmarking
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || o[0] >= 240 // reserved
}

fn non_public_v6(v: Ipv6Addr) -> bool {
    if let Some(m) = v.to_ipv4_mapped() {
        return non_public_v4(m);
    }
    let s = v.segments();
    // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) embed an IPv4 address: judge that address.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
        return non_public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    if s[0] == 0x2002 {
        return non_public_v4(Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8));
    }
    v.is_loopback()
        || v.is_unspecified()
        || v.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0100 && s[1..4].iter().all(|x| *x == 0)) // discard prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_computer_and_private_networks_are_not_public_in_any_spelling() {
        for ip in [
            "127.0.0.1", "10.0.0.1", "172.16.5.5", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "224.0.0.1", "240.0.0.1", "203.0.113.9", "::1", "::", "fe80::1", "fc00::1", "ff02::1",
            "::ffff:127.0.0.1", "::ffff:10.0.0.1", "64:ff9b::7f00:1", "2002:a9fe:a9fe::1",
        ] {
            assert!(is_non_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "93.184.216.34", "172.32.0.1", "100.128.0.1", "2606:4700:4700::1111", "::ffff:8.8.8.8", "64:ff9b::808:808"] {
            assert!(!is_non_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn only_this_computer_is_loopback_or_unspecified() {
        for ip in ["127.0.0.1", "127.9.9.9", "0.0.0.0", "::1", "::", "::ffff:127.0.0.1", "::ffff:0.0.0.0"] {
            assert!(is_loopback_or_unspecified(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["10.0.0.1", "192.168.1.1", "8.8.8.8", "fe80::1", "2606:4700:4700::1111"] {
            assert!(!is_loopback_or_unspecified(ip.parse().unwrap()), "{ip}");
        }
    }
}
