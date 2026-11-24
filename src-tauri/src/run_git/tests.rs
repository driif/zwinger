//! The scripts run on owned synthetic repositories in the temp dir, with a
//! synthetic identity and no global or system git config. `origin` is an
//! HTTPS GitHub URL. A fake `git-remote-https` in the fixture's exec path
//! sends it to a local bare repository, so the URL that git reports is the
//! real one. A fake `curl` plays the GitHub API. Nothing reaches the network.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::runtime::fake::{FakePipe, FakeRuntime, FakeVm};
use crate::runtime::{PipeEvent, PipeInput};

const URL: &str = "https://github.com/o/r.git";

/// The GitHub API: `list` holds the PRs that a lookup gives, `posts` counts
/// the creations, and `post_fail` makes a creation fail. A creation sets
/// `list` to the one new open PR, made from the posted title, body and refs.
const FAKE_CURL: &str = r#"#!/bin/sh
out= method=GET url= data=
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift ;;
    -X) method=$2; shift ;;
    --data-binary) data=${2#@}; shift ;;
    -w|-H|--max-time) shift ;;
    -*) ;;
    *) url=$1 ;;
  esac
  shift
done
if [ "$method" = POST ]; then
  echo x >> "$FAKE_GH/posts"
  if [ -e "$FAKE_GH/post_fail" ]; then
    printf '{"message":"Validation Failed"}' > "$out"; printf 422; exit 0
  fi
  { printf '['; sed -e 's/^{/{"html_url":"https:\/\/github.com\/o\/r\/pull\/1","state":"open",/' \
    -e 's/"head":\("[^"]*"\)/"head":{"ref":\1}/' -e 's/"base":\("[^"]*"\)/"base":{"ref":\1}/' "$data"; printf ']'; } > "$FAKE_GH/list"
  printf '{"html_url":"https://github.com/o/r/pull/1"}' > "$out"; printf 201; exit 0
fi
if [ -e "$FAKE_GH/list" ]; then cat "$FAKE_GH/list" > "$out"; else printf '[]' > "$out"; fi
printf 200
"#;

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    bare: PathBuf,
    gh: PathBuf,
    exec: Exec<FakeRuntime>,
    ctx: Context,
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zwinger-run-git-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// Sends the HTTPS transport of git to `$ZW_BARE` through `git remote-ext`.
const FAKE_REMOTE_HTTPS: &str = "#!/bin/sh\nexec git remote-ext \"$1\" \"git %s $ZW_BARE\"\n";

