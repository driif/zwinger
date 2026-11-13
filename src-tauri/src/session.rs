use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::cage::{self, Bound, Cage, FileId};
use crate::error::{Error, Result};
use crate::runtime::TerminalKind;

const REQUEST_LIMIT: usize = 64 << 10;
const MODEL_LIMIT: usize = 128;
const GATE_COUNT: usize = 16;
const COMMAND_LIMIT: usize = 4096;
/// The saved map positions per session; more entries are dropped.
const LAYOUT_LIMIT: usize = 1000;
/// The deepest split of a pane layout.
const PANE_DEPTH: usize = 8;

/// One host folder with its terminals, its pane layout and its tasks. Its
/// cage is the sandbox config that a VM runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub name: String,
    pub folder: PathBuf,
    /// The identity of `folder` when the user picked it.
    #[serde(default)]
    pub folder_id: Option<FileId>,
    /// The guest folder name below `/workspace` in a VM with more sessions
    /// (ADR 0007). It is set once and does not change with a rename.
    pub slug: String,
    /// The id of the cage of the session.
    pub cage: String,
    /// The guest folder where agent terminals start: below `/workspace` or a mount.
    #[serde(default)]
    pub harness: Option<String>,
    /// In tree order. A terminal with no live process is dormant: it opens
    /// again under the same id.
    #[serde(default)]
    pub terminals: Vec<TerminalRecord>,
    /// The split tree of the terminal view. `None`: no pane is open.
    #[serde(default)]
    pub pane_layout: Option<PaneNode>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// The map positions that the user dragged, by node id.
    #[serde(default)]
    pub layout: BTreeMap<String, Pos>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// The saved record of a shell or agent terminal. It stays when its process ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRecord {
    pub id: String,
    pub kind: TerminalKind,
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

    fn check<'a>(&'a self, depth: usize, terminals: &[TerminalRecord], seen: &mut Vec<&'a str>) -> Result<()> {
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

/// A request that two agents of a session plan, build and review together.
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

impl Session {
    /// A new session of `folder` in the cage `cage`. `taken` are the slugs
    /// of all other sessions.
    pub fn new<'a>(folder: &str, cage: &str, taken: impl IntoIterator<Item = &'a str> + Clone) -> Result<Self> {
        let (folder, folder_id) = cage::host_dir_with_id(folder)?;
        let name = cage::folder_name(&folder);
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            slug: folder_slug(&folder, taken),
            name,
            folder,
            folder_id: Some(folder_id),
            cage: cage.to_owned(),
            harness: None,
            terminals: Vec::new(),
            pane_layout: None,
            tasks: Vec::new(),
            layout: BTreeMap::new(),
        })
    }

    /// A blank name reverts to the folder name.
    pub fn rename(&mut self, name: &str) -> Result<()> {
        self.name = if name.trim().is_empty() { cage::folder_name(&self.folder) } else { cage::display_name(name)? };
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

    /// Adds a terminal at the end and returns its id.
    pub fn add_terminal(&mut self, kind: TerminalKind) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.terminals.push(TerminalRecord { id: id.clone(), kind });
        id
    }

    /// Also removes its pane. An unknown id is not an error: the terminal is
    /// already gone.
    pub fn remove_terminal(&mut self, id: &str) {
        self.terminals.retain(|t| t.id != id);
        self.pane_layout = self.pane_layout.take().and_then(|layout| layout.without(id));
    }

    /// Each pane names a terminal of the session, and each terminal is in at
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
        cage::reorder(&mut self.terminals, ids, |t| &t.id)
    }

    pub fn terminal(&self, id: &str) -> Result<&TerminalRecord> {
        self.terminals.iter().find(|t| t.id == id).ok_or(Error::TerminalNotFound)
    }

    /// Accepts a sub-path of `/workspace` whose host folder exists, or a
    /// guest path of a mount of `cage` or a sub-path of it. `None` clears the harness.
    pub fn set_harness(&mut self, cage: &Cage, harness: Option<&str>) -> Result<()> {
        self.harness = match harness {
            None => None,
            Some(input) => Some(Bound { session: self, cage }.harness(input)?),
        };
        Bound { session: self, cage }.check()
    }

    /// Adds the task, or replaces the task with the draft's id. Returns its id.
    pub fn put_task(&mut self, cage: &Cage, draft: TaskDraft) -> Result<String> {
        let title = cage::display_name(&draft.title)?;
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
        let workdir = Bound { session: self, cage }.guest_dir(&draft.workdir)?;
        let slug = match &draft.id {
            Some(existing) => self.task(existing)?.slug.clone(),
            None => kebab(&title, "task", self.tasks.iter().map(|t| t.slug.as_str())),
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

    pub fn remove_task(&mut self, id: &str) -> Result<()> {
        self.task(id)?;
        self.tasks.retain(|t| t.id != id);
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<&Task> {
        self.tasks.iter().find(|t| t.id == id).ok_or(Error::TaskNotFound)
    }

    /// Sets the missing identity of the folder from the folder as it is now,
    /// if it still resolves to its stored path. Returns true if it set one.
    pub fn fill_ids(&mut self) -> bool {
        cage::fill_id(&self.folder, &mut self.folder_id)
    }
}

/// The slug of a session folder: its basename in kebab case, unique over
/// `taken`, the slugs of all sessions (ADR 0007; not only the sessions of
/// one cage, as the task of #118 says, so a session keeps it after a move
/// to a different cage).
pub fn folder_slug<'a>(folder: &Path, taken: impl IntoIterator<Item = &'a str> + Clone) -> String {
    kebab(&cage::folder_name(folder), "session", taken)
}

/// A kebab-case slug of `text` of at most 40 characters that is not in
/// `taken`: `<base>`, else `<base>-2`, `<base>-3`, ...
fn kebab<'a>(text: &str, fallback: &str, taken: impl IntoIterator<Item = &'a str> + Clone) -> String {
    let mut base = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = match base.trim_end_matches('-') {
        "" => fallback.to_owned(),
        b => b.chars().take(40).collect::<String>().trim_end_matches('-').to_owned(),
    };
    (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|slug| !taken.clone().into_iter().any(|t| t == slug))
        .expect("a free slug")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_slugs_are_kebab_case_and_unique() {
        assert_eq!(folder_slug(Path::new("/x/My App_v2"), []), "my-app-v2");
        assert_eq!(folder_slug(Path::new("/x/api"), ["api", "api-2"]), "api-3");
        assert_eq!(folder_slug(Path::new("/x/ü"), []), "session");
    }

    #[test]
    fn set_layout_keeps_finite_positions() {
        let dir = std::env::temp_dir().join(format!("zwinger-layout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Session::new(dir.to_str().unwrap(), "c", []).unwrap();
        session.set_layout(Some(
            [
                ("harness".to_owned(), Pos { x: 1.0, y: 2.0 }),
                ("host:a.io".to_owned(), Pos { x: f64::NAN, y: 0.0 }),
                ("host:b.io".to_owned(), Pos { x: 0.0, y: f64::INFINITY }),
            ]
            .into(),
        ));
        assert_eq!(session.layout.keys().collect::<Vec<_>>(), ["harness"]);
        session.set_layout(None);
        assert!(session.layout.is_empty());
    }
}
