pub(crate) mod apps;
pub(crate) mod lifecycle;
pub(crate) mod manager;
pub(crate) mod network;
pub(crate) mod tasks;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::{self, AgentKind};
use crate::error::{Error, Result};
use crate::host_agents::login;
use crate::network::NetworkPolicy;
use crate::runtime::SessionKind;
use crate::secrets::{self, Origin, Secret};

pub const WORKSPACE: &str = "/workspace";
const FOLDER_LIMIT: usize = 50;
const COMMAND_LIMIT: usize = 4096;
const REQUEST_LIMIT: usize = 64 << 10;
const MODEL_LIMIT: usize = 128;
const GATE_COUNT: usize = 16;
/// The saved map positions per shield; more entries are dropped.
const LAYOUT_LIMIT: usize = 1000;
/// The deepest split of a pane layout.
const PANE_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shield {
    pub id: String,
    pub name: String,
    pub folder: PathBuf,
    /// The identity of `folder` when the user picked it.
    #[serde(default)]
    pub folder_id: Option<FileId>,
    /// The load after the update to folder identities is done. After it, a
    /// folder with no identity gets one only when the user confirms it.
    #[serde(default)]
    pub ids_migrated: bool,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
    /// The load after the update to the network rules is done: the user
    /// secrets of older configs are credentials of rules since.
    #[serde(default)]
    pub network_migrated: bool,
    /// The guest folder where agent sessions start: below `/workspace` or a mount.
    #[serde(default)]
    pub harness: Option<String>,
    /// The host logins of the host agents. Older configs kept the user
    /// secrets here too; the load moves them to the rules.
    #[serde(default, alias = "secrets")]
    pub logins: Vec<Secret>,
    /// The map positions that the user dragged, by node id.
    #[serde(default)]
    pub layout: BTreeMap<String, Pos>,
    #[serde(default)]
    pub apps: Vec<App>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// The terminals of the shield, in tree order. A terminal with no live
    /// session is dormant: it opens again under the same id.
    #[serde(default)]
    pub terminals: Vec<Terminal>,
    /// The split tree of the terminal view. `None`: no pane is open.
    #[serde(default)]
    pub pane_layout: Option<PaneNode>,
    /// The agents that the shield takes from the host. At most one per kind.
    #[serde(default)]
    pub host_agents: Vec<HostAgentUse>,
    /// The agent commands skip their permission prompts: the shield is the sandbox.
    #[serde(default = "yes")]
    pub agents_unprompted: bool,
}

fn yes() -> bool {
    true
}

/// A host agent of a shield: its runtime, and if the shield also gets the
/// agent's profile (read-only) and its host login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAgentUse {
    pub kind: AgentKind,
    pub profile: bool,
    pub login: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// A saved, named, long-running command of a shield.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct App {
    pub id: String,
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub autostart: bool,
}

/// A shell or agent terminal of a shield. It stays when its process ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terminal {
    pub id: String,
    pub kind: SessionKind,
}

/// A node of a pane layout: a split in two, or a pane with one terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PaneNode {
    /// `ratio` is the share of `a`, the left or top side.
    Split {
        direction: SplitDirection,
        ratio: f64,
        a: Box<PaneNode>,
        b: Box<PaneNode>,
    },
    Leaf {
        terminal: String,
    },
}

/// `Row` puts `b` to the right of `a`, `Column` below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitDirection {
    Row,
    Column,
}

impl PaneNode {
    /// Removes the pane of `terminal`; its sibling takes the place of the
    /// split. `None` if no pane stays.
    fn without(self, terminal: &str) -> Option<Self> {
        match self {
            Self::Leaf { terminal: t } if t == terminal => None,
            Self::Leaf { .. } => Some(self),
            Self::Split { direction, ratio, a, b } => match (a.without(terminal), b.without(terminal)) {
                (Some(a), Some(b)) => Some(Self::Split { direction, ratio, a: Box::new(a), b: Box::new(b) }),
                (one, other) => one.or(other),
            },
        }
    }

    fn check<'a>(&'a self, depth: usize, terminals: &[Terminal], seen: &mut Vec<&'a str>) -> Result<()> {
        let invalid = |why: &str| Err(Error::InvalidPaneLayout(why.to_owned()));
        match self {
            Self::Leaf { terminal } => {
                if !terminals.iter().any(|t| &t.id == terminal) {
                    return invalid("a pane names an unknown terminal");
                }
                if seen.contains(&terminal.as_str()) {
                    return invalid("a terminal is in more than one pane");
                }
                seen.push(terminal);
                Ok(())
            }
            Self::Split { .. } if depth >= PANE_DEPTH => invalid("the splits are too deep"),
            Self::Split { ratio, .. } if !(0.1..=0.9).contains(ratio) => invalid("a split ratio is not 0.1 to 0.9"),
            Self::Split { a, b, .. } => {
                a.check(depth + 1, terminals, seen)?;
                b.check(depth + 1, terminals, seen)
            }
        }
    }
}