/// A copy of the git exec path whose `git-remote-https` is `FAKE_REMOTE_HTTPS`.
fn fake_exec_path(root: &Path) {
    let real = Command::new("git").arg("--exec-path").output().unwrap();
    let real = PathBuf::from(String::from_utf8(real.stdout).unwrap().trim());
    let dir = root.join("git-exec");
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(&real).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "git-remote-https" {
            std::os::unix::fs::symlink(entry.path(), dir.join(entry.file_name())).unwrap();
        }
    }
    std::fs::write(dir.join("git-remote-https"), FAKE_REMOTE_HTTPS).unwrap();
    std::fs::set_permissions(dir.join("git-remote-https"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn isolated(command: &mut Command, root: &Path) {
    command
        .env("GIT_EXEC_PATH", root.join("git-exec"))
        .env("ZW_BARE", root.join("bare.git"))
        .env("HOME", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
}

fn git(f: &Fixture, args: &[&str]) -> String {
    git_in(&f.root, &f.repo, args)
}

fn git_in(root: &Path, dir: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    isolated(&mut command, root);
    let out = command.output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
}

fn write(f: &Fixture, path: &str, text: &str) {
    let path = f.repo.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Plays each pipe of the runtime as a host process with the fixture's env.
fn serve(runtime: &FakeRuntime, root: PathBuf, path: OsString, gh: PathBuf) {
    let pipes = runtime.pipes.clone();
    tokio::spawn(async move {
        loop {
            let opened: Vec<FakePipe> = std::mem::take(&mut *pipes.lock().unwrap());
            for pipe in opened {
                tokio::spawn(play(pipe, root.clone(), path.clone(), gh.clone()));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
}

async fn play(mut pipe: FakePipe, root: PathBuf, path: OsString, gh: PathBuf) {
    let mut input = Vec::new();
    while let Some(msg) = pipe.stdin.recv().await {
        match msg {
            PipeInput::Data(data) => input.extend(data),
            PipeInput::Close => break,
            PipeInput::Kill => return,
        }
    }
    let argv = pipe.argv.clone();
    let out = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).env("PATH", path).env("FAKE_GH", gh);
        isolated(&mut command, &root);
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    let _ = pipe.out.send(PipeEvent::Stdout(out.stdout)).await;
    let _ = pipe.out.send(PipeEvent::Stderr(out.stderr)).await;
    let _ = pipe.out.send(PipeEvent::Exit(out.status.code().unwrap_or(-1))).await;
}

/// A repository on `main` with one commit, pushed to the bare `origin`.
async fn fixture(name: &str) -> Fixture {
    fixture_with(name, true).await
}

async fn fixture_with(name: &str, curl: bool) -> Fixture {
    let root = temp(name);
    fake_exec_path(&root);
    let (repo, bare, gh, bin) = (root.join("repo"), root.join("bare.git"), root.join("gh"), root.join("bin"));
    for dir in [&repo, &gh, &bin] {
        std::fs::create_dir_all(dir).unwrap();
    }
    if curl {
        std::fs::write(bin.join("curl"), FAKE_CURL).unwrap();
        std::fs::set_permissions(bin.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git_in(&root, &root, &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
    git_in(&root, &repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.invalid"),
        ("remote.origin.url", URL),
        ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
    ] {
        git_in(&root, &repo, &["config", key, value]);
    }
    std::fs::write(repo.join("a.txt"), "a1\n").unwrap();
    std::fs::write(repo.join("c.txt"), "c1\n").unwrap();
    std::fs::write(repo.join("d.txt"), "d1\n").unwrap();
    git_in(&root, &repo, &["add", "."]);
    git_in(&root, &repo, &["commit", "-q", "-m", "first"]);
    git_in(&root, &repo, &["push", "-q", "origin", "main"]);
    // Remove the samples, so that only a hook of a test counts.
    let _ = std::fs::remove_dir_all(repo.join(".git/hooks"));

    let runtime = FakeRuntime::default();
    let system = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths([bin].into_iter().chain(std::env::split_paths(&system))).unwrap();
    serve(&runtime, root.clone(), path, gh.clone());
    let exec = Exec {
        runtime,
        vm: Arc::new(FakeVm { key: "k".into() }),
        cwd: repo.display().to_string(),
        scope: repo.display().to_string(),
        artifacts: None,
        pi_host: false,
        gemini: None,
        proxy: None,
        runtimes: crate::runtimes::fake_installed().to_vec(),
        placeholders: Vec::new(),
        state: None,
    };
    let dir = root.join("round");
    std::fs::create_dir_all(&dir).unwrap();
    let (_keep, mut cancel) = watch::channel(false);
    let mut start = start_identity(&exec, &mut cancel).await.expect("the start identity");
    start.base = start.head.clone();
    let origin = Origin {
        host: repo.display().to_string(),
        guest: repo.display().to_string(),
        workdir: repo.display().to_string(),
        scope: repo.display().to_string(),
    };
    let ctx = Context {
        run: "s/t/01".into(),
        slug: "fix".into(),
        run_name: "01".into(),
        origin,
        start,
        start_status: Some(format!("HEAD {}\n", git_in(&root, &repo, &["rev-parse", "HEAD"]))),
        github: true,
        api: "https://api.example.invalid".into(),
        dir,
    };
    Fixture { root, repo, bare, gh, exec, ctx }
}

fn commit_request(selection: Option<&[&str]>) -> Request {
    Request {
        kind: Some(Kind::Commit),
        selection: selection.map(|s| s.iter().map(|p| (*p).to_owned()).collect()),
        message: "Fix the map\n\nThe body.".into(),
        ..Request::default()
    }
}

fn pr_request(selection: &[&str]) -> Request {
    Request {
        kind: Some(Kind::Pr),
        title: "Fix the map".into(),
        body: "Why.".into(),
        ..commit_request(Some(selection))
    }
}

async fn approved(f: &Fixture, request: &Request) -> (Review, String) {
    let (review, _) = review(&f.exec, &f.ctx, request).await.unwrap();
    let token = review.token.clone().unwrap_or_else(|| panic!("blocked: {:?}", review.blocked));
    (review, token)
}

fn file<'a>(review: &'a Review, path: &str) -> &'a File {
    review.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} in {:?}", review.files))
}

fn show(f: &Fixture, object: &str) -> String {
    git(f, &["show", object])
}

#[tokio::test]
async fn a_commit_takes_the_reviewed_bytes_of_the_selected_files_only() {
    let f = fixture("selected").await;
    let head = git(&f, &["rev-parse", "HEAD"]);
    write(&f, "a.txt", "a2\n");
    write(&f, "new/b.txt", "b\n");
    write(&f, "c.txt", "c2\n");
    write(&f, "d.txt", "d2\n");
    git(&f, &["add", "d.txt"]);
    write(&f, "d.txt", "d3\n");

    let (review, token) = approved(&f, &commit_request(Some(&["a.txt", "new/b.txt"]))).await;
    assert_eq!(file(&review, "a.txt").kind, "modified");
    assert_eq!(file(&review, "new/b.txt").kind, "untracked");
    assert!(file(&review, "a.txt").diff.contains("+a2"), "{}", file(&review, "a.txt").diff);
    assert!(!file(&review, "c.txt").selected && file(&review, "c.txt").diff.is_empty());
    assert!(file(&review, "d.txt").staged);
    assert!(review.notes.iter().any(|n| n.contains("not signed")));
    assert_eq!(review.repo.branch, "refs/heads/main");

    let op = apply(&f.exec, &f.ctx, &commit_request(Some(&["a.txt", "new/b.txt"])), &token).await.unwrap();
    assert_eq!(op.state, State::Committed, "{op:?}");
    let sha = op.sha.clone().unwrap();
    assert_eq!(git(&f, &["rev-parse", "HEAD"]), sha);
    assert_eq!(git(&f, &["rev-parse", "HEAD^"]), head);
    assert_eq!(show(&f, "HEAD:a.txt"), "a2");
    assert_eq!(show(&f, "HEAD:new/b.txt"), "b");
    assert_eq!(show(&f, "HEAD:c.txt"), "c1");
    assert_eq!(show(&f, "HEAD:d.txt"), "d1");
    assert_eq!(git(&f, &["log", "-1", "--format=%B"]), "Fix the map\n\nThe body.");
    // Unselected work stays: c unstaged, d staged and changed again.
    let status = git(&f, &["status", "--porcelain=v1"]);
    assert_eq!(status, " M c.txt\nMM d.txt", "{status}");
    assert_eq!(show(&f, ":d.txt"), "d2");

    // A repeated click returns the recorded commit and makes no second one.
    let again = apply(&f.exec, &f.ctx, &commit_request(Some(&["a.txt", "new/b.txt"])), &token).await.unwrap();
    assert_eq!(again, op);
    assert_eq!(git(&f, &["rev-parse", "HEAD"]), sha);
    assert_eq!(run_git_operation_of(&f, &token).unwrap().sha.as_deref(), Some(sha.as_str()));
}

fn run_git_operation_of(f: &Fixture, token: &str) -> Option<Operation> {
    journal(&f.ctx.dir).remove(token).map(|r| r.last)
}

#[tokio::test]
async fn the_review_covers_the_scope_and_skips_the_round_copies() {
    let mut f = fixture("scope").await;
    write(&f, "sub/in.txt", "in\n");
    write(&f, "out.txt", "out\n");
    write(&f, "sub/.zwinger/tasks/fix/01/events.jsonl", "round\n");
    f.exec.scope = f.repo.join("sub").display().to_string();
    f.exec.artifacts = Some(f.repo.join("sub/.zwinger/tasks/fix/01").display().to_string());
    let (review, _) = review(&f.exec, &f.ctx, &commit_request(None)).await.unwrap();
    let paths: Vec<&str> = review.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["sub/in.txt"]);
    // A path outside the scope cannot be selected.
    let (review, _) = review_of(&f, &commit_request(Some(&["out.txt"]))).await;
    assert!(review.token.is_none() && review.blocked.iter().any(|b| b.contains("out.txt has no change")));
}

async fn review_of(f: &Fixture, request: &Request) -> (Review, Option<Plan>) {
    review(&f.exec, &f.ctx, request).await.unwrap()
}

#[tokio::test]
async fn deletes_renames_and_new_files_commit_and_a_binary_blocks() {
    let f = fixture("kinds").await;
    std::fs::remove_file(f.repo.join("c.txt")).unwrap();
    git(&f, &["mv", "d.txt", "moved.txt"]);
    write(&f, "new.txt", "new\n");
    std::fs::write(f.repo.join("blob.bin"), [0u8, 1, 2, 0, 255]).unwrap();

    let (review, _) = review_of(&f, &commit_request(None)).await;
    assert_eq!(file(&review, "c.txt").kind, "deleted");
    let moved = file(&review, "moved.txt");
    assert_eq!((moved.kind, moved.from.as_deref()), ("renamed", Some("d.txt")));
    assert!(file(&review, "blob.bin").binary);
    assert!(review.token.is_none());
    assert!(review.blocked.iter().any(|b| b.contains("blob.bin is binary")), "{:?}", review.blocked);

    let request = commit_request(Some(&["c.txt", "moved.txt", "new.txt"]));
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(op.state, State::Committed, "{op:?}");
    let tree = git(&f, &["ls-tree", "--name-only", "HEAD"]);
    assert_eq!(tree, "a.txt\nmoved.txt\nnew.txt");
    assert_eq!(git(&f, &["status", "--porcelain=v1"]), "?? blob.bin");
}

#[tokio::test]
async fn changes_from_before_the_run_are_marked_and_partly_staged_work_stays() {
    let mut f = fixture("pre").await;
    write(&f, "a.txt", "a-before\n");
    write(&f, "c.txt", "c2\n");
    git(&f, &["add", "c.txt"]);
    write(&f, "c.txt", "c3\n");
    write(&f, "d.txt", "d2\n");
    git(&f, &["add", "d.txt"]);
    write(&f, "d.txt", "d3\n");
    f.ctx.start_status = Some("HEAD x\n M a.txt\n".to_owned());

    let request = commit_request(Some(&["a.txt", "c.txt"]));
    let (review, token) = approved(&f, &request).await;
    assert_eq!(file(&review, "a.txt").dirty_at_start, Some(true));
    assert_eq!(file(&review, "c.txt").dirty_at_start, Some(false));
    assert!(review.notes.iter().any(|n| n.starts_with("a.txt was already changed")), "{:?}", review.notes);
    assert!(file(&review, "c.txt").diff.contains("+c3"));

    apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(show(&f, "HEAD:c.txt"), "c3");
    assert_eq!(show(&f, "HEAD:a.txt"), "a-before");
    // The unselected, partly staged file keeps its index and its worktree.
    assert_eq!(show(&f, ":d.txt"), "d2");
    assert_eq!(std::fs::read_to_string(f.repo.join("d.txt")).unwrap(), "d3\n");
    assert_eq!(git(&f, &["status", "--porcelain=v1"]), "MM d.txt");

    // A legacy start: the state at the start is not known.
    f.ctx.start_status = None;
    write(&f, "a.txt", "a3\n");
    let (review, _) = review_of(&f, &commit_request(None)).await;
    assert_eq!(file(&review, "a.txt").dirty_at_start, None);
    assert!(review.notes.iter().any(|n| n.contains("state at the start of the run is not known")));
}

#[tokio::test]
async fn an_empty_selection_or_message_cannot_be_approved() {
    let f = fixture("empty").await;
    write(&f, "a.txt", "a2\n");
    let (review, _) = review_of(&f, &commit_request(Some(&[]))).await;
    assert!(review.token.is_none() && review.blocked.iter().any(|b| b.contains("select at least one file")));
    let mut request = commit_request(None);
    request.message = "  ".into();
    let (review, _) = review_of(&f, &request).await;
    assert!(review.token.is_none() && review.blocked.iter().any(|b| b.contains("commit message")));
}

#[tokio::test]
async fn a_change_after_the_review_makes_the_approval_stale() {
    let f = fixture("stale").await;
    let head = git(&f, &["rev-parse", "HEAD"]);
    write(&f, "a.txt", "a2\n");
    let request = commit_request(Some(&["a.txt"]));

    // The worktree changes.
    let (_, token) = approved(&f, &request).await;
    write(&f, "a.txt", "a3\n");
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(op.state, State::Stale, "{op:?}");
    assert_eq!(git(&f, &["rev-parse", "HEAD"]), head);

    // The index of a selected file changes.
    let (_, token) = approved(&f, &request).await;
    git(&f, &["add", "a.txt"]);
    assert_eq!(apply(&f.exec, &f.ctx, &request, &token).await.unwrap().state, State::Stale);

    // HEAD moves.
    let (_, token) = approved(&f, &request).await;
    write(&f, "c.txt", "c2\n");
    git(&f, &["commit", "-q", "-m", "concurrent", "--", "c.txt"]);
    assert_eq!(apply(&f.exec, &f.ctx, &request, &token).await.unwrap().state, State::Stale);
    assert_eq!(git(&f, &["log", "-1", "--format=%s"]), "concurrent");

    // A message change is a different approval.
    let (_, token) = approved(&f, &request).await;
    let mut other = request.clone();
    other.message = "Other".into();
    assert_eq!(apply(&f.exec, &f.ctx, &other, &token).await.unwrap().state, State::Stale);
}

#[tokio::test]
async fn a_moved_branch_between_the_check_and_the_commit_is_stale() {
    let f = fixture("cas").await;
    write(&f, "a.txt", "a2\n");
    let (_, plan) = review_of(&f, &commit_request(Some(&["a.txt"]))).await;
    let plan = plan.unwrap();
    write(&f, "c.txt", "c2\n");
    git(&f, &["commit", "-q", "-am", "concurrent"]);
    let (sha, why) = commit(&f.exec, &plan, false).await.unwrap_err();
    assert!(sha.is_none() && why.starts_with("stale:"), "{why}");
    assert_eq!(git(&f, &["log", "-1", "--format=%s"]), "concurrent");
}

#[tokio::test]
async fn a_crash_after_the_commit_recovers_it_and_repairs_the_index() {
    let f = fixture("crash").await;
    write(&f, "a.txt", "a2\n");
    write(&f, "c.txt", "c2\n");
    git(&f, &["add", "c.txt"]);
    let request = commit_request(Some(&["a.txt"]));
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    let sha = op.sha.unwrap();
    // The crash: only the planned line is durable, and the real index
    // still has the old a.txt.
    let journal = std::fs::read_to_string(f.ctx.dir.join(OPERATIONS)).unwrap();
    std::fs::write(f.ctx.dir.join(OPERATIONS), format!("{}\n", journal.lines().next().unwrap())).unwrap();
    git(&f, &["reset", "-q", "HEAD^", "--", "a.txt"]);
    assert_eq!(show(&f, ":a.txt"), "a1");

    let again = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((again.state, again.sha.as_deref()), (State::Committed, Some(sha.as_str())), "{again:?}");
    assert_eq!(git(&f, &["rev-parse", "HEAD"]), sha);
    assert_eq!(show(&f, ":a.txt"), "a2");
    // The unselected staged file is untouched.
    assert_eq!(show(&f, ":c.txt"), "c2");
}

#[tokio::test]
async fn the_review_refuses_hooks_signing_filters_and_a_missing_identity() {
    let f = fixture("config").await;
    write(&f, "a.txt", "a2\n");
    for (key, value, expect) in [
        ("commit.gpgsign", "true", "commit.gpgsign"),
        ("filter.lfs.clean", "git-lfs clean -- %f", "filter.lfs.clean"),
        ("core.hooksPath", ".githooks", "core.hookspath"),
        ("core.fsmonitor", "true", "core.fsmonitor"),
        ("credential.helper", "store", "credential.helper"),
        ("gpg.program", "gpg2", "gpg.program"),
    ] {
        git(&f, &["config", key, value]);
        let (review, _) = review_of(&f, &commit_request(None)).await;
        assert!(
            review.token.is_none() && review.blocked.iter().any(|b| b.contains(expect)),
            "{key}: {:?}",
            review.blocked
        );
        git(&f, &["config", "--unset", key]);
    }
    git(&f, &["config", "commit.gpgsign", "false"]);
    approved(&f, &commit_request(None)).await;

    let hooks = f.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("reference-transaction"), "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(hooks.join("reference-transaction"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let (review, _) = review_of(&f, &commit_request(None)).await;
    assert!(review.blocked.iter().any(|b| b.contains("hook reference-transaction")), "{:?}", review.blocked);
    std::fs::remove_file(hooks.join("reference-transaction")).unwrap();

    git(&f, &["config", "--unset", "user.email"]);
    let (review, _) = review_of(&f, &commit_request(None)).await;
    assert!(review.blocked.iter().any(|b| b.contains("no user.name or user.email")), "{:?}", review.blocked);
}

#[tokio::test]
async fn a_different_repository_or_a_read_only_store_is_unavailable() {
    let f = fixture("identity").await;
    write(&f, "a.txt", "a2\n");
    git(&f, &["config", "remote.origin.url", "https://github.com/o/other.git"]);
    let error = review(&f.exec, &f.ctx, &commit_request(None)).await.unwrap_err();
    assert!(error.contains("origin changed"), "{error}");
    git(&f, &["config", "remote.origin.url", URL]);

    git(&f, &["checkout", "-q", "-b", "other"]);
    let error = review(&f.exec, &f.ctx, &commit_request(None)).await.unwrap_err();
    assert!(error.contains("the branch changed"), "{error}");
    git(&f, &["checkout", "-q", "main"]);

    // The cage cannot write the object store.
    let objects = f.repo.join(".git/objects");
    let set = |mode| {
        for entry in std::fs::read_dir(&objects).unwrap().flatten() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    set(0o555);
    let result = review(&f.exec, &f.ctx, &commit_request(None)).await;
    set(0o755);
    // Root can write anyway; then the review is complete.
    if std::fs::write(objects.join("probe"), "").is_err() {
        assert!(result.unwrap_err().contains("failed"));
    }
    let _ = std::fs::remove_file(objects.join("probe"));

    // No git in the cage.
    let mut exec = Exec { runtime: FakeRuntime::default(), ..clone_exec(&f.exec) };
    serve(&exec.runtime, f.root.clone(), f.root.join("empty-path").into_os_string(), f.gh.clone());
    exec.scope = f.repo.display().to_string();
    let error = review(&exec, &f.ctx, &commit_request(None)).await.unwrap_err();
    assert!(error.contains("the git check"), "{error}");
}

fn clone_exec(exec: &Exec<FakeRuntime>) -> Exec<FakeRuntime> {
    Exec {
        runtime: exec.runtime.clone(),
        vm: exec.vm.clone(),
        cwd: exec.cwd.clone(),
        scope: exec.scope.clone(),
        artifacts: exec.artifacts.clone(),
        pi_host: false,
        gemini: None,
        proxy: None,
        runtimes: crate::runtimes::fake_installed().to_vec(),
        placeholders: Vec::new(),
        state: None,
    }
}

#[tokio::test]
async fn a_pr_pushes_a_new_branch_and_opens_one_pr() {
    let f = fixture("pr").await;
    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);
    let (review, token) = approved(&f, &request).await;
    let pr = review.pr.clone().unwrap();
    assert_eq!((pr.owner.as_str(), pr.repo.as_str(), pr.base.as_str()), ("o", "r", "main"));
    assert_eq!((pr.head.as_str(), pr.new_branch, pr.head_tip.as_str()), ("zwinger/fix-01", true, ""));
    assert_eq!(pr.base_tip, git(&f, &["rev-parse", "HEAD"]));
    assert_eq!(pr.commits, ["(new) Fix the map"]);
    assert!(pr.diff.contains("+a2"), "{}", pr.diff);

    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(op.state, State::Pr, "{op:?}");
    assert_eq!(op.url.as_deref(), Some("https://github.com/o/r/pull/1"));
    let sha = op.sha.clone().unwrap();
    assert_eq!(git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]), sha);
    // The commit goes onto the current local branch; no local branch is
    // created or switched.
    assert_eq!(git(&f, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(git(&f, &["rev-parse", "refs/heads/main"]), sha);
    assert_eq!(git(&f, &["for-each-ref", "--format=%(refname)", "refs/heads/"]), "refs/heads/main");

    let again = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(again, op);
    assert_eq!(std::fs::read_to_string(f.gh.join("posts")).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn a_failed_pr_creation_keeps_the_push_and_a_retry_opens_it_once() {
    let f = fixture("pr-fail").await;
    write(&f, "a.txt", "a2\n");
    std::fs::write(f.gh.join("post_fail"), "").unwrap();
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("pr")), "{op:?}");
    let sha = op.sha.clone().unwrap();
    assert_eq!(op.reference.as_deref(), Some("zwinger/fix-01"));
    assert!(op.detail.as_deref().unwrap().contains(&sha), "{op:?}");
    assert_eq!(git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]), sha);

    std::fs::remove_file(f.gh.join("post_fail")).unwrap();
    let retry = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(retry.state, State::Pr, "{retry:?}");
    assert_eq!(retry.sha.as_deref(), Some(sha.as_str()));
    assert_eq!(git(&f, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(std::fs::read_to_string(f.gh.join("posts")).unwrap().lines().count(), 2);
}

#[tokio::test]
async fn a_rejected_push_keeps_the_commit_and_never_forces() {
    let f = fixture("push-fail").await;
    // The head branch of origin already has other work.
    git(&f, &["push", "-q", "origin", "main:refs/heads/zwinger/fix-01"]);
    write(&f, "c.txt", "remote\n");
    git(&f, &["commit", "-q", "-am", "remote work"]);
    git(&f, &["push", "-q", "origin", "main:refs/heads/zwinger/fix-01"]);
    git(&f, &["reset", "-q", "--hard", "HEAD^"]);
    let remote = git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]);

    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("push")), "{op:?}");
    let sha = op.sha.clone().unwrap();
    assert!(op.detail.as_deref().unwrap().contains("stays"), "{op:?}");
    assert_eq!(git(&f, &["rev-parse", "HEAD"]), sha);
    assert_eq!(git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]), remote);
    assert!(!f.gh.join("posts").exists());
}

