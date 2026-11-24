//! The agent runtimes that zwinger downloads on first use: Claude Code,
//! Codex and Antigravity CLI. Claude Code and Codex are npm packages, fetched
//! on the host from the registry and checked against a pinned SHA-512 (and
//! the registry's `dist.integrity`, which must name the same hash).
//! Antigravity CLI is an asset of its GitHub release, checked against a
//! pinned SHA-256. Each one is unpacked into
//! `<app_data>/runtimes/<name>/<version>/`. A VM start mounts the whole
//! `runtimes` folder read-only at [`GUEST_DIR`], so a runtime that is
//! installed later is in each running VM at once. The folder holds only the
//! unpacked runtimes and the confirmed versions: an install works in
//! [`WORK`], outside it, and renames the complete dir into place.
//!
//! A host agent asks for the Linux build of the host's version. A version
//! with no pinned hash can be installed only after the user confirms it
//! once; then the registry's `dist.integrity` is the only check.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use flate2::read::GzDecoder;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::error::{Error, Result};

const REGISTRY: &str = "https://registry.npmjs.org";
const CLAUDE_VERSION: &str = "2.1.306";
const CODEX_VERSION: &str = "0.163.0";
const AGY_VERSION: &str = "1.5.15";
/// The release assets of Antigravity CLI.
const AGY_RELEASES: &str = "https://github.com/google-antigravity/antigravity-cli/releases/download";
pub const NAMES: [&str; 3] = ["claude", "codex", "agy"];
/// The SRI hash of each pinned version per npm arch.
const PINS: [(&str, &str, &str, &str); 4] = [
    (
        "claude",
        CLAUDE_VERSION,
        "arm64",
        "sha512-AePKWuymgDSCh7nScJ4dx/pF1Kppzk4IwQuh6JCROiqQdBVGaaHw6jVf/fY5PhbSjy9EQML6yOiUfc50aFpnTA==",
    ),
    (
        "claude",
        CLAUDE_VERSION,
        "x64",
        "sha512-4gsb0IQhxzi1NhoUETXkNZb1t6wJ9tnfAYBKmb0+NmsAwOG37Dacq7bLqsooEkusaT0PyQRFUMt0SgWxx9LEEA==",
    ),
    (
        "codex",
        CODEX_VERSION,
        "arm64",
        "sha512-IH2/R+4OarjGvsAf1ll3fdyLwrPgYLl3uzp6u9CyD9sj2Lw+ViDSe3QBZVHdcHJs0eIiZFg6Wprcuy4CRZaUGw==",
    ),
    (
        "codex",
        CODEX_VERSION,
        "x64",
        "sha512-uwRaOsrYogLQhcxAM/GoeWrw0QIWEMRo7c9gM8sKAviVkZQHlu9kRLJ5PkufbV+ABGjyZgIwiFdJ0LCJ0N6e8w==",
    ),
];
/// The SHA-256 (hex) of each Antigravity CLI asset per arch: the static musl
/// build, so it runs on any glibc or musl image.
const AGY_PINS: [(&str, &str); 2] = [
    ("arm64", "455d20cc37507058bd9287d47d51aa7e6110351eb717fbf17e19619a7dc9d32e"),
    ("x64", "a12514fb33e8835186082a00b60725f6fb15941ba061f0403347b86c4dbd8f46"),
];
/// The unpinned versions that the user confirmed, as `<name>@<version>`.
const CONFIRMED: &str = "confirmed.json";
/// Progress is reported after each this many bytes.
const REPORT: u64 = 1 << 20;
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
/// The longest wait for the next bytes. A slow download goes on.
const READ_LIMIT: Duration = Duration::from_secs(60);
/// The prefix of the work dir of an install in the runtimes folder of
/// earlier builds.
const TMP: &str = ".tmp-";
/// The folder of the work dirs of the installs, beside the runtimes folder:
/// a VM never sees an unchecked download.
const WORK: &str = "runtime-work";
/// The guest dir of the runtimes folder, bound read-only.
pub const GUEST_DIR: &str = "/opt/zwinger/runtimes";

/// A file of the package that the runtime needs.
struct Wanted {
    /// The path in the tarball.
    from: String,
    /// The path in the runtime dir.
    to: &'static str,
    required: bool,
}

/// Where the archive of a runtime comes from.
enum Origin {
    Npm {
        npm: String,
        npm_version: String,
        /// The pinned SRI hash of the tarball of this version and arch.
        /// `None` for a version that zwinger does not pin.
        sha512: Option<&'static str>,
    },
    /// A release asset with a pinned SHA-256 (hex). Only the pinned version.
    Release { url: String, sha256: &'static str },
}

struct Package {
    name: &'static str,
    /// The version that the UI shows.
    version: String,
    /// The name of the runtime dir.
    folder: String,
    origin: Origin,
    files: Vec<Wanted>,
}

impl Package {
    /// zwinger pins the hash of this archive.
    fn is_pinned(&self) -> bool {
        match self.origin {
            Origin::Npm { sha512, .. } => sha512.is_some(),
            Origin::Release { .. } => true,
        }
    }
}

/// The npm arch and the Rust target of the guest. The VM has the arch of the host.
fn arch() -> Result<(&'static str, &'static str)> {
    match std::env::consts::ARCH {
        "aarch64" => Ok(("arm64", "aarch64-unknown-linux-musl")),
        "x86_64" => Ok(("x64", "x86_64-unknown-linux-musl")),
        other => Err(Error::UnsupportedArch(other.to_owned())),
    }
}

fn pinned_version(name: &str) -> Result<&'static str> {
    match name {
        "claude" => Ok(CLAUDE_VERSION),
        "codex" => Ok(CODEX_VERSION),
        "agy" => Ok(AGY_VERSION),
        _ => Err(Error::UnknownRuntime(name.to_owned())),
    }
}

