//! Client-IP resolution for PIN rate limiting.
//!
//! The legacy backend trusts `CF-Connecting-IP` / `X-Forwarded-For`
//! unconditionally (`backend/ip-utils.js`). Behind a public tunnel that is
//! spoofable, so here the headers are honoured **only** when the TCP peer is a
//! private/loopback address (docker bridge, cloudflared sidecar, LAN/Tailscale);
//! otherwise the peer address is the rate-limit key (plan §5.5 "trusted proxy").
//!
//! Requests without connect info (unit/oneshot tests) map to `"unknown"`;
//! production always serves through `into_make_service_with_connect_info`.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderMap;

/// Rate-limit key used when no address can be resolved.
pub const UNKNOWN_IP: &str = "unknown";

/// Resolves the client IP for an HTTP request.
pub fn from_request(request: &Request) -> String {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0);
    from_parts(peer, request.headers())
}

/// Pure resolution from an optional peer and request headers (unit-tested).
pub fn from_parts(peer: Option<SocketAddr>, headers: &HeaderMap) -> String {
    match peer {
        Some(peer) => {
            if is_trusted_proxy(peer.ip()) {
                forwarded(headers).unwrap_or_else(|| peer.ip().to_string())
            } else {
                peer.ip().to_string()
            }
        }
        None => UNKNOWN_IP.to_owned(),
    }
}

/// `CF-Connecting-IP`, then the first `X-Forwarded-For` entry.
fn forwarded(headers: &HeaderMap) -> Option<String> {
    for name in ["cf-connecting-ip", "x-forwarded-for"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            let candidate = value.split(',').next().unwrap_or_default().trim();
            if !candidate.is_empty() {
                return Some(candidate.to_owned());
            }
        }
    }
    None
}

/// Loopback, RFC 1918, link-local and IPv6 unique-local peers are proxies.
pub fn is_trusted_proxy(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            address.is_loopback() || address.is_private() || address.is_link_local()
        }
        IpAddr::V6(address) => {
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer(ip: &str) -> Option<SocketAddr> {
        Some(SocketAddr::new(ip.parse().expect("ip"), 12345))
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn no_connect_info_maps_to_unknown() {
        assert_eq!(from_parts(None, &HeaderMap::new()), UNKNOWN_IP);
        // Forwarded headers without a peer are ignored (fail closed).
        assert_eq!(
            from_parts(None, &headers(&[("x-forwarded-for", "203.0.113.7")])),
            UNKNOWN_IP
        );
    }

    #[test]
    fn public_peers_are_used_directly_and_headers_ignored() {
        let resolved = from_parts(
            peer("198.51.100.20"),
            &headers(&[
                ("cf-connecting-ip", "203.0.113.7"),
                ("x-forwarded-for", "203.0.113.8"),
            ]),
        );
        assert_eq!(resolved, "198.51.100.20");
    }

    #[test]
    fn trusted_peers_honour_cf_then_forwarded_for() {
        let both = headers(&[
            ("cf-connecting-ip", "203.0.113.7"),
            ("x-forwarded-for", "198.51.100.1, 10.0.0.1"),
        ]);
        assert_eq!(from_parts(peer("127.0.0.1"), &both), "203.0.113.7");
        assert_eq!(from_parts(peer("172.17.0.2"), &both), "203.0.113.7");

        let only_xff = headers(&[("x-forwarded-for", "198.51.100.1, 10.0.0.1")]);
        assert_eq!(from_parts(peer("127.0.0.1"), &only_xff), "198.51.100.1");
        assert_eq!(from_parts(peer("::1"), &only_xff), "198.51.100.1");
    }

    #[test]
    fn trusted_peer_without_headers_uses_its_own_address() {
        assert_eq!(
            from_parts(peer("192.168.100.20"), &HeaderMap::new()),
            "192.168.100.20"
        );
    }

    #[test]
    fn blank_forwarded_values_fall_back_to_the_peer() {
        let headers = headers(&[("cf-connecting-ip", "   "), ("x-forwarded-for", "")]);
        assert_eq!(from_parts(peer("127.0.0.1"), &headers), "127.0.0.1");
    }

    #[test]
    fn trusted_proxy_ranges_are_private_and_loopback_only() {
        for trusted in [
            "127.0.0.1",
            "::1",
            "10.1.2.3",
            "172.16.0.5",
            "192.168.1.1",
            "169.254.1.1",
            "fd00::1",
        ] {
            assert!(is_trusted_proxy(trusted.parse().expect("ip")), "{trusted}");
        }
        for untrusted in ["8.8.8.8", "198.51.100.20", "2001:db8::1"] {
            assert!(
                !is_trusted_proxy(untrusted.parse().expect("ip")),
                "{untrusted}"
            );
        }
    }
}
