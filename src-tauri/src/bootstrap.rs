use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use microsandbox::setup::{
    InstallOptions, InstallSource, RuntimeOrigin, Version, install_runtime, resolve_runtime, resolve_runtime_version,
};
use microsandbox::{Image, LocalBackend, MicrosandboxError, set_default_backend, with_backend};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter as _};

use crate::error::{Error, Result};
use crate::runtime::{self, SandboxRuntime};

/// The first image pull and the agent runtimes need about this much.
const MIN_FREE: u64 = 5_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Checking,
    Installing,
    Ready,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    Pass,
    Warn,
    Fail,
}

/// One system check, with what to do if it does not pass.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub message: String,
}

impl Check {
    pub fn new(name: &'static str, level: Level, message: impl Into<String>) -> Self {
        Self { name, level, message: message.into() }
    }
}

/// The steps of the runtime start, in order. The embedded install has no
/// progress hook, so the UI shows the step and its time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    Resolve,
    ReapLegacy,
    Install,
    Reap,
    Ready,
}

/// The app start. The project commands work only in the `Ready` phase.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Startup {
    pub phase: Phase,
    pub checks: Vec<Check>,
    pub step: Option<Step>,
    /// When `step` started, in milliseconds since the Unix epoch.
    pub started_at: u64,
}

pub struct Bootstrap(Mutex<Startup>);

impl Default for Bootstrap {
    fn default() -> Self {
        Self(Mutex::new(Startup { phase: Phase::Checking, checks: Vec::new(), step: None, started_at: now() }))
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Bootstrap {
    pub fn get(&self) -> Startup {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, app: &AppHandle, phase: Phase, checks: Vec<Check>, step: Option<Step>) {
        let startup = Startup { phase, checks, step, started_at: now() };
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = startup.clone();
        let _ = app.emit("startup", startup);
    }
}

/// The microsandbox home of zwinger. It is short, because microsandbox puts
/// the sockets of each VM below it.
pub fn runtime_home() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let home = dirs::home_dir()
        .ok_or_else(|| std::io::Error::other("this user has no home folder"))?
        .join(".zwinger")
        .join("microsandbox");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&home)?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    Ok(home)
}

/// The size of `sockaddr_un.sun_path` on macOS, with the closing zero byte.
const SUN_PATH: usize = 104;

/// The longest socket that microsandbox makes below `home`:
/// `<home>/run/sandboxes/<24 hex>/control.sock`.
fn control_socket_len(home: &Path) -> usize {
    home.join("run/sandboxes").join("0".repeat(24)).join("control.sock").as_os_str().len()
}

pub fn socket_fits(home: &Path) -> Check {
    let len = control_socket_len(home);
    if len < SUN_PATH {
        return Check::new("VM folder", Level::Pass, format!("{} is short enough for the VM sockets.", home.display()));
    }
    Check::new(
        "VM folder",
        Level::Fail,
        format!(
            "The path of the VM folder {} is too long for the VM sockets ({len} of at most {} bytes). This happens with a long user name. Use a macOS user account with a shorter name.",
            home.display(),
            SUN_PATH - 1
        ),
    )
}

/// `translated` is `sysctl.proc_translated`: `1` under Rosetta. Intel Macs do not have it.
pub fn arch(arch: &str, translated: Option<&str>) -> Check {
    if translated == Some("1") {
        return Check::new(
            "Processor",
            Level::Fail,
            "This copy of zwinger is for Intel Macs and runs under Rosetta. Download the Apple silicon build of zwinger.",
        );
    }
    if arch != "aarch64" {
        return Check::new(
            "Processor",
            Level::Fail,
            "zwinger runs only on Macs with Apple silicon (M1 or newer). This Mac is not supported.",
        );
    }
    Check::new("Processor", Level::Pass, "Apple silicon.")
}

pub fn hypervisor(hv_support: Option<&str>) -> Check {
    match hv_support {
        Some("1") => Check::new("Virtualization", Level::Pass, "Hypervisor.framework is available."),
        Some("0") => Check::new(
            "Virtualization",
            Level::Fail,
            "This Mac cannot run virtual machines. If macOS itself runs in a virtual machine, run zwinger on the host Mac instead.",
        ),
        _ => Check::new(
            "Virtualization",
            Level::Fail,
            "zwinger could not read the virtualization support of this Mac (sysctl kern.hv_support). Restart the Mac, then open zwinger again.",
        ),
    }
}