/// zwinger pins a hash for this version of the runtime.
pub fn is_pinned(name: &str, version: &str) -> bool {
    PINS.iter().any(|(n, v, ..)| *n == name && *v == version)
}

/// A version is a plain `x.y.z` (maybe with a pre-release), so it is safe
/// in a registry URL and a folder name.
fn check_version(name: &str, version: &str) -> Result<()> {
    let valid = !version.is_empty()
        && version.len() <= 64
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && version.starts_with(|c: char| c.is_ascii_digit());
    if valid { Ok(()) } else { Err(Error::BadPackage(name.to_owned(), format!("invalid version {version:?}"))) }
}

fn package(name: &str, version: &str) -> Result<Package> {
    let (arch, target) = arch()?;
    pinned_version(name)?;
    check_version(name, version)?;
    let sha512 = PINS.iter().find(|(n, v, a, _)| *n == name && *v == version && *a == arch).map(|p| p.3);
    match name {
        "claude" => Ok(Package {
            name: "claude",
            version: version.to_owned(),
            folder: version.to_owned(),
            origin: Origin::Npm {
                npm: format!("@anthropic-ai/claude-code-linux-{arch}"),
                npm_version: version.to_owned(),
                sha512,
            },
            files: vec![Wanted { from: "package/claude".to_owned(), to: "claude", required: true }],
        }),
        "agy" => {
            let sha256 = AGY_PINS.iter().find(|(a, _)| *a == arch).map(|p| p.1);
            let Some(sha256) = sha256.filter(|_| version == AGY_VERSION) else {
                return Err(Error::BadPackage(name.to_owned(), format!("no pinned hash for {version}")));
            };
            Ok(Package {
                name: "agy",
                version: version.to_owned(),
                folder: version.to_owned(),
                origin: Origin::Release {
                    url: format!("{AGY_RELEASES}/{version}/agy_cli_linux_{arch}_musl.tar.gz"),
                    sha256,
                },
                // The official installer also installs `antigravity` as `agy`.
                files: vec![Wanted { from: "antigravity".to_owned(), to: "agy", required: true }],
            })
        }
        _ => Ok(Package {
            name: "codex",
            version: version.to_owned(),
            folder: format!("{version}-linux-{arch}"),
            origin: Origin::Npm {
                npm: "@openai/codex".to_owned(),
                npm_version: format!("{version}-linux-{arch}"),
                sha512,
            },
            files: vec![
                Wanted { from: format!("package/vendor/{target}/bin/codex"), to: "bin/codex", required: true },
                // Codex finds files with it; it falls back without.
                Wanted { from: format!("package/vendor/{target}/codex-path/rg"), to: "codex-path/rg", required: false },
            ],
        }),
    }
}

/// The host folder that a VM mounts at [`GUEST_DIR`].
pub fn root(data: &Path) -> PathBuf {
    data.join("runtimes")
}

/// The name of the runtime dir of `version`, as `package` makes it. It needs
/// no pin, so it also names a version that is no longer pinned.
fn folder(name: &str, version: &str) -> Result<String> {
    let (arch, _) = arch()?;
    pinned_version(name)?;
    check_version(name, version)?;
    Ok(if name == "codex" { format!("{version}-linux-{arch}") } else { version.to_owned() })
}

/// The version of the runtime dir `folder`, if it is one for this arch.
fn version_of(name: &str, folder: &str) -> Option<String> {
    let version = match name {
        "codex" => folder.strip_suffix(&format!("-linux-{}", arch().ok()?.0))?,
        _ => folder,
    };
    check_version(name, version).ok()?;
    Some(version.to_owned())
}

/// The dirs of the runtimes in use, as `<name>/<folder>` below [`root`].
pub type InUse = BTreeSet<String>;

/// The `<name>/<folder>` key of a guest runtime dir below [`GUEST_DIR`].
pub fn in_use_key(guest: &str) -> Option<String> {
    guest.strip_prefix(GUEST_DIR)?.strip_prefix('/').map(str::to_owned)
}

/// The `<name>/<folder>` key of `version` of the runtime `name`.
pub fn version_key(name: &str, version: &str) -> Option<String> {
    Some(format!("{name}/{}", folder(name, version).ok()?))
}

/// `a` is an older version than `b`: the numbers of `x.y.z` compare first,
/// then a version with a pre-release is older than one without.
fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| {
        let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p.to_owned())));
        let numbers: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
        (numbers, pre.is_none(), pre)
    };
    parts(a) < parts(b)
}

