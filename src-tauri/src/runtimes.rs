//! The agent runtimes that zwinger downloads on first use: Claude Code and
//! Codex. They are pinned npm packages, fetched on the host from the
//! registry, checked against a pinned SHA-512 (and the registry's
//! `dist.integrity`, which must name the same hash) and unpacked into
//! `<app_data>/runtimes/<name>/<version>/`. A shield start mounts each
//! installed runtime read-only at `/opt/zwinger/<name>`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use flate2::read::GzDecoder;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha512};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::error::{Error, Result};

const REGISTRY: &str = "https://registry.npmjs.org";
const CLAUDE_VERSION: &str = "2.1.306";
const CODEX_VERSION: &str = "0.163.0";
pub const NAMES: [&str; 2] = ["claude", "codex"];
/// Progress is reported after each this many bytes.
const REPORT: u64 = 1 << 20;
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
/// The longest wait for the next bytes. A slow download goes on.
const READ_LIMIT: Duration = Duration::from_secs(60);
/// The prefix of the work dir of an install.
const TMP: &str = ".tmp-";

/// A file of the package that the runtime needs.
struct Wanted {
    /// The path in the tarball.
    from: String,
    /// The path in the runtime dir.
    to: &'static str,
    required: bool,
}

struct Package {
    name: &'static str,
    /// The version that the UI shows.
    version: &'static str,
    npm: String,
    npm_version: String,
    /// The SRI hash of the tarball of this version and arch.
    sha512: &'static str,
    files: Vec<Wanted>,
}

/// The npm arch and the Rust target of the guest. The VM has the arch of the host.
fn arch() -> Result<(&'static str, &'static str)> {
    match std::env::consts::ARCH {
        "aarch64" => Ok(("arm64", "aarch64-unknown-linux-musl")),
        "x86_64" => Ok(("x64", "x86_64-unknown-linux-musl")),
        other => Err(Error::UnsupportedArch(other.to_owned())),
    }
}

fn package(name: &str) -> Result<Package> {
    let (arch, target) = arch()?;
    match name {
        "claude" => Ok(Package {
            name: "claude",
            version: CLAUDE_VERSION,
            npm: format!("@anthropic-ai/claude-code-linux-{arch}"),
            npm_version: CLAUDE_VERSION.to_owned(),
            sha512: match arch {
                "arm64" => "sha512-AePKWuymgDSCh7nScJ4dx/pF1Kppzk4IwQuh6JCROiqQdBVGaaHw6jVf/fY5PhbSjy9EQML6yOiUfc50aFpnTA==",
                _ => "sha512-4gsb0IQhxzi1NhoUETXkNZb1t6wJ9tnfAYBKmb0+NmsAwOG37Dacq7bLqsooEkusaT0PyQRFUMt0SgWxx9LEEA==",
            },
            files: vec![Wanted {
                from: "package/claude".to_owned(),
                to: "claude",
                required: true,
            }],
        }),
        "codex" => Ok(Package {
            name: "codex",
            version: CODEX_VERSION,
            npm: "@openai/codex".to_owned(),
            npm_version: format!("{CODEX_VERSION}-linux-{arch}"),
            sha512: match arch {
                "arm64" => "sha512-IH2/R+4OarjGvsAf1ll3fdyLwrPgYLl3uzp6u9CyD9sj2Lw+ViDSe3QBZVHdcHJs0eIiZFg6Wprcuy4CRZaUGw==",
                _ => "sha512-uwRaOsrYogLQhcxAM/GoeWrw0QIWEMRo7c9gM8sKAviVkZQHlu9kRLJ5PkufbV+ABGjyZgIwiFdJ0LCJ0N6e8w==",
            },
            files: vec![
                Wanted {
                    from: format!("package/vendor/{target}/bin/codex"),
                    to: "bin/codex",
                    required: true,
                },
                // Codex finds files with it; it falls back without.
                Wanted {
                    from: format!("package/vendor/{target}/codex-path/rg"),
                    to: "codex-path/rg",
                    required: false,
                },
            ],
        }),
        _ => Err(Error::UnknownRuntime(name.to_owned())),
    }
}

fn runtime_dir(data: &Path, package: &Package) -> PathBuf {
    data.join("runtimes").join(package.name).join(&package.npm_version)
}

fn is_installed(dir: &Path, package: &Package) -> bool {
    package.files.iter().filter(|f| f.required).all(|f| dir.join(f.to).is_file())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeView {
    pub name: &'static str,
    pub version: &'static str,
    pub installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "camelCase")]
