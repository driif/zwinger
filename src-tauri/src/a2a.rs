//! The A2A bridge: one in-app run, shared over the A2A 1.0 HTTP+JSON
//! binding on `127.0.0.1` with an OS-assigned port.
//!
//! A tool on this Mac reads the run's status and typed messages, and sends
//! advisory messages to its following stages. The bridge is polling only.
//! The Agent Card is public and names no run. Each other endpoint needs the
//! share's bearer credential. The credential is in memory only, and a stop
//! of the share, a removal of the run, or the app exit ends it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Read as _;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use tokio::net::TcpListener;
use tokio::task::{AbortHandle, JoinSet};

use crate::error::{Error, Result};
use crate::task_artifacts::{self, Message, Room};
use crate::task_runs::{Contribution, EXTERNAL, Refusal, RunHandle, RunState, Status};

/// The protocol version, as `Major.Minor`.
const VERSION: &str = "1.0";
const CARD_PATH: &str = "/.well-known/agent-card.json";
const MEDIA_TYPE: &str = "application/a2a+json";
/// A request body: a message of 16 KiB with JSON escapes, and the envelope.
const BODY_LIMIT: usize = 128 << 10;
/// How long SendMessage waits until the round has the message.
const APPLY_WAIT: Duration = Duration::from_secs(2);
const HEADER_WAIT: Duration = Duration::from_secs(10);
const MESSAGE_ID_LIMIT: usize = 256;
const PAGE_SIZE: i64 = 50;
const PAGE_SIZE_LIMIT: i64 = 100;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a client needs to use a share. The app shows it to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareInfo {
    /// The base URL of the HTTP+JSON interface.
    pub url: String,
    pub card_url: String,
    /// The A2A task: the shared run.
    pub task_id: String,
    pub context_id: String,
    /// The bearer credential.
    pub token: String,
}

struct Share {
    session_id: String,
    task_id: String,
    run: String,
    info: ShareInfo,
    /// The accept loop. It owns the connection tasks, so its abort also
    /// ends them and releases the run handle.
    server: AbortHandle,
}

impl Drop for Share {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The A2A shares of this app session. Sharing is off at app start.
#[derive(Default)]
pub struct Shares {
    shares: Mutex<Vec<Share>>,
}

impl Shares {
    /// Starts a share of the run, or returns the share that runs. `find`
    /// gets the run under the lock of the shares, so a removal of the run
    /// that [`Self::forget`] follows cannot leave a share behind.
    pub fn start(
        &self,
        session_id: &str,
        task_id: &str,
        run: &str,
        dir: PathBuf,
        find: impl FnOnce() -> Option<Arc<RunHandle>>,
    ) -> Result<ShareInfo> {
        let mut shares = lock(&self.shares);
        if let Some(share) = shares.iter().find(|s| s.session_id == session_id && s.task_id == task_id && s.run == run)
        {
            return Ok(share.info.clone());
        }
        let handle = find().ok_or(Error::RunNotShareable)?;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let info = ShareInfo {
            card_url: format!("{url}{CARD_PATH}"),
            task_id: format!("{task_id}-{run}"),
            context_id: task_id.to_owned(),
            token: token()?,
            url,
        };
        let bridge = Arc::new(Bridge { run: handle, dir, info: info.clone() });
        let server = tokio::spawn(serve(listener, bridge)).abort_handle();
        shares.push(Share {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
            run: run.to_owned(),
            info: info.clone(),
            server,
        });
        Ok(info)
    }

    pub fn get(&self, session_id: &str, task_id: &str, run: &str) -> Option<ShareInfo> {
        lock(&self.shares)
            .iter()
            .find(|s| s.session_id == session_id && s.task_id == task_id && s.run == run)
            .map(|s| s.info.clone())
    }

    /// Stops the share of the run, if there is one.
    pub fn stop(&self, session_id: &str, task_id: &str, run: &str) {
        lock(&self.shares).retain(|s| !(s.session_id == session_id && s.task_id == task_id && s.run == run));
    }

    /// Stops the shares of a session or of one of its tasks.
    pub fn forget(&self, session_id: &str, task_id: Option<&str>) {
        lock(&self.shares).retain(|s| s.session_id != session_id || task_id.is_some_and(|t| s.task_id != t));
    }

