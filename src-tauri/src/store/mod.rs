mod legacy;

use std::fs::File;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::agent::AgentKind;
use crate::cage::Cage;
use crate::error::{Error, Result};
use crate::host_agents::{dir_flags, type_at};
use crate::network::{Action, Credential, DefaultAction, Inject, NetworkPolicy, Rule, Source, Target};
use crate::secrets::{Origin, Secret};
use crate::session::Session;

/// The store files in the app data dir.
pub const SESSIONS: &str = "sessions.json";
pub const CAGES: &str = "cages.json";
/// The store file from before the split into sessions and cages. It is
/// read for the conversion only and never written, so an older app version
/// still finds it.
const PROJECTS: &str = "projects.json";
/// The store file from before the rename of shields to projects.
const SHIELDS: &str = "shields.json";

/// The first app version whose stable builds delete the shields file. See
/// `Compatibility removals` in `docs/release.md` before you change it.
const BACKUP_REMOVAL_FROM: [u64; 3] = [0, 4, 0];

/// True for a stable version (no pre-release or build part) at or after
/// [`BACKUP_REMOVAL_FROM`].
fn removes_backup(version: &str) -> bool {
    if version.contains(['-', '+']) {
        return false;
    }
    let parts: Vec<u64> = version.split('.').map_while(|part| part.parse().ok()).collect();
    parts.len() == 3 && parts.as_slice() >= BACKUP_REMOVAL_FROM.as_slice()
}

/// Copies the shields file to the projects file once, when only the
/// shields file is there. The shields file stays. `dir` is the app data
/// dir `data`. A projects file that is made meanwhile stays.
fn migrate_legacy(dir: &OwnedFd, data: &Path) -> Result<()> {
    if type_at(dir, PROJECTS)?.is_some() {
        return Ok(());
    }
    let Some(mut legacy) = open_regular(dir, SHIELDS, &data.join(SHIELDS))? else {
        return Ok(());
    };
    if !publish(dir, PROJECTS, false, |file| std::io::copy(&mut legacy, file).map(drop))? {
        eprintln!("{} was made during the copy of {SHIELDS} and stays", data.join(PROJECTS).display());
    }
    Ok(())
}

/// Opens the folder `path` with no link followed. `None`: it is missing.
fn open_dir(path: &Path) -> Result<Option<OwnedFd>> {
    match rustix::fs::open(path, dir_flags(), Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(std::io::Error::from(e).into()),
    }
}

/// Opens the regular file `name` of `dir`, at `path`, to read it. It
/// follows no link and does not wait on a special file, such as a FIFO.
/// `None`: it is missing.
fn open_regular(dir: &OwnedFd, name: &str, path: &Path) -> Result<Option<File>> {
    let refused = || Error::StoreFile { path: path.display().to_string() };
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => return Err(refused()),
        Err(e) => return Err(std::io::Error::from(e).into()),
    };
    let stat = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(refused());
    }
    Ok(Some(File::from(fd)))
}