fn runtime_dir(data: &Path, package: &Package) -> PathBuf {
    root(data).join(package.name).join(&package.folder)
}

fn is_installed(dir: &Path, package: &Package) -> bool {
    package.files.iter().filter(|f| f.required).all(|f| dir.join(f.to).is_file())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeView {
    pub name: &'static str,
    /// The pinned version.
    pub version: String,
    pub installed: bool,
    /// The pinned version is not installed, and an older version is: an
    /// install of the pinned version is an update.
    pub update: bool,
    /// The installed versions that are not the pinned one, newest first.
    pub others: Vec<OtherVersion>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherVersion {
    pub version: String,
    /// A running VM's wrappers or an agent on this Mac use it, so it cannot be deleted.
    pub in_use: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "camelCase")]
pub enum Progress {
    Download { received: u64, total: Option<u64> },
    Extract,
}

/// An installed runtime, for a cage start.
#[derive(Debug, Clone)]
pub struct Installed {
    pub name: &'static str,
    pub version: String,
    /// The runtime dir in the guest, below [`GUEST_DIR`].
    pub guest: String,
}

/// The Linux build of this version is installed.
pub fn is_cached(data: &Path, name: &str, version: &str) -> bool {
    package(name, version).is_ok_and(|package| is_installed(&runtime_dir(data, &package), &package))
}

/// The installed runtime of each name: the version in `wanted` (the host
/// version of a host agent) if it is installed, else the pinned version.
pub fn installed(data: &Path, wanted: &HashMap<&str, String>) -> Vec<Installed> {
    NAMES
        .iter()
        .filter_map(|name| {
            let pinned = pinned_version(name).ok()?.to_owned();
            let versions = wanted.get(name).into_iter().cloned().chain([pinned]);
            versions.filter_map(|v| package(name, &v).ok()).find_map(|package| {
                let dir = runtime_dir(data, &package);
                let guest = format!("{GUEST_DIR}/{}/{}", package.name, package.folder);
                is_installed(&dir, &package).then_some(Installed {
                    name: package.name,
                    version: package.version,
                    guest,
                })
            })
        })
        .collect()
}

pub struct Runtimes {
    data: PathBuf,
    /// The installs that run, with the signal that cancels each one.
    installing: Mutex<HashMap<&'static str, Arc<Notify>>>,
}

/// Marks a runtime as being installed until it is dropped.
struct Claim<'a> {
    installing: &'a Mutex<HashMap<&'static str, Arc<Notify>>>,
    name: &'static str,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.installing.lock().unwrap_or_else(|e| e.into_inner()).remove(self.name);
    }
}

impl Runtimes {
    /// Removes the work dirs of installs that an earlier app run left.
    pub fn new(data: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(data.join(WORK));
        for name in NAMES {
            let Ok(entries) = std::fs::read_dir(data.join("runtimes").join(name)) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(TMP) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        Self { data, installing: Mutex::new(HashMap::new()) }
    }

    pub fn list(&self, in_use: &InUse) -> Result<Vec<RuntimeView>> {
        NAMES
            .iter()
            .map(|name| {
                let package = package(name, pinned_version(name)?)?;
                let installed = is_installed(&runtime_dir(&self.data, &package), &package);
                let mut others: Vec<OtherVersion> = self
                    .versions(name)
                    .into_iter()
                    .filter(|v| *v != package.version)
                    .map(|version| OtherVersion {
                        in_use: version_key(name, &version).is_some_and(|key| in_use.contains(&key)),
                        version,
                    })
                    .collect();
                others.sort_by(|a, b| {
                    if older(&a.version, &b.version) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less }
                });
                let update = !installed && others.iter().any(|o| older(&o.version, &package.version));
                Ok(RuntimeView { name: package.name, version: package.version, installed, update, others })
            })
            .collect()
    }

    /// The complete installed versions of `name`, for this arch.
    fn versions(&self, name: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(root(&self.data).join(name)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| version_of(name, entry.file_name().to_str()?))
            .filter(|version| self.is_complete(name, version))
            .collect()
    }

    /// The binary of the version is there. Unlike [`is_cached`], this needs no
    /// pin, so it also finds an Antigravity CLI version that is no longer pinned.
    fn is_complete(&self, name: &str, version: &str) -> bool {
        let binary = if name == "codex" { "bin/codex" } else { name };
        folder(name, version).is_ok_and(|folder| root(&self.data).join(name).join(folder).join(binary).is_file())
    }

    /// Deletes an installed version that is not the pinned one and that
    /// nothing in `in_use` uses. The dir moves out of the mounted folder at
    /// once, then it is removed.
    pub async fn remove(&self, name: &str, version: &str, in_use: &InUse) -> Result<()> {
        let folder = folder(name, version)?;
        let name = NAMES.iter().copied().find(|n| *n == name).ok_or_else(|| Error::UnknownRuntime(name.to_owned()))?;
        if version == pinned_version(name)? {
            return Err(Error::RuntimePinned(name.to_owned(), version.to_owned()));
        }
        if in_use.contains(&format!("{name}/{folder}")) {
            return Err(Error::RuntimeInUse(name.to_owned(), version.to_owned()));
        }
        let dir = root(&self.data).join(name).join(&folder);
        if std::fs::symlink_metadata(&dir).is_err() {
            return Ok(());
        }
        let work = self.data.join(WORK);
        std::fs::create_dir_all(&work)?;
        let gone = work.join(uuid::Uuid::new_v4().to_string());
        std::fs::rename(&dir, &gone)?;
        tokio::fs::remove_dir_all(&gone).await?;
        Ok(())
    }

