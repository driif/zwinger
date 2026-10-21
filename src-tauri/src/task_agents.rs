//! One-shot agent calls for task runs: one process per stage call, with the
//! prompt on stdin and the reply at the end. The live output goes through the
//! agent mappers, so the room shows pi-shaped records.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::agent::{AgentKind, Framer, Line, claude, codex, gemini, launch};
use crate::runtime::{PipeEvent, PipeInput, SandboxRuntime};
use crate::shield::TaskAgent;

/// How long a killed process has to exit.
const KILL_WAIT: Duration = Duration::from_secs(10);
const STDERR_LINES: usize = 20;
/// The output of a gate or a diff that is kept: its end.
const OUTPUT_TAIL: usize = 32 << 10;
const DIFF_LIMIT: usize = 2 << 20;
const STATUS_LIMIT: usize = 64 << 10;
const GIT_LIMIT: Limits = Limits { idle: Duration::from_secs(120), hard: Duration::from_secs(300) };
/// A longer reply degrades the agent.
pub const REPLY_LIMIT: usize = 4 << 20;
/// The end of a reply that the JSON search reads.
const SCAN_LIMIT: usize = 256 << 10;
const PARSE_LIMIT: Duration = Duration::from_secs(5);
/// The tools of pi in a read-only stage.
const PI_READ_TOOLS: &str = "read,grep,find,ls";
/// The tree of a repository with no commit: the base of a run that started there.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The start of each git script. `$1` is the guest path of the run's own
/// artifact folder, the only path that the scripts skip, and `$2` the base
/// commit. The scripts see the whole repository, not only the working dir.
const GIT_PRELUDE: &str = r#"art=$1 base=$2
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 3
top=$(git rev-parse --show-toplevel) && cd "$top" || exit 4
[ -n "$art" ] && art=$(cd "$art" 2>/dev/null && pwd -P)
set -- .
case "$art" in "$top"/?*) set -- . ":(exclude,literal)${art#"$top"/}" ;; esac
"#;
const RETRY: &str = "\n\n---\n\nYour last reply had no valid JSON object. Do not do the work again. \
Return only the JSON object that the Output section asks for, in a fenced ```json block.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A process with no output for this long is killed.
    pub idle: Duration,
    pub hard: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Start(String),
    Exit {
        code: i32,
        stderr: Vec<String>,
    },
    Idle(Duration),
    Hard(Duration),
    Cancelled,
    /// The CLI reported an error, such as a failed model request.
    Agent(String),
    /// The reply had this many bytes, more than `REPLY_LIMIT`.
    TooLarge(usize),
}

impl Failure {
    /// The event kind of the failure, as duo names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Idle(_) => "agent_stalled",
            Self::Hard(_) => "agent_timeout",
            _ => "agent_degraded",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(e) => write!(f, "did not start: {e}"),
            Self::Exit { code, stderr } => {
                write!(f, "ended with code {code}")?;
                match stderr.last() {
                    Some(line) => write!(f, ": {line}"),
                    None => Ok(()),
                }
            }
            Self::Idle(d) => write!(f, "idle {}s", d.as_secs()),
            Self::Hard(d) => write!(f, "ran {}s", d.as_secs()),
            Self::Cancelled => write!(f, "was cancelled"),
            Self::Agent(e) => write!(f, "failed: {e}"),
            Self::TooLarge(n) => write!(f, "replied {n} bytes, more than the limit of {REPLY_LIMIT}"),
        }
    }
}

