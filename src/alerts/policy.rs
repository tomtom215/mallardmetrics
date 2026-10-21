//! Where an outbound notification may go.
//!
//! The server may sit inside a private network, so a webhook URL is a request
//! to make a connection from a privileged position (SSRF). This module decides
//! which destinations are acceptable. It is pure: no I/O, so every rule is
//! table-tested.
//!
//! Nothing here ever echoes the URL back in an error. A webhook URL commonly
//! carries a token in its path (Slack, Discord), so the messages describe the
//! problem and the logs name only `scheme://host`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why `ip` must not be connected to, or `None` when it is an ordinary public
/// address.
///
/// Refuses loopback, private (RFC 1918), link-local (which includes the
/// 169.254.169.254 metadata endpoint of AWS, GCP, Azure and Oracle), CGNAT
/// (which includes Alibaba's 100.100.100.200), multicast, unspecified,
/// broadcast, reserved and benchmarking ranges, IPv6 unique-local space
/// (`fc00::/7`, which contains AWS's `fd00:ec2::254`), and IPv6 forms that
/// embed an IPv4 address (mapped, NAT64, 6to4), judged by the address inside.
///
/// Documentation ranges (192.0.2.0/24, 2001:db8::/32 and friends) are not
/// refused: they are not routable on the internet, and the tests use them as
/// stand-ins for public addresses.
pub fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

fn blocked_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, c, _] = ip.octets();
    if ip == Ipv4Addr::new(169, 254, 169, 254) {
        return Some("cloud metadata address");
    }
    if ip == Ipv4Addr::new(168, 63, 129, 16) {
        return Some("cloud metadata address (Azure wire server)");
    }
    match (a, b, c) {
        (0, _, _) => Some("unspecified or this-network address"),
        (10, _, _) | (192, 168, _) | (172, 16..=31, _) => Some("private address"),
        (127, _, _) => Some("loopback address"),
        (169, 254, _) => Some("link-local address"),
        (100, 64..=127, _) => Some("carrier-grade NAT address"),
        (224..=239, _, _) => Some("multicast address"),
        (240..=255, _, _) => Some("reserved or broadcast address"),
        (192, 0, 0) => Some("IETF protocol assignment address"),
        (198, 18..=19, _) => Some("benchmarking address"),
        _ => None,
    }
}

fn blocked_v6(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_unspecified() {
        return Some("unspecified address");
    }
    if ip.is_loopback() {
        return Some("loopback address");
    }
    let seg = ip.segments();
    // ::ffff:a.b.c.d is an IPv4 address in IPv6 clothes.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return blocked_v4(v4);
    }
    // ::a.b.c.d (deprecated IPv4-compatible): never a legitimate target.
    if seg[..6].iter().all(|&x| x == 0) {
        return Some("IPv4-compatible address");
    }
    let embedded = |hi: u16, lo: u16| {
        let (h, l) = (hi.to_be_bytes(), lo.to_be_bytes());
        Ipv4Addr::new(h[0], h[1], l[0], l[1])
    };
    // 64:ff9b::/96 (NAT64): judged by the IPv4 address it carries.
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return blocked_v4(embedded(seg[6], seg[7]));
    }
    if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2] == 1 {
        return Some("local-use NAT64 address");
    }
    // 2002::/16 (6to4): judged by the embedded IPv4 address.
    if seg[0] == 0x2002 {
        return blocked_v4(embedded(seg[1], seg[2]));
    }
    if seg[0] == 0x2001 && seg[1] == 0 {
        return Some("Teredo address");
    }
    if seg[0] == 0x100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 {
        return Some("discard-only address");
    }
    match seg[0] {
        x if x & 0xffc0 == 0xfe80 => Some("link-local address"),
        x if x & 0xfe00 == 0xfc00 => {
            Some("unique-local address (includes the IPv6 metadata endpoints)")
        }
        x if x & 0xff00 == 0xff00 => Some("multicast address"),
        _ => None,
    }
}

/// How a hostname in a webhook URL is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// A registered name, lower-cased; resolved at delivery time.
    Name(String),
    /// A literal address.
    Ip(IpAddr),
}

/// A parsed webhook URL.
///
/// `Debug` and `Display` show `scheme://host` only: the path and query may be
/// a bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub https: bool,
    pub host: Host,
    pub port: u16,
    /// Path and query, always starting with `/`. Secret-bearing; never logged.
    pub path: String,
}

