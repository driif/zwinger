//! The one reading of a request authority (`host[:port]`) of the guest.
//! The CONNECT target, the URI of an absolute-form request and each `host`
//! field go through [`parse`], and the policy check, the credentials and
//! the log use only its result. An authority that two readers could read
//! as two hosts is refused: userinfo, percent-encoding, a written port
//! that is not 1 to 5 decimal digits (or is 0), more than one final dot,
//! an empty label, a byte outside letters, digits, `-`, `_` and `.` (send
//! IDNA as punycode), a short or hex IPv4 form, and an IPv6 address with
//! no brackets or a zone. The host port grant has its own strict check.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The canonical host and the port of an authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    /// Lowercase, with no final dot. An IP address in its canonical text,
    /// an IPv6 address with no brackets.
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    Userinfo,
    Port,
    Host,
}

/// Reads `raw` as `host[:port]`. No written port means `default_port`.
pub fn parse(raw: &str, default_port: u16) -> Result<Authority, Bad> {
    if raw.contains('@') {
        return Err(Bad::Userinfo);
    }
    if raw.contains('%') {
        return Err(Bad::Host);
    }
    let (host, written) = match raw.strip_prefix('[') {
        Some(rest) => {
            let (inside, after) = rest.split_once(']').ok_or(Bad::Host)?;
            let ip = inside.parse::<Ipv6Addr>().map_err(|_| Bad::Host)?;
            let written = match after {
                "" => None,
                after => Some(after.strip_prefix(':').ok_or(Bad::Host)?),
            };
            return Ok(Authority { host: ip.to_string(), port: port(written, default_port)? });
        }
        // An IPv6 address needs brackets.
        None if raw.matches(':').count() > 1 => return Err(Bad::Host),
        None => match raw.split_once(':') {
            Some((host, written)) => (host, Some(written)),
            None => (raw, None),
        },
    };
    let port = port(written, default_port)?;
    Ok(Authority { host: name(host)?, port })
}

/// A written port is 1 to 5 decimal digits, from 1 to 65535.
fn port(written: Option<&str>, default_port: u16) -> Result<u16, Bad> {
    let Some(written) = written else {
        return Ok(default_port);
    };
    if !(1..=5).contains(&written.len()) || !written.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Bad::Port);
    }
    match written.parse::<u16>() {
        Ok(0) | Err(_) => Err(Bad::Port),
        Ok(port) => Ok(port),
    }
}

