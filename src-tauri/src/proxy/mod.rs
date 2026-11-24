//! The egress proxy of a running cage (ADR 0003). It listens on
//! `127.0.0.1` on a free port, the only host port that the VM can reach.
//! Each request needs the cage's proxy token. A CONNECT is decided on its
//! host and its checked addresses, then terminated with a leaf of the zwinger
//! CA, so each inner request is decided, logged and gets its credentials.
//! Plain HTTP is decided and logged the same, but gets no credentials.
//! Every upstream connection goes through [`dial::dial`], except the host
//! port grant (ADR 0009): plain HTTP to the gateway name on the granted port
//! goes to `127.0.0.1:<port>` through [`LocalConnector`] alone. Its CONNECT
//! carries plain HTTP only.

pub mod ask;
pub mod authority;
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
use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroUsize};
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
use hyper_util::client::legacy::connect::{Connect, Connected, Connection};
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
use crate::error::Error;
use crate::host_agents::login::{self, TOKENINFO_HOST};
use crate::network::{Action, Credential, NetworkPolicy, Rule, Target};
use crate::secrets::{CredentialRef, SecretValue};
use ca::Ca;
use decide::{Context, Decision, Reason, decide};
use dial::{DialError, Limits, Permits, Resolve};
use inject::Login;
use log::{Entry, Sink};

/// The name of the Mac in the guest. It maps to the host's `127.0.0.1`.
pub const GUEST_HOST: &str = "host.microsandbox.internal";
/// The user name in the proxy URL. Only the token counts.
const USER: &str = "zw";
/// The connections from one cage at once. More are closed at once.
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
/// The log note of TLS in a CONNECT to the host port: the grant is plain HTTP only.
const HOST_PORT_HTTP_ONLY: &str = "the host port takes plain http only";
/// The log note of an upgrade request to the host port.
const HOST_PORT_NO_UPGRADE: &str = "the host port takes no upgrade";
/// The log note of a request to the host port with another method.
const HOST_PORT_METHOD: &str = "the host port takes GET, HEAD, POST, PUT, DELETE, OPTIONS and PATCH only";
/// The log note of a request to the host port that does not name its authority.
const HOST_PORT_AUTHORITY: &str =
    "the request must name http://host.microsandbox.internal:<port>; it can omit the port only for port 80";
/// The log note of a tokeninfo check that the proxy answers itself.
const TOKENINFO_LOCAL: &str = "answered by zwinger";
/// The log note of a tokeninfo check that goes to Google with the real token.
const TOKENINFO_GOOGLE: &str = "sent to Google";
/// The 502 reason of a tokeninfo check that did not reach Google. It is
/// fixed, so no part of the request, and no token, gets into it.
const TOKENINFO_UNREACHABLE: &str = "the tokeninfo endpoint did not answer";
/// The most Allow rules until stop of one VM.
pub const TEMPORARY_LIMIT: usize = 128;
/// The methods of the host port. A CONNECT or an unknown method could make
/// the local service a raw tunnel.
const HOST_PORT_METHODS: [Method; 7] =
    [Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::DELETE, Method::OPTIONS, Method::PATCH];

pub type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;
/// One of [`MAX_CONNECTIONS`]. A guest connection holds it, then the tunnel
/// or the WebSocket that it becomes, until the last of them ends.
type Permit = Arc<OwnedSemaphorePermit>;

/// The policy and the credential values that one request uses together.
#[derive(Clone)]
pub struct Snapshot {
    pub policy: Arc<NetworkPolicy>,
    /// The credential values by source and name.
    pub values: Arc<HashMap<CredentialRef, SecretValue>>,
    /// The Allow rules until stop of this VM. They have no credentials.
    pub temporary: Arc<Vec<Rule>>,
    /// The host logins of the VM's host agents.
    pub logins: Arc<Vec<Arc<Login>>>,
    /// The refresh hosts of the host logins.
    pub refresh: Arc<Vec<&'static str>>,
}

/// The parts of a VM's proxy that edits change while it runs: the next
/// request uses them. An edit replaces the policy and the values in one
/// step, so no request sees a new rule with an old value. Each start makes
/// a new one, so the rules until stop and the asks end with the VM.
pub struct Live {
    snapshot: Mutex<Snapshot>,
    /// Changes at each edit, so a held request decides again.
    version: watch::Sender<u64>,
    pub asks: Arc<ask::Registry>,
}

