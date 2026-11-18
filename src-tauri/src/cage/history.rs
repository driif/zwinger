//! The totals of the last successful starts of each VM, kept across app runs.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cage::progress;
use crate::error::Result;
use crate::store;

/// The history file in the app data dir. It is host-owned: never in a
/// VM's state dir, which the guest can write.
pub(crate) const FILE: &str = "start-history.json";

/// The starts kept per VM.
pub(crate) const KEPT: usize = 10;

/// One successful start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRecord {
    /// When the start ended, in ms since the Unix epoch.
    pub at: u64,
    pub total_ms: u64,
}

impl StartRecord {
    pub(crate) fn now(total_ms: u64) -> Self {
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, progress::ms);
        Self { at, total_ms }
    }
}

pub(crate) type Records = BTreeMap<String, Vec<StartRecord>>;

/// Per VM key, the last [`KEPT`] starts in the order they were recorded, oldest first.
#[derive(Debug, Default)]
pub(crate) struct StartHistory {
    records: Records,
    /// The records changed since the last snapshot for a save.
    dirty: bool,
}

impl StartHistory {
    /// A missing file gives an empty history. A file that cannot be read or
    /// parsed also does, with a log line.
    pub(crate) fn load(path: &Path) -> Self {
        let mut records = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("the start history is not valid, it starts empty: {e}");
                Records::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Records::new(),
            Err(e) => {
                eprintln!("the start history could not be read, it starts empty: {e}");
                Records::new()
            }
        };
        // A file written by hand or by another version can hold more.
        for starts in records.values_mut() {
            starts.drain(..starts.len().saturating_sub(KEPT));
        }
        Self { records, dirty: false }
    }

    pub(crate) fn of(&self, key: &str) -> Vec<StartRecord> {
        self.records.get(key).cloned().unwrap_or_default()
    }

    pub(crate) fn push(&mut self, key: &str, record: StartRecord) {
        let records = self.records.entry(key.to_owned()).or_default();
        records.push(record);
        let over = records.len().saturating_sub(KEPT);
        records.drain(..over);
        self.dirty = true;
    }

    pub(crate) fn remove(&mut self, key: &str) {
        if self.records.remove(key).is_some() {
            self.dirty = true;
        }
    }

    /// The records to save, if they changed since the last snapshot.
    pub(crate) fn snapshot(&mut self) -> Option<Records> {
        std::mem::take(&mut self.dirty).then(|| self.records.clone())
    }

    /// A save failed: the next save writes the records again.
    pub(crate) fn unsaved(&mut self) {
        self.dirty = true;
    }
}