/// The argv of a one-shot call. Read-only stages pass the read-only flags
/// that the CLI has: Claude Code loses its edit tools and pi gets read tools
/// only. Gemini CLI without `--yolo` refuses the tools that need an approval.
/// Codex's own sandbox does not work in the VM, so for Codex read-only
/// is the instruction in the prompt and the check after the stage. `pi_host`
/// runs the host pi that the shield mounts. `gemini` is the guest path of
/// Gemini CLI's entry point, if the shield mounts it.
pub fn argv(agent: &TaskAgent, access: Access, pi_host: bool, gemini: Option<&str>) -> Result<Vec<String>, Failure> {
    let model = agent.model.as_deref();
    let Some(mut argv) = launch::command(agent.kind, launch::Found { pi_host, gemini }) else {
        return Err(Failure::Start("Gemini CLI is not mounted in the shield".to_owned()));
    };
    match agent.kind {
        AgentKind::Claude => {
            // `--dangerously-skip-permissions` as in the agent sessions: the shield is the sandbox.
            argv.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--include-partial-messages",
                    "--dangerously-skip-permissions",
                ]
                .map(str::to_owned),
            );
            if access == Access::Read {
                argv.extend(["--disallowedTools".to_owned(), "Edit,Write,NotebookEdit".to_owned()]);
            }
            if let Some(model) = model {
                argv.extend(["--model".to_owned(), model.to_owned()]);
            }
        }
        AgentKind::Codex => {
            argv.extend(
                ["exec", "--json", "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check"]
                    .map(str::to_owned),
            );
            if let Some(model) = model {
                argv.extend(["-m".to_owned(), model.to_owned()]);
            }
            argv.push("-".to_owned());
        }
        AgentKind::Gemini => {
            argv.extend(["--output-format".to_owned(), "stream-json".to_owned()]);
            // `--yolo` approves each tool call: acceptable only because the agent runs inside the shield.
            if access == Access::Write {
                argv.push("--yolo".to_owned());
            }
            if let Some(model) = model {
                argv.extend(["-m".to_owned(), model.to_owned()]);
            }
        }
        AgentKind::Pi => {
            argv.extend(["--no-session", "--mode", "json"].map(str::to_owned));
            if access == Access::Read {
                argv.extend(["--tools".to_owned(), PI_READ_TOOLS.to_owned()]);
            }
            if let Some(model) = model {
                argv.extend(["--model".to_owned(), model.to_owned()]);
            }
            // Last, so pi does not take the next argument as the message.
            argv.push("-p".to_owned());
        }
    }
    Ok(argv)
}

/// The end of `text`: at most `limit` bytes, cut at a char boundary.
pub fn tail(text: &str, limit: usize) -> &str {
    let mut cut = text.len().saturating_sub(limit);
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}

/// The start of `text`: at most `limit` bytes, cut at a char boundary.
pub fn head(text: &str, limit: usize) -> &str {
    let mut cut = limit.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// The last JSON object in the end of a reply: the last fenced block that
/// parses, else the last outermost balanced `{...}` that parses. As duo's
/// `jsonblock.py`, but in linear time.
pub fn json_block(text: &str) -> Option<Value> {
    let text = tail(text, SCAN_LIMIT);
    let object = |s: &str| serde_json::from_str::<Value>(s).ok().filter(Value::is_object);
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(newline) = after.find('\n') else {
            break;
        };
        let lang = after[..newline].trim();
        let body = &after[newline + 1..];
        let Some(end) = body.find("```") else {
            break;
        };
        if lang.is_empty() || lang == "json" {
            blocks.push(&body[..end]);
        }
        rest = &body[end + 3..];
    }
    if let Some(found) = blocks.iter().rev().find_map(|b| object(b)) {
        return Some(found);
    }
    // One pass: the open braces on a stack, JSON strings skipped. A closed
    // span takes the place of the spans inside it, so `spans` holds only
    // outermost spans and each byte is parsed at most once.
    let (mut open, mut spans) = (Vec::new(), Vec::<(usize, usize)>::new());
    let (mut in_string, mut escaped) = (false, false);
    for (i, &b) in text.as_bytes().iter().enumerate() {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' if !open.is_empty() => in_string = true,
            b'{' => open.push(i),
            b'}' => {
                if let Some(start) = open.pop() {
                    while spans.last().is_some_and(|&(s, _)| s > start) {
                        spans.pop();
                    }
                    spans.push((start, i));
                }
            }
            _ => {}
        }
    }
    spans.iter().rev().find_map(|&(start, end)| object(&text[start..=end]))
}

/// `json_block` off the async runtime, with a time limit.
async fn parse(raw: &str) -> Option<Value> {
    let text = tail(raw, SCAN_LIMIT).to_owned();
    let parsed = tokio::time::timeout(PARSE_LIMIT, tokio::task::spawn_blocking(move || json_block(&text))).await;
    parsed.ok()?.ok()?
}

/// What one call gave: the JSON, the raw text, and the failure if there was no JSON.
#[derive(Debug, Clone)]
pub struct Reply {
    pub json: Option<Value>,
    pub raw: String,
    pub retried: bool,
    pub failure: Option<Failure>,
}

/// The end of a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// `None` if it did not exit by itself.
    pub code: Option<i32>,
    pub output: String,
}

