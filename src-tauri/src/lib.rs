mod agent;
mod appearance;
mod bootstrap;
mod cage;
mod commands;
mod editor;
mod editor_icons;
mod error;
mod host_agents;
mod instance;
mod netlog;
mod network;
mod preferences;
mod proxy;
mod runtime;
mod runtimes;
mod secrets;
mod session;
mod store;
mod task_agents;
mod task_artifacts;
mod task_runs;
mod terminal;
mod updates;
mod workspaces;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter as _, Manager as _, RunEvent, Wry};

use crate::bootstrap::{Bootstrap, Check, Level, Phase, Step};
use crate::cage::manager::{Manager, SessionView};
use crate::host_agents::HostAgents;
use crate::instance::InstanceLock;
use crate::runtime::{Microsandbox, SandboxRuntime};
use crate::secrets::Vault;
use crate::session::VmChoice;

pub use instance::run_update_helper;

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

/// Removes the VMs of an earlier run, loads the sessions and cages and makes the
/// manager. The manager starts with no VMs, so every cage shows as
/// stopped; a notice names the removed VMs. If the reap fails, the first
/// cage start reaps again.
fn boot<R: SandboxRuntime>(
    runtime: R,
    vault: Arc<dyn Vault>,
    data: &Path,
    host: Arc<HostAgents>,
) -> error::Result<Manager<R>> {
    // The timer must be made inside the runtime, so it is in the future.
    let reaped = tauri::async_runtime::block_on(async { tokio::time::timeout(REAP_LIMIT, runtime.reap()).await });
    let reaped = match reaped {
        Ok(Ok(reaped)) => Some(reaped),
        Ok(Err(e)) => {
            eprintln!("reap failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("reap timed out");
            None
        }
    };
    let store = store::Store::new(data.to_owned());
    let manager = Manager::new(runtime, store, vault, data.to_owned(), host)?;
    let Some(reaped) = reaped else {
        manager.reap_pending.store(true, Ordering::SeqCst);
        return Ok(manager);
    };
    if !reaped.is_empty() {
        let listing = manager.list();
        // The shared VM has the name of its cage, an own VM the name of its session.
        let name = |key: &str| {
            let cage = listing.cages.iter().find(|v| v.cage.id == key).map(|v| v.cage.name.clone());
            cage.or_else(|| {
                let own = |s: &&SessionView| matches!(&s.session.vm, VmChoice::Own { key: k, .. } if k == key);
                listing.sessions.iter().find(own).map(|s| s.session.name.clone())
            })
        };
        let names: Vec<String> =
            reaped.iter().map(|vm| runtime::vm_key(vm).and_then(name).unwrap_or_else(|| vm.clone())).collect();
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
    manager.set_net_sink(Box::new(move |vm_key, event| {
        let _ = handle.emit("net-event", serde_json::json!({ "vmKey": vm_key, "event": event }));
    }));
    let handle = app.clone();
    manager.set_agent_sink(Box::new(move |process_id, state| {
        let _ = handle.emit("agent-state", serde_json::json!({ "processId": process_id, "state": state }));
    }));
    let handle = app.clone();
    manager.set_folder_sink(Box::new(move |process_id, seq, folder| {
        let _ = handle
            .emit("terminal-folder", serde_json::json!({ "processId": process_id, "seq": seq, "folder": folder }));
    }));
    let handle = app.clone();
    manager.set_guest_sink(Box::new(move |vm_key| {
        let _ = handle.emit("guest-login", vm_key);
    }));
    let handle = app.clone();
    manager.set_progress_sink(Box::new(move |vm_key, phase, elapsed_ms| {
        let payload = serde_json::json!({ "vmKey": vm_key, "phase": phase, "elapsedMs": elapsed_ms });
        let _ = handle.emit("cage-start-progress", payload);
    }));
}

/// Makes the app data folder and locks it. The path is canonical, like the
/// host paths of the sessions it is compared with.
fn open_data(dir: &Path) -> std::io::Result<(PathBuf, Option<InstanceLock>)> {
    std::fs::create_dir_all(dir)?;
    let data = dir.canonicalize()?;
    let lock = instance::acquire(&data)?;
    Ok((data, lock))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .menu(menu)
        .setup(|app| {
            // Must stay first: a second instance must not touch the data or
            // reap the VMs of the first.
            let (data, lock) = open_data(&app.path().app_data_dir()?)?;
            let Some(lock) = lock else {
                use tauri_plugin_dialog::DialogExt as _;
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
                let handle = app.handle().clone();
                app.dialog()
                    .message("zwinger is already open with the same app data. Use the open window, or quit it first.")
                    .title("zwinger")
                    .kind(tauri_plugin_dialog::MessageDialogKind::Warning)
                    .show(move |_| handle.exit(0));
                return Ok(());
            };
            app.manage(lock);
            app.manage(commands::window::WindowReveal::default());
            report_panics(app.handle().clone());
            if let Err(e) = raise_open_files() {
                eprintln!("the open file limit could not be raised: {e}");
            }
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
            let vault = Arc::new(secrets::FileVault::new(&data)?);
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
            app.manage(editor_icons::EditorIcons::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::window::reveal_main_window,
            commands::sessions::list_sessions,
            commands::sessions::create_session,
            commands::sessions::delete_session,
            commands::sessions::rename_session,
            commands::sessions::list_workspaces,
            commands::sessions::create_workspace,
            commands::sessions::rename_workspace,
            commands::sessions::delete_workspace,
            commands::sessions::arrange_workspaces,
            commands::sessions::set_layout,
            commands::sessions::set_viewport,
            commands::terminal::reorder_terminals,
            commands::terminal::set_pane_layout,
            commands::terminal::attach_session,
            commands::terminal::detach_session,
            commands::terminal::ack_session,
            commands::sessions::resolve_folder,
            commands::sessions::set_harness,
            commands::sessions::set_project_editor,
            commands::sessions::set_project_accent,
            commands::sessions::list_folders,
            commands::cages::add_mount,
            commands::cages::remove_mount,
            commands::cages::set_mount_readonly,
            commands::network::network_catalog,
            commands::network::proxy_ca_fingerprint,
            commands::network::set_network_default,
            commands::network::set_offline,
            commands::network::put_rule,
            commands::network::remove_rule,
            commands::network::set_rule_credential,
            commands::network::stored_credentials,
            commands::network::library_credentials,
            commands::network::set_library_credential,
            commands::network::delete_library_credential,
            commands::network::test_network,
            commands::network::wildcard_for,
            commands::network::list_net_events,
            commands::cages::start_cage,
            commands::cages::stop_cage,
            commands::cages::restart_cage,
            commands::cages::restart_cage_to_add,
            commands::cages::create_cage,
            commands::cages::rename_cage,
            commands::cages::delete_cage,
            commands::cages::set_session_cage,
            commands::cages::set_session_vm,
            commands::cages::set_workspace_cage,
            commands::terminal::open_session,
            commands::terminal::reopen_terminal,
            commands::terminal::write_session,
            commands::terminal::resize_session,
            commands::terminal::close_terminal,
            commands::terminal::terminal_folders,
            commands::settings::list_runtimes,
            commands::settings::install_runtime,
            commands::settings::cancel_runtime_install,
            commands::settings::host_agents,
            commands::settings::detect_host_agents,
            commands::cages::put_host_agent,
            commands::cages::set_claude_token,
            commands::cages::remove_host_agent,
            commands::cages::set_agents_unprompted,
            commands::tasks::put_task,
            commands::tasks::remove_task,
            commands::tasks::start_task_run,
            commands::tasks::control_task_run,
            commands::tasks::list_task_runs,
            commands::tasks::run_diff,
            commands::tasks::is_round_folder,
            commands::tasks::watch_room,
            commands::tasks::unwatch_room,
            commands::sessions::take_app_notices,
            commands::cages::remove_guest_login,
            commands::sessions::confirm_paths,
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
            commands::editors::reveal_session,
            commands::editors::terminal_location,
            commands::editors::open_terminal_location,
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
        RunEvent::Reopen { .. }
            if app.try_state::<InstanceLock>().is_some()
                && app.state::<commands::window::WindowReveal>().is_revealed()
                && let Some(window) = app.get_webview_window("main") =>
        {
            let _ = window.show();
            let _ = window.set_focus();
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
    use std::path::PathBuf;

    #[test]
    fn open_data_makes_the_folder_and_takes_the_lock() {
        let dir = std::env::temp_dir().join(format!("zwinger-open-{}", uuid::Uuid::new_v4())).join("data");
        let (data, lock) = open_data(&dir).unwrap();
        assert_eq!(data, dir.canonicalize().unwrap());
        assert!(lock.is_some());
        assert!(instance::acquire(&data).unwrap().is_none());
    }

    /// A plain test: `boot` makes its own runtime context, as in the app's setup.
    #[test]
    fn boot_reaps_loads_and_names_the_removed_vms() {
        let data = std::env::temp_dir().join(format!("zwinger-boot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(data.join("proj")).unwrap();
        let data = data.canonicalize().unwrap();
        let [id] = stored(&data, [data.join("proj")]);
        let runtime = FakeRuntime::default();
        *runtime.reaped.lock().unwrap() = vec![format!("zwinger-{id}"), "zwinger-gone".into()];
        let host = Arc::new(HostAgents::new(
            Arc::new(FakeFs::default()),
            Arc::new(FakeLogins::default()),
            Arc::new(FakeClock(0.into())),
            data.clone(),
        ));

        let manager = boot(runtime.clone(), Arc::new(MemoryVault::default()), &data, host).unwrap();
        assert_eq!(*runtime.log.lock().unwrap(), ["reap"]);
        assert_eq!(manager.list().sessions.len(), 1);
        assert_eq!(
            manager.take_app_notices(),
            ["zwinger stopped the VMs that the last run left running: proj, zwinger-gone."]
        );
        assert!(manager.take_app_notices().is_empty());
    }

    fn host_agents(dir: &Path) -> Arc<HostAgents> {
        Arc::new(HostAgents::new(
            Arc::new(FakeFs::default()),
            Arc::new(FakeLogins::default()),
            Arc::new(FakeClock(0.into())),
            dir.to_owned(),
        ))
    }

    /// Saves a session with its own cage of the same id per folder, as the
    /// conversion does. Returns their ids.
    fn stored<const N: usize>(data: &Path, folders: [PathBuf; N]) -> [String; N] {
        let pairs = folders.map(|folder| {
            let session = session::Session::new(folder.to_str().unwrap(), None, []).unwrap();
            let cage = cage::Cage::new(session.id.clone(), session.name.clone());
            (session::Session { cage: Some(cage.id.clone()), ..session }, cage)
        });
        let store = store::Store::new(data.to_owned());
        store.save_cages(&pairs.iter().map(|p| p.1.clone()).collect::<Vec<_>>()).unwrap();
        store.save_sessions(&pairs.iter().map(|p| p.0.clone()).collect::<Vec<_>>()).unwrap();
        pairs.map(|p| p.0.id)
    }

    /// Boots with two stored sessions whose folders are outside the app data.
    fn boot_two(runtime: &FakeRuntime) -> (Manager<FakeRuntime>, [String; 2]) {
        let root = std::env::temp_dir().join(format!("zwinger-boot-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let root = root.canonicalize().unwrap();
        let folders = ["a", "b"].map(|name| {
            std::fs::create_dir_all(root.join(name)).unwrap();
            root.join(name)
        });
        let data = root.join("data");
        let ids = stored(&data, folders);
        let manager = boot(runtime.clone(), Arc::new(MemoryVault::default()), &data, host_agents(&root)).unwrap();
        (manager, ids)
    }

    fn reaps(runtime: &FakeRuntime) -> usize {
        runtime.log.lock().unwrap().iter().filter(|l| *l == "reap").count()
    }

    #[test]
    fn a_failed_boot_reap_reaps_at_the_first_start() {
        let runtime = FakeRuntime::default();
        *runtime.reap_fail.lock().unwrap() = true;
        let (manager, [a, b]) = boot_two(&runtime);
        assert_eq!(reaps(&runtime), 1);
        *runtime.reap_fail.lock().unwrap() = false;
        tauri::async_runtime::block_on(async {
            manager.start(&a).await.unwrap();
            manager.start(&b).await.unwrap();
        });
        let log = runtime.log.lock().unwrap().clone();
        assert_eq!(log, ["reap".to_owned(), "reap".into(), format!("start {a}"), format!("start {b}")]);
    }

    #[test]
    fn a_reap_that_still_fails_blocks_each_start() {
        let runtime = FakeRuntime::default();
        *runtime.reap_fail.lock().unwrap() = true;
        let (manager, [a, _]) = boot_two(&runtime);
        for _ in 0..2 {
            let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
            assert!(e.to_string().contains("the VMs of the last app run are not stopped: reap failed"), "{e}");
        }
        assert_eq!(*runtime.log.lock().unwrap(), ["reap", "reap", "reap"]);
        assert_eq!(manager.list().cages[0].shared().status, cage::manager::Status::Stopped);
    }

    #[test]
    fn concurrent_starts_reap_once() {
        let runtime = FakeRuntime::default();
        *runtime.reap_fail.lock().unwrap() = true;
        let (manager, [a, b]) = boot_two(&runtime);
        *runtime.reap_fail.lock().unwrap() = false;
        let (ra, rb) = tauri::async_runtime::block_on(async { tokio::join!(manager.start(&a), manager.start(&b)) });
        ra.unwrap();
        rb.unwrap();
        assert_eq!(reaps(&runtime), 2);
        assert_eq!(runtime.log.lock().unwrap()[..2], ["reap", "reap"]);
    }

    #[test]
    fn a_successful_boot_reap_gives_no_second_reap() {
        let runtime = FakeRuntime::default();
        let (manager, [a, _]) = boot_two(&runtime);
        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
        assert_eq!(*runtime.log.lock().unwrap(), ["reap".to_owned(), format!("start {a}")]);
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
        assert!(handler.contains("commands::cages::set_agents_unprompted,"));
        for gone in ["open_agent", "agent_call", "close_agent", "list_chats", "delete_chat", "rename_chat"] {
            assert!(!handler.contains(&format!("::{gone},")), "{gone}");
        }
    }
}
