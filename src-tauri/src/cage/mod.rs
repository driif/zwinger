pub(crate) mod history;
pub(crate) mod lifecycle;
pub(crate) mod manager;
pub(crate) mod network;
pub(crate) mod progress;
pub(crate) mod tasks;
#[cfg(test)]
pub(crate) mod testing;
pub(crate) mod undo;
pub(crate) mod vm_state;

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::{self, AgentKind};
use crate::error::{Error, Result};
use crate::host_agents::login;
use crate::network::{NetworkPolicy, Source};
use crate::secrets::{self, CredentialRef, Origin, Secret};
use crate::session::{Session, VmChoice};

pub const WORKSPACE: &str = "/workspace";
/// The credential that holds the Claude Code setup token.
pub(crate) const CLAUDE_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The Claude token of each source.
pub(crate) fn claude_token_refs() -> impl Iterator<Item = CredentialRef> {
    [Source::Local, Source::Library].into_iter().map(|source| CredentialRef { source, name: CLAUDE_TOKEN.into() })
}
const FOLDER_LIMIT: usize = 50;

/// A saved config for a sandbox: mounts, network policy, credentials and
/// host agents. A VM runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cage {
    pub id: String,
    pub name: String,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
    /// The host logins of the host agents.
    #[serde(default)]
    pub logins: Vec<Secret>,
    /// The agents that the cage takes from the host. At most one per kind.
    #[serde(default)]
    pub host_agents: Vec<HostAgentUse>,
    /// The agent commands skip their permission prompts: the cage is the sandbox.
    #[serde(default = "yes")]
    pub agents_unprompted: bool,
    /// The CPUs and memory of each VM of the cage. Applies on the next start.
    #[serde(default)]
    pub resources: Resources,
}

fn yes() -> bool {
    true
}

pub(crate) const CPUS: std::ops::RangeInclusive<u32> = 1..=8;
pub(crate) const MEMORY_MIB: std::ops::RangeInclusive<u32> = 512..=8192;

/// The CPUs and memory of a VM. Always in [`CPUS`] and [`MEMORY_MIB`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RawResources")]
pub struct Resources {
    cpus: u8,
    memory_mib: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawResources {
    cpus: u32,
    memory_mib: u32,
}

impl TryFrom<RawResources> for Resources {
    type Error = Error;

    fn try_from(raw: RawResources) -> Result<Self> {
        Self::new(raw.cpus, raw.memory_mib)
    }
}

impl Resources {
    pub fn new(cpus: u32, memory_mib: u32) -> Result<Self> {
        match u8::try_from(cpus) {
            Ok(cpus_u8) if CPUS.contains(&cpus) && MEMORY_MIB.contains(&memory_mib) => {
                Ok(Self { cpus: cpus_u8, memory_mib })
            }
            _ => Err(Error::InvalidResources { cpus, memory_mib }),
        }
    }

    pub fn cpus(self) -> u8 {
        self.cpus
    }

    pub fn memory_mib(self) -> u32 {
        self.memory_mib
    }
}

/// The microsandbox 0.7.8 defaults, so a cage from before resources keeps them.
impl Default for Resources {
    fn default() -> Self {
        Self { cpus: 1, memory_mib: 512 }
    }
}

/// A host agent of a cage: its runtime, and if the cage also gets the
/// agent's profile (read-only) and its host login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAgentUse {
    pub kind: AgentKind,
    pub profile: bool,
    pub login: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub host: PathBuf,
    /// The identity of `host` when the user picked it.
    #[serde(default)]
    pub id: Option<FileId>,
    pub guest: String,
    pub readonly: bool,
}

/// The identity of a host folder: a folder that is swapped for another one
/// at the same path has a different identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

impl FileId {
    fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self { dev: metadata.dev(), ino: metadata.ino() }
    }
}

