mod agent;
mod appearance;
mod bootstrap;
mod cage;
mod commands;
mod editor;
mod error;
mod host_agents;
mod netlog;
mod network;
mod preferences;
mod proxy;
mod runtime;
mod runtimes;
mod secrets;
mod store;
mod task_agents;
mod task_artifacts;
mod task_runs;
mod terminal;
mod updates;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter as _, Manager as _, RunEvent, Wry};

use crate::bootstrap::{Bootstrap, Check, Level, Phase, Step};
use crate::cage::manager::Manager;
use crate::host_agents::HostAgents;
use crate::runtime::{Microsandbox, SandboxRuntime};
use crate::secrets::Vault;

pub(crate) const REAP_LIMIT: Duration = Duration::from_secs(10);
/// The open files that the app asks for. Each cage's proxy holds guest and
/// upstream connections; the default soft limit of macOS is 256.
const OPEN_FILES: libc::rlim_t = 8192;
pub(crate) const STOP_ALL_LIMIT: Duration = Duration::from_secs(20);

/// Has no Close Window item, so ⌘W reaches the webview and closes a session.
fn menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    Menu::with_items(
        app,
        &[
            &Submenu::with_items(
                app,
                "zwinger",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Window",
                true,
                &[&PredefinedMenuItem::minimize(app, None)?, &PredefinedMenuItem::fullscreen(app, None)?],
            )?,
        ],
    )
}

/// A panic shows the recover screen. A panic in the emit itself does not loop.
fn report_panics(app: AppHandle) {
    let default = std::panic::take_hook();
    let reported = AtomicBool::new(false);
    std::panic::set_hook(Box::new(move |info| {
        if !reported.swap(true, Ordering::SeqCst) {
            let _ = app.emit("backend-lost", ());
        }
        default(info);
    }));
}

/// Removes the VMs of an earlier run, loads the projects and makes the
/// manager. The manager starts with no VMs, so every cage shows as
/// stopped; a notice names the removed VMs.
fn boot<R: SandboxRuntime>(
    runtime: R,
    vault: Arc<dyn Vault>,
    data: &Path,
    host: Arc<HostAgents>,
) -> error::Result<Manager<R>> {
    store::migrate_legacy(data)?;
    // The timer must be made inside the runtime, so it is in the future.
    let reaped = tauri::async_runtime::block_on(async { tokio::time::timeout(REAP_LIMIT, runtime.reap()).await });
    let reaped = match reaped {
        Ok(Ok(reaped)) => reaped,
        Ok(Err(e)) => {
            eprintln!("reap failed: {e}");
            Vec::new()
        }
        Err(_) => {
            eprintln!("reap timed out");
            Vec::new()
        }
    };
    let store = store::Store::new(data.join(store::FILE));
    let manager = Manager::new(runtime, store, vault, data.to_owned(), host)?;
    if !reaped.is_empty() {
        let projects = manager.list();
        let names: Vec<String> = reaped
            .iter()
            .map(|vm| {
                let project = runtime::project_id(vm).and_then(|id| projects.iter().find(|v| v.project.id == id));
                project.map_or_else(|| vm.clone(), |v| v.project.name.clone())
            })
            .collect();
        manager
            .add_app_notice(format!("zwinger stopped the VMs that the last run left running: {}.", names.join(", ")));
    }
    Ok(manager)
}

/// Raises the soft limit of open files to [`OPEN_FILES`], or to the hard
/// limit if it is lower. Returns the soft limit after the change.
fn raise_open_files() -> std::io::Result<libc::rlim_t> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes the struct that it gets.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let wanted = OPEN_FILES.min(limit.rlim_max);
    if limit.rlim_cur < wanted {
        limit.rlim_cur = wanted;
        // SAFETY: setrlimit reads the struct that it gets.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(limit.rlim_cur)
}

