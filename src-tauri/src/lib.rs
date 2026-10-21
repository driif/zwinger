mod agent;
mod commands;
mod error;
mod host_agents;
mod netlog;
mod runtime;
mod runtimes;
mod secrets;
mod sessions;
mod shield;
mod store;
mod task_agents;
mod task_artifacts;
mod task_runs;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter as _, Manager as _, RunEvent, Wry};

use crate::host_agents::HostAgents;
use crate::runtime::{Microsandbox, SandboxRuntime};
use crate::secrets::Vault;
use crate::sessions::Manager;

const REAP_LIMIT: Duration = Duration::from_secs(10);
const STOP_ALL_LIMIT: Duration = Duration::from_secs(20);

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

/// Removes the VMs of an earlier run, loads the shields and makes the
/// manager. The manager starts with no VMs, so every shield shows as
/// stopped; a notice names the removed VMs.
fn boot<R: SandboxRuntime>(
    runtime: R,
    vault: Arc<dyn Vault>,
    data: &Path,
    host: Arc<HostAgents>,
) -> error::Result<Manager<R>> {
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
    let store = store::Store::new(data.join("shields.json"));
    let manager = Manager::new(runtime, store, vault, data.to_owned(), host)?;
    if !reaped.is_empty() {
        let shields = manager.list();
        let names: Vec<String> = reaped
            .iter()
            .map(|vm| {
                let shield = runtime::shield_id(vm).and_then(|id| shields.iter().find(|v| v.shield.id == id));
                shield.map_or_else(|| vm.clone(), |v| v.shield.name.clone())
            })
            .collect();
        manager
            .add_app_notice(format!("zwinger stopped the VMs that the last run left running: {}.", names.join(", ")));
    }
    Ok(manager)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .menu(menu)
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data)?;
            // Canonical, like the host paths of the shields it is compared with.
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
            let manager = boot(runtime, vault, &data, host.clone())?;
            // One app-wide event, so the rail and the map update while the
            // Network tab is closed.
            let handle = app.handle().clone();
            manager.set_net_sink(Box::new(move |shield_id, event| {
                let _ = handle.emit("net-event", serde_json::json!({ "shieldId": shield_id, "event": event }));
            }));
            // A host login is due: the UI shows the notice or 'Restart to
            // renew', and the manager restarts the shield if the Mac has a
            // newer login and the user turned that on. The renewal ends with
            // the VM.
            let handle = app.handle().clone();
            manager.set_renew_sink(Box::new(move |shield_id| {
                let _ = handle.emit("renew-due", shield_id);
                let (handle, shield_id) = (handle.clone(), shield_id.to_owned());
                tauri::async_runtime::spawn(async move {
                    let manager = handle.state::<Manager<Microsandbox>>();
                    if let Err(e) = manager.renew(&shield_id).await {
                        eprintln!("renewal of {shield_id} failed: {e}");
                    }
                    let _ = handle.emit("renew-due", shield_id);
                });
            }));
            let handle = app.handle().clone();
            manager.set_guest_sink(Box::new(move |shield_id| {
                let _ = handle.emit("guest-login", shield_id);
            }));
            app.manage(manager);
            app.manage(host);
            app.manage(runtimes::Runtimes::new(data_dir));
            app.manage(commands::Watches::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_shields,
            commands::create_shield,
            commands::delete_shield,
            commands::rename_shield,
            commands::reorder_shields,
            commands::set_layout,
            commands::put_app,
            commands::remove_app,
            commands::reorder_apps,
            commands::start_app,
            commands::stop_app,
            commands::restart_app,
            commands::reorder_terminals,
            commands::attach_session,
            commands::detach_session,
            commands::ack_session,
            commands::resolve_folder,
            commands::set_harness,
            commands::list_folders,
            commands::add_mount,
            commands::remove_mount,
            commands::set_mount_readonly,
            commands::set_network_mode,
            commands::add_domain,
            commands::remove_domain,
            commands::allow_host,
            commands::wildcard_for,
            commands::list_net_events,
            commands::put_secret,
            commands::remove_secret,
            commands::list_secrets,
            commands::start_shield,
            commands::stop_shield,
            commands::open_session,
            commands::reopen_terminal,
            commands::write_session,
            commands::resize_session,
            commands::close_terminal,
            commands::list_runtimes,
            commands::install_runtime,
            commands::cancel_runtime_install,
            commands::host_agents,
            commands::detect_host_agents,
            commands::put_host_agent,
            commands::remove_host_agent,
            commands::set_agents_unprompted,
            commands::set_auto_renew,
            commands::put_task,
            commands::remove_task,
            commands::start_task_run,
            commands::control_task_run,
            commands::list_task_runs,
            commands::run_diff,
            commands::is_round_folder,
            commands::watch_room,
            commands::unwatch_room,
            commands::take_app_notices,
            commands::remove_guest_login,
            commands::confirm_paths,
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
                app.state::<Manager<Microsandbox>>().stop_all(STOP_ALL_LIMIT).await;
                app.exit(0);
            });
        }
        // ⌘Q on macOS ends the loop without ExitRequested. Stop the VMs here,
        // before the process exits.
        RunEvent::Exit => {
            tauri::async_runtime::block_on(app.state::<Manager<Microsandbox>>().stop_all(STOP_ALL_LIMIT));
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
        let shield = shield::Shield::new(data.join("proj").to_str().unwrap()).unwrap();
        store::Store::new(data.join("shields.json")).save(std::slice::from_ref(&shield)).unwrap();
        let runtime = FakeRuntime::default();
        *runtime.reaped.lock().unwrap() = vec![format!("zwinger-{}", shield.id), "zwinger-gone".into()];
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
    fn the_chat_commands_are_gone() {
        let source = include_str!("lib.rs");
        let handler = &source[source.find("generate_handler![").unwrap()..];
        let handler = &handler[..handler.find("])").unwrap()];
        assert!(handler.contains("commands::open_session,"));
        assert!(handler.contains("commands::set_agents_unprompted,"));
        for gone in ["open_agent", "agent_call", "close_agent", "list_chats", "delete_chat", "rename_chat"] {
            assert!(!handler.contains(&format!("commands::{gone},")), "{gone}");
        }
    }
}