#[tokio::test]
async fn a_pr_needs_a_github_credential_an_https_remote_and_curl() {
    let mut f = fixture("pr-needs").await;
    write(&f, "a.txt", "a2\n");
    f.ctx.github = false;
    let (review, _) = review_of(&f, &pr_request(&["a.txt"])).await;
    assert!(review.blocked.iter().any(|b| b.contains("no Allow rule for GitHub")), "{:?}", review.blocked);
    f.ctx.github = true;

    assert!(github_repo("git@github.com:o/r.git").unwrap_err().contains("does not use HTTPS"));
    assert!(github_repo("").unwrap_err().contains("no remote"));
    assert_eq!(github_repo("https://github.com/o/r").unwrap(), ("o".into(), "r".into()));

    // No curl in the cage: the commit and the push stay, the PR step names the tool.
    let f = fixture_with("pr-curl", false).await;
    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    let tools = f.root.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let system = std::env::var_os("PATH").unwrap();
    for tool in ["git", "sh", "mktemp", "wc", "tr", "cat", "xargs", "readlink", "rm", "printf"] {
        if let Some(dir) = std::env::split_paths(&system).find(|d| d.join(tool).exists()) {
            std::os::unix::fs::symlink(dir.join(tool), tools.join(tool)).unwrap();
        }
    }
    let exec = Exec { runtime: FakeRuntime::default(), ..clone_exec(&f.exec) };
    serve(&exec.runtime, f.root.clone(), tools.into_os_string(), f.gh.clone());
    let op = apply(&exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("pr")), "{op:?}");
    assert!(op.detail.as_deref().unwrap().contains("missing"), "{op:?}");
    assert_eq!(op.reference.as_deref(), Some("zwinger/fix-01"));
}