impl Cage {
    pub fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            mounts: Vec::new(),
            network: NetworkPolicy::default(),
            logins: Vec::new(),
            host_agents: Vec::new(),
            agents_unprompted: true,
            resources: Resources::default(),
        }
    }

    /// Adds or replaces the host agent of its kind. `login` is the agent's
    /// host-login secret if the use has a login; it replaces the old one.
    /// The Claude login off also removes its token credential from the rules.
    pub fn put_host_agent(&mut self, used: HostAgentUse, login: Option<Secret>) {
        let kind = used.kind;
        if kind == AgentKind::Claude && !used.login {
            self.remove_claude_token();
        }
        self.logins.retain(|s| s.origin != Origin::HostLogin { agent: kind });
        if kind != AgentKind::Claude {
            self.logins.extend(login);
        }
        match self.host_agents.iter_mut().find(|a| a.kind == kind) {
            Some(current) => *current = used,
            None => self.host_agents.push(used),
        }
    }

    /// Also removes its host-login secret, and for Claude its token credential.
    pub fn remove_host_agent(&mut self, kind: AgentKind) {
        if kind == AgentKind::Claude {
            self.remove_claude_token();
        }
        self.host_agents.retain(|a| a.kind != kind);
        self.logins.retain(|s| s.origin != Origin::HostLogin { agent: kind });
    }

    pub fn host_agent(&self, kind: AgentKind) -> Option<&HostAgentUse> {
        self.host_agents.iter().find(|a| a.kind == kind)
    }

    /// The rules of the cage alone that every stored config must meet.
    /// [`Bound::check`] adds the rules that need a session.
    pub fn check(&self) -> Result<()> {
        self.network.check()?;
        self.claude_token_binding()?;
        secrets::check_unique(&self.logins)?;
        for (i, used) in self.host_agents.iter().enumerate() {
            if self.host_agents[..i].iter().any(|a| a.kind == used.kind) {
                return Err(Error::DuplicateHostAgent(used.kind.name().to_owned()));
            }
        }
        // A host login belongs to a host agent with a login.
        for secret in &self.logins {
            if secret.origin == (Origin::HostLogin { agent: AgentKind::Claude }) {
                return Err(Error::InvalidSecret(
                    secret.name.clone(),
                    "Claude Code uses a setup token in the vault".into(),
                ));
            }
            if !matches!(secret.origin, Origin::HostLogin { agent } if self.host_agent(agent).is_some_and(|a| a.login))
            {
                return Err(Error::InvalidSecret(secret.name.clone(), "it is not the login of a host agent".into()));
            }
        }
        if let Some(credential) =
            self.network.credentials().into_iter().find(|c| self.logins.iter().any(|l| l.name == c.name))
        {
            return Err(Error::DuplicateSecret(credential.name.clone()));
        }
        // The proxy blocks the refresh hosts of the host logins, so no
        // credential may go there, such as a GitHub token with pi's Copilot login.
        // A Block rule never sends its credentials, so it may keep them.
        for host in login::denied_hosts(&self.logins) {
            let holder = self.network.rules.iter().find(|r| {
                r.action == crate::network::Action::Allow
                    && !r.credentials.is_empty()
                    && r.credential_hosts().iter().any(|h| crate::network::covers(h, host))
            });
            if let Some(rule) = holder {
                return Err(Error::RefreshHostConflict(
                    host.to_owned(),
                    format!("the credentials of {}", rule.label()),
                ));
            }
        }
        Ok(())
    }

    fn remove_claude_token(&mut self) {
        for rule in &mut self.network.rules {
            rule.credentials.retain(|c| c.name != CLAUDE_TOKEN);
        }
    }

    /// The value of the Claude token credential: the one of the Allow rule
    /// first. With none, the local value.
    pub fn claude_token_ref(&self) -> CredentialRef {
        let mut rules: Vec<&crate::network::Rule> = self.network.rules.iter().collect();
        rules.sort_by_key(|r| r.action != crate::network::Action::Allow);
        rules
            .into_iter()
            .flat_map(|r| &r.credentials)
            .find(|c| c.name == CLAUDE_TOKEN)
            .map_or_else(|| CredentialRef { source: Source::Local, name: CLAUDE_TOKEN.into() }, |c| c.key())
    }

    /// Whether an Allow rule injects the Claude token. Fails if one injects
    /// it other than to exact host api.anthropic.com as a Bearer token. The
    /// check runs whether the Claude login is on or off.
    pub fn claude_token_binding(&self) -> Result<bool> {
        let mut found = false;
        for rule in &self.network.rules {
            for credential in &rule.credentials {
                if credential.name != CLAUDE_TOKEN || rule.action != crate::network::Action::Allow {
                    continue;
                }
                let valid = matches!(&rule.target, crate::network::Target::Host { host } if host == "api.anthropic.com")
                    && matches!(&credential.inject, crate::network::Inject::Set { header, template }
                        if header == "authorization" && template == "Bearer {secret}");
                if !valid {
                    return Err(Error::InvalidSecret(credential.name.clone(),
                        "Claude Code requires an Allow rule for exact host api.anthropic.com with authorization: Bearer {secret}".into()));
                }
                found = true;
            }
        }
        Ok(found)
    }

    fn guest_target_exists(&self, path: &Path) -> bool {
        path.starts_with(WORKSPACE) || self.mounts.iter().any(|m| path.starts_with(&m.guest))
    }

    /// The guest path of the mount that holds `harness`: the deepest mount
    /// whose guest path contains it. `None` for `/workspace` or no harness.
    fn harness_mount(&self, harness: Option<&str>) -> Option<&str> {
        let harness = Path::new(harness?);
        self.mounts.iter().filter(|m| harness.starts_with(&m.guest)).map(|m| m.guest.as_str()).max_by_key(|g| g.len())
    }

    /// Refuses a mount change that moves the harness of one of `sessions`
    /// to a different mount, so the harness cannot silently point to a
    /// different host folder.
    fn keep_harness_mount(&mut self, sessions: &[&Session], change: impl FnOnce(&mut Self)) -> Result<()> {
        let before: Vec<Option<String>> =
            sessions.iter().map(|s| self.harness_mount(s.harness.as_deref()).map(str::to_owned)).collect();
        let saved = self.mounts.clone();
        change(self);
        for (session, before) in sessions.iter().zip(before) {
            if self.harness_mount(session.harness.as_deref()) != before.as_deref() {
                self.mounts = saved;
                let harness = session.harness.clone().unwrap_or_default();
                return Err(Error::HarnessMount(harness, before.unwrap_or_else(|| WORKSPACE.to_owned())));
            }
        }
        Ok(())
    }

    /// `sessions` are the sessions of the cage: a mount may not nest with
    /// their folders, and may not move their harness.
    pub fn add_mount(&mut self, sessions: &[&Session], host: &str, guest: &str, readonly: bool) -> Result<()> {
        let (host, id) = host_dir_with_id(host)?;
        let guest = guest_path(guest)?;
        self.place_mount(sessions, Mount { host, id: Some(id), guest, readonly }, self.mounts.len())
    }

    /// Inserts `mount` at `index`, or at the end if `index` is past it, with
    /// the checks of [`Self::add_mount`]. The guest path must already be in
    /// its canonical form: a stored mount is never rewritten. An error leaves
    /// the mounts as they were.
    pub fn insert_mount(&mut self, sessions: &[&Session], mount: Mount, index: usize) -> Result<()> {
        if guest_path(&mount.guest)? != mount.guest {
            return Err(Error::InvalidGuestPath(mount.guest));
        }
        self.place_mount(sessions, mount, index)
    }

    /// The checks of [`Self::insert_mount`] after the guest path check. An
    /// old config can hold a guest path that is not canonical, such as
    /// `/mnt/a/`: it still takes `/mnt/a`.
    fn place_mount(&mut self, sessions: &[&Session], mount: Mount, index: usize) -> Result<()> {
        let taken = |m: &Mount| normalize(&m.guest).map_or(m.guest == mount.guest, |p| p == Path::new(&mount.guest));
        if self.mounts.iter().any(taken) {
            return Err(Error::DuplicateMount(mount.guest));
        }
        let index = index.min(self.mounts.len());
        self.keep_harness_mount(sessions, |c| c.mounts.insert(index, mount))?;
        sessions.iter().try_for_each(|session| Bound { session, cage: self }.check_overlap()).inspect_err(|_| {
            self.mounts.remove(index);
        })
    }

    /// No mount may nest with a reserved dir, in either direction, even
    /// read-only: the app data dir holds the pi runtime, the credentials and
    /// the agent state of every cage. All paths are canonical.
    pub fn check_app_data(&self, reserved: &[PathBuf]) -> Result<()> {
        for m in &self.mounts {
            if let Some(dir) = reserved_overlap(&m.host, reserved) {
                return Err(Error::MountAppData {
                    cage: self.name.clone(),
                    guest: m.guest.clone(),
                    host: m.host.display().to_string(),
                    dir: dir.display().to_string(),
                });
            }
        }
        Ok(())
    }

    pub fn remove_mount(&mut self, sessions: &[&Session], guest: &str) -> Result<()> {
        self.mount_mut(guest)?;
        self.keep_harness_mount(sessions, |c| c.mounts.retain(|m| m.guest != guest))
    }

    pub fn mount_mut(&mut self, guest: &str) -> Result<&mut Mount> {
        self.mounts.iter_mut().find(|m| m.guest == guest).ok_or(Error::MountNotFound)
    }

    /// Sets the missing identities of the mounts from the folders as they
    /// are now, if they still resolve to their stored paths. Returns true if
    /// it set one.
    pub fn fill_ids(&mut self) -> bool {
        self.mounts.iter_mut().fold(false, |filled, m| fill_id(&m.host, &mut m.id) | filled)
    }
}

