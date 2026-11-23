//! Commit or pull request after a shipped run. The user reviews the exact
//! change and approves it; only then does the app write. Every git and
//! GitHub step is a fixed app script that runs in the run's cage VM: there
//! is no host git, no host `gh` and no host credential.
//!
//! - The review lists the changed files of the run's scope, writes the
//!   selected files as blobs, and binds the approval to a token: a SHA-256
//!   of the repository, the branch, `HEAD`, the selected index entries and
//!   blobs, the message and, for a PR, the full plan.
//! - The apply recomputes the token. A difference is `stale`: the user
//!   reviews again. The commit uses a temporary index, so unselected work
//!   stays as it is, and moves the branch with a compare-and-swap.
//! - `operations.jsonl` in the host round dir records each step, so a
//!   repeated click or a retry never makes a second commit, push or PR.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::agent::cli::{iso, now_ms};
use crate::cage::Mount;
use crate::cage::manager::{Manager, Vm, blocking, in_guest_folder, lock};
use crate::cage::tasks::{git_scope, guest_artifacts};
use crate::error::{Error, Result};
use crate::network::{Action, NetworkPolicy, Target};
use crate::runtime::SandboxRuntime;
use crate::task_agents::{Exec, Failure, ScriptOut};
use crate::task_artifacts::{self, read_record};
use crate::task_recovery::{self, RunRecord, digest};

/// The git state of the repository at the start of the run.
pub const START: &str = "start.json";
/// The porcelain status at the start of the run, after its `HEAD` line.
pub const START_STATUS: &str = "start-status.txt";
/// The host-only journal of the approved git operations of the run.
pub const OPERATIONS: &str = "operations.jsonl";
pub const GITHUB_API: &str = "https://api.github.com";
/// A longer review is not complete, so it cannot be approved.
const REVIEW_LIMIT: usize = 8 << 20;
/// The status at the start was cut at this size, so it is not complete.
const START_STATUS_LIMIT: usize = 64 << 10;

/// The start of each script after `GIT_PRELUDE`: no hooks, no fsmonitor, no
/// credential helper and no signing, whatever the repository configures, and
/// a temporary folder. `put` writes one frame: `<name> <bytes>\n<bytes>`.
const HARDEN: &str = r#"export LC_ALL=C GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0 GIT_CONFIG_COUNT=4
export GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=/dev/null
export GIT_CONFIG_KEY_1=core.fsmonitor GIT_CONFIG_VALUE_1=false
export GIT_CONFIG_KEY_2=credential.helper GIT_CONFIG_VALUE_2=
export GIT_CONFIG_KEY_3=commit.gpgSign GIT_CONFIG_VALUE_3=false
tmp=$(mktemp -d "${TMPDIR:-/tmp}/zwinger-git.XXXXXX") || exit 4
trap 'rm -rf "$tmp"' EXIT
put() { printf '%s %s\n' "$1" "$(wc -c < "$2" | tr -d ' ')" && cat -- "$2"; }
"#;

/// The repository, its configuration and the status of the scope. The
/// configuration and the hooks are read without the overrides of `HARDEN`.
const INSPECT: &str = r#"gitdir=$(git rev-parse --absolute-git-dir) || exit 4
hooks=$(unset GIT_CONFIG_COUNT; git rev-parse --git-path hooks) || exit 4
branch=$(git symbolic-ref -q HEAD) || branch=
head=$(git rev-parse -q --verify HEAD) || head=
{
  printf 'top=%s\n' "$top"
  printf 'gitdir=%s\n' "$gitdir"
  printf 'branch=%s\n' "$branch"
  printf 'head=%s\n' "$head"
  printf 'remote=%s\n' "$(git config --get remote.origin.url)"
  printf 'name=%s\n' "$(git config --get user.name)"
  printf 'email=%s\n' "$(git config --get user.email)"
} > "$tmp/meta" || exit 4
put meta "$tmp/meta"
(unset GIT_CONFIG_COUNT; git config -z --get-regexp '^(core\.hookspath|core\.fsmonitor|filter\..*|gpg\..*|commit\.gpgsign|credential\..*)$') > "$tmp/config"
[ "$?" -le 1 ] || exit 4
put config "$tmp/config"
: > "$tmp/hooks"
for f in "$hooks"/*; do
  [ -f "$f" ] && [ -x "$f" ] || continue
  case $f in *.sample) continue ;; esac
  printf '%s\n' "${f##*/}" >> "$tmp/hooks"
done
put hooks "$tmp/hooks"
{ git remote get-url --all origin; git remote get-url --push --all origin; } > "$tmp/urls" 2> /dev/null
put urls "$tmp/urls"
git status --porcelain=v2 -z -uall --renames -- "$@" > "$tmp/status" || exit 4
put status "$tmp/status"
"#;

/// The start identity of the repository: `INSPECT` without the status.
const IDENTITY: &str = r#"gitdir=$(git rev-parse --absolute-git-dir) || exit 4
branch=$(git symbolic-ref -q HEAD) || branch=
head=$(git rev-parse -q --verify HEAD) || head=
printf 'top=%s\ngitdir=%s\nbranch=%s\nhead=%s\nremote=%s\n' "$top" "$gitdir" "$branch" "$head" "$(git config --get remote.origin.url)"
"#;

/// Writes each selected path (NUL-separated on stdin) as a blob, and gives
/// its entry, its index entry and its diff from `HEAD`. A missing path is
/// a deletion. The diff compares the two blobs, so it shows exactly the
/// bytes that the commit takes. A changed mode comes first, as `git diff`
/// shows it.
const ENTRIES: &str = r#"cat > "$tmp/paths" || exit 4
empty=$(git hash-object -w -t blob /dev/null) || exit 4
xargs -0 -r sh -c '
  tmp=$1 empty=$2; shift 2
  put() { printf "%s %s\n" "$1" "$(wc -c < "$2" | tr -d " ")" && cat -- "$2"; }
  for path; do
    if [ -L "$path" ]; then
      mode=120000
      target=$(readlink -n -- "$path" && printf x) || exit 1
      oid=$(printf %s "${target%x}" | git hash-object -w --stdin) || exit 1
      new=$oid
    elif [ -f "$path" ]; then
      if [ -x "$path" ]; then mode=100755; else mode=100644; fi
      oid=$(git hash-object -w --no-filters -- "$path") || exit 1
      new=$oid
    elif [ -e "$path" ]; then
      exit 1
    else
      mode=0 oid=- new=$empty
    fi
    old=$(git rev-parse -q --verify "HEAD:$path") || old=$empty
    oldmode=$(git ls-tree --full-tree HEAD -- ":(literal)$path" 2> /dev/null | head -n 1 | cut -d " " -f 1)
    { printf "%s %s\n" "$mode" "$oid"; printf %s "$path"; } > "$tmp/entry" || exit 1
    put entry "$tmp/entry" || exit 1
    git ls-files -s -z -- ":(literal)$path" > "$tmp/index" || exit 1
    put index "$tmp/index" || exit 1
    {
      if [ -n "$oldmode" ] && [ "$mode" != 0 ] && [ "$oldmode" != "$mode" ]; then
        printf "old mode %s\nnew mode %s\n" "$oldmode" "$mode"
      fi
      git diff --no-color --no-ext-diff --no-textconv "$old" "$new"
    } > "$tmp/diff" || exit 1
    put diff "$tmp/diff" || exit 1
  done