/// The state of a git repository: `HEAD`, the porcelain status and a hash
/// of both and of all changes, untracked files included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The commit, or `EMPTY_TREE` before the first commit.
    pub head: String,
    /// The line `HEAD <commit>`, then the porcelain status.
    pub status: String,
    pub hash: [u8; 32],
}

/// Why a snapshot has no state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The working dir is not in a git work tree, or git is missing.
    NotGit,
    Failed(String),
}

/// Gets each live record of the call.
pub type Live<'a> = &'a (dyn Fn(Value) + Send + Sync);

/// Runs processes in a shield's VM, in one working dir.
pub struct Exec<R: SandboxRuntime> {
    pub runtime: R,
    pub vm: Arc<R::Vm>,
    pub cwd: String,
    /// The guest path of the run's artifact folder, which git checks skip.
    pub artifacts: Option<String>,
    /// pi runs from the host install that the shield mounts.
    pub pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if the shield mounts it.
    pub gemini: Option<String>,
}

/// The final text of a call and the error that the CLI reported.
#[derive(Default)]
struct Result_ {
    text: String,
    error: Option<String>,
}

enum Mapper {
    Claude(claude::Mapper),
    Codex(codex::Mapper),
    Gemini(gemini::Mapper),
    Pi,
}

impl<R: SandboxRuntime> Exec<R> {
    /// Runs `argv` with `input` on stdin until it exits, is killed by a limit,
    /// or `cancel` becomes true. Each stdout chunk goes to `on_stdout`.
    async fn drive(
        &self,
        argv: &[String],
        input: &[u8],
        limits: Limits,
        cancel: &mut watch::Receiver<bool>,
        mut on_stdout: impl FnMut(&[u8]),
    ) -> Result<(i32, Vec<String>), Failure> {
        if *cancel.borrow() {
            return Err(Failure::Cancelled);
        }
        let mut pipe =
            self.runtime.open_pipe(&self.vm, argv, &self.cwd).await.map_err(|e| Failure::Start(e.to_string()))?;
        let _ = pipe.input.send(PipeInput::Data(input.to_vec())).await;
        let _ = pipe.input.send(PipeInput::Close).await;
        let hard = Instant::now() + limits.hard;
        let mut idle = Instant::now() + limits.idle;
        let (mut stderr, mut framer) = (VecDeque::new(), Framer::default());
        let failure = loop {
            tokio::select! {
                event = pipe.events.recv() => match event {
                    Some(PipeEvent::Stdout(bytes)) => {
                        idle = Instant::now() + limits.idle;
                        on_stdout(&bytes);
                    }
                    Some(PipeEvent::Stderr(bytes)) => {
                        idle = Instant::now() + limits.idle;
                        for line in framer.push(&bytes) {
                            if let Line::Record(line) = line {
                                if stderr.len() == STDERR_LINES {
                                    stderr.pop_front();
                                }
                                stderr.push_back(String::from_utf8_lossy(&line[..line.len().min(2048)]).into_owned());
                            }
                        }
                    }
                    Some(PipeEvent::Exit(code)) => return Ok((code, stderr.into())),
                    None => return Ok((-1, stderr.into())),
                },
                () = tokio::time::sleep_until(idle) => break Failure::Idle(limits.idle),
                () = tokio::time::sleep_until(hard) => break Failure::Hard(limits.hard),
                _ = cancel.wait_for(|c| *c) => break Failure::Cancelled,
            }
        };
        // The wrapper kills the process group. Wait for the exit, so no
        // process of this stage runs on into the next one.
        let _ = pipe.input.send(PipeInput::Kill).await;
        let _ = tokio::time::timeout(KILL_WAIT, async {
            while let Some(event) = pipe.events.recv().await {
                if matches!(event, PipeEvent::Exit(_)) {
                    break;
                }
            }
        })
        .await;
        Err(failure)
    }

