//! The egress proxy of a running shield (ADR 0003). It listens on
//! `127.0.0.1` on a free port, the only host port that the VM can reach.
//! Each request needs the shield's proxy token. A CONNECT is decided on its
//! host and its checked addresses, then terminated with a leaf of the zwinger
//! CA, so each inner request is decided, logged and gets its credentials.
//! Plain HTTP is decided and logged the same, but gets no credentials.
//! Every upstream connection goes through [`dial::dial`].

pub mod ca;
pub mod decide;
pub mod dial;
pub mod inject;
pub mod local;
pub mod log;

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context as TaskContext, Poll, ready};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use http::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE,
    TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use http_body::{Body, Frame};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use lru::LruCache;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::agent::AgentKind;
use crate::host_agents::login::{self, TOKENINFO_HOST, TOKENINFO_PATH};
use crate::network::{Credential, NetworkPolicy};
use crate::secrets::SecretValue;
use ca::Ca;
use decide::{Context, Decision, Reason, decide};
use dial::{DialError, Limits, Permits, Resolve};
use inject::Login;
use log::{Entry, Sink};

/// The name of the Mac in the guest. It maps to the host's `127.0.0.1`.
pub const GUEST_HOST: &str = "host.microsandbox.internal";
/// The user name in the proxy URL. Only the token counts.
const USER: &str = "zw";
/// The connections from one shield at once. More are closed at once.
const MAX_CONNECTIONS: usize = 256;
/// The most bytes of a request head.
const HEADER_BYTES: usize = 256 << 10;
const HEADER_LIMIT: Duration = Duration::from_secs(30);
const TLS_LIMIT: Duration = Duration::from_secs(30);
/// A connection with no bytes in either direction for this long is closed.
const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// The longest life of one guest connection.
const CONNECTION_LIMIT: Duration = Duration::from_secs(12 * 60 * 60);
/// The longest wait for the head of an upstream response.
const RESPONSE_LIMIT: Duration = Duration::from_secs(10 * 60);
const POOL_IDLE: Duration = Duration::from_secs(90);
/// The idle upstream connections that the pool keeps per host.
const POOL_PER_HOST: usize = 8;
/// The requests at once in one HTTP/2 connection from the guest.
const GUEST_STREAMS: u32 = 32;
/// The hosts whose checked CONNECT addresses stay pinned for the dial.
const PINS: usize = 1024;
/// The field of each block response from the proxy. Its value is the reason.
/// The network test reads it: a 403 of the host itself has no such field.
pub const BLOCKED_HEADER: &str = "x-zwinger-blocked";
/// The field of each other response that the proxy makes itself, such as a
/// 502 when the host cannot be reached. Its value is the reason.
pub const ERROR_HEADER: &str = "x-zwinger-error";
/// The log note of a plain HTTP request to a host with credentials.
const PLAIN_HTTP: &str = "no credential: plain http";

pub type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;
/// One of [`MAX_CONNECTIONS`]. A guest connection holds it, then the tunnel
/// or the WebSocket that it becomes, until the last of them ends.
type Permit = Arc<OwnedSemaphorePermit>;

/// The policy and the credential values that one request uses together.
#[derive(Clone)]
pub struct Snapshot {
    pub policy: Arc<NetworkPolicy>,
    /// The credential values by name.
    pub values: Arc<HashMap<String, SecretValue>>,
}

/// The parts of a shield's proxy that edits change while it runs: the
/// next request uses them. An edit replaces the policy and the values in one
/// step, so no request sees a new rule with an old value.
pub struct Live {
    snapshot: Mutex<Snapshot>,
}

impl Live {
    pub fn new(policy: NetworkPolicy, values: HashMap<String, SecretValue>) -> Arc<Self> {
        Arc::new(Self { snapshot: Mutex::new(Snapshot { policy: Arc::new(policy), values: Arc::new(values) }) })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Changes the policy and the values together.
    pub fn edit(&self, edit: impl FnOnce(&mut NetworkPolicy, &mut HashMap<String, SecretValue>)) {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        let Snapshot { policy, values } = &mut *snapshot;
        edit(Arc::make_mut(policy), Arc::make_mut(values));
    }

    pub fn set_value(&self, name: &str, value: Option<SecretValue>) {
        self.edit(|_, values| {
            match value {
                Some(value) => values.insert(name.to_owned(), value),
                None => values.remove(name),
            };
        });
    }
}

pub struct Config {
    pub ca: Arc<Ca>,
    pub live: Arc<Live>,
    pub logins: Vec<Arc<Login>>,
    /// The refresh hosts of the host logins.
    pub refresh: Vec<&'static str>,
    pub sink: Sink,
    pub resolver: Arc<dyn Resolve>,
}

/// A running proxy. A drop stops it and its connections.
pub struct Proxy {
    pub port: u16,
    token: String,
    logins: Vec<Arc<Login>>,
    task: AbortHandle,
    shutdown: watch::Sender<bool>,
    #[cfg(test)]
    permits: Arc<Semaphore>,
    #[cfg(test)]
    upstreams: Arc<Upstreams>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
        self.shutdown.send_replace(true);
    }
}

struct Shared {
    /// `zw:<token>`, which the Basic credentials must be.
    expected: Vec<u8>,
    ca: Arc<Ca>,
    live: Arc<Live>,
    refresh: Vec<&'static str>,
    logins: Vec<Arc<Login>>,
    sink: Sink,
    upstreams: Arc<Upstreams>,
    /// HTTP/2 or HTTP/1.1 by ALPN.
    auto: Client<Connector, ProxyBody>,
    /// HTTP/1.1 only, for WebSocket upgrades.
    h1: Client<Connector, ProxyBody>,
    shutdown: watch::Receiver<bool>,
}

