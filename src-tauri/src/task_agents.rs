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
use crate::task_participants::Participant;

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
/// The start of each git script. `$1` is the folder that the check covers, `$2` is
/// the run copy to skip, and `$3` is the base commit.
pub(crate) const GIT_PRELUDE: &str = r#"scope=$1 art=$2 base=$3
cd "$scope" || exit 4
scope=$(pwd -P) || exit 4
inside=$(LC_ALL=C git rev-parse --is-inside-work-tree 2>&1) || {
  case "$inside" in
    "fatal: not a git repository (or any of the parent directories): .git"|"fatal: not a git repository (or any parent up to mount point "*) exit 3 ;;
  esac
  printf '%s\n' "$inside" >&2
  exit 4
}
case "$inside" in true) ;; false) exit 3 ;; *) exit 4 ;; esac
top=$(git rev-parse --show-toplevel) && cd "$top" || exit 4
[ -n "$art" ] && art=$(cd "$art" 2>/dev/null && pwd -P)
case "$scope" in
  "$top") set -- . ;;
  "$top"/*) set -- ":(literal)${scope#"$top"/}" ;;
  *) exit 4 ;;
esac
case "$art" in "$top"/?*) set -- "$@" ":(exclude,literal)${art#"$top"/}" ;; esac
"#;
/// The diff script after `GIT_PRELUDE`. It ends with code 4 when a git command
/// fails. `diff --no-index` ends with 1 when the files differ, so that is not a failure.
const DIFF_SCRIPT: &str = r#"q='-c core.quotepath=off'
if [ -n "$base" ]; then
  echo 'Commits since the start of the run:'
  empty=$(git hash-object -t tree /dev/null) || exit 4
  if [ "$base" = "$empty" ]; then
    if git rev-parse -q --verify HEAD >/dev/null; then
      git $q log --oneline HEAD -- "$@" || exit 4
    else
      [ "$?" -eq 1 ] || exit 4
    fi
  else
    git $q log --oneline "$base..HEAD" -- "$@" || exit 4
  fi
  echo
fi
git $q status --short -uall -- "$@" || exit 4
echo
if [ -n "$base" ]; then
  git $q diff --no-color "$base" -- "$@" || exit 4
else
  git $q diff --no-color -- "$@" || exit 4
  git $q diff --cached --no-color -- "$@" || exit 4
fi
tmp=$(mktemp -d /tmp/zwinger-diff.XXXXXX) || exit 4
trap 'rm -rf "$tmp"' EXIT
# Keep the list NUL-delimited and check its producer before consuming it.
git ls-files -o --exclude-standard -z -- "$@" > "$tmp/paths" || exit 4
xargs -0 -r -n1 sh -c '
  LC_ALL=C git -c core.quotepath=off diff --no-color --no-index -- /dev/null "$2" 2>"$1"
  [ "$?" -le 1 ] || exit 1
  # A vanished path also gives exit 1, with an error. Conversion warnings are harmless.
  grep -Eq "^(error|fatal):" "$1"
  [ "$?" -eq 1 ]
' _ "$tmp/error" < "$tmp/paths" || exit 4
"#;
/// The snapshot script after `GIT_PRELUDE`: the HEAD, or the empty tree
/// before the first commit, and the status, a NUL, then the bytes that the
/// fence hashes. The NUL-framed list of untracked paths is followed by one
/// content id per path in the same order, so a move of content between
/// files changes the hash. It ends with code 4 when a git command fails or a path
/// is gone. `--no-filters` keeps clean filters out of the content ids.
const SNAPSHOT_SCRIPT: &str = r#"printf 'HEAD %s\n' "$(git rev-parse -q --verify HEAD || echo 4b825dc642cb6eb9a060e54bf8d69288fbee4904)"
git status --porcelain=v1 -uall -- "$@" || exit 4
printf '\0'
git diff --binary -- "$@" || exit 4
git diff --cached --binary -- "$@" || exit 4
tmp=$(mktemp -d "${TMPDIR:-/tmp}/zwinger-snapshot.XXXXXX") || exit 4
trap 'rm -rf "$tmp"' EXIT
git ls-files -o --exclude-standard -z -- "$@" > "$tmp/paths" || exit 4
cat -- "$tmp/paths" || exit 4
# A link has the id of its exact target bytes, as git stores it. The `x`
# keeps the trailing newlines of the target from the command substitution. Any other path that is not a
# file, such as a nested repository, has no content id and fails the snapshot.
xargs -0 -r sh -c '
  for path; do
    if [ -L "$path" ]; then
      target=$(readlink -n -- "$path" && printf x) || exit 1
      printf %s "${target%x}" | git hash-object --stdin || exit 1
    elif [ -f "$path" ]; then
      git hash-object --no-filters -- "$path" || exit 1
    else
      exit 1
    fi
  done
' _ < "$tmp/paths" || exit 4
git ls-files -s -- "$@" || exit 4
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
/// is the instruction in the prompt and the check after the stage. `found`
/// picks the program and sets the proxy env. `home` is the state root of the call.
pub fn argv(
    agent: &Participant,
    access: Access,
    found: launch::Found,
    home: Option<&str>,
) -> Result<Vec<String>, Failure> {
    let model = agent.model.as_deref();
    let Some(mut argv) = launch::command(agent.kind, found, home) else {
        return Err(Failure::Start("Gemini CLI is not mounted in the cage".to_owned()));
    };
    match agent.kind {
        AgentKind::Claude => {
            // `--dangerously-skip-permissions` as in the agent sessions: the cage is the sandbox.
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
            // `--yolo` approves each tool call: acceptable only because the agent runs inside the cage.
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
    /// The commit, or the empty tree before the first commit.
    pub head: String,
    /// The line `HEAD <commit>`, then the porcelain status.
    pub status: String,
    pub hash: [u8; 32],
}

/// The end of an app git script.
#[derive(Debug, Clone)]
pub struct ScriptOut {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<String>,
    /// The stdout was longer than its limit.
    pub truncated: bool,
}

/// Why a snapshot has no state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The working dir is not in a git work tree.
    NotGit,
    Failed(String),
}

/// Gets each live record of the call.
pub type Live<'a> = &'a (dyn Fn(Value) + Send + Sync);

/// Runs processes in a cage, in one working dir.
pub struct Exec<R: SandboxRuntime> {
    pub runtime: R,
    pub vm: Arc<R::Vm>,
    pub cwd: String,
    /// The folder that the git check and the diff cover: the session's guest
    /// folder when `cwd` is in it, else `cwd` in its mount.
    pub scope: String,
    /// The guest path of the run's artifact folder, which git checks skip.
    pub artifacts: Option<String>,
    /// pi runs from the host install that the cage mounts.
    pub pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if the cage mounts it.
    pub gemini: Option<String>,
    /// The URL of the cage's proxy.
    pub proxy: Option<String>,
    /// The guest folder of the run's agent state: each participant has its
    /// own state root below it. `None` uses the state of the terminals.
    pub state: Option<String>,
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
    fn found(&self) -> launch::Found<'_> {
        launch::Found { pi_host: self.pi_host, gemini: self.gemini.as_deref(), proxy: self.proxy.as_deref() }
    }

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
        agent: &Participant,
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
        let home = self.state.as_ref().map(|state| format!("{state}/{}", agent.id));
        let argv = argv(agent, access, self.found(), home.as_deref())?;
        let (code, stderr) = self.drive(&argv, prompt.as_bytes(), limits, cancel, on_stdout).await?;
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
        agent: &Participant,
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

    /// `exclude` is the folder that git skips, below the work tree.
    fn git_argv(&self, script: String, exclude: Option<&str>, base: Option<&str>) -> [String; 7] {
        [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{GIT_PRELUDE}{script}"),
            "zwinger".to_owned(),
            self.scope.clone(),
            exclude.unwrap_or_default().to_owned(),
            base.unwrap_or_default().to_owned(),
        ]
    }

    /// Runs one fixed app script after `GIT_PRELUDE`, with `args` as `$a1`,
    /// `$a2` and so on, and `input` on stdin. Keeps at most `limit` bytes of
    /// stdout. With `net`, the script reaches the network through the cage's
    /// proxy. No caller passes a script that the user or an agent wrote.
    pub async fn git_script(
        &self,
        script: &str,
        args: &[&str],
        input: &[u8],
        limit: usize,
        net: bool,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<ScriptOut, Failure> {
        let mut head = String::new();
        if net && let Some(proxy) = &self.proxy {
            for (name, value) in launch::net_env(proxy) {
                head.push_str(&format!("export {name}='{}'\n", value.replace('\'', "'\\''")));
            }
        }
        for i in 1..=args.len() {
            head.push_str(&format!("a{i}=${{{}}}\n", i + 3));
        }
        let mut argv = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("{head}{GIT_PRELUDE}{script}"),
            "zwinger".to_owned(),
            self.scope.clone(),
            self.task_runs().unwrap_or_default().to_owned(),
            String::new(),
        ];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let (mut stdout, mut truncated) = (Vec::new(), false);
        let (code, stderr) = self
            .drive(&argv, input, GIT_LIMIT, cancel, |bytes| {
                let room = limit.saturating_sub(stdout.len());
                truncated |= bytes.len() > room;
                stdout.extend_from_slice(&bytes[..bytes.len().min(room)]);
            })
            .await?;
        Ok(ScriptOut { code, stdout, stderr, truncated })
    }

    /// The task folder: it holds the round copies of all runs of the task.
    fn task_runs(&self) -> Option<&str> {
        self.artifacts.as_deref().and_then(|a| a.rsplit_once('/')).map(|(task, _)| task)
    }

    /// The git state of the session folder, without the round copies of
    /// all runs of the task, so a continuation compares the same files.
    pub async fn task_snapshot(&self, cancel: &mut watch::Receiver<bool>) -> Result<Snapshot, SnapshotError> {
        self.snapshot_without(self.task_runs(), cancel).await
    }

    async fn snapshot_without(
        &self,
        exclude: Option<&str>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Snapshot, SnapshotError> {
        let (mut status, mut hash, mut in_status) = (Vec::new(), Sha256::new(), true);
        let ran = self
            .drive(&self.git_argv(SNAPSHOT_SCRIPT.to_owned(), exclude, None), b"", GIT_LIMIT, cancel, |mut bytes| {
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
                if let Some(line) = status.iter().position(|&b| b == b'\n') {
                    hash.update(&status[line + 1..]);
                }
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
    /// uncommitted change. `NotGit` if it is not a git work tree.
    pub async fn diff(&self, base: Option<&str>, cancel: &mut watch::Receiver<bool>) -> Result<String, SnapshotError> {
        let base = base.filter(|b| is_commit(b));
        let mut out = Vec::new();
        let ran = self
            .drive(&self.git_argv(DIFF_SCRIPT.to_owned(), self.task_runs(), base), b"", GIT_LIMIT, cancel, |bytes| {
                let room = DIFF_LIMIT.saturating_sub(out.len());
                out.extend_from_slice(&bytes[..bytes.len().min(room)]);
            })
            .await;
        match ran {
            Ok((0, _)) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Ok((3, _)) => Err(SnapshotError::NotGit),
            Ok((code, _)) => Err(SnapshotError::Failed(format!("git ended with code {code}"))),
            Err(failure) => Err(SnapshotError::Failed(format!("git {failure}"))),
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

/// The stdout line of a pi assistant `message_end` with one text block, for fakes.
#[cfg(test)]
pub fn pi_result_line(text: &str) -> Vec<u8> {
    let message = serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": text }] });
    let mut line = serde_json::json!({ "type": "message_end", "message": message }).to_string().into_bytes();
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

    /// The tree of a repository with no commit: the base of a run that started there.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn agent(kind: AgentKind, model: Option<&str>) -> Participant {
        Participant { id: kind.name().to_owned(), kind, model: model.map(str::to_owned) }
    }

    const GEMINI: Option<&str> = Some("/opt/zwinger/host/gemini/bundle/gemini.js");

    #[test]
    fn git_scope_excludes_other_sessions_and_the_round_copy() {
        let root = std::env::temp_dir().join(format!("zwinger-git-scope-{}", uuid::Uuid::new_v4()));
        let session = root.join("session");
        let copy = session.join(".zwinger/tasks/fix/01");
        let workdir = std::env::temp_dir().join(format!("zwinger-other-workdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(root.join("outside.txt"), "outside").unwrap();
        std::fs::write(session.join("code.txt"), "code").unwrap();
        std::fs::write(copy.join("events.jsonl"), "round").unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).arg(&root).status().unwrap().success());
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{GIT_PRELUDE}git status --porcelain=v1 -uall -- \"$@\""))
            .arg("zwinger")
            .arg(&session)
            .arg(&copy)
            .arg("")
            .current_dir(&workdir)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "?? session/code.txt\n");
    }

    #[test]
    fn git_scope_excludes_every_round_copy_of_the_task() {
        let exec = Exec {
            runtime: crate::runtime::fake::FakeRuntime::default(),
            vm: Arc::new(crate::runtime::fake::FakeVm { key: "s".into() }),
            cwd: "/s".into(),
            scope: "/s".into(),
            artifacts: Some("/s/.zwinger/tasks/fix/02".into()),
            pi_host: false,
            gemini: None,
            proxy: None,
            state: None,
        };
        assert_eq!(exec.task_runs(), Some("/s/.zwinger/tasks/fix"));
        let repo = temp_repo("task-runs");
        for run in ["01", "02"] {
            let copy = repo.join(".zwinger/tasks/fix").join(run);
            std::fs::create_dir_all(&copy).unwrap();
            std::fs::write(copy.join("events.jsonl"), "round").unwrap();
        }
        std::fs::write(repo.join("code.txt"), "code").unwrap();
        let task = repo.join(".zwinger/tasks/fix");
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{GIT_PRELUDE}git status --porcelain=v1 -uall -- \"$@\""))
            .arg("zwinger")
            .arg(&repo)
            .arg(&task)
            .arg("")
            .output()
            .unwrap();
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
        assert_eq!(String::from_utf8(status.stdout).unwrap(), "?? code.txt\n");
        let diff = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{GIT_PRELUDE}{DIFF_SCRIPT}"))
            .arg("zwinger")
            .arg(&repo)
            .arg(&task)
            .arg("")
            .output()
            .unwrap();
        assert!(diff.status.success(), "{}", String::from_utf8_lossy(&diff.stderr));
        let diff = String::from_utf8(diff.stdout).unwrap();
        assert!(diff.contains("code.txt"), "{diff}");
        assert!(!diff.contains(".zwinger"), "{diff}");
    }

    fn diff_command(repo: &std::path::Path, base: &str) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(format!("{GIT_PRELUDE}{DIFF_SCRIPT}")).arg("zwinger").arg(repo).arg("").arg(base);
        command
    }

    fn temp_repo(name: &str) -> std::path::PathBuf {
        let repo = std::env::temp_dir().join(format!("zwinger-{name}-{}", uuid::Uuid::new_v4()));
        git_init(&repo);
        repo
    }

    fn git_init(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).arg(path).status().unwrap().success());
    }

    fn snapshot_command(repo: &std::path::Path) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(format!("{GIT_PRELUDE}{SNAPSHOT_SCRIPT}")).arg("zwinger").arg(repo).arg("").arg("");
        command
    }

    /// The SHA-256 of the bytes after the NUL, which the fence hashes.
    fn untracked_digest(repo: &std::path::Path) -> [u8; 32] {
        let output = snapshot_command(repo).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let nul = output.stdout.iter().position(|&b| b == 0).unwrap();
        Sha256::digest(&output.stdout[nul + 1..]).into()
    }

    #[test]
    fn the_snapshot_binds_each_untracked_name_to_its_content() {
        assert!(SNAPSHOT_SCRIPT.contains(EMPTY_TREE));
        let repo = temp_repo("snapshot-untracked");
        std::fs::write(repo.join("a"), "X\nY\n").unwrap();
        std::fs::write(repo.join("b"), "Z\n").unwrap();
        let before = untracked_digest(&repo);
        // The same bytes in the same order, but one line moved to the other file.
        std::fs::write(repo.join("a"), "X\n").unwrap();
        std::fs::write(repo.join("b"), "Y\nZ\n").unwrap();
        assert_ne!(untracked_digest(&repo), before);

        // A dangling link is content: its exact target bytes, with trailing newlines.
        std::os::unix::fs::symlink("target", repo.join("link")).unwrap();
        let with_link = untracked_digest(&repo);
        std::fs::remove_file(repo.join("link")).unwrap();
        std::os::unix::fs::symlink("target\n", repo.join("link")).unwrap();
        let with_newline = untracked_digest(&repo);
        assert_ne!(with_newline, with_link);
        std::fs::remove_file(repo.join("link")).unwrap();
        std::os::unix::fs::symlink("target\n\n", repo.join("link")).unwrap();
        assert_ne!(untracked_digest(&repo), with_newline);
        std::fs::remove_file(repo.join("link")).unwrap();
        std::os::unix::fs::symlink("target", repo.join("link")).unwrap();
        assert_eq!(untracked_digest(&repo), with_link);
        // The id is the one git stores for the link.
        let output = snapshot_command(&repo).output().unwrap();
        let blob = std::process::Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(b"target")?;
                child.wait_with_output()
            })
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains(String::from_utf8_lossy(&blob.stdout).trim()));
    }

    #[test]
    fn the_snapshot_fails_when_the_listing_or_a_hash_fails() {
        use std::os::unix::fs::PermissionsExt;

        let repo = temp_repo("snapshot-fail");
        std::fs::write(repo.join("a"), "a").unwrap();
        let bin = repo.join(".git/bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = bin.join("git");
        std::fs::write(
            &shim,
            r#"#!/bin/sh
if [ "$1" = ls-files ]; then
  case " $* " in *" -o "*) exit 1 ;; esac
fi
PATH=$ORIGINAL_PATH exec git "$@"
"#,
        )
        .unwrap();
        std::fs::set_permissions(shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = std::env::var_os("PATH").unwrap();
        let path = std::env::join_paths([bin].into_iter().chain(std::env::split_paths(&original))).unwrap();
        let output = snapshot_command(&repo).env("PATH", &path).env("ORIGINAL_PATH", &original).output().unwrap();
        assert_eq!(output.status.code(), Some(4), "{}", String::from_utf8_lossy(&output.stderr));

        // A nested repository has no content id, so it gives no hash.
        git_init(&repo.join("nested"));
        assert_eq!(snapshot_command(&repo).output().unwrap().status.code(), Some(4));
        std::fs::remove_dir_all(repo.join("nested")).unwrap();
        assert!(snapshot_command(&repo).output().unwrap().status.success());

        // An untracked file that git cannot read gives no hash.
        std::fs::set_permissions(repo.join("a"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(repo.join("a")).is_err() {
            assert_eq!(snapshot_command(&repo).output().unwrap().status.code(), Some(4));
        }
        std::fs::set_permissions(repo.join("a"), std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn the_diff_fails_when_git_fails() {
        let repo = temp_repo("diff-missing-base");
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        let output = diff_command(&repo, &"1".repeat(40)).output().unwrap();
        assert_eq!(output.status.code(), Some(4), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn git_discovery_distinguishes_no_work_tree_from_operational_errors() {
        let repo = temp_repo("discovery");
        assert_eq!(diff_command(&repo.join("missing"), "").output().unwrap().status.code(), Some(4));
        assert_eq!(diff_command(&repo, "").env("PATH", repo.join("missing")).output().unwrap().status.code(), Some(4));
        std::fs::write(repo.join(".git/config"), "[broken").unwrap();
        assert_eq!(diff_command(&repo, "").output().unwrap().status.code(), Some(4));
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(
            diff_command(&repo, "")
                .env("GIT_CEILING_DIRECTORIES", repo.parent().unwrap())
                .output()
                .unwrap()
                .status
                .code(),
            Some(3)
        );
    }

    #[test]
    fn the_diff_rejects_a_failed_listing_or_a_vanished_untracked_path() {
        use std::os::unix::fs::PermissionsExt;

        let repo = temp_repo("diff-list");
        let bin = repo.join(".git/bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = bin.join("git");
        std::fs::write(
            &shim,
            r#"#!/bin/sh
if [ "$1" = ls-files ]; then
  echo listed >> .git/list-calls
  case " $* " in
    *" -z "*)
      PATH=$ORIGINAL_PATH git "$@" || exit 2
      if [ "$MODE" = vanish ]; then rm vanished; else exit 1; fi
      exit 0 ;;
  esac
fi
PATH=$ORIGINAL_PATH exec git "$@"
"#,
        )
        .unwrap();
        std::fs::set_permissions(shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(repo.join("present"), "present").unwrap();
        let original = std::env::var_os("PATH").unwrap();
        let path = std::env::join_paths([bin].into_iter().chain(std::env::split_paths(&original))).unwrap();
        for mode in ["fail", "vanish"] {
            std::fs::write(repo.join("vanished"), "about to disappear").unwrap();
            let output = diff_command(&repo, "")
                .env("PATH", &path)
                .env("ORIGINAL_PATH", &original)
                .env("MODE", mode)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(4), "{mode}");
            assert_eq!(std::fs::read_to_string(repo.join(".git/list-calls")).unwrap(), "listed\n");
            std::fs::remove_file(repo.join(".git/list-calls")).unwrap();
        }
    }

    #[test]
    fn the_diff_of_an_unborn_repository_shows_the_new_files() {
        let repo = temp_repo("diff-unborn");
        let output = diff_command(&repo, EMPTY_TREE).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

        let empty = diff_command(&repo, "").output().unwrap();
        assert!(empty.status.success() && empty.stdout.iter().all(u8::is_ascii_whitespace));
        assert!(
            std::process::Command::new("git")
                .args(["config", "core.autocrlf", "true"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("new.txt"), "hello\n").unwrap();
        std::fs::write(repo.join("-new path\nline"), "unusual name").unwrap();
        std::os::unix::fs::symlink("missing target", repo.join("link")).unwrap();
        let output = diff_command(&repo, EMPTY_TREE).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("?? new.txt"), "{text}");
        assert!(text.contains("new file mode") && text.contains("+hello"), "{text}");
        assert!(text.contains("+unusual name") && text.contains("+missing target"), "{text}");
    }

    fn found(pi_host: bool, gemini: Option<&str>) -> launch::Found<'_> {
        launch::Found { pi_host, gemini, proxy: None }
    }

    #[test]
    fn read_only_stages_pass_the_read_only_flags() {
        let claude = argv(&agent(AgentKind::Claude, Some("opus")), Access::Read, found(false, GEMINI), None).unwrap();
        assert!(claude.windows(2).any(|w| w == ["--disallowedTools", "Edit,Write,NotebookEdit"]));
        assert!(claude.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(
            !argv(&agent(AgentKind::Claude, None), Access::Write, found(false, GEMINI), None)
                .unwrap()
                .contains(&"--disallowedTools".to_owned())
        );

        let pi = argv(&agent(AgentKind::Pi, None), Access::Read, found(false, GEMINI), None).unwrap();
        assert!(pi.windows(2).any(|w| w == ["--tools", PI_READ_TOOLS]));
        assert_eq!(pi.last().map(String::as_str), Some("-p"));
        assert!(
            !argv(&agent(AgentKind::Pi, None), Access::Write, found(false, GEMINI), None)
                .unwrap()
                .contains(&"--tools".to_owned())
        );

        let codex = argv(&agent(AgentKind::Codex, Some("gpt-5")), Access::Read, found(false, GEMINI), None).unwrap();
        assert_eq!(
            codex,
            argv(&agent(AgentKind::Codex, Some("gpt-5")), Access::Write, found(false, GEMINI), None).unwrap()
        );
        assert!(codex.windows(2).any(|w| w == ["-m", "gpt-5"]));
        assert_eq!(codex.last().map(String::as_str), Some("-"));

        let gemini = argv(&agent(AgentKind::Gemini, None), Access::Read, found(false, GEMINI), None).unwrap();
        assert!(!gemini.contains(&"--yolo".to_owned()));
        assert!(
            argv(&agent(AgentKind::Gemini, None), Access::Write, found(false, GEMINI), None)
                .unwrap()
                .contains(&"--yolo".to_owned())
        );
        assert_eq!(gemini[3], "/opt/zwinger/host/gemini/bundle/gemini.js");
        assert!(matches!(
            argv(&agent(AgentKind::Gemini, None), Access::Write, found(false, None), None),
            Err(Failure::Start(_))
        ));

        let host_pi = argv(&agent(AgentKind::Pi, None), Access::Write, found(true, GEMINI), None).unwrap();
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

/// The limits of one process call: `idle` restarts on each stdout or
/// stderr chunk, `hard` does not. Each call, retry and gate command has its
/// own budget from its start; a stage has no limit of its own.
#[cfg(test)]
mod limit_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::time::Instant;

    use super::*;
    use crate::runtime::fake::{FakePipe, FakeRuntime, FakeVm};

    #[derive(Clone)]
    enum Act {
        Silent,
        /// Prints a line on stdout, then on stderr, at each interval.
        Ticks(Duration),
        /// Replies `text` as a claude result after the delay, then exits.
        Reply(Duration, String),
    }

    /// The pipes that the call opens play `acts`, in order. Counts the kills.
    fn exec(acts: Vec<Act>) -> (Exec<FakeRuntime>, Arc<AtomicUsize>) {
        let runtime = FakeRuntime::default();
        let kills = Arc::new(AtomicUsize::new(0));
        let (pipes, counted) = (runtime.pipes.clone(), kills.clone());
        tokio::spawn(async move {
            let mut next = 0;
            loop {
                let opened: Vec<FakePipe> = std::mem::take(&mut *pipes.lock().unwrap());
                for pipe in opened {
                    tokio::spawn(play(pipe, acts[next.min(acts.len() - 1)].clone(), counted.clone()));
                    next += 1;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        let exec = Exec {
            runtime,
            vm: Arc::new(FakeVm { key: "s".into() }),
            cwd: "/workspace".into(),
            scope: "/workspace".into(),
            artifacts: None,
            pi_host: false,
            gemini: None,
            proxy: None,
            state: None,
        };
        (exec, kills)
    }

    async fn play(mut pipe: FakePipe, act: Act, kills: Arc<AtomicUsize>) {
        loop {
            match pipe.stdin.recv().await {
                Some(PipeInput::Data(_)) => {}
                Some(PipeInput::Close) => break,
                Some(PipeInput::Kill) | None => return,
            }
        }
        let out = pipe.out.clone();
        let work = async move {
            match act {
                Act::Silent => std::future::pending::<()>().await,
                Act::Ticks(every) => loop {
                    tokio::time::sleep(every).await;
                    let _ = out.send(PipeEvent::Stdout(b"tick\n".to_vec())).await;
                    tokio::time::sleep(every).await;
                    let _ = out.send(PipeEvent::Stderr(b"tick\n".to_vec())).await;
                },
                Act::Reply(after, text) => {
                    tokio::time::sleep(after).await;
                    let _ = out.send(PipeEvent::Stdout(claude_result_line(&text))).await;
                    let _ = out.send(PipeEvent::Exit(0)).await;
                    std::future::pending::<()>().await
                }
            }
        };
        tokio::select! {
            () = work => {}
            input = pipe.stdin.recv() => if matches!(input, Some(PipeInput::Kill)) {
                kills.fetch_add(1, Ordering::SeqCst);
                let _ = pipe.out.send(PipeEvent::Exit(137)).await;
            },
        }
    }

    fn claude() -> Participant {
        Participant { id: "claude".to_owned(), kind: AgentKind::Claude, model: None }
    }

    fn limits(idle: u64, hard: u64) -> Limits {
        Limits { idle: Duration::from_millis(idle), hard: Duration::from_millis(hard) }
    }

    async fn agent_call(exec: &Exec<FakeRuntime>, limits: Limits, cancel: &mut watch::Receiver<bool>) -> Failure {
        exec.agent(&claude(), Access::Read, "Do it.", limits, cancel, &|_| {}).await.unwrap_err()
    }

    #[tokio::test]
    async fn silence_ends_an_agent_call_and_a_gate_at_the_idle_limit() {
        let (exec, kills) = exec(vec![Act::Silent]);
        let (_tx, mut cancel) = watch::channel(false);
        let start = Instant::now();
        assert!(matches!(agent_call(&exec, limits(100, 5000), &mut cancel).await, Failure::Idle(_)));
        let gate = exec.sh("pnpm build", limits(100, 5000), &mut cancel).await.unwrap();
        assert_eq!(gate.code, None);
        assert!(start.elapsed() < Duration::from_secs(2));
        // Each timed-out process was killed before the call returned.
        assert_eq!(kills.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn output_postpones_the_idle_limit_but_not_the_hard_limit() {
        let (exec, kills) = exec(vec![Act::Ticks(Duration::from_millis(30))]);
        let (_tx, mut cancel) = watch::channel(false);
        let start = Instant::now();
        assert!(matches!(agent_call(&exec, limits(150, 500), &mut cancel).await, Failure::Hard(_)));
        assert!(start.elapsed() >= Duration::from_millis(500));
        let start = Instant::now();
        let gate = exec.sh("pnpm build", limits(150, 500), &mut cancel).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(500));
        assert_eq!(gate.code, None);
        assert!(gate.output.contains("tick"));
        assert_eq!(kills.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_retry_gets_a_fresh_budget() {
        let slow = Duration::from_millis(250);
        let (exec, kills) = exec(vec![
            Act::Reply(slow, "No JSON here.".into()),
            Act::Reply(slow, "```json\n{\"ok\": true}\n```".into()),
        ]);
        let (_tx, mut cancel) = watch::channel(false);
        let start = Instant::now();
        let reply = exec.call_json(&claude(), Access::Read, "Do it.", limits(400, 400), &mut cancel, &|_| {}).await;
        // The two calls together take longer than one hard limit.
        assert!(start.elapsed() >= Duration::from_millis(500));
        assert!(reply.retried && reply.failure.is_none());
        assert_eq!(reply.json, Some(serde_json::json!({ "ok": true })));
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_cancel_kills_the_call_at_once() {
        let (exec, kills) = exec(vec![Act::Silent]);
        let (tx, mut cancel) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(true);
        });
        let start = Instant::now();
        assert!(matches!(agent_call(&exec, limits(5000, 5000), &mut cancel).await, Failure::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(kills.load(Ordering::SeqCst), 1);
    }
}