/// The reserved dir that `path` nests with, in either direction.
pub(crate) fn reserved_overlap<'r>(path: &Path, reserved: &'r [PathBuf]) -> Option<&'r PathBuf> {
    reserved.iter().find(|dir| path.starts_with(dir) || dir.starts_with(path))
}

/// A session and its cage: the checks and guest paths that need both.
#[derive(Clone, Copy)]
pub struct Bound<'a> {
    pub session: &'a Session,
    pub cage: &'a Cage,
}

impl Bound<'_> {
    /// The host paths of the session folder and the mounts, each with true
    /// if the guest can write it. The session folder is writable.
    fn host_paths(&self) -> impl Iterator<Item = (&Path, bool)> {
        std::iter::once((self.session.folder.as_path(), true))
            .chain(self.cage.mounts.iter().map(|m| (m.host.as_path(), !m.readonly)))
    }

    /// A guest folder that a process can start in: `/workspace` or a
    /// sub-path whose host folder exists, or a path below a mount.
    pub(crate) fn guest_dir(&self, input: &str) -> Result<String> {
        let path = normalize(input).ok_or_else(|| Error::InvalidGuestPath(input.to_owned()))?;
        if let Ok(rel) = path.strip_prefix(WORKSPACE) {
            if !self.session.folder.join(rel).is_dir() {
                return Err(Error::MissingPath(path.display().to_string()));
            }
        } else if !self.cage.guest_target_exists(&path) {
            return Err(Error::GuestDirTarget(path.display().to_string()));
        }
        Ok(path.to_string_lossy().into_owned())
    }

    /// A harness path: a sub-path of `/workspace` whose host folder exists,
    /// or a mount's guest path or a sub-path of it. [`Bound::check`] refuses
    /// a path outside them.
    pub(crate) fn harness(&self, input: &str) -> Result<String> {
        let path = normalize(input).ok_or_else(|| Error::InvalidGuestPath(input.to_owned()))?;
        if let Ok(rel) = path.strip_prefix(WORKSPACE)
            && !rel.as_os_str().is_empty()
            && !self.session.folder.join(rel).is_dir()
        {
            return Err(Error::MissingPath(path.display().to_string()));
        }
        Ok(path.to_string_lossy().into_owned())
    }

    /// The rules that every stored session and cage must meet together.
    pub fn check(&self) -> Result<()> {
        self.check_overlap()?;
        self.cage.check()?;
        let (session, cage) = (self.session, self.cage);
        if let Some(task) = session.tasks.iter().find(|t| !cage.guest_target_exists(Path::new(&t.workdir))) {
            return Err(Error::TaskWorkdir(task.title.clone(), task.workdir.clone()));
        }
        match &session.harness {
            Some(harness) if !self.harness_target_exists(Path::new(harness)) => {
                Err(Error::HarnessTarget(harness.clone()))
            }
            _ => Ok(()),
        }
    }

    fn harness_target_exists(&self, harness: &Path) -> bool {
        harness != Path::new(WORKSPACE) && self.cage.guest_target_exists(harness)
    }

    /// Two host paths may not be nested, in either direction, when either one is
    /// writable: the guest could then write through one mount into the other.
    pub fn check_overlap(&self) -> Result<()> {
        let paths: Vec<(&Path, bool)> = self.host_paths().collect();
        for (i, (a, a_rw)) in paths.iter().enumerate() {
            for (b, b_rw) in &paths[i + 1..] {
                if (*a_rw || *b_rw) && (a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(a.display().to_string(), b.display().to_string()));
                }
            }
        }
        Ok(())
    }

    /// A writable path of another session and its cage may not nest with
    /// any path of this pair, in either direction: a guest of one could then
    /// swap a folder that the other binds at start.
    pub fn check_overlap_with<'b>(&self, others: impl IntoIterator<Item = Bound<'b>>) -> Result<()> {
        let ours: Vec<&Path> = self.host_paths().map(|(p, _)| p).collect();
        for other in others {
            for (b, _) in other.host_paths().filter(|(_, rw)| *rw) {
                if let Some(a) = ours.iter().find(|a| a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(a.display().to_string(), b.display().to_string()));
                }
            }
        }
        Ok(())
    }

    /// No host path may nest with a reserved dir: see [`Cage::check_app_data`].
    /// The session folder is not a mount, so its error names only the paths.
    pub fn check_app_data(&self, reserved: &[PathBuf]) -> Result<()> {
        let folder = &self.session.folder;
        if let Some(dir) = reserved_overlap(folder, reserved) {
            return Err(Error::AppDataOverlap(folder.display().to_string(), dir.display().to_string()));
        }
        self.cage.check_app_data(reserved)
    }

    /// Resolves the stored host paths again, around a start. A path that now
    /// resolves elsewhere (for example, a swapped symlink), or to another
    /// folder at the same path (a swapped folder), is refused. A path with no
    /// stored identity is refused until the user confirms it.
    pub fn verify_paths(&self) -> Result<()> {
        let stored = std::iter::once((&self.session.folder, self.session.folder_id))
            .chain(self.cage.mounts.iter().map(|m| (&m.host, m.id)));
        for (path, id) in stored {
            let display = path.display().to_string();
            let Some(id) = id else {
                return Err(Error::PathUnverified(display));
            };
            let (now, now_id) = host_dir_with_id(&display)?;
            if now != *path || id != now_id {
                return Err(Error::PathChanged(display));
            }
        }
        self.check_overlap()
    }
}

/// The member sessions of one VM and their cage: the checks of a VM start.
/// Each member is a [`Bound`] pair, and the guest can write each member
/// folder, so no two of them may nest.
#[derive(Clone, Copy)]
pub struct Members<'a> {
    pub sessions: &'a [Session],
    pub cage: &'a Cage,
}