' _ "$tmp" "$empty" < "$tmp/paths" || exit 4
"#;

/// The tree of the commit: `HEAD` (`$a1`, empty before the first commit)
/// and the entries on stdin, in a temporary index.
const TREE: &str = r#"cat > "$tmp/entries" || exit 4
export GIT_INDEX_FILE="$tmp/index"
if [ -n "$a1" ]; then git read-tree "$a1" || exit 4; else git read-tree --empty || exit 4; fi
git update-index -z --index-info < "$tmp/entries" || exit 4
tree=$(git write-tree) || exit 4
unset GIT_INDEX_FILE
"#;

/// After `TREE`: fetches the base branch `$a2`, then the commits from its tip
/// to `HEAD` and the diff from the merge base to the new tree.
const PR_PREVIEW: &str = r#"git fetch --no-tags --no-recurse-submodules --no-write-fetch-head origin "+refs/heads/$a2:refs/remotes/origin/$a2" > /dev/null || exit 5
tip=$(git rev-parse -q --verify "refs/remotes/origin/$a2^{commit}") || exit 5
mb=$(git merge-base "$tip" "$a1") || mb=
printf '%s\n%s\n%s\n' "$tip" "$mb" "$tree" > "$tmp/meta" || exit 4
put meta "$tmp/meta"
git log --no-color --format='%H %s' "$tip..$a1" > "$tmp/log" || exit 4
put log "$tmp/log"
if [ -n "$mb" ]; then
  git diff --no-color --no-ext-diff --no-textconv "$mb" "$tree" > "$tmp/diff" || exit 4
else
  : > "$tmp/diff"
fi
put diff "$tmp/diff"
"#;

/// After `TREE`: commits the tree on `$a1` with the message `$a3`, then
/// moves the branch `$a2` with a compare-and-swap, then updates the real
/// index for the selected paths only. A `HEAD` that is already a commit of
/// this tree on `$a1` is the commit of an earlier try. With `$a4` set it
/// only finds such a commit. Codes: 10 `HEAD` moved, 11 the branch moved
/// during the commit, 12 committed but the index was not updated.
const COMMIT: &str = r#"[ "$(git symbolic-ref -q HEAD)" = "$a2" ] || exit 10
now=$(git rev-parse -q --verify HEAD) || now=
if [ "$now" = "$a1" ] && [ -z "$a4" ]; then
  printf '%s' "$a3" > "$tmp/msg" || exit 4
  if [ -n "$a1" ]; then
    new=$(git commit-tree --no-gpg-sign "$tree" -p "$a1" -F "$tmp/msg") || exit 4
  else
    new=$(git commit-tree --no-gpg-sign "$tree" -F "$tmp/msg") || exit 4
  fi
  zero=$(printf %s "$tree" | tr 0-9a-f 0)
  git update-ref -m 'zwinger: approved commit' "$a2" "$new" "${a1:-$zero}" || exit 11
else
  parent=$(git rev-parse -q --verify "$now^1") || parent=
  [ -n "$now" ] && [ "$parent" = "$a1" ] && [ "$(git rev-parse "$now^{tree}")" = "$tree" ] || exit 10
  new=$now
fi
printf 'committed %s\n' "$new"
git update-index -z --index-info < "$tmp/entries" || exit 12
"#;

/// Pushes the commit `$a1` to the new or same branch `$a2` of `origin`, never forced.
const PUSH: &str = r#"git check-ref-format "refs/heads/$a2" || exit 6
git push --no-verify --porcelain origin "$a1:refs/heads/$a2" || exit 7
"#;

/// The commit of the branch `$a1` of `origin`, if it has one.
const LS_REMOTE: &str = r#"git ls-remote origin "refs/heads/$a1" || exit 7
"#;

/// One GitHub API call through the cage's proxy, which adds the credential
/// of the cage's GitHub rule. `$a1` is `GET` or `POST`, `$a2` the URL, and
/// the JSON body is on stdin.
const API: &str = r#"cat > "$tmp/body" || exit 4
if [ "$a1" = POST ]; then
  curl -sS --max-time 60 -X POST -H 'Accept: application/vnd.github+json' -H 'Content-Type: application/json' --data-binary @"$tmp/body" -o "$tmp/out" -w '%{http_code}' "$a2" > "$tmp/code" || exit 8
else
  curl -sS --max-time 60 -H 'Accept: application/vnd.github+json' -o "$tmp/out" -w '%{http_code}' "$a2" > "$tmp/code" || exit 8
fi
put code "$tmp/code"
put body "$tmp/out"
"#;

/// Where the run's working dir was at its start: on the host and in the VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Origin {
    /// The canonical host folder that the guest root mounts.
    pub host: String,
    /// The guest root: the session folder or the mount.
    pub guest: String,
    /// The working dir in the VM.
    pub workdir: String,
    /// The folder that the git checks cover.
    pub scope: String,
}

impl Origin {
    /// The mapping of `workdir` in the VM: the session folder or a mount.
    pub fn of(workdir: &str, scope: &str, folder: &str, host_folder: &Path, mounts: &[Mount]) -> Option<Self> {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned()).display().to_string();
        let path = Path::new(workdir);
        let (host, guest) = if path.starts_with(folder) {
            (canonical(host_folder), folder.to_owned())
        } else {
            let mount = mounts.iter().find(|m| path.starts_with(&m.guest))?;
            (canonical(&mount.host), mount.guest.clone())
        };
        Some(Self { host, guest, workdir: workdir.to_owned(), scope: scope.to_owned() })
    }
}

/// The repository at the start of a run, in `start.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub toplevel: String,
    pub gitdir: String,
    /// `refs/heads/<name>`, or empty if `HEAD` is detached.
    pub branch: String,
    /// Empty before the first commit.
    pub head: String,
    /// The URL of `origin`, or empty.
    pub remote: String,
    /// The commit at the start of the run, for the diff.
    #[serde(default)]
    pub base: String,
}

fn parse_meta(bytes: &[u8]) -> (Repo, BTreeMap<String, String>) {
    let text = String::from_utf8_lossy(bytes);
    let map: BTreeMap<String, String> =
        text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    let get = |k: &str| map.get(k).cloned().unwrap_or_default();
    let repo = Repo {
        toplevel: get("top"),
        gitdir: get("gitdir"),
        branch: get("branch"),
        head: get("head"),
        remote: get("remote"),
        base: String::new(),
    };
    (repo, map)
}

/// The identity of the repository at the start of the run. `None` if the
/// working dir is not in git or git failed: then no git action is offered.
pub async fn start_identity<R: SandboxRuntime>(exec: &Exec<R>, cancel: &mut watch::Receiver<bool>) -> Option<Repo> {
    let out = exec.git_script(&format!("{HARDEN}{IDENTITY}"), &[], b"", 64 << 10, false, cancel).await.ok()?;
    if out.code != 0 || out.truncated {
        return None;
    }
    let (repo, _) = parse_meta(&out.stdout);
    (!repo.toplevel.is_empty() && !repo.gitdir.is_empty()).then_some(repo)
}

