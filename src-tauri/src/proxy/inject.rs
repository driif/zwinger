//! Puts credentials into requests. A credential of a rule is set as a
//! header, or replaces its placeholder where the agent sent it (header
//! fields, query, body). A host login replaces its placeholder in the
//! `authorization` header with the current token of the Mac, which the
//! proxy reads again when it is due. No value is ever logged.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue};
use http_body::{Body, Frame};
use tokio::time::Instant;

use crate::agent::AgentKind;
use crate::error::Result;
use crate::host_agents::HostAgents;
use crate::host_agents::login::HostToken;
use crate::network::{Credential, Inject, SECRET_MARK, basic_user};
use crate::secrets::{self, SecretValue};

/// How long a due login waits before the proxy reads the Mac again.
const LOGIN_RETRY: Duration = Duration::from_secs(60);

/// The header value of a `Set` credential.
pub fn render(template: &str, value: &str) -> String {
    match basic_user(template) {
        Some((before, user, after)) => {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{value}"));
            format!("{before}{basic}{after}")
        }
        None => template.replace(SECRET_MARK, value),
    }
}

/// What [`apply`] put in: the credential names, and the placeholder and
/// value pairs that the body still needs.
#[derive(Default)]
pub struct Injected {
    pub names: Vec<String>,
    pub body: Vec<(Vec<u8>, Vec<u8>)>,
}

fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(value).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// Puts each credential into the request head.
pub fn apply(parts: &mut http::request::Parts, credentials: &[(Credential, SecretValue)]) -> Injected {
    let mut injected = Injected::default();
    for (credential, value) in credentials {
        let placeholder = credential.placeholder();
        let used = match &credential.inject {
            Inject::Set { header, template } => match sensitive(&render(template, value.expose())) {
                Some(rendered) => match http::HeaderName::from_bytes(header.as_bytes()) {
                    Ok(name) => {
                        parts.headers.insert(name, rendered);
                        true
                    }
                    Err(_) => false,
                },
                None => false,
            },
            Inject::Replace { places } => {
                let mut used = false;
                for field in &places.header_fields {
                    used |= replace_header(&mut parts.headers, field, &placeholder, value.expose());
                }
                if places.query {
                    used |= replace_query(&mut parts.uri, &placeholder, value.expose());
                }
                if places.body {
                    injected.body.push((placeholder.into_bytes(), value.expose().as_bytes().to_vec()));
                    used = true;
                }
                used
            }
        };
        if used && !injected.names.contains(&credential.name) {
            injected.names.push(credential.name.clone());
        }
    }
    injected
}

fn replace_header(headers: &mut HeaderMap, field: &str, placeholder: &str, value: &str) -> bool {
    let current: Vec<HeaderValue> = headers.get_all(field).iter().cloned().collect();
    if !current.iter().any(|v| v.to_str().is_ok_and(|v| v.contains(placeholder))) {
        return false;
    }
    headers.remove(field);
    for old in current {
        let new = old
            .to_str()
            .ok()
            .filter(|v| v.contains(placeholder))
            .and_then(|v| sensitive(&v.replace(placeholder, value)));
        // A value that is no valid header keeps the placeholder.
        headers.append(http::HeaderName::from_bytes(field.as_bytes()).expect("a checked field"), new.unwrap_or(old));
    }
    true
}

/// The placeholder as a client percent-encodes it in a query.
fn encoded(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn replace_query(uri: &mut http::Uri, placeholder: &str, value: &str) -> bool {
    let Some(query) = uri.query() else {
        return false;
    };
    let new = query.replace(placeholder, &encoded(value)).replace(&encoded(placeholder), &encoded(value));
    if new == query {
        return false;
    }
    let mut parts = uri.clone().into_parts();
    let Ok(path) = http::uri::PathAndQuery::try_from(format!("{}?{new}", uri.path())) else {
        return false;
    };
    parts.path_and_query = Some(path);
    match http::Uri::from_parts(parts) {
        Ok(changed) => {
            *uri = changed;
            true
        }
        Err(_) => false,
    }
}

/// The request carries the placeholder: in its `authorization` header, or
/// as its `access_token` query parameter.
pub fn carries(parts: &http::request::Parts, placeholder: &str) -> bool {
    let header =
        parts.headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains(placeholder));
    let query = parts.uri.query().is_some_and(|q| {
        q.split('&').any(|p| {
            p == format!("access_token={placeholder}") || p == format!("access_token={}", encoded(placeholder))
        })
    });
    header || query
}

/// Reads an agent's login on the Mac.
pub trait TokenSource: Send + Sync + 'static {
    fn token(&self, kind: AgentKind) -> Result<HostToken>;
}

impl TokenSource for HostAgents {
    fn token(&self, kind: AgentKind) -> Result<HostToken> {
        HostAgents::token(self, kind)
    }
}