impl Live {
    pub fn new(policy: NetworkPolicy, values: HashMap<CredentialRef, SecretValue>) -> Arc<Self> {
        let snapshot = Snapshot {
            policy: Arc::new(policy),
            values: Arc::new(values),
            temporary: Arc::default(),
            logins: Arc::default(),
            refresh: Arc::default(),
        };
        Arc::new(Self { snapshot: Mutex::new(snapshot), version: watch::channel(0).0, asks: Arc::default() })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Changes the policy and the values together. The rules until stop stay.
    pub fn edit(&self, edit: impl FnOnce(&mut NetworkPolicy, &mut HashMap<CredentialRef, SecretValue>)) {
        {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
            let Snapshot { policy, values, .. } = &mut *snapshot;
            edit(Arc::make_mut(policy), Arc::make_mut(values));
        }
        self.version.send_modify(|v| *v += 1);
    }

    /// Replaces the host logins and their refresh hosts. The next request uses them.
    pub fn set_logins(&self, logins: Vec<Arc<Login>>, refresh: Vec<&'static str>) {
        {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
            snapshot.logins = Arc::new(logins);
            snapshot.refresh = Arc::new(refresh);
        }
        self.version.send_modify(|v| *v += 1);
    }

    pub fn logins(&self) -> Arc<Vec<Arc<Login>>> {
        self.snapshot().logins
    }

    /// Changes at each edit.
    fn changes(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    pub fn temporary(&self) -> Vec<Rule> {
        self.snapshot().temporary.to_vec()
    }

    /// Adds an Allow rule until stop for the normalized `target`, or returns
    /// the one that it has. It is never saved.
    pub fn allow_until_stop(&self, target: Target) -> crate::error::Result<Rule> {
        let rule = {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(rule) = snapshot.temporary.iter().find(|r| r.target == target) {
                return Ok(rule.clone());
            }
            if snapshot.temporary.len() >= TEMPORARY_LIMIT {
                return Err(Error::InvalidRule(format!("a VM has at most {TEMPORARY_LIMIT} rules until stop")));
            }
            let rule = Rule::new(target, Action::Allow);
            Arc::make_mut(&mut snapshot.temporary).push(rule.clone());
            rule
        };
        self.version.send_modify(|v| *v += 1);
        Ok(rule)
    }

    pub fn remove_temporary(&self, rule_id: &str) -> crate::error::Result<()> {
        {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
            let i = snapshot.temporary.iter().position(|r| r.id == rule_id).ok_or(Error::RuleNotFound)?;
            Arc::make_mut(&mut snapshot.temporary).remove(i);
        }
        self.version.send_modify(|v| *v += 1);
        Ok(())
    }

    pub fn set_value(&self, key: &CredentialRef, value: Option<SecretValue>) {
        self.edit(|_, values| {
            match value {
                Some(value) => values.insert(key.clone(), value),
                None => values.remove(key),
            };
        });
    }
}

pub struct Config {
    pub ca: Arc<Ca>,
    pub live: Arc<Live>,
    /// The first host logins of `live` ([`Live::set_logins`]).
    pub logins: Vec<Arc<Login>>,
    /// The refresh hosts of the host logins.
    pub refresh: Vec<&'static str>,
    pub sink: Sink,
    pub resolver: Arc<dyn Resolve>,
    /// The pending asks of the app, [`ask::APP_PENDING`] at most.
    pub asks: Arc<Semaphore>,
    /// Gets a signal when the pending asks of this VM change.
    pub ask_sink: Arc<dyn Fn() + Send + Sync>,
    /// The only root for the upstream TLS in place of the platform roots,
    /// so a test can run a local TLS recipient.
    #[cfg(test)]
    pub upstream_root: Option<rustls::pki_types::CertificateDer<'static>>,
    /// In place of [`ask::DEADLINE`], so a test does not wait 30 s.
    #[cfg(test)]
    pub ask_deadline: Duration,
}

/// A running proxy. A drop stops it and its connections.
pub struct Proxy {
    pub port: u16,
    token: String,
    pub live: Arc<Live>,
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
    sink: Sink,
    upstreams: Arc<Upstreams>,
    /// HTTP/2 or HTTP/1.1 by ALPN.
    auto: Client<Connector, ProxyBody>,
    /// HTTP/1.1 only, for WebSocket upgrades.
    h1: Client<Connector, ProxyBody>,
    /// The host port grant only. It keeps no idle connection, so no
    /// connection outlives a removed grant.
    local: Client<LocalConnector, ProxyBody>,
    /// The port of this proxy, which the grant never opens.
    port: u16,
    shutdown: watch::Receiver<bool>,
    asks: Arc<Semaphore>,
    ask_sink: Arc<dyn Fn() + Send + Sync>,
    ask_deadline: Duration,
}

impl Proxy {
    pub async fn start(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let (shutdown, shutdown_rx) = watch::channel(false);
        config.live.set_logins(config.logins, config.refresh);
        let upstreams = Arc::new(Upstreams {
            resolver: config.resolver,
            limits: Limits::project(),
            pins: Mutex::new(LruCache::new(NonZeroUsize::new(PINS).expect("not zero"))),
        });
        let shared = Arc::new(Shared {
            expected: format!("{USER}:{token}").into_bytes(),
            ca: config.ca,
            live: config.live.clone(),
            sink: config.sink,
            auto: client(
                upstreams.clone(),
                &[b"h2", b"http/1.1"],
                #[cfg(test)]
                config.upstream_root.as_ref(),
            )?,
            h1: client(
                upstreams.clone(),
                &[b"http/1.1"],
                #[cfg(test)]
                config.upstream_root.as_ref(),
            )?,
            local: Client::builder(TokioExecutor::new())
                .timer(TokioTimer::new())
                .pool_max_idle_per_host(0)
                .build(LocalConnector { upstreams: upstreams.clone() }),
            port,
            upstreams,
            shutdown: shutdown_rx,
            asks: config.asks,
            ask_sink: config.ask_sink,
            #[cfg(test)]
            ask_deadline: config.ask_deadline,
            #[cfg(not(test))]
            ask_deadline: ask::DEADLINE,
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
            live: config.live,
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
        self.live.logins().iter().filter_map(|l| l.expires()).min()
    }

    /// The guest connections, tunnels and WebSockets that hold a permit.
    #[cfg(test)]
    fn open(&self) -> usize {
        MAX_CONNECTIONS - self.permits.available_permits()
    }
}

fn client(
    upstreams: Arc<Upstreams>,
    alpn: &[&[u8]],
    #[cfg(test)] root: Option<&rustls::pki_types::CertificateDer<'static>>,
) -> io::Result<Client<Connector, ProxyBody>> {
    let builder = ClientConfig::builder_with_provider(ca::provider())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?;
    #[cfg(test)]
    let mut tls = match root {
        Some(root) => {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(root.clone()).map_err(io::Error::other)?;
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        None => builder.with_platform_verifier().map_err(io::Error::other)?.with_no_client_auth(),
    };
    #[cfg(not(test))]
    let mut tls = builder.with_platform_verifier().map_err(io::Error::other)?.with_no_client_auth();
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

/// An authority that [`authority::parse`] refuses. Nothing is decided or
/// logged: the proxy has no host that it can name.
fn invalid_target() -> Response<ProxyBody> {
    failed(
        StatusCode::BAD_REQUEST,
        "invalid target",
        "zwinger: the target host or port is not valid (no userinfo, percent-encoding or short IP forms)\n".into(),
    )
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

    fn decide(&self, snapshot: &Snapshot, origin: &Origin, request: Option<(&str, &str)>) -> Decision {
        let tokeninfo = snapshot.logins.iter().any(|l| l.kind == AgentKind::Gemini);
        let context = Context {
            policy: &snapshot.policy,
            temporary: &snapshot.temporary,
            refresh: &snapshot.refresh,
            tokeninfo,
            listener: self.port,
        };
        decide(&context, &origin.host, origin.port, request)
    }

    /// Decides the request on the current snapshot, and returns both.
    fn decide_now(&self, origin: &Origin, request: (&str, &str)) -> (Decision, Snapshot) {
        let snapshot = self.live.snapshot();
        (self.decide(&snapshot, origin, Some(request)), snapshot)
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

/// Where a connection of the guest goes: the host that the proxy checked.
#[derive(Clone)]
struct Origin {
    https: bool,
    host: String,
    port: u16,
}

impl Origin {
    /// The origin that [`authority::parse`] reads from `raw`. `None` for an
    /// authority that it refuses.
    fn read(https: bool, raw: &str) -> Option<Self> {
        let default_port = if https { 443 } else { 80 };
        let authority::Authority { host, port } = authority::parse(raw, default_port).ok()?;
        Some(Self { https, host, port })
    }

    /// `authority` names this origin.
    fn is(&self, authority: &str) -> bool {
        Self::read(self.https, authority).is_some_and(|o| o.host == self.host && o.port == self.port)
    }

    /// A log entry for a request to this origin. The gateway name carries
    /// its port, as the host port grant opens one port of it.
    fn entry(&self, method: &str, path: Option<&str>) -> Entry {
        let mut entry = Entry::new(method, &self.host, path);
        entry.port = (self.host == GUEST_HOST).then_some(self.port);
        entry
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
    match (req.uri().scheme_str(), req.uri().authority()) {
        (Some("http"), Some(authority)) => match Origin::read(false, authority.as_str()) {
            Some(origin) => forward(shared, req, origin, permit, None).await,
            None => invalid_target(),
        },
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
    let Some(origin) = Origin::read(true, authority.as_str()) else {
        return invalid_target();
    };
    let snapshot = shared.live.snapshot();
    let host_port = match shared.decide(&snapshot, &origin, None) {
        Decision::Block(reason) => {
            return shared.block(origin.entry("CONNECT", None), &reason.to_string());
        }
        Decision::HostPort(port) if !names_gateway(authority.as_str(), port) => {
            let mut entry = origin.entry("CONNECT", None);
            entry.note = Some(HOST_PORT_AUTHORITY.into());
            return shared.block(entry, &Reason::HostPort.to_string());
        }
        Decision::HostPort(_) => true,
        // Each inner request is decided again; one that goes to Google dials checked addresses.
        Decision::TokenInfo => false,
        // An Ask holds the first inner request that no rule decides, where the method is known.
        Decision::Allow(_) | Decision::Ask => {
            let upstreams = &shared.upstreams;
            match dial::checked(upstreams.resolver.as_ref(), &origin.host, origin.port).await {
                Ok(addrs) => upstreams.pin(&origin.host, origin.port, addrs),
                Err(DialError::Blocked(_)) => {
                    let entry = origin.entry("CONNECT", None);
                    return shared.block(entry, &Reason::AlwaysBlocked.to_string());
                }
                Err(e) => {
                    let mut entry = origin.entry("CONNECT", None);
                    entry.status = Some(StatusCode::BAD_GATEWAY.as_u16());
                    entry.allowed = true;
                    entry.reason = "unreachable".into();
                    shared.log(entry);
                    return failed(StatusCode::BAD_GATEWAY, &e.to_string(), format!("zwinger: {e}\n"));
                }
            }
            false
        }
    };
    let shutdown = shared.shutdown.clone();
    tokio::spawn(guard(shutdown, activity, async move {
        if let Ok(upgraded) = hyper::upgrade::on(req).await {
            let io = TokioIo::new(upgraded);
            if host_port {
                local_tunnel(shared, io, origin, permit).await;
            } else {
                mitm(shared, io, origin, permit).await;
            }
        }
    }));
    Response::new(empty())
}

/// A CONNECT to the host port. Node's fetch sends plain HTTP through such a
/// tunnel too, so the tunnel serves plain HTTP/1.1 and decides each request
/// again. TLS in it is closed and logged: the proxy does not silently turn
/// the guest's HTTPS into plain HTTP.
async fn local_tunnel(shared: Arc<Shared>, mut io: TokioIo<hyper::upgrade::Upgraded>, origin: Origin, permit: Permit) {
    use tokio::io::AsyncReadExt;
    let mut first = [0];
    let Ok(Ok(1)) = tokio::time::timeout(HEADER_LIMIT, io.read(&mut first)).await else {
        return;
    };
    // 0x16: the record type of a TLS handshake.
    if first[0] == 0x16 {
        let mut entry = origin.entry("CONNECT", None);
        entry.note = Some(HOST_PORT_HTTP_ONLY.into());
        shared.block(entry, &Reason::HostPort.to_string());
        return;
    }
    let plain = Origin { https: false, ..origin };
    let service = service_fn(move |req| {
        let (shared, plain, permit) = (shared.clone(), plain.clone(), permit.clone());
        async move { Ok::<_, Infallible>(forward(shared, req, plain, permit, None).await) }
    });
    let io = Prefixed { first: Some(first[0]), io };
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_LIMIT)
        .max_buf_size(HEADER_BYTES)
        .serve_connection(TokioIo::new(io), service)
        .await;
}

/// A stream that gives back one byte that was read from it first.
struct Prefixed<T> {
    first: Option<u8>,
    io: T,
}

impl<T: AsyncRead + Unpin> AsyncRead for Prefixed<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Some(byte) = self.first.take() {
            buf.put_slice(&[byte]);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Prefixed<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
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
    let tunnel = Arc::new(Tunnel::default());
    let service = service_fn(move |req| {
        let (shared, origin, permit, tunnel) = (shared.clone(), origin.clone(), permit.clone(), tunnel.clone());
        async move { Ok::<_, Infallible>(forward(shared, req, origin, permit, Some(tunnel)).await) }
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

/// The request names `origin` and no other authority: in its URI and in
/// one `host` field at most. A `host` value that is not text fails.
fn names_only(parts: &http::request::Parts, origin: &Origin) -> bool {
    let hosts = parts.headers.get_all(HOST);
    if hosts.iter().count() > 1 {
        return false;
    }
    let uri = parts.uri.authority().map(|a| Some(a.as_str()));
    let fields = hosts.iter().map(|v| v.to_str().ok());
    uri.into_iter().chain(fields).all(|name| name.is_some_and(|n| origin.is(n)))
}

/// The answer to the ask of one MITM tunnel. Its later requests that the
/// Ask default decides take it, so HTTP/2 asks once per tunnel, not once
/// per stream. Each request still checks the rules first.
#[derive(Default)]
struct Tunnel {
    answer: tokio::sync::Mutex<Option<Decision>>,
}

/// Holds a request that the Ask default decides until it is settled. The
/// answer, a policy change, the deadline, the end of the request and the
/// proxy stop each settle the ask once. The policy is decided again after
/// the answer, so a late answer cannot override a newer rule, Offline or
/// the floor. Returns the decision, never `Ask`, with the snapshot it used.
async fn held(
    shared: &Shared,
    origin: &Origin,
    request: (&str, &str),
    tunnel: Option<&Tunnel>,
) -> (Decision, Snapshot) {
    let mut tunnel = match tunnel {
        Some(tunnel) => Some(tunnel.answer.lock().await),
        None => None,
    };
    // Subscribe before the read, so an edit between the two wakes the wait.
    let mut changes = shared.live.changes();
    let (decision, snapshot) = shared.decide_now(origin, request);
    if decision != Decision::Ask {
        return (decision, snapshot);
    }
    if let Some(Some(answered)) = tunnel.as_deref() {
        return (answered.clone(), snapshot);
    }
    let expires_at = log::now_ms() + shared.ask_deadline.as_millis() as u64;
    let opened =
        shared.live.asks.open(&origin.host, origin.port, request.0, &shared.asks, expires_at, shared.ask_sink.clone());
    let Some((ticket, mut answer)) = opened else {
        return (Decision::Block(Reason::AskTimeout), snapshot);
    };
    let mut shutdown = shared.shutdown.clone();
    let deadline = tokio::time::sleep(shared.ask_deadline);
    tokio::pin!(deadline);
    let answered = loop {
        tokio::select! {
            answer = &mut answer => break match answer {
                Ok(ask::Answer::Allow) => Decision::Allow(Reason::Ask),
                Ok(ask::Answer::Block) | Err(_) => Decision::Block(Reason::Ask),
            },
            () = &mut deadline => break Decision::Block(Reason::AskTimeout),
            _ = shutdown.wait_for(|stop| *stop) => return (Decision::Block(Reason::AskTimeout), snapshot),
            Ok(()) = changes.changed() => {
                let (decision, snapshot) = shared.decide_now(origin, request);
                if decision != Decision::Ask {
                    return (decision, snapshot);
                }
            }
        }
    };
    drop(ticket);
    if let Some(slot) = tunnel.as_deref_mut() {
        *slot = Some(answered.clone());
    }
    let (decision, snapshot) = shared.decide_now(origin, request);
    (if decision == Decision::Ask { answered } else { decision }, snapshot)
}

async fn forward(
    shared: Arc<Shared>,
    mut req: Request<Incoming>,
    origin: Origin,
    permit: Permit,
    tunnel: Option<Arc<Tunnel>>,
) -> Response<ProxyBody> {
    let started = Instant::now();
    let websocket = is_websocket(&req);
    let guest_upgrade =
        websocket.then(|| GuestUpgrade { on: hyper::upgrade::on(&mut req), permit, shutdown: shared.shutdown.clone() });
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let mut entry = origin.entry(parts.method.as_str(), Some(&path));
    if !names_only(&parts, &origin) {
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
    let request = (parts.method.as_str(), parts.uri.path());
    let (mut decision, mut snapshot) = shared.decide_now(&origin, request);
    if decision == Decision::Ask {
        (decision, snapshot) = held(&shared, &origin, request, tunnel.as_deref()).await;
    }
    let Snapshot { policy, values, logins, .. } = snapshot;
    match decision {
        Decision::Block(reason) => return shared.block(entry, &reason.to_string()),
        // `held` never returns Ask; fail closed.
        Decision::Ask => return shared.block(entry, &Reason::AskTimeout.to_string()),
        Decision::TokenInfo => return tokeninfo(&shared, &origin, parts, body, entry, started).await,
        // A host port tunnel serves plain HTTP only, so no TLS request gets here.
        Decision::HostPort(_) if origin.https => return shared.block(entry, &Reason::HostPort.to_string()),
        Decision::HostPort(port) => return host_port(&shared, parts, body, entry, started, port).await,
        Decision::Allow(reason) => {
            entry.allowed = true;
            entry.reason = reason.to_string();
        }
    }

    // Only TLS to the host keeps a credential from the network on the way.
    let credentials: Vec<(Credential, SecretValue)> = policy
        .rules
        .iter()
        .filter(|r| r.sends_credentials_to(&origin.host) && r.permits(parts.method.as_str()))
        .flat_map(|r| &r.credentials)
        .filter_map(|c| values.get(&c.key()).map(|v| (c.clone(), v.clone())))
        .collect();
    let logins: Vec<&Arc<Login>> = logins.iter().filter(|l| l.hosts.contains(&origin.host)).collect();
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
    send(client, Request::from_parts(parts, body), finish, guest_upgrade, None).await
}

/// The host port grant: plain HTTP to `127.0.0.1:<port>` of the Mac. It
/// adds no credential and no host login, and takes no upgrade, so it opens
/// no tunnel. The service gets `127.0.0.1:<port>` as its `host`.
async fn host_port(
    shared: &Shared,
    mut parts: http::request::Parts,
    body: Incoming,
    mut entry: Entry,
    started: Instant,
    port: NonZeroU16,
) -> Response<ProxyBody> {
    let refused = if !HOST_PORT_METHODS.contains(&parts.method) {
        Some(HOST_PORT_METHOD)
    } else if !names_grant(&parts, port) {
        Some(HOST_PORT_AUTHORITY)
    } else if parts.headers.contains_key(UPGRADE) {
        Some(HOST_PORT_NO_UPGRADE)
    } else {
        None
    };
    if let Some(note) = refused {
        entry.note = Some(note.into());
        return shared.block(entry, &Reason::HostPort.to_string());
    }
    entry.allowed = true;
    entry.reason = Reason::HostPort.to_string();
    strip_hop(&mut parts.headers, false);
    parts.headers.remove(HOST);
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let Ok(uri) = format!("http://{}:{port}{path}", Ipv4Addr::LOCALHOST).parse() else {
        return shared.block(entry, "invalid target");
    };
    parts.uri = uri;
    parts.version = Version::HTTP_11;
    parts.extensions = http::Extensions::new();
    let up = Arc::new(AtomicU64::new(0));
    let body = Counted { inner: body, count: up.clone() }.boxed_unsync();
    let finish = Finish { entry, started, up, sink: shared.sink.clone() };
    send(&shared.local, Request::from_parts(parts, body), finish, None, None).await
}

/// The request names `http://host.microsandbox.internal:<port>` and no
/// other authority: in a URI with no scheme or `http`, and in one `host`
/// field at most. A `host` value that is not text fails.
fn names_grant(parts: &http::request::Parts, port: NonZeroU16) -> bool {
    if !matches!(parts.uri.scheme_str(), None | Some("http")) {
        return false;
    }
    let mut names = Vec::new();
    if let Some(authority) = parts.uri.authority() {
        names.push(authority.as_str());
    }
    let hosts = parts.headers.get_all(HOST);
    if hosts.iter().count() > 1 {
        return false;
    }
    for host in hosts {
        let Ok(host) = host.to_str() else {
            return false;
        };
        names.push(host);
    }
    !names.is_empty() && names.iter().all(|n| names_gateway(n, port))
}

/// `authority` is exactly the gateway name, with one final dot at most and
/// any case, and the granted port. No port means port 80. A written port
/// must be decimal digits: `Authority::port_u16` gives `None` for a bad
/// port, and `u16::from_str` takes a leading `+`.
fn names_gateway(authority: &str, port: NonZeroU16) -> bool {
    let (host, written) = match authority.split_once(':') {
        Some((host, written)) => (host, Some(written)),
        None => (authority, None),
    };
    if !host.strip_suffix('.').unwrap_or(host).eq_ignore_ascii_case(GUEST_HOST) {
        return false;
    }
    match written {
        None => port.get() == 80,
        Some(written) => {
            (1..=5).contains(&written.len())
                && written.bytes().all(|b| b.is_ascii_digit())
                && written.parse::<u16>() == Ok(port.get())
        }
    }
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
/// is a block. `fixed_why`, if given, replaces the error text of a 502.
async fn send<C: Connect + Clone + Send + Sync + 'static>(
    client: &Client<C, ProxyBody>,
    request: Request<ProxyBody>,
    mut finish: Finish,
    guest_upgrade: Option<GuestUpgrade>,
    fixed_why: Option<&str>,
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
                let why = match fixed_why {
                    Some(why) => why.to_owned(),
                    None => dial_error(&e).map_or_else(|| e.to_string(), |d| d.to_string()),
                };
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
/// scope on the Mac, it sends the original request to Google with the real
/// token in the placeholder places only, and over HTTPS on port 443 only.
async fn tokeninfo(
    shared: &Shared,
    origin: &Origin,
    mut parts: http::request::Parts,
    body: Incoming,
    mut entry: Entry,
    started: Instant,
) -> Response<ProxyBody> {
    entry.reason = Reason::TokenInfo.to_string();
    let logins = shared.live.logins();
    let Some(login) = logins.iter().find(|l| l.kind == AgentKind::Gemini) else {
        return shared.block(entry, &Reason::AlwaysBlocked.to_string());
    };
    let placeholder = login.placeholder();
    if !inject::carries(&parts, &placeholder) {
        return shared.block(entry, &Reason::AlwaysBlocked.to_string());
    }
    let token = login.current().await;
    if let Some(info) = login::tokeninfo(&token, log::now_ms() as i64) {
        entry.allowed = true;
        entry.credentials = vec![login.name.clone()];
        entry.note = Some(TOKENINFO_LOCAL.into());
        entry.status = Some(StatusCode::OK.as_u16());
        entry.ms = started.elapsed().as_millis() as u64;
        shared.log(entry);
        let mut response = Response::new(full(info.to_string()));
        response.headers_mut().insert(CONTENT_TYPE, "application/json; charset=utf-8".parse().expect("a header value"));
        return response;
    }
    if !origin.https || origin.port != 443 {
        return shared.block(entry, &Reason::AlwaysBlocked.to_string());
    }
    let value = token.full_access();
    inject::replace_header(&mut parts.headers, AUTHORIZATION.as_str(), &placeholder, value.expose());
    inject::replace_access_token(&mut parts.uri, &placeholder, value.expose());
    // A place that could not take the token keeps the placeholder: send nothing.
    if inject::carries(&parts, &placeholder) {
        return shared.block(entry, "invalid token");
    }
    entry.allowed = true;
    entry.credentials = vec![login.name.clone()];
    entry.note = Some(TOKENINFO_GOOGLE.into());
    strip_hop(&mut parts.headers, false);
    parts.headers.remove(HOST);
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let Ok(uri) = format!("https://{TOKENINFO_HOST}:443{path}").parse() else {
        return shared.block(entry, "invalid target");
    };
    parts.uri = uri;
    parts.version = Version::HTTP_11;
    parts.extensions = http::Extensions::new();
    let up = Arc::new(AtomicU64::new(0));
    let body = Counted { inner: body, count: up.clone() }.boxed_unsync();
    let finish = Finish { entry, started, up, sink: shared.sink.clone() };
    send(&shared.auto, Request::from_parts(parts, body), finish, None, Some(TOKENINFO_UNREACHABLE)).await
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
            let https = uri.scheme_str() == Some("https");
            let authority = uri.authority().ok_or(ConnectError::Name)?;
            let Origin { host, port, .. } = Origin::read(https, authority.as_str()).ok_or(ConnectError::Name)?;
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

/// Connects the client of the host port grant: the permits, then TCP to
/// `127.0.0.1` on the port of the URI. It resolves no name and dials no
/// other address.
#[derive(Clone)]
struct LocalConnector {
    upstreams: Arc<Upstreams>,
}

impl tower_service::Service<Uri> for LocalConnector {
    type Response = Upstream;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Upstream, ConnectError>> + Send>>;

    fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), ConnectError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let upstreams = self.upstreams.clone();
        Box::pin(async move {
            if uri.scheme_str() != Some("http") || uri.host() != Some("127.0.0.1") {
                return Err(ConnectError::Name);
            }
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, uri.port_u16().ok_or(ConnectError::Name)?));
            let permits = upstreams.limits.acquire(GUEST_HOST).await.map_err(ConnectError::Dial)?;
            let name = addr.to_string();
            let tcp = tokio::time::timeout(dial::CONNECT_LIMIT, TcpStream::connect(addr))
                .await
                .map_err(|_| ConnectError::Dial(DialError::Timeout(name.clone())))?
                .map_err(|e| ConnectError::Dial(DialError::Connect(name, e)))?;
            Ok(Upstream { io: UpstreamIo::Plain(TokioIo::new(tcp)), _permits: permits })
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
    use crate::network::{Action, DefaultAction, Inject, Rule, Source, Target};
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
        resolver: Arc<FakeResolver>,
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
        let values = [(local("KEY"), SecretValue::new("sk-1"))].into();
        start(policy, values, logins, refresh, None).await
    }

    fn local(name: &str) -> CredentialRef {
        CredentialRef { source: Source::Local, name: name.into() }
    }

    fn temp_ca() -> Arc<Ca> {
        Arc::new(Ca::load(&crate::test_dir::root().join(uuid::Uuid::new_v4().to_string())).unwrap())
    }

    async fn start(
        policy: NetworkPolicy,
        values: HashMap<CredentialRef, SecretValue>,
        logins: Vec<Arc<Login>>,
        refresh: Vec<&'static str>,
        upstream_root: Option<rustls::pki_types::CertificateDer<'static>>,
    ) -> Fixture {
        let ca = temp_ca();
        let resolver = Arc::new(FakeResolver { loopback: true, ..FakeResolver::default() });
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for host in ["api.test", "a1.test", "a2.test", "c.test"] {
            resolver.names.lock().unwrap().insert(host.into(), vec![ip("127.0.0.1")]);
        }
        resolver.names.lock().unwrap().insert("rebind.test".into(), vec![ip("10.0.0.1")]);
        let live = Live::new(policy, values);
        let log: Arc<Mutex<Vec<Entry>>> = Arc::default();
        let sink_log = log.clone();
        let config = Config {
            ca: ca.clone(),
            live: live.clone(),
            logins,
            refresh,
            sink: Arc::new(move |entry| sink_log.lock().unwrap().push(entry)),
            resolver: resolver.clone(),
            asks: Arc::new(Semaphore::new(ask::APP_PENDING)),
            ask_sink: Arc::new(|| {}),
            upstream_root,
            ask_deadline: Duration::from_secs(2),
        };
        Fixture { proxy: Proxy::start(config).await.unwrap(), ca, live, log, resolver }
    }

    fn rule(host: &str, action: Action) -> Rule {
        Rule::new(Target::Host { host: host.into() }, action)
    }

    /// Sends one HTTP/1.1 request over `io` and returns the status and body.
    async fn send_over<T, B>(io: T, request: Request<B>) -> (StatusCode, HeaderMap, String)
    where
        T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
        B: Body<Data = Bytes, Error = Infallible> + Send + 'static,
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
        let tcp = connect_only(f, authority).await?;
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

    /// A CONNECT to `authority`. Returns the stream after the `200`.
    async fn connect_only(f: &Fixture, authority: &str) -> Result<TcpStream, StatusCode> {
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
        Ok(tcp)
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
            source: Source::Local,
        });
        let policy = NetworkPolicy { default: DefaultAction::Block, rules: vec![allowed], ..Default::default() };
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
            source: Source::Local,
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
            rules: vec![rule("api.test", Action::Allow)],
            ..Default::default()
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

    /// Writes `head` to a new connection to the proxy and returns the
    /// first bytes of the answer.
    async fn raw_plain(f: &Fixture, head: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        tcp.write_all(head.replace("{auth}", &f.auth()).as_bytes()).await.unwrap();
        let mut answer = vec![0; 1024];
        let n = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut answer)).await.unwrap().unwrap_or(0);
        String::from_utf8_lossy(&answer[..n]).into_owned()
    }

    /// Writes `head` in a new MITM tunnel to `authority` and returns the
    /// first bytes of the answer.
    async fn raw_in_tls(f: &Fixture, authority: &str, head: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tls = tunnel(f, authority).await.unwrap().into_inner();
        tls.write_all(head).await.unwrap();
        let mut answer = vec![0; 1024];
        let n = tokio::time::timeout(Duration::from_secs(2), tls.read(&mut answer)).await.unwrap().unwrap_or(0);
        String::from_utf8_lossy(&answer[..n]).into_owned()
    }

    fn status_of(answer: &str) -> Option<&str> {
        answer.get(9..12)
    }

    /// Absolute-form plain HTTP: a target that the parser refuses is a 400
    /// with no log entry and no dial; a `host` field that names another
    /// authority is a 421.
    #[tokio::test]
    async fn plain_http_reads_its_target_once_and_refuses_an_ambiguous_one() {
        let f = fixture(NetworkPolicy { default: DefaultAction::Allow, ..Default::default() }, Vec::new(), Vec::new())
            .await;
        let port = upstream().await;
        let refused = [
            format!("GET http://user@api.test:{port}/ HTTP/1.1\r\nproxy-authorization: {{auth}}\r\n\r\n"),
            format!("GET http://evil.test:{port}@api.test:{port}/ HTTP/1.1\r\nproxy-authorization: {{auth}}\r\n\r\n"),
            "GET http://api.test:abc/ HTTP/1.1\r\nproxy-authorization: {auth}\r\n\r\n".into(),
            "GET http://api.test:0/ HTTP/1.1\r\nproxy-authorization: {auth}\r\n\r\n".into(),
            format!(
                "GET http://api.test:{}/ HTTP/1.1\r\nproxy-authorization: {{auth}}\r\n\r\n",
                u32::from(port) + 65536
            ),
            format!("GET http://api%2etest:{port}/ HTTP/1.1\r\nproxy-authorization: {{auth}}\r\n\r\n"),
            format!("GET http://2130706433:{port}/ HTTP/1.1\r\nproxy-authorization: {{auth}}\r\n\r\n"),
            format!("GET http://api.test..:{port}/ HTTP/1.1\r\nproxy-authorization: {{auth}}\r\n\r\n"),
        ];
        for head in &refused {
            assert_eq!(status_of(&raw_plain(&f, head).await), Some("400"), "{head}");
        }
        assert!(f.entries().is_empty(), "{:?}", f.entries());

        let misdirected = [
            format!("host: evil.test:{port}\r\n"),
            format!("host: api.test:{port}\r\nhost: evil.test:{port}\r\n"),
            format!("host: evil.test@api.test:{port}\r\n"),
            "host: api.test\r\n".to_owned(),
        ];
        for host in &misdirected {
            let head = format!("GET http://api.test:{port}/ HTTP/1.1\r\n{host}proxy-authorization: {{auth}}\r\n\r\n");
            assert_eq!(status_of(&raw_plain(&f, &head).await), Some("421"), "{host}");
        }
        assert!(f.entries().iter().all(|e| !e.allowed && e.host == "api.test"), "{:?}", f.entries());

        let same = format!(
            "GET http://API.Test.:{port}/ HTTP/1.1\r\nhost: api.test:{port}\r\nproxy-authorization: {{auth}}\r\n\r\n"
        );
        assert_eq!(status_of(&raw_plain(&f, &same).await), Some("200"));
        assert_eq!(f.entries().last().unwrap().host, "api.test");
    }

    /// The credential of `a1.test` goes only to a request that names
    /// exactly `a1.test` and the CONNECT port, by one reading of the
    /// authority. Every ambiguous spelling is refused before any dial.
    #[tokio::test]
    async fn only_the_canonical_authority_gets_a_credential() {
        let recipients = temp_ca();
        let (a1, seen) = tls_recipient(&recipients, "a1.test").await;
        let policy = NetworkPolicy {
            default: DefaultAction::Allow,
            rules: vec![bound("a1.test", Source::Local)],
            ..Default::default()
        };
        let values = [(local("KEY"), SecretValue::new("local-fixture"))].into();
        let f = start(policy, values, Vec::new(), Vec::new(), Some(recipients.der().clone())).await;

        let connects = [
            format!("user@a1.test:{a1}"),
            format!("a1.test:{a1}@evil.test:443"),
            format!("a1.test:+{a1}"),
            format!("a1.test:{}", u32::from(a1) + 65536),
            "a1.test:abc".to_owned(),
            "a1.test:0".to_owned(),
            "127.1:443".to_owned(),
            format!("a1.test..:{a1}"),
            format!("a1%2etest:{a1}"),
        ];
        for authority in &connects {
            assert!(connect_only(&f, authority).await.is_err(), "CONNECT {authority}");
        }
        assert!(f.entries().is_empty(), "{:?}", f.entries());

        let authority = format!("a1.test:{a1}");
        let heads: Vec<Vec<u8>> = vec![
            format!("GET / HTTP/1.1\r\nhost: {authority}\r\nhost: evil.test\r\n\r\n").into(),
            format!("GET / HTTP/1.1\r\nhost: evil.test@{authority}\r\n\r\n").into(),
            format!("GET / HTTP/1.1\r\nhost: a1.test:0{a1}\r\n\r\n").into(),
            b"GET / HTTP/1.1\r\nhost: a1.test\r\n\r\n".to_vec(),
            format!("GET https://{authority}/ HTTP/1.1\r\nhost: evil.test\r\n\r\n").into(),
            format!("GET https://user@{authority}/ HTTP/1.1\r\nhost: {authority}\r\n\r\n").into(),
            [format!("GET / HTTP/1.1\r\nhost: {authority}").as_bytes(), b"\xff\r\n\r\n"].concat(),
        ];
        for head in &heads {
            let answer = raw_in_tls(&f, &authority, head).await;
            assert!(
                status_of(&answer).is_some_and(|s| s.starts_with('4')),
                "{} -> {answer:?}",
                String::from_utf8_lossy(head)
            );
        }
        assert!(seen.lock().unwrap().is_empty(), "a misdirected request reached the recipient");

        // An equal spelling is the same host: it gets the credential.
        let head = format!("GET / HTTP/1.1\r\nhost: A1.TEST.:{a1}\r\n\r\n");
        let answer = raw_in_tls(&f, &format!("A1.Test.:{a1}"), head.as_bytes()).await;
        assert_eq!(status_of(&answer), Some("200"), "{answer}");
        assert_eq!(seen.lock().unwrap().last().cloned().flatten().as_deref(), Some("Bearer local-fixture"));
        let last = f.entries().pop().unwrap();
        assert_eq!((last.host.as_str(), last.credentials.as_slice()), ("a1.test", ["KEY".to_owned()].as_slice()));
    }

    /// The authorization field of each request at a recipient.
    type Seen = Arc<Mutex<Vec<Option<String>>>>;

    /// A TLS recipient for `host` on loopback, with a leaf of `ca`. It keeps
    /// the authorization field of each request and answers without it.
    async fn tls_recipient(ca: &Ca, host: &str) -> (u16, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = TlsAcceptor::from(ca.server_config(host).await.unwrap());
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let sink = sink.clone();
                let service = service_fn(move |req: Request<Incoming>| {
                    let auth = req.headers().get(AUTHORIZATION).map(|v| v.to_str().unwrap().to_owned());
                    sink.lock().unwrap().push(auth);
                    async { Ok::<_, Infallible>(Response::new(Full::new(Bytes::from("ok")))) }
                });
                tokio::spawn(async move {
                    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                    let _ = builder.serve_connection(TokioIo::new(tls), service).await;
                });
            }
        });
        (port, seen)
    }

    /// Sends the placeholder to `host` through a tunnel. Returns the status.
    async fn via_tunnel(f: &Fixture, host: &str, port: u16) -> Result<StatusCode, StatusCode> {
        let authority = format!("{host}:{port}");
        let io = tunnel(f, &authority).await?;
        let request = Request::get("/").header(HOST, &authority).header(AUTHORIZATION, "Bearer $MSB_KEY$");
        let (status, _, body) = send_over(io, request.body(Empty::new()).unwrap()).await;
        assert!(!body.contains("fixture"), "{body}");
        Ok(status)
    }

    fn bound(host: &str, source: Source) -> Rule {
        let mut rule = rule(host, Action::Allow);
        rule.credentials.push(Credential {
            name: "KEY".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
            source,
        });
        rule
    }

    /// One cage, one credential name of two sources on two recipients: each
    /// recipient gets the value of its own rule, in both rule orders. A rule
    /// with no credential sends the placeholder only. A rotation of the
    /// library value reaches its recipient alone.
    #[tokio::test]
    async fn one_name_of_two_sources_reaches_each_recipient_with_its_own_value() {
        let recipients = temp_ca();
        let (a1, seen_a1) = tls_recipient(&recipients, "a1.test").await;
        let (a2, seen_a2) = tls_recipient(&recipients, "a2.test").await;
        let (c, seen_c) = tls_recipient(&recipients, "c.test").await;
        let library = CredentialRef { source: Source::Library, name: "KEY".into() };
        let last = |seen: &Seen| seen.lock().unwrap().last().cloned().flatten();
        for reversed in [false, true] {
            let mut rules =
                vec![bound("a1.test", Source::Library), bound("a2.test", Source::Local), rule("c.test", Action::Allow)];
            if reversed {
                rules.reverse();
            }
            let policy = NetworkPolicy { default: DefaultAction::Block, rules, ..Default::default() };
            policy.check().unwrap();
            let values = [
                (library.clone(), SecretValue::new("shared-fixture")),
                (local("KEY"), SecretValue::new("local-fixture")),
            ]
            .into();
            let f = start(policy, values, Vec::new(), Vec::new(), Some(recipients.der().clone())).await;

            for (host, port) in [("a1.test", a1), ("a2.test", a2), ("c.test", c)] {
                assert_eq!(via_tunnel(&f, host, port).await, Ok(StatusCode::OK), "{host}");
            }
            assert_eq!(last(&seen_a1).as_deref(), Some("Bearer shared-fixture"));
            assert_eq!(last(&seen_a2).as_deref(), Some("Bearer local-fixture"));
            assert_eq!(last(&seen_c).as_deref(), Some("Bearer $MSB_KEY$"));

            f.live.set_value(&library, Some(SecretValue::new("rotated-fixture")));
            via_tunnel(&f, "a1.test", a1).await.unwrap();
            via_tunnel(&f, "a2.test", a2).await.unwrap();
            assert_eq!(last(&seen_a1).as_deref(), Some("Bearer rotated-fixture"));
            assert_eq!(last(&seen_a2).as_deref(), Some("Bearer local-fixture"));

            // A Block rule keeps its credential inactive: nothing reaches the recipient.
            let count = seen_a1.lock().unwrap().len();
            f.live.edit(|policy, _| policy.rules.iter_mut().for_each(|r| r.action = Action::Block));
            assert_eq!(via_tunnel(&f, "a1.test", a1).await, Err(StatusCode::FORBIDDEN));
            assert_eq!(seen_a1.lock().unwrap().len(), count);

            // A missing library value sends no value, and never the local one.
            f.live.edit(|policy, values| {
                policy.rules.iter_mut().for_each(|r| r.action = Action::Allow);
                values.remove(&library);
            });
            via_tunnel(&f, "a1.test", a1).await.unwrap();
            assert_eq!(last(&seen_a1).as_deref(), Some("Bearer $MSB_KEY$"));
            let entries = format!("{:?}", f.entries());
            assert!(!entries.contains("fixture"), "{entries}");
        }
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

    /// A Gemini login. `None` gives a login with no scope.
    fn gemini_login(scope: Option<&str>) -> Arc<Login> {
        let mut doc =
            serde_json::json!({ "access_token": "ya29.real/x", "expiry_date": log::now_ms() as i64 + 3_600_000 });
        if let Some(scope) = scope {
            doc["scope"] = scope.into();
        }
        let token = host_login::parse(AgentKind::Gemini, &doc.to_string()).unwrap();
        let name = host_login::secret_name(AgentKind::Gemini).to_owned();
        Arc::new(Login::new(name, vec!["cloudcode-pa.googleapis.com".into()], token, Arc::new(NoLogin)))
    }

    #[tokio::test]
    async fn the_gemini_tokeninfo_check_is_answered_and_its_host_stays_blocked() {
        let policy = NetworkPolicy { default: DefaultAction::Allow, ..Default::default() };
        let f = fixture(policy, vec![gemini_login(Some("openid email"))], vec![TOKENINFO_HOST]).await;
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
        assert_eq!(entry.note.as_deref(), Some(TOKENINFO_LOCAL));

        // The refresh endpoint and a request with no placeholder stay blocked.
        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let request = Request::post("/token").header(HOST, TOKENINFO_HOST).body(Empty::new()).unwrap();
        assert_eq!(send_over(io, request).await.0, StatusCode::FORBIDDEN);
        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let request =
            Request::get("/tokeninfo?access_token=mine").header(HOST, TOKENINFO_HOST).body(Empty::new()).unwrap();
        assert_eq!(send_over(io, request).await.0, StatusCode::FORBIDDEN);
        assert!(f.entries().iter().all(|e| !format!("{e:?}").contains("ya29")));

        // An empty scope is a known scope: the answer stays local, also over plain http.
        let policy = NetworkPolicy { default: DefaultAction::Allow, ..Default::default() };
        let f = fixture(policy, vec![gemini_login(Some(""))], vec![TOKENINFO_HOST]).await;
        let uri = format!("http://{TOKENINFO_HOST}/tokeninfo?access_token=$MSB_GEMINI_HOST_LOGIN$");
        let (status, _, body) = plain(&f, &uri, Some(f.auth())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["scope"], "");
        assert!(f.resolver.lookups.lock().unwrap().is_empty());
    }

    /// What a recording recipient got: method, path and query, chosen
    /// header fields, and body.
    #[derive(Debug, Clone)]
    struct Recorded {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    const RECORDED_FIELDS: [&str; 8] =
        ["authorization", "content-type", "x-client", "host", "proxy-authorization", "x-hop", "keep-alive", "te"];

    /// A TLS recipient for `host` on loopback over HTTP/1.1, with a leaf of
    /// `ca`. It records each request and answers `400` with a fixed body, an
    /// end-to-end field and a hop field. It echoes nothing.
    async fn recording_recipient(ca: &Ca, host: &str) -> (u16, Arc<Mutex<Vec<Recorded>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = (*ca.server_config(host).await.unwrap()).clone();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let seen: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let Ok(tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let sink = sink.clone();
                let service = service_fn(move |req: Request<Incoming>| {
                    let sink = sink.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let headers = RECORDED_FIELDS
                            .iter()
                            .flat_map(|name| {
                                parts
                                    .headers
                                    .get_all(*name)
                                    .iter()
                                    .map(|v| (name.to_string(), v.to_str().unwrap().to_owned()))
                            })
                            .collect();
                        let body = body.collect().await.unwrap().to_bytes();
                        sink.lock().unwrap().push(Recorded {
                            method: parts.method.to_string(),
                            target: parts.uri.to_string(),
                            headers,
                            body: String::from_utf8_lossy(&body).into_owned(),
                        });
                        let response = Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .header("x-google", "kept")
                            .header("keep-alive", "timeout=5")
                            .body(Full::new(Bytes::from("{\"error\":\"invalid_token\"}")))
                            .unwrap();
                        Ok::<_, Infallible>(response)
                    }
                });
                tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), service));
            }
        });
        (port, seen)
    }

    /// A Gemini login with no scope, a proxy that trusts `recipients`, and
    /// Google on 443 sent to `port` of loopback.
    async fn unscoped_fixture(recipients: &Ca, port: u16) -> Fixture {
        let policy = NetworkPolicy { default: DefaultAction::Allow, ..Default::default() };
        let logins = vec![gemini_login(None)];
        let f = start(policy, HashMap::new(), logins, vec![TOKENINFO_HOST], Some(recipients.der().clone())).await;
        f.resolver.ports.lock().unwrap().insert((TOKENINFO_HOST.into(), 443), port);
        f
    }

    #[tokio::test]
    async fn an_unscoped_tokeninfo_check_goes_to_google_as_the_guest_sent_it() {
        let recipients = temp_ca();
        let (port, seen) = recording_recipient(&recipients, TOKENINFO_HOST).await;
        let f = unscoped_fixture(&recipients, port).await;
        let header = |r: &Recorded, name: &str| r.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());

        // GET with the placeholder in the query, raw and encoded: only that value changes.
        for query in [
            "access_token=$MSB_GEMINI_HOST_LOGIN$&x=a%20b+c&flag",
            "x=a%20b+c&access_token=%24MSB_GEMINI_HOST_LOGIN%24&flag",
        ] {
            let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
            let request = Request::get(format!("/tokeninfo?{query}"))
                .header(HOST, TOKENINFO_HOST)
                .header("x-client", "gemini-cli")
                .body(Empty::new())
                .unwrap();
            let (status, headers, body) = send_over(io, request).await;
            assert_eq!((status, body.as_str()), (StatusCode::BAD_REQUEST, "{\"error\":\"invalid_token\"}"));
            assert_eq!(headers["x-google"], "kept");
            assert!(headers.get("keep-alive").is_none() && headers.get(BLOCKED_HEADER).is_none());
            let got = seen.lock().unwrap().last().cloned().unwrap();
            let expected = query
                .replace("%24MSB_GEMINI_HOST_LOGIN%24", "ya29.real%2Fx")
                .replace("$MSB_GEMINI_HOST_LOGIN$", "ya29.real%2Fx");
            assert_eq!((got.method.as_str(), got.target.clone()), ("GET", format!("/tokeninfo?{expected}")));
            assert_eq!(header(&got, "x-client").as_deref(), Some("gemini-cli"));
            assert_eq!(header(&got, "host").as_deref(), Some(TOKENINFO_HOST));
            assert_eq!(header(&got, "authorization"), None);
        }

        // POST with the placeholder in Authorization: its other bytes, the
        // other fields and the body stay; the body gets no token; hop fields go.
        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let sent = "access_token=$MSB_GEMINI_HOST_LOGIN$&k=v";
        let request = Request::post("/tokeninfo?a=1")
            .header(HOST, TOKENINFO_HOST)
            .header(AUTHORIZATION, "Bearer $MSB_GEMINI_HOST_LOGIN$ extra")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(CONNECTION, "x-hop")
            .header("x-hop", "1")
            .header(PROXY_AUTHORIZATION, "Basic inner")
            .body(Full::new(Bytes::from(sent)))
            .unwrap();
        let (status, _, body) = send_over(io, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let got = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!((got.method.as_str(), got.target.as_str(), got.body.as_str()), ("POST", "/tokeninfo?a=1", sent));
        assert_eq!(header(&got, "authorization").as_deref(), Some("Bearer ya29.real/x extra"));
        assert_eq!(header(&got, "content-type").as_deref(), Some("application/x-www-form-urlencoded"));
        for hop in ["x-hop", "proxy-authorization"] {
            assert_eq!(header(&got, hop), None, "{hop}");
        }

        let entries = f.entries();
        assert_eq!(entries.len(), 3);
        let last = entries.last().unwrap();
        assert_eq!((last.reason.as_str(), last.allowed, last.status), ("tokeninfo", true, Some(400)));
        assert_eq!((last.method.as_deref(), last.path.as_deref()), (Some("POST"), Some("/tokeninfo?a")));
        assert_eq!(last.note.as_deref(), Some(TOKENINFO_GOOGLE));
        assert_eq!(last.credentials, [host_login::secret_name(AgentKind::Gemini)]);
        assert_eq!((last.up, last.down), (sent.len() as u64, body.len() as u64));
        assert_eq!(entries[0].path.as_deref(), Some("/tokeninfo?access_token&x&flag"));
        assert!(!format!("{entries:?}").contains("ya29"));
    }

    #[tokio::test]
    async fn an_unscoped_tokeninfo_check_needs_https_on_443_and_hides_the_token_in_errors() {
        let recipients = temp_ca();
        let (port, seen) = recording_recipient(&recipients, TOKENINFO_HOST).await;
        let f = unscoped_fixture(&recipients, port).await;
        f.resolver.ports.lock().unwrap().insert((TOKENINFO_HOST.into(), 8443), port);
        let query = "/tokeninfo?access_token=$MSB_GEMINI_HOST_LOGIN$";

        let io = tunnel(&f, "oauth2.googleapis.com:8443").await.unwrap();
        let request = Request::get(query).header(HOST, "oauth2.googleapis.com:8443").body(Empty::new()).unwrap();
        let (status, headers, _) = send_over(io, request).await;
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "always-blocked"));
        let (status, headers, _) = plain(&f, &format!("http://{TOKENINFO_HOST}{query}"), Some(f.auth())).await;
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "always-blocked"));
        assert!(seen.lock().unwrap().is_empty());

