//! Finds the agent logins that the user made in the guest, such as with
//! `/login`. Such a login keeps a real token, with its refresh token, in the
//! guest-writable state dir: the agent can read and send it. zwinger's own
//! guest logins hold placeholders only and are not found. It checks only
//! the shape of the fields and never returns or logs their values.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};
use serde_json::Value;

use super::AgentKind;
use crate::host_agents::{self, KINDS, login};

/// A login file over this size is not read.
const FILE_LIMIT: u64 = 1 << 20;

/// The folders below the state dir and the name of the agent's login file.
fn place(kind: AgentKind) -> (&'static [&'static str], &'static str) {
    match kind {
        AgentKind::Claude => (&["claude", ".claude"], ".credentials.json"),
        AgentKind::Codex => (&["codex"], "auth.json"),
        AgentKind::Pi => (&["agent"], "auth.json"),
        AgentKind::Gemini => (&["gemini", ".gemini"], "oauth_creds.json"),
    }
}

/// The agents that have a login of their own in the state dir `state`.
pub fn found(state: &Path) -> Vec<AgentKind> {
    KINDS.into_iter().filter(|&kind| read(state, kind).is_some_and(|doc| is_login(kind, &doc))).collect()
}

/// Removes the agent's login file if it still holds a real token, so a
/// placeholder login that zwinger wrote since is kept. No file is not an error.
pub fn remove(state: &Path, kind: AgentKind) -> std::io::Result<()> {
    if !read(state, kind).is_some_and(|doc| is_login(kind, &doc)) {
        return Ok(());
    }
    let (dirs, name) = place(kind);
    match open_dirs(state, dirs) {
        Ok(dir) => match rustix::fs::unlinkat(&dir, name, rustix::fs::AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(e) => Err(e.into()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// A token field that holds a real value, not a placeholder.
fn is_token(value: &Value) -> bool {
    value.as_str().is_some_and(|token| !token.is_empty() && !token.contains("$MSB_"))
}

fn is_login(kind: AgentKind, doc: &Value) -> bool {
    match kind {
        AgentKind::Claude => is_token(&doc["claudeAiOauth"]["accessToken"]),
        AgentKind::Codex => is_token(&doc["tokens"]["access_token"]) || is_token(&doc["OPENAI_API_KEY"]),
        AgentKind::Pi => doc.as_object().is_some_and(|providers| providers.values().any(|p| is_token(&p["access"]))),
        AgentKind::Gemini => {
            is_token(&doc["access_token"])
                || (is_token(&doc["refresh_token"]) && doc["refresh_token"] != login::INERT_REFRESH)
        }
    }
}

/// The guest can write the state dir, so no link is followed, and a file
/// that is not regular (such as a FIFO) is not read.
fn read(state: &Path, kind: AgentKind) -> Option<Value> {
    let (dirs, name) = place(kind);
    let dir = open_dirs(state, dirs).ok()?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(&dir, name, flags, Mode::empty()).ok()?;
    if FileType::from_raw_mode(rustix::fs::fstat(&fd).ok()?.st_mode) != FileType::RegularFile {
        return None;
    }
    let mut text = String::new();
    std::fs::File::from(fd).take(FILE_LIMIT).read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

/// Opens `state/<dirs>` and makes no folder.
fn open_dirs(state: &Path, dirs: &[&str]) -> std::io::Result<OwnedFd> {
    let mut dir = rustix::fs::open(state, host_agents::dir_flags(), Mode::empty())?;
    for part in dirs {
        dir = rustix::fs::openat(&dir, *part, host_agents::dir_flags(), Mode::empty())?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn state() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-guest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(state: &Path, kind: AgentKind, doc: &Value) {
        let (dirs, name) = place(kind);
        let dir = dirs.iter().fold(state.to_owned(), |dir, part| dir.join(part));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), doc.to_string()).unwrap();
    }

    #[test]
    fn each_agent_login_is_found() {
        let state = state();
        assert!(found(&state).is_empty());
        put(
            &state,
            AgentKind::Claude,
            &json!({ "claudeAiOauth": { "accessToken": "sk-ant-oat01-x", "refreshToken": "r" } }),
        );
        put(&state, AgentKind::Codex, &json!({ "OPENAI_API_KEY": null, "tokens": { "access_token": "eyJ" } }));
        put(&state, AgentKind::Pi, &json!({ "anthropic": { "access": "sk-ant-oat01-y" } }));
        put(&state, AgentKind::Gemini, &json!({ "refresh_token": "1//r" }));
        assert_eq!(found(&state), KINDS);
    }

    #[test]
    fn placeholder_logins_of_zwinger_are_not_found() {
        let state = state();
        let placeholder = "$MSB_CODEX_HOST_LOGIN$";
        put(
            &state,
            AgentKind::Codex,
            &json!({ "OPENAI_API_KEY": null, "tokens": { "access_token": placeholder, "refresh_token": "zwinger-host-login-no-refresh" } }),
        );
        put(
            &state,
            AgentKind::Pi,
            &json!({ "github-copilot": { "access": "tid=1;proxy-ep=proxy.business.githubcopilot.com;$MSB_PI_HOST_LOGIN$" } }),
        );
        put(&state, AgentKind::Claude, &json!({ "other": "shape" }));
        assert!(found(&state).is_empty());
        // An API key login of Codex is a real value; a placeholder key is not.
        put(&state, AgentKind::Codex, &json!({ "OPENAI_API_KEY": "$MSB_OPENAI_API_KEY$" }));
        assert!(found(&state).is_empty());
        put(&state, AgentKind::Codex, &json!({ "OPENAI_API_KEY": "sk-proj-x" }));
        assert_eq!(found(&state), [AgentKind::Codex]);
    }

    #[test]
    fn links_and_odd_files_are_not_followed() {
        let (state, outside) = (state(), state());
        put(&outside, AgentKind::Claude, &json!({ "claudeAiOauth": { "accessToken": "host-token" } }));
        std::os::unix::fs::symlink(outside.join("claude"), state.join("claude")).unwrap();
        std::fs::create_dir_all(state.join("codex")).unwrap();
        std::os::unix::fs::symlink(outside.join("claude/.claude/.credentials.json"), state.join("codex/auth.json"))
            .unwrap();
        std::fs::create_dir_all(state.join("agent")).unwrap();
        let fifo = std::process::Command::new("mkfifo").arg(state.join("agent/auth.json")).status().unwrap();
        assert!(fifo.success());
        assert!(found(&state).is_empty());

        // A link is no login, so a remove never takes the file that it points to.
        remove(&state, AgentKind::Codex).unwrap();
        remove(&state, AgentKind::Claude).unwrap();
        assert!(outside.join("claude/.claude/.credentials.json").exists());
    }

    #[test]
    fn remove_deletes_only_the_login_file() {
        let state = state();
        put(&state, AgentKind::Gemini, &json!({ "access_token": "ya29.x" }));
        std::fs::write(state.join("gemini/.gemini/settings.json"), "{}").unwrap();
        remove(&state, AgentKind::Gemini).unwrap();
        assert!(found(&state).is_empty());
        assert!(state.join("gemini/.gemini/settings.json").exists());
        remove(&state, AgentKind::Gemini).unwrap();
        remove(&state, AgentKind::Pi).unwrap();
    }

    #[test]
    fn remove_keeps_a_placeholder_login_that_replaced_the_guest_login() {
        let state = state();
        put(&state, AgentKind::Codex, &json!({ "tokens": { "access_token": "$MSB_CODEX_HOST_LOGIN$" } }));
        remove(&state, AgentKind::Codex).unwrap();
        assert!(state.join("codex/auth.json").exists());
    }
}
