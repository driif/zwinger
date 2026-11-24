//! The folders of VM keys that no session, cage or VM uses: the agent
//! state (`projects/<key>`), the wrappers (`bin/<key>`) and the profiles
//! (`profiles/<key>`) that an interrupted removal left. The user picks the
//! keys to remove in Settings > Storage. A link is never followed, and a
//! key whose old state in `shields/<key>` is not moved is never touched.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cage::manager::{Manager, lock};
use crate::cage::vm_state;
use crate::error::{Error, Result};
use crate::runtime::SandboxRuntime;

/// The parents of the folders of each VM key, below the app data dir.
const PARENTS: [&str; 3] = [vm_state::STATE_DIR, "bin", "profiles"];

/// A VM key that nothing uses, with its folders. `size` in bytes, `None`: unknown.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Leftover {
    pub key: String,
    pub folders: Vec<PathBuf>,
    pub size: Option<u64>,
}

impl<R: SandboxRuntime> Manager<R> {
    /// The VM keys with a folder that no session, cage or VM uses.
    pub fn leftovers(&self) -> Vec<Leftover> {
        let taken: HashSet<String> = lock(&self.state).keys().map(str::to_owned).collect();
        unused(&self.data, |key| taken.contains(key))
            .into_iter()
            .map(|(key, folders)| Leftover { size: folders.iter().map(|f| dir_size(f)).sum(), key, folders })
            .collect()
    }

    /// Removes the folders of each of `keys` that is still unused, and
    /// returns those keys. It refuses with [`Error::NotReaped`] while a VM of
    /// an earlier run can still mount them. Under the state lock, so no start
    /// or new own VM can take a key meanwhile.
    pub fn remove_leftovers(&self, keys: &[String]) -> Result<Vec<String>> {
        let state = lock(&self.state);
        self.check_reaped()?;
        let mut removed = Vec::new();
        for (key, folders) in unused(&self.data, |key| state.key_taken(key)) {
            if !keys.contains(&key) {
                continue;
            }
            for dir in folders {
                // `remove_dir_all` does not follow the links in the folder.
                std::fs::remove_dir_all(&dir).map_err(|source| Error::VmStateLeft {
                    key: key.clone(),
                    path: dir.display().to_string(),
                    source,
                })?;
            }
            removed.push(key);
        }
        Ok(removed)
    }
}

/// Each VM key with a real folder in a parent that is a real folder, which
/// `taken` does not have and whose state is not pending, with its folders.
fn unused(data: &Path, taken: impl Fn(&str) -> bool) -> BTreeMap<String, Vec<PathBuf>> {
    let mut found: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for parent in PARENTS {
        let root = data.join(parent);
        if !std::fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            // The file type of an entry does not follow a link.
            if vm_state::vm_key(&name) && entry.file_type().is_ok_and(|t| t.is_dir()) && !taken(&name) {
                found.entry(name).or_default().push(entry.path());
            }
        }
    }
    // A pending key, or a state path with a link, is never used.
    found.retain(|key, _| matches!(vm_state::resolve(data, key), Ok(Ok(_))));
    found
}

/// The bytes of the files below `path`. Does not follow links. `None` if a part cannot be read.
pub(crate) fn dir_size(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return Some(meta.len());
    }
    std::fs::read_dir(path).ok()?.map(|entry| dir_size(&entry.ok()?.path())).sum()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::cage::testing::*;

    fn folder(data: &Path, parent: &str, key: &str) -> PathBuf {
        let dir = data.join(parent).join(key);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), "12345").unwrap();
        dir
    }

    #[tokio::test]
    async fn lists_and_removes_only_the_folders_of_unused_keys() {
        let (manager, _, a) = manager();
        let data = manager.data.clone();
        // The cage `a` uses its key; `gone` and `gone-2` are unused.
        folder(&data, "projects", &a);
        let state = folder(&data, "projects", "gone");
        let bin = folder(&data, "bin", "gone");
        let profiles = folder(&data, "profiles", "gone-2");
        // No key, a file and a link are never listed.
        folder(&data, "bin", ".hidden");
        std::fs::write(data.join("bin/file"), "x").unwrap();
        let outside = temp_dir();
        std::fs::write(outside.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, data.join("profiles/linked")).unwrap();
        // A key whose state is not moved yet is never touched.
        folder(&data, "shields", "pending");
        let pending = folder(&data, "bin", "pending");

        let listed = manager.leftovers();
        assert_eq!(listed.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), ["gone", "gone-2"]);
        assert_eq!(listed[0].folders, [state.clone(), bin.clone()]);
        assert_eq!(listed[0].size, Some(10));

        // The reap of an earlier run failed: nothing goes.
        manager.reap_pending.store(true, Ordering::SeqCst);
        assert!(matches!(manager.remove_leftovers(&["gone".into()]), Err(Error::NotReaped { .. })));
        assert!(state.is_dir());
        manager.reap_pending.store(false, Ordering::SeqCst);

        // Only the chosen key that is still unused goes.
        let keys = ["gone".to_owned(), a.clone(), "pending".into(), "linked".into()];
        assert_eq!(manager.remove_leftovers(&keys).unwrap(), ["gone"]);
        assert!(!state.exists() && !bin.exists());
        assert!(profiles.is_dir() && pending.is_dir() && data.join("projects").join(&a).is_dir());
        assert!(outside.join("keep").exists());
    }

    #[tokio::test]
    async fn a_linked_parent_is_never_read() {
        let (manager, _, _) = manager();
        let outside = temp_dir();
        folder(&outside, "", "elsewhere");
        std::os::unix::fs::symlink(&outside, manager.data.join("bin")).unwrap();
        assert!(manager.leftovers().is_empty());
    }
}
