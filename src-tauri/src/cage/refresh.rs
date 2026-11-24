//! Brings each running VM up to date with no restart: the agent runtimes
//! that were downloaded after its start, and the native host agents (Claude
//! Code, Codex) of its cage with their profiles and host logins. A VM mounts
//! the whole runtimes folder and the folder of its profile snapshots, so
//! new wrappers, new snapshots and new logins in the proxy are enough. The
//! JS host agents (pi, Gemini CLI) are bound from their host package at the
//! start, so a change of them still waits for a restart: the VM keeps the
//! applied config of them, and shows the pending restart.

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::{AgentKind, guest_login, launch};
use crate::cage::Cage;
use crate::cage::lifecycle::{
    DOWNLOAD_HINT, EXPIRED_CLAUDE_TOKEN, NO_CLAUDE_TOKEN, apply_profile, download_notices, guest_placeholders,
    host_login, mounted, profiles_dir,
};
use crate::cage::manager::{Manager, State, Vm, blocking, lock};
use crate::error::Result;
use crate::host_agents::login;
use crate::proxy::Live;
use crate::proxy::inject::Login;
use crate::runtime::SandboxRuntime;
use crate::runtimes;
use crate::secrets::Origin;

/// The host agents that run from a downloaded runtime.
const NATIVE: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

fn is_native(kind: AgentKind) -> bool {
    NATIVE.contains(&kind)
}

fn native_login(origin: Origin) -> bool {
    matches!(origin, Origin::HostLogin { agent } if is_native(agent))
}

/// The config that a running VM can have with no restart: `current`'s
/// native host agents and their logins, and `applied`'s JS host agents and
/// their logins. With no change of the JS host agents, it is `current`'s
/// host agents and logins as they are, so the VM shows no pending restart.
pub(crate) fn live_config(applied: &Cage, current: &Cage) -> Cage {
    let js = |cage: &Cage| {
        let agents: Vec<_> = cage.host_agents.iter().filter(|a| !is_native(a.kind)).cloned().collect();
        let logins: Vec<_> = cage.logins.iter().filter(|s| !native_login(s.origin)).cloned().collect();
        (agents, logins)
    };
    if js(applied) == js(current) {
        return Cage { host_agents: current.host_agents.clone(), logins: current.logins.clone(), ..applied.clone() };
    }
    let mut host_agents: Vec<_> = applied.host_agents.iter().filter(|a| !is_native(a.kind)).cloned().collect();
    host_agents.extend(current.host_agents.iter().filter(|a| is_native(a.kind)).cloned());
    let mut logins: Vec<_> = applied.logins.iter().filter(|s| !native_login(s.origin)).cloned().collect();
    logins.extend(current.logins.iter().filter(|s| native_login(s.origin)).cloned());
    Cage { host_agents, logins, ..applied.clone() }
}

/// A running VM to refresh, as it was when the refresh began.
struct Target {
    key: String,
    generation: u64,
    applied: Cage,
    current: Cage,
    live: Arc<Live>,
}

/// What the blocking part of a refresh found.
struct Found {
    installed: Vec<runtimes::Installed>,
    logins: Vec<Arc<Login>>,
    notices: Vec<String>,
    guest_logins: Vec<AgentKind>,
}

impl<R: SandboxRuntime> Manager<R> {
    /// Refreshes each running VM of `cage_id`, or each running VM with
    /// `None`: its runtimes, its native host agents with their profiles and
    /// host logins, its wrappers. A VM that fails keeps its old setup and
    /// gets a notice; a VM that stopped or started again meanwhile is skipped.
    pub async fn refresh_agents(&self, cage_id: Option<&str>) {
        let _gate = self.refresh_gate.lock().await;
        let targets: Vec<Target> = {
            let state = lock(&self.state);
            state
                .vms
                .iter()
                .filter_map(|(key, vm)| match vm {
                    Vm::Running { generation, applied, proxy, .. } if cage_id.is_none_or(|id| applied.1.id == id) => {
                        Some(Target {
                            key: key.clone(),
                            generation: *generation,
                            applied: applied.1.clone(),
                            current: state.cage(&applied.1.id).ok()?.clone(),
                            live: proxy.live.clone(),
                        })
                    }
                    _ => None,
                })
                .collect()
        };
        for target in targets {
            let key = target.key.clone();
            if let Err(e) = self.refresh_vm(target).await {
                eprintln!("the agents of {key} could not be refreshed: {e}");
                let mut state = lock(&self.state);
                if let Some(Vm::Running { host, .. }) = state.vms.get_mut(&key) {
                    host.notices.push(format!("The agent change could not reach the running VM ({e}). Restart it."));
                }
            }
        }
    }