#[test]
fn start_paths_read_the_porcelain_status() {
    let status = "HEAD abc\n M a.txt\nR  old.txt -> new.txt\n?? \"sp ace\\tx\\303\\251\"\n";
    assert_eq!(start_paths(status).unwrap(), ["a.txt", "old.txt", "new.txt", "sp ace\txé"]);
    assert!(start_paths(&"x".repeat(START_STATUS_LIMIT)).is_none());
}

#[test]
fn the_journal_keeps_the_effects_of_earlier_lines() {
    let dir = temp("journal");
    let op = |state, sha: Option<&str>, url: Option<&str>| Operation {
        token: "t".into(),
        kind: Kind::Pr,
        state,
        sha: sha.map(str::to_owned),
        reference: None,
        url: url.map(str::to_owned),
        step: None,
        detail: None,
    };
    append(&dir, &op(State::Planned, None, None), None).unwrap();
    append(&dir, &op(State::Committed, Some("abc"), None), None).unwrap();
    append(&dir, &op(State::Failed, None, None), None).unwrap();
    let record = journal(&dir).remove("t").unwrap();
    assert_eq!((record.last.state, record.last.sha.as_deref()), (State::Failed, Some("abc")));
}

#[tokio::test]
async fn a_push_destination_other_than_the_reviewed_repository_blocks_the_pr() {
    let f = fixture("pr-dest").await;
    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);
    for (key, value) in [
        ("remote.origin.pushurl", "https://github.com/evil/r.git"),
        ("url.https://github.com/evil/.pushInsteadOf", "https://github.com/o/"),
        ("url.https://github.com/evil/.insteadOf", "https://github.com/o/"),
    ] {
        git(&f, &["config", key, value]);
        let (review, _) = review_of(&f, &request).await;
        assert!(review.token.is_none());
        assert!(
            review.blocked.iter().any(|b| b.contains("github.com/evil/r.git, not to the reviewed github.com/o/r")),
            "{key}: {:?}",
            review.blocked
        );
        git(&f, &["config", "--unset", key]);
    }
    approved(&f, &request).await;
}