/// Writes a new private temp file `.zwinger-<uuid>` of `dir` with `fill`
/// and renames it to `name`. The temp file is made exclusively, so no
/// planted link is followed. `replace`: false keeps an existing `name` and
/// returns false. On an error, the temp file is deleted.
fn publish(
    dir: &OwnedFd,
    name: impl rustix::path::Arg + Copy,
    replace: bool,
    fill: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<bool> {
    let tmp = format!(".zwinger-{}", uuid::Uuid::new_v4());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(dir, tmp.as_str(), flags, Mode::from_raw_mode(0o600)).map_err(std::io::Error::from)?;
    let renamed = |flags| match rustix::fs::renameat_with(dir, tmp.as_str(), dir, name, flags) {
        Ok(()) => Ok(true),
        Err(Errno::EXIST) if !replace => Ok(false),
        Err(e) => Err(std::io::Error::from(e).into()),
    };
    let published = match fill(&mut File::from(fd)) {
        Ok(()) if replace => renamed(RenameFlags::empty()),
        Ok(()) => renamed(RenameFlags::NOREPLACE),
        Err(e) => Err(e.into()),
    };
    if !matches!(published, Ok(true)) {
        let _ = rustix::fs::unlinkat(dir, tmp.as_str(), AtFlags::empty());
    }
    published
}

/// The sessions and the cages of a load.
type Loaded = (Vec<Session>, Vec<Cage>);

/// The sessions and cages files in the app data dir.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[cfg(test)]
    pub fn cages_path(&self) -> PathBuf {
        self.dir.join(CAGES)
    }

    /// When one of the two files is missing, a conversion did not run or
    /// did not end: it converts the projects file again and writes both.
    /// A session attached to a cage that is not in the cages file is an error.
    pub fn load(&self) -> Result<(Vec<Session>, Vec<Cage>)> {
        self.load_as(env!("CARGO_PKG_VERSION"))
    }

    /// [`Store::load`] by the app `version`. A version that
    /// [`removes_backup`] deletes the shields file after a load with no error.
    fn load_as(&self, version: &str) -> Result<(Vec<Session>, Vec<Cage>)> {
        let (loaded, clean) = self.load_stores()?;
        if clean && removes_backup(version) {
            self.remove_backup();
        }
        Ok(loaded)
    }

    /// Deletes the shields file when it is a regular file, and the sessions
    /// and cages files read back as regular files with no link followed and
    /// are durable: both files and the data dir are synced first. A link or
    /// another file stays. A failure is logged, and the next load tries again.
    fn remove_backup(&self) {
        let path = self.dir.join(SHIELDS);
        if let Err(e) = self.try_remove_backup() {
            eprintln!("{} stays: {e}", path.display());
        }
    }

    fn try_remove_backup(&self) -> Result<()> {
        let Some(dir) = open_dir(&self.dir)? else {
            return Ok(());
        };
        if type_at(&dir, SHIELDS)? != Some(FileType::RegularFile) {
            return Ok(());
        }
        read_synced::<Session>(&dir, SESSIONS, &self.dir.join(SESSIONS))?;
        read_synced::<Cage>(&dir, CAGES, &self.dir.join(CAGES))?;
        rustix::fs::fsync(&dir).map_err(std::io::Error::from)?;
        rustix::fs::unlinkat(&dir, SHIELDS, AtFlags::empty()).map_err(std::io::Error::from)?;
        Ok(())
    }

    /// The sessions and cages, and true if each save that the load needed
    /// succeeded. A failed save of the cages without the old Claude logins
    /// is false and does not fail the load.
    fn load_stores(&self) -> Result<(Loaded, bool)> {
        let (sessions, cages) = (self.dir.join(SESSIONS), self.dir.join(CAGES));
        if sessions.exists() && cages.exists() {
            let (sessions, cages): (Vec<Session>, Vec<Cage>) = (read(&sessions)?, read(&cages)?);
            for s in &sessions {
                if let Some(cage) = &s.cage {
                    check_cage(&cages, &format!("session {} ({})", s.name, s.id), cage)?;
                }
            }
            let mut cages = cages;
            let mut clean = true;
            if drop_claude_logins(&mut cages)
                && let Err(e) = self.save_cages(&cages)
            {
                eprintln!("the migrated cages could not be saved: {e}");
                clean = false;
            }
            return Ok(((sessions, cages), clean));
        }
        let (sessions, mut cages) = self.convert()?;
        drop_claude_logins(&mut cages);
        self.save_cages(&cages)?;
        self.save_sessions(&sessions)?;
        Ok(((sessions, cages), true))
    }

    /// Each project becomes one session and one cage. The migrations of
    /// older records run in memory only.
    fn convert(&self) -> Result<(Vec<Session>, Vec<Cage>)> {
        let Some(dir) = open_dir(&self.dir)? else {
            return Ok((Vec::new(), Vec::new()));
        };
        migrate_legacy(&dir, &self.dir)?;
        let projects: Vec<legacy::Project> = match open_regular(&dir, PROJECTS, &self.dir.join(PROJECTS))? {
            Some(file) => serde_json::from_reader(std::io::BufReader::new(file))?,
            None => Vec::new(),
        };
        let (mut sessions, mut cages) = (Vec::new(), Vec::new());
        let mut slugs = Vec::new();
        for mut project in projects {
            project.migrate();
            let (session, cage) = project.split(&slugs);
            slugs.push(session.slug.clone());
            sessions.push(session);
            cages.push(cage);
        }
        Ok((sessions, cages))
    }

    pub fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        write(&self.dir.join(SESSIONS), sessions)
    }

    pub fn save_cages(&self, cages: &[Cage]) -> Result<()> {
        write(&self.dir.join(CAGES), cages)
    }
}

/// Claude Code uses a setup token in the vault now, not a host login.
/// Removes the old Claude host logins. Returns true if it removed one.
fn drop_claude_logins(cages: &mut [Cage]) -> bool {
    let claude = Origin::HostLogin { agent: AgentKind::Claude };
    cages.iter_mut().fold(false, |dropped, cage| {
        let before = cage.logins.len();
        cage.logins.retain(|secret| secret.origin != claude);
        dropped | (before != cage.logins.len())
    })
}

/// `owner` names who uses `cage` in the error.
pub fn check_cage(cages: &[Cage], owner: &str, cage: &str) -> Result<()> {
    if cages.iter().any(|c| c.id == cage) {
        return Ok(());
    }
    Err(Error::MissingCage { owner: owner.to_owned(), cage: cage.to_owned() })
}

/// Parses the regular file `name` of `dir`, at `path`, and syncs it to
/// disk. A missing, linked or special file is an error ([`open_regular`]).
fn read_synced<T: DeserializeOwned>(dir: &OwnedFd, name: &str, path: &Path) -> Result<Vec<T>> {
    let Some(file) = open_regular(dir, name, path)? else {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, path.display().to_string())));
    };
    let records = serde_json::from_reader(std::io::BufReader::new(&file))?;
    file.sync_all()?;
    Ok(records)
}

fn read<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Writes a new private temp file in the same dir and renames it over the
/// file ([`publish`]).
pub(crate) fn write<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(Error::Io(std::io::Error::other(format!("{} is not a file path", path.display()))));
    };
    std::fs::create_dir_all(dir)?;
    let dir = rustix::fs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .map_err(std::io::Error::from)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    publish(&dir, name, true, |file| file.write_all(&bytes))?;
    Ok(())
}

/// Opens a regular file relative to its directory without following links.
pub(crate) fn open_data_file(dir: &Path, name: &str) -> Result<Option<File>> {
    let Some(fd) = open_dir(dir)? else { return Ok(None) };
    open_regular(&fd, name, &dir.join(name))
}