    fn confirmed(&self) -> BTreeSet<String> {
        std::fs::read(self.data.join("runtimes").join(CONFIRMED))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Keeps the user's confirm of an unpinned version, so it is asked once.
    fn confirm(&self, package: &Package) -> Result<()> {
        let mut confirmed = self.confirmed();
        if confirmed.insert(format!("{}@{}", package.name, package.version)) {
            let dir = self.data.join("runtimes");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(CONFIRMED), serde_json::to_vec_pretty(&confirmed)?)?;
        }
        Ok(())
    }

    /// Downloads, checks and unpacks the runtime: `version`, else the pinned
    /// version. A version with no pin needs `confirm` once, which is kept.
    /// An installed runtime is not an error. The work is in a temp dir that
    /// is renamed into place, so a failed install leaves nothing that a start
    /// would mount.
    pub async fn install(
        &self,
        name: &str,
        version: Option<&str>,
        confirm: bool,
        progress: impl Fn(Progress) + Send + Sync,
    ) -> Result<()> {
        let package = package(name, version.unwrap_or(pinned_version(name)?))?;
        let dest = runtime_dir(&self.data, &package);
        if is_installed(&dest, &package) {
            return Ok(());
        }
        if !package.is_pinned() {
            if confirm {
                self.confirm(&package)?;
            } else if !self.confirmed().contains(&format!("{}@{}", package.name, package.version)) {
                return Err(Error::UnpinnedVersion(package.name.to_owned(), package.version));
            }
        }
        let cancel = Arc::new(Notify::new());
        {
            let mut installing = self.installing.lock().unwrap_or_else(|e| e.into_inner());
            if installing.contains_key(package.name) {
                return Err(Error::RuntimeInstalling(package.name.to_owned()));
            }
            installing.insert(package.name, cancel.clone());
        }
        // Held until the work dir is removed, so a new install waits for that.
        let _claim = Claim { installing: &self.installing, name: package.name };
        std::fs::create_dir_all(dest.parent().expect("runtime dir has a parent"))?;
        let work = self.data.join(WORK);
        std::fs::create_dir_all(&work)?;
        let tmp = work.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&tmp)?;
        let result = fetch(&package, &tmp, &dest, &progress, &cancel).await;
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        result
    }

    /// Stops the download of the runtime. No install is not an error.
    pub fn cancel(&self, name: &str) {
        if let Some(cancel) = self.installing.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
            // The permit stays if the install does not wait yet.
            cancel.notify_one();
        }
    }
}

/// Only the download can be cancelled: the unpack is short, and its
/// blocking task must end before the work dir is removed.
async fn fetch(
    package: &Package,
    tmp: &Path,
    dest: &Path,
    progress: &(impl Fn(Progress) + Send + Sync),
    cancel: &Notify,
) -> Result<()> {
    let tgz = tmp.join("package.tgz");
    let (digest, expected) = tokio::select! {
        // A cancel wins over a download that ends at the same time.
        biased;
        () = cancel.notified() => return Err(Error::InstallCancelled(package.name.to_owned())),
        digest = download(package, &tgz, progress) => digest?,
    };

    progress(Progress::Extract);
    let root = tmp.join("root");
    let name = package.name;
    let files: Vec<Wanted> =
        package.files.iter().map(|f| Wanted { from: f.from.clone(), to: f.to, required: f.required }).collect();
    let unpacked = root.clone();
    tokio::task::spawn_blocking(move || {
        expected.check(name, &digest)?;
        extract(name, &tgz, &files, &unpacked)
    })
    .await
    .map_err(|e| Error::Io(std::io::Error::other(e)))??;
    if let Err(e) = std::fs::rename(&root, dest) {
        // Another install may have won the rename.
        if !is_installed(dest, package) {
            return Err(e.into());
        }
    }
    Ok(())
}

/// The hash that a download must match.
enum Expected {
    /// An SRI value: `sha512-<base64>`, maybe with more hashes.
    Sri(String),
    /// A SHA-256 in hex.
    Sha256(&'static str),
}

impl Expected {
    fn hasher(&self) -> Hasher {
        match self {
            Self::Sri(_) => Hasher::Sha512(Sha512::new()),
            Self::Sha256(_) => Hasher::Sha256(Sha256::new()),
        }
    }

    fn check(&self, name: &str, digest: &[u8]) -> Result<()> {
        match self {
            Self::Sri(integrity) => check_integrity(name, integrity, digest),
            Self::Sha256(hex) => {
                let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                if actual == *hex { Ok(()) } else { Err(Error::IntegrityMismatch(name.to_owned())) }
            }
        }
    }
}

enum Hasher {
    Sha512(Sha512),
    Sha256(Sha256),
}

impl Hasher {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha512(h) => h.update(bytes),
            Self::Sha256(h) => h.update(bytes),
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Self::Sha512(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
        }
    }
}