/// The state after a crash between the commit and the push: the plan and
/// the commit are in the journal, and nothing is pushed.
async fn committed_before_push(f: &Fixture, request: &Request) -> (String, String) {
    let (review, plan) = review_of(f, request).await;
    let token = review.token.unwrap_or_else(|| panic!("blocked: {:?}", review.blocked));
    let sha = commit(&f.exec, plan.as_ref().unwrap(), false).await.unwrap();
    let op = Operation {
        token: token.clone(),
        kind: Kind::Pr,
        state: State::Committed,
        sha: Some(sha.clone()),
        reference: None,
        url: None,
        step: None,
        detail: None,
    };
    append(&f.ctx.dir, &op, plan.as_ref()).unwrap();
    (token, sha)
}

#[tokio::test]
async fn a_retry_after_the_commit_rechecks_the_push_destination() {
    let f = fixture("pr-retry-dest").await;
    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);
    let (token, sha) = committed_before_push(&f, &request).await;

    git(&f, &["config", "remote.origin.pushurl", "https://github.com/evil/r.git"]);
    let retry = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((retry.state, retry.step.as_deref()), (State::Failed, Some("push")), "{retry:?}");
    assert!(retry.detail.as_deref().unwrap().contains("not to the reviewed"), "{retry:?}");
    assert!(retry.detail.as_deref().unwrap().contains(&sha), "{retry:?}");
    assert!(git_in(&f.root, &f.bare, &["for-each-ref", "refs/heads/zwinger/"]).is_empty());

    git(&f, &["config", "--unset", "remote.origin.pushurl"]);
    let fixed = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(fixed.state, State::Pr, "{fixed:?}");
    assert_eq!(git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]), sha);
}

