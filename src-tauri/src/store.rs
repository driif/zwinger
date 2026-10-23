use std::path::{Path, PathBuf};

use crate::cage::Project;
use crate::error::Result;
use crate::network::{Action, Credential, DefaultAction, Inject, Rule, Target};

/// The store file in the app data dir.
pub const FILE: &str = "projects.json";
/// The store file from before the rename of shields to projects.
const LEGACY_FILE: &str = "shields.json";

/// Copies the legacy store file to [`FILE`] once, when only the legacy file
/// is there. The legacy file stays, so an older app version still finds it.
pub fn migrate_legacy(data: &Path) -> Result<()> {
    let (legacy, path) = (data.join(LEGACY_FILE), data.join(FILE));
    if path.exists() || !legacy.exists() {
        return Ok(());
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::copy(&legacy, &tmp)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

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
    /// identity as they are now, once. Moves the user secrets of records
    /// from before the network rules to the rules, once.
    pub fn load(&self) -> Result<Vec<Project>> {
        let mut projects: Vec<Project> = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut filled = false;
        for project in projects.iter_mut().filter(|s| !s.ids_migrated) {
            project.fill_ids();
            project.ids_migrated = true;
            filled = true;
        }
        for project in projects.iter_mut().filter(|s| !s.network_migrated) {
            migrate_secrets(project);
            filled = true;
        }
        if filled && let Err(e) = self.save(&projects) {
            eprintln!("the migrated projects could not be saved: {e}");
        }
        Ok(projects)
    }

    pub fn save(&self, projects: &[Project]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(projects)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Each user secret becomes a credential with the same name, so it keeps its
/// Keychain account, on a rule for each of its exact hosts, with its header,
/// query or body places. The rule allows the host only where the old policy
/// reached it: Public mode, or a host on the allow list of List mode. Else
/// (Deny mode, or a host off the list) the rule blocks the host and keeps
/// the credential inactive. The host logins stay.
fn migrate_secrets(project: &mut Project) {
    let (users, logins) = std::mem::take(&mut project.logins).into_iter().partition(|s| s.origin.is_user());
    project.logins = logins;
    let users: Vec<crate::secrets::Secret> = users;
    // The policy came from an old shape: only Deny mode is offline, only Public mode allows by default.
    let reached = |rules: &[Rule], host: &str, offline: bool, default: DefaultAction| {
        !offline
            && (default == DefaultAction::Allow || rules.iter().any(|r| r.action == Action::Allow && r.matches(host)))
    };
    for secret in users {
        for host in &secret.hosts {
            let network = &mut project.network;
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
                });
            }
        }
    }
    project.network_migrated = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentKind;
    use crate::cage::{App, FileId, HostAgentUse, Mount, PaneNode, Pos, SplitDirection, Task, TaskAgent, Terminal};
    use crate::network::{Action, Credential, DefaultAction, Inject, NetworkPolicy, Rule, Target};
    use crate::runtime::SessionKind;
    use crate::secrets::{Origin, Places, Secret};

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        let store = Store::new(dir.join("nested").join("projects.json"));
        assert!(store.load().unwrap().is_empty());

        let projects = vec![Project {
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
            network: NetworkPolicy {
                default: DefaultAction::Block,
                offline: false,
                rules: vec![Rule {
                    id: "rule-1".into(),
                    target: Target::Service { id: "openai".into() },
                    action: Action::Allow,
                    credentials: vec![Credential {
                        name: "OPENAI_API_KEY".into(),
                        inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
                    }],
                }],
            },
            network_migrated: true,
            harness: Some("/workspace/agent".into()),
            logins: vec![Secret {
                name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
                hosts: vec!["api.anthropic.com".into()],
                places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
                origin: Origin::HostLogin { agent: AgentKind::Claude },
            }],
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
                Terminal { id: "term-3".into(), kind: SessionKind::Host },
            ],
            pane_layout: Some(PaneNode::Split {
                direction: SplitDirection::Row,
                ratio: 0.5,
                a: Box::new(PaneNode::Leaf { terminal: "term-3".into() }),
                b: Box::new(PaneNode::Leaf { terminal: "term-1".into() }),
            }),
            host_agents: vec![HostAgentUse { kind: AgentKind::Claude, profile: true, login: true }],
            agents_unprompted: false,
        }];
        store.save(&projects).unwrap();
        assert_eq!(store.load().unwrap(), projects);
        assert!(!dir.join("nested").join("projects.json.tmp").exists());
    }

    fn data_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migrate_copies_the_legacy_file_and_keeps_it() {
        let dir = data_dir();
        let old = r#"[{"id":"a","name":"proj","folder":"/tmp/proj","mounts":[],
            "network":{"allowAll":false,"domains":[]}}]"#;
        std::fs::write(dir.join(LEGACY_FILE), old).unwrap();

        migrate_legacy(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(LEGACY_FILE)).unwrap(), old);
        let projects = Store::new(dir.join(FILE)).load().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "proj");
    }

    #[test]
    fn migrate_does_not_overwrite_projects() {
        let dir = data_dir();
        std::fs::write(dir.join(LEGACY_FILE), "old").unwrap();
        std::fs::write(dir.join(FILE), "new").unwrap();

        migrate_legacy(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(dir.join(LEGACY_FILE)).unwrap(), "old");
    }

    #[test]
    fn migrate_without_legacy_file_does_nothing() {
        let dir = data_dir();
        migrate_legacy(&dir).unwrap();
        assert!(!dir.join(FILE).exists());
    }

    #[test]
    fn loads_config_without_newer_fields() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.json");
        std::fs::write(
            &path,
            r#"[{"id":"a","name":"proj","folder":"/tmp/proj","mounts":[],
                "network":{"allowAll":false,"domains":[]}}]"#,
        )
        .unwrap();
        let project = &Store::new(path).load().unwrap()[0];
        assert_eq!(project.harness, None);
        assert!(project.logins.is_empty());
        assert!(project.layout.is_empty());
        assert!(project.apps.is_empty());
        assert!(project.tasks.is_empty());
        assert_eq!(project.pane_layout, None);
        assert!(project.terminals.is_empty());
        assert!(project.host_agents.is_empty());
        assert_eq!(project.network.default, DefaultAction::Block);
        assert!(project.agents_unprompted);
        assert_eq!(project.folder_id, None);
    }

    #[test]
    fn load_fills_the_folder_ids_of_old_records_once() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let folder = dir.join("proj").canonicalize().unwrap();
        let path = dir.join("projects.json");
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
        let project = &store.load().unwrap()[0];
        assert_eq!(project.folder_id, Some(id));
        assert!(project.verify_paths().is_err());
    }

    /// A project of M2 with an allow list, two secrets for one host, a
    /// secret for two hosts with query and body places, and a host login.
    #[test]
    fn load_moves_the_old_secrets_to_rule_credentials_once() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.json");
        let header = |field: &str| serde_json::json!({ "headerFields": [field], "query": false, "body": false });
        let old = serde_json::json!([{ "id": "a", "name": "proj", "folder": "/tmp/proj", "mounts": [], "idsMigrated": true,
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
            ] }]);
        std::fs::write(&path, old.to_string()).unwrap();
        let store = Store::new(path.clone());
        let project = store.load().unwrap().remove(0);
        assert!(project.network_migrated);
        assert_eq!(project.logins.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["CLAUDE_CODE_OAUTH_TOKEN"]);
        let rule = |host: &str| {
            project.network.rules.iter().find(|r| r.target == Target::Host { host: host.into() }).unwrap().clone()
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
        // One secret on two hosts: one credential name, so one Keychain account, on each host.
        // The host off the allow list stays blocked, with the credential inactive.
        let maps = Places { header_fields: vec![], query: true, body: true };
        for (host, action) in [("maps.example.com", Action::Allow), ("tiles.example.com", Action::Block)] {
            let rule = rule(host);
            assert_eq!(rule.action, action, "{host}");
            assert_eq!(
                rule.credentials,
                [Credential { name: "MAPS_KEY".into(), inject: Inject::Replace { places: maps.clone() } }]
            );
        }
        project.check().unwrap_or_else(|e| panic!("{e}"));
        // The saved file has the new shape, and a second load changes nothing.
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(saved[0]["secrets"].is_null() && saved[0]["networkMigrated"] == true);
        assert_eq!(store.load().unwrap()[0], project);
    }

    /// Loads one old project with `network` and one secret for `a.example.com`
    /// and `b.example.com`. Returns the action of each host's rule.
    fn migrate(network: serde_json::Value) -> (Project, [Option<Action>; 2]) {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.json");
        let old = serde_json::json!([{ "id": "a", "name": "proj", "folder": "/tmp/proj", "mounts": [], "idsMigrated": true,
            "network": network,
            "secrets": [{ "name": "KEY", "hosts": ["a.example.com", "b.example.com"],
                "places": { "headerFields": ["authorization"], "query": false, "body": false } }] }]);
        std::fs::write(&path, old.to_string()).unwrap();
        let project = Store::new(path).load().unwrap().remove(0);
        project.check().unwrap_or_else(|e| panic!("{e}"));
        let action = |host: &str| {
            let rule = project.network.rules.iter().find(|r| r.target == Target::Host { host: host.into() })?;
            assert_eq!(rule.credentials.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["KEY"]);
            Some(rule.action)
        };
        let actions = [action("a.example.com"), action("b.example.com")];
        (project, actions)
    }

    #[test]
    fn a_secret_of_list_mode_is_allowed_only_on_the_listed_hosts() {
        let (project, actions) = migrate(serde_json::json!({ "mode": "list", "domains": ["*.a.example.com"] }));
        assert_eq!(actions, [Some(Action::Allow), Some(Action::Block)]);
        assert_eq!(project.network.default, DefaultAction::Block);
        // The proxy sends the inactive credential nowhere.
        let blocked = project.network.rules.iter().find(|r| r.action == Action::Block).unwrap();
        assert!(!blocked.sends_credentials_to("b.example.com"));
    }

    #[test]
    fn a_secret_of_public_mode_stays_allowed_with_the_stored_domains() {
        let (project, actions) = migrate(serde_json::json!({ "mode": "public", "domains": ["c.example.com"] }));
        assert_eq!(actions, [Some(Action::Allow), Some(Action::Allow)]);
        assert_eq!(project.network.default, DefaultAction::Allow);
        let c = Target::Host { host: "c.example.com".into() };
        assert!(project.network.rules.iter().any(|r| r.target == c && r.action == Action::Allow));
    }

    #[test]
    fn a_secret_of_deny_mode_stays_inactive_on_block_rules() {
        let (project, actions) = migrate(serde_json::json!({ "mode": "deny", "domains": ["a.example.com"] }));
        assert_eq!(actions, [Some(Action::Block), Some(Action::Block)]);
        assert!(project.network.offline);
    }
}