impl Proxy {
    pub async fn start(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let upstreams = Arc::new(Upstreams {
            resolver: config.resolver,
            limits: Limits::shield(),
            pins: Mutex::new(LruCache::new(NonZeroUsize::new(PINS).expect("not zero"))),
        });
        let shared = Arc::new(Shared {
            expected: format!("{USER}:{token}").into_bytes(),
            ca: config.ca,
            live: config.live,
            refresh: config.refresh,
            logins: config.logins.clone(),
            sink: config.sink,
            auto: client(upstreams.clone(), &[b"h2", b"http/1.1"])?,
            h1: client(upstreams.clone(), &[b"http/1.1"])?,
            upstreams,
            shutdown: shutdown_rx,
        });
        #[cfg(test)]
        let shared_upstreams = shared.upstreams.clone();
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let accept_permits = permits.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let tcp = match listener.accept().await {
                    Ok((tcp, _)) => tcp,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                while connections.try_join_next().is_some() {}
                let Ok(permit) = accept_permits.clone().try_acquire_owned() else {
                    continue;
                };
                connections.spawn(serve(shared.clone(), tcp, Arc::new(permit)));
            }
        })
        .abort_handle();
        Ok(Self {
            port,
            token,
            logins: config.logins,
            task,
            shutdown,
            #[cfg(test)]
            permits,
            #[cfg(test)]
            upstreams: shared_upstreams,
        })
    }

    /// The URL that the guest uses. Its token is not a user secret: it only
    /// keeps other processes of the Mac out.
    pub fn url(&self) -> String {
        format!("http://{USER}:{}@{GUEST_HOST}:{}", self.token, self.port)
    }

    /// The earliest expiry of the host logins, as the proxy has them now.
    pub fn login_expires(&self) -> Option<i64> {
        self.logins.iter().filter_map(|l| l.expires()).min()
    }

    /// The guest connections, tunnels and WebSockets that hold a permit.
    #[cfg(test)]
    fn open(&self) -> usize {
        MAX_CONNECTIONS - self.permits.available_permits()
    }
}

fn client(upstreams: Arc<Upstreams>, alpn: &[&[u8]]) -> io::Result<Client<Connector, ProxyBody>> {
    let mut tls = ClientConfig::builder_with_provider(ca::provider())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_platform_verifier()
        .map_err(io::Error::other)?
        .with_no_client_auth();
    tls.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(Client::builder(TokioExecutor::new())
        .timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(POOL_IDLE)
        .pool_max_idle_per_host(POOL_PER_HOST)
        .build(Connector { upstreams, tls: Arc::new(tls) }))
}

fn full(body: impl Into<Bytes>) -> ProxyBody {
    Full::new(body.into()).map_err(|never| match never {}).boxed_unsync()
}

fn empty() -> ProxyBody {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

fn text(status: StatusCode, body: String) -> Response<ProxyBody> {
    let mut response = Response::new(full(body));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, "text/plain; charset=utf-8".parse().expect("a header value"));
    response
}

/// A readable 403 with the reason in [`BLOCKED_HEADER`].
fn blocked(host: &str, reason: &str) -> Response<ProxyBody> {
    let mut response = text(StatusCode::FORBIDDEN, format!("zwinger blocked {host}: {reason}\n"));
    if let Ok(value) = reason.parse() {
        response.headers_mut().insert(BLOCKED_HEADER, value);
    }
    response
}

/// A response that the proxy makes itself, with `reason` in [`ERROR_HEADER`].
fn failed(status: StatusCode, reason: &str, body: String) -> Response<ProxyBody> {
    let mut response = text(status, body);
    let value = reason.parse().unwrap_or(http::HeaderValue::from_static("zwinger error"));
    response.headers_mut().insert(ERROR_HEADER, value);
    response
}

impl Shared {
    /// Compares the whole credentials in constant time.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(PROXY_AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        let Some((scheme, encoded)) = value.trim().split_once(' ') else {
            return false;
        };
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).unwrap_or_default();
        scheme.eq_ignore_ascii_case("basic") && bool::from(decoded.ct_eq(&self.expected))
    }

    fn decide(&self, policy: &NetworkPolicy, host: &str, request: Option<(&str, &str)>) -> Decision {
        let tokeninfo = self.logins.iter().any(|l| l.kind == AgentKind::Gemini);
        decide(&Context { policy, refresh: &self.refresh, tokeninfo }, host, request)
    }

    fn log(&self, entry: Entry) {
        (self.sink)(entry);
    }

    /// Logs the blocked request and answers it with a readable 403.
    fn block(&self, mut entry: Entry, reason: &str) -> Response<ProxyBody> {
        entry.status = Some(StatusCode::FORBIDDEN.as_u16());
        entry.reason = reason.to_owned();
        let response = blocked(&entry.host, reason);
        self.log(entry);
        response
    }
}

/// When the last byte moved on a guest connection.
#[derive(Clone)]
struct Activity {
    base: Instant,
    last: Arc<AtomicU64>,
}

impl Activity {
    fn new() -> Self {
        Self { base: Instant::now(), last: Arc::default() }
    }