#[tokio::test]
async fn a_pr_retry_refuses_a_moved_remote_head() {
    let f = fixture("pr-moved").await;
    write(&f, "a.txt", "a2\n");
    std::fs::write(f.gh.join("post_fail"), "").unwrap();
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("pr")), "{op:?}");
    let sha = op.sha.clone().unwrap();
    std::fs::remove_file(f.gh.join("post_fail")).unwrap();

    // Someone moves the head branch of origin to other work.
    let other = git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/main"]);
    git_in(&f.root, &f.bare, &["update-ref", "refs/heads/zwinger/fix-01", &other]);
    let retry = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((retry.state, retry.step.as_deref()), (State::Failed, Some("pr")), "{retry:?}");
    let detail = retry.detail.as_deref().unwrap();
    assert!(detail.contains(&format!("is now {other}, not the approved commit {sha}")), "{detail}");
    assert!(retry.url.is_none());
    assert_eq!(std::fs::read_to_string(f.gh.join("posts")).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn a_mode_change_shows_in_the_review_and_binds_the_approval() {
    let f = fixture("mode").await;
    let set = |mode| std::fs::set_permissions(f.repo.join("a.txt"), std::fs::Permissions::from_mode(mode)).unwrap();
    set(0o755);
    let request = commit_request(Some(&["a.txt"]));
    let (review, token) = approved(&f, &request).await;
    let diff = &file(&review, "a.txt").diff;
    assert!(diff.starts_with("old mode 100644\nnew mode 100755\n"), "{diff}");

    // The mode is part of the approval.
    set(0o644);
    assert_eq!(apply(&f.exec, &f.ctx, &request, &token).await.unwrap().state, State::Stale);
    set(0o755);
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(op.state, State::Committed, "{op:?}");
    assert!(git(&f, &["ls-tree", "HEAD", "a.txt"]).starts_with("100755 "));
}

#[tokio::test]
async fn the_review_discloses_an_existing_head_branch_and_the_push_needs_it_unchanged() {
    let f = fixture("pr-head").await;
    let tip = git(&f, &["rev-parse", "HEAD"]);
    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);

    // The head branch of origin exists at the review: it is not new, and its tip is in the plan.
    git(&f, &["push", "-q", "origin", "main:refs/heads/zwinger/fix-01"]);
    let (review, token) = approved(&f, &request).await;
    let pr = review.pr.unwrap();
    assert_eq!((pr.new_branch, pr.head_tip.as_str()), (false, tip.as_str()));
    // It is deleted after the review: the approval is stale.
    git_in(&f.root, &f.bare, &["update-ref", "-d", "refs/heads/zwinger/fix-01"]);
    assert_eq!(apply(&f.exec, &f.ctx, &request, &token).await.unwrap().state, State::Stale);
    assert_eq!(git(&f, &["rev-parse", "HEAD"]), tip);

    // A resumed push: the branch was new at the review and exists now, so a
    // push would fast-forward it. The app does not push.
    let (token, sha) = committed_before_push(&f, &request).await;
    git_in(&f.root, &f.bare, &["update-ref", "refs/heads/zwinger/fix-01", &tip]);
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("push")), "{op:?}");
    let detail = op.detail.as_deref().unwrap();
    assert!(detail.contains(&format!("did not exist at the review and is now at {tip}")), "{detail}");
    assert!(detail.contains(&sha), "{detail}");
    assert_eq!(git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]), tip);
    assert!(!f.gh.join("posts").exists());
}