/// The frames of a script's stdout, in order. `None` if a frame is cut.
fn frames(mut bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let newline = bytes.iter().position(|&b| b == b'\n')?;
        let header = std::str::from_utf8(&bytes[..newline]).ok()?;
        let (name, len) = header.split_once(' ')?;
        let len: usize = len.parse().ok()?;
        let body = bytes.get(newline + 1..newline + 1 + len)?;
        out.push((name.to_owned(), body.to_vec()));
        bytes = &bytes[newline + 1 + len..];
    }
    Some(out)
}

fn frame<'a>(frames: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
    frames.iter().find(|(n, _)| n == name).map_or(&[], |(_, b)| b)
}

/// What a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Commit,
    Pr,
}

/// The user's choice for a review: the files, the message and, for a PR,
/// its title, body and base branch. A change asks for a new review.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub kind: Option<Kind>,
    /// `None` selects every changed file that can be selected.
    pub selection: Option<Vec<String>>,
    pub message: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// The base branch of the PR. Empty: the default branch of `origin`.
    #[serde(default)]
    pub base: String,
}

impl Request {
    fn kind(&self) -> Kind {
        self.kind.unwrap_or(Kind::Commit)
    }
}

/// A changed file of the scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub path: String,
    /// The old path of a rename.
    pub from: Option<String>,
    /// `modified`, `added`, `deleted`, `renamed`, `untracked` or `unmerged`.
    pub kind: &'static str,
    pub binary: bool,
    /// The index has a change of the file.
    pub staged: bool,
    /// `None`: the state at the start of the run is not known.
    pub dirty_at_start: Option<bool>,
    pub selected: bool,
    /// Why the file cannot be selected.
    pub unselectable: Option<String>,
    /// The full diff from `HEAD` to the selected content. Empty if not selected.
    pub diff: String,
}

/// The plan of a PR, all of it shown before the approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPlan {
    pub owner: String,
    pub repo: String,
    pub base: String,
    /// The branch of `origin` that the push writes.
    pub head: String,
    /// The head branch does not exist on `origin` at the review.
    pub new_branch: bool,
    /// The commit of the head branch of `origin` at the review, or empty if
    /// it does not exist. The push needs it unchanged.
    #[serde(default)]
    pub head_tip: String,
    pub base_tip: String,
    pub title: String,
    pub body: String,
    /// `<base tip>..HEAD`, then the new commit.
    pub commits: Vec<String>,
    /// The full diff from the merge base to the new commit.
    pub diff: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub kind: Kind,
    /// `None` if the review cannot be approved; `blocked` says why.
    pub token: Option<String>,
    pub blocked: Vec<String>,
    /// Where the git steps run.
    pub location: String,
    pub mount: String,
    pub scope: String,
    pub repo: Repo,
    /// Problems of the review that do not block it, such as files that
    /// were already changed at the start of the run.
    pub notes: Vec<String>,
    pub files: Vec<File>,
    pub pr: Option<PrPlan>,
    /// The recorded operation of this token, if it ran.
    pub operation: Option<Operation>,
}

/// The facts of a run that a review and an apply use.
#[derive(Debug, Clone)]
pub struct Context {
    /// `<session>/<task>/<run>`.
    pub run: String,
    pub slug: String,
    pub run_name: String,
    pub origin: Origin,
    pub start: Repo,
    /// `None`: the state at the start is not known.
    pub start_status: Option<String>,
    /// The cage has an Allow rule for GitHub with a credential.
    pub github: bool,
    pub api: String,
    pub dir: PathBuf,
}

/// The selected entry of a file: what the commit takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub path: String,
    /// `0` deletes the path.
    pub mode: String,
    /// `None` for a deletion.
    pub oid: Option<String>,
    /// The index entries of the path at the review, as `ls-files -s` gives them.
    pub index: String,
}

/// What an approved operation does, as the journal keeps it for a retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub kind: Kind,
    pub branch: String,
    pub head: String,
    pub entries: Vec<Entry>,
    pub message: String,
    pub pr: Option<PrPlan>,
}

/// The token of a plan: the run, the repository and the whole plan.
fn token(ctx: &Context, repo: &Repo, plan: &Plan) -> String {
    let pr = plan.pr.as_ref().map(|p| {
        json!({
            "owner": p.owner, "repo": p.repo, "base": p.base, "head": p.head, "headTip": p.head_tip, "baseTip": p.base_tip,
            "title": p.title, "body": p.body, "range": format!("{}..{}", p.base_tip, plan.head),
        })
    });
    let bound = json!({
        "run": ctx.run, "mount": ctx.origin.host, "toplevel": repo.toplevel, "gitdir": repo.gitdir,
        "remote": repo.remote, "branch": plan.branch, "head": plan.head, "entries": plan.entries,
        "message": plan.message, "kind": plan.kind, "pr": pr,
    });
    digest(bound.to_string().as_bytes())
}

fn script_error(step: &str, out: &ScriptOut) -> String {
    let last = out.stderr.iter().rev().find(|l| !l.trim().is_empty());
    if out.code == 127 || last.is_some_and(|l| l.contains("not found")) {
        return format!("{step}: a tool is missing in the cage ({})", last.map_or("exit 127", String::as_str));
    }
    match last {
        Some(line) => format!("{step} failed (code {}): {line}", out.code),
        None => format!("{step} failed (code {})", out.code),
    }
}

fn failure(step: &str, failure: Failure) -> String {
    format!("{step} {failure}")
}

async fn run<R: SandboxRuntime>(
    exec: &Exec<R>,
    script: &str,
    args: &[&str],
    input: &[u8],
    net: bool,
    step: &str,
) -> std::result::Result<ScriptOut, String> {
    // A dropped sender reads as a cancel, so it lives until the script ends.
    let (_keep, mut cancel) = watch::channel(false);
    exec.git_script(&format!("{HARDEN}{script}"), args, input, REVIEW_LIMIT, net, &mut cancel)
        .await
        .map_err(|f| failure(step, f))
}

/// The NUL-separated stdin of `update-index -z --index-info`.
fn index_info(entries: &[Entry], zero: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        let (mode, oid) = match &entry.oid {
            Some(oid) => (entry.mode.as_str(), oid.as_str()),
            None => ("0", zero),
        };
        out.extend_from_slice(format!("{mode} {oid}\t{}", entry.path).as_bytes());
        out.push(0);
    }
    out
}

fn zero_of(plan: &Plan) -> String {
    let len = plan.entries.iter().find_map(|e| e.oid.as_ref()).map_or(plan.head.len().max(40), String::len);
    "0".repeat(len)
}

/// A status record of `git status --porcelain=v2 -z`.
struct Change {
    path: String,
    from: Option<String>,
    kind: &'static str,
    staged: bool,
    submodule: bool,
}

