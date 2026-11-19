//! The decision for one request, in this order: offline; the tokeninfo
//! check of a Gemini host login, unless a Block rule names its host; the
//! host port grant (ADR 0009), unless a Block rule names its host; the
//! always-blocked floor (private and metadata addresses, local names, the
//! refresh hosts of the host logins); Block rules; Allow rules; the
//! default. The addresses that a name resolves to are checked in
//! [`super::dial`]: for a CONNECT before its answer, and before each connect.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

use crate::host_agents::login::{TOKENINFO_HOST, TOKENINFO_PATH};
use crate::network::{Action, DefaultAction, NetworkPolicy};
use crate::proxy::GUEST_HOST;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Offline,
    AlwaysBlocked,
    /// The label of the rule: a service name or a host.
    Rule(String),
    Default,
    TokenInfo,
    HostPort,
}

/// The log shows the reason as `offline`, `always-blocked`, `rule:<name>`,
/// `default`, `tokeninfo` or `host-port`.
impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline => f.write_str("offline"),
            Self::AlwaysBlocked => f.write_str("always-blocked"),
            Self::Rule(label) => write!(f, "rule:{label}"),
            Self::Default => f.write_str("default"),
            Self::TokenInfo => f.write_str("tokeninfo"),
            Self::HostPort => f.write_str("host-port"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow(Reason),
    Block(Reason),
    /// The proxy answers the request itself and connects nowhere.
    TokenInfo,
    /// The host port grant: the request goes to `127.0.0.1:<port>` of the Mac.
    HostPort(NonZeroU16),
}

pub struct Context<'a> {
    pub policy: &'a NetworkPolicy,
    /// The refresh hosts of the project's host logins.
    pub refresh: &'a [&'static str],
    /// The project has a Gemini host login, whose tokeninfo check the proxy
    /// answers. The login is the allow that the check needs.
    pub tokeninfo: bool,
    /// The port of the proxy itself. The grant never opens it.
    pub listener: u16,
}

/// `port` is the port of the request target. `request` is the method and
/// the path (no query) of a request. It is `None` for a CONNECT, which the
/// proxy decides on the host and the port only.
pub fn decide(ctx: &Context, host: &str, port: u16, request: Option<(&str, &str)>) -> Decision {
    if ctx.policy.offline {
        return Decision::Block(Reason::Offline);
    }
    let rules = &ctx.policy.rules;
    let block = rules.iter().find(|r| r.action == Action::Block && r.matches(host));
    if ctx.tokeninfo && host == TOKENINFO_HOST {
        return match (block, request) {
            (Some(rule), _) => Decision::Block(Reason::Rule(rule.label())),
            (None, None | Some(("GET" | "POST", TOKENINFO_PATH))) => Decision::TokenInfo,
            (None, Some(_)) => Decision::Block(Reason::AlwaysBlocked),
        };
    }
    if host == GUEST_HOST
        && block.is_none()
        && port != ctx.listener
        && let Some(granted) = ctx.policy.host_port.filter(|g| g.get() == port)
    {
        return Decision::HostPort(granted);
    }
    if is_local_name(host) || ctx.refresh.contains(&host) || host.parse::<IpAddr>().is_ok_and(is_always_blocked) {
        return Decision::Block(Reason::AlwaysBlocked);
    }
    if let Some(rule) = block {
        return Decision::Block(Reason::Rule(rule.label()));
    }
    if let Some(rule) = rules.iter().find(|r| r.action == Action::Allow && r.matches(host)) {
        return Decision::Allow(Reason::Rule(rule.label()));
    }
    match ctx.policy.default {
        DefaultAction::Allow => Decision::Allow(Reason::Default),
        DefaultAction::Block => Decision::Block(Reason::Default),
    }
}

/// Names that point at this Mac, the VM host or the LAN.
fn is_local_name(host: &str) -> bool {
    host == "localhost" || [".localhost", ".local", ".internal", ".home.arpa"].iter().any(|s| host.ends_with(s))
}

/// Loopback, private and LAN (RFC 1918, ULA), link-local, CGNAT, cloud
/// metadata, the microsandbox gateway and other non-public ranges. An IPv6
/// address with an IPv4 address inside counts as that IPv4 address.
pub fn is_always_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b))
        || a >= 240
}