fn pr_json(number: u32, state: &str, base: &str, title: &str) -> String {
    format!(
        r#"{{"html_url":"https://github.com/o/r/pull/{number}","state":"{state}","head":{{"ref":"zwinger/fix-01"}},"base":{{"ref":"{base}"}},"title":"{title}","body":"Why."}}"#
    )
}

#[tokio::test]
async fn the_pr_lookup_takes_only_the_open_pr_of_the_approved_plan() {
    let f = fixture("pr-lookup").await;
    write(&f, "a.txt", "a2\n");
    std::fs::write(f.gh.join("post_fail"), "").unwrap();
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("pr")), "{op:?}");

    // A closed PR, an open PR with another title and an open PR into another
    // base are not the approved PR: the retry tries to create it.
    let others = [
        pr_json(7, "closed", "main", "Fix the map"),
        pr_json(8, "open", "main", "Other"),
        pr_json(9, "open", "dev", "Fix the map"),
    ];
    std::fs::write(f.gh.join("list"), format!("[{}]", others.join(","))).unwrap();
    let retry = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((retry.state, retry.step.as_deref()), (State::Failed, Some("pr")), "{retry:?}");
    assert!(retry.url.is_none());
    assert_eq!(std::fs::read_to_string(f.gh.join("posts")).unwrap().lines().count(), 2);

    // An earlier creation whose answer was lost: the open PR of the plan counts.
    let all = [others.join(","), pr_json(10, "open", "main", "Fix the map")].join(",");
    std::fs::write(f.gh.join("list"), format!("[{all}]")).unwrap();
    let found = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((found.state, found.url.as_deref()), (State::Pr, Some("https://github.com/o/r/pull/10")), "{found:?}");
    assert_eq!(std::fs::read_to_string(f.gh.join("posts")).unwrap().lines().count(), 2);
}