    /// One agent call. Returns the final text of the agent.
    pub async fn agent(
        &self,
        agent: &TaskAgent,
        access: Access,
        prompt: &str,
        limits: Limits,
        cancel: &mut watch::Receiver<bool>,
        live: Live<'_>,
    ) -> Result<String, Failure> {
        let mut mapper = match agent.kind {
            AgentKind::Claude => Mapper::Claude(claude::Mapper::default()),
            AgentKind::Codex => Mapper::Codex(codex::Mapper::default()),
            AgentKind::Gemini => Mapper::Gemini(gemini::Mapper::default()),
            AgentKind::Pi => Mapper::Pi,
        };
        let mut framer = Framer::default();
        let mut result = Result_::default();
        let on_stdout = |bytes: &[u8]| {
            for line in framer.push(bytes) {
                let Line::Record(line) = line else {
                    continue;
                };
                let Ok(event) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let records = match &mut mapper {
                    Mapper::Claude(m) => {
                        claude_result(&event, &mut result);
                        m.map(&event)
                    }
                    Mapper::Codex(m) => {
                        codex_result(&event, &mut result);
                        m.map(&event)
                    }
                    Mapper::Gemini(m) => {
                        gemini_result(&event, &mut result);
                        m.map(&event)
                    }
                    Mapper::Pi => {
                        pi_result(&event, &mut result);
                        vec![event]
                    }
                };
                for record in records {
                    live(record);
                }
            }
        };
        let (code, stderr) = self
            .drive(
                &argv(agent, access, self.pi_host, self.gemini.as_deref())?,
                prompt.as_bytes(),
                limits,
                cancel,
                on_stdout,
            )
            .await?;
        if let Some(error) = result.error {
            return Err(Failure::Agent(error));
        }
        if code != 0 {
            return Err(Failure::Exit { code, stderr });
        }
        Ok(result.text)
    }

    /// One agent call that must end with a JSON object. A reply with no JSON
    /// gets one retry; a failed process gets none.
    pub async fn call_json(
        &self,
        agent: &TaskAgent,
        access: Access,
        prompt: &str,
        limits: Limits,
        cancel: &mut watch::Receiver<bool>,
        live: Live<'_>,
    ) -> Reply {
        let mut retried = false;
        let mut prompt = prompt.to_owned();
        loop {
            match self.agent(agent, access, &prompt, limits, cancel, live).await {
                Ok(raw) if raw.len() > REPLY_LIMIT => {
                    return Reply { json: None, failure: Some(Failure::TooLarge(raw.len())), raw, retried };
                }
                Ok(raw) => match parse(&raw).await {
                    Some(json) => {
                        return Reply { json: Some(json), raw, retried, failure: None };
                    }
                    None if !retried => {
                        retried = true;
                        prompt.push_str(RETRY);
                    }
                    None => {
                        return Reply {
                            json: None,
                            raw,
                            retried,
                            failure: Some(Failure::Agent("the reply had no JSON object".into())),
                        };
                    }
                },
                Err(failure) => {
                    return Reply { json: None, raw: String::new(), retried, failure: Some(failure) };
                }
            }
        }
    }

    /// Runs one shell line with `/bin/sh -lc`. Keeps the end of its output,
    /// stdout and stderr together.
    pub async fn sh(&self, script: &str, limits: Limits, cancel: &mut watch::Receiver<bool>) -> Result<Shell, Failure> {
        // Stderr goes to stdout, so the output keeps its order.
        let argv = ["/bin/sh".to_owned(), "-lc".to_owned(), format!("{{ {script}\n}} 2>&1")];
        let mut output = VecDeque::new();
        let ran = self
            .drive(&argv, b"", limits, cancel, |bytes| {
                output.extend(bytes);
                let excess = output.len().saturating_sub(OUTPUT_TAIL);
                output.drain(..excess);
            })
            .await;
        let output = String::from_utf8_lossy(&output.into_iter().collect::<Vec<u8>>()).into_owned();
        match ran {
            Ok((code, _)) => Ok(Shell { code: Some(code), output }),
            Err(Failure::Cancelled) => Err(Failure::Cancelled),
            Err(_) => Ok(Shell { code: None, output }),
        }
    }

    fn git_argv(&self, script: String, base: Option<&str>) -> [String; 6] {
        [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{GIT_PRELUDE}{script}"),
            "zwinger".to_owned(),
            self.artifacts.clone().unwrap_or_default(),
            base.unwrap_or_default().to_owned(),
        ]
    }