/// An app as the UI sends it. No `id` adds a new app.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDraft {
    pub id: Option<String>,
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub autostart: bool,
}

/// A request that two agents of a shield plan, build and review together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    /// The folder name of its runs. It is set once, so a rename keeps the runs.
    pub slug: String,
    pub title: String,
    pub request: String,
    /// Two agents of different kinds. The first one decides a tiebreak.
    pub agents: [TaskAgent; 2],
    /// Shell lines that the run executes after the implementation.
    pub gates: Vec<String>,
    /// The guest folder where the agents and the gates run.
    pub workdir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAgent {
    pub kind: AgentKind,
    /// `None` uses the default model of the CLI.
    pub model: Option<String>,
}

/// A task as the UI sends it. No `id` adds a new task.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDraft {
    pub id: Option<String>,
    pub title: String,
    pub request: String,
    pub agents: [TaskAgent; 2],
    pub gates: Vec<String>,
    pub workdir: String,
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

impl Shield {
    pub fn new(folder: &str) -> Result<Self> {
        let (folder, folder_id) = host_dir_with_id(folder)?;
        let name =
            folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            folder,
            folder_id: Some(folder_id),
            ids_migrated: true,
            mounts: Vec::new(),
            network: NetworkPolicy::default(),
            network_migrated: true,
            harness: None,
            logins: Vec::new(),
            layout: BTreeMap::new(),
            apps: Vec::new(),
            tasks: Vec::new(),
            terminals: Vec::new(),
            pane_layout: None,
            host_agents: Vec::new(),
            agents_unprompted: true,
        })
    }

    pub fn rename(&mut self, name: &str) -> Result<()> {
        self.name = display_name(name)?;
        Ok(())
    }

    /// Keeps only finite positions. `None` restores the auto layout.
    pub fn set_layout(&mut self, layout: Option<BTreeMap<String, Pos>>) {
        self.layout = layout
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, p)| p.x.is_finite() && p.y.is_finite())
            .take(LAYOUT_LIMIT)
            .collect();
    }

    /// Adds the app, or replaces the app with the draft's id. Returns its id.
    pub fn put_app(&mut self, draft: AppDraft) -> Result<String> {
        let name = display_name(&draft.name)?;
        let command = draft.command.trim();
        if command.is_empty() {
            return Err(Error::InvalidApp(name, "the command is empty".into()));
        }
        if command.chars().count() > COMMAND_LIMIT || command.chars().any(|c| c.is_control() && c != '\t') {
            return Err(Error::InvalidApp(
                name,
                format!("the command must be one line of at most {COMMAND_LIMIT} characters"),
            ));
        }
        let cwd = self.guest_dir(&draft.cwd)?;
        let app = App {
            id: draft.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            name,
            command: command.to_owned(),
            cwd,
            autostart: draft.autostart,
        };
        let id = app.id.clone();
        match draft.id {
            Some(existing) => *self.app_mut(&existing)? = app,
            None => self.apps.push(app),
        }
        Ok(id)
    }

    pub fn remove_app(&mut self, id: &str) -> Result<()> {
        self.app_mut(id)?;
        self.apps.retain(|a| a.id != id);
        Ok(())
    }

    /// `ids` must name each app once.
    pub fn reorder_apps(&mut self, ids: &[String]) -> Result<()> {
        reorder(&mut self.apps, ids, |a| &a.id)
    }

    /// Adds a terminal at the end and returns its id.
    pub fn add_terminal(&mut self, kind: SessionKind) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.terminals.push(Terminal { id: id.clone(), kind });
        id
    }

    /// Also removes its pane. An unknown id is not an error: the terminal is
    /// already gone.
    pub fn remove_terminal(&mut self, id: &str) {
        self.terminals.retain(|t| t.id != id);
        self.pane_layout = self.pane_layout.take().and_then(|layout| layout.without(id));
    }

    /// Each pane names a terminal of the shield, and each terminal is in at
    /// most one pane.
    pub fn set_pane_layout(&mut self, layout: Option<PaneNode>) -> Result<()> {
        if let Some(layout) = &layout {
            layout.check(0, &self.terminals, &mut Vec::new())?;
        }
        self.pane_layout = layout;
        Ok(())
    }

    /// `ids` must name each terminal once.
    pub fn reorder_terminals(&mut self, ids: &[String]) -> Result<()> {
        reorder(&mut self.terminals, ids, |t| &t.id)
    }

    pub fn terminal(&self, id: &str) -> Result<&Terminal> {
        self.terminals.iter().find(|t| t.id == id).ok_or(Error::TerminalNotFound)
    }

    pub fn app(&self, id: &str) -> Result<&App> {
        self.apps.iter().find(|a| a.id == id).ok_or(Error::AppNotFound)
    }

    fn app_mut(&mut self, id: &str) -> Result<&mut App> {
        self.apps.iter_mut().find(|a| a.id == id).ok_or(Error::AppNotFound)
    }

    /// Adds the task, or replaces the task with the draft's id. Returns its id.
    pub fn put_task(&mut self, draft: TaskDraft) -> Result<String> {
        let title = display_name(&draft.title)?;
        let invalid = |reason: String| Error::InvalidTask(title.clone(), reason);
        let request = draft.request.trim();
        if request.is_empty() || request.len() > REQUEST_LIMIT {
            return Err(invalid(format!("the request must have 1 to {REQUEST_LIMIT} bytes")));
        }
        if draft.agents[0].kind == draft.agents[1].kind {
            return Err(invalid("the two agents must be of different kinds".into()));
        }
        let mut agents = draft.agents.clone();
        for agent in &mut agents {
            agent.model = agent.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_owned);
            if let Some(model) = &agent.model
                && (model.len() > MODEL_LIMIT || !model.bytes().all(|b| b.is_ascii_graphic()))
            {
                return Err(invalid(format!("the model {model} is not valid")));
            }
        }
        let gates: Vec<String> =
            draft.gates.iter().map(|g| g.trim()).filter(|g| !g.is_empty()).map(str::to_owned).collect();
        if gates.len() > GATE_COUNT
            || gates.iter().any(|g| g.chars().count() > COMMAND_LIMIT || g.chars().any(|c| c.is_control() && c != '\t'))
        {
            return Err(invalid(format!(
                "use at most {GATE_COUNT} gates, each one line of at most {COMMAND_LIMIT} characters"
            )));
        }
        let workdir = self.guest_dir(&draft.workdir)?;
        let slug = match &draft.id {
            Some(existing) => self.task(existing)?.slug.clone(),
            None => self.new_slug(&title),
        };
        let task = Task {
            id: draft.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            slug,
            title,
            request: request.to_owned(),
            agents,
            gates,
            workdir,
        };
        let id = task.id.clone();
        match draft.id {
            Some(existing) => *self.tasks.iter_mut().find(|t| t.id == existing).ok_or(Error::TaskNotFound)? = task,
            None => self.tasks.push(task),
        }
        Ok(id)
    }

    /// A kebab-case slug of the title that no other task of the shield has.
    fn new_slug(&self, title: &str) -> String {
        let mut base = String::new();
        for c in title.chars().flat_map(char::to_lowercase) {
            if c.is_ascii_alphanumeric() {
                base.push(c);
            } else if !base.is_empty() && !base.ends_with('-') {
                base.push('-');
            }
        }
        let base = match base.trim_end_matches('-') {
            "" => "task".to_owned(),
            b => b.chars().take(40).collect::<String>().trim_end_matches('-').to_owned(),
        };
        let taken = |slug: &str| self.tasks.iter().any(|t| t.slug == slug);
        (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
            .find(|slug| !taken(slug))
            .expect("a free slug")
    }

    pub fn remove_task(&mut self, id: &str) -> Result<()> {
        self.task(id)?;
        self.tasks.retain(|t| t.id != id);
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<&Task> {
        self.tasks.iter().find(|t| t.id == id).ok_or(Error::TaskNotFound)
    }

    /// The host folder that holds a guest path (`/workspace` or a mount),
    /// the path below it, and if the guest can write it. The deepest mount
    /// that holds the path wins.
    pub fn host_of(&self, guest: &str) -> Option<(PathBuf, PathBuf, bool)> {
        let path = Path::new(guest);
        if let Ok(rel) = path.strip_prefix(WORKSPACE) {
            return Some((self.folder.clone(), rel.to_owned(), true));
        }
        let mount = self.mounts.iter().filter(|m| path.starts_with(&m.guest)).max_by_key(|m| m.guest.len())?;
        let rel = path.strip_prefix(&mount.guest).ok()?;
        Some((mount.host.clone(), rel.to_owned(), !mount.readonly))
    }

    /// A guest folder that a process can start in: `/workspace` or a
    /// sub-path whose host folder exists, or a path below a mount.
    fn guest_dir(&self, input: &str) -> Result<String> {
        let path = normalize(input).ok_or_else(|| Error::InvalidGuestPath(input.to_owned()))?;
        if let Ok(rel) = path.strip_prefix(WORKSPACE) {
            if !self.folder.join(rel).is_dir() {
                return Err(Error::MissingPath(path.display().to_string()));
            }
        } else if !self.guest_target_exists(&path) {
            return Err(Error::GuestDirTarget(path.display().to_string()));
        }
        Ok(path.to_string_lossy().into_owned())
    }

    /// Accepts a sub-path of `/workspace` whose host folder exists, or a mount's
    /// guest path or a sub-path of it. `None` clears the harness.
    pub fn set_harness(&mut self, harness: Option<&str>) -> Result<()> {
        let Some(input) = harness else {
            self.harness = None;
            return Ok(());
        };
        let path = normalize(input).ok_or_else(|| Error::InvalidGuestPath(input.to_owned()))?;
        if let Ok(rel) = path.strip_prefix(WORKSPACE)
            && !rel.as_os_str().is_empty()
            && !self.folder.join(rel).is_dir()
        {
            return Err(Error::MissingPath(path.display().to_string()));
        }
        self.harness = Some(path.to_string_lossy().into_owned());
        self.check()
    }

    /// Adds or replaces the host agent of its kind. `login` is the agent's
    /// host-login secret if the use has a login; it replaces the old one.
    pub fn put_host_agent(&mut self, used: HostAgentUse, login: Option<Secret>) {
        let kind = used.kind;
        self.logins.retain(|s| s.origin != Origin::HostLogin { agent: kind });
        self.logins.extend(login);
        match self.host_agents.iter_mut().find(|a| a.kind == kind) {
            Some(current) => *current = used,
            None => self.host_agents.push(used),
        }
    }

    /// Also removes its host-login secret.
    pub fn remove_host_agent(&mut self, kind: AgentKind) {
        self.host_agents.retain(|a| a.kind != kind);
        self.logins.retain(|s| s.origin != Origin::HostLogin { agent: kind });
    }

    pub fn host_agent(&self, kind: AgentKind) -> Option<&HostAgentUse> {
        self.host_agents.iter().find(|a| a.kind == kind)
    }

    /// The rules that every stored config must meet.
    pub fn check(&self) -> Result<()> {
        self.check_overlap()?;
        self.network.check()?;
        secrets::check_unique(&self.logins)?;
        for (i, used) in self.host_agents.iter().enumerate() {
            if self.host_agents[..i].iter().any(|a| a.kind == used.kind) {
                return Err(Error::DuplicateHostAgent(used.kind.name().to_owned()));
            }
        }
        // A host login belongs to a host agent with a login.
        for secret in &self.logins {
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
        if let Some(app) = self.apps.iter().find(|a| !self.guest_target_exists(Path::new(&a.cwd))) {
            return Err(Error::AppCwd(app.name.clone(), app.cwd.clone()));
        }
        if let Some(task) = self.tasks.iter().find(|t| !self.guest_target_exists(Path::new(&t.workdir))) {
            return Err(Error::TaskWorkdir(task.title.clone(), task.workdir.clone()));
        }
        match &self.harness {
            Some(harness) if !self.harness_target_exists(Path::new(harness)) => {
                Err(Error::HarnessTarget(harness.clone()))
            }
            _ => Ok(()),
        }
    }

    fn harness_target_exists(&self, harness: &Path) -> bool {
        harness != Path::new(WORKSPACE) && self.guest_target_exists(harness)
    }

    fn guest_target_exists(&self, path: &Path) -> bool {
        path.starts_with(WORKSPACE) || self.mounts.iter().any(|m| path.starts_with(&m.guest))
    }

    /// The guest path of the mount that holds the harness: the deepest mount
    /// whose guest path contains it. `None` for `/workspace` or no harness.
    fn harness_mount(&self) -> Option<&str> {
        let harness = Path::new(self.harness.as_deref()?);
        self.mounts.iter().filter(|m| harness.starts_with(&m.guest)).map(|m| m.guest.as_str()).max_by_key(|g| g.len())
    }

    /// Refuses a mount change that moves the harness to a different mount, so
    /// the harness cannot silently point to a different host folder.
    fn keep_harness_mount(&mut self, change: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let before = self.harness_mount().map(str::to_owned);
        let saved = self.mounts.clone();
        change(self)?;
        if self.harness_mount() != before.as_deref() {
            self.mounts = saved;
            let harness = self.harness.clone().unwrap_or_default();
            return Err(Error::HarnessMount(harness, before.unwrap_or_else(|| WORKSPACE.to_owned())));
        }
        Ok(())
    }

    pub fn add_mount(&mut self, host: &str, guest: &str, readonly: bool) -> Result<()> {
        let (host, id) = host_dir_with_id(host)?;
        let guest = guest_path(guest)?;
        if self.mounts.iter().any(|m| m.guest == guest) {
            return Err(Error::DuplicateMount(guest));
        }
        self.keep_harness_mount(|s| {
            s.mounts.push(Mount { host, id: Some(id), guest, readonly });
            Ok(())
        })?;
        self.check_overlap().inspect_err(|_| {
            self.mounts.pop();
        })
    }

    pub fn remove_mount(&mut self, guest: &str) -> Result<()> {
        self.mount_mut(guest)?;
        self.keep_harness_mount(|s| {
            s.mounts.retain(|m| m.guest != guest);
            Ok(())
        })
    }

    /// Two host paths may not be nested, in either direction, when either one is
    /// writable: the guest could then write through one mount into the other.
    /// The shield folder is writable.
    pub fn check_overlap(&self) -> Result<()> {
        let paths: Vec<(&Path, bool)> = std::iter::once((self.folder.as_path(), true))
            .chain(self.mounts.iter().map(|m| (m.host.as_path(), !m.readonly)))
            .collect();
        for (i, (a, a_rw)) in paths.iter().enumerate() {
            for (b, b_rw) in &paths[i + 1..] {
                if (*a_rw || *b_rw) && (a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(a.display().to_string(), b.display().to_string()));
                }
            }
        }
        Ok(())
    }

    /// A writable path of another shield may not nest with any path of this
    /// shield, in either direction: a guest of one shield could then swap a
    /// folder that the other shield binds at start.
    pub fn check_overlap_with<'a>(&self, others: impl IntoIterator<Item = &'a Shield>) -> Result<()> {
        let ours: Vec<&Path> =
            std::iter::once(self.folder.as_path()).chain(self.mounts.iter().map(|m| m.host.as_path())).collect();
        for other in others {
            let theirs = std::iter::once(other.folder.as_path())
                .chain(other.mounts.iter().filter(|m| !m.readonly).map(|m| m.host.as_path()));
            for b in theirs {
                if let Some(a) = ours.iter().find(|a| a.starts_with(b) || b.starts_with(a)) {
                    return Err(Error::MountOverlap(a.display().to_string(), b.display().to_string()));
                }
            }
        }
        Ok(())
    }

    /// No host path may nest with the app data dir, in either direction, even
    /// read-only: it holds the pi runtime and the agent state of every shield.
    pub fn check_app_data(&self, data: &Path) -> Result<()> {
        let paths = std::iter::once(&self.folder).chain(self.mounts.iter().map(|m| &m.host));
        for path in paths {
            if path.starts_with(data) || data.starts_with(path) {
                return Err(Error::AppDataOverlap(path.display().to_string(), data.display().to_string()));
            }
        }
        Ok(())
    }

    /// Resolves the stored host paths again, around a start. A path that now
    /// resolves elsewhere (for example, a swapped symlink), or to another
    /// folder at the same path (a swapped folder), is refused. A path with no
    /// stored identity is refused until the user confirms it.
    pub fn verify_paths(&self) -> Result<()> {
        let stored = std::iter::once((&self.folder, self.folder_id)).chain(self.mounts.iter().map(|m| (&m.host, m.id)));
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

    /// Sets the missing identities from the folders as they are now, if they
    /// still resolve to their stored paths. Returns true if it set one.
    pub fn fill_ids(&mut self) -> bool {
        let mut filled = false;
        let paths = std::iter::once((&self.folder, &mut self.folder_id))
            .chain(self.mounts.iter_mut().map(|m| (&m.host, &mut m.id)));
        for (path, id) in paths.filter(|(_, id)| id.is_none()) {
            if let Ok((now, now_id)) = host_dir_with_id(&path.display().to_string())
                && now == *path
            {
                *id = Some(now_id);
                filled = true;
            }
        }
        filled
    }

    pub fn mount_mut(&mut self, guest: &str) -> Result<&mut Mount> {
        self.mounts.iter_mut().find(|m| m.guest == guest).ok_or(Error::MountNotFound)
    }
}

/// A trimmed name of 1 to 64 characters with no control characters.
fn display_name(input: &str) -> Result<String> {
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
fn host_dir_with_id(path: &str) -> Result<(PathBuf, FileId)> {
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

/// The top-level folders of a shield folder, for display: real directories
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
        let (mut shield, root) = shield_with_dirs(&["project", "data", "data/app", "other"]);
        shield.add_mount(&path(&root, "other"), "/mnt/other", true).unwrap();
        assert!(shield.check_app_data(&root.join("data/app")).is_ok());

        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        assert!(matches!(shield.check_app_data(&root.join("data/app")), Err(Error::AppDataOverlap(..))));
        assert!(matches!(shield.check_app_data(&shield.folder.join("x")), Err(Error::AppDataOverlap(..))));
    }

    #[test]
    fn guest_path_accepts_and_trims() {
        assert_eq!(guest_path("//mnt/data/").unwrap(), "/mnt/data");
        assert_eq!(guest_path("/workspaces").unwrap(), "/workspaces");
    }

    #[test]
    fn add_mount_rejects_duplicate_guest_path() {
        let mut shield = Shield::new(temp_dir("dup").to_str().unwrap()).unwrap();
        let data = temp_dir("dup-data");
        shield.add_mount(data.to_str().unwrap(), "/mnt/a", true).unwrap();
        assert!(matches!(shield.add_mount(data.to_str().unwrap(), "/mnt/a/", false), Err(Error::DuplicateMount(_))));
    }

    fn shield_with_dirs(names: &[&str]) -> (Shield, PathBuf) {
        let root = temp_dir("overlap").canonicalize().unwrap();
        for name in names {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let shield = Shield::new(root.join("project").to_str().unwrap()).unwrap();
        (shield, root)
    }

    fn path(root: &Path, name: &str) -> String {
        root.join(name).to_str().unwrap().to_owned()
    }

    #[test]
    fn mount_inside_folder_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project/sub"]);
        assert!(matches!(
            shield.add_mount(&path(&root, "project/sub"), "/mnt/sub", true),
            Err(Error::MountOverlap(..))
        ));
        assert!(shield.mounts.is_empty());
    }

    #[test]
    fn mount_containing_folder_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project"]);
        assert!(matches!(shield.add_mount(root.to_str().unwrap(), "/mnt/root", true), Err(Error::MountOverlap(..))));
    }

    #[test]
    fn mount_inside_rw_mount_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project", "data/inner"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", false).unwrap();
        assert!(matches!(
            shield.add_mount(&path(&root, "data/inner"), "/mnt/inner", true),
            Err(Error::MountOverlap(..))
        ));
    }

    #[test]
    fn shield_inside_other_shields_folder_is_rejected() {
        let (mut other, root) = shield_with_dirs(&["project/inner", "data"]);
        other.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        let inner = Shield::new(&path(&root, "project/inner")).unwrap();
        assert!(matches!(inner.check_overlap_with([&other]), Err(Error::MountOverlap(..))));
        let data = Shield::new(&path(&root, "data")).unwrap();
        data.check_overlap_with([&other]).unwrap();
    }

    #[test]
    fn nested_readonly_mounts_are_accepted() {
        let (mut shield, root) = shield_with_dirs(&["project", "data/inner"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.add_mount(&path(&root, "data/inner"), "/mnt/inner", true).unwrap();
        assert_eq!(shield.mounts.len(), 2);
    }

    #[test]
    fn readwrite_toggle_that_creates_overlap_is_rejected() {
        let (mut shield, root) = shield_with_dirs(&["project", "data/inner"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.add_mount(&path(&root, "data/inner"), "/mnt/inner", true).unwrap();
        shield.mount_mut("/mnt/data").unwrap().readonly = false;
        assert!(matches!(shield.check_overlap(), Err(Error::MountOverlap(..))));
    }

    #[test]
    fn verify_paths_detects_symlink_swap() {
        let (mut shield, root) = shield_with_dirs(&["project", "data", "elsewhere"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.verify_paths().unwrap();

        std::fs::remove_dir(root.join("data")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("data")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::PathChanged(_))));
    }

    #[test]
    fn verify_paths_detects_a_swapped_folder_at_the_same_path() {
        let (mut shield, root) = shield_with_dirs(&["project", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        assert!(shield.folder_id.is_some() && shield.mounts[0].id.is_some());

        std::fs::rename(root.join("data"), root.join("old")).unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::PathChanged(p)) if p.ends_with("data")));

        std::fs::remove_dir(root.join("data")).unwrap();
        std::fs::rename(root.join("old"), root.join("data")).unwrap();
        shield.verify_paths().unwrap();
        std::fs::rename(root.join("project"), root.join("was")).unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::PathChanged(p)) if p.ends_with("project")));
    }

    #[test]
    fn fill_ids_sets_only_missing_ids_of_unmoved_folders() {
        let (mut shield, root) = shield_with_dirs(&["project", "data", "gone"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.add_mount(&path(&root, "gone"), "/mnt/gone", true).unwrap();
        let ids = (shield.folder_id, shield.mounts[0].id);
        shield.folder_id = None;
        shield.mounts[0].id = None;
        shield.mounts[1].id = None;
        std::fs::remove_dir(root.join("gone")).unwrap();
        assert!(shield.fill_ids());
        assert_eq!((shield.folder_id, shield.mounts[0].id, shield.mounts[1].id), (ids.0, ids.1, None));
        assert!(!shield.fill_ids());
        // A folder that appears later at the path is refused until it is confirmed.
        std::fs::create_dir(root.join("gone")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::PathUnverified(p)) if p.ends_with("gone")));
        assert!(shield.fill_ids());
        shield.verify_paths().unwrap();
    }

    #[test]
    fn verify_paths_detects_missing_folder() {
        let (shield, root) = shield_with_dirs(&["project"]);
        std::fs::remove_dir(root.join("project")).unwrap();
        assert!(matches!(shield.verify_paths(), Err(Error::MissingPath(_))));
    }

    #[test]
    fn set_harness_accepts_workspace_folders_and_mounts() {
        let (mut shield, root) = shield_with_dirs(&["project/agent", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        for (input, stored) in
            [("//workspace/agent/", "/workspace/agent"), ("/mnt/data", "/mnt/data"), ("/mnt/data/sub", "/mnt/data/sub")]
        {
            shield.set_harness(Some(input)).unwrap();
            assert_eq!(shield.harness.as_deref(), Some(stored));
        }
        shield.set_harness(None).unwrap();
        assert_eq!(shield.harness, None);
    }

    #[test]
    fn set_harness_rejects_other_paths() {
        let (mut shield, _) = shield_with_dirs(&["project"]);
        for input in ["agent", "/workspace/../etc", "/"] {
            assert!(matches!(shield.set_harness(Some(input)), Err(Error::InvalidGuestPath(_))), "{input}");
        }
        assert!(matches!(shield.set_harness(Some("/workspace/missing")), Err(Error::MissingPath(_))));
        for input in ["/workspace", "/mnt/data", "/etc"] {
            assert!(matches!(shield.set_harness(Some(input)), Err(Error::HarnessTarget(_))), "{input}");
        }
    }

    #[test]
    fn check_refuses_harness_without_its_mount() {
        let (mut shield, root) = shield_with_dirs(&["project", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.set_harness(Some("/mnt/data")).unwrap();
        shield.mounts.clear();
        assert!(matches!(shield.check(), Err(Error::HarnessTarget(h)) if h == "/mnt/data"));
    }

    #[test]
    fn mount_change_that_moves_the_harness_is_refused() {
        let (mut shield, root) = shield_with_dirs(&["project", "outer", "inner"]);
        shield.add_mount(&path(&root, "outer"), "/mnt", true).unwrap();
        shield.add_mount(&path(&root, "inner"), "/mnt/sub", true).unwrap();
        shield.set_harness(Some("/mnt/sub/agent")).unwrap();
        assert!(matches!(
            shield.remove_mount("/mnt/sub"),
            Err(Error::HarnessMount(h, m)) if h == "/mnt/sub/agent" && m == "/mnt/sub"
        ));
        assert_eq!(shield.mounts.len(), 2);
        shield.remove_mount("/mnt").unwrap();

        shield.set_harness(Some("/mnt/sub/deeper")).unwrap();
        assert!(matches!(
            shield.add_mount(&path(&root, "outer"), "/mnt/sub/deeper", true),
            Err(Error::HarnessMount(..))
        ));
        assert_eq!(shield.mounts.len(), 1);
    }

    fn draft(name: &str, command: &str, cwd: &str) -> AppDraft {
        AppDraft { id: None, name: name.into(), command: command.into(), cwd: cwd.into(), autostart: false }
    }

    #[test]
    fn put_app_validates_name_command_and_cwd() {
        let (mut shield, root) = shield_with_dirs(&["project/web", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        for cwd in ["/workspace", "//workspace/web/", "/mnt/data", "/mnt/data/sub"] {
            shield.put_app(draft(" web ", " pnpm dev ", cwd)).unwrap();
        }
        assert_eq!(shield.apps[0].name, "web");
        assert_eq!(shield.apps[0].command, "pnpm dev");
        assert_eq!(shield.apps[1].cwd, "/workspace/web");

        assert!(matches!(shield.put_app(draft("", "x", "/workspace")), Err(Error::InvalidName(_))));
        for command in ["", "   ", "a\nb", &"x".repeat(COMMAND_LIMIT + 1)] {
            assert!(
                matches!(shield.put_app(draft("a", command, "/workspace")), Err(Error::InvalidApp(..))),
                "{command:?}"
            );
        }
        assert!(matches!(shield.put_app(draft("a", "x", "workspace")), Err(Error::InvalidGuestPath(_))));
        assert!(matches!(shield.put_app(draft("a", "x", "/workspace/missing")), Err(Error::MissingPath(_))));
        assert!(matches!(shield.put_app(draft("a", "x", "/etc")), Err(Error::GuestDirTarget(_))));
        assert_eq!(shield.apps.len(), 4);
    }

    #[test]
    fn put_app_with_id_replaces_and_unknown_id_fails() {
        let (mut shield, _) = shield_with_dirs(&["project"]);
        let id = shield.put_app(draft("web", "pnpm dev", "/workspace")).unwrap();
        shield
            .put_app(AppDraft { id: Some(id.clone()), autostart: true, ..draft("api", "cargo run", "/workspace") })
            .unwrap();
        assert_eq!(shield.apps.len(), 1);
        assert_eq!((shield.apps[0].id.as_str(), shield.apps[0].name.as_str()), (id.as_str(), "api"));
        assert!(shield.apps[0].autostart);
        assert!(matches!(
            shield.put_app(AppDraft { id: Some("nope".into()), ..draft("a", "x", "/workspace") }),
            Err(Error::AppNotFound)
        ));
        shield.remove_app(&id).unwrap();
        assert!(matches!(shield.remove_app(&id), Err(Error::AppNotFound)));
    }

    #[test]
    fn check_refuses_an_app_without_its_mount() {
        let (mut shield, root) = shield_with_dirs(&["project", "data"]);
        shield.add_mount(&path(&root, "data"), "/mnt/data", true).unwrap();
        shield.put_app(draft("web", "x", "/mnt/data")).unwrap();
        shield.remove_mount("/mnt/data").unwrap();
        assert!(matches!(shield.check(), Err(Error::AppCwd(n, c)) if n == "web" && c == "/mnt/data"));
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
    fn set_layout_keeps_finite_positions() {
        let (mut shield, _) = shield_with_dirs(&["project"]);
        shield.set_layout(Some(
            [
                ("harness".to_owned(), Pos { x: 1.0, y: 2.0 }),
                ("host:a.io".to_owned(), Pos { x: f64::NAN, y: 0.0 }),
                ("host:b.io".to_owned(), Pos { x: 0.0, y: f64::INFINITY }),
            ]
            .into(),
        ));
        assert_eq!(shield.layout.keys().collect::<Vec<_>>(), ["harness"]);
        shield.set_layout(None);
        assert!(shield.layout.is_empty());
    }

    #[test]
    fn rename_trims_and_limits() {
        let (mut shield, _) = shield_with_dirs(&["project"]);
        shield.rename("  api  ").unwrap();
        assert_eq!(shield.name, "api");
        shield.rename(&"ü".repeat(64)).unwrap();
        for name in ["", "   ", "a\nb", "tab\there", &"x".repeat(65)] {
            assert!(matches!(shield.rename(name), Err(Error::InvalidName(_))), "{name:?}");
        }
        assert_eq!(shield.name, "ü".repeat(64));
    }

    #[test]
    fn list_folders_returns_visible_real_directories() {
        let (_, root) = shield_with_dirs(&["project/web", "project/api", "project/.git", "outside"]);
        let folder = root.join("project");
        std::fs::write(folder.join("README.md"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), folder.join("link")).unwrap();
        assert_eq!(list_folders(&folder).unwrap(), ["api", "web"]);
    }

    #[test]
    fn list_folders_caps_the_count() {
        let (_, root) = shield_with_dirs(&["project"]);
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
        let mut shield = Shield::new(temp_dir("host").to_str().unwrap()).unwrap();
        let login = |agent| Secret {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            hosts: vec!["api.anthropic.com".into()],
            places: secrets::Places { header_fields: vec!["authorization".into()], query: false, body: false },
            origin: Origin::HostLogin { agent },
        };
        let claude = |profile, login| HostAgentUse { kind: AgentKind::Claude, profile, login };
        shield.put_host_agent(claude(true, true), Some(login(AgentKind::Claude)));
        shield.put_host_agent(claude(false, true), Some(login(AgentKind::Claude)));
        shield.check().unwrap();
        assert_eq!((shield.host_agents.len(), shield.logins.len()), (1, 1));
        assert!(!shield.host_agents[0].profile);

        // The login off removes its secret.
        shield.put_host_agent(claude(false, false), None);
        assert!(shield.logins.is_empty());
        shield.check().unwrap();

        // A host-login secret with no host login, and two entries of a kind, are refused.
        shield.logins.push(login(AgentKind::Claude));
        assert!(matches!(shield.check(), Err(Error::InvalidSecret(..))));
        shield.remove_host_agent(AgentKind::Claude);
        assert!(shield.logins.is_empty() && shield.host_agents.is_empty());
        shield.host_agents = vec![claude(false, false), claude(true, false)];
        assert!(matches!(shield.check(), Err(Error::DuplicateHostAgent(_))));
    }

    #[test]
    fn pi_copilot_login_conflicts_with_a_github_api_credential() {
        use crate::network::{Action, Credential, Inject, RuleDraft, Target};

        let mut shield = Shield::new(temp_dir("copilot").to_str().unwrap()).unwrap();
        let pi = HostAgentUse { kind: AgentKind::Pi, profile: false, login: true };
        shield.put_host_agent(pi, Some(login::secret(AgentKind::Pi, "api.business.githubcopilot.com")));
        // Allowing GitHub is fine: the proxy blocks api.github.com anyway.
        let github = Target::Service { id: "github".into() };
        let rule = shield.network.put_rule(RuleDraft { id: None, target: github, action: Action::Allow }).unwrap();
        shield.check().unwrap();

        let token = Credential {
            name: "GITHUB_TOKEN".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
        };
        shield.network.rule_mut(&rule).unwrap().credentials.push(token.clone());
        let err = shield.check().unwrap_err();
        assert!(matches!(&err, Error::RefreshHostConflict(h, _) if h == "api.github.com"), "{err}");
        // On a Block rule the credential is inactive, so it may stay.
        shield.network.rule_mut(&rule).unwrap().action = Action::Block;
        shield.check().unwrap();
        shield.network.rule_mut(&rule).unwrap().action = Action::Allow;
        assert!(matches!(shield.check(), Err(Error::RefreshHostConflict(..))));
        // Without the login, the GitHub API is open again.
        shield.remove_host_agent(AgentKind::Pi);
        shield.check().unwrap();
        // A credential may not take the name of a host login.
        let claude = HostAgentUse { kind: AgentKind::Claude, profile: false, login: true };
        shield.put_host_agent(claude, Some(login::secret(AgentKind::Claude, "api.anthropic.com")));
        shield.network.rule_mut(&rule).unwrap().credentials[0].name = "CLAUDE_CODE_OAUTH_TOKEN".into();
        assert!(matches!(shield.check(), Err(Error::DuplicateSecret(_))));
    }
}
