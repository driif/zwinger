//! The request log of a shield. An entry has no query values, no header and
//! no body: only names, sizes and the decision.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::netlog::{NetEvent, NetKind};

/// The reason of a connection that the VM blocked because it did not use the proxy.
pub const DIRECT: &str = "direct";
/// A longer path is cut.
const PATH_LIMIT: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Per shield. The manager sets it.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at: u64,
    /// `None` for a direct connection.
    pub method: Option<String>,
    pub host: String,
    /// The path and the names of the query parameters.
    pub path: Option<String>,
    pub status: Option<u16>,
    /// Body bytes to the host and from it.
    pub up: u64,
    pub down: u64,
    /// Milliseconds from the request to the end of the response.
    pub ms: u64,
    pub allowed: bool,
    /// `offline`, `always-blocked`, `rule:<name>`, `default`, `tokeninfo` or `direct`.
    pub reason: String,
    /// The names of the injected credentials.
    pub credentials: Vec<String>,
    /// What else the proxy did, such as `no credential: plain http`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub type Sink = Arc<dyn Fn(Entry) + Send + Sync>;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Entry {
    pub fn new(method: &str, host: &str, path: Option<&str>) -> Self {
        Self {
            seq: 0,
            at: now_ms(),
            method: Some(method.to_owned()),
            host: host.to_owned(),
            path: path.map(redact),
            status: None,
            up: 0,
            down: 0,
            ms: 0,
            allowed: false,
            reason: String::new(),
            credentials: Vec::new(),
            note: None,
        }
    }

    /// A blocked direct connection from the VM's own log (DNS or TLS).
    pub fn direct(event: &NetEvent) -> Option<Self> {
        matches!(event.kind, NetKind::DnsDenied | NetKind::TlsDenied).then(|| Self {
            seq: 0,
            at: event.at,
            method: None,
            host: event.host.clone(),
            path: None,
            status: None,
            up: 0,
            down: 0,
            ms: 0,
            allowed: false,
            reason: DIRECT.to_owned(),
            credentials: Vec::new(),
            note: None,
        })
    }
}

/// The path with only the names of the query parameters, and no fragment.
pub fn redact(path_and_query: &str) -> String {
    let path_and_query = path_and_query.split('#').next().unwrap_or_default();
    let (path, query) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
    let mut out: String = path.chars().take(PATH_LIMIT).collect();
    let names: Vec<&str> =
        query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or(p, |(n, _)| n)).collect();
    if !names.is_empty() {
        out.push('?');
        out.push_str(&names.join("&").chars().take(PATH_LIMIT).collect::<String>());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_no_query_values() {
        assert_eq!(redact("/v1/x?key=sk-secret&flag&b=2#frag"), "/v1/x?key&flag&b");
        assert_eq!(redact("/plain"), "/plain");
        assert_eq!(redact("/a?"), "/a");
        let entry = Entry::new("GET", "a.io", Some("/q?access_token=ya29.secret"));
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("ya29") && json.contains("access_token"), "{json}");
        let long = format!("/{}", "a".repeat(2000));
        assert_eq!(redact(&long).len(), PATH_LIMIT);
    }

    #[test]
    fn denied_direct_connections_become_entries() {
        let event = |kind| NetEvent { seq: 0, at: 7, kind, host: "pypi.org".into(), dst: None };
        let entry = Entry::direct(&event(NetKind::DnsDenied)).unwrap();
        assert_eq!((entry.reason.as_str(), entry.allowed, entry.method), (DIRECT, false, None));
        assert!(Entry::direct(&event(NetKind::DnsAllowed)).is_none());
    }
}
