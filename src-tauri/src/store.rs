use std::path::PathBuf;

use crate::error::Result;
use crate::shield::Shield;

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Gives the folders of records from before folder identities their
    /// identity as they are now, once.
    pub fn load(&self) -> Result<Vec<Shield>> {
        let mut shields: Vec<Shield> = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut filled = false;
        for shield in shields.iter_mut().filter(|s| !s.ids_migrated) {
            shield.fill_ids();
            shield.ids_migrated = true;
            filled = true;
        }
        if filled && let Err(e) = self.save(&shields) {
            eprintln!("the folder identities could not be saved: {e}");
        }
        Ok(shields)
    }

    pub fn save(&self, shields: &[Shield]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(shields)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentKind;
    use crate::runtime::SessionKind;
    use crate::secrets::{Origin, Places, Secret};
    use crate::shield::{App, FileId, HostAgentUse, Mount, NetworkMode, NetworkPolicy, Pos, Task, TaskAgent, Terminal};

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        let store = Store::new(dir.join("nested").join("shields.json"));
        assert!(store.load().unwrap().is_empty());

        let shields = vec![Shield {
            id: "a".into(),
            name: "proj".into(),
            folder: "/tmp/proj".into(),
            folder_id: Some(FileId { dev: 1, ino: 2 }),
            ids_migrated: true,
            mounts: vec![Mount {
                host: "/tmp/data".into(),
                id: Some(FileId { dev: 1, ino: 3 }),
                guest: "/mnt/data".into(),
                readonly: true,
            }],
            network: NetworkPolicy { mode: NetworkMode::Public, domains: vec!["registry.npmjs.org".into()] },
            harness: Some("/workspace/agent".into()),
            secrets: vec![
                Secret {
                    name: "OPENAI_API_KEY".into(),
                    hosts: vec!["api.openai.com".into()],
                    places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
                    origin: Origin::User,
                },
                Secret {
                    name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
                    hosts: vec!["api.anthropic.com".into()],
                    places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
                    origin: Origin::HostLogin { agent: AgentKind::Claude },
                },
            ],
            layout: [("mount:/mnt/data".to_owned(), Pos { x: 12.5, y: -40.0 })].into(),
            apps: vec![App {
                id: "app-1".into(),
                name: "web".into(),
                command: "pnpm dev".into(),
                cwd: "/workspace".into(),
                autostart: true,
            }],
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
            terminals: vec![
                Terminal { id: "term-1".into(), kind: SessionKind::Agent { agent: AgentKind::Codex, resume: false } },
                Terminal { id: "term-2".into(), kind: SessionKind::Shell },
            ],
            host_agents: vec![HostAgentUse { kind: AgentKind::Claude, profile: true, login: true }],
            auto_renew: true,
            agents_unprompted: false,
        }];
        store.save(&shields).unwrap();
        assert_eq!(store.load().unwrap(), shields);
        assert!(!dir.join("nested").join("shields.json.tmp").exists());
    }

    #[test]
    fn loads_config_without_newer_fields() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shields.json");
        std::fs::write(
            &path,
            r#"[{"id":"a","name":"proj","folder":"/tmp/proj","mounts":[],
                "network":{"allowAll":false,"domains":[]}}]"#,
        )
        .unwrap();
        let shield = &Store::new(path).load().unwrap()[0];
        assert_eq!(shield.harness, None);
        assert!(shield.secrets.is_empty());
        assert!(shield.layout.is_empty());
        assert!(shield.apps.is_empty());
        assert!(shield.tasks.is_empty());
        assert!(shield.terminals.is_empty());
        assert!(shield.host_agents.is_empty() && !shield.auto_renew);
        assert!(shield.agents_unprompted);
        assert_eq!(shield.folder_id, None);
    }

    #[test]
    fn load_fills_the_folder_ids_of_old_records_once() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let folder = dir.join("proj").canonicalize().unwrap();
        let path = dir.join("shields.json");
        let old = serde_json::json!([{ "id": "a", "name": "proj", "folder": folder, "mounts": [],
            "network": { "mode": "deny", "domains": [] } }]);
        std::fs::write(&path, old.to_string()).unwrap();
        let store = Store::new(path.clone());
        let id = store.load().unwrap()[0].folder_id.unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved[0]["folderId"], serde_json::json!({ "dev": id.dev, "ino": id.ino }));

        // Later loads keep the stored id, also after a swap.
        std::fs::rename(&folder, dir.join("was")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        let shield = &store.load().unwrap()[0];
        assert_eq!(shield.folder_id, Some(id));
        assert!(shield.verify_paths().is_err());
    }
}