/// The archive URL and the hash that it must match. The registry must name
/// the pinned hash before the download starts. An unpinned npm version must
/// match the registry's integrity.
async fn source(client: &reqwest::Client, package: &Package) -> Result<(String, Expected)> {
    let bad = |why: &str| Error::BadPackage(package.name.to_owned(), why.to_owned());
    let (npm, npm_version, sha512) = match &package.origin {
        Origin::Release { url, sha256 } => return Ok((url.clone(), Expected::Sha256(sha256))),
        Origin::Npm { npm, npm_version, sha512 } => (npm, npm_version, sha512),
    };
    let url = format!("{REGISTRY}/{npm}/{npm_version}");
    let meta: Value = client.get(&url).send().await?.error_for_status()?.json().await?;
    let tarball = meta["dist"]["tarball"].as_str().ok_or_else(|| bad("no dist.tarball"))?;
    if !tarball.starts_with(&format!("{REGISTRY}/")) {
        return Err(bad("the tarball is not on the registry"));
    }
    let integrity = meta["dist"]["integrity"].as_str().ok_or_else(|| bad("no dist.integrity"))?;
    let expected = match sha512 {
        Some(pinned) => {
            check_pinned(pinned, package.name, integrity)?;
            (*pinned).to_owned()
        }
        None => integrity.to_owned(),
    };
    Ok((tarball.to_owned(), Expected::Sri(expected)))
}

/// Downloads the archive to `tgz` and returns its digest and the hash that
/// the download must match.
async fn download(
    package: &Package,
    tgz: &Path,
    progress: &(impl Fn(Progress) + Send + Sync),
) -> Result<(Vec<u8>, Expected)> {
    let client =
        reqwest::Client::builder().https_only(true).connect_timeout(CONNECT_LIMIT).read_timeout(READ_LIMIT).build()?;
    let (url, expected) = source(&client, package).await?;

    // About 100 to 290 MB: it streams to disk and is hashed on the way.
    let mut response = client.get(url).send().await?.error_for_status()?;
    let total = response.content_length();
    let mut file = tokio::fs::File::create(tgz).await?;
    let mut hasher = expected.hasher();
    let (mut received, mut reported) = (0, 0);
    progress(Progress::Download { received, total });
    while let Some(chunk) = response.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        if received - reported >= REPORT {
            reported = received;
            progress(Progress::Download { received, total });
        }
    }
    file.flush().await?;
    drop(file);
    progress(Progress::Download { received, total });
    Ok((hasher.finalize(), expected))
}

/// The registry's `dist.integrity` must name the pinned hash, so a changed
/// package fails before its download.
fn check_pinned(pinned: &str, name: &str, integrity: &str) -> Result<()> {
    if integrity.split_whitespace().any(|h| h == pinned) {
        Ok(())
    } else {
        Err(Error::IntegrityMismatch(name.to_owned()))
    }
}

/// Checks a SHA-512 digest against an SRI value (`sha512-<base64>`, maybe
/// with more hashes, separated by spaces).
fn check_integrity(name: &str, integrity: &str, digest: &[u8]) -> Result<()> {
    let expected: Vec<&str> = integrity.split_whitespace().filter_map(|h| h.strip_prefix("sha512-")).collect();
    if expected.is_empty() {
        return Err(Error::BadPackage(name.to_owned(), format!("no sha512 integrity: {integrity}")));
    }
    let actual = base64::engine::general_purpose::STANDARD.encode(digest);
    if expected.contains(&actual.as_str()) { Ok(()) } else { Err(Error::IntegrityMismatch(name.to_owned())) }
}

/// Writes only the wanted files of the tarball into `root`, each one to the
/// path that the package names, never to a path from the archive. An entry
/// with an absolute or `..` path fails the install, and so does a wanted
/// path that is not a regular file. Other entries are skipped.
fn extract(name: &str, tgz: &Path, files: &[Wanted], root: &Path) -> Result<()> {
    let bad = |why: String| Error::BadPackage(name.to_owned(), why);
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(tgz)?));
    let mut found = vec![false; files.len()];
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let mut path = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::Normal(part) => path.push(part),
                Component::CurDir => {}
                _ => return Err(bad(format!("unsafe path {}", raw.display()))),
            }
        }
        let Some(i) = files.iter().position(|f| Path::new(&f.from) == path) else {
            continue;
        };
        if !entry.header().entry_type().is_file() || found[i] {
            return Err(bad(format!("{} is not one regular file", raw.display())));
        }
        let to = root.join(files[i].to);
        std::fs::create_dir_all(to.parent().expect("file has a parent"))?;
        let mut out = OpenOptions::new().write(true).create_new(true).mode(0o755).open(&to)?;
        std::io::copy(&mut entry, &mut out)?;
        found[i] = true;
    }
    match files.iter().zip(&found).find(|(f, found)| f.required && !**found) {
        Some((missing, _)) => Err(bad(format!("{} is missing", missing.from))),
        None => Ok(()),
    }
}