fn parse_status(bytes: &[u8]) -> Vec<Change> {
    let mut records = bytes.split(|&b| b == 0).map(|r| String::from_utf8_lossy(r).into_owned());
    let mut out = Vec::new();
    while let Some(record) = records.next() {
        let fields: Vec<&str> = record.splitn(2, ' ').collect();
        match fields.as_slice() {
            ["1", rest] | ["2", rest] => {
                let renamed = record.starts_with('2');
                let parts: Vec<&str> = rest.splitn(if renamed { 9 } else { 8 }, ' ').collect();
                let Some(path) = parts.last() else { continue };
                let xy = parts[0].as_bytes();
                let (x, y) = (xy[0], xy.get(1).copied().unwrap_or(b'.'));
                let from = if renamed { records.next() } else { None };
                let kind = if renamed {
                    "renamed"
                } else if x == b'D' || y == b'D' {
                    "deleted"
                } else if x == b'A' {
                    "added"
                } else {
                    "modified"
                };
                out.push(Change {
                    path: (*path).to_owned(),
                    from,
                    kind,
                    staged: x != b'.',
                    submodule: parts.get(1).is_some_and(|s| s.starts_with('S')),
                });
            }
            ["u", rest] => {
                if let Some(path) = rest.splitn(10, ' ').nth(9) {
                    out.push(Change {
                        path: path.to_owned(),
                        from: None,
                        kind: "unmerged",
                        staged: true,
                        submodule: false,
                    });
                }
            }
            ["?", path] => out.push(Change {
                path: (*path).to_owned(),
                from: None,
                kind: "untracked",
                staged: false,
                submodule: false,
            }),
            _ => {}
        }
    }
    out
}

/// The paths that `git status --porcelain=v1` lists, unquoted. `None` if
/// the list is not complete.
fn start_paths(status: &str) -> Option<Vec<String>> {
    if status.len() >= START_STATUS_LIMIT {
        return None;
    }
    let mut out = Vec::new();
    for line in status.lines().skip_while(|l| l.starts_with("HEAD ")) {
        let Some(rest) = line.get(3..) else { continue };
        match rest.split_once(" -> ") {
            Some((from, to)) if !from.starts_with('"') || from.ends_with('"') => {
                out.push(unquote(from));
                out.push(unquote(to));
            }
            _ => out.push(unquote(rest)),
        }
    }
    Some(out)
}