    async fn refresh_vm(&self, target: Target) -> Result<()> {
        let Target { key, generation, applied, current, live } = target;
        let config = live_config(&applied, &current);
        let changed: Vec<AgentKind> =
            NATIVE.into_iter().filter(|&k| applied.host_agent(k) != config.host_agent(k)).collect();
        let state_dir = self.state_dir(&key)?;
        // The stored cage has the token binding that the proxy uses now.
        let token_notice = {
            let _edit = lock(&self.secret_edit);
            self.claude_token_notice(&current)?
        };
        let (host, data, old) = (self.host.clone(), self.data.clone(), live.logins());
        let (vm_key, logins_of) = (key.clone(), config.logins.clone());
        let uses: Vec<_> = NATIVE.into_iter().map(|k| (k, config.host_agent(k).cloned())).collect();
        let found = blocking(move || {
            let mut notices = Vec::new();
            for (kind, used) in uses.iter().filter(|(k, _)| changed.contains(k)) {
                apply_profile(
                    &host,
                    &profiles_dir(&data, &vm_key),
                    &state_dir,
                    *kind,
                    used.as_ref(),
                    &mut notices,
                    &|_| {},
                )?;
            }
            let detection = host.last();
            let wanted: HashMap<&'static str, String> = uses
                .iter()
                .filter(|(_, used)| used.is_some())
                .filter_map(|(kind, _)| Some((kind.name(), detection.agent(*kind)?.version.clone())))
                .collect();
            let installed = runtimes::installed(&data, &wanted);
            notices.extend(download_notices(&wanted, &installed));
            let now = host.now_ms();
            let mut logins = Vec::new();
            for secret in &logins_of {
                let Origin::HostLogin { agent } = secret.origin else {
                    continue;
                };
                // A login that the proxy has keeps its cached token.
                match old.iter().find(|l| l.name == secret.name && l.hosts == secret.hosts) {
                    Some(login) => logins.push(login.clone()),
                    None => {
                        let token = host_login(&host, &state_dir, secret, agent, now, &mut notices)?;
                        logins.push(Arc::new(Login::new(
                            secret.name.clone(),
                            secret.hosts.clone(),
                            token,
                            host.clone(),
                        )));
                    }
                }
            }
            Ok(Found { installed, logins, notices, guest_logins: guest_login::found(&state_dir) })
        })
        .await?;

        let written = (|| -> Result<Option<Arc<R::Vm>>> {
            let mut state = lock(&self.state);
            let unprompted = state.cage(&config.id).is_ok_and(|c| c.agents_unprompted);
            let network = state.cage(&config.id).map_or_else(|_| current.network.clone(), |c| c.network.clone());
            let Some(Vm::Running { generation: now, applied, runtimes, host, vm, .. }) = state.vms.get_mut(&key) else {
                return Ok(None);
            };
            let vm = vm.clone();
            if *now != generation {
                return Ok(None);
            }
            live.set_logins(found.logins, login::denied_hosts(&config.logins));
            host.placeholders = guest_placeholders(&config.logins, &network);
            host.installed = found.installed;
            host.guest_logins = found.guest_logins;
            host.notices.retain(|n| !n.ends_with(DOWNLOAD_HINT) && n != NO_CLAUDE_TOKEN && n != EXPIRED_CLAUDE_TOKEN);
            for notice in found.notices.into_iter().chain(token_notice.map(str::to_owned)) {
                if !host.notices.contains(&notice) {
                    host.notices.push(notice);
                }
            }
            *runtimes = host.installed.iter().map(|r| r.name).collect();
            if host.gemini.is_some() {
                runtimes.push(AgentKind::Gemini.name());
            }
            applied.1 = config;
            launch::write(&self.bin_dir(&key), &mounted(runtimes), host.found(), unprompted)?;
            Ok(Some(vm))
        })()?;
        if let Some(vm) = written {
            self.flush(&vm).await;
        }
        Ok(())
    }

    /// Makes the VM see the rewritten wrappers at once. A failure only
    /// delays them by the guest's cache time.
    async fn flush(&self, vm: &R::Vm) {
        if let Err(e) = self.probe(vm, &launch::flush_argv()).await {
            eprintln!("the guest file cache could not be flushed: {e}");
        }
    }