/// The host login of an agent in a running shield.
pub struct Login {
    pub kind: AgentKind,
    /// The name of its placeholder.
    pub name: String,
    /// The exact API hosts that get the token.
    pub hosts: Vec<String>,
    source: Arc<dyn TokenSource>,
    state: Mutex<LoginState>,
}

struct LoginState {
    token: Arc<HostToken>,
    /// The last read of the Mac while the token is due.
    checked: Option<Instant>,
}

impl Login {
    /// `token` is the login that the start read.
    pub fn new(name: String, hosts: Vec<String>, token: HostToken, source: Arc<dyn TokenSource>) -> Self {
        Self {
            kind: token.kind,
            name,
            hosts,
            source,
            state: Mutex::new(LoginState { token: Arc::new(token), checked: None }),
        }
    }

    pub fn placeholder(&self) -> String {
        secrets::placeholder(&self.name)
    }

    pub fn expires(&self) -> Option<i64> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).token.expires
    }

    /// The token. It reads the Mac again at most once a minute, so a new
    /// login on the Mac applies without a restart, and keeps a newer login
    /// for the same host.
    pub async fn current(&self) -> Arc<HostToken> {
        let (token, due) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let due = state.checked.is_none_or(|t| t.elapsed() >= LOGIN_RETRY);
            if due {
                state.checked = Some(Instant::now());
            }
            (state.token.clone(), due)
        };
        if !due {
            return token;
        }
        // The Keychain read can wait for the macOS prompt.
        let (source, kind) = (self.source.clone(), self.kind);
        match tokio::task::spawn_blocking(move || source.token(kind)).await {
            // A changed token for the same host replaces the old one; a token
            // that expires earlier is not newer.
            Ok(Ok(new))
                if new.host == token.host
                    && new.expires >= token.expires
                    && new.full_access().expose() != token.full_access().expose() =>
            {
                let new = Arc::new(new);
                self.state.lock().unwrap_or_else(PoisonError::into_inner).token = new.clone();
                new
            }
            _ => token,
        }
    }

    /// Replaces the `authorization` header that carries the placeholder
    /// with the whole token, under the same scheme.
    pub fn apply(&self, headers: &mut HeaderMap, token: &HostToken) -> bool {
        let placeholder = self.placeholder();
        let Some(current) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        if !current.contains(&placeholder) {
            return false;
        }
        let scheme = current.split_once(' ').map_or("Bearer", |(scheme, _)| scheme);
        match sensitive(&format!("{scheme} {}", token.full_access().expose())) {
            Some(value) => {
                headers.insert(AUTHORIZATION, value);
                true
            }
            None => false,
        }
    }
}

/// A request body with the placeholders replaced as it streams. A
/// placeholder split over two chunks is held back until the next chunk.
pub struct Rewrite<B> {
    inner: B,
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    carry: Vec<u8>,
    trailers: Option<HeaderMap>,
    done: bool,
}

impl<B> Rewrite<B> {
    /// Each placeholder starts with `$`.
    pub fn new(inner: B, pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        Self { inner, pairs, carry: Vec::new(), trailers: None, done: false }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        'scan: while i < buf.len() {
            if buf[i] == b'$' {
                for (from, to) in &self.pairs {
                    if buf[i..].starts_with(from) {
                        out.extend_from_slice(to);
                        i += from.len();
                        continue 'scan;
                    }
                }
                if self.pairs.iter().any(|(from, _)| from.starts_with(&buf[i..])) {
                    self.carry = buf[i..].to_vec();
                    return out;
                }
            }
            out.push(buf[i]);
            i += 1;
        }
        out
    }
}