    /// The git state of the repository of the working dir.
    pub async fn snapshot(&self, cancel: &mut watch::Receiver<bool>) -> Result<Snapshot, SnapshotError> {
        let script = format!(
            "printf 'HEAD %s\\n' \"$(git rev-parse -q --verify HEAD || echo {EMPTY_TREE})\"\n\
             git status --porcelain=v1 -uall -- \"$@\" || exit 4\n\
             printf '\\0'\n\
             git diff --binary -- \"$@\" || exit 4\n\
             git diff --cached --binary -- \"$@\" || exit 4\n\
             git ls-files -o --exclude-standard -z -- \"$@\" | xargs -0 -r cat --"
        );
        let (mut status, mut hash, mut in_status) = (Vec::new(), Sha256::new(), true);
        let ran = self
            .drive(&self.git_argv(script, None), b"", GIT_LIMIT, cancel, |mut bytes| {
                if in_status {
                    match bytes.iter().position(|&b| b == 0) {
                        Some(i) => {
                            status.extend_from_slice(&bytes[..i]);
                            bytes = &bytes[i + 1..];
                            in_status = false;
                        }
                        None => {
                            status.extend_from_slice(bytes);
                            return;
                        }
                    }
                }
                hash.update(bytes);
            })
            .await;
        match ran {
            Ok((3, _)) => Err(SnapshotError::NotGit),
            Ok((0, _)) if !in_status => {
                hash.update(&status);
                status.truncate(STATUS_LIMIT);
                let status = String::from_utf8_lossy(&status).into_owned();
                let head = status.lines().next().and_then(|l| l.strip_prefix("HEAD ")).filter(|h| is_commit(h));
                let Some(head) = head.map(str::to_owned) else {
                    return Err(SnapshotError::Failed("git printed no HEAD".to_owned()));
                };
                Ok(Snapshot { head, status, hash: hash.finalize().into() })
            }
            Ok((code, _)) => Err(SnapshotError::Failed(format!("git ended with code {code}"))),
            Err(failure) => Err(SnapshotError::Failed(format!("git {failure}"))),
        }
    }

    /// The change since `base`: the new commits, the status, the diff from
    /// `base` and the new files as diffs from nothing. With no base: the
    /// uncommitted change. `None` if it is not a git work tree.
    pub async fn diff(&self, base: Option<&str>, cancel: &mut watch::Receiver<bool>) -> Option<String> {
        let base = base.filter(|b| is_commit(b));
        let script = format!(
            "q='-c core.quotepath=off'\n\
             if [ -n \"$base\" ]; then\n\
               echo 'Commits since the start of the run:'\n\
               if [ \"$base\" = {EMPTY_TREE} ]; then git $q log --oneline HEAD 2>/dev/null; else git $q log --oneline \"$base..HEAD\"; fi\n\
               echo\n\
             fi\n\
             git $q status --short -uall -- \"$@\"\n\
             echo\n\
             if [ -n \"$base\" ]; then\n\
               git $q diff --no-color \"$base\" -- \"$@\"\n\
             else\n\
               git $q diff --no-color -- \"$@\"\n\
               git $q diff --cached --no-color -- \"$@\"\n\
             fi\n\
             git ls-files -o --exclude-standard -z -- \"$@\" | xargs -0 -r -n1 git $q diff --no-color --no-index -- /dev/null\n\
             exit 0"
        );
        let mut out = Vec::new();
        let ran = self
            .drive(&self.git_argv(script, base), b"", GIT_LIMIT, cancel, |bytes| {
                let room = DIFF_LIMIT.saturating_sub(out.len());
                out.extend_from_slice(&bytes[..bytes.len().min(room)]);
            })
            .await;
        match ran {
            Ok((0, _)) => Some(String::from_utf8_lossy(&out).into_owned()),
            _ => None,
        }
    }
}