/// A path as git's C quoting gives it.
fn unquote(text: &str) -> String {
    let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
        return text.to_owned();
    };
    let (mut bytes, mut chars) = (Vec::new(), inner.bytes().peekable());
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(7),
            Some(b'b') => bytes.push(8),
            Some(b'f') => bytes.push(12),
            Some(b'v') => bytes.push(11),
            Some(d @ b'0'..=b'7') => {
                let mut value = u32::from(d - b'0');
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = chars.peek() {
                        value = value * 8 + u32::from(n - b'0');
                        chars.next();
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `owner` and `repo` of an HTTPS GitHub URL.
fn github_repo(url: &str) -> std::result::Result<(String, String), String> {
    let Some(rest) = url.strip_prefix("https://github.com/") else {
        return Err(if url.is_empty() {
            "the repository has no remote origin".to_owned()
        } else if url.starts_with("https://") {
            format!("origin ({url}) is not a GitHub repository")
        } else {
            format!("origin ({url}) does not use HTTPS, so the cage proxy cannot add the GitHub credential")
        });
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let valid = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    match rest.split_once('/') {
        Some((owner, repo)) if valid(owner) && valid(repo) => Ok((owner.to_owned(), repo.to_owned())),
        _ => Err(format!("origin ({url}) is not a GitHub repository URL")),
    }
}

/// A branch name in a URL query.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The configuration and the hooks that the app does not run.
fn unsupported(config: &[u8], hooks: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for record in config.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8_lossy(record);
        let (key, value) = record.split_once('\n').unwrap_or((&record, ""));
        let key = key.to_ascii_lowercase();
        let off = matches!(value.to_ascii_lowercase().as_str(), "" | "false" | "no" | "off" | "0");
        let refused = match key.as_str() {
            "commit.gpgsign" | "core.fsmonitor" => !off,
            k if k.starts_with("credential.") => k.ends_with(".helper") && !value.is_empty(),
            _ => true,
        };
        if refused {
            out.push(format!("the repository configures {key}, which the app does not run; remove it to commit here"));
        }
    }
    for hook in String::from_utf8_lossy(hooks).lines().filter(|l| !l.is_empty()) {
        out.push(format!("the repository has the hook {hook}, which the app does not run; remove it to commit here"));
    }
    out
}

/// Compares the repository now with the one at the start of the run.
fn same_repo(start: &Repo, now: &Repo) -> std::result::Result<(), String> {
    if start.toplevel != now.toplevel || start.gitdir != now.gitdir {
        return Err(format!(
            "the working dir is now in the repository {} ({}), not in {} ({}) as at the start of the run",
            now.toplevel, now.gitdir, start.toplevel, start.gitdir
        ));
    }
    if start.remote != now.remote {
        return Err(format!("origin changed since the start of the run: {} is now {}", start.remote, now.remote));
    }
    if start.branch != now.branch {
        let name = |b: &str| if b.is_empty() { "a detached HEAD".to_owned() } else { short(b).to_owned() };
        return Err(format!(
            "the branch changed since the start of the run: {} is now {}",
            name(&start.branch),
            name(&now.branch)
        ));
    }
    Ok(())
}

fn short(branch: &str) -> &str {
    branch.strip_prefix("refs/heads/").unwrap_or(branch)
}

type Frames = Vec<(String, Vec<u8>)>;

/// The repository now, checked against the one at the start of the run.
async fn inspect<R: SandboxRuntime>(
    exec: &Exec<R>,
    ctx: &Context,
) -> std::result::Result<(Repo, BTreeMap<String, String>, Frames), String> {
    let out = run(exec, INSPECT, &[], b"", false, "the git check").await?;
    if out.code == 3 {
        return Err("the working dir of the run is not in a git repository".to_owned());
    }
    if out.code != 0 {
        return Err(script_error("the git check", &out));
    }
    let found = frames(&out.stdout).filter(|_| !out.truncated).ok_or("the git check gave too much output")?;
    let (repo, meta) = parse_meta(frame(&found, "meta"));
    same_repo(&ctx.start, &repo)?;
    Ok((repo, meta, found))
}

/// Every URL that `git fetch origin` and `git push origin` use, after
/// `pushurl` and the URL rewrites, must be the reviewed GitHub repository.
fn destinations(urls: &[u8], owner: &str, repo: &str) -> std::result::Result<(), String> {
    let text = String::from_utf8_lossy(urls);
    let mut any = false;
    for url in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        any = true;
        if github_repo(url)? != (owner.to_owned(), repo.to_owned()) {
            return Err(format!("origin fetches or pushes to {url}, not to the reviewed github.com/{owner}/{repo}"));
        }
    }
    if !any {
        return Err("git gave no fetch or push URL for origin".to_owned());
    }
    Ok(())
}

/// Before a resumed operation does more: the repository, its configuration
/// and, for a PR, the push destination are still the reviewed ones.
async fn recheck<R: SandboxRuntime>(exec: &Exec<R>, ctx: &Context, plan: &Plan) -> std::result::Result<(), String> {
    let (repo, _, found) = inspect(exec, ctx).await?;
    if repo.branch != plan.branch {
        return Err(format!("the branch is now {}, not {}", short(&repo.branch), short(&plan.branch)));
    }
    if let Some(why) = unsupported(frame(&found, "config"), frame(&found, "hooks")).into_iter().next() {
        return Err(why);
    }
    if let Some(pr) = &plan.pr {
        destinations(frame(&found, "urls"), &pr.owner, &pr.repo)?;
    }
    Ok(())
}

/// Reviews the change of the run for the request: complete, or blocked.
/// An `Err` is a named reason why no git action is available.
pub async fn review<R: SandboxRuntime>(
    exec: &Exec<R>,
    ctx: &Context,
    request: &Request,
) -> std::result::Result<(Review, Option<Plan>), String> {
    let kind = request.kind();
    let (mut repo, meta, found) = inspect(exec, ctx).await?;
    repo.base = ctx.start.base.clone();
    let mut blocked = unsupported(frame(&found, "config"), frame(&found, "hooks"));
    let mut notes = vec!["The commit is not signed.".to_owned()];
    if meta.get("name").is_none_or(String::is_empty) || meta.get("email").is_none_or(String::is_empty) {
        blocked.push(
            "git in the cage has no user.name or user.email; set them in the cage, the app does not copy them from this Mac"
                .to_owned(),
        );
    }
    if repo.branch.is_empty() {
        blocked.push("HEAD is detached; check out a branch in the cage".to_owned());
    }

    let start = ctx.start_status.as_deref().and_then(start_paths);
    if start.is_none() {
        notes.push(
            "The state at the start of the run is not known, so a file can have changes from before the run."
                .to_owned(),
        );
    }
    let changes = parse_status(frame(&found, "status"));
    let mut files: Vec<File> = changes
        .iter()
        .map(|c| {
            let unselectable = if c.kind == "unmerged" {
                Some("the file has a merge conflict".to_owned())
            } else if c.submodule {
                Some("the app does not commit submodules".to_owned())
            } else {
                None
            };
            let dirty = start.as_ref().map(|s| s.contains(&c.path) || c.from.as_ref().is_some_and(|f| s.contains(f)));
            File {
                path: c.path.clone(),
                from: c.from.clone(),
                kind: c.kind,
                binary: false,
                staged: c.staged,
                dirty_at_start: dirty,
                selected: false,
                unselectable,
                diff: String::new(),
            }
        })
        .collect();
    if files.iter().any(|f| f.kind == "unmerged") {
        blocked.push("the repository has a merge conflict".to_owned());
    }
    match &request.selection {
        None => files.iter_mut().filter(|f| f.unselectable.is_none()).for_each(|f| f.selected = true),
        Some(paths) => {
            for path in paths {
                match files.iter_mut().find(|f| &f.path == path) {
                    Some(file) if file.unselectable.is_none() => file.selected = true,
                    Some(file) => blocked
                        .push(format!("{path} cannot be selected: {}", file.unselectable.as_deref().unwrap_or(""))),
                    None => blocked.push(format!("{path} has no change now")),
                }
            }
        }
    }
    if !files.iter().any(|f| f.selected) {
        blocked.push("select at least one file".to_owned());
    }
    if request.message.trim().is_empty() {
        blocked.push("write a commit message".to_owned());
    }

    // The selected paths, with the old path of a rename.
    let mut paths: Vec<String> = Vec::new();
    for file in files.iter().filter(|f| f.selected) {
        paths.extend(file.from.clone());
        paths.push(file.path.clone());
    }
    let mut entries = Vec::new();
    if !paths.is_empty() {
        let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
        let out = run(exec, ENTRIES, &[], &input, false, "the review").await?;
        if out.code != 0 {
            return Err(script_error("the review of the selected files", &out));
        }
        if out.truncated {
            blocked.push(
                "the review is too large to show in full, so it cannot be approved; select fewer files".to_owned(),
            );
        }
        let found = frames(&out.stdout).unwrap_or_default();
        let mut diffs: HashMap<String, String> = HashMap::new();
        for chunk in found.chunks(3) {
            let [(_, entry), (_, index), (_, diff)] = chunk else { break };
            let entry = String::from_utf8_lossy(entry);
            let Some((head, path)) = entry.split_once('\n') else { continue };
            let Some((mode, oid)) = head.split_once(' ') else { continue };
            let diff = String::from_utf8_lossy(diff).into_owned();
            entries.push(Entry {
                path: path.to_owned(),
                mode: mode.to_owned(),
                oid: (oid != "-").then(|| oid.to_owned()),
                index: String::from_utf8_lossy(index).replace('\0', "\n"),
            });
            diffs.insert(path.to_owned(), diff);
        }
        if entries.len() != paths.len() && !out.truncated {
            return Err("the review of the selected files is not complete".to_owned());
        }
        for file in files.iter_mut().filter(|f| f.selected) {
            let mut diff = String::new();
            for path in file.from.iter().chain([&file.path]) {
                diff.push_str(diffs.get(path).map_or("", String::as_str));
            }
            file.binary = diff.lines().any(|l| l.starts_with("Binary files "));
            if file.binary {
                blocked.push(format!("{} is binary, so the review cannot show its content; leave it out", file.path));
            }
            if file.dirty_at_start == Some(true) {
                notes.push(format!(
                    "{} was already changed at the start of the run; the commit takes the whole file, also those changes",
                    file.path
                ));
            }
            file.diff = diff;
        }
    }

    let mut pr = None;
    if kind == Kind::Pr && !repo.head.is_empty() {
        match pr_plan(exec, ctx, &repo, request, &entries, frame(&found, "urls")).await {
            Ok(plan) => {
                if plan.diff.lines().any(|l| l.starts_with("Binary files ")) {
                    blocked.push("the PR has a binary change, so the review cannot show all its content".to_owned());
                }
                pr = Some(plan);
            }
            Err(why) => blocked.push(why),
        }
    } else if kind == Kind::Pr {
        blocked.push("the branch has no commit yet; make a commit first".to_owned());
    }

    let plan = Plan {
        kind,
        branch: repo.branch.clone(),
        head: repo.head.clone(),
        entries,
        message: request.message.clone(),
        pr: pr.clone(),
    };
    let token = blocked.is_empty().then(|| token(ctx, &repo, &plan));
    let operation = match &token {
        Some(t) => blocking_journal(&ctx.dir).await.remove(t).map(|r| r.last),
        None => None,
    };
    let review = Review {
        kind,
        token,
        blocked,
        location: format!("in the cage VM, in {}", repo.toplevel),
        mount: format!("{} is {} in the VM", ctx.origin.host, ctx.origin.guest),
        scope: ctx.origin.scope.clone(),
        repo,
        notes,
        files,
        pr,
        operation,
    };
    Ok((review, Some(plan)))
}

async fn pr_plan<R: SandboxRuntime>(
    exec: &Exec<R>,
    ctx: &Context,
    repo: &Repo,
    request: &Request,
    entries: &[Entry],
    urls: &[u8],
) -> std::result::Result<PrPlan, String> {
    let (owner, name) = github_repo(&repo.remote)?;
    destinations(urls, &owner, &name)?;
    if !ctx.github {
        return Err("the cage has no Allow rule for GitHub with a credential; add one in the network rules".to_owned());
    }
    let base = if request.base.trim().is_empty() { "main".to_owned() } else { request.base.trim().to_owned() };
    let branch = short(&repo.branch).to_owned();
    let head = if branch == base { format!("zwinger/{}-{}", ctx.slug, ctx.run_name) } else { branch };
    let zero = "0".repeat(repo.head.len());
    let input = index_info(entries, &zero);
    let out = run(exec, &format!("{TREE}{PR_PREVIEW}"), &[&repo.head, &base], &input, true, "the PR review").await?;
    if out.code == 5 {
        return Err(format!(
            "the cage could not fetch the base branch {base} of origin: {}",
            script_error("git fetch", &out)
        ));
    }
    if out.code != 0 {
        return Err(script_error("the PR review", &out));
    }
    if out.truncated {
        return Err("the PR diff is too large to show in full, so it cannot be approved".to_owned());
    }
    let found = frames(&out.stdout).ok_or("the PR review gave damaged output")?;
    let meta = String::from_utf8_lossy(frame(&found, "meta")).into_owned();
    let mut lines = meta.lines();
    let (tip, merge_base) = (lines.next().unwrap_or_default().to_owned(), lines.next().unwrap_or_default());
    if merge_base.is_empty() {
        return Err(format!("the branch has no common history with {base} of origin"));
    }
    let title = if request.title.trim().is_empty() {
        request.message.lines().next().unwrap_or_default().trim().to_owned()
    } else {
        request.title.trim().to_owned()
    };
    let mut commits: Vec<String> = String::from_utf8_lossy(frame(&found, "log")).lines().map(str::to_owned).collect();
    commits.reverse();
    commits.push(format!("(new) {}", request.message.lines().next().unwrap_or_default()));
    let head_tip = remote_head(exec, &head).await?.unwrap_or_default();
    Ok(PrPlan {
        owner,
        repo: name,
        base,
        head,
        new_branch: head_tip.is_empty(),
        head_tip,
        base_tip: tip,
        title,
        body: request.body.clone(),
        commits,
        diff: String::from_utf8_lossy(frame(&found, "diff")).into_owned(),
    })
}

/// The state of an approved operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    Planned,
    Committed,
    Pushed,
    Pr,
    Failed,
    /// The approval no longer matches the repository. Not in the journal.
    Stale,
}

