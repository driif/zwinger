//! The agent state of a task run, apart from the agent terminals: each
//! participant of a run has its own state root with the layout of the VM
//! state dir. So a terminal `claude -c` or `codex resume --last` never
//! continues a task call, and task calls do not fill the terminal history.
//!
//! The config entries of the profile and the login file of each agent are
//! links to the same entries in the terminal home, so a task call has the
//! same settings, profile and login as a terminal. The state of a task stays
//! until its next run starts or the task is removed.
//!
//! The guest can write the state dir, so each change follows no link.

use std::io;
use std::path::Path;

use crate::agent::{AgentKind, STATE_DIR, guest_login};
use crate::host_agents::{self, profile};
use crate::task_participants::Participant;

/// The folder of the task state, below the VM state dir.
const DIR: &str = "tasks";

/// The guest folder of a run's state. Each participant has its own state
/// root in it, named by its id.
pub fn guest_root(session: &str, slug: &str, run: &str) -> String {
    format!("{STATE_DIR}/{DIR}/{session}/{slug}/{run}")
}

/// Removes the state of the task's earlier runs, then makes the state root
/// of each participant of `run` below the VM state dir `state`.
pub fn create(state: &Path, session: &str, slug: &str, run: &str, homes: &[Participant]) -> io::Result<()> {
    let session_dir = host_agents::open_parts(state, &[DIR, session])?;
    host_agents::remove_at(&session_dir, slug)?;
    for home in homes {
        link_config(state, &[DIR, session, slug, run, &home.id], home.kind)?;
    }
    Ok(())
}

/// Removes the state of all runs of the task. No state is not an error.
pub fn remove(state: &Path, session: &str, slug: &str) -> io::Result<()> {
    host_agents::remove_at(&host_agents::open_parts(state, &[DIR, session])?, slug)
}

/// Makes the home `state/<home>` and links each config entry and the login
/// file of `kind` that the terminal home has into the same place below it.
fn link_config(state: &Path, home: &[&str], kind: AgentKind) -> io::Result<()> {
    let parts = profile::state_home(kind);
    let terminal = host_agents::open_parts(state, parts)?;
    let dir = host_agents::open_parts(state, &[home, parts].concat())?;
    let login = guest_login::place(kind).map(|(_, name)| name);
    for name in profile::entries(kind).iter().chain(&login) {
        if host_agents::type_at(&terminal, *name)?.is_some() {
            let target = format!("{STATE_DIR}/{}/{name}", parts.join("/"));
            rustix::fs::symlinkat(target.as_str(), &dir, *name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn home(id: &str, kind: AgentKind) -> Participant {
        Participant { id: id.to_owned(), kind, model: None }
    }

    fn state() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-task-state-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn each_participant_gets_a_home_with_links_to_the_terminal_config() {
        let state = state();
        fs::create_dir_all(state.join("claude/.claude/projects")).unwrap();
        fs::write(state.join("claude/.claude/history.jsonl"), "terminal").unwrap();
        fs::write(state.join("claude/.claude/.credentials.json"), "{}").unwrap();
        std::os::unix::fs::symlink(
            "/opt/zwinger/profile/claude/settings.json",
            state.join("claude/.claude/settings.json"),
        )
        .unwrap();
        fs::create_dir_all(state.join("codex")).unwrap();
        fs::write(state.join("codex/config.toml"), "").unwrap();
        fs::write(state.join("codex/auth.json"), "{}").unwrap();

        create(&state, "s1", "fix", "01", &[home("claude", AgentKind::Claude), home("codex", AgentKind::Codex)])
            .unwrap();

        let claude = state.join("tasks/s1/fix/01/claude/claude/.claude");
        let mut names: Vec<_> =
            fs::read_dir(&claude).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, [".credentials.json", "settings.json"]);
        assert_eq!(
            fs::read_link(claude.join("settings.json")).unwrap(),
            Path::new("/opt/zwinger/state/claude/.claude/settings.json")
        );
        assert_eq!(
            fs::read_link(claude.join(".credentials.json")).unwrap(),
            Path::new("/opt/zwinger/state/claude/.claude/.credentials.json")
        );
        let codex = state.join("tasks/s1/fix/01/codex/codex");
        assert_eq!(fs::read_link(codex.join("auth.json")).unwrap(), Path::new("/opt/zwinger/state/codex/auth.json"));
        assert_eq!(
            fs::read_link(codex.join("config.toml")).unwrap(),
            Path::new("/opt/zwinger/state/codex/config.toml")
        );
        assert_eq!(guest_root("s1", "fix", "01"), "/opt/zwinger/state/tasks/s1/fix/01");
    }

    #[test]
    fn a_new_run_removes_the_state_of_the_earlier_runs_of_its_task_only() {
        let state = state();
        create(&state, "s1", "fix", "01", &[home("pi-1", AgentKind::Pi)]).unwrap();
        create(&state, "s1", "docs", "01", &[home("pi-1", AgentKind::Pi)]).unwrap();
        fs::write(state.join("tasks/s1/fix/01/pi-1/agent/run.jsonl"), "old").unwrap();

        create(&state, "s1", "fix", "02", &[home("pi-1", AgentKind::Pi), home("pi-2", AgentKind::Pi)]).unwrap();
        assert!(!state.join("tasks/s1/fix/01").exists());
        assert!(state.join("tasks/s1/fix/02/pi-2/agent").is_dir());
        assert!(state.join("tasks/s1/docs/01").is_dir());

        remove(&state, "s1", "fix").unwrap();
        assert!(!state.join("tasks/s1/fix").exists());
        assert!(state.join("tasks/s1/docs").is_dir());
        remove(&state, "s2", "fix").unwrap();
    }

    #[test]
    fn a_planted_link_is_not_followed() {
        let state = state();
        let outside = self::state();
        fs::create_dir_all(outside.join("fix")).unwrap();
        fs::write(outside.join("fix/keep"), "host").unwrap();
        fs::create_dir_all(state.join("tasks")).unwrap();
        std::os::unix::fs::symlink(&outside, state.join("tasks/s1")).unwrap();

        assert!(create(&state, "s1", "fix", "01", &[home("claude", AgentKind::Claude)]).is_err());
        assert!(remove(&state, "s1", "fix").is_err());
        assert!(outside.join("fix/keep").is_file());
    }
}