/// A runtime of each name at version 1, for the guest paths of a test.
#[cfg(test)]
pub fn fake_installed() -> &'static [Installed] {
    static RUNTIMES: std::sync::LazyLock<Vec<Installed>> = std::sync::LazyLock::new(|| {
        NAMES.map(|name| Installed { name, version: "1".into(), guest: format!("{GUEST_DIR}/{name}/1") }).to_vec()
    });
    &RUNTIMES
}

/// Puts the required files of the pinned version in place, as an install does.
#[cfg(test)]
pub fn fake_install(data: &Path, name: &str) {
    fake_install_version(data, name, pinned_version(name).unwrap());
}

#[cfg(test)]
pub fn fake_install_version(data: &Path, name: &str, version: &str) {
    let package = package(name, version).unwrap();
    let dir = runtime_dir(data, &package);
    for file in package.files.iter().filter(|f| f.required) {
        let to = dir.join(file.to);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(to, "bin").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = crate::test_dir::root().join(format!("zwinger-runtimes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes the header name as is, so a test can make an unsafe path.
    fn tgz(dir: &Path, entries: &[(&str, tar::EntryType, &[u8])]) -> PathBuf {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (path, kind, data) in entries {
            let mut header = tar::Header::new_old();
            header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        let path = dir.join("package.tgz");
        std::fs::write(&path, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        path
    }

    /// The npm name, version and pin of a package.
    fn npm(package: &Package) -> (&str, &str, Option<&'static str>) {
        match &package.origin {
            Origin::Npm { npm, npm_version, sha512 } => (npm, npm_version, *sha512),
            Origin::Release { .. } => panic!("not an npm package"),
        }
    }

    fn files() -> Vec<Wanted> {
        vec![
            Wanted { from: "package/bin/tool".into(), to: "bin/tool", required: true },
            Wanted { from: "package/rg".into(), to: "rg", required: false },
        ]
    }

    #[test]
    fn extract_writes_only_the_wanted_files() {
        let dir = temp_dir();
        let tgz = tgz(
            &dir,
            &[
                ("package/package.json", tar::EntryType::Regular, b"{}"),
                ("./package/bin/tool", tar::EntryType::Regular, b"#!bin"),
                ("package/README.md", tar::EntryType::Regular, b"read me"),
            ],
        );
        let root = dir.join("root");
        extract("tool", &tgz, &files(), &root).unwrap();
        assert_eq!(std::fs::read(root.join("bin/tool")).unwrap(), b"#!bin");
        let mode = std::fs::metadata(root.join("bin/tool")).unwrap().permissions();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o755);
        let names: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["bin"]);
    }

    #[test]
    fn extract_rejects_unsafe_and_unexpected_entries() {
        let cases: [&[(&str, tar::EntryType, &[u8])]; 5] = [
            &[("../evil", tar::EntryType::Regular, b"x"), ("package/bin/tool", tar::EntryType::Regular, b"t")],
            &[("/etc/evil", tar::EntryType::Regular, b"x")],
            &[("package/bin/tool", tar::EntryType::Symlink, b"")],
            &[("package/bin/tool", tar::EntryType::Regular, b"a"), ("package/bin/tool", tar::EntryType::Regular, b"b")],
            &[("package/rg", tar::EntryType::Regular, b"rg")],
        ];
        for entries in cases {
            let dir = temp_dir();
            let root = dir.join("root");
            let result = extract("tool", &tgz(&dir, entries), &files(), &root);
            assert!(matches!(result, Err(Error::BadPackage(..))), "{:?}", entries[0].0);
            assert!(!dir.join("evil").exists() && !Path::new("/etc/evil").exists());
        }
    }

    #[test]
    fn integrity_must_match_the_sha512() {
        let digest = Sha512::digest(b"tarball").to_vec();
        let good = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(&digest));
        check_integrity("tool", &good, &digest).unwrap();
        check_integrity("tool", &format!("sha1-abc {good}"), &digest).unwrap();
        let other = Sha512::digest(b"changed").to_vec();
        assert!(matches!(check_integrity("tool", &good, &other), Err(Error::IntegrityMismatch(_))));
        assert!(matches!(check_integrity("tool", "sha1-abc", &digest), Err(Error::BadPackage(..))));
    }

    #[test]
    fn the_pinned_hash_must_match_the_registry_and_the_download() {
        let pinned = npm(&package("claude", CLAUDE_VERSION).unwrap()).2.unwrap();
        let hash = base64::engine::general_purpose::STANDARD.decode(pinned.strip_prefix("sha512-").unwrap()).unwrap();
        assert_eq!(hash.len(), 64);
        check_pinned(pinned, "claude", &format!("sha1-abc {pinned}")).unwrap();
        let other = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(b"x")));
        assert!(matches!(check_pinned(pinned, "claude", &other), Err(Error::IntegrityMismatch(_))));
        check_integrity("claude", pinned, &hash).unwrap();
        let download = Sha512::digest(b"changed tarball").to_vec();
        assert!(matches!(check_integrity("claude", pinned, &download), Err(Error::IntegrityMismatch(_))));
        assert_ne!(npm(&package("codex", CODEX_VERSION).unwrap()).2, Some(pinned));
    }

    #[tokio::test]
    async fn cancel_ends_the_install_and_drops_the_claim() {
        let data = temp_dir();
        let runtimes = Arc::new(Runtimes::new(data.clone()));
        let install = tokio::spawn({
            let runtimes = runtimes.clone();
            async move { runtimes.install("codex", None, false, |_| {}).await }
        });
        while !runtimes.installing.lock().unwrap().contains_key("codex") {
            tokio::task::yield_now().await;
        }
        assert!(matches!(runtimes.install("codex", None, false, |_| {}).await, Err(Error::RuntimeInstalling(_))));
        runtimes.cancel("codex");
        assert!(matches!(install.await.unwrap(), Err(Error::InstallCancelled(_))));
        assert!(runtimes.installing.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(data.join("runtimes/codex")).unwrap().count(), 0);
        // The work dir is outside the folder that a VM mounts, and it is gone.
        assert_eq!(std::fs::read_dir(data.join("runtime-work")).unwrap().count(), 0);
        // No install: nothing to cancel.
        runtimes.cancel("claude");
    }

    #[test]
    fn new_removes_stale_work_dirs() {
        let data = temp_dir();
        let stale = data.join("runtimes/claude/.tmp-1234/root");
        let work = data.join("runtime-work/1234/root");
        let kept = data.join("runtimes/codex/0.163.0-linux-arm64/bin");
        for dir in [&stale, &work, &kept] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Runtimes::new(data.clone());
        assert!(!data.join("runtimes/claude/.tmp-1234").exists());
        assert!(!data.join("runtime-work").exists());
        assert!(kept.exists());
    }

    #[test]
    fn packages_are_pinned_per_arch() {
        let (arch, target) = arch().unwrap();
        let claude = package("claude", "2.1.306").unwrap();
        assert_eq!(npm(&claude).0, format!("@anthropic-ai/claude-code-linux-{arch}"));
        assert_eq!(npm(&claude).1, "2.1.306");
        let codex = package("codex", "0.163.0").unwrap();
        assert_eq!((npm(&codex).0, npm(&codex).1.to_owned()), ("@openai/codex", format!("0.163.0-linux-{arch}")));
        assert_eq!(codex.files[0].from, format!("package/vendor/{target}/bin/codex"));
        assert!(matches!(package("pi", "1.0.0"), Err(Error::UnknownRuntime(_))));
        // Another version has no pin, and a version must be safe in a URL and a folder name.
        let newer = package("codex", "0.170.1").unwrap();
        assert_eq!((npm(&newer).2, newer.folder.clone()), (None, format!("0.170.1-linux-{arch}")));
        assert!(is_pinned("claude", "2.1.306") && !is_pinned("claude", "2.1.310"));
        for bad in ["", "../1", "1.0/x", "latest", "1.0 0"] {
            assert!(matches!(package("claude", bad), Err(Error::BadPackage(..))), "{bad:?}");
        }
    }

    #[test]
    fn antigravity_is_the_pinned_musl_release_asset() {
        let (arch, _) = arch().unwrap();
        let agy = package("agy", AGY_VERSION).unwrap();
        let Origin::Release { url, sha256 } = &agy.origin else { panic!("not a release") };
        assert_eq!(
            url,
            &format!(
                "https://github.com/google-antigravity/antigravity-cli/releases/download/{AGY_VERSION}/agy_cli_linux_{arch}_musl.tar.gz"
            )
        );
        assert_eq!(sha256.len(), 64);
        assert!(agy.is_pinned());
        assert_eq!((agy.files[0].from.as_str(), agy.files[0].to), ("antigravity", "agy"));
        // Only the pinned version: a release asset has no second check.
        assert!(matches!(package("agy", "1.5.16"), Err(Error::BadPackage(..))));
    }

    #[test]
    fn a_sha256_must_match_its_hex() {
        let digest = Sha256::digest(b"archive").to_vec();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let hex: &'static str = Box::leak(hex.into_boxed_str());
        Expected::Sha256(hex).check("agy", &digest).unwrap();
        let other = Sha256::digest(b"changed").to_vec();
        assert!(matches!(Expected::Sha256(hex).check("agy", &other), Err(Error::IntegrityMismatch(_))));
        let mut hasher = Expected::Sha256(hex).hasher();
        hasher.update(b"archive");
        assert_eq!(hasher.finalize(), digest);
    }

    #[tokio::test]
    async fn an_unpinned_version_needs_one_confirm() {
        let data = temp_dir();
        let runtimes = Runtimes::new(data.clone());
        let result = runtimes.install("claude", Some("2.1.310"), false, |_| {}).await;
        assert!(matches!(result, Err(Error::UnpinnedVersion(n, v)) if n == "claude" && v == "2.1.310"));
        let package = package("claude", "2.1.310").unwrap();
        runtimes.confirm(&package).unwrap();
        assert!(Runtimes::new(data).confirmed().contains("claude@2.1.310"));
        // An installed version needs no confirm.
        let other = temp_dir();
        fake_install_version(&other, "claude", "2.1.311");
        Runtimes::new(other).install("claude", Some("2.1.311"), false, |_| {}).await.unwrap();
    }

    #[test]
    fn installed_lists_complete_runtimes_only() {
        let data = temp_dir();
        let pinned = HashMap::new();
        assert!(installed(&data, &pinned).is_empty());
        let codex = package("codex", CODEX_VERSION).unwrap();
        let dir = runtime_dir(&data, &codex);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        assert!(installed(&data, &pinned).is_empty());
        std::fs::write(dir.join("bin/codex"), "x").unwrap();
        let found = installed(&data, &pinned);
        assert_eq!(found.len(), 1);
        let guest = format!("{GUEST_DIR}/codex/{}", codex.folder);
        assert_eq!((found[0].name, &found[0].guest), ("codex", &guest));
        let list = Runtimes::new(data.clone()).list(&InUse::new()).unwrap();
        assert_eq!(
            list.iter().map(|r| (r.name, r.installed)).collect::<Vec<_>>(),
            [("claude", false), ("codex", true), ("agy", false)]
        );

        // The host version wins when it is cached; else the pinned version is the fallback.
        let host = HashMap::from([("codex", "0.170.1".to_owned())]);
        assert_eq!(installed(&data, &host)[0].version, CODEX_VERSION);
        fake_install_version(&data, "codex", "0.170.1");
        assert!(is_cached(&data, "codex", "0.170.1"));
        let found = installed(&data, &host);
        let folder = package("codex", "0.170.1").unwrap().folder;
        assert_eq!(
            (found[0].version.as_str(), found[0].guest.clone()),
            ("0.170.1", format!("{GUEST_DIR}/codex/{folder}"))
        );
    }

    #[tokio::test]
    async fn an_older_version_shows_an_update_and_can_be_deleted_when_unused() {
        let data = temp_dir();
        let runtimes = Runtimes::new(data.clone());
        for version in ["2.1.100", "2.1.99", "9.0.0"] {
            fake_install_version(&data, "claude", version);
        }
        // A dir with no binary and a dir that is no version are no runtimes.
        std::fs::create_dir_all(root(&data).join("claude/2.1.50")).unwrap();
        std::fs::create_dir_all(root(&data).join("claude/notes")).unwrap();
        let in_use = InUse::from([version_key("claude", "2.1.99").unwrap()]);
        let claude = |list: Vec<RuntimeView>| list.into_iter().find(|r| r.name == "claude").unwrap();
        let view = claude(runtimes.list(&in_use).unwrap());
        assert!(!view.installed && view.update);
        let others: Vec<_> = view.others.iter().map(|o| (o.version.as_str(), o.in_use)).collect();
        assert_eq!(others, [("9.0.0", false), ("2.1.100", false), ("2.1.99", true)]);

        assert!(matches!(runtimes.remove("claude", "2.1.99", &in_use).await, Err(Error::RuntimeInUse(..))));
        assert!(matches!(runtimes.remove("claude", CLAUDE_VERSION, &in_use).await, Err(Error::RuntimePinned(..))));
        assert!(runtimes.remove("claude", "../x", &in_use).await.is_err());
        runtimes.remove("claude", "2.1.100", &in_use).await.unwrap();
        runtimes.remove("claude", "9.0.0", &in_use).await.unwrap();
        assert!(!root(&data).join("claude/2.1.100").exists());
        // Only a newer pinned version is an update.
        let view = claude(runtimes.list(&in_use).unwrap());
        assert!(view.update);
        runtimes.remove("claude", "2.1.99", &InUse::new()).await.unwrap();
        fake_install_version(&data, "claude", "9.0.0");
        assert!(!claude(runtimes.list(&InUse::new()).unwrap()).update);
        // The pinned version stays installed with its files.
        fake_install(&data, "claude");
        let view = claude(runtimes.list(&InUse::new()).unwrap());
        assert!(view.installed && !view.update);
        assert_eq!(view.others.len(), 1);
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(older("2.1.99", "2.1.100"));
        assert!(older("0.163.0-alpha.1", "0.163.0"));
        assert!(!older("2.1.306", "2.1.306"));
        assert!(!older("10.0.0", "9.9.9"));
        assert_eq!(
            in_use_key(&format!("{GUEST_DIR}/codex/1.0.0-linux-arm64")).as_deref(),
            Some("codex/1.0.0-linux-arm64")
        );
        assert_eq!(in_use_key("/opt/zwinger/pi"), None);
    }

    /// Downloads the real pinned Codex from the npm registry. Needs network:
    /// `cargo test -- --ignored live_install`.
    #[tokio::test]
    #[ignore]
    async fn live_install_codex() {
        let data = crate::test_dir::root().join(format!("zwinger-live-{}", uuid::Uuid::new_v4()));
        let runtimes = Runtimes::new(data.clone());
        runtimes.install("codex", None, false, |_| {}).await.unwrap();
        let list = runtimes.list(&InUse::new()).unwrap();
        assert!(list.iter().any(|r| r.name == "codex" && r.installed));
        let _ = std::fs::remove_dir_all(data);
    }
}