/// A full commit or tree id, so it is safe in a git command line.
fn is_commit(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The `result` event ends a Claude Code call.
fn claude_result(event: &Value, result: &mut Result_) {
    if event["type"] != "result" {
        return;
    }
    let text = event["result"].as_str().unwrap_or_default().to_owned();
    if event["is_error"].as_bool() == Some(true) {
        result.error = Some(if text.is_empty() { text_or(&event["subtype"], "Claude Code failed") } else { text });
    } else {
        result.text = text;
    }
}

/// The last agent message of a Codex turn is its reply.
fn codex_result(event: &Value, result: &mut Result_) {
    match event["type"].as_str() {
        Some("item.completed") if event["item"]["type"] == "agent_message" => {
            result.text = event["item"]["text"].as_str().unwrap_or_default().to_owned();
        }
        Some("turn.failed") => result.error = Some(text_or(&event["error"]["message"], "Codex failed")),
        _ => {}
    }
}

/// The assistant text after the last tool result of a Gemini CLI run is its
/// reply: Gemini CLI streams it in deltas.
fn gemini_result(event: &Value, result: &mut Result_) {
    match event["type"].as_str() {
        Some("tool_result") => result.text.clear(),
        Some("message") if event["role"] == "assistant" => {
            if event["delta"] != true {
                result.text.clear();
            }
            result.text.push_str(event["content"].as_str().unwrap_or_default());
        }
        Some("result") if event["status"] != "success" => {
            result.error = Some(text_or(&event["error"]["message"], "Gemini CLI failed"));
        }
        _ => {}
    }
}

/// The last assistant message of a pi run is its reply.
fn pi_result(event: &Value, result: &mut Result_) {
    let message = &event["message"];
    if event["type"] != "message_end" || message["role"] != "assistant" {
        return;
    }
    if message["stopReason"] == "error" {
        result.error = Some(text_or(&message["errorMessage"], "pi failed"));
        return;
    }
    let blocks = message["content"].as_array().map(Vec::as_slice).unwrap_or_default();
    let text: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    if !text.is_empty() {
        result.text = text.join("");
    }
}

fn text_or(value: &Value, default: &str) -> String {
    value.as_str().filter(|s| !s.is_empty()).unwrap_or(default).to_owned()
}

/// The stdout line of a Claude Code `result` event, for fakes.
#[cfg(test)]
pub fn claude_result_line(text: &str) -> Vec<u8> {
    let mut line = serde_json::json!({ "type": "result", "subtype": "success", "is_error": false, "result": text })
        .to_string()
        .into_bytes();
    line.push(b'\n');
    line
}

/// The stdout lines of a Codex turn with one agent message, for fakes.
#[cfg(test)]
pub fn codex_result_lines(text: &str) -> Vec<u8> {
    let message = serde_json::json!({ "type": "item.completed", "item": { "id": "item_0", "type": "agent_message", "text": text } });
    format!("{message}\n{}\n", serde_json::json!({ "type": "turn.completed" })).into_bytes()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn agent(kind: AgentKind, model: Option<&str>) -> TaskAgent {
        TaskAgent { kind, model: model.map(str::to_owned) }
    }

    const GEMINI: Option<&str> = Some("/opt/zwinger/host/gemini/bundle/gemini.js");

    #[test]
    fn read_only_stages_pass_the_read_only_flags() {
        let claude = argv(&agent(AgentKind::Claude, Some("opus")), Access::Read, false, GEMINI).unwrap();
        assert!(claude.windows(2).any(|w| w == ["--disallowedTools", "Edit,Write,NotebookEdit"]));
        assert!(claude.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(
            !argv(&agent(AgentKind::Claude, None), Access::Write, false, GEMINI)
                .unwrap()
                .contains(&"--disallowedTools".to_owned())
        );

        let pi = argv(&agent(AgentKind::Pi, None), Access::Read, false, GEMINI).unwrap();
        assert!(pi.windows(2).any(|w| w == ["--tools", PI_READ_TOOLS]));
        assert_eq!(pi.last().map(String::as_str), Some("-p"));
        assert!(
            !argv(&agent(AgentKind::Pi, None), Access::Write, false, GEMINI).unwrap().contains(&"--tools".to_owned())
        );

        let codex = argv(&agent(AgentKind::Codex, Some("gpt-5")), Access::Read, false, GEMINI).unwrap();
        assert_eq!(codex, argv(&agent(AgentKind::Codex, Some("gpt-5")), Access::Write, false, GEMINI).unwrap());
        assert!(codex.windows(2).any(|w| w == ["-m", "gpt-5"]));
        assert_eq!(codex.last().map(String::as_str), Some("-"));

        let gemini = argv(&agent(AgentKind::Gemini, None), Access::Read, false, GEMINI).unwrap();
        assert!(!gemini.contains(&"--yolo".to_owned()));
        assert!(
            argv(&agent(AgentKind::Gemini, None), Access::Write, false, GEMINI).unwrap().contains(&"--yolo".to_owned())
        );
        assert_eq!(gemini[3], "/opt/zwinger/host/gemini/bundle/gemini.js");
        assert!(matches!(argv(&agent(AgentKind::Gemini, None), Access::Write, false, None), Err(Failure::Start(_))));

        let host_pi = argv(&agent(AgentKind::Pi, None), Access::Write, true, GEMINI).unwrap();
        assert_eq!(host_pi[2..4], ["node", "/opt/zwinger/host/pi/dist/cli.js"]);
        assert_eq!(pi[..4], ["env", "PI_CODING_AGENT_DIR=/opt/zwinger/state/agent", "node", "/opt/zwinger/pi/cli.js"]);
    }

    #[test]
    fn the_gemini_reply_is_the_text_after_the_last_tool_result() {
        let mut result = Result_::default();
        for line in [
            r#"{"type":"message","role":"assistant","content":"I will look.","delta":true}"#,
            r#"{"type":"tool_result","tool_id":"t","status":"success","output":"a"}"#,
            r#"{"type":"message","role":"assistant","content":"```json\n{\"a\":","delta":true}"#,
            r#"{"type":"message","role":"assistant","content":"1}\n```","delta":true}"#,
            r#"{"type":"result","status":"success"}"#,
        ] {
            gemini_result(&serde_json::from_str(line).unwrap(), &mut result);
        }
        assert_eq!((result.text.as_str(), result.error.as_deref()), ("```json\n{\"a\":1}\n```", None));
        gemini_result(&json!({ "type": "result", "status": "error", "error": { "message": "429" } }), &mut result);
        assert_eq!(result.error.as_deref(), Some("429"));
    }

    #[test]
    fn json_block_takes_the_last_object() {
        assert_eq!(json_block("x\n```json\n{\"a\":1}\n```\ny\n```json\n{\"a\":2}\n```"), Some(json!({ "a": 2 })));
        // A broken last block falls back to the one before.
        assert_eq!(json_block("```json\n{\"a\":1}\n```\n```json\n{\"a\":\n```"), Some(json!({ "a": 1 })));
        assert_eq!(
            json_block("Done. {\"ship\": true, \"x\": {\"y\": 1}} bye"),
            Some(json!({ "ship": true, "x": { "y": 1 } }))
        );
        assert_eq!(json_block("```json\n[1, 2]\n```"), None);
        assert_eq!(json_block("no json here"), None);
        // Braces in strings, and an unclosed brace before the object.
        assert_eq!(json_block("Done. {\"a\": \"}{\"}"), Some(json!({ "a": "}{" })));
        assert_eq!(json_block("Use {braces. Then {\"ship\": true}"), Some(json!({ "ship": true })));
    }

    #[test]
    fn json_block_is_fast_on_adversarial_input() {
        let started = std::time::Instant::now();
        assert_eq!(json_block(&"{".repeat(4 << 20)), None);
        assert_eq!(json_block(&"{\"a\":".repeat(1 << 19)), None);
        assert_eq!(json_block(&"{x}".repeat(2 << 20)), None);
        assert_eq!(json_block(&format!("{}{}", "{".repeat(2 << 20), "}".repeat(2 << 20))), None);
        let big = format!("{}{{\"ok\": 1}}", "x".repeat(8 << 20));
        assert_eq!(json_block(&big), Some(json!({ "ok": 1 })));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn results_of_each_cli() {
        let mut r = Result_::default();
        claude_result(&serde_json::from_slice(&claude_result_line("hi")).unwrap(), &mut r);
        assert_eq!((r.text.as_str(), r.error.as_deref()), ("hi", None));
        claude_result(
            &json!({ "type": "result", "is_error": true, "result": "", "subtype": "error_max_turns" }),
            &mut r,
        );
        assert_eq!(r.error.as_deref(), Some("error_max_turns"));

        let mut r = Result_::default();
        for line in String::from_utf8(codex_result_lines("done")).unwrap().lines() {
            codex_result(&serde_json::from_str(line).unwrap(), &mut r);
        }
        assert_eq!(r.text, "done");

        let mut r = Result_::default();
        pi_result(
            &json!({ "type": "message_end", "message": { "role": "assistant", "content": [{ "type": "text", "text": "ok" }] } }),
            &mut r,
        );
        assert_eq!(r.text, "ok");
        pi_result(
            &json!({ "type": "message_end", "message": { "role": "assistant", "content": [], "stopReason": "error", "errorMessage": "401" } }),
            &mut r,
        );
        assert_eq!(r.error.as_deref(), Some("401"));
    }
}