pub enum Progress {
    Download { received: u64, total: Option<u64> },
    Extract,
}

/// An installed runtime, for a shield start.
#[derive(Debug, Clone)]
pub struct Installed {
    pub name: &'static str,
    pub dir: PathBuf,
}

/// The installed runtimes of the pinned versions.
pub fn installed(data: &Path) -> Vec<Installed> {
    NAMES
        .iter()
        .filter_map(|name| package(name).ok())
        .filter_map(|package| {
            let dir = runtime_dir(data, &package);
            is_installed(&dir, &package).then_some(Installed { name: package.name, dir })
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
        Self {
            data,
            installing: Mutex::new(HashMap::new()),
        }
    }

    pub fn list(&self) -> Result<Vec<RuntimeView>> {
        NAMES
            .iter()
            .map(|name| {
                let package = package(name)?;
                Ok(RuntimeView {
                    name: package.name,
                    version: package.version,
                    installed: is_installed(&runtime_dir(&self.data, &package), &package),
                })
            })
            .collect()
    }

    /// Downloads, checks and unpacks the runtime. An installed runtime is
    /// not an error. The work is in a temp dir that is renamed into place,
    /// so a failed install leaves nothing that a start would mount.
    pub async fn install(&self, name: &str, progress: impl Fn(Progress) + Send + Sync) -> Result<()> {
        let package = package(name)?;
        let dest = runtime_dir(&self.data, &package);
        if is_installed(&dest, &package) {
            return Ok(());
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
        let _claim = Claim {
            installing: &self.installing,
            name: package.name,
        };
        let parent = dest.parent().expect("runtime dir has a parent").to_owned();
        std::fs::create_dir_all(&parent)?;
        let tmp = parent.join(format!("{TMP}{}", uuid::Uuid::new_v4()));
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
    let digest = tokio::select! {
        // A cancel wins over a download that ends at the same time.
        biased;
        () = cancel.notified() => return Err(Error::InstallCancelled(package.name.to_owned())),
        digest = download(package, &tgz, progress) => digest?,
    };

    progress(Progress::Extract);
    let root = tmp.join("root");
    let name = package.name;
    let files: Vec<Wanted> = package
        .files
        .iter()
        .map(|f| Wanted {
            from: f.from.clone(),
            to: f.to,
            required: f.required,
        })
        .collect();
    let (unpacked, sha512) = (root.clone(), package.sha512);
    tokio::task::spawn_blocking(move || {
        check_integrity(name, sha512, &digest)?;
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

/// Downloads the tarball to `tgz` and returns its SHA-512 digest. The
/// registry must name the pinned hash before the download starts.
async fn download(package: &Package, tgz: &Path, progress: &(impl Fn(Progress) + Send + Sync)) -> Result<Vec<u8>> {
    let bad = |why: &str| Error::BadPackage(package.name.to_owned(), why.to_owned());
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(CONNECT_LIMIT)
        .read_timeout(READ_LIMIT)
        .build()?;
    let url = format!("{REGISTRY}/{}/{}", package.npm, package.npm_version);
    let meta: Value = client.get(&url).send().await?.error_for_status()?.json().await?;
    let tarball = meta["dist"]["tarball"].as_str().ok_or_else(|| bad("no dist.tarball"))?;
    if !tarball.starts_with(&format!("{REGISTRY}/")) {
        return Err(bad("the tarball is not on the registry"));
    }
    let integrity = meta["dist"]["integrity"].as_str().ok_or_else(|| bad("no dist.integrity"))?;
    check_pinned(package, integrity)?;

    // About 290 MB: it streams to disk and is hashed on the way.
    let mut response = client.get(tarball).send().await?.error_for_status()?;
    let total = response.content_length();
    let mut file = tokio::fs::File::create(tgz).await?;
    let mut hasher = Sha512::new();
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
    Ok(hasher.finalize().to_vec())
}

/// The registry's `dist.integrity` must name the pinned hash, so a changed
/// package fails before its download.
fn check_pinned(package: &Package, integrity: &str) -> Result<()> {
    if integrity.split_whitespace().any(|h| h == package.sha512) {
        Ok(())
    } else {
        Err(Error::IntegrityMismatch(package.name.to_owned()))
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
    if expected.contains(&actual.as_str()) {
        Ok(())
    } else {
        Err(Error::IntegrityMismatch(name.to_owned()))
    }
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

/// Puts the required files of the pinned version in place, as an install does.
#[cfg(test)]
pub fn fake_install(data: &Path, name: &str) {
    let package = package(name).unwrap();
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
        let dir = std::env::temp_dir().join(format!("zwinger-runtimes-{}", uuid::Uuid::new_v4()));
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

    fn files() -> Vec<Wanted> {
        vec![
            Wanted {
                from: "package/bin/tool".into(),
                to: "bin/tool",
                required: true,
            },
            Wanted {
                from: "package/rg".into(),
                to: "rg",
                required: false,
            },
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
        let claude = package("claude").unwrap();
        let hash = base64::engine::general_purpose::STANDARD
            .decode(claude.sha512.strip_prefix("sha512-").unwrap())
            .unwrap();
        assert_eq!(hash.len(), 64);
        check_pinned(&claude, &format!("sha1-abc {}", claude.sha512)).unwrap();
        let other = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(b"x")));
        assert!(matches!(check_pinned(&claude, &other), Err(Error::IntegrityMismatch(_))));
        check_integrity("claude", claude.sha512, &hash).unwrap();
        let download = Sha512::digest(b"changed tarball").to_vec();
        assert!(matches!(check_integrity("claude", claude.sha512, &download), Err(Error::IntegrityMismatch(_))));
        assert_ne!(package("codex").unwrap().sha512, claude.sha512);
    }

    #[tokio::test]
    async fn cancel_ends_the_install_and_drops_the_claim() {
        let data = temp_dir();
        let runtimes = Arc::new(Runtimes::new(data.clone()));
        let install = tokio::spawn({
            let runtimes = runtimes.clone();
            async move { runtimes.install("codex", |_| {}).await }
        });
        while !runtimes.installing.lock().unwrap().contains_key("codex") {
            tokio::task::yield_now().await;
        }
        assert!(matches!(runtimes.install("codex", |_| {}).await, Err(Error::RuntimeInstalling(_))));
        runtimes.cancel("codex");
        assert!(matches!(install.await.unwrap(), Err(Error::InstallCancelled(_))));
        assert!(runtimes.installing.lock().unwrap().is_empty());
        let parent = data.join("runtimes/codex");
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 0);
        // No install: nothing to cancel.
        runtimes.cancel("claude");
    }

    #[test]
    fn new_removes_stale_work_dirs() {
        let data = temp_dir();
        let stale = data.join("runtimes/claude/.tmp-1234/root");
        let kept = data.join("runtimes/codex/0.163.0-linux-arm64/bin");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&kept).unwrap();
        Runtimes::new(data.clone());
        assert!(!data.join("runtimes/claude/.tmp-1234").exists());
        assert!(kept.exists());
    }

    #[test]
    fn packages_are_pinned_per_arch() {
        let (arch, target) = arch().unwrap();
        let claude = package("claude").unwrap();
        assert_eq!(claude.npm, format!("@anthropic-ai/claude-code-linux-{arch}"));
        assert_eq!(claude.npm_version, "2.1.306");
        let codex = package("codex").unwrap();
        assert_eq!((codex.npm.as_str(), codex.npm_version.clone()), ("@openai/codex", format!("0.163.0-linux-{arch}")));
        assert_eq!(codex.files[0].from, format!("package/vendor/{target}/bin/codex"));
        assert!(matches!(package("pi"), Err(Error::UnknownRuntime(_))));
    }

    #[test]
    fn installed_lists_complete_runtimes_only() {
        let data = temp_dir();
        assert!(installed(&data).is_empty());
        let codex = package("codex").unwrap();
        let dir = runtime_dir(&data, &codex);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        assert!(installed(&data).is_empty());
        std::fs::write(dir.join("bin/codex"), "x").unwrap();
        let found = installed(&data);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].name, &found[0].dir), ("codex", &dir));
        let list = Runtimes::new(data).list().unwrap();
        assert_eq!(list.iter().map(|r| (r.name, r.installed)).collect::<Vec<_>>(), [("claude", false), ("codex", true)]);
    }

    /// Downloads the real pinned Codex from the npm registry. Needs network:
    /// `cargo test -- --ignored live_install`.
    #[tokio::test]
    #[ignore]
    async fn live_install_codex() {
        let data = std::env::temp_dir().join(format!("zwinger-live-{}", uuid::Uuid::new_v4()));
        let runtimes = Runtimes::new(data.clone());
        runtimes.install("codex", |_| {}).await.unwrap();
        let list = runtimes.list().unwrap();
        assert!(list.iter().any(|r| r.name == "codex" && r.installed));
        let _ = std::fs::remove_dir_all(data);
    }
}
