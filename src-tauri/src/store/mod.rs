mod legacy;

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::agent::AgentKind;
use crate::cage::Cage;
use crate::error::{Error, Result};
use crate::network::{Action, Credential, DefaultAction, Inject, NetworkPolicy, Rule, Target};
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

/// Copies the shields file to the projects file once, when only the
/// shields file is there. The shields file stays.
fn migrate_legacy(data: &Path) -> Result<()> {
    let (legacy, path) = (data.join(SHIELDS), data.join(PROJECTS));
    if path.exists() || !legacy.exists() {
        return Ok(());
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::copy(&legacy, &tmp)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

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
    /// A session whose cage is not in the cages file is an error.
    pub fn load(&self) -> Result<(Vec<Session>, Vec<Cage>)> {
        let (sessions, cages) = (self.dir.join(SESSIONS), self.dir.join(CAGES));
        if sessions.exists() && cages.exists() {
            let (sessions, cages): (Vec<Session>, Vec<Cage>) = (read(&sessions)?, read(&cages)?);
            if let Some(s) = sessions.iter().find(|s| !cages.iter().any(|c| c.id == s.cage)) {
                return Err(Error::MissingCage { session: format!("{} ({})", s.name, s.id), cage: s.cage.clone() });
            }
            let mut cages = cages;
            if drop_claude_logins(&mut cages)
                && let Err(e) = self.save_cages(&cages)
            {
                eprintln!("the migrated cages could not be saved: {e}");
            }
            return Ok((sessions, cages));
        }
        let (sessions, mut cages) = self.convert()?;
        drop_claude_logins(&mut cages);
        self.save_cages(&cages)?;
        self.save_sessions(&sessions)?;
        Ok((sessions, cages))
    }

    /// Each project becomes one session and one cage. The migrations of
    /// older records run in memory only.
    fn convert(&self) -> Result<(Vec<Session>, Vec<Cage>)> {
        migrate_legacy(&self.dir)?;
        let projects: Vec<legacy::Project> = match std::fs::read(self.dir.join(PROJECTS)) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
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

fn read<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Writes a temp file in the same dir and renames it over the file.
fn write<T: Serialize>(path: &Path, items: &[T]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(items)?)?;
    std::fs::rename(&tmp, path)?;
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
    use crate::session::{PaneNode, Pos, SplitDirection, Task, TaskAgent, TerminalRecord};

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
        let mut session = Session::new(data_dir().to_str().unwrap(), "c", []).unwrap();
        session.layout = [("mount:/mnt/data".to_owned(), Pos { x: 12.5, y: -40.0 })].into();
        let cage = Cage::new("c".into(), "proj".into());
        store.save_cages(std::slice::from_ref(&cage)).unwrap();
        store.save_sessions(std::slice::from_ref(&session)).unwrap();
        assert_eq!(store.load().unwrap(), (vec![session], vec![cage]));
        assert!(!dir.join("sessions.json.tmp").exists() && !dir.join("cages.json.tmp").exists());
        assert!(!dir.join(PROJECTS).exists());
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
            cage: "p-1".into(),
            harness: Some("/workspace/agent".into()),
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
        let session = Session::new(data_dir().to_str().unwrap(), "c", []).unwrap();
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
        let (sessions, cages) = Store::new(dir.clone()).load().unwrap();
        assert_eq!((sessions[0].name.as_str(), cages[0].id.as_str()), ("proj", "a"));
        assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), old);
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), old);
    }

    #[test]
    fn the_shields_file_does_not_overwrite_the_projects_file() {
        let dir = data_dir();
        std::fs::write(dir.join(SHIELDS), "old").unwrap();
        std::fs::write(dir.join(PROJECTS), "[]").unwrap();
        assert_eq!(Store::new(dir.clone()).load().unwrap(), (Vec::new(), Vec::new()));
        assert_eq!(std::fs::read_to_string(dir.join(PROJECTS)).unwrap(), "[]");
        assert_eq!(std::fs::read_to_string(dir.join(SHIELDS)).unwrap(), "old");
    }

    #[test]
    fn an_old_record_gets_the_defaults() {
        let (sessions, cages) = convert(serde_json::json!([{ "id": "a", "name": "proj", "folder": "/tmp/proj",
            "mounts": [], "network": { "allowAll": false, "domains": [] } }]));
        let (session, cage) = (&sessions[0], &cages[0]);
        assert_eq!(session.harness, None);
        assert!(session.layout.is_empty() && session.tasks.is_empty() && session.terminals.is_empty());
        assert_eq!(session.pane_layout, None);
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
                [Credential { name: "MAPS_KEY".into(), inject: Inject::Replace { places: maps.clone() } }]
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