impl<'a> Members<'a> {
    pub(crate) fn bounds(&self) -> impl Iterator<Item = Bound<'a>> {
        let cage = self.cage;
        self.sessions.iter().map(move |session| Bound { session, cage })
    }

    fn check_members_overlap(&self) -> Result<()> {
        for (i, a) in self.sessions.iter().enumerate() {
            for b in &self.sessions[i + 1..] {
                if a.folder.starts_with(&b.folder) || b.folder.starts_with(&a.folder) {
                    return Err(Error::MountOverlap(a.folder.display().to_string(), b.folder.display().to_string()));
                }
            }
        }
        Ok(())
    }

    pub fn check(&self) -> Result<()> {
        self.bounds().try_for_each(|b| b.check())?;
        self.check_members_overlap()
    }

    pub fn verify_paths(&self) -> Result<()> {
        self.bounds().try_for_each(|b| b.verify_paths())?;
        self.check_members_overlap()
    }

    pub fn check_app_data(&self, reserved: &[PathBuf]) -> Result<()> {
        self.bounds().try_for_each(|b| b.check_app_data(reserved))
    }

    pub fn check_overlap_with<'b>(&self, others: &[Bound<'b>]) -> Result<()> {
        self.bounds().try_for_each(|b| b.check_overlap_with(others.iter().copied()))
    }

    /// `folders` are the folders of the sessions of the same cage in a
    /// different VM. Their guest can write each one, so none may nest with a
    /// path of these members. The mounts are the same in both VMs.
    pub fn check_folders_with(&self, folders: &[&Path]) -> Result<()> {
        for bound in self.bounds() {
            for (a, _) in bound.host_paths() {
                if let Some(b) = folders.iter().find(|b| a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(a.display().to_string(), b.display().to_string()));
                }
            }
        }
        Ok(())
    }
}

/// The key of the VM that `session` uses in `cage`, its effective cage: the
/// cage id for the shared VM, the stored key for an own VM. `None`: the own
/// key belongs to a different cage, so the session needs a new key.
pub fn vm_key<'a>(session: &'a Session, cage: &'a Cage) -> Option<&'a str> {
    match &session.vm {
        VmChoice::Shared => Some(&cage.id),
        VmChoice::Own { key, cage: of } if *of == cage.id => Some(key),
        VmChoice::Own { .. } => None,
    }
}

/// The lowest own-VM key of cage `cage_id` that is not `taken`:
/// `<cage id>-<n>` with n >= 2.
pub fn free_vm_key(cage_id: &str, mut taken: impl FnMut(&str) -> bool) -> String {
    (2..).map(|n| format!("{cage_id}-{n}")).find(|key| !taken(key)).expect("a free key")
}

/// Sets a missing `id` from the folder at `path` as it is now, if the path
/// still resolves to itself. Returns true if it set it.
pub(crate) fn fill_id(path: &Path, id: &mut Option<FileId>) -> bool {
    if id.is_some() {
        return false;
    }
    match host_dir_with_id(&path.display().to_string()) {
        Ok((now, now_id)) if now == path => {
            *id = Some(now_id);
            true
        }
        _ => false,
    }
}

pub(crate) fn folder_name(folder: &Path) -> String {
    folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// A trimmed name of 1 to 64 characters with no control characters.
pub(crate) fn display_name(input: &str) -> Result<String> {
    let name = input.trim();
    if !(1..=64).contains(&name.chars().count()) || name.chars().any(char::is_control) {
        return Err(Error::InvalidName(name.to_owned()));
    }
    Ok(name.to_owned())
}

/// Puts `items` in the order of `ids`, which must name each item once.
/// `items` does not change on an error.
pub fn reorder<T>(items: &mut Vec<T>, ids: &[String], id: impl Fn(&T) -> &String) -> Result<()> {
    let order = ids
        .iter()
        .map(|wanted| items.iter().position(|item| id(item) == wanted).ok_or(Error::InvalidOrder))
        .collect::<Result<Vec<usize>>>()?;
    let mut unique = order.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != items.len() || order.len() != items.len() {
        return Err(Error::InvalidOrder);
    }
    let mut slots: Vec<Option<T>> = items.drain(..).map(Some).collect();
    *items = order.into_iter().map(|i| slots[i].take().expect("unique index")).collect();
    Ok(())
}

/// Resolves a host folder to its canonical path, so symlinks are followed once here
/// and the stored path is the real directory.
pub fn host_dir(path: &str) -> Result<PathBuf> {
    Ok(host_dir_with_id(path)?.0)
}

/// [`host_dir`] and the identity of the folder.
pub(crate) fn host_dir_with_id(path: &str) -> Result<(PathBuf, FileId)> {
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return Err(Error::RelativePath(path.to_owned()));
    }
    let canonical = raw.canonicalize().map_err(|_| Error::MissingPath(path.to_owned()))?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| Error::MissingPath(path.to_owned()))?;
    if !metadata.is_dir() {
        return Err(Error::NotDirectory(path.to_owned()));
    }
    Ok((canonical, FileId::of(&metadata)))
}

/// The canonical dir of a host path, as [`host_dir`] mounts it. A confirm
/// step shows it, so the user sees where a link points.
pub fn resolve_folder(path: &str) -> Result<String> {
    host_dir(path)?.into_os_string().into_string().map_err(|_| Error::MissingPath(path.to_owned()))
}

