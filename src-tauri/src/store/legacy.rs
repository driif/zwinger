//! The project records of `projects.json`, from before the split into
//! sessions and cages. They are only read, for the conversion.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::cage::{Cage, FileId, HostAgentUse, Mount, fill_id};
use crate::network::NetworkPolicy;
use crate::secrets::Secret;
use crate::session::{PaneNode, Pos, Session, Task, TerminalRecord, folder_slug};

/// Unknown fields, such as the `apps` of older builds, are dropped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Project {
    pub id: String,
    pub name: String,
    pub folder: PathBuf,
    #[serde(default)]
    pub folder_id: Option<FileId>,
    /// The load after the update to folder identities is done.
    #[serde(default)]
    pub ids_migrated: bool,
    pub mounts: Vec<Mount>,
    pub network: NetworkPolicy,
    /// The load after the update to the network rules is done.
    #[serde(default)]
    pub network_migrated: bool,
    #[serde(default)]
    pub harness: Option<String>,
    /// Older configs kept the user secrets here too.
    #[serde(default, alias = "secrets")]
    pub logins: Vec<Secret>,
    #[serde(default)]
    pub layout: BTreeMap<String, Pos>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub terminals: Vec<TerminalRecord>,
    #[serde(default)]
    pub pane_layout: Option<PaneNode>,
    #[serde(default)]
    pub host_agents: Vec<HostAgentUse>,
    #[serde(default = "yes")]
    pub agents_unprompted: bool,
}

fn yes() -> bool {
    true
}

impl Project {
    /// Gives the folders of a record from before folder identities their
    /// identity as they are now. Moves the user secrets of a record from
    /// before the network rules to the rules.
    pub(super) fn migrate(&mut self) {
        if !self.ids_migrated {
            fill_id(&self.folder, &mut self.folder_id);
            for mount in &mut self.mounts {
                fill_id(&mount.host, &mut mount.id);
            }
        }
        if !self.network_migrated {
            super::migrate_secrets(&mut self.logins, &mut self.network);
        }
    }

    /// One session and one cage, both with the id of the project, so VM
    /// names, credential accounts and state folders stay valid.
    pub(super) fn split(self, taken: &[String]) -> (Session, Cage) {
        let slug = folder_slug(&self.folder, taken.iter().map(String::as_str));
        let session = Session {
            id: self.id.clone(),
            name: self.name.clone(),
            folder: self.folder,
            folder_id: self.folder_id,
            slug,
            cage: Some(self.id.clone()),
            harness: self.harness,
            terminals: self.terminals,
            pane_layout: self.pane_layout,
            tasks: self.tasks,
            layout: self.layout,
        };
        let cage = Cage {
            id: self.id,
            name: self.name,
            mounts: self.mounts,
            network: self.network,
            logins: self.logins,
            host_agents: self.host_agents,
            agents_unprompted: self.agents_unprompted,
        };
        (session, cage)
    }
}