impl<B: Body<Data = Bytes> + Unpin> Body for Rewrite<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, B::Error>>> {
        let this = &mut *self;
        loop {
            if this.done {
                if !this.carry.is_empty() {
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(std::mem::take(&mut this.carry))))));
                }
                return Poll::Ready(this.trailers.take().map(|t| Ok(Frame::trailers(t))));
            }
            match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        let out = this.feed(&data);
                        if !out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                        }
                    }
                    Err(frame) => {
                        this.trailers = frame.into_trailers().ok();
                        this.done = true;
                    }
                },
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => this.done = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use http_body_util::{BodyExt, Full};

    use super::*;
    use crate::secrets::Places;

    /// A body of the given chunks.
    struct Chunks(VecDeque<&'static str>);

    impl Body for Chunks {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
            Poll::Ready(self.0.pop_front().map(|c| Ok(Frame::data(Bytes::from(c)))))
        }
    }

    fn parts(uri: &str, headers: &[(&str, &str)]) -> http::request::Parts {
        let mut request = http::Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(()).unwrap().into_parts().0
    }

    fn set(name: &str, header: &str, template: &str) -> Credential {
        Credential { name: name.into(), inject: Inject::Set { header: header.into(), template: template.into() } }
    }

    fn replace(name: &str, places: Places) -> Credential {
        Credential { name: name.into(), inject: Inject::Replace { places } }
    }

    #[test]
    fn set_adds_or_overwrites_the_header() {
        let mut head = parts("https://api.github.com/user", &[("authorization", "token guest-made")]);
        let creds = [(set("GITHUB_TOKEN", "authorization", "Bearer {secret}"), SecretValue::new("ghp_1"))];
        let injected = apply(&mut head, &creds);
        assert_eq!(head.headers["authorization"], "Bearer ghp_1");
        assert!(head.headers["authorization"].is_sensitive());
        assert_eq!(injected.names, ["GITHUB_TOKEN"]);
        let mut head = parts("https://github.com/x.git", &[]);
        apply(&mut head, &[(set("GH", "authorization", "Basic {basic:x-access-token}"), SecretValue::new("t"))]);
        assert_eq!(head.headers["authorization"], "Basic eC1hY2Nlc3MtdG9rZW46dA==");
    }

    #[test]
    fn replace_touches_only_the_placeholder_in_the_chosen_places() {
        let places = Places { header_fields: vec!["x-api-key".into()], query: true, body: true };
        let mut head = parts(
            "https://maps.example/v1?key=%24MSB_KEY%24&other=$MSB_KEY$",
            &[("x-api-key", "$MSB_KEY$"), ("x-echo", "$MSB_KEY$")],
        );
        let injected = apply(&mut head, &[(replace("KEY", places), SecretValue::new("v a"))]);
        assert_eq!(head.headers["x-api-key"], "v a");
        assert_eq!(head.headers["x-echo"], "$MSB_KEY$");
        assert_eq!(head.uri.query(), Some("key=v%20a&other=v%20a"));
        assert_eq!(injected.body, [(b"$MSB_KEY$".to_vec(), b"v a".to_vec())]);

        // A request with no placeholder gets nothing and names nothing.
        let places = Places { header_fields: vec!["authorization".into()], query: false, body: false };
        let mut head = parts("https://a.io/", &[("authorization", "Bearer mine")]);
        let injected = apply(&mut head, &[(replace("KEY", places), SecretValue::new("v"))]);
        assert_eq!(head.headers["authorization"], "Bearer mine");
        assert!(injected.names.is_empty());
    }

    #[tokio::test]
    async fn the_body_is_rewritten_across_chunks() {
        let chunks = ["{\"k\":\"$MS", "B_KEY$\",\"x\":\"$", "MSB_NO$ $MSB_KEY$\"}"];
        let body = Rewrite::new(Chunks(chunks.into()), vec![(b"$MSB_KEY$".to_vec(), b"secret".to_vec())]);
        let out = body.collect().await.unwrap().to_bytes();
        assert_eq!(out, "{\"k\":\"secret\",\"x\":\"$MSB_NO$ secret\"}");
        let tail = Rewrite::new(Full::new(Bytes::from("end $MSB_K")), vec![(b"$MSB_KEY$".to_vec(), b"s".to_vec())]);
        assert_eq!(tail.collect().await.unwrap().to_bytes(), "end $MSB_K");
    }

    /// Logins from a document per kind; reads are counted.
    struct Source {
        doc: std::sync::Mutex<String>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl TokenSource for Source {
        fn token(&self, kind: AgentKind) -> Result<HostToken> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::host_agents::login::parse(kind, &self.doc.lock().unwrap())
        }
    }

    #[tokio::test]
    async fn the_mac_login_is_read_again_and_a_newer_token_applies_at_once() {
        use crate::host_agents::login::fake::claude_doc;
        const NOW: i64 = 1_800_000_000_000;
        let doc = claude_doc("old", NOW + 5 * 60_000);
        let token = crate::host_agents::login::parse(AgentKind::Claude, &doc).unwrap();
        let source = Arc::new(Source { doc: std::sync::Mutex::new(doc), reads: Default::default() });
        let login =
            Login::new("CLAUDE_CODE_OAUTH_TOKEN".into(), vec!["api.anthropic.com".into()], token, source.clone());

        // The Mac has the same login: it stays, and the next read waits a minute.
        assert_eq!(login.current().await.access.expose(), "old");
        assert_eq!(login.current().await.access.expose(), "old");
        assert_eq!(source.reads.load(std::sync::atomic::Ordering::SeqCst), 1);

        *source.doc.lock().unwrap() = claude_doc("new", NOW + 3_600_000);
        login.state.lock().unwrap().checked = None;
        let token = login.current().await;
        assert_eq!((token.access.expose(), login.expires()), ("new", Some(NOW + 3_600_000)));
        // Read a minute ago: no read.
        login.current().await;
        assert_eq!(source.reads.load(std::sync::atomic::Ordering::SeqCst), 2);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer $MSB_CLAUDE_CODE_OAUTH_TOKEN$"));
        assert!(login.apply(&mut headers, &token));
        assert_eq!(headers[AUTHORIZATION], "Bearer new");
        let mut other = HeaderMap::new();
        other.insert(AUTHORIZATION, HeaderValue::from_static("Bearer mine"));
        assert!(!login.apply(&mut other, &token));
        assert_eq!(other[AUTHORIZATION], "Bearer mine");
    }
}