    fn touch(&self) {
        self.last.store(self.base.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn idle(&self) -> Duration {
        self.base.elapsed().saturating_sub(Duration::from_millis(self.last.load(Ordering::Relaxed)))
    }
}

/// A stream that marks its activity and counts its bytes.
struct Tracked<T> {
    io: T,
    activity: Activity,
    read: u64,
    written: u64,
}

impl<T> Tracked<T> {
    fn new(io: T, activity: Activity) -> Self {
        Self { io, activity, read: 0, written: 0 }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Tracked<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = ready!(Pin::new(&mut self.io).poll_read(cx, buf));
        if buf.filled().len() > before {
            self.read += (buf.filled().len() - before) as u64;
            self.activity.touch();
        }
        Poll::Ready(result)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tracked<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let result = ready!(Pin::new(&mut self.io).poll_write(cx, buf));
        if let Ok(n @ 1..) = result {
            self.written += n as u64;
            self.activity.touch();
        }
        Poll::Ready(result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// Runs `work` until it ends, the connection idles or lives too long, or
/// the proxy stops.
async fn guard(mut shutdown: watch::Receiver<bool>, activity: Activity, work: impl Future) {
    let idle = async {
        loop {
            tokio::time::sleep(IDLE_LIMIT / 4).await;
            if activity.idle() >= IDLE_LIMIT {
                return;
            }
        }
    };
    tokio::select! {
        _ = work => {}
        _ = idle => {}
        _ = tokio::time::sleep(CONNECTION_LIMIT) => {}
        _ = shutdown.wait_for(|stop| *stop) => {}
    }
}

async fn serve(shared: Arc<Shared>, tcp: TcpStream, permit: Permit) {
    let activity = Activity::new();
    let io = TokioIo::new(Tracked::new(tcp, activity.clone()));
    let (service_shared, service_activity) = (shared.clone(), activity.clone());
    let service = service_fn(move |req| {
        let (shared, activity, permit) = (service_shared.clone(), service_activity.clone(), permit.clone());
        async move { Ok::<_, Infallible>(outer(shared, activity, permit, req).await) }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_LIMIT)
        .max_buf_size(HEADER_BYTES)
        .serve_connection(io, service)
        .with_upgrades();
    guard(shared.shutdown.clone(), activity, connection).await;
}

/// The host of a request target in lowercase, with no brackets and no final dot.
fn normal_host(host: &str) -> String {
    host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase()
}

/// Where a connection of the guest goes: the host that the proxy checked.
#[derive(Clone)]
struct Origin {
    https: bool,
    host: String,
    port: u16,
}

impl Origin {
    fn default_port(&self) -> u16 {
        if self.https { 443 } else { 80 }
    }

    /// `authority` names this origin.
    fn is(&self, authority: &str) -> bool {
        let Ok(authority) = authority.parse::<http::uri::Authority>() else {
            return false;
        };
        normal_host(authority.host()) == self.host && authority.port_u16().unwrap_or(self.default_port()) == self.port
    }

    fn uri(&self, path_and_query: &str) -> Option<Uri> {
        let scheme = if self.https { "https" } else { "http" };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{scheme}://{host}:{}{path_and_query}", self.port).parse().ok()
    }
}

async fn outer(shared: Arc<Shared>, activity: Activity, permit: Permit, req: Request<Incoming>) -> Response<ProxyBody> {
    if !shared.authorized(req.headers()) {
        let mut response = failed(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "proxy authentication required",
            "zwinger: proxy authentication required\n".into(),
        );
        response.headers_mut().insert(PROXY_AUTHENTICATE, "Basic realm=\"zwinger\"".parse().expect("a header value"));
        return response;
    }
    if req.method() == Method::CONNECT {
        return connect(shared, activity, permit, req).await;
    }
    match (req.uri().scheme_str(), req.uri().host()) {
        (Some("http"), Some(host)) => {
            let origin = Origin { https: false, host: normal_host(host), port: req.uri().port_u16().unwrap_or(80) };
            forward(shared, req, origin, permit).await
        }
        _ => failed(
            StatusCode::BAD_REQUEST,
            "bad request",
            "zwinger: send an absolute http:// URL, or CONNECT for https\n".into(),
        ),
    }
}

/// Decides the CONNECT on its host, then resolves the host and checks its
/// addresses, all before the `200`. The dials of the tunnel use the checked
/// addresses.
async fn connect(
    shared: Arc<Shared>,
    activity: Activity,
    permit: Permit,
    req: Request<Incoming>,
) -> Response<ProxyBody> {
    let Some(authority) = req.uri().authority() else {
        return failed(StatusCode::BAD_REQUEST, "bad request", "zwinger: CONNECT needs host:port\n".into());
    };
    let origin = Origin { https: true, host: normal_host(authority.host()), port: authority.port_u16().unwrap_or(443) };
    let policy = shared.live.snapshot().policy;
    match shared.decide(&policy, &origin.host, None) {
        Decision::Block(reason) => {
            return shared.block(Entry::new("CONNECT", &origin.host, None), &reason.to_string());
        }
        // The proxy answers the tokeninfo check itself.
        Decision::TokenInfo => {}
        Decision::Allow(_) => {
            let upstreams = &shared.upstreams;
            match dial::checked(upstreams.resolver.as_ref(), &origin.host, origin.port).await {
                Ok(addrs) => upstreams.pin(&origin.host, origin.port, addrs),
                Err(DialError::Blocked(_)) => {
                    let entry = Entry::new("CONNECT", &origin.host, None);
                    return shared.block(entry, &Reason::AlwaysBlocked.to_string());
                }
                Err(e) => {
                    let mut entry = Entry::new("CONNECT", &origin.host, None);
                    entry.status = Some(StatusCode::BAD_GATEWAY.as_u16());
                    entry.allowed = true;
                    entry.reason = "unreachable".into();
                    shared.log(entry);
                    return failed(StatusCode::BAD_GATEWAY, &e.to_string(), format!("zwinger: {e}\n"));
                }
            }
        }
    }
    let shutdown = shared.shutdown.clone();
    tokio::spawn(guard(shutdown, activity, async move {
        if let Ok(upgraded) = hyper::upgrade::on(req).await {
            mitm(shared, TokioIo::new(upgraded), origin, permit).await;
        }
    }));
    Response::new(empty())
}

/// Terminates the guest's TLS with a leaf for the CONNECT host and serves
/// its requests, which may name only that host.
async fn mitm(shared: Arc<Shared>, io: TokioIo<hyper::upgrade::Upgraded>, origin: Origin, permit: Permit) {
    let Ok(config) = shared.ca.server_config(&origin.host).await else {
        return;
    };
    let Ok(Ok(tls)) = tokio::time::timeout(TLS_LIMIT, TlsAcceptor::from(config).accept(io)).await else {
        return;
    };
    let service = service_fn(move |req| {
        let (shared, origin, permit) = (shared.clone(), origin.clone(), permit.clone());
        async move { Ok::<_, Infallible>(forward(shared, req, origin, permit).await) }
    });
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder.http1().timer(TokioTimer::new()).header_read_timeout(HEADER_LIMIT).max_buf_size(HEADER_BYTES);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_header_list_size(HEADER_BYTES as u32)
        .max_concurrent_streams(GUEST_STREAMS);
    let _ = builder.serve_connection_with_upgrades(TokioIo::new(tls), service).await;
}

/// Removes the hop-by-hop fields, and the fields that the `connection`
/// field names. A WebSocket upgrade keeps its `upgrade` and `connection`.
fn strip_hop(headers: &mut HeaderMap, websocket: bool) {
    let named: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|n| n.trim().to_ascii_lowercase()))
        .filter(|n| !(websocket && n == "upgrade"))
        .collect();
    for name in named {
        headers.remove(name.as_str());
    }
    for name in [PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING] {
        headers.remove(name);
    }
    for name in ["keep-alive", "proxy-connection"] {
        headers.remove(name);
    }
    if !websocket {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
    }
}

fn is_websocket(req: &Request<Incoming>) -> bool {
    req.version() != Version::HTTP_2
        && req.headers().get(UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The authorities that the request names: its URI and its `host` field.
fn named(parts: &http::request::Parts) -> Vec<String> {
    let mut names: Vec<String> = parts.uri.authority().map(|a| a.as_str().to_owned()).into_iter().collect();
    names.extend(parts.headers.get_all(HOST).iter().filter_map(|v| v.to_str().ok().map(str::to_owned)));
    names
}

async fn forward(
    shared: Arc<Shared>,
    mut req: Request<Incoming>,
    origin: Origin,
    permit: Permit,
) -> Response<ProxyBody> {
    let started = Instant::now();
    let websocket = is_websocket(&req);
    let guest_upgrade =
        websocket.then(|| GuestUpgrade { on: hyper::upgrade::on(&mut req), permit, shutdown: shared.shutdown.clone() });
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let mut entry = Entry::new(parts.method.as_str(), &origin.host, Some(&path));
    if named(&parts).iter().any(|a| !origin.is(a)) {
        entry.status = Some(StatusCode::MISDIRECTED_REQUEST.as_u16());
        entry.reason = "misdirected".into();
        shared.log(entry);
        return failed(
            StatusCode::MISDIRECTED_REQUEST,
            "misdirected",
            format!("zwinger: this connection goes to {} only\n", origin.host),
        );
    }
    // The host would send the request back with its headers, credentials included.
    if ["TRACE", "TRACK"].contains(&parts.method.as_str()) {
        entry.status = Some(StatusCode::METHOD_NOT_ALLOWED.as_u16());
        entry.reason = "method".into();
        shared.log(entry);
        return failed(
            StatusCode::METHOD_NOT_ALLOWED,
            "method",
            format!("zwinger refuses {}: it echoes the request\n", parts.method),
        );
    }
    let Snapshot { policy, values } = shared.live.snapshot();
    match shared.decide(&policy, &origin.host, Some((parts.method.as_str(), parts.uri.path()))) {
        Decision::Block(reason) => return shared.block(entry, &reason.to_string()),
        Decision::TokenInfo => return tokeninfo(&shared, &parts, entry, started).await,
        Decision::Allow(reason) => {
            entry.allowed = true;
            entry.reason = reason.to_string();
        }
    }

    // Only TLS to the host keeps a credential from the network on the way.
    let credentials: Vec<(Credential, SecretValue)> = policy
        .rules
        .iter()
        .filter(|r| r.sends_credentials_to(&origin.host))
        .flat_map(|r| &r.credentials)
        .filter_map(|c| values.get(&c.name).map(|v| (c.clone(), v.clone())))
        .collect();
    let logins: Vec<&Arc<Login>> = shared.logins.iter().filter(|l| l.hosts.contains(&origin.host)).collect();
    let mut injected = inject::Injected::default();
    if origin.https {
        injected = inject::apply(&mut parts, &credentials);
        for login in logins {
            let token = login.current().await;
            if login.apply(&mut parts.headers, &token) {
                injected.names.push(login.name.clone());
            }
        }
    } else if !credentials.is_empty() || !logins.is_empty() {
        entry.note = Some(PLAIN_HTTP.into());
    }
    entry.credentials = std::mem::take(&mut injected.names);

    strip_hop(&mut parts.headers, websocket);
    parts.headers.remove(HOST);
    let Some(uri) = origin.uri(parts.uri.path_and_query().map_or("/", |p| p.as_str())) else {
        return shared.block(entry, "invalid target");
    };
    parts.uri = uri;
    parts.version = Version::HTTP_11;
    parts.extensions = http::Extensions::new();
    let up = Arc::new(AtomicU64::new(0));
    let body = if injected.body.is_empty() {
        Counted { inner: body, count: up.clone() }.boxed_unsync()
    } else {
        parts.headers.remove(CONTENT_LENGTH);
        let rewrite = inject::Rewrite::new(body, injected.body);
        Counted { inner: rewrite, count: up.clone() }.boxed_unsync()
    };
    let client = if websocket { &shared.h1 } else { &shared.auto };
    let finish = Finish { entry, started, up, sink: shared.sink.clone() };
    send(client, Request::from_parts(parts, body), finish, guest_upgrade).await
}

/// The guest side of a WebSocket upgrade.
struct GuestUpgrade {
    on: hyper::upgrade::OnUpgrade,
    permit: Permit,
    shutdown: watch::Receiver<bool>,
}

/// The parts of a log entry that the end of the response completes.
struct Finish {
    entry: Entry,
    started: Instant,
    up: Arc<AtomicU64>,
    sink: Sink,
}

impl Finish {
    fn done(mut self, down: u64) {
        self.entry.up = self.up.load(Ordering::Relaxed);
        self.entry.down = down;
        self.entry.ms = self.started.elapsed().as_millis() as u64;
        (self.sink)(self.entry);
    }
}

/// Sends the request upstream and streams the response back. The entry is
/// logged when the response body ends. A dial to an always-blocked address
/// is a block.
async fn send(
    client: &Client<Connector, ProxyBody>,
    request: Request<ProxyBody>,
    mut finish: Finish,
    guest_upgrade: Option<GuestUpgrade>,
) -> Response<ProxyBody> {
    let host = finish.entry.host.clone();
    let mut response = match tokio::time::timeout(RESPONSE_LIMIT, client.request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            let response = if dial_error(&e).is_some_and(|d| matches!(d, DialError::Blocked(_))) {
                finish.entry.allowed = false;
                finish.entry.reason = Reason::AlwaysBlocked.to_string();
                blocked(&host, &finish.entry.reason)
            } else {
                let why = dial_error(&e).map_or_else(|| e.to_string(), |d| d.to_string());
                failed(StatusCode::BAD_GATEWAY, &why, format!("zwinger could not reach {host}: {why}\n"))
            };
            finish.entry.status = Some(response.status().as_u16());
            finish.done(0);
            return response;
        }
        Err(_) => {
            finish.entry.status = Some(StatusCode::GATEWAY_TIMEOUT.as_u16());
            finish.done(0);
            return failed(StatusCode::GATEWAY_TIMEOUT, "timeout", format!("zwinger: {host} did not answer in time\n"));
        }
    };
    finish.entry.status = Some(response.status().as_u16());
    if let Some(guest) = guest_upgrade
        && response.status() == StatusCode::SWITCHING_PROTOCOLS
    {
        let upstream = hyper::upgrade::on(&mut response);
        let mut answer = Response::new(empty());
        *answer.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *answer.headers_mut() = response.headers().clone();
        // The copy runs under the bounds of a guest connection, and holds its permit.
        tokio::spawn(async move {
            let GuestUpgrade { on, permit, shutdown } = guest;
            let Ok((guest, upstream)) = tokio::try_join!(on, upstream) else {
                return finish.done(0);
            };
            let activity = Activity::new();
            let mut guest = Tracked::new(TokioIo::new(guest), activity.clone());
            let mut upstream = TokioIo::new(upstream);
            guard(shutdown, activity, tokio::io::copy_bidirectional(&mut guest, &mut upstream)).await;
            drop(permit);
            finish.up.fetch_add(guest.read, Ordering::Relaxed);
            finish.done(guest.written);
        });
        return answer;
    }
    let (mut parts, body) = response.into_parts();
    strip_hop(&mut parts.headers, false);
    parts.version = Version::HTTP_11;
    let body = Metered { inner: body, down: 0, finish: Some(finish) }.boxed_unsync();
    Response::from_parts(parts, body)
}

fn dial_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a DialError> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(dial) = e.downcast_ref::<DialError>() {
            return Some(dial);
        }
        current = e.source();
    }
    None
}

/// Answers Gemini CLI's check of its cached token from the host login,
/// only for a request that carries the login's placeholder. Without a
/// scope on the Mac, it asks Google with the real token.
async fn tokeninfo(
    shared: &Shared,
    parts: &http::request::Parts,
    mut entry: Entry,
    started: Instant,
) -> Response<ProxyBody> {
    entry.reason = Reason::TokenInfo.to_string();
    let Some(login) = shared.logins.iter().find(|l| l.kind == AgentKind::Gemini) else {
        return shared.block(entry, &Reason::AlwaysBlocked.to_string());
    };
    if !inject::carries(parts, &login.placeholder()) {
        return shared.block(entry, &Reason::AlwaysBlocked.to_string());
    }
    let token = login.current().await;
    entry.allowed = true;
    entry.credentials = vec![login.name.clone()];
    if let Some(info) = login::tokeninfo(&token, log::now_ms() as i64) {
        entry.status = Some(StatusCode::OK.as_u16());
        entry.ms = started.elapsed().as_millis() as u64;
        shared.log(entry);
        let mut response = Response::new(full(info.to_string()));
        response.headers_mut().insert(CONTENT_TYPE, "application/json; charset=utf-8".parse().expect("a header value"));
        return response;
    }
    let mut request = Request::new(empty());
    *request.method_mut() = Method::POST;
    *request.uri_mut() = format!("https://{TOKENINFO_HOST}{TOKENINFO_PATH}").parse().expect("a URI");
    let Ok(mut bearer) = format!("Bearer {}", token.full_access().expose()).parse::<http::HeaderValue>() else {
        return shared.block(entry, "invalid token");
    };
    bearer.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let finish = Finish { entry, started, up: Arc::default(), sink: shared.sink.clone() };
    send(&shared.auto, request, finish, None).await
}

/// A request body that counts its bytes.
struct Counted<B> {
    inner: B,
    count: Arc<AtomicU64>,
}

impl<B: Body<Data = Bytes> + Unpin> Body for Counted<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if let Some(Ok(data)) = frame.as_ref().map(|f| f.as_ref().map(Frame::data_ref)) {
            self.count.fetch_add(data.map_or(0, |d| d.len() as u64), Ordering::Relaxed);
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// A response body that counts its bytes and logs the entry at its end,
/// also when the guest drops it early.
struct Metered<B> {
    inner: B,
    down: u64,
    finish: Option<Finish>,
}

impl<B: Body<Data = Bytes> + Unpin> Body for Metered<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(cx));
        match &frame {
            Some(Ok(f)) => self.down += f.data_ref().map_or(0, |d| d.len() as u64),
            Some(Err(_)) | None => {
                let down = self.down;
                if let Some(finish) = self.finish.take() {
                    finish.done(down);
                }
            }
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for Metered<B> {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish.done(self.down);
        }
    }
}

/// What the upstream connections of one proxy share.
struct Upstreams {
    resolver: Arc<dyn Resolve>,
    limits: Limits,
    /// The checked addresses of the CONNECT hosts, by host and port.
    pins: Mutex<LruCache<(String, u16), Vec<SocketAddr>>>,
}

impl Upstreams {
    fn pin(&self, host: &str, port: u16, addrs: Vec<SocketAddr>) {
        self.pins.lock().unwrap_or_else(PoisonError::into_inner).put((host.to_owned(), port), addrs);
    }

    fn pinned(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        self.pins.lock().unwrap_or_else(PoisonError::into_inner).get(&(host.to_owned(), port)).cloned()
    }
}

/// Connects the upstream client: the permits, then [`dial::dial`], then TLS
/// with the Mac's trust store for `https`.
#[derive(Clone)]
struct Connector {
    upstreams: Arc<Upstreams>,
    tls: Arc<ClientConfig>,
}

#[derive(Debug, thiserror::Error)]
enum ConnectError {
    #[error("{0}")]
    Dial(#[source] DialError),
    #[error("TLS: {0}")]
    Tls(#[source] io::Error),
    #[error("the TLS handshake did not end in time")]
    Timeout,
    #[error("the URI has no valid host")]
    Name,
}

impl tower_service::Service<Uri> for Connector {
    type Response = Upstream;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Upstream, ConnectError>> + Send>>;

    fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), ConnectError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let (upstreams, tls) = (self.upstreams.clone(), self.tls.clone());
        Box::pin(async move {
            let host = normal_host(uri.host().ok_or(ConnectError::Name)?);
            let https = uri.scheme_str() == Some("https");
            let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
            let permits = upstreams.limits.acquire(&host).await.map_err(ConnectError::Dial)?;
            let pinned = if https { upstreams.pinned(&host, port) } else { None };
            let tcp = dial::dial(upstreams.resolver.as_ref(), &host, port, pinned).await.map_err(ConnectError::Dial)?;
            if !https {
                return Ok(Upstream { io: UpstreamIo::Plain(TokioIo::new(tcp)), _permits: permits });
            }
            let name = ServerName::try_from(host).map_err(|_| ConnectError::Name)?;
            let stream = tokio::time::timeout(TLS_LIMIT, TlsConnector::from(tls).connect(name, tcp))
                .await
                .map_err(|_| ConnectError::Timeout)?
                .map_err(ConnectError::Tls)?;
            let h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
            Ok(Upstream { io: UpstreamIo::Tls(Box::new(TokioIo::new(stream)), h2), _permits: permits })
        })
    }
}

/// An upstream connection. It holds its permits until it closes, also
/// while it is idle in the pool or carries a WebSocket.
struct Upstream {
    io: UpstreamIo,
    _permits: Permits,
}

enum UpstreamIo {
    Plain(TokioIo<TcpStream>),
    /// The flag: ALPN chose HTTP/2.
    Tls(Box<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>>, bool),
}

impl Connection for Upstream {
    fn connected(&self) -> Connected {
        match self.io {
            UpstreamIo::Tls(_, true) => Connected::new().negotiated_h2(),
            _ => Connected::new(),
        }
    }
}

impl hyper::rt::Read for Upstream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().io {
            UpstreamIo::Plain(io) => Pin::new(io).poll_read(cx, buf),
            UpstreamIo::Tls(io, _) => Pin::new(io.as_mut()).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Upstream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().io {
            UpstreamIo::Plain(io) => Pin::new(io).poll_write(cx, buf),
            UpstreamIo::Tls(io, _) => Pin::new(io.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().io {
            UpstreamIo::Plain(io) => Pin::new(io).poll_flush(cx),
            UpstreamIo::Tls(io, _) => Pin::new(io.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().io {
            UpstreamIo::Plain(io) => Pin::new(io).poll_shutdown(cx),
            UpstreamIo::Tls(io, _) => Pin::new(io.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use http_body_util::BodyExt;
    use rustls::RootCertStore;

    use super::*;
    use crate::host_agents::login::{self as host_login};
    use crate::network::{Action, DefaultAction, Inject, Rule, Target};
    use crate::secrets::fake::MemoryVault;
    use dial::fake::FakeResolver;
    use inject::TokenSource;

    struct NoLogin;

    impl TokenSource for NoLogin {
        fn token(&self, kind: AgentKind) -> crate::error::Result<host_login::HostToken> {
            Err(crate::error::Error::HostLoginMissing(kind.name().into()))
        }
    }

    /// An upstream on loopback that answers with what it got.
    async fn upstream() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let service = service_fn(|req: Request<Incoming>| async move {
                    let head = format!(
                        "{} {} host={:?} auth={:?} proxy={:?}",
                        req.method(),
                        req.uri(),
                        req.headers().get(HOST),
                        req.headers().get(AUTHORIZATION),
                        req.headers().get(PROXY_AUTHORIZATION),
                    );
                    let body = req.into_body().collect().await.unwrap().to_bytes();
                    let text = format!("{head} body={}", String::from_utf8_lossy(&body));
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(text))))
                });
                tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), service));
            }
        });
        port
    }

    struct Fixture {
        proxy: Proxy,
        ca: Arc<Ca>,
        live: Arc<Live>,
        log: Arc<Mutex<Vec<Entry>>>,
    }

    impl Fixture {
        fn token(&self) -> String {
            let url = self.proxy.url();
            url["http://zw:".len()..url.find('@').unwrap()].to_owned()
        }

        fn auth(&self) -> String {
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("zw:{}", self.token())))
        }

        fn entries(&self) -> Vec<Entry> {
            self.log.lock().unwrap().clone()
        }
    }

    async fn fixture(policy: NetworkPolicy, logins: Vec<Arc<Login>>, refresh: Vec<&'static str>) -> Fixture {
        let ca = Arc::new(
            Ca::load(&MemoryVault::default(), &std::env::temp_dir().join(uuid::Uuid::new_v4().to_string())).unwrap(),
        );
        let resolver = FakeResolver { loopback: true, ..FakeResolver::default() };
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        resolver.names.lock().unwrap().insert("api.test".into(), vec![ip("127.0.0.1")]);
        resolver.names.lock().unwrap().insert("rebind.test".into(), vec![ip("10.0.0.1")]);
        let live = Live::new(policy, [("KEY".to_owned(), SecretValue::new("sk-1"))].into());
        let log: Arc<Mutex<Vec<Entry>>> = Arc::default();
        let sink_log = log.clone();
        let config = Config {
            ca: ca.clone(),
            live: live.clone(),
            logins,
            refresh,
            sink: Arc::new(move |entry| sink_log.lock().unwrap().push(entry)),
            resolver: Arc::new(resolver),
        };
        Fixture { proxy: Proxy::start(config).await.unwrap(), ca, live, log }
    }

    fn rule(host: &str, action: Action) -> Rule {
        Rule::new(Target::Host { host: host.into() }, action)
    }

    /// Sends one HTTP/1.1 request over `io` and returns the status and body.
    async fn send_over<T>(io: T, request: Request<Empty<Bytes>>) -> (StatusCode, HeaderMap, String)
    where
        T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(connection);
        let response = sender.send_request(request).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    async fn plain(f: &Fixture, uri: &str, auth: Option<String>) -> (StatusCode, HeaderMap, String) {
        let tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let mut request = Request::get(uri);
        if let Some(auth) = auth {
            request = request.header(PROXY_AUTHORIZATION, auth);
        }
        send_over(TokioIo::new(tcp), request.body(Empty::new()).unwrap()).await
    }

    /// A CONNECT to `authority`, then TLS with the zwinger CA as the only root.
    async fn tunnel(
        f: &Fixture,
        authority: &str,
    ) -> Result<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>, StatusCode> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let connect =
            format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: {}\r\n\r\n", f.auth());
        tcp.write_all(connect.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            tcp.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let status: u16 = String::from_utf8_lossy(&head)[9..12].parse().unwrap();
        if status != 200 {
            return Err(StatusCode::from_u16(status).unwrap());
        }
        let mut roots = RootCertStore::empty();
        roots.add(f.ca.der().clone()).unwrap();
        let config = ClientConfig::builder_with_provider(ca::provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let host = authority.rsplit_once(':').unwrap().0.to_owned();
        let tls = TlsConnector::from(Arc::new(config)).connect(ServerName::try_from(host).unwrap(), tcp).await.unwrap();
        Ok(TokioIo::new(tls))
    }

    #[tokio::test]
    async fn a_request_without_the_proxy_token_gets_407() {
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        let port = upstream().await;
        let uri = format!("http://api.test:{port}/");
        let (status, headers, _) = plain(&f, &uri, None).await;
        assert_eq!(status, StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        assert_eq!(headers[PROXY_AUTHENTICATE], "Basic realm=\"zwinger\"");
        let wrong = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("zw:not-the-token"));
        assert_eq!(plain(&f, &uri, Some(wrong)).await.0, StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        let short = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("zw:"));
        assert_eq!(plain(&f, &uri, Some(short)).await.0, StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        assert_eq!(plain(&f, &uri, Some(f.auth())).await.0, StatusCode::OK);
        assert!(f.entries().iter().all(|e| !format!("{e:?}").contains(&f.token())));
    }

    #[tokio::test]
    async fn plain_http_is_decided_and_logged_without_values_and_gets_no_credential() {
        let mut allowed = rule("api.test", Action::Allow);
        allowed.credentials.push(Credential {
            name: "KEY".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
        });
        let policy = NetworkPolicy { default: DefaultAction::Block, offline: false, rules: vec![allowed] };
        let f = fixture(policy, Vec::new(), Vec::new()).await;
        let port = upstream().await;

        let (status, headers, body) = plain(&f, &format!("http://api.test:{port}/v1?q=private"), Some(f.auth())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(BLOCKED_HEADER).is_none() && headers.get(ERROR_HEADER).is_none());
        // With no TLS to the host, the credential stays out.
        assert!(body.contains("auth=None"), "{body}");
        assert!(body.contains("proxy=None"), "the proxy token leaked upstream: {body}");
        // The rule list changes at once, and Block beats Allow.
        f.live.edit(|p, _| p.rules.push(rule("*.test", Action::Block)));
        let (status, headers, body) = plain(&f, &format!("http://api.test:{port}/"), Some(f.auth())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "zwinger blocked api.test: rule:*.test\n");
        assert_eq!(headers[BLOCKED_HEADER], "rule:*.test");
        let (status, _, body) = plain(&f, "http://other.example/", Some(f.auth())).await;
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "zwinger blocked other.example: default\n"));

        let entries = f.entries();
        let first = &entries[0];
        assert_eq!(
            (first.method.as_deref(), first.host.as_str(), first.path.as_deref()),
            (Some("GET"), "api.test", Some("/v1?q"))
        );
        assert_eq!((first.status, first.allowed, first.reason.as_str()), (Some(200), true, "rule:api.test"));
        assert!(first.credentials.is_empty());
        assert_eq!(first.note.as_deref(), Some(PLAIN_HTTP));
        assert!(first.down > 0);
        let text = format!("{entries:?}");
        assert!(!text.contains("sk-1") && !text.contains("private"), "{text}");
        assert_eq!(entries[1].reason, "rule:*.test");
        assert_eq!(entries[2].reason, "default");
    }

    #[tokio::test]
    async fn an_idle_pooled_upstream_keeps_its_permits_until_it_closes() {
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        let port = upstream().await;
        assert_eq!(plain(&f, &format!("http://api.test:{port}/"), Some(f.auth())).await.0, StatusCode::OK);
        // The connection is idle in the pool, and still counts.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.proxy.upstreams.limits.used(), 1);
        assert_eq!(plain(&f, &format!("http://api.test:{port}/"), Some(f.auth())).await.0, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.proxy.upstreams.limits.used(), 1);
    }

    #[tokio::test]
    async fn plain_http_to_port_443_gets_no_credential() {
        let mut allowed = rule("api.test", Action::Allow);
        allowed.credentials.push(Credential {
            name: "KEY".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
        });
        let f = fixture(NetworkPolicy { rules: vec![allowed], ..Default::default() }, Vec::new(), Vec::new()).await;
        plain(&f, "http://api.test:443/", Some(f.auth())).await;
        let entry = f.entries().pop().unwrap();
        assert!(entry.credentials.is_empty());
        assert_eq!(entry.note.as_deref(), Some(PLAIN_HTTP));
    }

    #[tokio::test]
    async fn responses_of_the_proxy_carry_their_reason() {
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        // Nothing listens on this port of api.test: the proxy answers 502.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let (status, headers, _) = plain(&f, &format!("http://api.test:{closed}/"), Some(f.auth())).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(headers[ERROR_HEADER].to_str().unwrap().starts_with("cannot connect to api.test"), "{headers:?}");
        let port = upstream().await;
        let tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let request = Request::builder()
            .method("TRACE")
            .uri(format!("http://api.test:{port}/"))
            .header(PROXY_AUTHORIZATION, f.auth())
            .body(Empty::new())
            .unwrap();
        let (status, headers, _) = send_over(TokioIo::new(tcp), request).await;
        assert_eq!((status, headers[ERROR_HEADER].to_str().unwrap()), (StatusCode::METHOD_NOT_ALLOWED, "method"));
        assert_eq!(plain(&f, "/relative", Some(f.auth())).await.1[ERROR_HEADER], "bad request");
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_a_private_address_is_blocked_at_the_dial() {
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        let (status, headers, body) = plain(&f, "http://rebind.test/", Some(f.auth())).await;
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "zwinger blocked rebind.test: always-blocked\n"));
        assert_eq!(headers[BLOCKED_HEADER], "always-blocked");
        let entry = f.entries().pop().unwrap();
        assert_eq!((entry.allowed, entry.reason.as_str()), (false, "always-blocked"));
        assert_eq!(plain(&f, "http://10.0.0.1/", Some(f.auth())).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn connect_checks_the_addresses_before_its_answer_and_pins_them() {
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        assert_eq!(tunnel(&f, "rebind.test:443").await.err(), Some(StatusCode::FORBIDDEN));
        let entry = f.entries().pop().unwrap();
        assert_eq!((entry.method.as_deref(), entry.reason.as_str()), (Some("CONNECT"), "always-blocked"));
        assert_eq!(tunnel(&f, "unknown.test:443").await.err(), Some(StatusCode::BAD_GATEWAY));
        assert!(tunnel(&f, "api.test:443").await.is_ok());
        let pinned = f.proxy.upstreams.pinned("api.test", 443).unwrap();
        assert_eq!(pinned, [SocketAddr::from(([127, 0, 0, 1], 443))]);
    }

    #[tokio::test]
    async fn connect_is_decided_on_its_host_and_the_inner_requests_are_bound_to_it() {
        let policy = NetworkPolicy {
            default: DefaultAction::Block,
            offline: false,
            rules: vec![rule("api.test", Action::Allow)],
        };
        let f = fixture(policy, Vec::new(), Vec::new()).await;
        assert_eq!(tunnel(&f, "blocked.example:443").await.err(), Some(StatusCode::FORBIDDEN));
        assert_eq!(f.entries()[0].method.as_deref(), Some("CONNECT"));

        // A request in the tunnel that names another host is refused before any dial.
        let io = tunnel(&f, "api.test:443").await.unwrap();
        let request = Request::get("/").header(HOST, "evil.example").body(Empty::new()).unwrap();
        let (status, _, _) = send_over(io, request).await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
        let io = tunnel(&f, "api.test:443").await.unwrap();
        let request = Request::get("/").header(HOST, "api.test:8443").body(Empty::new()).unwrap();
        assert_eq!(send_over(io, request).await.0, StatusCode::MISDIRECTED_REQUEST);
        assert_eq!(f.entries().last().unwrap().reason, "misdirected");
    }

    /// Waits until `f` is true, for at most two seconds.
    async fn eventually(f: impl Fn() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_tunnel_holds_its_permit_until_it_ends_and_the_stop_ends_it() {
        use tokio::io::AsyncReadExt;
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        let io = tunnel(&f, "api.test:443").await.unwrap();
        // The outer connection handed its stream to the tunnel, which keeps the permit.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(f.proxy.open(), 1);
        drop(io);
        assert!(eventually(|| f.proxy.open() == 0).await);

        let mut tls = tunnel(&f, "api.test:443").await.unwrap().into_inner();
        let Fixture { proxy, .. } = f;
        drop(proxy);
        let read = tokio::time::timeout(Duration::from_secs(2), tls.read(&mut [0; 16])).await;
        assert!(matches!(read, Ok(Ok(0) | Err(_))), "the tunnel stayed open after the stop: {read:?}");
    }

    fn gemini_login() -> Arc<Login> {
        let doc = serde_json::json!({ "access_token": "ya29.real", "expiry_date": log::now_ms() as i64 + 3_600_000, "scope": "openid email" });
        let token = host_login::parse(AgentKind::Gemini, &doc.to_string()).unwrap();
        let name = host_login::secret_name(AgentKind::Gemini).to_owned();
        Arc::new(Login::new(name, vec!["cloudcode-pa.googleapis.com".into()], token, Arc::new(NoLogin)))
    }

    #[tokio::test]
    async fn the_gemini_tokeninfo_check_is_answered_and_its_host_stays_blocked() {
        let policy = NetworkPolicy { default: DefaultAction::Allow, ..Default::default() };
        let f = fixture(policy, vec![gemini_login()], vec![TOKENINFO_HOST]).await;
        let placeholder = "Bearer $MSB_GEMINI_HOST_LOGIN$";

        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let request = Request::post("/tokeninfo").header(HOST, TOKENINFO_HOST).header(AUTHORIZATION, placeholder);
        let (status, _, body) = send_over(io, request.body(Empty::new()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(info["scope"], "openid email");
        assert!(!body.contains("ya29"));
        let entry = f.entries().pop().unwrap();
        assert_eq!((entry.reason.as_str(), entry.allowed), ("tokeninfo", true));

        // The refresh endpoint and a request with no placeholder stay blocked.
        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let request = Request::post("/token").header(HOST, TOKENINFO_HOST).body(Empty::new()).unwrap();
        assert_eq!(send_over(io, request).await.0, StatusCode::FORBIDDEN);
        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let request =
            Request::get("/tokeninfo?access_token=mine").header(HOST, TOKENINFO_HOST).body(Empty::new()).unwrap();
        assert_eq!(send_over(io, request).await.0, StatusCode::FORBIDDEN);
        assert!(f.entries().iter().all(|e| !format!("{e:?}").contains("ya29")));
    }
}