pub(crate) fn write(path: &Path, records: &Records) -> Result<()> {
    store::write(path, records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(total_ms: u64) -> StartRecord {
        StartRecord { at: total_ms, total_ms }
    }

    #[test]
    fn the_oldest_start_goes_after_ten() {
        let mut history = StartHistory::default();
        assert!(history.of("a").is_empty());
        assert_eq!(history.snapshot(), None);
        history.push("a", record(1));
        assert_eq!(history.of("a"), [record(1)]);
        for ms in 2..=10 {
            history.push("a", record(ms));
        }
        assert_eq!(history.of("a"), (1..=10).map(record).collect::<Vec<_>>());
        history.push("a", record(11));
        history.push("a", record(12));
        assert_eq!(history.of("a"), (3..=12).map(record).collect::<Vec<_>>());
        assert!(history.of("b").is_empty());
        assert!(history.snapshot().is_some());
        assert_eq!(history.snapshot(), None);
    }

    #[test]
    fn the_insertion_order_counts_not_the_time() {
        let mut history = StartHistory::default();
        history.push("a", StartRecord { at: 50, total_ms: 1 });
        history.push("a", StartRecord { at: 10, total_ms: 2 });
        assert_eq!(history.of("a").iter().map(|r| r.total_ms).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn a_remove_of_an_unknown_key_changes_nothing() {
        let mut history = StartHistory::default();
        history.remove("a");
        assert_eq!(history.snapshot(), None);
        history.push("a", record(1));
        history.snapshot();
        history.remove("a");
        assert_eq!(history.snapshot(), Some(Records::new()));
    }

    #[test]
    fn a_saved_history_loads_and_a_bad_file_loads_empty() {
        let dir = std::env::temp_dir().join(format!("zwinger-history-{}", uuid::Uuid::new_v4()));
        let path = dir.join(FILE);
        assert!(StartHistory::load(&path).of("a").is_empty());
        let mut history = StartHistory::default();
        history.push("a", record(4200));
        write(&path, &history.snapshot().unwrap()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"totalMs\": 4200"), "{text}");
        assert_eq!(StartHistory::load(&path).of("a"), [record(4200)]);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(StartHistory::load(&path).of("a").is_empty());
    }

    #[test]
    fn a_loaded_file_keeps_only_the_newest_ten_per_vm() {
        let dir = std::env::temp_dir().join(format!("zwinger-history-{}", uuid::Uuid::new_v4()));
        let path = dir.join(FILE);
        let records =
            Records::from([("a".to_owned(), (1..=13).map(record).collect()), ("b".to_owned(), vec![record(1)])]);
        write(&path, &records).unwrap();
        let history = StartHistory::load(&path);
        assert_eq!(history.of("a"), (4..=13).map(record).collect::<Vec<_>>());
        assert_eq!(history.of("b"), [record(1)]);
    }
}

#[cfg(test)]
mod manager_tests {
    use crate::cage::testing::*;
    use crate::session::VmChoice;

    use super::{FILE, StartHistory};

    fn history(manager: &Manager<FakeRuntime>, key: &str) -> Vec<u64> {
        let cages = manager.list().cages;
        let vm = cages.iter().flat_map(|c| c.vms.iter()).find(|v| v.key == key);
        vm.map(|v| v.start_history.iter().map(|r| r.total_ms).collect()).unwrap_or_default()
    }

    fn saved(manager: &Manager<FakeRuntime>, key: &str) -> usize {
        StartHistory::load(&manager.store.dir().join(FILE)).of(key).len()
    }

    fn relaunch(manager: &Manager<FakeRuntime>) -> Manager<FakeRuntime> {
        let store = Store::new(manager.store.dir().to_owned());
        Manager::new(FakeRuntime::default(), store, vault(), temp_dir(), host()).unwrap()
    }

    async fn cycle(manager: &Manager<FakeRuntime>, id: &str) {
        manager.start(id).await.unwrap();
        manager.stop(id, None).await.unwrap();
    }

    #[tokio::test]
    async fn each_start_is_recorded_with_its_timings_and_the_last_ten_stay() {
        let (manager, _, id) = manager();
        assert!(history(&manager, &id).is_empty());
        assert!(!manager.store.dir().join(FILE).exists());
        manager.start(&id).await.unwrap();
        let view = manager.list().cages.remove(0).shared();
        // The record and the timings of the running VM come from the same start.
        assert_eq!(view.start_history.len(), 1);
        assert_eq!(view.start_history[0].total_ms, view.start_timings.unwrap().total_ms);
        assert_eq!(saved(&manager, &id), 1);
        manager.stop(&id, None).await.unwrap();
        // A stopped VM still shows its history.
        assert_eq!(manager.list().cages.remove(0).shared().status, Status::Stopped);
        assert_eq!(history(&manager, &id).len(), 1);
        for _ in 1..10 {
            cycle(&manager, &id).await;
        }
        assert_eq!(history(&manager, &id).len(), 10);
        let before = lock(&manager.state).history.of(&id);
        cycle(&manager, &id).await;
        let after = lock(&manager.state).history.of(&id);
        assert_eq!(after.len(), 10);
        assert_eq!(after[..9], before[1..]);
        assert_eq!(saved(&manager, &id), 10);
    }

    #[tokio::test]
    async fn the_history_loads_after_a_relaunch() {
        let (manager, _, id) = manager();
        cycle(&manager, &id).await;
        cycle(&manager, &id).await;
        let reloaded = relaunch(&manager);
        assert_eq!(history(&reloaded, &id), history(&manager, &id));
        assert_eq!(history(&reloaded, &id).len(), 2);
    }

    #[tokio::test]
    async fn a_corrupt_file_gives_an_empty_history_and_no_error() {
        let (manager, _, id) = manager();
        cycle(&manager, &id).await;
        std::fs::write(manager.store.dir().join(FILE), "{ not json").unwrap();
        let reloaded = relaunch(&manager);
        assert!(history(&reloaded, &id).is_empty());
    }

    #[tokio::test]
    async fn a_failed_save_does_not_block_a_start_and_the_next_save_writes_all() {
        let (manager, _, id) = manager();
        let path = manager.store.dir().join(FILE);
        // The rename onto a non-empty folder fails.
        std::fs::create_dir_all(path.join("blocker")).unwrap();
        cycle(&manager, &id).await;
        assert_eq!(history(&manager, &id).len(), 1);
        std::fs::remove_dir_all(&path).unwrap();
        cycle(&manager, &id).await;
        assert_eq!(saved(&manager, &id), 2);
    }

    #[tokio::test]
    async fn only_a_successful_start_is_recorded() {
        let (manager, runtime, id) = manager();
        *runtime.node_version.lock().unwrap() = Some("v22.12.0\n".into());
        assert!(manager.start(&id).await.is_err());
        assert!(history(&manager, &id).is_empty());
        assert!(!manager.store.dir().join(FILE).exists());
        *runtime.node_version.lock().unwrap() = None;
        manager.start(&id).await.unwrap();
        assert_eq!(history(&manager, &id).len(), 1);
    }

    #[tokio::test]
    async fn a_shared_and_an_own_vm_have_their_own_history_and_a_wipe_clears_it() {
        let (manager, _, a) = manager();
        let b = manager.create(temp_dir().to_str().unwrap(), None).unwrap();
        manager.set_session_cage(&b, Some(&a)).unwrap();
        manager.set_session_vm(&b, true, None).await.unwrap();
        let own = vm_key(&manager, &b).unwrap();
        assert_ne!(own, a);
        cycle(&manager, &a).await;
        cycle(&manager, &b).await;
        cycle(&manager, &b).await;
        assert_eq!((history(&manager, &a).len(), history(&manager, &own).len()), (1, 2));

        // Back to the shared VM: the own key is wiped, also in the file.
        manager.set_session_vm(&b, false, None).await.unwrap();
        assert!(lock(&manager.state).history.of(&own).is_empty());
        assert_eq!((saved(&manager, &a), saved(&manager, &own)), (1, 0));
        // The reused key starts empty.
        manager.set_session_vm(&b, true, None).await.unwrap();
        assert_eq!(lock(&manager.state).session(&b).unwrap().vm, VmChoice::Own { key: own.clone(), cage: a.clone() });
        assert!(history(&manager, &own).is_empty());
        assert_eq!(history(&manager, &a).len(), 1);
    }

    #[tokio::test]
    async fn a_deleted_cage_loses_its_history() {
        let (manager, _, a) = manager();
        cycle(&manager, &a).await;
        manager.delete(&a).await.unwrap();
        manager.delete_cage(&a).unwrap();
        assert!(lock(&manager.state).history.of(&a).is_empty());
        assert_eq!(saved(&manager, &a), 0);
    }
}