    /// [`Manager::flush`] for each of `vms`, in a task, for a caller that cannot wait.
    pub(crate) fn flush_later(&self, vms: Vec<Arc<R::Vm>>) {
        if vms.is_empty() {
            return;
        }
        let runtime = self.runtime.clone();
        tauri::async_runtime::spawn(async move {
            for vm in vms {
                let flushed = runtime.open_pipe(&vm, &launch::flush_argv(), crate::cage::WORKSPACE, &[]).await;
                let mut pipe = match flushed {
                    Ok(pipe) => pipe,
                    Err(e) => {
                        eprintln!("the guest file cache could not be flushed: {e}");
                        continue;
                    }
                };
                let wait = async { while pipe.events.recv().await.is_some() {} };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(10), wait).await;
            }
        });
    }
}

/// Rewrites the wrappers of each running VM of `cage_id` from the stored
/// config: the permission setting and the credential placeholders. With no
/// `all`, only a VM whose placeholders changed. The caller holds the state
/// lock, so no other edit writes in between, and then flushes the returned
/// VMs ([`Manager::flush_later`]).
pub(crate) fn rewrite_wrappers<V>(
    state: &mut State<V>,
    data: &std::path::Path,
    cage_id: &str,
    all: bool,
) -> Result<Vec<Arc<V>>> {
    let Ok(cage) = state.cage(cage_id) else {
        return Ok(Vec::new());
    };
    let (unprompted, network) = (cage.agents_unprompted, cage.network.clone());
    let mut written = Vec::new();
    for (key, running) in state.vms.iter_mut() {
        if let Vm::Running { runtimes, host, applied, vm, .. } = running
            && applied.1.id == cage_id
        {
            let placeholders = guest_placeholders(&applied.1.logins, &network);
            if !all && placeholders == host.placeholders {
                continue;
            }
            host.placeholders = placeholders;
            launch::write(&crate::cage::manager::bin_dir(data, key), &mounted(runtimes), host.found(), unprompted)?;
            written.push(vm.clone());
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::live_config;
    use crate::cage::lifecycle::{EXPIRED_CLAUDE_TOKEN, NO_CLAUDE_TOKEN};
    use crate::cage::testing::*;

    fn starts(runtime: &FakeRuntime) -> usize {
        runtime.log.lock().unwrap().iter().filter(|l| l.starts_with("start")).count()
    }

    fn codex_logins() -> FakeLogins {
        let logins = FakeLogins::default();
        let doc = serde_json::json!({
            "tokens": { "id_token": "id.real", "access_token": "at-real", "refresh_token": "rt-real", "account_id": "acc" },
        });
        logins.docs.lock().unwrap().insert(AgentKind::Codex, doc.to_string());
        logins
    }

    #[tokio::test]
    async fn codex_with_its_host_login_reaches_a_running_vm_with_no_restart() {
        let (manager, runtime, id, _) = host_manager(codex_logins(), NOW);
        runtimes::fake_install(&manager.data, "codex");
        manager.start(&id).await.unwrap();
        let before = std::fs::read_to_string(manager.bin_dir(&id).join("codex")).unwrap();
        assert!(!before.contains("CODEX_HOST_LOGIN"), "{before}");

        manager.put_host_agent(&id, uses(AgentKind::Codex, false, true), true).await.unwrap();
        let vm = manager.list().cages.remove(0).shared();
        assert!(!vm.pending_restart);
        assert_eq!(vm.runtimes, ["codex"]);
        assert_eq!(starts(&runtime), 1);
        // The guest reads the renamed wrappers at once.
        assert!(*runtime.flushes.lock().unwrap() >= 1);
        let wrapper = std::fs::read_to_string(manager.bin_dir(&id).join("codex")).unwrap();
        assert!(wrapper.contains("exec /opt/zwinger/runtimes/codex/0.163.0-linux-"), "{wrapper}");
        assert!(wrapper.contains(" CODEX_HOST_LOGIN='$MSB_CODEX_HOST_LOGIN$' "), "{wrapper}");
        // The guest gets the placeholder login, and the proxy the real token.
        let auth = std::fs::read_to_string(manager.state_dir(&id).unwrap().join("codex/auth.json")).unwrap();
        assert!(auth.contains("$MSB_CODEX_HOST_LOGIN$") && !auth.contains("at-real"), "{auth}");
        let live = live(&manager, &id);
        let logins = live.logins();
        assert_eq!(logins.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["CODEX_HOST_LOGIN"]);
        assert_eq!(live.snapshot().refresh.len(), login::denied_hosts(&manager.list().cages[0].cage.logins).len());

        // Off again: the proxy drops the login, with no restart.
        manager.remove_host_agent(&id, AgentKind::Codex).unwrap();
        manager.refresh_agents(Some(&id)).await;
        assert!(live.logins().is_empty());
        assert!(!manager.list().cages[0].shared().pending_restart);
        assert!(!std::fs::read_to_string(manager.bin_dir(&id).join("codex")).unwrap().contains("CODEX_HOST_LOGIN"));
        assert_eq!(starts(&runtime), 1);
    }

    #[tokio::test]
    async fn a_claude_token_reaches_a_running_vm_with_no_restart() {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        runtimes::fake_install(&manager.data, "claude");
        manager.start(&id).await.unwrap();
        manager
            .set_claude_token(&id, Some(SecretValue::new("sk-ant-oat-real")), Source::Local, true, true, None)
            .unwrap();
        manager.refresh_agents(Some(&id)).await;
        let vm = manager.list().cages.remove(0).shared();
        assert!(!vm.pending_restart);
        let env = std::fs::read_to_string(manager.bin_dir(&id).join("env.sh")).unwrap();
        assert!(env.contains("export CLAUDE_CODE_OAUTH_TOKEN='$MSB_CLAUDE_CODE_OAUTH_TOKEN$'\n"), "{env}");
        let wrapper = std::fs::read_to_string(manager.bin_dir(&id).join("claude")).unwrap();
        assert!(wrapper.contains(" CLAUDE_CODE_OAUTH_TOKEN='$MSB_CLAUDE_CODE_OAUTH_TOKEN$' "), "{wrapper}");
        assert!(!wrapper.contains("sk-ant-oat-real"));
        // The profile is in the folder that the VM mounts at its start.
        assert!(manager.profile_dir(&id).join("claude/skills/review/SKILL.md").is_file());
        assert_eq!(starts(&runtime), 1);
    }

    #[tokio::test]
    async fn a_saved_claude_token_clears_the_token_notice_with_no_restart() {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        runtimes::fake_install(&manager.data, "claude");
        manager.put_host_agent(&id, uses(AgentKind::Claude, false, true), false).await.unwrap();
        manager.start(&id).await.unwrap();
        assert!(manager.list().cages[0].shared().host_notices.contains(&NO_CLAUDE_TOKEN.to_owned()));
        manager
            .set_claude_token(&id, Some(SecretValue::new("sk-ant-oat-real")), Source::Local, true, false, None)
            .unwrap();
        manager.refresh_agents(Some(&id)).await;
        let notices = manager.list().cages.remove(0).shared().host_notices;
        assert!(!notices.iter().any(|n| n == NO_CLAUDE_TOKEN || n == EXPIRED_CLAUDE_TOKEN), "{notices:?}");
        assert_eq!(starts(&runtime), 1);
    }

    #[tokio::test]
    async fn a_js_host_agent_change_still_needs_a_restart_and_the_native_part_applies() {
        let (manager, runtime, id, _) = host_manager(FakeLogins::default(), NOW);
        runtimes::fake_install(&manager.data, "codex");
        manager.start(&id).await.unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Pi, false, false), false).await.unwrap();
        manager.put_host_agent(&id, uses(AgentKind::Codex, false, false), false).await.unwrap();
        let vm = manager.list().cages.remove(0).shared();
        assert!(vm.pending_restart);
        assert_eq!(vm.runtimes, ["codex"]);
        // The bundled pi runs until the restart.
        assert!(std::fs::read_to_string(manager.bin_dir(&id).join("pi")).unwrap().contains("/opt/zwinger/pi/"));
        manager.stop(&id, None).await.unwrap();
        manager.start(&id).await.unwrap();
        assert!(!manager.list().cages[0].shared().pending_restart);
        assert_eq!(starts(&runtime), 2);
    }

    #[test]
    fn the_live_config_takes_the_native_agents_only() {
        let (manager, _, _) = manager();
        let applied = manager.list().cages.remove(0).cage;
        let mut current = applied.clone();
        current.put_host_agent(uses(AgentKind::Claude, true, false), None);
        assert_eq!(live_config(&applied, &current), current);
        current.put_host_agent(uses(AgentKind::Gemini, false, false), None);
        current.mounts.clear();
        let config = live_config(&applied, &current);
        assert_eq!(config.host_agents, [uses(AgentKind::Claude, true, false)]);
        assert_eq!(config.mounts, applied.mounts);
    }
}