/// Accepts a hostname with at least two labels, or `*.` and such a hostname
/// that is not a public suffix (`*.com`, `*.co.uk`, `*.github.io`).
/// Returns it in lowercase.
pub fn domain(input: &str) -> Result<String> {
    let invalid = |reason: &str| Error::InvalidDomain(input.to_owned(), reason.to_owned());
    let domain = input.trim().to_ascii_lowercase();
    let host = domain.strip_prefix("*.").unwrap_or(&domain);
    if host.len() > 253 {
        return Err(invalid("longer than 253 characters"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(invalid("needs at least two labels, like example.com"));
    }
    let valid_label = |l: &&str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if !labels.iter().all(valid_label) {
        return Err(invalid("use only letters, digits, hyphens and dots, with no scheme, path or port"));
    }
    if host.len() < domain.len() && psl::suffix_str(host) == Some(host) {
        return Err(invalid("a wildcard may not cover a public suffix"));
    }
    Ok(domain)
}

/// The wildcard of the registrable domain of `host`, as `*.bbc.co.uk` for
/// `news.bbc.co.uk`. `None` if `host` is the registrable domain itself.
pub fn wildcard_for(host: &str) -> Option<String> {
    let registrable = psl::domain_str(host)?;
    (registrable.len() < host.len()).then(|| format!("*.{registrable}"))
}

/// An absolute guest path below `/`, with no `.` or `..` and no repeated or
/// trailing slashes.
fn normalize(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let normalized = p.is_absolute()
        && p.components().skip(1).all(|c| matches!(c, Component::Normal(_)))
        && p.components().count() > 1;
    normalized.then(|| p.components().collect())
}

pub fn guest_path(path: &str) -> Result<String> {
    let p = normalize(path).ok_or_else(|| Error::InvalidGuestPath(path.to_owned()))?;
    if p.starts_with(WORKSPACE) || Path::new(WORKSPACE).starts_with(&p) {
        return Err(Error::WorkspaceOverlap(path.to_owned()));
    }
    if p.starts_with(agent::RESERVED) || Path::new(agent::RESERVED).starts_with(&p) {
        return Err(Error::ReservedGuestPath(path.to_owned()));
    }
    Ok(p.to_string_lossy().into_owned())
}

/// The top-level folders of a session folder, for display: real directories
/// only (no symlinks, no hidden names), sorted, at most `FOLDER_LIMIT`.
pub fn list_folders(folder: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string()
            && !name.starts_with('.')
        {
            names.push(name);
        }
    }
    names.sort();
    names.truncate(FOLDER_LIMIT);
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session and its own cage, as the app makes them.
    #[derive(Clone)]
    struct Pair {
        session: Session,
        cage: Cage,
    }

    impl Pair {
        fn new(folder: &str) -> Self {
            let session = Session::new(folder, Some("c".into()), []).unwrap();
            let cage = Cage::new("c".into(), session.name.clone());
            Self { session, cage }
        }

        fn bound(&self) -> Bound<'_> {
            Bound { session: &self.session, cage: &self.cage }
        }

        fn add_mount(&mut self, host: &str, guest: &str, readonly: bool) -> Result<()> {
            self.cage.add_mount(&[&self.session], host, guest, readonly)
        }

        fn remove_mount(&mut self, guest: &str) -> Result<()> {
            self.cage.remove_mount(&[&self.session], guest)
        }

        fn mount_mut(&mut self, guest: &str) -> Result<&mut Mount> {
            self.cage.mount_mut(guest)
        }

        fn set_harness(&mut self, harness: Option<&str>) -> Result<()> {
            self.session.set_harness(&self.cage, harness)
        }

        fn fill_ids(&mut self) -> bool {
            self.session.fill_ids() | self.cage.fill_ids()
        }

        fn check(&self) -> Result<()> {
            self.bound().check()
        }

        fn check_overlap(&self) -> Result<()> {
            self.bound().check_overlap()
        }

        fn verify_paths(&self) -> Result<()> {
            self.bound().verify_paths()
        }

        fn check_app_data(&self, reserved: &[PathBuf]) -> Result<()> {
            self.bound().check_app_data(reserved)
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn host_dir_rejects_relative_path() {
        assert!(matches!(host_dir("some/dir"), Err(Error::RelativePath(_))));
    }

    #[test]
    fn host_dir_rejects_missing_path() {
        let missing = temp_dir("missing").join("nope");
        assert!(matches!(host_dir(missing.to_str().unwrap()), Err(Error::MissingPath(_))));
    }

    #[test]
    fn host_dir_rejects_file() {
        let file = temp_dir("file").join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(host_dir(file.to_str().unwrap()), Err(Error::NotDirectory(_))));
    }

    #[test]
    fn host_dir_canonicalizes_symlink() {
        let dir = temp_dir("link");
        let target = dir.join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.join("alias");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(host_dir(link.to_str().unwrap()).unwrap(), target.canonicalize().unwrap());
    }

    #[test]
    fn resolve_folder_returns_the_link_target() {
        let dir = temp_dir("resolve");
        let target = dir.join("real");
        std::fs::create_dir(&target).unwrap();
        let link = dir.join("alias");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let canonical = target.canonicalize().unwrap();
        assert_eq!(resolve_folder(link.to_str().unwrap()).unwrap(), canonical.to_str().unwrap());
        assert_eq!(resolve_folder(canonical.to_str().unwrap()).unwrap(), canonical.to_str().unwrap());
        assert!(matches!(resolve_folder("rel"), Err(Error::RelativePath(_))));
        assert!(matches!(resolve_folder(dir.join("nope").to_str().unwrap()), Err(Error::MissingPath(_))));
    }

    #[test]
    fn guest_path_rejects_workspace_overlap() {
        for path in ["/workspace", "/workspace/sub", "/"] {
            assert!(guest_path(path).is_err(), "{path}");
        }
        assert!(matches!(guest_path("/workspace/x"), Err(Error::WorkspaceOverlap(_))));
    }

    #[test]
    fn guest_path_rejects_relative_and_dot_segments() {
        for path in ["data", "/data/../workspace", "/mnt/.."] {
            assert!(matches!(guest_path(path), Err(Error::InvalidGuestPath(_))), "{path}");
        }
    }

    #[test]
    fn guest_path_rejects_the_zwinger_prefix() {
        for path in ["/opt", "/opt/zwinger", "/opt/zwinger/pi", "/opt/zwinger/state/x"] {
            assert!(matches!(guest_path(path), Err(Error::ReservedGuestPath(_))), "{path}");
        }
        assert_eq!(guest_path("/opt/tools").unwrap(), "/opt/tools");
    }

    #[test]
    fn app_data_may_not_nest_with_any_path() {
        let (mut project, root) = project_with_dirs(&["project", "data", "data/app", "other"]);
        project.add_mount(&path(&root, "other"), "/mnt/other", true).unwrap();
        let app = [root.join("data/app")];
        assert!(project.check_app_data(&app).is_ok());

        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        assert!(matches!(
            project.check_app_data(&app),
            Err(Error::MountAppData { guest, .. }) if guest == "/mnt/data"
        ));
        assert!(matches!(project.check_app_data(&[project.session.folder.join("x")]), Err(Error::AppDataOverlap(..))));
    }

    #[test]
    fn app_data_check_covers_each_reserved_dir() {
        let (project, root) = project_with_dirs(&["project", "data", "home/.zwinger"]);
        let reserved = [root.join("data"), root.join("home/.zwinger")];
        assert!(project.check_app_data(&reserved).is_ok());

        for (folder, dir) in [("/", "data"), ("home", "home/.zwinger"), ("home/.zwinger", "home/.zwinger")] {
            let mut other = project.clone();
            other.session.folder = if folder == "/" { PathBuf::from("/") } else { root.join(folder) };
            let error = other.check_app_data(&reserved).unwrap_err();
            assert!(
                matches!(&error, Error::AppDataOverlap(_, d) if *d == root.join(dir).display().to_string()),
                "{error}"
            );
        }
    }

    #[test]
    fn guest_path_accepts_and_trims() {
        assert_eq!(guest_path("//mnt/data/").unwrap(), "/mnt/data");
        assert_eq!(guest_path("/workspaces").unwrap(), "/workspaces");
    }

    #[test]
    fn add_mount_rejects_duplicate_guest_path() {
        let mut project = Pair::new(temp_dir("dup").to_str().unwrap());
        let data = temp_dir("dup-data");
        project.add_mount(data.to_str().unwrap(), "/mnt/a", true).unwrap();
        assert!(matches!(project.add_mount(data.to_str().unwrap(), "/mnt/a/", false), Err(Error::DuplicateMount(_))));
    }

    fn project_with_dirs(names: &[&str]) -> (Pair, PathBuf) {
        let root = temp_dir("overlap").canonicalize().unwrap();
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let project = Pair::new(root.join("project").to_str().unwrap());
        (project, root)
    }

    fn path(root: &Path, name: &str) -> String {
        root.join(name).to_str().unwrap().to_owned()
    }

    #[test]
    fn mount_inside_folder_is_rejected() {
        let (mut project, root) = project_with_dirs(&["project/sub"]);
        assert!(matches!(
            project.add_mount(&path(&root, "project/sub"), "/mnt/sub", true),
            Err(Error::MountOverlap(..))
        ));
        assert!(project.cage.mounts.is_empty());
    }

    #[test]
    fn mount_containing_folder_is_rejected() {
        let (mut project, root) = project_with_dirs(&["project"]);
        assert!(matches!(project.add_mount(root.to_str().unwrap(), "/mnt/root", true), Err(Error::MountOverlap(..))));
    }

    #[test]
    fn mount_inside_rw_mount_is_rejected() {
        let (mut project, root) = project_with_dirs(&["project", "data/inner"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", false).unwrap();
        assert!(matches!(
            project.add_mount(&path(&root, "data/inner"), "/mnt/inner", true),
            Err(Error::MountOverlap(..))
        ));
    }

    #[test]
    fn project_inside_other_projects_folder_is_rejected() {
        let (mut other, root) = project_with_dirs(&["project/inner", "data"]);
        other.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        let inner = Pair::new(&path(&root, "project/inner"));
        assert!(matches!(inner.bound().check_overlap_with([other.bound()]), Err(Error::MountOverlap(..))));
        let data = Pair::new(&path(&root, "data"));
        data.bound().check_overlap_with([other.bound()]).unwrap();
    }

    /// The project of `project_with_dirs` and a second session of its cage at `name`.
    fn two_members(project: &Pair, root: &Path, name: &str) -> Vec<Session> {
        let other = Session::new(&path(root, name), Some("c".into()), [project.session.slug.as_str()]).unwrap();
        vec![project.session.clone(), other]
    }

    #[test]
    fn nested_member_folders_are_rejected() {
        let (project, root) = project_with_dirs(&["project/inner", "other"]);
        let nested = two_members(&project, &root, "project/inner");
        let members = Members { sessions: &nested, cage: &project.cage };
        assert!(matches!(members.check(), Err(Error::MountOverlap(..))));
        assert!(matches!(members.verify_paths(), Err(Error::MountOverlap(..))));
        let apart = two_members(&project, &root, "other");
        let members = Members { sessions: &apart, cage: &project.cage };
        members.check().unwrap();
        members.verify_paths().unwrap();
    }

    #[test]
    fn a_mount_may_not_nest_with_any_member_folder() {
        let (mut project, root) = project_with_dirs(&["project", "other/data"]);
        project.add_mount(&path(&root, "other/data"), "/mnt/data", true).unwrap();
        let sessions = two_members(&project, &root, "other");
        let members = Members { sessions: &sessions, cage: &project.cage };
        assert!(matches!(members.check(), Err(Error::MountOverlap(..))));
        assert!(matches!(members.check_overlap_with(&[]), Ok(())));
        let outside = Pair::new(&path(&root, "other/data"));
        assert!(matches!(members.check_overlap_with(&[outside.bound()]), Err(Error::MountOverlap(..))));
    }

    #[test]
    fn identity_and_app_data_are_checked_per_member() {
        let (project, root) = project_with_dirs(&["project", "other"]);
        let sessions = two_members(&project, &root, "other");
        let members = Members { sessions: &sessions, cage: &project.cage };
        members.verify_paths().unwrap();
        assert!(matches!(members.check_app_data(&[root.join("other")]), Err(Error::AppDataOverlap(..))));
        std::fs::rename(root.join("other"), root.join("was")).unwrap();
        std::fs::create_dir(root.join("other")).unwrap();
        assert!(matches!(members.verify_paths(), Err(Error::PathChanged(p)) if p.ends_with("other")));
    }

    #[test]
    fn the_folders_of_other_vms_of_the_cage_may_not_nest() {
        let (mut project, root) = project_with_dirs(&["project/inner", "other", "data/sub"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        let sessions = [project.session.clone()];
        let members = Members { sessions: &sessions, cage: &project.cage };
        members.check_folders_with(&[&root.join("other")]).unwrap();
        for nested in ["project/inner", "data/sub", "data"] {
            let folder = root.join(nested);
            assert!(matches!(members.check_folders_with(&[&folder]), Err(Error::MountOverlap(..))), "{nested}");
        }
    }

    #[test]
    fn the_vm_key_is_the_cage_id_or_the_own_key_of_the_cage() {
        let (mut project, _) = project_with_dirs(&["project"]);
        assert_eq!(vm_key(&project.session, &project.cage), Some("c"));
        project.session.vm = VmChoice::Own { key: "c-2".into(), cage: "c".into() };
        assert_eq!(vm_key(&project.session, &project.cage), Some("c-2"));
        project.session.vm = VmChoice::Own { key: "x-2".into(), cage: "x".into() };
        assert_eq!(vm_key(&project.session, &project.cage), None);
    }

    #[test]
    fn a_free_vm_key_is_the_lowest_unused_number_from_two() {
        assert_eq!(free_vm_key("a-b", |_| false), "a-b-2");
        assert_eq!(free_vm_key("a-b", |k| ["a-b-2", "a-b-3"].contains(&k)), "a-b-4");
        assert_eq!(free_vm_key("a-b", |k| k == "a-b-3"), "a-b-2");
    }

    #[test]
    fn nested_readonly_mounts_are_accepted() {
        let (mut project, root) = project_with_dirs(&["project", "data/inner"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        project.add_mount(&path(&root, "data/inner"), "/mnt/inner", true).unwrap();
        assert_eq!(project.cage.mounts.len(), 2);
    }

    #[test]
    fn readwrite_toggle_that_creates_overlap_is_rejected() {
        let (mut project, root) = project_with_dirs(&["project", "data/inner"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        project.add_mount(&path(&root, "data/inner"), "/mnt/inner", true).unwrap();
        project.mount_mut("/mnt/data").unwrap().readonly = false;
        assert!(matches!(project.check_overlap(), Err(Error::MountOverlap(..))));
    }

    #[test]
    fn verify_paths_detects_symlink_swap() {
        let (mut project, root) = project_with_dirs(&["project", "data", "elsewhere"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        project.verify_paths().unwrap();

        std::fs::remove_dir(root.join("data")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("data")).unwrap();
        assert!(matches!(project.verify_paths(), Err(Error::PathChanged(_))));
    }

    #[test]
    fn verify_paths_detects_a_swapped_folder_at_the_same_path() {
        let (mut project, root) = project_with_dirs(&["project", "data"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        assert!(project.session.folder_id.is_some() && project.cage.mounts[0].id.is_some());

        std::fs::rename(root.join("data"), root.join("old")).unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        assert!(matches!(project.verify_paths(), Err(Error::PathChanged(p)) if p.ends_with("data")));

        std::fs::remove_dir(root.join("data")).unwrap();
        std::fs::rename(root.join("old"), root.join("data")).unwrap();
        project.verify_paths().unwrap();
        std::fs::rename(root.join("project"), root.join("was")).unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        assert!(matches!(project.verify_paths(), Err(Error::PathChanged(p)) if p.ends_with("project")));
    }

    #[test]
    fn fill_ids_sets_only_missing_ids_of_unmoved_folders() {
        let (mut project, root) = project_with_dirs(&["project", "data", "gone"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        project.add_mount(&path(&root, "gone"), "/mnt/gone", true).unwrap();
        let ids = (project.session.folder_id, project.cage.mounts[0].id);
        project.session.folder_id = None;
        project.cage.mounts[0].id = None;
        project.cage.mounts[1].id = None;
        std::fs::remove_dir(root.join("gone")).unwrap();
        assert!(project.fill_ids());
        assert_eq!(
            (project.session.folder_id, project.cage.mounts[0].id, project.cage.mounts[1].id),
            (ids.0, ids.1, None)
        );
        assert!(!project.fill_ids());
        // A folder that appears later at the path is refused until it is confirmed.
        std::fs::create_dir(root.join("gone")).unwrap();
        assert!(matches!(project.verify_paths(), Err(Error::PathUnverified(p)) if p.ends_with("gone")));
        assert!(project.fill_ids());
        project.verify_paths().unwrap();
    }

    #[test]
    fn verify_paths_detects_missing_folder() {
        let (project, root) = project_with_dirs(&["project"]);
        std::fs::remove_dir(root.join("project")).unwrap();
        assert!(matches!(project.verify_paths(), Err(Error::MissingPath(_))));
    }

    #[test]
    fn set_harness_accepts_workspace_folders_and_mounts() {
        let (mut project, root) = project_with_dirs(&["project/agent", "data"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        for (input, stored) in
            [("//workspace/agent/", "/workspace/agent"), ("/mnt/data", "/mnt/data"), ("/mnt/data/sub", "/mnt/data/sub")]
        {
            project.set_harness(Some(input)).unwrap();
            assert_eq!(project.session.harness.as_deref(), Some(stored));
        }
        project.set_harness(None).unwrap();
        assert_eq!(project.session.harness, None);
    }

    #[test]
    fn set_harness_rejects_other_paths() {
        let (mut project, _) = project_with_dirs(&["project"]);
        for input in ["agent", "/workspace/../etc", "/"] {
            assert!(matches!(project.set_harness(Some(input)), Err(Error::InvalidGuestPath(_))), "{input}");
        }
        assert!(matches!(project.set_harness(Some("/workspace/missing")), Err(Error::MissingPath(_))));
        for input in ["/workspace", "/mnt/data", "/etc"] {
            assert!(matches!(project.set_harness(Some(input)), Err(Error::HarnessTarget(_))), "{input}");
        }
    }

    #[test]
    fn check_refuses_harness_without_its_mount() {
        let (mut project, root) = project_with_dirs(&["project", "data"]);
        project.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        project.set_harness(Some("/mnt/data")).unwrap();
        project.cage.mounts.clear();
        assert!(matches!(project.check(), Err(Error::HarnessTarget(h)) if h == "/mnt/data"));
    }

    #[test]
    fn mount_change_that_moves_the_harness_is_refused() {
        let (mut project, root) = project_with_dirs(&["project", "outer", "inner"]);
        project.add_mount(&path(&root, "outer"), "/mnt", true).unwrap();
        project.add_mount(&path(&root, "inner"), "/mnt/sub", true).unwrap();
        project.set_harness(Some("/mnt/sub/agent")).unwrap();
        assert!(matches!(
            project.remove_mount("/mnt/sub"),
            Err(Error::HarnessMount(h, m)) if h == "/mnt/sub/agent" && m == "/mnt/sub"
        ));
        assert_eq!(project.cage.mounts.len(), 2);
        project.remove_mount("/mnt").unwrap();

        project.set_harness(Some("/mnt/sub/deeper")).unwrap();
        assert!(matches!(
            project.add_mount(&path(&root, "outer"), "/mnt/sub/deeper", true),
            Err(Error::HarnessMount(..))
        ));
        assert_eq!(project.cage.mounts.len(), 1);
    }

    #[test]
    fn reorder_needs_each_id_once() {
        let ids = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let mut items = ids(&["a", "b", "c"]);
        reorder(&mut items, &ids(&["c", "a", "b"]), |s| s).unwrap();
        assert_eq!(items, ["c", "a", "b"]);
        for bad in [&["c", "a"][..], &["c", "a", "a"], &["c", "a", "b", "b"], &["c", "a", "x"]] {
            assert!(matches!(reorder(&mut items, &ids(bad), |s| s), Err(Error::InvalidOrder)), "{bad:?}");
            assert_eq!(items, ["c", "a", "b"]);
        }
    }

    #[test]
    fn rename_trims_and_limits() {
        let (mut project, _) = project_with_dirs(&["project"]);
        project.session.rename("  api  ").unwrap();
        assert_eq!(project.session.name, "api");
        project.session.rename(&"ü".repeat(64)).unwrap();
        for name in ["a\nb", "tab\there", &"x".repeat(65)] {
            assert!(matches!(project.session.rename(name), Err(Error::InvalidName(_))), "{name:?}");
        }
        assert_eq!(project.session.name, "ü".repeat(64));
        for name in ["", "   "] {
            project.session.rename(name).unwrap();
            assert_eq!(project.session.name, "project");
        }
    }

    #[test]
    fn list_folders_returns_visible_real_directories() {
        let (_, root) = project_with_dirs(&["project/web", "project/api", "project/.git", "outside"]);
        let folder = root.join("project");
        std::fs::write(folder.join("README.md"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), folder.join("link")).unwrap();
        assert_eq!(list_folders(&folder).unwrap(), ["api", "web"]);
    }

    #[test]
    fn list_folders_caps_the_count() {
        let (_, root) = project_with_dirs(&["project"]);
        let folder = root.join("project");
        for i in 0..60 {
            std::fs::create_dir(folder.join(format!("d{i:02}"))).unwrap();
        }
        let names = list_folders(&folder).unwrap();
        assert_eq!(names.len(), FOLDER_LIMIT);
        assert_eq!(names[0], "d00");
        assert_eq!(names[49], "d49");
    }

    #[test]
    fn domain_accepts_hostnames_and_wildcards() {
        assert_eq!(domain(" Registry.NPMjs.org ").unwrap(), "registry.npmjs.org");
        assert_eq!(domain("*.anthropic.com").unwrap(), "*.anthropic.com");
        assert_eq!(domain("a-b.c1.io").unwrap(), "a-b.c1.io");
    }

    #[test]
    fn wildcard_for_uses_the_registrable_domain() {
        assert_eq!(wildcard_for("bbc.co.uk"), None);
        assert_eq!(wildcard_for("news.bbc.co.uk").as_deref(), Some("*.bbc.co.uk"));
        assert_eq!(wildcard_for("x.user.github.io").as_deref(), Some("*.user.github.io"));
        assert_eq!(wildcard_for("google.com"), None);
        assert_eq!(wildcard_for("a.b.example.com").as_deref(), Some("*.example.com"));
    }

    #[test]
    fn domain_rejects_other_input() {
        for input in [
            "",
            "localhost",
            "*.com",
            "*.co.uk",
            "*.github.io",
            "https://example.com",
            "example.com/path",
            "example.com:443",
            "exa mple.com",
            "*example.com",
            "a.*.example.com",
            "-bad.example.com",
            "bad-.example.com",
            "example..com",
            "example.com.",
            "ex_ample.com",
        ] {
            assert!(matches!(domain(input), Err(Error::InvalidDomain(..))), "{input:?}");
        }
    }

    #[test]
    fn host_agents_keep_one_entry_and_one_login_per_kind() {
        let mut project = Pair::new(temp_dir("host").to_str().unwrap());
        let login = |agent| Secret {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            hosts: vec!["api.anthropic.com".into()],
            places: secrets::Places { header_fields: vec!["authorization".into()], query: false, body: false },
            origin: Origin::HostLogin { agent },
        };
        let claude = |profile, login| HostAgentUse { kind: AgentKind::Claude, profile, login };
        project.cage.put_host_agent(claude(true, true), Some(login(AgentKind::Claude)));
        project.cage.put_host_agent(claude(false, true), Some(login(AgentKind::Claude)));
        project.check().unwrap();
        assert_eq!((project.cage.host_agents.len(), project.cage.logins.len()), (1, 0));
        assert!(!project.cage.host_agents[0].profile);

        // The login off removes its secret.
        project.cage.put_host_agent(claude(false, false), None);
        assert!(project.cage.logins.is_empty());
        project.check().unwrap();

        // A host-login secret with no host login, and two entries of a kind, are refused.
        project.cage.logins.push(login(AgentKind::Claude));
        assert!(matches!(project.check(), Err(Error::InvalidSecret(..))));
        project.cage.remove_host_agent(AgentKind::Claude);
        assert!(project.cage.logins.is_empty() && project.cage.host_agents.is_empty());
        project.cage.host_agents = vec![claude(false, false), claude(true, false)];
        assert!(matches!(project.check(), Err(Error::DuplicateHostAgent(_))));
    }

    #[test]
    fn pi_copilot_login_conflicts_with_a_github_api_credential() {
        use crate::network::{Action, Credential, Inject, RuleDraft, Target};

        let mut project = Pair::new(temp_dir("copilot").to_str().unwrap());
        let pi = HostAgentUse { kind: AgentKind::Pi, profile: false, login: true };
        project.cage.put_host_agent(pi, Some(login::secret(AgentKind::Pi, "api.business.githubcopilot.com")));
        // Allowing GitHub is fine: the proxy blocks api.github.com anyway.
        let github = Target::Service { id: "github".into() };
        let rule =
            project.cage.network.put_rule(RuleDraft { id: None, target: github, action: Action::Allow }).unwrap();
        project.check().unwrap();

        let token = Credential {
            name: "GITHUB_TOKEN".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
            source: crate::network::Source::Local,
        };
        project.cage.network.rule_mut(&rule).unwrap().credentials.push(token.clone());
        let err = project.check().unwrap_err();
        assert!(matches!(&err, Error::RefreshHostConflict(h, _) if h == "api.github.com"), "{err}");
        // On a Block rule the credential is inactive, so it may stay.
        project.cage.network.rule_mut(&rule).unwrap().action = Action::Block;
        project.check().unwrap();
        project.cage.network.rule_mut(&rule).unwrap().action = Action::Allow;
        assert!(matches!(project.check(), Err(Error::RefreshHostConflict(..))));
        // Without the login, the GitHub API is open again.
        project.cage.remove_host_agent(AgentKind::Pi);
        project.check().unwrap();
        // The Claude token may only bind to its exact API host.
        let claude = HostAgentUse { kind: AgentKind::Claude, profile: false, login: true };
        project.cage.put_host_agent(claude, Some(login::secret(AgentKind::Claude, "api.anthropic.com")));
        project.cage.network.rule_mut(&rule).unwrap().credentials[0].name = "CLAUDE_CODE_OAUTH_TOKEN".into();
        assert!(matches!(project.check(), Err(Error::InvalidSecret(..))));
    }

    #[test]
    fn claude_binding_refuses_other_hosts_and_templates() {
        use crate::network::{Action, Credential, Inject, RuleDraft, Target};

        let mut cage = Cage::new("c".into(), "proj".into());
        cage.put_host_agent(HostAgentUse { kind: AgentKind::Claude, profile: false, login: true }, None);
        let rule = cage
            .network
            .put_rule(RuleDraft {
                id: None,
                target: Target::Host { host: "other.example.com".into() },
                action: Action::Allow,
            })
            .unwrap();
        cage.network.rule_mut(&rule).unwrap().credentials.push(Credential {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
            source: crate::network::Source::Local,
        });
        assert!(matches!(cage.check(), Err(Error::InvalidSecret(..))));
        let rule = cage
            .network
            .put_rule(RuleDraft {
                id: Some(rule),
                target: Target::Host { host: "api.anthropic.com".into() },
                action: Action::Allow,
            })
            .unwrap();
        cage.network.rule_mut(&rule).unwrap().credentials[0].inject =
            Inject::Set { header: "x-api-key".into(), template: "{secret}".into() };
        assert!(matches!(cage.check(), Err(Error::InvalidSecret(..))));
        cage.network.rule_mut(&rule).unwrap().credentials[0].inject =
            Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() };
        assert!(cage.check().is_ok());
    }
}
