//! Which address to hand a client for the village, usercomm, the referee
//! and the matches other players host.
//!
//! The client connects to whatever `ip` the lobby lists, so the lobby must
//! name an address that works from where that client is:
//!
//! - A client on the server's own network (loopback, a LAN address, a
//!   virtual LAN such as Tailscale's 100.64.0.0/10) gets the address it
//!   reached the lobby on.
//! - A client from the internet gets `--public-host` when the server is
//!   behind a router (the address it reached the lobby on is then the
//!   server's LAN address), else that address too.
//!
//! A match host is listed with the address the lobby sees it on. A host on
//! the server's machine or LAN shows up with a loopback or LAN address that
//! players elsewhere cannot use; they get the server's address instead (the
//! router forwards the match port to the host, see docs/PLAYING-ONLINE.md).

use std::net::IpAddr;

/// On the server's side of the router: loopback, private, link-local, or
/// a carrier-grade NAT / Tailscale address (100.64.0.0/10).
pub fn is_near(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Loopback or an RFC 1918 / link-local address: what a host behind the
/// server's own router looks like (a virtual LAN address is not one).
fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// The server's address for a client at `peer` that reached it on `local`.
pub fn server_for(peer: IpAddr, local: IpAddr, public: Option<IpAddr>) -> IpAddr {
    if is_near(peer) {
        local
    } else {
        public.unwrap_or(local)
    }
}

/// The address an observer at `observer` should use for a match host the
/// lobby sees at `host`; `server` is the server's address for that
/// observer (`server_for`).
pub fn host_for(host: IpAddr, observer: IpAddr, server: IpAddr, public: Option<IpAddr>) -> IpAddr {
    if host.is_loopback() && !observer.is_loopback() {
        // The host plays on the server's machine.
        return server;
    }
    if is_lan(host) && !is_near(observer) {
        // The host is on the server's LAN; only the server's router can
        // reach it from outside.
        return public.unwrap_or(host);
    }
    host
}

/// A match address a player sets with `!adresse`: an IPv4 address,
/// optionally with a port (`100.101.1.2`, `203.0.113.7:24680`). The client
/// takes only addresses, not names.
pub fn parse_match_address(text: &str) -> Option<(String, Option<u16>)> {
    let (ip, port) = match text.split_once(':') {
        Some((ip, port)) => (ip, Some(port.parse::<u16>().ok().filter(|&p| p != 0)?)),
        None => (text, None),
    };
    let ip: std::net::Ipv4Addr = ip.parse().ok()?;
    (!ip.is_unspecified() && !ip.is_broadcast()).then(|| (ip.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn near_addresses() {
        for near in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.178.20", "169.254.1.1", "100.101.102.103", "::1", "fd7a:115c::1"] {
            assert!(is_near(ip(near)), "{near}");
        }
        for far in ["8.8.8.8", "100.128.0.1", "172.32.0.1", "2001:db8::1"] {
            assert!(!is_near(ip(far)), "{far}");
        }
    }

    #[test]
    fn clients_get_the_address_that_works_from_where_they_are() {
        let public = Some(ip("203.0.113.7"));
        assert_eq!(server_for(ip("127.0.0.1"), ip("127.0.0.1"), public), ip("127.0.0.1"));
        assert_eq!(server_for(ip("192.168.1.20"), ip("192.168.1.10"), public), ip("192.168.1.10"));
        assert_eq!(server_for(ip("100.90.1.2"), ip("100.64.0.5"), public), ip("100.64.0.5"));
        assert_eq!(server_for(ip("198.51.100.9"), ip("192.168.1.10"), public), ip("203.0.113.7"));
        // A server with an address of its own (a rented one) needs no --public-host.
        assert_eq!(server_for(ip("198.51.100.9"), ip("203.0.113.7"), None), ip("203.0.113.7"));
    }

    #[test]
    fn match_addresses_players_set() {
        assert_eq!(parse_match_address("100.101.1.2"), Some(("100.101.1.2".into(), None)));
        assert_eq!(parse_match_address("203.0.113.7:24680"), Some(("203.0.113.7".into(), Some(24680))));
        for bad in ["", "0.0.0.0", "example.org", "1.2.3.4:", "1.2.3.4:0", "1.2.3.4:99999", "::1"] {
            assert_eq!(parse_match_address(bad), None, "{bad}");
        }
    }

    #[test]
    fn match_hosts_behind_the_servers_router_are_listed_with_its_address() {
        let public = Some(ip("203.0.113.7"));
        let far = ip("198.51.100.9");
        // Host on the server's machine.
        assert_eq!(host_for(ip("127.0.0.1"), ip("127.0.0.1"), ip("127.0.0.1"), public), ip("127.0.0.1"));
        assert_eq!(host_for(ip("127.0.0.1"), ip("192.168.1.20"), ip("192.168.1.10"), public), ip("192.168.1.10"));
        assert_eq!(host_for(ip("127.0.0.1"), far, ip("203.0.113.7"), public), ip("203.0.113.7"));
        // Host on the server's LAN.
        assert_eq!(host_for(ip("192.168.1.20"), ip("192.168.1.30"), ip("192.168.1.10"), public), ip("192.168.1.20"));
        assert_eq!(host_for(ip("192.168.1.20"), far, ip("203.0.113.7"), public), ip("203.0.113.7"));
        // Hosts elsewhere and on a virtual LAN keep their address.
        assert_eq!(host_for(far, ip("192.168.1.20"), ip("192.168.1.10"), public), far);
        assert_eq!(host_for(ip("100.90.1.2"), ip("100.90.1.3"), ip("100.64.0.5"), public), ip("100.90.1.2"));
    }
}
