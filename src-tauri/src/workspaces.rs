use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cage::display_name;
use crate::error::{Error, Result};

/// The store file in the app data dir, next to the sessions file.
pub const FILE: &str = "workspaces.json";
/// The name of a new workspace, and of a workspace whose name is cleared.
pub const DEFAULT_NAME: &str = "Workspace";

/// A named group of sessions in the rail. Each session is in exactly one
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub sessions: Vec<String>,
    /// The cage of each session of the workspace that inherits its cage.
    #[serde(default)]
    pub default_cage: Option<String>,
}

impl Workspace {
    pub fn new(sessions: Vec<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), name: DEFAULT_NAME.to_owned(), sessions, default_cage: None }
    }
}

/// The wanted sessions of one workspace in the wanted workspace order.
#[derive(Debug, Deserialize)]
pub struct Arrangement {
    pub id: String,
    pub sessions: Vec<String>,
}

pub struct WorkspaceStore {
    path: PathBuf,
    /// Set when `load` fails: the file on disk is then not ours to replace.
    locked: bool,
}

impl WorkspaceStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, locked: false }
    }

    /// `None` if the file does not exist yet, or if it did not parse and was
    /// moved to `workspaces.json.bad`. If the read or that move fails, the
    /// error is returned, the file stays as it is and each later `save` is
    /// refused.
    pub fn load(&mut self) -> Result<Option<Vec<Workspace>>> {
        let loaded = self.read();
        self.locked = loaded.is_err();
        loaded
    }

    fn read(&self) -> Result<Option<Vec<Workspace>>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice(&bytes) {
            Ok(workspaces) => Ok(Some(workspaces)),
            Err(e) => {
                let bad = self.path.with_extension("json.bad");
                eprintln!("the workspaces file does not parse, it moves to {}: {e}", bad.display());
                std::fs::rename(&self.path, &bad)?;
                Ok(None)
            }
        }
    }

    pub fn save(&self, workspaces: &[Workspace]) -> Result<()> {
        if self.locked {
            return Err(Error::WorkspacesLocked(self.path.display().to_string()));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(workspaces)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Makes the stored workspaces match the sessions: unknown and repeated
/// session ids go, missing ones go to the end of the first workspace. With no
/// stored list, one default workspace holds all sessions in their order. The
/// result is never empty.
pub fn reconcile(stored: Option<Vec<Workspace>>, session_ids: &[String]) -> Vec<Workspace> {
    let Some(mut workspaces) = stored.filter(|w| !w.is_empty()) else {
        return vec![Workspace::new(session_ids.to_vec())];
    };
    let known: HashSet<&String> = session_ids.iter().collect();
    let mut seen = HashSet::new();
    for workspace in &mut workspaces {
        workspace.sessions.retain(|id| known.contains(id) && seen.insert(id.clone()));
    }
    let missing = session_ids.iter().filter(|id| !seen.contains(*id)).cloned();
    workspaces[0].sessions.extend(missing);
    workspaces
}

/// The workspaces in the order of `wanted`, which must name each workspace
/// and each session exactly once.
pub fn arrange(workspaces: &[Workspace], wanted: Vec<Arrangement>, session_ids: &[String]) -> Result<Vec<Workspace>> {
    let mut wanted_workspaces: Vec<&String> = wanted.iter().map(|a| &a.id).collect();
    let mut current: Vec<&String> = workspaces.iter().map(|w| &w.id).collect();
    let mut wanted_sessions: Vec<&String> = wanted.iter().flat_map(|a| &a.sessions).collect();
    let mut sessions: Vec<&String> = session_ids.iter().collect();
    for list in [&mut wanted_workspaces, &mut current, &mut wanted_sessions, &mut sessions] {
        list.sort_unstable();
    }
    let unique = |list: &[&String]| list.windows(2).all(|pair| pair[0] != pair[1]);
    if wanted_workspaces != current || wanted_sessions != sessions || !unique(&wanted_workspaces) || !unique(&sessions)
    {
        return Err(Error::InvalidOrder);
    }
    Ok(wanted
        .into_iter()
        .map(|a| {
            let old = workspaces.iter().find(|w| w.id == a.id);
            let name = old.map(|w| w.name.clone()).unwrap_or_default();
            let default_cage = old.and_then(|w| w.default_cage.clone());
            Workspace { id: a.id, name, sessions: a.sessions, default_cage }
        })
        .collect())
}

/// A blank name reverts to [`DEFAULT_NAME`].
pub fn workspace_name(input: &str) -> Result<String> {
    if input.trim().is_empty() { Ok(DEFAULT_NAME.to_owned()) } else { display_name(input) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn workspace(id: &str, sessions: &[&str]) -> Workspace {
        Workspace { id: id.to_owned(), name: id.to_owned(), sessions: ids(sessions), default_cage: None }
    }

    fn arrangement(id: &str, sessions: &[&str]) -> Arrangement {
        Arrangement { id: id.to_owned(), sessions: ids(sessions) }
    }

    #[test]
    fn no_file_gives_one_default_workspace_in_project_order() {
        for stored in [None, Some(Vec::new())] {
            let result = reconcile(stored, &ids(&["b", "a"]));
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].name, DEFAULT_NAME);
            assert_eq!(result[0].sessions, ids(&["b", "a"]));
        }
        assert_eq!(reconcile(None, &[]).len(), 1);
    }

    #[test]
    fn reconcile_drops_unknown_and_repeated_ids_and_appends_missing_ones() {
        let stored = vec![workspace("w1", &["a", "x", "a"]), workspace("w2", &["b", "a", "b"]), workspace("w3", &[])];
        let result = reconcile(Some(stored), &ids(&["a", "b", "c", "d"]));
        assert_eq!(result, [workspace("w1", &["a", "c", "d"]), workspace("w2", &["b"]), workspace("w3", &[])]);
    }

    #[test]
    fn arrange_needs_each_workspace_and_each_project_once() {
        let current = [workspace("w1", &["a"]), workspace("w2", &["b"])];
        let projects = ids(&["a", "b"]);
        let result =
            arrange(&current, vec![arrangement("w2", &["b", "a"]), arrangement("w1", &[])], &projects).unwrap();
        assert_eq!(result, [workspace("w2", &["b", "a"]), workspace("w1", &[])]);

        let bad = [
            vec![arrangement("w1", &["a", "b"])],
            vec![arrangement("w1", &["a"]), arrangement("w2", &["b"]), arrangement("w3", &[])],
            vec![arrangement("w1", &["a"]), arrangement("w1", &["b"])],
            vec![arrangement("w1", &["a"]), arrangement("w2", &[])],
            vec![arrangement("w1", &["a", "a"]), arrangement("w2", &["b"])],
            vec![arrangement("w1", &["a", "x"]), arrangement("w2", &["b"])],
        ];
        for wanted in bad {
            assert!(matches!(arrange(&current, wanted, &projects), Err(Error::InvalidOrder)));
        }
    }

    #[test]
    fn a_blank_name_reverts_to_the_default() {
        assert_eq!(workspace_name("  Work  ").unwrap(), "Work");
        assert_eq!(workspace_name("").unwrap(), DEFAULT_NAME);
        assert_eq!(workspace_name("   ").unwrap(), DEFAULT_NAME);
        assert!(matches!(workspace_name("a\nb"), Err(Error::InvalidName(_))));
    }

    #[test]
    fn store_round_trips_and_a_missing_file_is_none() {
        let dir = crate::cage::testing::temp_dir();
        let mut store = WorkspaceStore::new(dir.join(FILE));
        assert_eq!(store.load().unwrap(), None);
        let saved = vec![workspace("w1", &["a"])];
        store.save(&saved).unwrap();
        assert_eq!(store.load().unwrap(), Some(saved));
    }

    #[test]
    fn a_file_that_does_not_parse_moves_to_bad() {
        let dir = crate::cage::testing::temp_dir();
        let path = dir.join(FILE);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(WorkspaceStore::new(path.clone()).load().unwrap(), None);
        assert!(!path.exists());
        assert_eq!(std::fs::read(dir.join("workspaces.json.bad")).unwrap(), b"{not json");
    }

    #[test]
    fn a_failed_backup_keeps_the_file() {
        let dir = crate::cage::testing::temp_dir();
        let path = dir.join(FILE);
        std::fs::write(&path, "{not json").unwrap();
        // A non-empty directory at the backup path makes the rename fail.
        std::fs::create_dir_all(dir.join("workspaces.json.bad").join("x")).unwrap();
        let mut store = WorkspaceStore::new(path.clone());
        assert!(store.load().is_err());
        assert!(matches!(store.save(&[Workspace::new(Vec::new())]), Err(Error::WorkspacesLocked(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
    }
}