/// Sends the manager's events to the UI.
fn connect(app: &AppHandle, manager: &Manager<Microsandbox>) {
    // One app-wide event, so the rail and the map update while the
    // Network tab is closed.
    let handle = app.clone();
    manager.set_net_sink(Box::new(move |project_id, event| {
        let _ = handle.emit("net-event", serde_json::json!({ "projectId": project_id, "event": event }));
    }));
    let handle = app.clone();
    manager.set_guest_sink(Box::new(move |project_id| {
        let _ = handle.emit("guest-login", project_id);
    }));
    let handle = app.clone();
    manager.set_progress_sink(Box::new(move |project_id, phase, elapsed_ms| {
        let payload = serde_json::json!({ "projectId": project_id, "phase": phase, "elapsedMs": elapsed_ms });
        let _ = handle.emit("cage-start-progress", payload);
    }));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .menu(menu)
        .setup(|app| {
            report_panics(app.handle().clone());
            if let Err(e) = raise_open_files() {
                eprintln!("the open file limit could not be raised: {e}");
            }
            let data = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data)?;
            // Canonical, like the host paths of the projects it is compared with.
            let data = data.canonicalize()?;
            let pi = agent::pi::install_pi(&app.path().resource_dir()?.join("pi"), &data.join("runtime"))?;
            let runtime = Microsandbox { pi };
            let data_dir = data.clone();
            let host = Arc::new(HostAgents::new(
                Arc::new(host_agents::RealFs),
                Arc::new(host_agents::login::HostLogins { home: dirs::home_dir().unwrap_or_default() }),
                Arc::new(host_agents::SystemClock),
                data.clone(),
            ));
            // At app start, off the main thread: detection runs `--version`.
            std::thread::spawn({
                let host = host.clone();
                move || host.detect()
            });
            let vault = Arc::new(secrets::Ordered::new(Arc::new(secrets::Keychain)));
            let minimum = app.config().bundle.macos.minimum_system_version.clone().unwrap_or_default();
            app.manage(Bootstrap::default());
            app.manage(preferences::Preferences::load(data.join("preferences.json")));
            app.manage(appearance::Themes { dir: data.join("themes") });
            // Off the main thread: the runtime install and the reap take seconds.
            // The project commands need the manager, so the UI shows the projects
            // only in the Ready phase.
            let handle = app.handle().clone();
            let host_agents = host.clone();
            std::thread::spawn(move || {
                let startup = handle.state::<Bootstrap>();
                let home = match bootstrap::runtime_home() {
                    Ok(home) => home,
                    Err(e) => {
                        let message = error::Error::Io(e).message();
                        return startup.set(
                            &handle,
                            Phase::Blocked,
                            vec![Check::new("VM folder", Level::Fail, message)],
                            None,
                        );
                    }
                };
                let mut checks = bootstrap::system_checks(&data, &home, &minimum);
                if checks.iter().any(|c| c.level == Level::Fail) {
                    return startup.set(&handle, Phase::Blocked, checks, None);
                }
                let step = |step| startup.set(&handle, Phase::Installing, checks.clone(), Some(step));
                let legacy = bootstrap::legacy_homes(&data, &dirs::home_dir().unwrap_or_default());
                let manager = bootstrap::prepare_runtime(&home, &legacy, &runtime, REAP_LIMIT, step).and_then(|()| {
                    step(Step::Reap);
                    boot(runtime, vault, &data, host_agents)
                });
                match manager {
                    Ok(manager) => {
                        connect(&handle, &manager);
                        handle.manage(manager);
                        startup.set(&handle, Phase::Ready, checks, Some(Step::Ready));
                    }
                    Err(e) => {
                        checks.push(Check::new("VM runtime", Level::Fail, e.message()));
                        startup.set(&handle, Phase::Blocked, checks, None);
                    }
                }
            });
            app.manage(host);
            app.manage(runtimes::Runtimes::new(data_dir));
            app.manage(commands::tasks::Watches::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::projects::list_projects,
            commands::projects::create_project,
            commands::projects::delete_project,
            commands::projects::rename_project,
            commands::projects::reorder_projects,
            commands::projects::set_layout,
            commands::apps::put_app,
            commands::apps::remove_app,
            commands::apps::reorder_apps,
            commands::apps::start_app,
            commands::apps::stop_app,
            commands::apps::restart_app,
            commands::terminal::reorder_terminals,
            commands::terminal::set_pane_layout,
            commands::terminal::attach_session,
            commands::terminal::detach_session,
            commands::terminal::ack_session,
            commands::projects::resolve_folder,
            commands::projects::set_harness,
            commands::projects::list_folders,
            commands::projects::add_mount,
            commands::projects::remove_mount,
            commands::projects::set_mount_readonly,
            commands::network::network_catalog,
            commands::network::set_network_default,
            commands::network::set_offline,
            commands::network::put_rule,
            commands::network::remove_rule,
            commands::network::set_rule_credential,
            commands::network::test_network,
            commands::network::wildcard_for,
            commands::network::list_net_events,
            commands::projects::start_cage,
            commands::projects::stop_cage,
            commands::terminal::open_session,
            commands::terminal::reopen_terminal,
            commands::terminal::write_session,
            commands::terminal::resize_session,
            commands::terminal::close_terminal,
            commands::settings::list_runtimes,
            commands::settings::install_runtime,
            commands::settings::cancel_runtime_install,
            commands::settings::host_agents,
            commands::settings::detect_host_agents,
            commands::projects::put_host_agent,
            commands::projects::remove_host_agent,
            commands::projects::set_agents_unprompted,
            commands::tasks::put_task,
            commands::tasks::remove_task,
            commands::tasks::start_task_run,
            commands::tasks::control_task_run,
            commands::tasks::list_task_runs,
            commands::tasks::run_diff,
            commands::tasks::is_round_folder,
            commands::tasks::watch_room,
            commands::tasks::unwatch_room,
            commands::projects::take_app_notices,
            commands::projects::remove_guest_login,
            commands::projects::confirm_paths,
            commands::settings::startup_status,
            commands::settings::get_preferences,
            commands::settings::set_preferences,
            commands::settings::import_ghostty_config,
            commands::settings::storage,
            commands::settings::clean_images,
            commands::settings::legacy_storage,
            commands::settings::remove_legacy_runtime,
            commands::editors::detect_editors,
            commands::editors::open_in_editor,
            commands::editors::reveal_project,
            commands::appearance::list_themes,
            commands::appearance::save_theme,
            commands::appearance::delete_theme,
            updates::check_update,
            updates::install_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // The first exit request stops the VMs and then exits again. The second
    // request passes, so a slow stop cannot keep the app open.
    let exiting = AtomicBool::new(false);
    app.run(move |app, event| match event {
        RunEvent::ExitRequested { api, .. } if !exiting.swap(true, Ordering::SeqCst) => {
            api.prevent_exit();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                // The host shells are not VMs, so `stop_all` does not see them.
                let hangup = app.clone();
                let _ = tauri::async_runtime::spawn_blocking(move || {
                    if let Some(manager) = hangup.try_state::<Manager<Microsandbox>>() {
                        manager.kill_host_sessions();
                    }
                })
                .await;
                if let Some(manager) = app.try_state::<Manager<Microsandbox>>() {
                    manager.stop_all(STOP_ALL_LIMIT).await;
                }
                app.exit(0);
            });
        }
        // ⌘Q on macOS ends the loop without ExitRequested. Kill the host
        // shells and stop the VMs here, before the process exits.
        RunEvent::Exit => {
            if let Some(manager) = app.try_state::<Manager<Microsandbox>>() {
                manager.kill_host_sessions();
                tauri::async_runtime::block_on(manager.stop_all(STOP_ALL_LIMIT));
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_agents::fake::{FakeClock, FakeFs};
    use crate::host_agents::login::fake::FakeLogins;
    use crate::runtime::fake::FakeRuntime;
    use crate::secrets::fake::MemoryVault;

    /// A plain test: `boot` makes its own runtime context, as in the app's setup.
    #[test]
    fn boot_reaps_loads_and_names_the_removed_vms() {
        let data = std::env::temp_dir().join(format!("zwinger-boot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(data.join("proj")).unwrap();
        let data = data.canonicalize().unwrap();
        let project = cage::Project::new(data.join("proj").to_str().unwrap()).unwrap();
        store::Store::new(data.join("projects.json")).save(std::slice::from_ref(&project)).unwrap();
        let runtime = FakeRuntime::default();
        *runtime.reaped.lock().unwrap() = vec![format!("zwinger-{}", project.id), "zwinger-gone".into()];
        let host = Arc::new(HostAgents::new(
            Arc::new(FakeFs::default()),
            Arc::new(FakeLogins::default()),
            Arc::new(FakeClock(0.into())),
            data.clone(),
        ));

        let manager = boot(runtime.clone(), Arc::new(MemoryVault::default()), &data, host).unwrap();
        assert_eq!(*runtime.log.lock().unwrap(), ["reap"]);
        assert_eq!(manager.list().len(), 1);
        assert_eq!(
            manager.take_app_notices(),
            ["zwinger stopped the VMs that the last run left running: proj, zwinger-gone."]
        );
        assert!(manager.take_app_notices().is_empty());
    }

    #[test]
    fn the_open_file_limit_is_raised() {
        let soft = raise_open_files().unwrap();
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        assert_eq!(limit.rlim_cur, soft);
        assert!(soft >= OPEN_FILES.min(limit.rlim_max));
    }

    #[test]
    fn the_chat_commands_are_gone() {
        let source = include_str!("lib.rs");
        let handler = &source[source.find("generate_handler![").unwrap()..];
        let handler = &handler[..handler.find("])").unwrap()];
        assert!(handler.contains("commands::terminal::open_session,"));
        assert!(handler.contains("commands::projects::set_agents_unprompted,"));
        for gone in ["open_agent", "agent_call", "close_agent", "list_chats", "delete_chat", "rename_chat"] {
            assert!(!handler.contains(&format!("::{gone},")), "{gone}");
        }
    }
}