    pub fn stop_all(&self) {
        lock(&self.shares).clear();
    }
}

/// 256 random bits, as URL-safe base64.
fn token() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

async fn serve(listener: TcpListener, bridge: Arc<Bridge>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((tcp, _)) = accepted else {
                    // Such as no free file descriptor: try again a little later.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let bridge = bridge.clone();
                connections.spawn(async move {
                    let service = service_fn(move |req| {
                        let bridge = bridge.clone();
                        async move { Ok::<_, Infallible>(bridge.respond(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_WAIT)
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
            Some(_) = connections.join_next() => {}
        }
    }
}

type Reply = Response<Full<Bytes>>;
/// An error reply. Boxed, so a `Result` stays small.
type Failure = Box<Reply>;

fn reply(code: StatusCode, content_type: &str, body: &Value) -> Reply {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = code;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type.parse().expect("a valid media type"));
    headers.insert(header::CACHE_CONTROL, "no-store".parse().expect("a valid header value"));
    response
}

fn ok(body: &Value) -> Reply {
    reply(StatusCode::OK, MEDIA_TYPE, body)
}

/// A `google.rpc.Status` error.
fn status(code: StatusCode, name: &str, message: &str, details: Vec<Value>) -> Reply {
    reply(
        code,
        MEDIA_TYPE,
        &json!({ "error": { "code": code.as_u16(), "status": name, "message": message, "details": details } }),
    )
}

/// The A2A errors that this bridge returns.
#[derive(Debug, Clone, Copy)]
enum A2aError {
    TaskNotFound,
    UnsupportedOperation,
    PushNotificationNotSupported,
    ContentTypeNotSupported,
    VersionNotSupported,
}

/// An A2A error, with the `ErrorInfo` that names it.
fn a2a(error: A2aError, message: &str) -> Failure {
    let (code, name, reason) = match error {
        A2aError::TaskNotFound => (StatusCode::NOT_FOUND, "NOT_FOUND", "TASK_NOT_FOUND"),
        A2aError::UnsupportedOperation => (StatusCode::BAD_REQUEST, "FAILED_PRECONDITION", "UNSUPPORTED_OPERATION"),
        A2aError::PushNotificationNotSupported => {
            (StatusCode::BAD_REQUEST, "FAILED_PRECONDITION", "PUSH_NOTIFICATION_NOT_SUPPORTED")
        }
        A2aError::ContentTypeNotSupported => {
            (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", "CONTENT_TYPE_NOT_SUPPORTED")
        }
        A2aError::VersionNotSupported => (StatusCode::BAD_REQUEST, "FAILED_PRECONDITION", "VERSION_NOT_SUPPORTED"),
    };
    let mut info =
        json!({ "@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": reason, "domain": "a2a-protocol.org" });
    if let A2aError::VersionNotSupported = error {
        info["metadata"] = json!({ "supportedVersions": VERSION });
    }
    Box::new(status(code, name, message, vec![info]))
}

fn invalid(field: &str, why: &str) -> Failure {
    let detail = json!({
        "@type": "type.googleapis.com/google.rpc.BadRequest",
        "fieldViolations": [{ "field": field, "description": why }],
    });
    Box::new(status(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", why, vec![detail]))
}

/// A known endpoint with a method that it does not have.
fn unsupported_method() -> Failure {
    a2a(A2aError::UnsupportedOperation, "This endpoint does not support this HTTP method.")
}

fn not_found() -> Failure {
    Box::new(status(StatusCode::NOT_FOUND, "NOT_FOUND", "This endpoint does not exist.", Vec::new()))
}

fn unauthenticated() -> Failure {
    let mut response = status(
        StatusCode::UNAUTHORIZED,
        "UNAUTHENTICATED",
        "Send the credential of the share as Authorization: Bearer <token>.",
        Vec::new(),
    );
    response.headers_mut().insert(header::WWW_AUTHENTICATE, "Bearer".parse().expect("a valid header value"));
    Box::new(response)
}

/// The query parameters, percent-decoded.
fn query(raw: Option<&str>) -> HashMap<String, String> {
    raw.unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `historyLength`: `None` for all messages.
fn history_length(query: &HashMap<String, String>) -> std::result::Result<Option<usize>, Failure> {
    match query.get("historyLength") {
        None => Ok(None),
        Some(value) => value
            .parse::<u32>()
            .map(|n| Some(n as usize))
            .map_err(|_| invalid("historyLength", "historyLength must be a number from 0")),
    }
}

fn task_state(state: RunState) -> &'static str {
    match state {
        // A pause is not an A2A state: the run still has work.
        RunState::Running | RunState::Paused => "TASK_STATE_WORKING",
        RunState::Waiting => "TASK_STATE_INPUT_REQUIRED",
        RunState::Done => "TASK_STATE_COMPLETED",
        RunState::Failed => "TASK_STATE_FAILED",
        RunState::Cancelled => "TASK_STATE_CANCELED",
    }
}

const TASK_STATES: [&str; 8] = [
    "TASK_STATE_SUBMITTED",
    "TASK_STATE_WORKING",
    "TASK_STATE_COMPLETED",
    "TASK_STATE_FAILED",
    "TASK_STATE_CANCELED",
    "TASK_STATE_INPUT_REQUIRED",
    "TASK_STATE_REJECTED",
    "TASK_STATE_AUTH_REQUIRED",
];

/// The bridge of one share.
struct Bridge {
    run: Arc<RunHandle>,
    /// The authoritative round in host app data, never the guest mirror.
    dir: PathBuf,
    info: ShareInfo,
}

impl Bridge {
    async fn respond(&self, req: Request<Incoming>) -> Reply {
        self.route(req).await.unwrap_or_else(|failure| *failure)
    }

    async fn route(&self, req: Request<Incoming>) -> std::result::Result<Reply, Failure> {
        let path = req.uri().path().to_owned();
        let query = query(req.uri().query());
        let method = req.method().clone();
        if path == CARD_PATH {
            return if method == Method::GET { Ok(ok(&self.card())) } else { Err(unsupported_method()) };
        }
        self.authorize(req.headers())?;
        version(req.headers(), &query)?;
        let streaming = "This agent does not stream. Poll the task with GetTask.";
        match (method, path.as_str()) {
            (Method::GET, "/tasks") => self.list(&query).await,
            (Method::POST, "/message:send") => self.send(req.into_body()).await,
            (_, "/tasks" | "/message:send") => Err(unsupported_method()),
            (_, "/message:stream") => Err(a2a(A2aError::UnsupportedOperation, streaming)),
            (_, "/extendedAgentCard") => {
                Err(a2a(A2aError::UnsupportedOperation, "This agent has no extended Agent Card."))
            }
            (method, path) => match path.strip_prefix("/tasks/") {
                Some(rest) if rest.contains("/pushNotificationConfigs") => Err(a2a(
                    A2aError::PushNotificationNotSupported,
                    "This agent does not send push notifications. Poll the task with GetTask.",
                )),
                Some(rest) if rest.ends_with(":cancel") => Err(a2a(
                    A2aError::UnsupportedOperation,
                    "A client cannot cancel the run. The user cancels it in the zwinger app.",
                )),
                Some(rest) if rest.ends_with(":subscribe") => Err(a2a(A2aError::UnsupportedOperation, streaming)),
                Some(id) if !id.is_empty() && !id.contains(['/', ':']) => {
                    if method == Method::GET {
                        self.get(id, &query).await
                    } else {
                        Err(unsupported_method())
                    }
                }
                _ => Err(not_found()),
            },
        }
    }

    fn authorize(&self, headers: &HeaderMap) -> std::result::Result<(), Failure> {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map_or("", |(_, token)| token.trim());
        if bool::from(given.as_bytes().ct_eq(self.info.token.as_bytes())) { Ok(()) } else { Err(unauthenticated()) }
    }

    fn card(&self) -> Value {
        json!({
            "name": "zwinger task run",
            "description": "One run of a zwinger task, which the user shares from the zwinger app on this Mac. \
                Read the status of the run and its typed messages {from, to, kind, body}. Send an advisory message \
                that the following stages of the run include. The share is local only, and it ends when the user \
                stops it, removes the run or quits the app.",
            "supportedInterfaces": [{ "url": self.info.url, "protocolBinding": "HTTP+JSON", "protocolVersion": VERSION }],
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": { "streaming": false, "pushNotifications": false, "extendedAgentCard": false },
            "securitySchemes": {
                "bearer": {
                    "httpAuthSecurityScheme": {
                        "scheme": "Bearer",
                        "description": "The credential that the zwinger app shows for this share."
                    }
                }
            },
            "securityRequirements": [{ "schemes": { "bearer": { "list": [] } } }],
            "defaultInputModes": ["text/plain", "text/markdown"],
            "defaultOutputModes": ["text/markdown", "application/json"],
            "skills": [{
                "id": "run-room",
                "name": "Run room",
                "description": "Read the run's status and messages with GetTask or ListTasks. Send an advisory \
                    message with SendMessage and the task ID of the share. The user and the two agents of the run \
                    decide; an advisory message does not answer open questions or approve a result.",
                "tags": ["task-run", "transcript", "advisory"],
                "inputModes": ["text/plain", "text/markdown"],
                "outputModes": ["text/markdown", "application/json"]
            }]
        })
    }

    async fn get(&self, id: &str, query: &HashMap<String, String>) -> std::result::Result<Reply, Failure> {
        if id != self.info.task_id {
            return Err(a2a(A2aError::TaskNotFound, "The task does not exist or this share cannot read it."));
        }
        let history = history_length(query)?;
        Ok(ok(&self.task(self.run.status(), history, false).await))
    }

    async fn list(&self, query: &HashMap<String, String>) -> std::result::Result<Reply, Failure> {
        let history = history_length(query)?;
        let page_size = match query.get("pageSize") {
            None => PAGE_SIZE,
            Some(v) => v
                .parse::<i64>()
                .ok()
                .filter(|n| (1..=PAGE_SIZE_LIMIT).contains(n))
                .ok_or_else(|| invalid("pageSize", "pageSize must be a number from 1 to 100"))?,
        };
        if query.get("pageToken").is_some_and(|t| !t.is_empty()) {
            return Err(invalid("pageToken", "This agent returns one page, so a page token is not valid."));
        }
        let artifacts = match query.get("includeArtifacts").map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid("includeArtifacts", "includeArtifacts must be true or false")),
        };
        if query.contains_key("statusTimestampAfter") {
            return Err(a2a(A2aError::UnsupportedOperation, "This agent does not filter by statusTimestampAfter."));
        }
        let wanted_state = query.get("status").map(String::as_str);
        if let Some(state) = wanted_state
            && !TASK_STATES.contains(&state)
        {
            return Err(invalid("status", "status must be a TaskState name, such as TASK_STATE_WORKING"));
        }
        let status = self.run.status();
        let matches = query.get("contextId").is_none_or(|c| *c == self.info.context_id)
            && wanted_state.is_none_or(|s| s == task_state(status.state));
        let tasks = if matches { vec![self.task(status, history, artifacts).await] } else { Vec::new() };
        let total = tasks.len();
        Ok(ok(&json!({ "tasks": tasks, "nextPageToken": "", "pageSize": page_size, "totalSize": total })))
    }

    /// The run as an A2A task. `status` comes before the read: a run sets
    /// its end state only after its last file.
    async fn task(&self, status: Status, history: Option<usize>, artifacts: bool) -> Value {
        let dir = self.dir.clone();
        let room = tokio::task::spawn_blocking(move || task_artifacts::read_room(&dir)).await.unwrap_or_default();
        let mut task = json!({
            "id": self.info.task_id,
            "contextId": self.info.context_id,
            "status": self.status(&status, &room),
            "metadata": { "runState": status.state, "stage": status.stage },
        });
        if history != Some(0) {
            let skip = history.map_or(0, |n| room.messages.len().saturating_sub(n));
            task["history"] = room.messages[skip..].iter().map(|m| self.message(m)).collect();
        }
        if artifacts {
            task["artifacts"] = json!([]);
        }
        task
    }

    fn status(&self, status: &Status, room: &Room) -> Value {
        let latest = room
            .messages
            .iter()
            .filter_map(|m| m.at.as_deref())
            .chain(room.stages.iter().filter_map(|s| s.started.as_deref()))
            .max();
        let mut out = json!({ "state": task_state(status.state) });
        if let Some(at) = room.ended_at.as_deref().or(latest) {
            out["timestamp"] = json!(at);
        }
        let note = match status.state {
            RunState::Waiting => Some(format!(
                "The run waits for the user's answers in the zwinger app. An A2A message does not answer them.\n\n{}",
                room.questions.iter().map(|q| format!("- {q}")).collect::<Vec<_>>().join("\n")
            )),
            RunState::Failed | RunState::Cancelled => room.outcome.as_ref().and_then(|o| o.stopped.clone()),
            _ => None,
        };
        if let Some(text) = note {
            out["message"] = self.agent_message(format!("{}-status", self.info.task_id), &text, None);
        }
        out
    }

    /// A room message: its text, and its typed fields as a data part.
    fn message(&self, m: &Message) -> Value {
        let role = if m.from == "user" || m.from == EXTERNAL { "ROLE_USER" } else { "ROLE_AGENT" };
        json!({
            "messageId": m.id,
            "contextId": self.info.context_id,
            "taskId": self.info.task_id,
            "role": role,
            "parts": [
                { "text": m.body, "mediaType": "text/markdown" },
                {
                    "data": {
                        "from": m.from, "to": m.to, "kind": m.kind, "stage": m.stage, "at": m.at, "body": m.body
                    },
                    "mediaType": "application/json"
                }
            ]
        })
    }

    fn agent_message(&self, id: String, text: &str, data: Option<Value>) -> Value {
        let mut parts = vec![json!({ "text": text, "mediaType": "text/plain" })];
        if let Some(data) = data {
            parts.push(json!({ "data": data, "mediaType": "application/json" }));
        }
        json!({
            "messageId": id,
            "contextId": self.info.context_id,
            "taskId": self.info.task_id,
            "role": "ROLE_AGENT",
            "parts": parts,
        })
    }

    async fn send(&self, body: Incoming) -> std::result::Result<Reply, Failure> {
        let bytes = Limited::new(body, BODY_LIMIT)
            .collect()
            .await
            .map_err(|_| invalid("message", "The request body must have at most 128 KiB."))?
            .to_bytes();
        let request: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("message", "The request body is not JSON."))?;
        let message = &request["message"];
        if !message.is_object() {
            return Err(invalid("message", "A SendMessageRequest needs a message."));
        }
        let id = message["messageId"]
            .as_str()
            .filter(|id| !id.trim().is_empty() && id.len() <= MESSAGE_ID_LIMIT)
            .ok_or_else(|| invalid("message.messageId", "message.messageId must have 1 to 256 bytes"))?;
        if message["role"] != "ROLE_USER" {
            return Err(invalid("message.role", "message.role must be ROLE_USER"));
        }
        let not_found = || a2a(A2aError::TaskNotFound, "The task does not exist or this share cannot write to it.");
        match message["taskId"].as_str() {
            None => {
                return Err(invalid("message.taskId", "message.taskId must name the task of the share"));
            }
            Some(task) if task != self.info.task_id => return Err(not_found()),
            Some(_) => {}
        }
        if !message["contextId"].is_null() && message["contextId"].as_str() != Some(self.info.context_id.as_str()) {
            return Err(not_found());
        }
        if !request["configuration"]["taskPushNotificationConfig"].is_null() {
            return Err(a2a(A2aError::PushNotificationNotSupported, "This agent does not send push notifications."));
        }
        let text = text_of(&message["parts"])?;
        let applied = match self.run.contribute(id, &text) {
            Ok(Contribution::Applied) => true,
            Ok(Contribution::Pending) => false,
            Ok(Contribution::Queued(rx)) => match tokio::time::timeout(APPLY_WAIT, rx).await {
                Ok(Ok(())) => true,
                Ok(Err(_)) => {
                    return Err(ended(
                        "The run did not apply the message: it ended or could not record it. It accepts no more \
                         messages.",
                    ));
                }
                Err(_) => false,
            },
            Err(Refusal::Ended) => {
                return Err(ended(
                    "The run has ended, stops or could not record a message, so it accepts no more messages.",
                ));
            }
            Err(Refusal::Conflict) => {
                return Err(invalid("message.messageId", "This messageId came before with a different text."));
            }
            Err(Refusal::Invalid(why)) => return Err(invalid("message.parts", why)),
        };
        let text = if applied {
            "Applied. The run added the message to its record. Each following stage prompt includes it as an \
             advisory message from an A2A client."
        } else {
            "Queued. The run applies the message at its next step. Then each following stage prompt includes it as \
             an advisory message from an A2A client. If the run ends first, the message does not apply. Read the \
             task to see it."
        };
        let data = json!({ "messageId": id, "result": if applied { "applied" } else { "queued" } });
        let ack = self.agent_message(format!("ack-{}", uuid::Uuid::new_v4()), text, Some(data));
        Ok(ok(&json!({ "message": ack })))
    }
}

fn ended(message: &str) -> Failure {
    a2a(A2aError::UnsupportedOperation, message)
}

/// The `A2A-Version` header, or the request parameter. No version is 0.3.
fn version(headers: &HeaderMap, query: &HashMap<String, String>) -> std::result::Result<(), Failure> {
    let given = headers
        .get("a2a-version")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("A2A-Version").map(String::as_str))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("0.3");
    if given == VERSION {
        return Ok(());
    }
    Err(a2a(
        A2aError::VersionNotSupported,
        &format!("This agent supports A2A version {VERSION}, not {given}. Send the header A2A-Version: {VERSION}."),
    ))
}

/// The text of the message parts. Only text parts are valid.
fn text_of(parts: &Value) -> std::result::Result<String, Failure> {
    let parts = parts
        .as_array()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| invalid("message.parts", "message.parts must have a text part"))?;
    let mut texts = Vec::new();
    for part in parts {
        if ["raw", "url", "data"].iter().any(|k| !part[k].is_null()) {
            return Err(a2a(A2aError::ContentTypeNotSupported, "This agent accepts only text parts."));
        }
        if let Some(media) = part["mediaType"].as_str()
            && !matches!(media, "text/plain" | "text/markdown")
        {
            return Err(a2a(A2aError::ContentTypeNotSupported, "A text part must be text/plain or text/markdown."));
        }
        let text = part["text"].as_str().ok_or_else(|| invalid("message.parts", "Each part must have text."))?;
        texts.push(text);
    }
    Ok(texts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::*;
    use crate::task_artifacts::read_room;
    use crate::task_runs::Control;
    use crate::task_runs::tests::{
        Fake, Stage, Started, calls_of, happy, no_git, plan, resume_full, settled, start, until,
    };

    /// A run whose first plan waits for the gate, and its share.
    fn shared(gate: Arc<Notify>) -> (Started, Shares, ShareInfo) {
        let run = start(move |agent, stage, prompt| match stage {
            Stage::Plan if agent == "claude" => Fake::After(
                gate.clone(),
                match plan(agent) {
                    Fake::Reply(text) => text,
                    _ => unreachable!(),
                },
            ),
            _ => happy(agent, stage, prompt),
        });
        let shares = Shares::default();
        let handle = run.handle.clone();
        let info = shares.start("s", "task-1", "01", run.dir.clone(), || Some(handle)).unwrap();
        (run, shares, info)
    }

    struct Client<'a> {
        info: &'a ShareInfo,
        token: Option<&'a str>,
        version: Option<&'a str>,
    }

    impl<'a> Client<'a> {
        fn new(info: &'a ShareInfo) -> Self {
            Self { info, token: Some(&info.token), version: Some(VERSION) }
        }

        /// One request on a new loopback connection: the status and the JSON body.
        async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
            let address = self.info.url.strip_prefix("http://").unwrap();
            let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.unwrap();
            tokio::spawn(connection);
            let mut request = Request::builder().method(method).uri(path).header(header::HOST, address);
            if let Some(token) = self.token {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            if let Some(version) = self.version {
                request = request.header("A2A-Version", version);
            }
            let body = body.map(|b| b.to_string()).unwrap_or_default();
            let request = request.header(header::CONTENT_TYPE, MEDIA_TYPE).body(Full::new(Bytes::from(body))).unwrap();
            let response = sender.send_request(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }

        async fn get(&self, path: &str) -> (StatusCode, Value) {
            self.call(Method::GET, path, None).await
        }

        async fn send(&self, id: &str, task_id: &str, parts: Value) -> (StatusCode, Value) {
            let message = json!({ "messageId": id, "role": "ROLE_USER", "taskId": task_id, "parts": parts });
            self.call(Method::POST, "/message:send", Some(json!({ "message": message }))).await
        }
    }

    fn reason(body: &Value) -> &str {
        body["error"]["details"][0]["reason"].as_str().unwrap_or_default()
    }

    fn texts(task: &Value) -> Vec<String> {
        task["history"].as_array().unwrap().iter().map(|m| m["parts"][0]["text"].as_str().unwrap().to_owned()).collect()
    }

    fn messages_file(run: &Started) -> String {
        std::fs::read_to_string(run.dir.join("messages.jsonl")).unwrap_or_default()
    }

    #[tokio::test]
    async fn the_card_is_public_and_the_rest_needs_the_credential_and_the_version() {
        let gate = Arc::new(Notify::new());
        let (run, shares, info) = shared(gate.clone());
        assert!(info.url.starts_with("http://127.0.0.1:"));
        let anonymous = Client { info: &info, token: None, version: None };
        let (code, card) = anonymous.get(CARD_PATH).await;
        assert_eq!(code, StatusCode::OK);
        let (code, body) = anonymous.call(Method::POST, CARD_PATH, None).await;
        assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, "UNSUPPORTED_OPERATION"));
        assert_eq!(
            card["supportedInterfaces"],
            json!([{ "url": info.url, "protocolBinding": "HTTP+JSON", "protocolVersion": "1.0" }])
        );
        assert_eq!(
            card["capabilities"],
            json!({ "streaming": false, "pushNotifications": false, "extendedAgentCard": false })
        );
        assert!(!card.to_string().contains(&info.task_id), "the card names no run");

        let path = format!("/tasks/{}", info.task_id);
        let (code, body) = anonymous.get(&path).await;
        assert_eq!((code, body["error"]["status"].as_str()), (StatusCode::UNAUTHORIZED, Some("UNAUTHENTICATED")));
        let wrong = Client { info: &info, token: Some("x"), version: Some(VERSION) };
        assert_eq!(wrong.get(&path).await.0, StatusCode::UNAUTHORIZED);
        // No version is 0.3.
        let old = Client { info: &info, token: Some(&info.token), version: None };
        let (code, body) = old.get(&path).await;
        assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, "VERSION_NOT_SUPPORTED"));
        let (code, _) = old.get(&format!("{path}?A2A-Version=1.0")).await;
        assert_eq!(code, StatusCode::OK);

        let client = Client::new(&info);
        for (method, path, want) in [
            (Method::POST, format!("/tasks/{}:cancel", info.task_id), "UNSUPPORTED_OPERATION"),
            (Method::POST, format!("/tasks/{}:subscribe", info.task_id), "UNSUPPORTED_OPERATION"),
            (Method::POST, "/message:stream".to_owned(), "UNSUPPORTED_OPERATION"),
            (Method::GET, "/extendedAgentCard".to_owned(), "UNSUPPORTED_OPERATION"),
            (
                Method::GET,
                format!("/tasks/{}/pushNotificationConfigs", info.task_id),
                "PUSH_NOTIFICATION_NOT_SUPPORTED",
            ),
            (Method::GET, "/message:send".to_owned(), "UNSUPPORTED_OPERATION"),
            (Method::POST, "/tasks".to_owned(), "UNSUPPORTED_OPERATION"),
            (Method::DELETE, format!("/tasks/{}", info.task_id), "UNSUPPORTED_OPERATION"),
        ] {
            let (code, body) = client.call(method, &path, None).await;
            assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, want), "{path}");
            assert_eq!(body["error"]["details"][0]["domain"], "a2a-protocol.org");
        }
        gate.notify_one();
        settled(&run).await;
        drop(shares);
    }

    #[tokio::test]
    async fn a_client_reads_the_run_and_its_contribution_reaches_the_next_prompt() {
        let gate = Arc::new(Notify::new());
        let (run, shares, info) = shared(gate.clone());
        let client = Client::new(&info);

        let (code, body) = client.get("/tasks/task-1-02").await;
        assert_eq!((code, reason(&body)), (StatusCode::NOT_FOUND, "TASK_NOT_FOUND"));
        let (code, task) = client.get(&format!("/tasks/{}", info.task_id)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!((task["id"].as_str(), task["contextId"].as_str()), (Some(info.task_id.as_str()), Some("task-1")));
        assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");

        // Wrong identities and content change nothing.
        let text = json!([{ "text": "Check the cache too." }]);
        let (code, body) = client.send("m-1", "task-1-02", text.clone()).await;
        assert_eq!((code, reason(&body)), (StatusCode::NOT_FOUND, "TASK_NOT_FOUND"));
        let other_context = json!({ "message": { "messageId": "m-1", "role": "ROLE_USER", "taskId": info.task_id,
            "contextId": "other", "parts": text } });
        let (code, _) = client.call(Method::POST, "/message:send", Some(other_context)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, body) = client.send("m-1", &info.task_id, json!([{ "url": "https://example.com/a.png" }])).await;
        assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, "CONTENT_TYPE_NOT_SUPPORTED"));
        let (code, body) = client.call(Method::POST, "/message:send", Some(json!({ "message": "no" }))).await;
        assert_eq!((code, body["error"]["status"].as_str()), (StatusCode::BAD_REQUEST, Some("INVALID_ARGUMENT")));
        assert_eq!(messages_file(&run), "");

        let (code, reply) = client.send("m-1", &info.task_id, text.clone()).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(reply["message"]["role"], "ROLE_AGENT");
        assert_eq!(reply["message"]["parts"][1]["data"], json!({ "messageId": "m-1", "result": "applied" }));
        let (_, again) = client.send("m-1", &info.task_id, text.clone()).await;
        assert_eq!(again["message"]["parts"][1]["data"]["result"], "applied");
        let (code, body) = client.send("m-1", &info.task_id, json!([{ "text": "Other." }])).await;
        assert_eq!((code, body["error"]["status"].as_str()), (StatusCode::BAD_REQUEST, Some("INVALID_ARGUMENT")));
        assert_eq!(messages_file(&run).lines().count(), 1);

        let (_, task) = client.get(&format!("/tasks/{}", info.task_id)).await;
        let external =
            task["history"].as_array().unwrap().iter().find(|m| m["messageId"] == "messages.jsonl#1").unwrap();
        assert_eq!(external["role"], "ROLE_USER");
        assert_eq!(external["parts"][1]["data"]["from"], "a2a");
        assert_eq!(external["parts"][1]["data"]["kind"], "external");

        gate.notify_one();
        settled(&run).await;
        let prompt = &calls_of(&run, Stage::Exchange)[0].prompt;
        assert!(prompt.contains("## Advisory messages from external tools (A2A)\n"));
        assert!(prompt.contains("- (during plan) Check the cache too."));

        // An ended run stays readable with its real state, and refuses new messages.
        let (_, task) = client.get(&format!("/tasks/{}", info.task_id)).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        let ids: Vec<Value> = task["history"].as_array().unwrap().iter().map(|m| m["messageId"].clone()).collect();
        assert!(ids.contains(&json!("turns/plan-claude.json")) && ids.contains(&json!("final-codex.json")));
        let room = read_room(&run.dir);
        for message in task["history"].as_array().unwrap() {
            let id = message["messageId"].as_str().unwrap();
            let source = room.messages.iter().find(|m| m.id == id).unwrap();
            assert_eq!(message["parts"][1]["data"]["body"], source.body, "{id}");
        }
        let (_, again) = client.get(&format!("/tasks/{}", info.task_id)).await;
        assert_eq!(again["history"], task["history"], "stable IDs and order");
        let (_, last) = client.get(&format!("/tasks/{}?historyLength=2", info.task_id)).await;
        assert_eq!(texts(&last), texts(&task)[texts(&task).len() - 2..]);
        let (_, none) = client.get(&format!("/tasks/{}?historyLength=0", info.task_id)).await;
        assert!(none.get("history").is_none());
        let (code, _) = client.get(&format!("/tasks/{}?historyLength=-1", info.task_id)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, body) = client.send("m-2", &info.task_id, json!([{ "text": "Late." }])).await;
        assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, "UNSUPPORTED_OPERATION"));
        // A repeated message also gets the error of the ended run.
        let (code, body) = client.send("m-1", &info.task_id, text).await;
        assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, "UNSUPPORTED_OPERATION"));
        assert_eq!(messages_file(&run).lines().count(), 1);

        let (_, list) = client.get("/tasks?status=TASK_STATE_COMPLETED&historyLength=0").await;
        assert_eq!((list["totalSize"].as_i64(), list["nextPageToken"].as_str()), (Some(1), Some("")));
        assert_eq!(list["tasks"][0]["id"].as_str(), Some(info.task_id.as_str()));
        let (_, other) = client.get("/tasks?contextId=other").await;
        assert_eq!(other["tasks"], json!([]));
        assert_eq!(client.get("/tasks?pageToken=x").await.0, StatusCode::BAD_REQUEST);
        drop(shares);
    }

    #[tokio::test]
    async fn a_contribution_that_the_run_cannot_record_ends_the_contributions() {
        let gate = Arc::new(Notify::new());
        let (run, shares, info) = shared(gate.clone());
        // A folder in place of the file makes each append fail.
        std::fs::create_dir(run.dir.join("messages.jsonl")).unwrap();
        let client = Client::new(&info);
        for id in ["m-1", "m-1", "m-2"] {
            let (code, body) = client.send(id, &info.task_id, json!([{ "text": "Check the cache." }])).await;
            assert_eq!((code, reason(&body)), (StatusCode::BAD_REQUEST, "UNSUPPORTED_OPERATION"), "{id}");
        }
        let (_, task) = client.get(&format!("/tasks/{}", info.task_id)).await;
        assert!(task["history"].as_array().unwrap().iter().all(|m| m["parts"][1]["data"]["kind"] != "external"));
        gate.notify_one();
        settled(&run).await;
        let (_, task) = client.get(&format!("/tasks/{}", info.task_id)).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        assert!(calls_of(&run, Stage::Exchange).is_empty());
        drop(shares);
    }

    #[tokio::test]
    async fn a_waiting_run_requires_input_from_the_user() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Exchange => Fake::Reply(format!(
                "```json\n{}\n```",
                json!({ "message": "Ok.", "plan": "P.", "implementer": "claude", "agreed": true, "conceded": [],
                    "unknowns": ["Which database?"], "backlog": [] })
            )),
            _ => happy(agent, stage, prompt),
        });
        let shares = Shares::default();
        let handle = run.handle.clone();
        let info = shares.start("s", "task-1", "01", run.dir.clone(), || Some(handle)).unwrap();
        until(&run, |s| s.state == RunState::Waiting).await;
        let (_, task) = Client::new(&info).get(&format!("/tasks/{}", info.task_id)).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
        assert!(task["status"]["message"]["parts"][0]["text"].as_str().unwrap().contains("- Which database?"));
        run.handle.control(crate::task_runs::Control::Cancel { reason: None }).unwrap();
        settled(&run).await;
        let (_, task) = Client::new(&info).get(&format!("/tasks/{}", info.task_id)).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");
    }

    #[tokio::test]
    async fn a_stop_revokes_the_share_and_releases_the_run() {
        let gate = Arc::new(Notify::new());
        let (run, shares, info) = shared(gate.clone());
        let handle = run.handle.clone();
        assert_eq!(shares.start("s", "task-1", "01", run.dir.clone(), || None).unwrap(), info, "a repeated start");
        assert_eq!(shares.get("s", "task-1", "01"), Some(info.clone()));
        assert_eq!(Client::new(&info).get(&format!("/tasks/{}", info.task_id)).await.0, StatusCode::OK);

        shares.stop("s", "task-1", "01");
        assert_eq!(shares.get("s", "task-1", "01"), None);
        let address = info.url.strip_prefix("http://").unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect(&address).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the listener closes");

        // A new share has a new credential: the old one does not open it.
        let again = shares.start("s", "task-1", "01", run.dir.clone(), || Some(handle.clone())).unwrap();
        assert_ne!(again.token, info.token);
        let stale = Client { info: &again, token: Some(&info.token), version: Some(VERSION) };
        assert_eq!(stale.get(&format!("/tasks/{}", again.task_id)).await.0, StatusCode::UNAUTHORIZED);

        // A removal of the task ends its shares, and no share keeps the run.
        shares.forget("s", Some("other-task"));
        assert!(shares.get("s", "task-1", "01").is_some());
        shares.forget("s", Some("task-1"));
        assert!(shares.get("s", "task-1", "01").is_none());
        gate.notify_one();
        settled(&run).await;
        drop(handle);
        tokio::time::timeout(Duration::from_secs(5), async {
            // The registry and the started run keep two references.
            while Arc::strong_count(&run.handle) > 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the share releases the run");
        assert!(matches!(shares.start("s", "task-1", "01", run.dir.clone(), || None), Err(Error::RunNotShareable)));
    }

    #[tokio::test]
    async fn the_old_task_stays_terminal_and_a_continuation_has_a_new_identity() {
        let run = start(|agent, stage, prompt| match stage {
            Stage::Implement => Fake::Hang,
            _ => happy(agent, stage, prompt),
        });
        let shares = Shares::default();
        let handle = run.handle.clone();
        let old = shares.start("s", "task-1", "01", run.dir.clone(), || Some(handle)).unwrap();
        until(&run, |s| s.stage.as_deref() == Some("implement")).await;
        run.handle.control(Control::Cancel { reason: None }).unwrap();
        settled(&run).await;

        let next = resume_full(&run, no_git, happy);
        settled(&next).await;
        let handle = next.handle.clone();
        let new = shares.start("s", "task-1", "02", next.dir.clone(), || Some(handle)).unwrap();
        assert_ne!(new.task_id, old.task_id);
        assert_ne!(new.token, old.token);
        let (_, task) = Client::new(&old).get(&format!("/tasks/{}", old.task_id)).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED", "the old task stays terminal");
        let (_, task) = Client::new(&new).get(&format!("/tasks/{}", new.task_id)).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        let stale = Client { info: &new, token: Some(&old.token), version: Some(VERSION) };
        assert_eq!(stale.get(&format!("/tasks/{}", new.task_id)).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(Client::new(&old).get(&format!("/tasks/{}", new.task_id)).await.0, StatusCode::NOT_FOUND);
    }
}