/// One line of `operations.jsonl`, and the state that the UI shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub token: String,
    pub kind: Kind,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The branch of `origin` that has the commit.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ref")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The failed step: `commit`, `index`, `push` or `pr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The journal of one token: its plan and its last state.
#[derive(Debug, Clone)]
pub struct Record {
    pub plan: Option<Plan>,
    pub last: Operation,
}

/// By token: the plan and the last state.
fn journal(dir: &Path) -> HashMap<String, Record> {
    let mut out: HashMap<String, Record> = HashMap::new();
    let Ok(Some(bytes)) = read_record(dir, OPERATIONS) else {
        return out;
    };
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(line) else { continue };
        let Ok(operation) = serde_json::from_value::<Operation>(value["operation"].clone()) else { continue };
        let plan = serde_json::from_value::<Plan>(value["plan"].clone()).ok();
        let record = out.entry(operation.token.clone()).or_insert(Record { plan: None, last: operation.clone() });
        if plan.is_some() {
            record.plan = plan;
        }
        // A later line keeps the effects that an earlier one recorded.
        let last = &mut record.last;
        let sha = operation.sha.clone().or(last.sha.take());
        let reference = operation.reference.clone().or(last.reference.take());
        let url = operation.url.clone().or(last.url.take());
        *last = Operation { sha, reference, url, ..operation };
    }
    out
}

fn append(dir: &Path, operation: &Operation, plan: Option<&Plan>) -> std::result::Result<(), String> {
    let line = json!({ "at": iso(now_ms()), "operation": operation, "plan": plan });
    task_artifacts::append_durable(dir, OPERATIONS, &line)
        .map_err(|e| format!("the app could not write {OPERATIONS}: {e}"))
}

/// The one lock of each repository: two applies never run at once.
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

fn repo_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    lock(&LOCKS).entry(key.to_owned()).or_default().clone()
}