        // Google does not answer: the 502 text is fixed.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        f.resolver.ports.lock().unwrap().insert((TOKENINFO_HOST.into(), 443), closed);
        let io = tunnel(&f, "oauth2.googleapis.com:443").await.unwrap();
        let request = Request::get(query).header(HOST, TOKENINFO_HOST).body(Empty::new()).unwrap();
        let (status, headers, body) = send_over(io, request).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(headers[ERROR_HEADER], TOKENINFO_UNREACHABLE);
        assert_eq!(body, format!("zwinger could not reach {TOKENINFO_HOST}: {TOKENINFO_UNREACHABLE}\n"));
        let entries = f.entries();
        assert_eq!(entries.last().unwrap().status, Some(502));
        assert!(!format!("{entries:?}").contains("ya29"));
    }

    /// A grant for `port`, an Allow rule with a credential for the gateway
    /// name, and a host login for it: the local route must use none of them.
    async fn host_port_fixture(port: u16) -> Fixture {
        let mut tempting = bound(GUEST_HOST, Source::Local);
        tempting.action = Action::Allow;
        let policy = NetworkPolicy {
            default: DefaultAction::Allow,
            rules: vec![tempting],
            host_port: NonZeroU16::new(port),
            ..Default::default()
        };
        let doc = serde_json::json!({ "access_token": "ya29.real", "expiry_date": log::now_ms() as i64 + 3_600_000 });
        let token = host_login::parse(AgentKind::Gemini, &doc.to_string()).unwrap();
        let login = Login::new("LOGIN".into(), vec![GUEST_HOST.into()], token, Arc::new(NoLogin));
        fixture(policy, vec![Arc::new(login)], Vec::new()).await
    }

    fn to_guest_host(f: &Fixture, port: u16, path: &str) -> http::request::Builder {
        Request::get(format!("http://{GUEST_HOST}:{port}{path}"))
            .header(PROXY_AUTHORIZATION, f.auth())
            .header(AUTHORIZATION, "Bearer $MSB_KEY$")
    }

    #[tokio::test]
    async fn the_host_port_grant_reaches_loopback_with_no_credential() {
        let port = upstream().await;
        let f = host_port_fixture(port).await;
        let tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let request = to_guest_host(&f, port, "/api/tags?key=v").body(Empty::new()).unwrap();
        let (status, headers, body) = send_over(TokioIo::new(tcp), request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(headers.get(BLOCKED_HEADER).is_none());
        assert!(body.starts_with("GET /api/tags?key=v "), "{body}");
        assert!(body.contains(&format!("host=Some(\"127.0.0.1:{port}\")")), "{body}");
        // The guest's own placeholder passes, and nothing replaces it.
        assert!(body.contains("auth=Some(\"Bearer $MSB_KEY$\")"), "{body}");
        assert!(body.contains("proxy=None"), "{body}");
        assert!(!body.contains("sk-1") && !body.contains("ya29"), "{body}");
        let entry = f.entries().pop().unwrap();
        assert_eq!((entry.allowed, entry.reason.as_str(), entry.port), (true, "host-port", Some(port)));
        assert!(entry.credentials.is_empty() && entry.note.is_none(), "{entry:?}");
        // The connection does not stay in a pool.
        assert!(eventually(|| f.proxy.upstreams.limits.used() == 0).await);

        // Another port of the gateway name stays always blocked.
        let other = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let (status, headers, _) = plain(&f, &format!("http://{GUEST_HOST}:{other}/"), Some(f.auth())).await;
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "always-blocked"));
        assert_eq!(f.entries().pop().unwrap().port, Some(other));
        for host in ["localhost", "127.0.0.1"] {
            let (status, _, _) = plain(&f, &format!("http://{host}:{port}/"), Some(f.auth())).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
        }
    }

    #[tokio::test]
    async fn the_host_port_refuses_https_upgrades_and_the_proxy_port() {
        let port = upstream().await;
        let f = host_port_fixture(port).await;
        // TLS in a CONNECT is closed: the proxy does not turn the guest's HTTPS into plain HTTP.
        let mut tcp = connect_only(&f, &format!("{GUEST_HOST}:{port}")).await.unwrap();
        {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            tcp.write_all(&[0x16, 0x03, 0x01, 0x00, 0x05]).await.unwrap();
            let read = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut [0; 16])).await;
            assert!(matches!(read, Ok(Ok(0) | Err(_))), "the TLS tunnel stayed open: {read:?}");
        }
        let entry = f.entries().pop().unwrap();
        assert_eq!(
            (entry.method.as_deref(), entry.reason.as_str(), entry.port, entry.note.as_deref()),
            (Some("CONNECT"), "host-port", Some(port), Some(HOST_PORT_HTTP_ONLY))
        );
        // A CONNECT to another port of the gateway name is refused at once.
        let other = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        assert_eq!(connect_only(&f, &format!("{GUEST_HOST}:{other}")).await.err(), Some(StatusCode::FORBIDDEN));
        let tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let request = to_guest_host(&f, port, "/")
            .header(CONNECTION, "upgrade")
            .header(UPGRADE, "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Empty::new())
            .unwrap();
        let (status, headers, _) = send_over(TokioIo::new(tcp), request).await;
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "host-port"));
        assert_eq!(f.entries().pop().unwrap().note.as_deref(), Some(HOST_PORT_NO_UPGRADE));
        // A grant of the proxy's own port opens nothing.
        let own = f.proxy.port;
        f.live.edit(|p, _| p.host_port = NonZeroU16::new(own));
        let (status, headers, _) = plain(&f, &format!("http://{GUEST_HOST}:{own}/"), Some(f.auth())).await;
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "always-blocked"));
    }

    #[tokio::test]
    async fn a_removed_grant_or_offline_blocks_the_next_request_of_a_kept_connection() {
        let port = upstream().await;
        let f = host_port_fixture(port).await;
        let tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.unwrap();
        tokio::spawn(connection);
        let status = async |sender: &mut hyper::client::conn::http1::SendRequest<Empty<Bytes>>| {
            let request = to_guest_host(&f, port, "/").body(Empty::new()).unwrap();
            let response = sender.send_request(request).await.unwrap();
            let status = response.status();
            response.into_body().collect().await.unwrap();
            status
        };
        assert_eq!(status(&mut sender).await, StatusCode::OK);
        f.live.edit(|p, _| p.offline = true);
        assert_eq!(status(&mut sender).await, StatusCode::FORBIDDEN);
        f.live.edit(|p, _| p.offline = false);
        assert_eq!(status(&mut sender).await, StatusCode::OK);
        f.live.edit(|p, _| p.host_port = None);
        assert_eq!(status(&mut sender).await, StatusCode::FORBIDDEN);
        let reasons: Vec<String> = f.entries().into_iter().map(|e| e.reason).collect();
        assert_eq!(reasons, ["host-port", "offline", "host-port", "always-blocked"]);
    }

    /// Node's fetch tunnels plain HTTP with a CONNECT. Each request in the
    /// tunnel is decided again and gets no credential.
    #[tokio::test]
    async fn plain_http_in_a_host_port_tunnel_is_decided_per_request() {
        let port = upstream().await;
        let f = host_port_fixture(port).await;
        let authority = format!("{GUEST_HOST}:{port}");
        let tcp = connect_only(&f, &authority).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.unwrap();
        tokio::spawn(connection);
        let request = || {
            Request::get("/v1")
                .header(HOST, &authority)
                .header(AUTHORIZATION, "Bearer $MSB_KEY$")
                .body(Empty::<Bytes>::new())
        };
        let response = sender.send_request(request().unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("auth=Some(\"Bearer $MSB_KEY$\")") && !body.contains("sk-1"), "{body}");
        let entry = f.entries().pop().unwrap();
        assert_eq!((entry.reason.as_str(), entry.port, entry.allowed), ("host-port", Some(port), true));
        assert!(entry.credentials.is_empty());

        f.live.edit(|p, _| p.offline = true);
        let response = sender.send_request(request().unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(f.entries().pop().unwrap().reason, "offline");
        f.live.edit(|p, _| {
            p.offline = false;
            p.host_port = None;
        });
        let response = sender.send_request(request().unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(f.entries().pop().unwrap().reason, "always-blocked");
    }

    /// Sends raw bytes in a new host port tunnel and returns the first
    /// answer, or an empty string if the proxy closes the tunnel.
    async fn in_tunnel(f: &Fixture, port: u16, bytes: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = connect_only(f, &format!("{GUEST_HOST}:{port}")).await.unwrap();
        tcp.write_all(bytes).await.unwrap();
        let mut answer = vec![0; 1024];
        let read = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut answer)).await;
        let n = read.unwrap_or(Ok(0)).unwrap_or(0);
        String::from_utf8_lossy(&answer[..n]).into_owned()
    }

    /// No request may make the local service a raw tunnel, reach it with
    /// TLS, or name another target. The service is a bare listener, so a
    /// dial would show as an accepted connection.
    #[tokio::test]
    async fn the_host_port_tunnel_refuses_each_bad_request_before_a_dial() {
        let service = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        let f = host_port_fixture(port).await;
        let authority = format!("{GUEST_HOST}:{port}");
        let refused = [
            (format!("CONNECT {authority} HTTP/1.1\r\nhost: {authority}\r\n\r\n"), "403", Some(HOST_PORT_METHOD)),
            (format!("FOO / HTTP/1.1\r\nhost: {authority}\r\n\r\n"), "403", Some(HOST_PORT_METHOD)),
            ("GET / HTTP/1.0\r\n\r\n".to_owned(), "403", Some(HOST_PORT_AUTHORITY)),
            (format!("GET / HTTP/1.1\r\nhost: {GUEST_HOST}\r\n\r\n"), "421", None),
            (format!("GET / HTTP/1.1\r\nhost: {GUEST_HOST}:{}\r\n\r\n", port + 1), "421", None),
            (format!("GET http://example.com/ HTTP/1.1\r\nhost: {authority}\r\n\r\n"), "421", None),
            (
                format!("GET https://{authority}/ HTTP/1.1\r\nhost: {authority}\r\n\r\n"),
                "403",
                Some(HOST_PORT_AUTHORITY),
            ),
        ];
        for (request, status, note) in refused {
            let answer = in_tunnel(&f, port, request.as_bytes()).await;
            assert_eq!(answer.get(9..13), Some(&*format!("{status} ")), "{request:?} -> {answer:?}");
            let entry = f.entries().pop().unwrap();
            assert!(!entry.allowed, "{request:?}");
            assert_eq!(entry.note.as_deref(), note, "{request:?}");
        }
        // Bytes that are not HTTP end at the parse error, with no entry.
        let before = f.entries().len();
        let answer = in_tunnel(&f, port, b"\x00\x01\x02 not http\r\n\r\n").await;
        assert!(answer.is_empty() || answer.starts_with("HTTP/1.1 400 "), "{answer:?}");
        assert_eq!(f.entries().len(), before);
        let dial = tokio::time::timeout(Duration::from_millis(300), service.accept()).await;
        assert!(dial.is_err(), "the proxy dialed the local service");
    }

    /// A bare host means port 80, as for each plain HTTP origin: it names
    /// a port-80 grant and no other grant.
    #[test]
    fn a_bare_host_names_only_a_port_80_grant() {
        let parts = |uri: &str, host: &str| Request::get(uri).header(HOST, host).body(()).unwrap().into_parts().0;
        let web = NonZeroU16::new(80).unwrap();
        assert!(names_grant(&parts("/", GUEST_HOST), web));
        assert!(names_grant(&parts("/", &format!("{GUEST_HOST}:80")), web));
        assert!(names_grant(&parts(&format!("http://{GUEST_HOST}/"), &format!("{GUEST_HOST}:80")), web));
        assert!(!names_grant(&parts(&format!("https://{GUEST_HOST}/"), GUEST_HOST), web));
        assert!(!names_grant(&parts("/", "example.com"), web));
        assert!(!names_grant(&parts("/", "example.com:80"), web));
        let ollama = NonZeroU16::new(11434).unwrap();
        assert!(!names_grant(&parts("/", GUEST_HOST), ollama));
        assert!(names_grant(&parts("/", &format!("{GUEST_HOST}:11434")), ollama));
    }

    #[test]
    fn only_the_exact_gateway_name_and_port_name_the_grant() {
        let web = NonZeroU16::new(80).unwrap();
        let ollama = NonZeroU16::new(11434).unwrap();
        let upper = GUEST_HOST.to_ascii_uppercase();
        let accepted = [
            (GUEST_HOST.to_owned(), web),
            (format!("{GUEST_HOST}:80"), web),
            (format!("{GUEST_HOST}:080"), web),
            (format!("{upper}.:80"), web),
            (format!("{GUEST_HOST}.:11434"), ollama),
        ];
        for (authority, port) in accepted {
            assert!(names_gateway(&authority, port), "{authority}");
        }
        let refused = [
            format!("{GUEST_HOST}:65536"),
            format!("{GUEST_HOST}:65616"),
            format!("{GUEST_HOST}:abc"),
            format!("user@{GUEST_HOST}:80"),
            format!("user:pass@{GUEST_HOST}"),
            format!("{GUEST_HOST}:"),
            format!("{GUEST_HOST}:+80"),
            format!("{GUEST_HOST}:-80"),
            format!("{GUEST_HOST}:000080"),
            format!("{GUEST_HOST}::80"),
            format!("[{GUEST_HOST}]:80"),
            format!("{GUEST_HOST}..:80"),
            format!("{GUEST_HOST}:80:80"),
            format!("{GUEST_HOST}:81"),
            format!(".{GUEST_HOST}:80"),
            format!("x{GUEST_HOST}:80"),
            "example.com:80".to_owned(),
            String::new(),
        ];
        for authority in refused {
            assert!(!names_gateway(&authority, web), "{authority}");
        }
        assert!(!names_gateway(GUEST_HOST, ollama));
        assert!(!names_gateway(&format!("{GUEST_HOST}:80"), ollama));
    }

    #[test]
    fn names_grant_fails_closed_on_two_host_fields_and_a_host_that_is_not_text() {
        let web = NonZeroU16::new(80).unwrap();
        let two = Request::get("/").header(HOST, GUEST_HOST).header(HOST, GUEST_HOST).body(()).unwrap();
        assert!(!names_grant(&two.into_parts().0, web));
        let binary = http::HeaderValue::from_bytes(b"host.microsandbox.internal\xff").unwrap();
        let request = Request::get("/").header(HOST, binary).body(()).unwrap();
        assert!(!names_grant(&request.into_parts().0, web));
        let none = Request::get("/").body(()).unwrap();
        assert!(!names_grant(&none.into_parts().0, web));
        let userinfo = Request::get(format!("http://user@{GUEST_HOST}/")).body(()).unwrap();
        assert!(!names_grant(&userinfo.into_parts().0, web));
    }

    /// A written port that is not the grant's digits reaches no local
    /// service: not in a CONNECT target, not in a tunnel, not in an
    /// absolute-form request.
    #[tokio::test]
    async fn a_malformed_port_of_the_gateway_name_reaches_no_local_service() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let service = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        let f = host_port_fixture(port).await;
        for written in [(u32::from(port) + 65536).to_string(), "abc".to_owned()] {
            let authority = format!("{GUEST_HOST}:{written}");
            assert!(connect_only(&f, &authority).await.is_err(), "CONNECT {authority}");

            let answer = in_tunnel(&f, port, format!("GET / HTTP/1.1\r\nhost: {authority}\r\n\r\n").as_bytes()).await;
            assert!(answer.get(9..12).is_some_and(|s| s != "200"), "tunnel {authority} -> {answer:?}");

            let mut tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
            let request = format!(
                "GET http://{authority}/ HTTP/1.1\r\nhost: {authority}\r\nproxy-authorization: {}\r\n\r\n",
                f.auth()
            );
            tcp.write_all(request.as_bytes()).await.unwrap();
            let mut answer = vec![0; 1024];
            let n = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut answer)).await.unwrap().unwrap();
            let answer = String::from_utf8_lossy(&answer[..n]);
            assert!(answer.get(9..12).is_some_and(|s| s != "200"), "absolute {authority} -> {answer:?}");
        }
        assert!(f.entries().iter().all(|e| !e.allowed), "{:?}", f.entries());
        let dial = tokio::time::timeout(Duration::from_millis(300), service.accept()).await;
        assert!(dial.is_err(), "the proxy dialed the local service");
    }

    /// With a grant of 443, a CONNECT whose port is not written as digits
    /// would default to 443 and open a local tunnel.
    #[tokio::test]
    async fn a_connect_with_a_malformed_port_opens_no_tunnel_for_a_443_grant() {
        let f = host_port_fixture(443).await;
        assert_eq!(connect_only(&f, &format!("{GUEST_HOST}:abc")).await.err(), Some(StatusCode::BAD_REQUEST));
        assert!(f.entries().is_empty(), "{:?}", f.entries());
    }

    fn ask_policy() -> NetworkPolicy {
        NetworkPolicy { default: DefaultAction::Ask, ..Default::default() }
    }

    /// Waits for the one pending ask and returns its id.
    async fn pending(f: &Fixture) -> String {
        assert!(eventually(|| f.live.asks.list().len() == 1).await, "no ask");
        f.live.asks.list()[0].id.clone()
    }

    #[tokio::test]
    async fn an_ask_holds_the_request_until_the_answer() {
        let f = fixture(ask_policy(), Vec::new(), Vec::new()).await;
        let port = upstream().await;
        let uri = format!("http://api.test:{port}/");
        for (answer, status, reason) in
            [(ask::Answer::Allow, StatusCode::OK, "ask"), (ask::Answer::Block, StatusCode::FORBIDDEN, "ask")]
        {
            let answered = async {
                let id = pending(&f).await;
                let ask = f.live.asks.get(&id).unwrap();
                assert_eq!((ask.host.as_str(), ask.method.as_str()), ("api.test", "GET"));
                f.live.asks.answer(&id, answer).unwrap();
            };
            let ((got, _, _), ()) = tokio::join!(plain(&f, &uri, Some(f.auth())), answered);
            assert_eq!(got, status);
            assert_eq!(f.entries().last().unwrap().reason, reason);
            assert!(f.live.asks.list().is_empty());
        }
    }

    #[tokio::test]
    async fn an_unanswered_ask_is_blocked_at_the_deadline() {
        let f = fixture(ask_policy(), Vec::new(), Vec::new()).await;
        let (status, headers, _) = plain(&f, "http://api.test:1/", Some(f.auth())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(headers[BLOCKED_HEADER], "ask-timeout");
        assert!(f.live.asks.list().is_empty());
    }

    #[tokio::test]
    async fn a_policy_edit_or_a_rule_until_stop_decides_a_held_request() {
        let f = fixture(ask_policy(), Vec::new(), Vec::new()).await;
        let port = upstream().await;
        let uri = format!("http://api.test:{port}/");
        let edit = async {
            pending(&f).await;
            f.live.edit(|p, _| p.rules.push(rule("api.test", Action::Allow)));
        };
        let ((status, _, _), ()) = tokio::join!(plain(&f, &uri, Some(f.auth())), edit);
        assert_eq!((status, f.entries().last().unwrap().reason.as_str()), (StatusCode::OK, "rule:api.test"));

        f.live.edit(|p, _| p.rules.clear());
        let until_stop = async {
            pending(&f).await;
            f.live.allow_until_stop(Target::Host { host: "api.test".into() }).unwrap()
        };
        let ((status, _, _), rule) = tokio::join!(plain(&f, &uri, Some(f.auth())), until_stop);
        assert_eq!(status, StatusCode::OK);
        // The rule stays for the next request, with no ask.
        assert_eq!(plain(&f, &uri, Some(f.auth())).await.0, StatusCode::OK);
        f.live.remove_temporary(&rule.id).unwrap();
        assert!(f.live.temporary().is_empty());
    }

    #[tokio::test]
    async fn a_full_ask_queue_blocks_at_once() {
        let f = fixture(ask_policy(), Vec::new(), Vec::new()).await;
        let app = Arc::new(Semaphore::new(1));
        let tickets: Vec<_> = (0..ask::VM_PENDING)
            .map(|_| f.live.asks.open("x.test", 443, "GET", &Arc::new(Semaphore::new(1)), 0, Arc::new(|| {})).unwrap())
            .collect();
        assert!(f.live.asks.open("x.test", 443, "GET", &app, 0, Arc::new(|| {})).is_none());
        let started = Instant::now();
        let (status, headers, _) = plain(&f, "http://api.test:1/", Some(f.auth())).await;
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "ask-timeout"));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(tickets);
    }

    #[tokio::test]
    async fn an_ask_in_a_tunnel_is_held_on_the_inner_request() {
        let recipients = temp_ca();
        let (port, seen) = tls_recipient(&recipients, "a1.test").await;
        let f = start(ask_policy(), HashMap::new(), Vec::new(), Vec::new(), Some(recipients.der().clone())).await;
        let answered = async {
            let id = pending(&f).await;
            f.live.asks.answer(&id, ask::Answer::Allow).unwrap();
        };
        let (status, ()) = tokio::join!(via_tunnel(&f, "a1.test", port), answered);
        assert_eq!(status, Ok(StatusCode::OK));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_method_list_limits_an_allow_rule_and_its_credential() {
        let mut limited = rule("api.test", Action::Allow);
        limited.methods = Some(vec!["GET".into()]);
        let policy = NetworkPolicy { default: DefaultAction::Allow, rules: vec![limited], ..Default::default() };
        let f = fixture(policy, Vec::new(), Vec::new()).await;
        let port = upstream().await;
        let uri = format!("http://api.test:{port}/");
        assert_eq!(plain(&f, &uri, Some(f.auth())).await.0, StatusCode::OK);
        let tcp = TcpStream::connect(("127.0.0.1", f.proxy.port)).await.unwrap();
        let post = Request::post(&uri).header(PROXY_AUTHORIZATION, f.auth()).body(Empty::new()).unwrap();
        let (status, headers, _) = send_over(TokioIo::new(tcp), post).await;
        // The default Allow does not open a method that the matched rule leaves out.
        assert_eq!((status, headers[BLOCKED_HEADER].to_str().unwrap()), (StatusCode::FORBIDDEN, "method"));
    }
}
