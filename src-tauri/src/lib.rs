mod a2a;
mod agent;
mod appearance;
mod bootstrap;
mod cage;
mod commands;
mod editor;
mod editor_icons;
mod error;
mod external_live;
mod folder_watch;
mod host_agents;
mod instance;
mod legacy_keychain;
mod netlog;
mod network;
mod preferences;
mod proxy;
mod room_watch;
mod run_git;
mod runtime;
mod runtimes;
mod secrets;
mod session;
mod store;
mod task_agents;
mod task_artifacts;
mod task_participants;
mod task_recovery;
mod task_runs;
mod terminal;
mod updates;
mod workspaces;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, PredefinedMenuItem, Submenu};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter as _, Manager as _, RunEvent, WindowEvent, Wry};

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
/// cage start reaps again. `unreaped_legacy`: the legacy runtime homes,
/// owned or shared, whose reap failed. The agent state moves from
/// `shields/` only after the reap of this home and of each legacy home
/// succeeded, so no VM of an earlier run still mounts it. Until then, each
/// cage start reaps the failed homes again, and no action deletes, resets
/// or releases agent state ([`Manager::last_run_reaped`]).
fn boot<R: SandboxRuntime>(
    runtime: R,
    vault: Arc<dyn Vault>,
    data: &Path,
    host: Arc<HostAgents>,
    unreaped_legacy: Vec<PathBuf>,
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
    let migrate = std::fs::symlink_metadata(data.join(cage::vm_state::LEGACY_DIR)).is_ok();
    manager.migrate_pending.store(migrate, Ordering::SeqCst);
    *manager.unreaped_legacy.lock().unwrap_or_else(|e| e.into_inner()) = unreaped_legacy;
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
    tauri::async_runtime::block_on(manager.try_migrate_vm_state());
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
    // No payload: the app takes the notices from the queue, which stays the
    // only source, so a signal before the listener loses nothing.
    let handle = app.clone();
    manager.set_notice_sink(Box::new(move || {
        let _ = handle.emit("app-notices", ());
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
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                if let Some(watches) = window.try_state::<folder_watch::FolderWatches>() {
                    watches.drop_owner(window.label());
                }
                if let Some(watches) = window.try_state::<room_watch::Watches>() {
                    watches.drop_owner(window.label());
                }
            }
        })
        // A reload, also after a crash of the web content process, loses the
        // React cleanup, so the watches of the old page end here.
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                if let Some(watches) = webview.try_state::<folder_watch::FolderWatches>() {
                    watches.drop_owner(webview.label());
                }
                if let Some(watches) = webview.try_state::<room_watch::Watches>() {
                    watches.drop_owner(webview.label());
                }
            }
        })
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
            // Makes no Keychain call: each one follows a click in Settings.
            app.manage(legacy_keychain::LegacyKeychain::new(legacy_keychain::system_source()));
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
                let manager =
                    bootstrap::prepare_runtime(&home, &legacy, &runtime, REAP_LIMIT, step).and_then(|unreaped| {
                        step(Step::Reap);
                        boot(runtime, vault, &data, host_agents, unreaped)
                    });
                match manager {
                    Ok(manager) => {
                        connect(&handle, &manager);
                        handle.manage(manager);
                        // The undo receipts expire also while the app is idle.
                        let sweeper = handle.clone();
                        tauri::async_runtime::spawn(async move {
                            let mut tick = tokio::time::interval(Duration::from_secs(1));
                            loop {
                                tick.tick().await;
                                sweeper.state::<Manager<Microsandbox>>().sweep_undo();
                            }
                        });
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
            app.manage(room_watch::Watches::default());
            app.manage(folder_watch::FolderWatches::default());
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
            commands::sessions::watch_folders,
            commands::sessions::unwatch_folders,
            commands::sessions::rescan_folders,
            commands::cages::add_mount,
            commands::cages::remove_mount,
            commands::cages::undo_removal,
            commands::cages::set_mount_readonly,
            commands::network::network_catalog,
            commands::network::proxy_ca_fingerprint,
            commands::network::set_network_default,
            commands::network::set_offline,
            commands::network::set_host_port,
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
            commands::network::export_net_events,
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
            commands::terminal::terminal_history,
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
            commands::cages::set_cage_resources,
            commands::tasks::put_task,
            commands::tasks::remove_task,
            commands::tasks::start_task_run,
            commands::tasks::resume_task_run,
            commands::tasks::control_task_run,
            commands::tasks::list_task_runs,
            commands::tasks::run_diff,
            commands::tasks::review_run_git,
            commands::tasks::apply_run_git,
            commands::tasks::run_git_operation,
            commands::tasks::run_git_unfinished,
            commands::tasks::share_task_run,
            commands::tasks::task_run_share,
            commands::tasks::stop_task_run_share,
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
            commands::settings::legacy_keychain_status,
            commands::settings::scan_legacy_keychain,
            commands::settings::import_legacy_keychain,
            commands::settings::clean_legacy_keychain,
            commands::editors::detect_editors,
            commands::editors::open_in_editor,
            commands::editors::reveal_session,
            commands::editors::terminal_location,
            commands::editors::open_terminal_location,
            commands::appearance::list_themes,
            commands::appearance::save_theme,
            commands::appearance::delete_theme,
            commands::appearance::import_theme,
            commands::appearance::export_theme,
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
                manager.shares.stop_all();
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

        let manager = boot(runtime.clone(), Arc::new(MemoryVault::default()), &data, host, Vec::new()).unwrap();
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
        let (root, ids) = two();
        (boot_at(runtime, &root, Vec::new()), ids)
    }

    /// A new root with app data in `data` and two stored sessions whose
    /// folders are outside it.
    fn two() -> (PathBuf, [String; 2]) {
        let root = std::env::temp_dir().join(format!("zwinger-boot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        let root = root.canonicalize().unwrap();
        let folders = ["a", "b"].map(|name| {
            std::fs::create_dir_all(root.join(name)).unwrap();
            root.join(name)
        });
        let ids = stored(&root.join("data"), folders);
        (root, ids)
    }

    /// Boots as a fresh app process on the app data of `root`. The reap of
    /// each legacy home in `unreaped` failed.
    fn boot_at(runtime: &FakeRuntime, root: &Path, unreaped: Vec<PathBuf>) -> Manager<FakeRuntime> {
        let data = root.join("data");
        boot(runtime.clone(), Arc::new(MemoryVault::default()), &data, host_agents(root), unreaped).unwrap()
    }

    /// An agent state dir of the build before the move, with one file.
    fn old_state(root: &Path, key: &str) {
        let dir = root.join("data/shields").join(key);
        std::fs::create_dir_all(dir.join("codex")).unwrap();
        std::fs::write(dir.join("codex/auth.json"), key).unwrap();
    }

    /// Each file below the agent state dirs and the store files, with its contents.
    fn snapshot(root: &Path) -> Vec<(PathBuf, String)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    out.push((path.clone(), String::new()));
                    walk(&path, out);
                } else {
                    out.push((path.clone(), std::fs::read_to_string(&path).unwrap_or_default()));
                }
            }
        }
        let data = root.join("data");
        let mut out = Vec::new();
        for dir in ["shields", "projects"] {
            walk(&data.join(dir), &mut out);
        }
        for file in ["sessions.json", "cages.json"] {
            out.push((data.join(file), std::fs::read_to_string(data.join(file)).unwrap()));
        }
        out.sort();
        out
    }

    fn not_moved(e: &error::Error, key: &str) -> bool {
        matches!(e, error::Error::VmStateNotMoved { key: k, .. } if k == key)
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
    fn the_boot_moves_the_agent_state_after_both_reaps() {
        let runtime = FakeRuntime::default();
        let (root, [a, _]) = two();
        old_state(&root, &a);
        let manager = boot_at(&runtime, &root, Vec::new());
        assert!(!root.join("data/shields").exists());
        let dir = manager.state_dir(&a).unwrap();
        assert_eq!(dir, root.join("data/projects").join(&a));
        assert_eq!(std::fs::read_to_string(dir.join("codex/auth.json")).unwrap(), a);
        assert!(manager.take_app_notices().is_empty());
        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
    }

    #[test]
    fn a_failed_boot_reap_moves_the_agent_state_at_the_first_reap_that_succeeds() {
        let runtime = FakeRuntime::default();
        *runtime.reap_fail.lock().unwrap() = true;
        let (root, [a, _]) = two();
        old_state(&root, &a);
        let manager = boot_at(&runtime, &root, Vec::new());
        assert!(root.join("data/shields").join(&a).is_dir(), "no move while old VMs can mount it");
        let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
        assert!(e.to_string().contains("not stopped"), "{e}");
        assert!(root.join("data/shields").join(&a).is_dir());

        *runtime.reap_fail.lock().unwrap() = false;
        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
        assert!(!root.join("data/shields").exists());
        let auth = root.join("data/projects").join(&a).join("codex/auth.json");
        assert_eq!(std::fs::read_to_string(auth).unwrap(), a);
        assert_eq!(
            *runtime.log.lock().unwrap(),
            ["reap".to_owned(), "reap".into(), "reap".into(), format!("start {a}")]
        );
    }

    /// A failed or timed-out reap of a legacy home, owned or shared, gives
    /// the same `unreaped` entry: the move waits until a reap of that home
    /// succeeds, at a later cage start or at the next app start.
    #[test]
    fn a_failed_legacy_reap_defers_the_move_until_a_retry_succeeds() {
        for home in ["data/microsandbox", ".microsandbox"] {
            let runtime = FakeRuntime::default();
            *runtime.home_reap_fail.lock().unwrap() = true;
            let (root, [a, b]) = two();
            old_state(&root, &a);
            let home = root.join(home);
            let manager = boot_at(&runtime, &root, vec![home.clone()]);
            let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
            assert!(not_moved(&e, &a), "{e}");
            assert!(root.join("data/shields").join(&a).is_dir());
            assert!(!root.join("data/projects").join(&a).exists(), "no empty new state is made");
            tauri::async_runtime::block_on(manager.start(&b)).unwrap();
            let retries =
                runtime.log.lock().unwrap().iter().filter(|l| **l == format!("reap {}", home.display())).count();
            assert_eq!(retries, 2, "each start reaps the failed home again");
            tauri::async_runtime::block_on(manager.stop(&b, None)).unwrap();

            // The next start in the same process reaps it again and moves the state.
            *runtime.home_reap_fail.lock().unwrap() = false;
            tauri::async_runtime::block_on(manager.start(&a)).unwrap();
            assert!(!root.join("data/shields").exists());
            let auth = root.join("data/projects").join(&a).join("codex/auth.json");
            assert_eq!(std::fs::read_to_string(auth).unwrap(), a);
            drop(manager);

            // The next app start, with the home still failing, moves the rest after a retry.
            *runtime.home_reap_fail.lock().unwrap() = true;
            let own = format!("{b}-2");
            old_state(&root, &own);
            let manager = boot_at(&runtime, &root, vec![home.clone()]);
            assert!(root.join("data/shields").join(&own).is_dir());
            drop(manager);
            *runtime.home_reap_fail.lock().unwrap() = false;
            let manager = boot_at(&runtime, &root, Vec::new());
            assert!(!root.join("data/shields").exists());
            assert!(manager.state_dir(&own).unwrap().join("codex/auth.json").is_file());
        }
    }

    /// With no `shields/`, a failed legacy reap is still retried at each
    /// start: its VMs can mount the moved state. The starts go on.
    #[test]
    fn a_failed_legacy_reap_with_nothing_to_move_is_retried() {
        let runtime = FakeRuntime::default();
        *runtime.home_reap_fail.lock().unwrap() = true;
        let (root, [a, _]) = two();
        let home = root.join(".microsandbox");
        let manager = boot_at(&runtime, &root, vec![home.clone()]);
        assert!(!manager.last_run_reaped());
        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
        let retry = format!("reap {}", home.display());
        assert_eq!(*runtime.log.lock().unwrap(), ["reap".to_owned(), retry.clone(), format!("start {a}")]);
        tauri::async_runtime::block_on(manager.stop(&a, None)).unwrap();

        *runtime.home_reap_fail.lock().unwrap() = false;
        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
        assert!(manager.last_run_reaped());
        tauri::async_runtime::block_on(manager.stop(&a, None)).unwrap();
        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
        let retries = runtime.log.lock().unwrap().iter().filter(|l| **l == retry).count();
        assert_eq!(retries, 2, "no retry after a reap succeeded");
    }

    /// A legacy home is reaped while an unrelated VM runs or starts: the
    /// old state of the other key stays in `shields/` and the move stays
    /// pending. After that VM stops, the next start moves it.
    #[test]
    fn the_agent_state_does_not_move_while_another_vm_is_active() {
        let runtime = FakeRuntime::default();
        *runtime.home_reap_fail.lock().unwrap() = true;
        let (root, [a, b]) = two();
        old_state(&root, &a);
        let legacy = root.join("data/shields").join(&a);
        let manager = boot_at(&runtime, &root, vec![root.join("data/microsandbox")]);
        tauri::async_runtime::block_on(manager.start(&b)).unwrap();
        *runtime.home_reap_fail.lock().unwrap() = false;

        // The reap succeeds now, but `b` runs.
        let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
        assert!(not_moved(&e, &a), "{e}");
        assert!(manager.last_run_reaped());
        assert!(manager.migrate_pending.load(Ordering::SeqCst));
        assert!(legacy.join("codex/auth.json").is_file() && !root.join("data/projects").join(&a).exists());
        tauri::async_runtime::block_on(manager.stop(&b, None)).unwrap();

        // A VM that starts blocks the move too.
        manager.state.lock().unwrap().vms.insert(
            b.clone(),
            cage::manager::Vm::Starting {
                cage: b.clone(),
                members: vec![b.clone()],
                phase: cage::progress::Phase::Prepare,
            },
        );
        tauri::async_runtime::block_on(manager.try_migrate_vm_state());
        assert!(manager.migrate_pending.load(Ordering::SeqCst));
        assert!(legacy.join("codex/auth.json").is_file());
        manager.state.lock().unwrap().vms.remove(&b);

        tauri::async_runtime::block_on(manager.start(&a)).unwrap();
        assert!(!manager.migrate_pending.load(Ordering::SeqCst));
        assert!(!root.join("data/shields").exists());
        let auth = root.join("data/projects").join(&a).join("codex/auth.json");
        assert_eq!(std::fs::read_to_string(auth).unwrap(), a);
    }

    /// The own VM key of session `a` is in `projects/`, and the reap of a
    /// runtime home failed: `failed_home` is a legacy home, or `None` for
    /// the home of this run. No guest-login removal, key release or project
    /// delete changes a record or a file, and the error names the home.
    fn a_moved_own_key_stays_while_a_home_is_not_reaped(failed_home: Option<&str>) {
        let runtime = FakeRuntime::default();
        let (root, [a, b]) = two();
        let own = format!("{a}-2");
        own_vm(&root, &a, &own);
        // Session `a` moved to cage `b` and keeps its own VM of cage `a`.
        let store = store::Store::new(root.join("data"));
        let (mut sessions, cages) = store.load().unwrap();
        sessions.iter_mut().for_each(|s| s.cage = Some(b.clone()));
        store.save_sessions(&sessions).unwrap();
        store.save_cages(&cages).unwrap();
        for key in [&a, &own] {
            let dir = root.join("data/projects").join(key);
            std::fs::create_dir_all(dir.join("codex")).unwrap();
            std::fs::write(dir.join("codex/auth.json"), key).unwrap();
        }
        let unreaped = match failed_home {
            Some(home) => {
                *runtime.home_reap_fail.lock().unwrap() = true;
                vec![root.join(home)]
            }
            None => {
                *runtime.reap_fail.lock().unwrap() = true;
                Vec::new()
            }
        };
        let manager = boot_at(&runtime, &root, unreaped);
        assert!(!manager.last_run_reaped());
        let before = snapshot(&root);
        let home = failed_home.map_or("~/.zwinger/microsandbox".to_owned(), |h| root.join(h).display().to_string());
        let refused = |r: error::Result<()>| match r {
            Err(e @ error::Error::NotReaped { .. }) => {
                let text = e.message();
                assert!(text.contains(&home) && text.contains("Start a cage or restart zwinger"), "{text}");
                true
            }
            other => panic!("{other:?}"),
        };
        assert!(refused(manager.remove_guest_login(&own, agent::AgentKind::Codex)));
        assert!(refused(tauri::async_runtime::block_on(manager.set_session_vm(&a, false, None))));
        assert!(refused(tauri::async_runtime::block_on(manager.delete(&a))));
        assert!(refused(tauri::async_runtime::block_on(manager.delete(&b))));
        assert!(refused(manager.delete_cage(&a)));
        assert_eq!(snapshot(&root), before);
        assert_eq!(manager.list().sessions.len(), 2);
        let session = manager.list().sessions.into_iter().find(|s| s.session.id == a).unwrap();
        assert_eq!(session.session.vm, VmChoice::Own { key: own.clone(), cage: a.clone() });

        // A start reaps the home again, and then the release runs.
        *runtime.home_reap_fail.lock().unwrap() = false;
        *runtime.reap_fail.lock().unwrap() = false;
        tauri::async_runtime::block_on(manager.start(&b)).unwrap();
        assert!(manager.last_run_reaped());
        tauri::async_runtime::block_on(manager.stop(&b, None)).unwrap();
        tauri::async_runtime::block_on(manager.set_session_vm(&a, false, None)).unwrap();
        assert!(!root.join("data/projects").join(&own).exists());
    }

    #[test]
    fn a_failed_legacy_home_reap_keeps_a_moved_own_key() {
        for home in [".microsandbox", "data/microsandbox"] {
            a_moved_own_key_stays_while_a_home_is_not_reaped(Some(home));
        }
    }

    #[test]
    fn a_failed_current_home_reap_keeps_a_moved_own_key() {
        a_moved_own_key_stays_while_a_home_is_not_reaped(None);
    }

    #[test]
    fn a_conflict_is_a_notice_and_keeps_the_key_pending() {
        let runtime = FakeRuntime::default();
        let (root, [a, b]) = two();
        old_state(&root, &a);
        old_state(&root, &b);
        let target = root.join("data/projects").join(&a);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("new"), "new").unwrap();
        let manager = boot_at(&runtime, &root, Vec::new());
        let notices = manager.take_app_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let legacy = root.join("data/shields").join(&a);
        assert!(notices[0].contains(&legacy.display().to_string()), "{notices:?}");
        assert!(notices[0].contains(&target.display().to_string()), "{notices:?}");
        assert!(legacy.join("codex/auth.json").is_file() && target.join("new").is_file());
        let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
        assert!(not_moved(&e, &a), "{e}");
        tauri::async_runtime::block_on(manager.start(&b)).unwrap();
    }

    /// Session `a` uses cage `b` and keeps its own VM `<a>-2` of cage `a`,
    /// as after a cage change. Cage `a` has no session. All three keys wait
    /// for the move, after a failed boot reap with no VM in the map, and
    /// then because their new dirs have contents.
    #[test]
    fn a_key_whose_state_did_not_move_changes_no_record_and_no_file() {
        let runtime = FakeRuntime::default();
        *runtime.reap_fail.lock().unwrap() = true;
        *runtime.home_reap_fail.lock().unwrap() = true;
        let (root, [a, b]) = two();
        let own = format!("{a}-2");
        let store = store::Store::new(root.join("data"));
        let (mut sessions, cages) = store.load().unwrap();
        for session in &mut sessions {
            session.cage = Some(b.clone());
            if session.id == a {
                session.vm = VmChoice::Own { key: own.clone(), cage: a.clone() };
            }
        }
        store.save_sessions(&sessions).unwrap();
        store.save_cages(&cages).unwrap();
        for key in [&a, &b, &own] {
            old_state(&root, key);
            let target = root.join("data/projects").join(key);
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("new"), "new").unwrap();
        }
        let manager = boot_at(&runtime, &root, vec![root.join("data/microsandbox")]);
        let before = snapshot(&root);
        assert!(matches!(manager.delete_cage(&a), Err(error::Error::NotReaped { .. })));
        assert_eq!(snapshot(&root), before);
        *runtime.reap_fail.lock().unwrap() = false;
        *runtime.home_reap_fail.lock().unwrap() = false;

        // `claim_start` with `renew_own_key`, and with a planned key.
        assert!(not_moved(&tauri::async_runtime::block_on(manager.start(&a)).unwrap_err(), &own));
        assert!(not_moved(&tauri::async_runtime::block_on(manager.start(&b)).unwrap_err(), &b));
        assert!(not_moved(&manager.claim_start(&b).unwrap_err(), &b));
        assert!(not_moved(&manager.remove_guest_login(&own, agent::AgentKind::Codex).unwrap_err(), &own));
        assert!(not_moved(&tauri::async_runtime::block_on(manager.set_session_vm(&a, false, None)).unwrap_err(), &own));
        let e = manager.delete_cage(&a).unwrap_err();
        assert!(not_moved(&e, &a) || not_moved(&e, &own), "{e}");
        assert!(not_moved(&tauri::async_runtime::block_on(manager.delete(&a)).unwrap_err(), &own));
        {
            let mut state = manager.state.lock().unwrap();
            assert!(not_moved(&manager.wipe_vm(&mut state, &b).unwrap_err(), &b));
            assert!(not_moved(&manager.remove_vm_dirs(&own).unwrap_err(), &own));
            assert!(state.vms.is_empty());
        }
        assert_eq!(snapshot(&root), before);
        assert_eq!(manager.list().sessions.len(), 2);
        assert_eq!(manager.list().cages.len(), 2);

        // A new own key skips a key with an old dir.
        old_state(&root, &format!("{b}-2"));
        tauri::async_runtime::block_on(manager.set_session_vm(&b, true, None)).unwrap();
        let session = manager.list().sessions.into_iter().find(|s| s.session.id == b).unwrap();
        assert_eq!(session.session.vm, VmChoice::Own { key: format!("{b}-3"), cage: b.clone() });
        assert!(root.join("data/shields").join(format!("{b}-2")).join("codex/auth.json").is_file());
    }

    /// Stores session `a` with its own VM `<a>-2` of cage `a`.
    fn own_vm(root: &Path, a: &str, key: &str) {
        let store = store::Store::new(root.join("data"));
        let (mut sessions, cages) = store.load().unwrap();
        for session in &mut sessions {
            if session.id == a {
                session.vm = VmChoice::Own { key: key.to_owned(), cage: a.to_owned() };
            }
        }
        store.save_sessions(&sessions).unwrap();
        store.save_cages(&cages).unwrap();
    }

    /// The state moved in an earlier run, whose VMs may still run: the boot
    /// reap of this run failed. No action deletes a state or hands a key out.
    #[test]
    fn a_moved_state_changes_nothing_while_the_last_run_is_not_reaped() {
        let runtime = FakeRuntime::default();
        *runtime.reap_fail.lock().unwrap() = true;
        let (root, [a, b]) = two();
        let own = format!("{a}-2");
        own_vm(&root, &a, &own);
        // Session `a` moved to cage `b` and keeps its own VM of cage `a`.
        let store = store::Store::new(root.join("data"));
        let (mut sessions, cages) = store.load().unwrap();
        sessions.iter_mut().for_each(|s| s.cage = Some(b.clone()));
        store.save_sessions(&sessions).unwrap();
        store.save_cages(&cages).unwrap();
        for key in [&a, &b, &own] {
            let dir = root.join("data/projects").join(key);
            std::fs::create_dir_all(dir.join("codex")).unwrap();
            std::fs::write(dir.join("codex/auth.json"), key).unwrap();
        }
        let manager = boot_at(&runtime, &root, Vec::new());
        let before = snapshot(&root);
        let not_reaped = |r: error::Result<()>| matches!(r, Err(error::Error::NotReaped { .. }));
        assert!(not_reaped(manager.remove_guest_login(&own, agent::AgentKind::Codex)));
        assert!(not_reaped(manager.remove_guest_login(&b, agent::AgentKind::Codex)));
        assert!(not_reaped(tauri::async_runtime::block_on(manager.set_session_vm(&a, false, None))));
        assert!(not_reaped(tauri::async_runtime::block_on(manager.set_session_vm(&b, true, None))));
        assert!(not_reaped(manager.delete_cage(&a)));
        assert!(not_reaped(tauri::async_runtime::block_on(manager.delete(&a))));
        // Session `b` uses the shared VM of cage `b`.
        assert!(not_reaped(tauri::async_runtime::block_on(manager.delete(&b))));
        assert_eq!(manager.list().sessions.len(), 2);
        {
            let mut state = manager.state.lock().unwrap();
            assert!(not_reaped(manager.wipe_vm(&mut state, &own)));
            assert!(not_reaped(manager.remove_vm_dirs(&b)));
            assert!(not_reaped(manager.new_vm_key(&mut state, &b).map(drop)));
        }
        assert_eq!(snapshot(&root), before);

        // After a reap succeeds, the actions run again.
        *runtime.reap_fail.lock().unwrap() = false;
        tauri::async_runtime::block_on(manager.start(&b)).unwrap();
        tauri::async_runtime::block_on(manager.set_session_vm(&a, false, None)).unwrap();
        assert!(!root.join("data/projects").join(&own).exists());
    }

    /// A link at `shields/`, `projects/` or `projects/<key>` is never used for
    /// a fresh start, and nothing is made at its target.
    #[test]
    fn a_fresh_start_never_uses_a_linked_state_path() {
        for linked in ["shields", "projects", "projects/<a>"] {
            let runtime = FakeRuntime::default();
            let (root, [a, b]) = two();
            let outside = root.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            let link = root.join("data").join(linked.replace("<a>", &a));
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let manager = boot_at(&runtime, &root, Vec::new());
            let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
            assert!(matches!(e, error::Error::VmStatePath { .. }), "{linked}: {e}");
            let b_start = tauri::async_runtime::block_on(manager.start(&b));
            assert_eq!(b_start.is_ok(), linked == "projects/<a>", "{linked}: {b_start:?}");
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "{linked}");
            assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "{linked}");
        }
    }

    /// A root-wide lookup error ends the search for a new own VM key.
    #[test]
    fn a_linked_state_parent_fails_a_new_own_key_once() {
        let runtime = FakeRuntime::default();
        let (root, [a, _]) = two();
        let manager = boot_at(&runtime, &root, Vec::new());
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("data/projects")).unwrap();
        let e = tauri::async_runtime::block_on(manager.set_session_vm(&a, true, None)).unwrap_err();
        assert!(matches!(e, error::Error::VmStatePath { .. }), "{e}");
        let session = manager.list().sessions.into_iter().find(|s| s.session.id == a).unwrap();
        assert_eq!(session.session.vm, VmChoice::Shared);
    }

    /// A stored VM key that is not one path component is refused.
    #[test]
    fn a_malformed_stored_vm_key_is_refused() {
        for key in ["../escape", "a/b", ".."] {
            let runtime = FakeRuntime::default();
            let (root, [a, _]) = two();
            own_vm(&root, &a, key);
            let manager = boot_at(&runtime, &root, Vec::new());
            let e = tauri::async_runtime::block_on(manager.start(&a)).unwrap_err();
            assert!(matches!(e, error::Error::BadVmKey(_)), "{key}: {e}");
            assert!(matches!(manager.remove_guest_login(key, agent::AgentKind::Codex), Err(error::Error::BadVmKey(_))));
            assert!(!root.join("data/projects").exists() && !root.join("escape").exists(), "{key}");
        }
    }

    /// The backup of the shields store stays or goes by the app version:
    /// the store tests check that with explicit versions.
    #[test]
    fn a_direct_upgrade_from_the_shields_store_converts_it_and_moves_the_state() {
        let runtime = FakeRuntime::default();
        let root = std::env::temp_dir().join(format!("zwinger-boot-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("proj")).unwrap();
        let root = root.canonicalize().unwrap();
        let old = serde_json::json!([{ "id": "a", "name": "proj", "folder": root.join("proj"), "mounts": [],
            "network": { "allowAll": false, "domains": [] } }])
        .to_string();
        std::fs::write(root.join("data/shields.json"), &old).unwrap();
        old_state(&root, "a");
        let manager = boot_at(&runtime, &root, Vec::new());
        assert_eq!(manager.list().cages[0].cage.id, "a");
        assert_eq!(std::fs::read_to_string(root.join("data/projects.json")).unwrap(), old);
        assert_eq!(std::fs::read_to_string(root.join("data/projects/a/codex/auth.json")).unwrap(), "a");
        assert!(!root.join("data/shields").exists());
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