/// Runs the approved operation of `token`, or continues it after a failure
/// or a crash. A finished operation returns its recorded state.
pub async fn apply<R: SandboxRuntime>(
    exec: &Exec<R>,
    ctx: &Context,
    request: &Request,
    token: &str,
) -> std::result::Result<Operation, String> {
    let guard = repo_lock(&format!("{}\n{}", ctx.origin.host, ctx.start.gitdir));
    let _held = guard.lock().await;
    let dir = &ctx.dir;
    let recorded = blocking_journal(dir).await.remove(token);
    let mut op = Operation {
        token: token.to_owned(),
        kind: request.kind(),
        state: State::Planned,
        sha: None,
        reference: None,
        url: None,
        step: None,
        detail: None,
    };
    let fresh_review = async || -> std::result::Result<Option<Plan>, String> {
        let (review, plan) = review(exec, ctx, request).await?;
        Ok(if review.token.as_deref() == Some(token) { plan } else { None })
    };
    // A matching fresh review checked the repository; a resumed plan is checked below.
    let mut checked = false;
    let plan = match recorded {
        Some(Record { last, .. }) if last.state == State::Pr => return Ok(last),
        Some(Record { last, .. }) if last.state == State::Committed && last.kind == Kind::Commit => return Ok(last),
        Some(Record { plan: Some(plan), last }) if last.sha.is_some() => {
            op = last;
            op.state = State::Committed;
            plan
        }
        Some(Record { plan: Some(plan), last }) => {
            // Planned, or failed before a known commit: the commit is not
            // known. A matching review commits; else only an earlier commit counts.
            op.kind = last.kind;
            match fresh_review().await? {
                Some(_) => {
                    checked = true;
                    plan
                }
                None => {
                    match commit(exec, &plan, true).await {
                        Ok(sha) => {
                            op.sha = Some(sha);
                            op.state = State::Committed;
                            append(dir, &op, None)?;
                        }
                        Err((_, why)) => return Ok(stale(op, why)),
                    }
                    plan
                }
            }
        }
        _ => match fresh_review().await? {
            Some(plan) => {
                checked = true;
                append(dir, &op, Some(&plan))?;
                plan
            }
            None => {
                return Ok(stale(op, "the files, the index or the branch changed since the review".to_owned()));
            }
        },
    };

    if !checked && let Err(why) = recheck(exec, ctx, &plan).await {
        let step = op.step.clone().unwrap_or_else(|| "push".to_owned());
        let why = match &op.sha {
            Some(sha) => format!("{why}; the commit {sha} stays"),
            None => why,
        };
        return fail(dir, op, &step, why);
    }
    if op.sha.is_none() {
        match commit(exec, &plan, false).await {
            Ok(sha) => {
                op.sha = Some(sha);
                op.state = State::Committed;
                append(dir, &op, None)?;
            }
            Err((Some(sha), why)) => {
                op.sha = Some(sha);
                return fail(dir, op, "index", why);
            }
            Err((None, why)) if why.starts_with("stale:") => {
                return Ok(stale(op, why.trim_start_matches("stale: ").to_owned()));
            }
            Err((None, why)) => return fail(dir, op, "commit", why),
        }
    } else if op.step.as_deref() == Some("index") {
        // The commit exists; repair the index of the selected paths only.
        if let Err((_, why)) = commit(exec, &plan, true).await {
            return fail(dir, op, "index", why);
        }
        op.step = None;
        op.detail = None;
        append(dir, &op, None)?;
    }
    if plan.kind == Kind::Commit {
        return Ok(op);
    }
    let Some(pr) = &plan.pr else {
        return fail(dir, op, "push", "the plan has no PR".to_owned());
    };
    let sha = op.sha.clone().unwrap_or_default();
    if !checked {
        // A resumed operation skipped the review: the PR must still compare
        // against the approved tip of the base branch.
        match remote_head(exec, &pr.base).await {
            Ok(Some(found)) if found == pr.base_tip => {}
            Ok(found) => {
                let now = found.map_or_else(|| "deleted".to_owned(), |f| format!("now {f}"));
                let why = format!(
                    "the base branch {} of origin is {now}, not the approved {}, so the PR would differ from the review; the commit {sha} stays",
                    pr.base, pr.base_tip
                );
                return Ok(stale(op, why));
            }
            Err(why) => {
                let step = if op.reference.is_some() { "pr" } else { "push" };
                return fail(dir, op, step, format!("{why}; the commit {sha} stays"));
            }
        }
    }
    if op.reference.is_some() {
        // A resumed PR step: the remote head must still be the approved commit.
        match remote_head(exec, &pr.head).await {
            Ok(Some(found)) if found == sha => {}
            Ok(found) => {
                let now = found.map_or_else(|| "deleted".to_owned(), |f| format!("now {f}"));
                let why = format!(
                    "{} of origin is {now}, not the approved commit {sha}, so the app made no PR; the commit {sha} stays",
                    pr.head
                );
                return fail(dir, op, "pr", why);
            }
            Err(why) => return fail(dir, op, "pr", format!("{why}; the commit {sha} stays")),
        }
    } else {
        match remote_head(exec, &pr.head).await {
            Ok(Some(found)) if found == sha => {}
            Ok(found) if found.as_deref().unwrap_or_default() != pr.head_tip => {
                let was =
                    if pr.head_tip.is_empty() { "did not exist".to_owned() } else { format!("was at {}", pr.head_tip) };
                let now = found.map_or_else(|| "deleted".to_owned(), |f| format!("now at {f}"));
                let why = format!(
                    "{} of origin {was} at the review and is {now}, so the app did not push; the commit {sha} stays",
                    pr.head
                );
                return fail(dir, op, "push", why);
            }
            Ok(_) => {
                let out = run(exec, PUSH, &[&sha, &pr.head], b"", true, "git push").await;
                match out {
                    Ok(out) if out.code == 0 => {}
                    Ok(out) => {
                        return fail(
                            dir,
                            op,
                            "push",
                            format!("{}; the commit {sha} stays", script_error("git push", &out)),
                        );
                    }
                    Err(why) => return fail(dir, op, "push", format!("{why}; the commit {sha} stays")),
                }
            }
            Err(why) => return fail(dir, op, "push", format!("{why}; the commit {sha} stays")),
        }
        op.reference = Some(pr.head.clone());
        op.state = State::Pushed;
        op.step = None;
        op.detail = None;
        append(dir, &op, None)?;
    }
    let pushed = format!("the commit {sha} is on {} of origin", pr.head);
    let found = match find_pr(exec, ctx, pr).await {
        Ok(found) => found,
        Err(why) => return fail(dir, op, "pr", format!("{why}; {pushed}")),
    };
    let url = match found {
        Some(url) => url,
        None => match create_pr(exec, ctx, pr).await {
            Ok(url) => url,
            Err(why) => return fail(dir, op, "pr", format!("{why}; {pushed}")),
        },
    };
    op.url = Some(url);
    op.state = State::Pr;
    op.step = None;
    op.detail = None;
    append(dir, &op, None)?;
    Ok(op)
}

fn stale(mut op: Operation, why: String) -> Operation {
    op.state = State::Stale;
    // After a commit the files are clean, so a new review cannot recreate the operation.
    let next = if op.sha.is_some() { "push and open the PR outside zwinger" } else { "review again" };
    op.detail = Some(format!("{why}; {next}"));
    op
}

fn fail(dir: &Path, mut op: Operation, step: &str, why: String) -> std::result::Result<Operation, String> {
    op.state = State::Failed;
    op.step = Some(step.to_owned());
    op.detail = Some(why);
    append(dir, &op, None)?;
    Ok(op)
}

/// The operations of the run that have an effect or an unknown result and
/// are not finished: a retry with their token continues them. A failure
/// before a known commit changed nothing, so it is not in the list.
fn unfinished(dir: &Path) -> Vec<Operation> {
    let mut out: Vec<Operation> = journal(dir)
        .into_values()
        .map(|r| r.last)
        .filter(|op| match op.state {
            State::Pr | State::Stale => false,
            State::Committed => op.kind == Kind::Pr,
            State::Failed => op.sha.is_some(),
            State::Planned | State::Pushed => true,
        })
        .collect();
    out.sort_by(|a, b| a.token.cmp(&b.token));
    out
}

async fn blocking_journal(dir: &Path) -> HashMap<String, Record> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || journal(&dir)).await.unwrap_or_default()
}

/// Makes the commit of the plan, or finds the commit of an earlier try.
/// The error has the commit if it exists.
async fn commit<R: SandboxRuntime>(
    exec: &Exec<R>,
    plan: &Plan,
    find_only: bool,
) -> std::result::Result<String, (Option<String>, String)> {
    let input = index_info(&plan.entries, &zero_of(plan));
    let flag = if find_only { "find" } else { "" };
    let out = run(
        exec,
        &format!("{TREE}{COMMIT}"),
        &[&plan.head, &plan.branch, &plan.message, flag],
        &input,
        false,
        "the commit",
    )
    .await
    .map_err(|why| (None, why))?;
    let sha =
        String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("committed ").map(str::to_owned));
    match out.code {
        0 => sha.ok_or((None, "the commit gave no id".to_owned())),
        10 | 11 => Err((None, "stale: HEAD or the branch changed since the review".to_owned())),
        12 => Err((sha, format!("the commit is made, but {}", script_error("the index update", &out)))),
        _ => Err((sha, script_error("the commit", &out))),
    }
}

async fn remote_head<R: SandboxRuntime>(exec: &Exec<R>, head: &str) -> std::result::Result<Option<String>, String> {
    let out = run(exec, LS_REMOTE, &[head], b"", true, "git ls-remote").await?;
    if out.code != 0 {
        return Err(script_error("git ls-remote", &out));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().find_map(|l| l.split_once('\t').map(|(sha, _)| sha.to_owned())))
}