impl Target {
    /// Parse and check a webhook URL.
    ///
    /// # Errors
    ///
    /// Describes what is wrong without repeating the URL.
    pub fn parse(url: &str) -> Result<Self, String> {
        let https = if url.starts_with("https://") {
            true
        } else if url.starts_with("http://") {
            false
        } else {
            return Err(
                "the webhook URL must start with https:// (or http://, which also \
                        needs allow_http_webhook)"
                    .to_string(),
            );
        };
        let uri: axum::http::Uri = url
            .parse()
            .map_err(|_| "the webhook URL is not a valid URL".to_string())?;
        let authority = uri
            .authority()
            .ok_or_else(|| "the webhook URL has no host".to_string())?;
        if authority.as_str().contains('@') {
            return Err(
                "the webhook URL must not contain credentials (user:password@); \
                        they would be sent in clear and written to logs. Put the secret in \
                        MALLARD_ALERTS_WEBHOOK_SECRET"
                    .to_string(),
            );
        }
        let raw_host = authority.host();
        let host = if let Some(inner) = raw_host.strip_prefix('[').and_then(|h| h.strip_suffix(']'))
        {
            Host::Ip(IpAddr::V6(inner.parse().map_err(|_| {
                "the webhook URL has an invalid IPv6 address".to_string()
            })?))
        } else if let Ok(ip) = raw_host.parse::<Ipv4Addr>() {
            Host::Ip(IpAddr::V4(ip))
        } else {
            let name = raw_host.trim_end_matches('.').to_ascii_lowercase();
            let valid = !name.is_empty()
                && name.len() <= 253
                && name.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                });
            if !valid {
                return Err("the webhook URL has an invalid host name".to_string());
            }
            Host::Name(name)
        };
        let port = match uri.port_u16() {
            Some(0) => return Err("the webhook URL names port 0".to_string()),
            Some(p) => p,
            None if https => 443,
            None => 80,
        };
        let path = uri
            .path_and_query()
            .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
        let path = if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        Ok(Self {
            https,
            host,
            port,
            path,
        })
    }

    /// `scheme://host`, the only form of the URL that may be logged.
    pub fn display(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        match &self.host {
            Host::Name(n) => format!("{scheme}://{n}"),
            Host::Ip(IpAddr::V6(ip)) => format!("{scheme}://[{ip}]"),
            Host::Ip(ip) => format!("{scheme}://{ip}"),
        }
    }

    /// The `Host` header value.
    pub fn host_header(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        let host = match &self.host {
            Host::Name(n) => n.clone(),
            Host::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
            Host::Ip(ip) => ip.to_string(),
        };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// Why this target is refused at configuration time without resolving
    /// anything: a literal address in a blocked range, or a name that is
    /// loopback by definition.
    pub fn static_refusal(&self) -> Option<String> {
        match &self.host {
            Host::Ip(ip) => blocked_reason(*ip).map(|r| format!("the webhook host is a {r}")),
            Host::Name(n) => (n == "localhost" || n.ends_with(".localhost"))
                .then(|| "the webhook host is a loopback name".to_string()),
        }
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Target({})", self.display())
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(s: &str) -> Option<&'static str> {
        blocked_reason(s.parse().unwrap())
    }

    #[test]
    fn refuses_every_non_public_range() {
        for blocked in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.0.1",
            "169.254.169.254",
            "168.63.129.16",
            "100.64.0.1",
            "100.100.100.200",
            "100.127.255.255",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "192.0.0.5",
            "198.18.0.1",
            "198.19.255.255",
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd00::1",
            "fd00:ec2::254",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "::7f00:1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "100::1",
        ] {
            assert!(reason(blocked).is_some(), "{blocked} must be refused");
        }
    }

    #[test]
    fn allows_public_and_documentation_addresses() {
        for ok in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.2.10",
            "198.51.100.7",
            "203.0.113.10",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2001:db8::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert_eq!(reason(ok), None, "{ok} is public");
        }
    }

    #[test]
    fn the_metadata_endpoint_is_named_as_such() {
        assert_eq!(reason("169.254.169.254"), Some("cloud metadata address"));
    }

    #[test]
    fn parses_ports_hosts_and_paths() {
        let t = Target::parse("https://Hooks.Example.com/services/T0/B0/xyz?a=1").unwrap();
        assert!(t.https);
        assert_eq!(t.host, Host::Name("hooks.example.com".into()));
        assert_eq!(t.port, 443);
        assert_eq!(t.path, "/services/T0/B0/xyz?a=1");
        assert_eq!(t.host_header(), "hooks.example.com");
        assert_eq!(t.display(), "https://hooks.example.com");

        let t = Target::parse("http://127.0.0.1:8080").unwrap();
        assert!(!t.https);
        assert_eq!(t.host, Host::Ip("127.0.0.1".parse().unwrap()));
        assert_eq!(t.path, "/");
        assert_eq!(t.host_header(), "127.0.0.1:8080");

        let t = Target::parse("https://[2001:db8::1]:8443/x").unwrap();
        assert_eq!(t.display(), "https://[2001:db8::1]");
        assert_eq!(t.host_header(), "[2001:db8::1]:8443");
    }

    #[test]
    fn rejects_malformed_urls_without_echoing_them() {
        let token = "SUPERSECRETTOKEN";
        for bad in [
            format!("ftp://example.com/{token}"),
            format!("example.com/{token}"),
            format!("https://user:{token}@example.com/"),
            format!("https://exa mple.com/{token}"),
            format!("https://example.com:0/{token}"),
            format!("https://[::zz]/{token}"),
            format!("https://-bad..host/{token}"),
            "https://".to_string(),
        ] {
            let err = Target::parse(&bad).expect_err(&bad);
            assert!(!err.contains(token), "error echoes the URL: {err}");
            assert!(!err.contains("example.com"), "error echoes the URL: {err}");
        }
    }

    #[test]
    fn static_refusal_catches_literals_and_localhost() {
        assert!(
            Target::parse("https://169.254.169.254/latest")
                .unwrap()
                .static_refusal()
                .is_some()
        );
        assert!(
            Target::parse("https://localhost/")
                .unwrap()
                .static_refusal()
                .is_some()
        );
        assert!(
            Target::parse("https://a.localhost/")
                .unwrap()
                .static_refusal()
                .is_some()
        );
        assert!(
            Target::parse("https://[fd00:ec2::254]/")
                .unwrap()
                .static_refusal()
                .is_some()
        );
        assert!(
            Target::parse("https://hooks.example.com/")
                .unwrap()
                .static_refusal()
                .is_none()
        );
    }

    #[test]
    fn debug_and_display_hide_the_path() {
        let t = Target::parse("https://hooks.example.com/services/SECRETPATH").unwrap();
        assert!(!format!("{t:?}").contains("SECRETPATH"));
        assert!(!format!("{t}").contains("SECRETPATH"));
    }
}