pub fn macos(version: Option<&str>, minimum: &str) -> Check {
    let parse = |v: &str| -> Option<Vec<u32>> { v.trim().split('.').map(|p| p.parse().ok()).collect() };
    let at_least = |found: &[u32], min: &[u32]| {
        (0..found.len().max(min.len()))
            .map(|i| (found.get(i).copied().unwrap_or(0), min.get(i).copied().unwrap_or(0)))
            .find(|(f, m)| f != m)
            .is_none_or(|(f, m)| f > m)
    };
    // No minimum in the config: every version passes.
    let min = parse(minimum).unwrap_or_default();
    match version.and_then(parse) {
        Some(found) if at_least(&found, &min) => {
            Check::new("macOS", Level::Pass, format!("macOS {} is supported.", version.unwrap_or_default().trim()))
        }
        Some(_) => Check::new(
            "macOS",
            Level::Fail,
            format!(
                "zwinger needs macOS {minimum} or newer. This Mac has macOS {}. Update macOS in System Settings > General > Software Update.",
                version.unwrap_or_default().trim()
            ),
        ),
        None => Check::new(
            "macOS",
            Level::Warn,
            format!("zwinger could not read the macOS version. It needs macOS {minimum} or newer."),
        ),
    }
}

pub fn disk(free: Option<u64>) -> Check {
    let gb = |bytes: u64| format!("{:.1} GB", bytes as f64 / 1e9);
    match free {
        Some(free) if free >= MIN_FREE => Check::new("Disk space", Level::Pass, format!("{} free.", gb(free))),
        Some(free) => Check::new(
            "Disk space",
            Level::Warn,
            format!(
                "Only {} free. The first cage image and the agents need about 5 GB. Free some space before you start a cage.",
                gb(free)
            ),
        ),
        None => Check::new("Disk space", Level::Warn, "zwinger could not read the free disk space."),
    }
}

fn sysctl(name: &str) -> Option<String> {
    let output = Command::new("/usr/sbin/sysctl").args(["-n", name]).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Fast: three `sysctl` calls and one `statvfs`.
pub fn system_checks(data: &Path, home: &Path, minimum: &str) -> Vec<Check> {
    let free = rustix::fs::statvfs(data).ok().map(|s| s.f_bavail.saturating_mul(s.f_frsize));
    vec![
        arch(std::env::consts::ARCH, sysctl("sysctl.proc_translated").as_deref()),
        macos(sysctl("kern.osproductversion").as_deref(), minimum),
        hypervisor(sysctl("kern.hv_support").as_deref()),
        disk(free),
        socket_fits(home),
    ]
}

/// A microsandbox home of an earlier zwinger build.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Legacy {
    /// `<app data>/microsandbox`. Only zwinger used it.
    AppData,
    /// `~/.microsandbox`. Other microsandbox tools can use it too.
    Shared,
}

impl Legacy {
    pub fn path(self, data: &Path, user_home: &Path) -> PathBuf {
        match self {
            Legacy::AppData => data.join("microsandbox"),
            Legacy::Shared => user_home.join(".microsandbox"),
        }
    }

    /// Remove deletes the whole home only when no other tool uses it.
    pub fn owned(self) -> bool {
        self == Legacy::AppData
    }
}

/// The legacy homes that exist.
pub fn legacy_homes(data: &Path, user_home: &Path) -> Vec<(Legacy, PathBuf)> {
    [Legacy::AppData, Legacy::Shared]
        .into_iter()
        .map(|legacy| (legacy, legacy.path(data, user_home)))
        .filter(|(_, path)| path.is_dir())
        .collect()
}

fn backend(home: &Path) -> Result<LocalBackend> {
    Ok(LocalBackend::builder().home(home).config_path(home.join("config.json")).build_lazy()?)
}