async fn api<R: SandboxRuntime>(
    exec: &Exec<R>,
    method: &str,
    url: &str,
    body: &[u8],
) -> std::result::Result<(u16, Value), String> {
    let out = run(exec, API, &[method, url], body, true, "the GitHub request").await?;
    if out.code != 0 {
        return Err(script_error("the GitHub request", &out));
    }
    let found = frames(&out.stdout).ok_or("the GitHub request gave damaged output")?;
    let code = String::from_utf8_lossy(frame(&found, "code")).trim().parse().unwrap_or(0);
    Ok((code, serde_json::from_slice(frame(&found, "body")).unwrap_or(Value::Null)))
}

/// The open PR that this operation opened earlier, by its URL: the same
/// head, base, title and body. Another PR of the head branch does not count.
async fn find_pr<R: SandboxRuntime>(
    exec: &Exec<R>,
    ctx: &Context,
    pr: &PrPlan,
) -> std::result::Result<Option<String>, String> {
    let url = format!(
        "{}/repos/{}/{}/pulls?state=open&head={}:{}",
        ctx.api,
        pr.owner,
        pr.repo,
        encode(&pr.owner),
        encode(&pr.head)
    );
    let (code, body) = api(exec, "GET", &url, b"").await?;
    if code != 200 {
        return Err(format!("GitHub answered {code} to the PR lookup: {}", body["message"].as_str().unwrap_or("")));
    }
    let approved = |p: &&Value| {
        p["state"] == "open"
            && p["head"]["ref"] == pr.head.as_str()
            && p["base"]["ref"] == pr.base.as_str()
            && p["title"] == pr.title.as_str()
            && p["body"].as_str().unwrap_or_default() == pr.body
    };
    Ok(body.as_array().and_then(|a| a.iter().find(approved)).and_then(|p| p["html_url"].as_str()).map(str::to_owned))
}

async fn create_pr<R: SandboxRuntime>(
    exec: &Exec<R>,
    ctx: &Context,
    pr: &PrPlan,
) -> std::result::Result<String, String> {
    let url = format!("{}/repos/{}/{}/pulls", ctx.api, pr.owner, pr.repo);
    let body = json!({ "title": pr.title, "body": pr.body, "head": pr.head, "base": pr.base }).to_string();
    let (code, reply) = api(exec, "POST", &url, body.as_bytes()).await?;
    match reply["html_url"].as_str() {
        Some(url) if code == 201 => Ok(url.to_owned()),
        _ => Err(format!("GitHub answered {code} to the PR creation: {}", reply["message"].as_str().unwrap_or(""))),
    }
}

/// The cage sends a GitHub credential: an Allow rule for the GitHub service
/// with a credential, and the cage is online.
fn has_github(policy: &NetworkPolicy) -> bool {
    !policy.offline
        && policy.rules.iter().any(|r| {
            r.action == Action::Allow
                && matches!(&r.target, Target::Service { id } if id == "github")
                && !r.credentials.is_empty()
        })
}

impl<R: SandboxRuntime> Manager<R> {
    /// The facts of a shipped run of this app and the exec in its cage.
    /// An `Err` names why no git action is available.
    async fn git_context(&self, id: &str, task_id: &str, run: &str) -> Result<(Exec<R>, Context)> {
        let dir = self.round_dir(id, task_id, run)?;
        if self.runs.find(id, task_id, run).is_some_and(|r| r.status().state.is_live()) {
            return Err(Error::RunGit("the run is still live".to_owned()));
        }
        let read = dir.clone();
        let (room, record, start, status) = blocking(move || {
            let record = read_record(&read, task_recovery::RUN)
                .ok()
                .flatten()
                .and_then(|b| serde_json::from_slice::<RunRecord>(&b).ok());
            let start = read_record(&read, START).ok().flatten().and_then(|b| serde_json::from_slice::<Repo>(&b).ok());
            let status =
                read_record(&read, START_STATUS).ok().flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
            Ok((task_artifacts::read_room(&read), record, start, status))
        })
        .await?;
        if room.ended.as_deref() != Some("done") || !room.outcome.as_ref().is_some_and(|o| o.ship) {
            return Err(Error::RunGit("only a run that ended with ship can commit".to_owned()));
        }
        let unknown = "this run did not record its repository at the start (state at start unknown)";
        let record = record.ok_or_else(|| Error::RunGit("run.json is missing or damaged".to_owned()))?;
        let origin = record.origin.clone().ok_or_else(|| Error::RunGit(unknown.to_owned()))?;
        let start = start.ok_or_else(|| Error::RunGit(unknown.to_owned()))?;
        let slug = lock(&self.state).session(id)?.task(task_id)?.slug.clone();
        let (exec, now, github) = {
            let state = lock(&self.state);
            let (member, cage, running, folder, _) = match state.task_cage(id) {
                Err(Error::NotRunning) => return Err(Error::RunGit("start the cage".to_owned())),
                found => found?,
            };
            let Vm::Running { vm, host, .. } = running else { unreachable!("task_cage returns a running VM") };
            let task = record.task(task_id, &slug, in_guest_folder(&folder, &record.workdir));
            let now = git_scope(&task, &folder, &cage.mounts)
                .ok()
                .and_then(|scope| Origin::of(&task.workdir, &scope, &folder, &member.folder, &cage.mounts));
            let exec = Exec {
                runtime: self.runtime.clone(),
                vm: vm.clone(),
                cwd: origin.workdir.clone(),
                scope: origin.scope.clone(),
                artifacts: Some(guest_artifacts(&folder, &slug, run)),
                pi_host: false,
                gemini: None,
                proxy: host.proxy.clone(),
                state: None,
            };
            (exec, now, has_github(&cage.network))
        };
        if now.as_ref() != Some(&origin) {
            return Err(Error::RunGit(
                "the folder of the run is not mapped into the VM as at the start of the run".to_owned(),
            ));
        }
        let ctx = Context {
            run: format!("{id}/{task_id}/{run}"),
            slug,
            run_name: run.to_owned(),
            origin,
            start,
            start_status: status,
            github,
            api: GITHUB_API.to_owned(),
            dir,
        };
        Ok((exec, ctx))
    }

    pub async fn review_run_git(&self, id: &str, task_id: &str, run: &str, request: Request) -> Result<Review> {
        let (exec, ctx) = self.git_context(id, task_id, run).await?;
        review(&exec, &ctx, &request).await.map(|(r, _)| r).map_err(Error::RunGit)
    }

    pub async fn apply_run_git(
        &self,
        id: &str,
        task_id: &str,
        run: &str,
        request: Request,
        token: &str,
    ) -> Result<Operation> {
        let (exec, ctx) = self.git_context(id, task_id, run).await?;
        apply(&exec, &ctx, &request, token).await.map_err(Error::RunGit)
    }

    /// The recorded state of an approved operation of the run.
    pub async fn run_git_operation(
        &self,
        id: &str,
        task_id: &str,
        run: &str,
        token: &str,
    ) -> Result<Option<Operation>> {
        let dir = self.round_dir(id, task_id, run)?;
        Ok(blocking_journal(&dir).await.remove(token).map(|r| r.last))
    }

    /// The unfinished operations of the run, from its journal.
    pub async fn run_git_unfinished(&self, id: &str, task_id: &str, run: &str) -> Result<Vec<Operation>> {
        let dir = self.round_dir(id, task_id, run)?;
        Ok(tokio::task::spawn_blocking(move || unfinished(&dir)).await.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests;