#[tokio::test]
async fn a_pr_retry_after_the_base_moved_asks_for_a_new_review() {
    let f = fixture("pr-base").await;
    write(&f, "a.txt", "a2\n");
    std::fs::write(f.gh.join("post_fail"), "").unwrap();
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("pr")), "{op:?}");
    let sha = op.sha.clone().unwrap();
    std::fs::remove_file(f.gh.join("post_fail")).unwrap();

    // Someone adds a commit to main of origin.
    let moved = git_in(
        &f.root,
        &f.bare,
        &[
            "-c",
            "user.name=O",
            "-c",
            "user.email=o@example.invalid",
            "commit-tree",
            "-p",
            "main",
            "-m",
            "other",
            "main^{tree}",
        ],
    );
    git_in(&f.root, &f.bare, &["update-ref", "refs/heads/main", &moved]);
    let retry = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    assert_eq!(retry.state, State::Stale, "{retry:?}");
    let detail = retry.detail.as_deref().unwrap();
    assert!(
        detail.contains(&format!("is now {moved}")) && detail.contains("push and open the PR outside zwinger"),
        "{detail}"
    );
    assert!(detail.contains(&sha), "{detail}");
    assert!(retry.url.is_none());
    assert_eq!(std::fs::read_to_string(f.gh.join("posts")).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn a_failed_push_is_listed_as_unfinished_and_continues_without_the_review() {
    let f = fixture("unfinished").await;
    write(&f, "a.txt", "a2\n");
    let request = pr_request(&["a.txt"]);
    let (_, token) = approved(&f, &request).await;
    // origin cannot write a new branch, so the push fails after the commit.
    let heads = f.bare.join("refs/heads");
    let set = |mode| std::fs::set_permissions(&heads, std::fs::Permissions::from_mode(mode)).unwrap();
    set(0o555);
    let op = apply(&f.exec, &f.ctx, &request, &token).await.unwrap();
    set(0o755);
    assert_eq!((op.state, op.step.as_deref()), (State::Failed, Some("push")), "{op:?}");
    let sha = op.sha.clone().unwrap();

    // After a close or a restart, the journal still has it: committed, not pushed.
    let listed = unfinished(&f.ctx.dir);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        (listed[0].token.as_str(), listed[0].sha.as_deref(), listed[0].reference.as_deref()),
        (token.as_str(), Some(sha.as_str()), None)
    );

    // The files are clean now, so a new review is blocked; the token continues.
    let (review, _) = review_of(&f, &request).await;
    assert!(review.token.is_none(), "{:?}", review.blocked);
    let done = apply(&f.exec, &f.ctx, &Request { kind: Some(Kind::Pr), ..Request::default() }, &token).await.unwrap();
    assert_eq!((done.state, done.sha.as_deref()), (State::Pr, Some(sha.as_str())), "{done:?}");
    assert_eq!(git_in(&f.root, &f.bare, &["rev-parse", "refs/heads/zwinger/fix-01"]), sha);
    assert!(unfinished(&f.ctx.dir).is_empty());

    // A failure before a known commit changed nothing, so it is not listed.
    let dir = temp("unfinished-none");
    let failed = Operation {
        token: "t".into(),
        kind: Kind::Commit,
        state: State::Failed,
        sha: None,
        reference: None,
        url: None,
        step: Some("commit".into()),
        detail: None,
    };
    append(&dir, &failed, None).unwrap();
    assert!(unfinished(&dir).is_empty());
}