/// Publishes a private file and syncs both its contents and its directory entry.
pub(crate) fn write_synced<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<()> {
    let dir = open_dir(dir)?.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    let bytes = serde_json::to_vec(value)?;
    publish(&dir, name, true, |file| {
        file.write_all(&bytes)?;
        file.sync_all()
    })?;
    rustix::fs::fsync(&dir).map_err(std::io::Error::from)?;
    Ok(())
}

/// Each user secret becomes a credential with the same name, so it keeps its
/// vault account, on a rule for each of its exact hosts, with its header,
/// query or body places. The rule allows the host only where the old policy
/// reached it: Public mode, or a host on the allow list of List mode. Else
/// (Deny mode, or a host off the list) the rule blocks the host and keeps
/// the credential inactive. The host logins stay.
fn migrate_secrets(logins: &mut Vec<Secret>, network: &mut NetworkPolicy) {
    let (users, kept) = std::mem::take(logins).into_iter().partition(|s| s.origin.is_user());
    *logins = kept;
    let users: Vec<Secret> = users;
    // The policy came from an old shape: only Deny mode is offline, only Public mode allows by default.
    let reached = |rules: &[Rule], host: &str, offline: bool, default: DefaultAction| {
        !offline
            && (default == DefaultAction::Allow || rules.iter().any(|r| r.action == Action::Allow && r.matches(host)))
    };
    for secret in users {
        for host in &secret.hosts {
            let action = if reached(&network.rules, host, network.offline, network.default) {
                Action::Allow
            } else {
                Action::Block
            };
            let target = Target::Host { host: host.clone() };
            let rules = &mut network.rules;
            let i = match rules.iter().position(|r| r.target == target) {
                Some(i) => {
                    rules[i].action = action;
                    i
                }
                None => {
                    rules.push(Rule::new(target, action));
                    rules.len() - 1
                }
            };
            let credentials = &mut rules[i].credentials;
            if !credentials.iter().any(|c| c.name == secret.name) {
                credentials.push(Credential {
                    name: secret.name.clone(),
                    inject: Inject::Replace { places: secret.places.clone() },
                    source: Source::Local,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentKind;
    use crate::cage::{Bound, FileId, HostAgentUse, Mount};
    use crate::runtime::TerminalKind;
    use crate::secrets::{Origin, Places};
    use crate::session::{PaneNode, Pos, SplitDirection, Task, TaskAgent, TerminalRecord, Viewport, VmChoice};

    fn data_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn convert(projects: serde_json::Value) -> (Vec<Session>, Vec<Cage>) {
        let dir = data_dir();
        std::fs::write(dir.join(PROJECTS), projects.to_string()).unwrap();
        Store::new(dir).load().unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = data_dir().join("nested");
        let store = Store::new(dir.clone());
        assert_eq!(store.load().unwrap(), (Vec::new(), Vec::new()));
        let mut session = Session::new(data_dir().to_str().unwrap(), Some("c".into()), []).unwrap();
        session.layout = [("mount:/mnt/data".to_owned(), Pos { x: 12.5, y: -40.0 })].into();
        session.viewport = Some(Viewport { x: -374.25, y: 812.5, zoom: 0.75 });
        let cage = Cage::new("c".into(), "proj".into());
        store.save_cages(std::slice::from_ref(&cage)).unwrap();
        store.save_sessions(std::slice::from_ref(&session)).unwrap();
        assert_eq!(store.load().unwrap(), (vec![session], vec![cage]));
        assert!(!dir.join("sessions.json.tmp").exists() && !dir.join("cages.json.tmp").exists());
        assert!(!dir.join(PROJECTS).exists());
    }

    #[test]
    fn a_stored_cage_id_attaches_and_no_cage_inherits() {
        let dir = data_dir();
        let session = |id: &str, cage: serde_json::Value| serde_json::json!({ "id": id, "name": id, "folder": "/tmp/x", "slug": id, "cage": cage });
        let sessions = serde_json::json!([session("a", "c".into()), session("b", serde_json::Value::Null)]);
        std::fs::write(dir.join(SESSIONS), sessions.to_string()).unwrap();
        let store = Store::new(dir.clone());
        store.save_cages(&[Cage::new("c".into(), "c".into())]).unwrap();
        let (sessions, _) = store.load().unwrap();
        assert_eq!(sessions.iter().map(|s| s.cage.as_deref()).collect::<Vec<_>>(), [Some("c"), None]);
        // A session saved before project editors follows the app default.
        assert!(sessions.iter().all(|s| s.editor.is_none()));
        // A session saved before map viewports fits its map.
        assert!(sessions.iter().all(|s| s.viewport.is_none()));
        // A session saved before project accents uses the theme color.
        assert!(sessions.iter().all(|s| s.accent.is_none()));

        store.save_cages(&[]).unwrap();
        assert!(matches!(store.load(), Err(Error::MissingCage { cage, .. }) if cage == "c"));
    }

    /// A project of a real config: two mounts, rules with credentials, a
    /// host login, host agents, terminals, a pane layout, tasks, a map
    /// layout and the removed `apps`.
    fn project_json() -> serde_json::Value {
        serde_json::json!([{
            "id": "p-1", "name": "My Proj", "folder": "/tmp/My Proj",
            "folderId": { "dev": 1, "ino": 2 }, "idsMigrated": true, "networkMigrated": true,
            "mounts": [
                { "host": "/tmp/data", "id": { "dev": 1, "ino": 3 }, "guest": "/mnt/data", "readonly": true },
                { "host": "/tmp/out", "id": { "dev": 1, "ino": 4 }, "guest": "/mnt/out", "readonly": false },
            ],
            "network": { "default": "block", "offline": false, "rules": [
                { "id": "rule-1", "target": { "kind": "service", "id": "openai" }, "action": "allow",
                  "credentials": [{ "name": "OPENAI_API_KEY",
                    "inject": { "mode": "set", "header": "authorization", "template": "Bearer {secret}" } }] },
                { "id": "rule-2", "target": { "kind": "host", "host": "maps.example.com" }, "action": "block",
                  "credentials": [{ "name": "MAPS_KEY",
                    "inject": { "mode": "replace", "places": { "headerFields": [], "query": true, "body": false } } }] },
            ] },
            "harness": "/workspace/agent",
            "logins": [{ "name": "CLAUDE_CODE_OAUTH_TOKEN", "hosts": ["api.anthropic.com"],
                "places": { "headerFields": ["authorization"], "query": false, "body": false },
                "origin": { "kind": "hostLogin", "agent": "claude" } }],
            "layout": { "mount:/mnt/data": { "x": 12.5, "y": -40.0 } },
            "tasks": [{ "id": "task-1", "slug": "fix-it", "title": "Fix it", "request": "Fix the bug.",
                "agents": [{ "kind": "claude", "model": "opus" }, { "kind": "codex", "model": null }],
                "gates": ["pnpm build"], "workdir": "/workspace" }],
            "terminals": [
                { "id": "term-1", "kind": { "type": "agent", "agent": "codex", "resume": false } },
                { "id": "term-2", "kind": { "type": "shell" } },
                { "id": "term-3", "kind": { "type": "host" } },
            ],
            "paneLayout": { "type": "split", "direction": "row", "ratio": 0.5,
                "a": { "type": "leaf", "terminal": "term-3" }, "b": { "type": "leaf", "terminal": "term-1" } },
            "hostAgents": [{ "kind": "claude", "profile": true, "login": true }],
            "agentsUnprompted": false,
            "apps": [{ "id": "app-1", "name": "web", "command": "pnpm dev", "cwd": "/workspace", "autostart": true }],
        }])
    }

    #[test]
    fn a_project_converts_to_a_session_and_a_cage_with_no_loss() {
        let (sessions, cages) = convert(project_json());
        let session = Session {
            id: "p-1".into(),
            name: "My Proj".into(),
            folder: "/tmp/My Proj".into(),
            folder_id: Some(FileId { dev: 1, ino: 2 }),
            slug: "my-proj".into(),
            cage: Some("p-1".into()),
            vm: VmChoice::Shared,
            harness: Some("/workspace/agent".into()),
            editor: None,
            accent: None,
            terminals: vec![
                TerminalRecord {
                    id: "term-1".into(),
                    kind: TerminalKind::Agent { agent: AgentKind::Codex, resume: false },
                },
                TerminalRecord { id: "term-2".into(), kind: TerminalKind::Shell },
                TerminalRecord { id: "term-3".into(), kind: TerminalKind::Host },
            ],
            pane_layout: Some(PaneNode::Split {
                direction: SplitDirection::Row,
                ratio: 0.5,
                a: Box::new(PaneNode::Leaf { terminal: "term-3".into() }),
                b: Box::new(PaneNode::Leaf { terminal: "term-1".into() }),
            }),
            tasks: vec![Task {
                id: "task-1".into(),
                slug: "fix-it".into(),
                title: "Fix it".into(),
                request: "Fix the bug.".into(),
                agents: [
                    TaskAgent { kind: AgentKind::Claude, model: Some("opus".into()) },
                    TaskAgent { kind: AgentKind::Codex, model: None },
                ],
                gates: vec!["pnpm build".into()],
                workdir: "/workspace".into(),
            }],
            layout: [("mount:/mnt/data".to_owned(), Pos { x: 12.5, y: -40.0 })].into(),
            viewport: None,
        };
        let cage = Cage {
            id: "p-1".into(),
            name: "My Proj".into(),
            mounts: vec![
                Mount {
                    host: "/tmp/data".into(),
                    id: Some(FileId { dev: 1, ino: 3 }),
                    guest: "/mnt/data".into(),
                    readonly: true,
                },
                Mount {
                    host: "/tmp/out".into(),
                    id: Some(FileId { dev: 1, ino: 4 }),
                    guest: "/mnt/out".into(),
                    readonly: false,
                },
            ],
            network: serde_json::from_value(project_json()[0]["network"].clone()).unwrap(),
            // The Claude host login is dropped: Claude Code uses a setup token now.
            logins: Vec::new(),
            host_agents: vec![HostAgentUse { kind: AgentKind::Claude, profile: true, login: true }],
            agents_unprompted: false,
        };
        assert_eq!(cage.network.credentials().len(), 2);
        assert_eq!((sessions, cages), (vec![session], vec![cage]));
    }

    #[test]
    fn a_stored_claude_host_login_is_dropped_and_saved() {
        let dir = data_dir();
        let store = Store::new(dir.clone());
        let session = Session::new(data_dir().to_str().unwrap(), Some("c".into()), []).unwrap();
        let mut cage = Cage::new("c".into(), "proj".into());
        cage.host_agents.push(HostAgentUse { kind: AgentKind::Claude, profile: false, login: true });
        cage.logins.push(Secret {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            hosts: vec!["api.anthropic.com".into()],
            places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
            origin: Origin::HostLogin { agent: AgentKind::Claude },
        });
        store.save_cages(&[cage]).unwrap();
        store.save_sessions(&[session]).unwrap();
        let (_, cages) = store.load().unwrap();
        assert!(cages[0].logins.is_empty());
        assert!(cages[0].host_agents[0].login);
        let saved: Vec<Cage> = read(&dir.join(CAGES)).unwrap();
        assert_eq!(saved, cages);
    }

    #[test]
    fn a_second_load_changes_nothing_and_never_writes_the_projects_file() {
        let dir = data_dir();
        let projects = project_json().to_string();
        std::fs::write(dir.join(PROJECTS), &projects).unwrap();
        let store = Store::new(dir.clone());
        let first = store.load().unwrap();
        let files = |name: &str| std::fs::read(dir.join(name)).unwrap();
        let (sessions, cages) = (files(SESSIONS), files(CAGES));

        assert_eq!(store.load().unwrap(), first);
        assert_eq!((files(SESSIONS), files(CAGES)), (sessions, cages));
        assert_eq!(files(PROJECTS), projects.as_bytes());
    }

    #[test]
    fn a_missing_sessions_or_cages_file_converts_again() {
        for left in [SESSIONS, CAGES] {
            let dir = data_dir();
            std::fs::write(dir.join(PROJECTS), project_json().to_string()).unwrap();
            let store = Store::new(dir.clone());
            let first = store.load().unwrap();
            let gone = if left == SESSIONS { CAGES } else { SESSIONS };
            std::fs::remove_file(dir.join(gone)).unwrap();
            // The file that is left is overwritten, also when it was edited.
            std::fs::write(dir.join(left), "[]").unwrap();
            assert_eq!(store.load().unwrap(), first, "{left} left");
            assert!(dir.join(gone).exists());
        }
    }

    #[test]
    fn the_slugs_are_unique_over_all_sessions() {
        let project = |id: &str, folder: &str| {
            serde_json::json!({ "id": id, "name": id, "folder": folder, "mounts": [], "idsMigrated": true,
                "networkMigrated": true, "network": { "default": "block", "offline": false, "rules": [] } })
        };
        let (sessions, _) = convert(serde_json::json!([project("a", "/x/api"), project("b", "/y/api")]));
        assert_eq!(sessions.iter().map(|s| s.slug.as_str()).collect::<Vec<_>>(), ["api", "api-2"]);
    }

    #[test]
    fn the_shields_file_converts_and_stays() {
        let dir = data_dir();
        let old = r#"[{"id":"a","name":"proj","folder":"/tmp/proj","mounts":[],
            "network":{"allowAll":false,"domains":[]}}]"#;
        std::fs::write(dir.join(SHIELDS), old).unwrap();
        let (sessions, cages) = Store::new(dir.clone()).load_as("0.1.0").unwrap();
        assert_eq!((sessions[0].name.as_str(), cages[0].id.as_str()), ("proj", "a"));
        assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), old);
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), old);
    }

    #[test]
    fn only_a_stable_build_at_or_after_0_4_0_removes_the_backup() {
        for version in ["0.4.0", "0.4.1", "0.10.0", "1.0.0"] {
            assert!(removes_backup(version), "{version}");
        }
        for version in ["0.1.0", "0.3.9", "0.4.0-beta.1", "0.4.0+1", "0.4", "x"] {
            assert!(!removes_backup(version), "{version}");
        }
    }

    /// The versions of the tests: one before the gate keeps the backup, one
    /// at the gate deletes it. `true`: the backup stays.
    const VERSIONS: [(&str, bool); 2] = [("0.1.0", true), ("0.4.0", false)];

    /// The names of the temp files that a write left in `dir`.
    fn temps(dir: &Path) -> Vec<String> {
        let names = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned());
        names.filter(|name| name.starts_with(".zwinger-")).collect()
    }

    #[test]
    fn a_populated_direct_upgrade_converts_and_keeps_or_deletes_the_backup_by_version() {
        let old = project_json().to_string();
        let expected = convert(project_json());
        for (version, kept) in VERSIONS {
            let dir = data_dir();
            std::fs::write(dir.join(SHIELDS), &old).unwrap();
            let store = Store::new(dir.clone());
            assert_eq!(store.load_as(version).unwrap(), expected, "{version}");
            assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), old, "{version}");
            assert_eq!(read::<Session>(&dir.join(SESSIONS)).unwrap(), expected.0, "{version}");
            assert_eq!(read::<Cage>(&dir.join(CAGES)).unwrap(), expected.1, "{version}");
            assert_eq!(dir.join(SHIELDS).exists(), kept, "{version}");
            if kept {
                assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), old);
            }
            assert_eq!(store.load_as(version).unwrap(), expected, "{version}: a second load");
            assert!(temps(&dir).is_empty(), "{version}");
        }
    }

    #[test]
    fn a_populated_existing_store_keeps_or_deletes_the_backup_by_version() {
        let old = project_json().to_string();
        let (sessions, cages) = convert(project_json());
        for (version, kept) in VERSIONS {
            let dir = data_dir();
            let store = Store::new(dir.clone());
            store.save_cages(&cages).unwrap();
            store.save_sessions(&sessions).unwrap();
            std::fs::write(dir.join(SHIELDS), &old).unwrap();
            assert_eq!(store.load_as(version).unwrap(), (sessions.clone(), cages.clone()), "{version}");
            assert_eq!(dir.join(SHIELDS).exists(), kept, "{version}");
            assert!(!dir.join(PROJECTS).exists(), "{version}: no conversion runs");
            assert_eq!(store.load_as(version).unwrap(), (sessions.clone(), cages.clone()), "{version}");
        }
    }

    /// The cages file still has an old Claude host login, and the save
    /// without it fails: the load goes on with the older cages file, and
    /// the backup stays until a load saves with no error.
    #[test]
    fn a_failed_save_of_the_loaded_cages_keeps_the_backup() {
        let dir = data_dir();
        let store = Store::new(dir.clone());
        let (sessions, mut cages) = convert(project_json());
        cages[0].logins.push(Secret {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            hosts: vec!["api.anthropic.com".into()],
            places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
            origin: Origin::HostLogin { agent: AgentKind::Claude },
        });
        store.save_cages(&cages).unwrap();
        store.save_sessions(&sessions).unwrap();
        let old = project_json().to_string();
        std::fs::write(dir.join(SHIELDS), &old).unwrap();
        let saved = std::fs::read(dir.join(CAGES)).unwrap();

        let mode = |m| std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(m)).unwrap();
        mode(0o555);
        let clean = store.load_stores().map(|(_, clean)| clean);
        let loaded = store.load_as("0.4.0");
        mode(0o755);
        assert!(!clean.unwrap(), "the failed save is not clean");
        assert!(loaded.unwrap().1[0].logins.is_empty());
        assert_eq!(std::fs::read(dir.join(CAGES)).unwrap(), saved);
        assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), old);
        assert!(temps(&dir).is_empty());

        let (_, cages) = store.load_as("0.4.0").unwrap();
        assert!(cages[0].logins.is_empty());
        assert_eq!(read::<Cage>(&dir.join(CAGES)).unwrap(), cages);
        assert!(!dir.join(SHIELDS).exists());
    }

    #[test]
    fn a_linked_shields_or_projects_file_is_refused_and_stays() {
        for name in [SHIELDS, PROJECTS] {
            let dir = data_dir();
            let outside = dir.join("outside.json");
            let old = project_json().to_string();
            std::fs::write(&outside, &old).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join(name)).unwrap();
            let e = Store::new(dir.clone()).load_as("0.4.0").unwrap_err();
            let path = dir.join(name).display().to_string();
            assert!(matches!(&e, Error::StoreFile { path: p } if *p == path), "{name}: {e}");
            assert!(e.message().contains(&path), "{name}: {e}");
            assert!(std::fs::symlink_metadata(dir.join(name)).unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), old);
            for file in [PROJECTS, SESSIONS, CAGES].into_iter().filter(|file| *file != name) {
                assert!(!dir.join(file).exists(), "{name}: {file}");
            }
            assert!(temps(&dir).is_empty());
        }
    }

    #[test]
    fn a_special_shields_file_is_refused_with_no_wait() {
        let fifo = data_dir();
        let path = std::ffi::CString::new(fifo.join(SHIELDS).into_os_string().into_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let folder = data_dir();
        std::fs::create_dir(folder.join(SHIELDS)).unwrap();
        for dir in [fifo, folder] {
            let e = Store::new(dir.clone()).load_as("0.4.0").unwrap_err();
            assert!(matches!(e, Error::StoreFile { .. }), "{e}");
            assert!(std::fs::symlink_metadata(dir.join(SHIELDS)).is_ok());
            assert!(!dir.join(PROJECTS).exists() && temps(&dir).is_empty());
        }
    }

    /// The readback before the backup delete follows no link: a linked
    /// sessions or cages file keeps the backup, though the load reads it.
    #[test]
    fn a_linked_modern_store_keeps_the_backup() {
        for name in [SESSIONS, CAGES] {
            let dir = data_dir();
            let store = Store::new(dir.clone());
            let (sessions, cages) = convert(project_json());
            store.save_cages(&cages).unwrap();
            store.save_sessions(&sessions).unwrap();
            let old = project_json().to_string();
            std::fs::write(dir.join(SHIELDS), &old).unwrap();
            let outside = dir.join("outside.json");
            std::fs::rename(dir.join(name), &outside).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join(name)).unwrap();
            assert_eq!(store.load_as("0.4.0").unwrap(), (sessions, cages), "{name}");
            assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), old, "{name}");
            assert!(std::fs::symlink_metadata(dir.join(name)).unwrap().file_type().is_symlink());
        }
    }

    /// A link at the temp path of an older build is not written through.
    #[test]
    fn a_linked_temp_file_is_not_followed() {
        let dir = data_dir();
        let precious = dir.join("precious");
        std::fs::write(&precious, "host").unwrap();
        for name in ["projects.json.tmp", "sessions.json.tmp", "cages.json.tmp"] {
            std::os::unix::fs::symlink(&precious, dir.join(name)).unwrap();
        }
        let old = project_json().to_string();
        std::fs::write(dir.join(SHIELDS), &old).unwrap();
        let loaded = Store::new(dir.clone()).load_as("0.1.0").unwrap();
        assert_eq!(loaded, convert(project_json()));
        assert_eq!(std::fs::read_to_string(&precious).unwrap(), "host");
        for name in [PROJECTS, SESSIONS, CAGES] {
            let meta = std::fs::symlink_metadata(dir.join(name)).unwrap();
            assert!(meta.file_type().is_file(), "{name}");
            assert_eq!(std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o077, 0, "{name} is private");
        }
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), old);
        assert!(temps(&dir).is_empty());
    }

    #[test]
    fn a_removal_build_converts_the_shields_file_and_then_deletes_it() {
        let dir = data_dir();
        let old = r#"[{"id":"a","name":"proj","folder":"/tmp/proj","mounts":[],
            "network":{"allowAll":false,"domains":[]}}]"#;
        std::fs::write(dir.join(SHIELDS), old).unwrap();
        let store = Store::new(dir.clone());
        let (sessions, cages) = store.load_as("0.4.0").unwrap();
        assert_eq!((sessions[0].name.as_str(), cages[0].id.as_str()), ("proj", "a"));
        assert!(!dir.join(SHIELDS).exists());
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), old);
        assert_eq!(store.load_as("0.4.0").unwrap(), (sessions, cages));
    }

    #[test]
    fn a_failed_load_keeps_the_backup_and_a_later_load_removes_it() {
        let dir = data_dir();
        std::fs::write(dir.join(SHIELDS), "[]").unwrap();
        std::fs::write(dir.join(SESSIONS), "not json").unwrap();
        std::fs::write(dir.join(CAGES), "[]").unwrap();
        let store = Store::new(dir.clone());
        assert!(store.load_as("0.4.0").is_err());
        assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), "[]");

        std::fs::write(dir.join(SESSIONS), "[]").unwrap();
        store.load_as("0.4.0").unwrap();
        assert!(!dir.join(SHIELDS).exists());
    }

    #[test]
    fn a_failed_removal_is_retried_and_a_link_stays() {
        let dir = data_dir();
        std::fs::write(dir.join(SESSIONS), "[]").unwrap();
        std::fs::write(dir.join(CAGES), "[]").unwrap();
        std::fs::create_dir(dir.join(SHIELDS)).unwrap();
        std::fs::write(dir.join(SHIELDS).join("x"), "x").unwrap();
        let store = Store::new(dir.clone());
        store.load_as("0.4.0").unwrap();
        assert!(dir.join(SHIELDS).join("x").is_file(), "a folder is not a backup and stays");
        std::fs::remove_dir_all(dir.join(SHIELDS)).unwrap();

        let outside = dir.join("outside.json");
        std::fs::write(&outside, "[]").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(SHIELDS)).unwrap();
        store.load_as("0.4.0").unwrap();
        assert!(std::fs::symlink_metadata(dir.join(SHIELDS)).unwrap().file_type().is_symlink());
        assert!(outside.is_file());
        std::fs::remove_file(dir.join(SHIELDS)).unwrap();

        // A read-only data dir makes the delete fail: the file stays for the next load.
        std::fs::write(dir.join(SHIELDS), "[]").unwrap();
        let mode = |m| std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(m)).unwrap();
        mode(0o555);
        let kept = store.load_as("0.4.0").map(|_| dir.join(SHIELDS).is_file());
        mode(0o755);
        assert!(kept.unwrap());
        store.load_as("0.4.0").unwrap();
        assert!(!dir.join(SHIELDS).exists());
    }

    #[test]
    fn the_shields_file_does_not_overwrite_the_projects_file() {
        let dir = data_dir();
        std::fs::write(dir.join(SHIELDS), "old").unwrap();
        std::fs::write(dir.join(PROJECTS), "[]").unwrap();
        assert_eq!(Store::new(dir.clone()).load_as("0.1.0").unwrap(), (Vec::new(), Vec::new()));
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), "[]");
        assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), "old");
    }

    #[test]
    fn an_old_record_gets_the_defaults() {
        let (sessions, cages) = convert(serde_json::json!([{ "id": "a", "name": "proj", "folder": "/tmp/proj",
            "mounts": [], "network": { "allowAll": false, "domains": [] } }]));
        let (session, cage) = (&sessions[0], &cages[0]);
        assert_eq!(session.harness, None);
        assert_eq!(session.editor, None);
        assert_eq!(session.accent, None);
        assert!(session.layout.is_empty() && session.tasks.is_empty() && session.terminals.is_empty());
        assert_eq!(session.pane_layout, None);
        assert_eq!(session.viewport, None);
        assert!(cage.logins.is_empty() && cage.host_agents.is_empty());
        assert_eq!(cage.network.default, DefaultAction::Block);
        assert!(cage.agents_unprompted);
    }

    #[test]
    fn the_conversion_fills_the_folder_ids_of_old_records_in_memory() {
        let dir = data_dir();
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let folder = dir.join("proj").canonicalize().unwrap();
        let old = serde_json::json!([{ "id": "a", "name": "proj", "folder": folder, "mounts": [],
            "network": { "mode": "deny", "domains": [] } }])
        .to_string();
        std::fs::write(dir.join(PROJECTS), &old).unwrap();
        let store = Store::new(dir.clone());
        let (sessions, cages) = store.load().unwrap();
        let id = sessions[0].folder_id.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), old);

        // Later loads keep the stored id, also after a swap.
        std::fs::rename(&folder, dir.join("was")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        let (sessions, _) = store.load().unwrap();
        assert_eq!(sessions[0].folder_id, Some(id));
        assert!(Bound { session: &sessions[0], cage: &cages[0] }.verify_paths().is_err());
    }

    /// A project of M2 with an allow list, two secrets for one host, a
    /// secret for two hosts with query and body places, and a host login.
    #[test]
    fn the_conversion_moves_the_old_secrets_to_rule_credentials() {
        let header = |field: &str| serde_json::json!({ "headerFields": [field], "query": false, "body": false });
        let (_, mut cages) = convert(serde_json::json!([{ "id": "a", "name": "proj", "folder": "/tmp/proj",
            "mounts": [], "idsMigrated": true,
            "network": { "mode": "list", "domains": ["api.openai.com", "maps.example.com"] },
            "hostAgents": [{ "kind": "claude", "profile": false, "login": true }],
            "autoRenew": true,
            "secrets": [
                { "name": "OPENAI_API_KEY", "hosts": ["api.openai.com"], "places": header("authorization") },
                { "name": "OPENAI_ORG", "hosts": ["api.openai.com"], "places": header("openai-organization") },
                { "name": "MAPS_KEY", "hosts": ["maps.example.com", "tiles.example.com"],
                  "places": { "headerFields": [], "query": true, "body": true } },
                { "name": "CLAUDE_CODE_OAUTH_TOKEN", "hosts": ["api.anthropic.com"], "places": header("authorization"),
                  "origin": { "kind": "hostLogin", "agent": "claude" } },
            ] }]));
        let cage = cages.remove(0);
        assert!(cage.logins.is_empty());
        let rule = |host: &str| {
            cage.network.rules.iter().find(|r| r.target == Target::Host { host: host.into() }).unwrap().clone()
        };
        let names = |rule: &Rule| rule.credentials.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        let openai = rule("api.openai.com");
        assert_eq!(openai.action, Action::Allow);
        assert_eq!(names(&openai), ["OPENAI_API_KEY", "OPENAI_ORG"]);
        assert_eq!(
            openai.credentials[1].inject,
            Inject::Replace {
                places: Places { header_fields: vec!["openai-organization".into()], query: false, body: false }
            }
        );
        // One secret on two hosts: one credential name, so one vault account, on each host.
        // The host off the allow list stays blocked, with the credential inactive.
        let maps = Places { header_fields: vec![], query: true, body: true };
        for (host, action) in [("maps.example.com", Action::Allow), ("tiles.example.com", Action::Block)] {
            let rule = rule(host);
            assert_eq!(rule.action, action, "{host}");
            assert_eq!(
                rule.credentials,
                [Credential {
                    name: "MAPS_KEY".into(),
                    inject: Inject::Replace { places: maps.clone() },
                    source: Source::Local
                }]
            );
        }
        cage.check().unwrap_or_else(|e| panic!("{e}"));
    }

    /// Converts one old project with `network` and one secret for
    /// `a.example.com` and `b.example.com`. Returns the action of each host's rule.
    fn migrate(network: serde_json::Value) -> (Cage, [Option<Action>; 2]) {
        let (_, mut cages) = convert(serde_json::json!([{ "id": "a", "name": "proj", "folder": "/tmp/proj",
            "mounts": [], "idsMigrated": true, "network": network,
            "secrets": [{ "name": "KEY", "hosts": ["a.example.com", "b.example.com"],
                "places": { "headerFields": ["authorization"], "query": false, "body": false } }] }]));
        let cage = cages.remove(0);
        cage.check().unwrap_or_else(|e| panic!("{e}"));
        let action = |host: &str| {
            let rule = cage.network.rules.iter().find(|r| r.target == Target::Host { host: host.into() })?;
            assert_eq!(rule.credentials.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["KEY"]);
            Some(rule.action)
        };
        let actions = [action("a.example.com"), action("b.example.com")];
        (cage, actions)
    }

    #[test]
    fn a_secret_of_list_mode_is_allowed_only_on_the_listed_hosts() {
        let (cage, actions) = migrate(serde_json::json!({ "mode": "list", "domains": ["*.a.example.com"] }));
        assert_eq!(actions, [Some(Action::Allow), Some(Action::Block)]);
        assert_eq!(cage.network.default, DefaultAction::Block);
        // The proxy sends the inactive credential nowhere.
        let blocked = cage.network.rules.iter().find(|r| r.action == Action::Block).unwrap();
        assert!(!blocked.sends_credentials_to("b.example.com"));
    }

    #[test]
    fn a_secret_of_public_mode_stays_allowed_with_the_stored_domains() {
        let (cage, actions) = migrate(serde_json::json!({ "mode": "public", "domains": ["c.example.com"] }));
        assert_eq!(actions, [Some(Action::Allow), Some(Action::Allow)]);
        assert_eq!(cage.network.default, DefaultAction::Allow);
        let c = Target::Host { host: "c.example.com".into() };
        assert!(cage.network.rules.iter().any(|r| r.target == c && r.action == Action::Allow));
    }

    #[test]
    fn a_secret_of_deny_mode_stays_inactive_on_block_rules() {
        let (cage, actions) = migrate(serde_json::json!({ "mode": "deny", "domains": ["a.example.com"] }));
        assert_eq!(actions, [Some(Action::Block), Some(Action::Block)]);
        assert!(cage.network.offline);
    }
}