/// A DNS name or a dotted-quad IPv4 address, with one final dot at most.
fn name(host: &str) -> Result<String, Bad> {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err(Bad::Host);
    }
    let labels: Vec<&str> = host.split('.').collect();
    let valid = |l: &&str| {
        (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !labels.iter().all(valid) {
        return Err(Bad::Host);
    }
    // A resolver reads a name whose last label is a number as an IPv4
    // address, also `127.1`, `0x7f.0.0.1` or `2130706433`: only the
    // dotted-quad form is taken.
    let last = labels.last().expect("split gives one label at least");
    let hex = last.strip_prefix("0x").is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
    if hex || last.bytes().all(|b| b.is_ascii_digit()) {
        let ip = host.parse::<Ipv4Addr>().map_err(|_| Bad::Host)?;
        return Ok(ip.to_string());
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str, default_port: u16) -> (String, u16) {
        let a = parse(raw, default_port).unwrap_or_else(|e| panic!("{raw}: {e:?}"));
        (a.host, a.port)
    }

    #[test]
    fn equal_spellings_read_as_one_host_and_port() {
        let same = ["api.test", "API.Test", "api.test.", "api.test:443", "Api.TEST.:443", "api.test:0443"];
        for raw in same {
            assert_eq!(ok(raw, 443), ("api.test".into(), 443), "{raw}");
        }
        assert_eq!(ok("api.test:8080", 443), ("api.test".into(), 8080));
        assert_eq!(ok("a_b.test", 80), ("a_b.test".into(), 80));
        assert_eq!(ok("1.2.3.4:80", 443), ("1.2.3.4".into(), 80));
        assert_eq!(ok("1.2.3.4.", 443), ("1.2.3.4".into(), 443));
        assert_eq!(ok("[2001:DB8::1]:8443", 443), ("2001:db8::1".into(), 8443));
        assert_eq!(ok("[2001:db8:0:0:0:0:0:1]", 443), ("2001:db8::1".into(), 443));
        assert_eq!(ok("[::ffff:127.0.0.1]", 80), ("::ffff:127.0.0.1".into(), 80));
        assert_eq!(ok("localhost.", 80), ("localhost".into(), 80));
    }

    #[test]
    fn ambiguous_or_malformed_authorities_are_refused() {
        let refused = [
            // Userinfo: the host after `@` is not what a naive reader takes.
            ("user@api.test", Bad::Userinfo),
            ("user:pass@api.test:443", Bad::Userinfo),
            ("api.test@evil.test", Bad::Userinfo),
            ("evil.test:443@api.test", Bad::Userinfo),
            ("@api.test", Bad::Userinfo),
            // Ports.
            ("api.test:", Bad::Port),
            ("api.test:abc", Bad::Port),
            ("api.test:+443", Bad::Port),
            ("api.test:-1", Bad::Port),
            ("api.test:0", Bad::Port),
            ("api.test:00000", Bad::Port),
            ("api.test:65536", Bad::Port),
            ("api.test:66979", Bad::Port),
            ("api.test:000443", Bad::Port),
            ("api.test: 443", Bad::Port),
            ("api.test:443 ", Bad::Port),
            ("api.test:4 43", Bad::Port),
            ("api.test:443:443", Bad::Host),
            ("[::1]:abc", Bad::Port),
            // Percent-encoding and zones.
            ("api%2etest", Bad::Host),
            ("api.test%00.evil.test", Bad::Host),
            ("[fe80::1%25en0]", Bad::Host),
            ("[fe80::1%en0]:443", Bad::Host),
            // Names.
            ("", Bad::Host),
            (".", Bad::Host),
            (":443", Bad::Host),
            ("api.test..", Bad::Host),
            (".api.test", Bad::Host),
            ("api..test", Bad::Host),
            ("api test", Bad::Host),
            ("api.test\\evil.test", Bad::Host),
            ("api.test/evil", Bad::Host),
            ("api.test#x", Bad::Host),
            ("api.test?x", Bad::Host),
            ("bücher.test", Bad::Host),
            ("api.tеst", Bad::Host),
            ("api.test\0", Bad::Host),
            ("api.test\t", Bad::Host),
            // IPv4 forms that a resolver reads as another address.
            ("127.1", Bad::Host),
            ("2130706433", Bad::Host),
            ("0x7f.0.0.1", Bad::Host),
            ("0x7f000001", Bad::Host),
            ("017.0.0.1", Bad::Host),
            ("127.0.0.01", Bad::Host),
            ("1.2.3.4.5", Bad::Host),
            ("256.0.0.1", Bad::Host),
            ("a.0x10", Bad::Host),
            // IPv6 needs brackets, and only an address in them.
            ("::1", Bad::Host),
            ("2001:db8::1:443", Bad::Host),
            ("[api.test]", Bad::Host),
            ("[::1", Bad::Host),
            ("[::1]443", Bad::Host),
            ("[::1]:443]", Bad::Port),
            ("api.test]", Bad::Host),
            ("[1.2.3.4]", Bad::Host),
        ];
        for (raw, bad) in refused {
            assert_eq!(parse(raw, 443), Err(bad), "{raw:?}");
        }
        let long = format!("{}.test", "a".repeat(64));
        assert_eq!(parse(&long, 443), Err(Bad::Host));
        let too_long = format!("{}test", "a.".repeat(127));
        assert_eq!(parse(&too_long, 443), Err(Bad::Host));
    }
}