/// Stops and removes the zwinger VMs of `home`. Every call runs inside the
/// scope, so no task falls back to the default home.
async fn reap_in(home: &Path, limit: Duration) -> Result<Vec<String>> {
    let scoped = backend(home)?;
    with_backend(scoped, async {
        tokio::time::timeout(limit, runtime::reap_zwinger())
            .await
            .map_err(|_| Error::Io(std::io::Error::other("the reap timed out")))?
    })
    .await
}

/// Removes zwinger's VMs and image from a legacy home. Deletes the home only
/// if zwinger owns it.
pub async fn remove_legacy(legacy: Legacy, home: &Path, limit: Duration) -> Result<()> {
    reap_in(home, limit).await?;
    if legacy.owned() {
        return Ok(tokio::fs::remove_dir_all(home).await?);
    }
    let scoped = backend(home)?;
    match with_backend(scoped, Image::remove(runtime::IMAGE, false)).await {
        // Another tool may still use the image.
        Ok(()) | Err(MicrosandboxError::ImageNotFound(_) | MicrosandboxError::ImageInUse(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Whether `home` holds only one half of the runtime pair. `install_runtime`
/// refuses to install over such a pair, even with `force`.
fn partial_pair(home: &Path) -> bool {
    home.join("bin/msb").is_file() != library_files(home).iter().any(|p| p.is_file())
}

/// The `libkrunfw` files and links in `home`.
fn library_files(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home.join("lib")).map_or_else(
        |_| Vec::new(),
        |dir| {
            dir.flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("libkrunfw")))
                .collect()
        },
    )
}

/// Removes the runtime pair of `home`, so the embedded pair can be installed.
fn remove_pair(home: &Path) -> std::io::Result<()> {
    let msb = home.join("bin/msb");
    for path in std::iter::once(msb).chain(library_files(home)) {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// What the start does with the runtime that resolves.
#[derive(Debug, PartialEq)]
enum Action {
    Keep,
    /// Install the embedded pair: there is none, or it is damaged.
    Install,
    /// Stop the VMs of the old pair, then install the embedded pair.
    Replace,
    /// `MSB_PATH` names a pair that does not work. A developer set it, so it is not replaced.
    Developer,
}

/// `origin` is `None` when no pair resolves. `version` is `Err` when the
/// version of the pair cannot be read.
fn action(
    origin: Option<RuntimeOrigin>,
    version: std::result::Result<Option<&Version>, ()>,
    msb_path_set: bool,
    embedded: &str,
) -> Action {
    match (origin, version) {
        (Some(RuntimeOrigin::Environment), Ok(_)) => Action::Keep,
        (Some(RuntimeOrigin::Environment), Err(())) => Action::Developer,
        (None, _) if msb_path_set => Action::Developer,
        (None, _) => Action::Install,
        (Some(_), Err(())) => Action::Replace,
        (Some(RuntimeOrigin::Home), Ok(found)) if found.is_none_or(|v| v.to_string() != embedded) => Action::Replace,
        (Some(_), Ok(_)) => Action::Keep,
    }
}

/// Stops the zwinger VMs of the legacy homes, makes the app-owned
/// microsandbox home the default for every sandbox operation, then installs
/// the runtime that is inside the app binary if needed.
pub fn prepare_runtime<R: SandboxRuntime>(
    home: &Path,
    legacy: &[(Legacy, PathBuf)],
    runtime: &R,
    reap_limit: Duration,
    on_step: impl Fn(Step),
) -> Result<()> {
    on_step(Step::Resolve);
    let backend = backend(home)?;
    let config = backend.config().clone();
    let embedded = InstallOptions { source: InstallSource::EmbeddedArchive, ..Default::default() };
    let resolved = resolve_runtime(&config);
    let version = match &resolved {
        Ok(r) => resolve_runtime_version(&r.msb_path).map_err(|e| eprintln!("the runtime version cannot be read: {e}")),
        Err(e) => {
            eprintln!("the runtime does not resolve: {e}");
            Err(())
        }
    };
    let msb_path = std::env::var_os("MSB_PATH");
    let action = action(
        resolved.as_ref().ok().map(|r| r.origin),
        version.as_ref().map(Option::as_ref).map_err(|_| ()),
        msb_path.is_some(),
        &embedded.version,
    );
    if action == Action::Developer {
        let path = resolved.map_or_else(|_| PathBuf::from(msb_path.unwrap_or_default()), |r| r.msb_path);
        return Err(Error::DeveloperRuntime(path.display().to_string()));
    }
    tauri::async_runtime::block_on(async {
        on_step(Step::ReapLegacy);
        for (_, path) in legacy {
            // A legacy home with a broken runtime must not block the start.
            match Box::pin(reap_in(path, reap_limit)).await {
                Ok(reaped) if !reaped.is_empty() => eprintln!("stopped {} VMs in {}", reaped.len(), path.display()),
                Ok(_) => {}
                Err(e) => eprintln!("reap in {} failed: {e}", path.display()),
            }
        }
        set_default_backend(backend);
        if action == Action::Keep {
            return Ok(());
        }
        on_step(Step::Install);
        let partial = partial_pair(home);
        if action == Action::Replace || partial {
            // The VMs of the old runtime must not outlive its files.
            match tokio::time::timeout(reap_limit, Box::pin(runtime.reap())).await {
                Ok(Err(e)) => eprintln!("reap before the runtime update failed: {e}"),
                Err(_) => eprintln!("reap before the runtime update timed out"),
                Ok(Ok(_)) => {}
            }
        }
        if partial {
            remove_pair(home)?;
        }
        // Boxed: the microsandbox futures are deep, and inline they overflow
        // the compiler's layout query depth in release builds.
        Box::pin(install_runtime(&config, InstallOptions { force: true, ..embedded })).await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hypervisor_support_comes_from_sysctl() {
        assert_eq!(hypervisor(Some("1")).level, Level::Pass);
        assert_eq!(hypervisor(Some("0")).level, Level::Fail);
        assert_eq!(hypervisor(None).level, Level::Fail);
        assert_eq!(hypervisor(Some("")).level, Level::Fail);
    }

    #[test]
    fn macos_version_is_compared_by_number() {
        assert_eq!(macos(Some("14.0"), "14.0").level, Level::Pass);
        assert_eq!(macos(Some("15.1.1\n"), "14.0").level, Level::Pass);
        assert_eq!(macos(Some("14"), "14.0").level, Level::Pass);
        assert_eq!(macos(Some("26.0"), "14.0").level, Level::Pass);
        assert_eq!(macos(Some("13.6.9"), "14.0").level, Level::Fail);
        assert!(macos(Some("13.6.9"), "14.0").message.contains("This Mac has macOS 13.6.9"));
        assert_eq!(macos(Some("garbage"), "14.0").level, Level::Warn);
        assert_eq!(macos(None, "14.0").level, Level::Warn);
        assert_eq!(macos(Some("12.0"), "").level, Level::Pass);
    }

    #[test]
    fn little_disk_space_is_a_warning() {
        assert_eq!(disk(Some(MIN_FREE)).level, Level::Pass);
        let low = disk(Some(1_200_000_000));
        assert_eq!(low.level, Level::Warn);
        assert!(low.message.starts_with("Only 1.2 GB free."));
        assert_eq!(disk(None).level, Level::Warn);
    }

    #[test]
    fn a_damaged_runtime_is_repaired_and_a_developer_runtime_is_kept() {
        let v = Version::parse("0.7.8").unwrap();
        let old = Version::parse("0.7.5").unwrap();
        let home = Some(RuntimeOrigin::Home);
        assert_eq!(action(home, Ok(Some(&v)), false, "0.7.8"), Action::Keep);
        assert_eq!(action(home, Ok(Some(&old)), false, "0.7.8"), Action::Replace);
        assert_eq!(action(home, Ok(None), false, "0.7.8"), Action::Replace);
        assert_eq!(action(home, Err(()), false, "0.7.8"), Action::Replace);
        assert_eq!(action(Some(RuntimeOrigin::Installed), Ok(Some(&v)), false, "0.7.8"), Action::Keep);
        assert_eq!(action(Some(RuntimeOrigin::Installed), Err(()), false, "0.7.8"), Action::Replace);
        assert_eq!(action(None, Err(()), false, "0.7.8"), Action::Install);
        let env = Some(RuntimeOrigin::Environment);
        assert_eq!(action(env, Ok(Some(&old)), true, "0.7.8"), Action::Keep);
        assert_eq!(action(env, Err(()), true, "0.7.8"), Action::Developer);
        assert_eq!(action(None, Err(()), true, "0.7.8"), Action::Developer);
    }

    #[test]
    fn the_control_socket_must_be_shorter_than_104_bytes() {
        // `/run/sandboxes/` + 24 + `/control.sock` add 52 bytes to the home.
        let home = |len: usize| PathBuf::from(format!("/{}", "a".repeat(len - 1)));
        assert_eq!(control_socket_len(&home(51)), 103);
        assert_eq!(socket_fits(&home(51)).level, Level::Pass);
        assert_eq!(control_socket_len(&home(52)), 104);
        assert_eq!(socket_fits(&home(52)).level, Level::Fail);
        assert!(socket_fits(&home(52)).message.contains("(104 of at most 103 bytes)"));
        // A user name of 22 characters still fits the app's home.
        assert_eq!(socket_fits(Path::new("/Users/abcdefghijklmnopqrstuv/.zwinger/microsandbox")).level, Level::Pass);
        assert_eq!(socket_fits(Path::new("/Users/abcdefghijklmnopqrstuvw/.zwinger/microsandbox")).level, Level::Fail);
    }

    #[test]
    fn only_native_apple_silicon_passes() {
        assert_eq!(arch("aarch64", Some("0")).level, Level::Pass);
        assert_eq!(arch("aarch64", None).level, Level::Pass);
        assert_eq!(arch("x86_64", Some("1")).level, Level::Fail);
        assert!(arch("x86_64", Some("1")).message.contains("Rosetta"));
        assert_eq!(arch("x86_64", Some("0")).level, Level::Fail);
        assert_eq!(arch("x86_64", None).level, Level::Fail);
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_existing_legacy_homes_are_found() {
        let (data, user) = (temp(), temp());
        assert!(legacy_homes(&data, &user).is_empty());
        std::fs::create_dir(user.join(".microsandbox")).unwrap();
        assert_eq!(legacy_homes(&data, &user), [(Legacy::Shared, user.join(".microsandbox"))]);
        std::fs::create_dir(data.join("microsandbox")).unwrap();
        assert_eq!(legacy_homes(&data, &user).len(), 2);
        assert!(Legacy::AppData.owned());
        assert!(!Legacy::Shared.owned());
    }

    #[test]
    fn remove_deletes_only_the_home_that_zwinger_owns() {
        let (data, user) = (temp(), temp());
        let owned = Legacy::AppData.path(&data, &user);
        let shared = Legacy::Shared.path(&data, &user);
        for home in [&owned, &shared] {
            std::fs::create_dir(home).unwrap();
            std::fs::write(home.join("other"), "data of another tool").unwrap();
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(remove_legacy(Legacy::AppData, &owned, Duration::from_secs(10))).unwrap();
        rt.block_on(remove_legacy(Legacy::Shared, &shared, Duration::from_secs(10))).unwrap();
        assert!(!owned.exists());
        assert!(shared.join("other").is_file());
    }

    #[test]
    fn a_partial_runtime_is_removed_before_the_install() {
        let home = temp();
        let config = backend(&home).unwrap().config().clone();
        let embedded = InstallOptions { source: InstallSource::EmbeddedArchive, force: true, ..Default::default() };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(install_runtime(&config, embedded.clone())).unwrap();
        assert!(!partial_pair(&home));
        std::fs::remove_file(home.join("bin/msb")).unwrap();
        assert!(partial_pair(&home));
        assert!(matches!(
            rt.block_on(install_runtime(&config, embedded.clone())),
            Err(MicrosandboxError::RuntimeIncomplete(_))
        ));
        remove_pair(&home).unwrap();
        assert!(!partial_pair(&home));
        rt.block_on(install_runtime(&config, embedded)).unwrap();
        assert!(resolve_runtime(&config).is_ok());
        assert!(!partial_pair(&home));
    }

    #[test]
    fn the_checks_of_this_mac_run() {
        let checks = system_checks(&std::env::temp_dir(), &std::env::temp_dir(), "11.0");
        assert_eq!(
            checks.iter().map(|c| c.name).collect::<Vec<_>>(),
            ["Processor", "macOS", "Virtualization", "Disk space", "VM folder"]
        );
    }
}