fn blocked_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return blocked_v4(v4);
    }
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // ULA fc00::/7, also fd00:ec2::254 and the gateway fd42:6d73:62:1::1.
        || (s[0] & 0xfe00) == 0xfc00
        // Link-local fe80::/10 and the old site-local fec0::/10.
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        // Documentation 2001:db8::/32 and Teredo 2001::/32.
        || (s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0))
        // IPv4-compatible ::a.b.c.d and NAT64 64:ff9b::/96.
        || (s[..6] == [0; 6] && blocked_v4(embedded(s[6], s[7])))
        || (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] && blocked_v4(embedded(s[6], s[7])))
        // The local-use NAT64 prefix 64:ff9b:1::/48.
        || s[..3] == [0x64, 0xff9b, 1]
        // 6to4 2002::/16 carries an IPv4 address in its next 32 bits.
        || (s[0] == 0x2002 && blocked_v4(embedded(s[1], s[2])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{Rule, Target};

    const LISTENER: u16 = 50_000;

    fn policy(default: DefaultAction, rules: &[(&str, Action)]) -> NetworkPolicy {
        NetworkPolicy {
            default,
            offline: false,
            rules: rules.iter().map(|(h, a)| Rule::new(Target::Host { host: (*h).to_owned() }, *a)).collect(),
            host_port: None,
        }
    }

    fn ctx(policy: &NetworkPolicy) -> Context<'_> {
        Context { policy, refresh: &[], tokeninfo: false, listener: LISTENER }
    }

    #[test]
    fn the_order_is_offline_floor_block_allow_default() {
        let mut p = policy(DefaultAction::Allow, &[("*.a.io", Action::Allow), ("x.a.io", Action::Block)]);
        assert_eq!(decide(&ctx(&p), "y.a.io", 443, None), Decision::Allow(Reason::Rule("*.a.io".into())));
        // Block beats Allow, whatever the order of the rules.
        assert_eq!(decide(&ctx(&p), "x.a.io", 443, None), Decision::Block(Reason::Rule("x.a.io".into())));
        assert_eq!(decide(&ctx(&p), "b.io", 443, None), Decision::Allow(Reason::Default));
        for host in ["127.0.0.1", "10.1.2.3", "169.254.169.254", "localhost", "host.microsandbox.internal"] {
            assert_eq!(decide(&ctx(&p), host, 443, None), Decision::Block(Reason::AlwaysBlocked), "{host}");
        }
        p.default = DefaultAction::Block;
        assert_eq!(decide(&ctx(&p), "b.io", 443, Some(("GET", "/"))), Decision::Block(Reason::Default));
        p.offline = true;
        assert_eq!(decide(&ctx(&p), "y.a.io", 443, None), Decision::Block(Reason::Offline));
    }

    #[test]
    fn refresh_hosts_are_always_blocked_also_when_allowed() {
        let p = policy(DefaultAction::Allow, &[("console.anthropic.com", Action::Allow)]);
        let ctx = Context { policy: &p, refresh: &["console.anthropic.com"], tokeninfo: false, listener: LISTENER };
        assert_eq!(decide(&ctx, "console.anthropic.com", 443, None), Decision::Block(Reason::AlwaysBlocked));
    }

    #[test]
    fn the_gemini_tokeninfo_check_is_the_only_exception_of_its_refresh_host() {
        let p = policy(DefaultAction::Block, &[]);
        let gemini = Context { policy: &p, refresh: &[TOKENINFO_HOST], tokeninfo: true, listener: LISTENER };
        assert_eq!(decide(&gemini, TOKENINFO_HOST, 443, None), Decision::TokenInfo);
        assert_eq!(decide(&gemini, TOKENINFO_HOST, 443, Some(("POST", "/tokeninfo"))), Decision::TokenInfo);
        assert_eq!(decide(&gemini, TOKENINFO_HOST, 443, Some(("GET", "/tokeninfo"))), Decision::TokenInfo);
        for (method, path) in [("POST", "/token"), ("GET", "/tokeninfo/x"), ("PUT", "/tokeninfo"), ("POST", "/")] {
            assert_eq!(
                decide(&gemini, TOKENINFO_HOST, 443, Some((method, path))),
                Decision::Block(Reason::AlwaysBlocked),
                "{method} {path}"
            );
        }
        // Without a Gemini login, the refresh deny of another login holds the host.
        let other = Context { policy: &p, refresh: &[TOKENINFO_HOST], tokeninfo: false, listener: LISTENER };
        assert_eq!(
            decide(&other, TOKENINFO_HOST, 443, Some(("POST", "/tokeninfo"))),
            Decision::Block(Reason::AlwaysBlocked)
        );
        // Offline comes first.
        let offline = NetworkPolicy { offline: true, ..p.clone() };
        let gemini = Context { policy: &offline, ..gemini };
        assert_eq!(decide(&gemini, TOKENINFO_HOST, 443, None), Decision::Block(Reason::Offline));
    }

    #[test]
    fn a_block_rule_beats_the_tokeninfo_check() {
        for blocked in [TOKENINFO_HOST, "*.googleapis.com"] {
            let p = policy(DefaultAction::Allow, &[(blocked, Action::Block)]);
            let gemini = Context { policy: &p, refresh: &[TOKENINFO_HOST], tokeninfo: true, listener: LISTENER };
            for request in [None, Some(("GET", TOKENINFO_PATH))] {
                assert_eq!(
                    decide(&gemini, TOKENINFO_HOST, 443, request),
                    Decision::Block(Reason::Rule(blocked.into())),
                    "{blocked}"
                );
            }
        }
        // With no Gemini login, an Allow rule allows the host, and the proxy does not answer.
        let p = policy(DefaultAction::Block, &[(TOKENINFO_HOST, Action::Allow)]);
        let none = Context { policy: &p, refresh: &[], tokeninfo: false, listener: LISTENER };
        assert_eq!(
            decide(&none, TOKENINFO_HOST, 443, Some(("GET", TOKENINFO_PATH))),
            Decision::Allow(Reason::Rule(TOKENINFO_HOST.into()))
        );
    }

    #[test]
    fn the_host_port_grant_opens_one_port_of_the_gateway_name_only() {
        let mut p = policy(DefaultAction::Allow, &[]);
        p.host_port = NonZeroU16::new(11434);
        let granted = Decision::HostPort(NonZeroU16::new(11434).unwrap());
        let get = Some(("GET", "/api/tags"));
        assert_eq!(decide(&ctx(&p), GUEST_HOST, 11434, get), granted);
        assert_eq!(decide(&ctx(&p), GUEST_HOST, 11434, None), granted);
        // Another port, another loopback name or address stays on the floor.
        for (host, port) in [
            (GUEST_HOST, 11435),
            (GUEST_HOST, 80),
            ("localhost", 11434),
            ("127.0.0.1", 11434),
            ("::1", 11434),
            ("x.host.microsandbox.internal", 11434),
            ("host.docker.internal", 11434),
        ] {
            assert_eq!(decide(&ctx(&p), host, port, get), Decision::Block(Reason::AlwaysBlocked), "{host}:{port}");
        }
        // The proxy's own port is never the grant.
        p.host_port = NonZeroU16::new(LISTENER);
        assert_eq!(decide(&ctx(&p), GUEST_HOST, LISTENER, get), Decision::Block(Reason::AlwaysBlocked));
        // No grant, no route.
        p.host_port = None;
        assert_eq!(decide(&ctx(&p), GUEST_HOST, 11434, get), Decision::Block(Reason::AlwaysBlocked));
        // An Allow rule for the name opens nothing.
        let mut allowed = policy(DefaultAction::Allow, &[(GUEST_HOST, Action::Allow), ("*.internal", Action::Allow)]);
        assert_eq!(decide(&ctx(&allowed), GUEST_HOST, 11434, get), Decision::Block(Reason::AlwaysBlocked));
        // A Block rule that names the host beats the grant, and Offline beats both.
        for blocked in [GUEST_HOST, "*.microsandbox.internal"] {
            let mut p = policy(DefaultAction::Allow, &[(blocked, Action::Block)]);
            p.host_port = NonZeroU16::new(11434);
            assert!(matches!(decide(&ctx(&p), GUEST_HOST, 11434, get), Decision::Block(_)), "{blocked}");
        }
        allowed.host_port = NonZeroU16::new(11434);
        allowed.offline = true;
        assert_eq!(decide(&ctx(&allowed), GUEST_HOST, 11434, get), Decision::Block(Reason::Offline));
    }

    #[test]
    fn always_blocked_ranges() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.5",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fd00:ec2::254",
            "fd42:6d73:62:1::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "64:ff9b:1::808:808",
            "2002:a00:1::",
        ] {
            assert!(is_always_blocked(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "140.82.121.4", "2606:4700::1111", "::ffff:8.8.8.8", "100.128.0.1", "64:ff9b::808:808"] {
            assert!(!is_always_blocked(ip.parse().unwrap()), "{ip}");
        }
    }
}
