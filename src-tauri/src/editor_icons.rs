//! The app icons of the editors, as small PNG data URLs, cached in memory.

use std::collections::HashMap;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;

use crate::editor::{declared_icon, icon_source};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_PNG: usize = 256 * 1024;
const SIPS_LIMIT: Duration = Duration::from_secs(10);

/// The identity and content stamp of a file. An atomic replace that keeps the
/// mtime and the length still gets a new inode and change time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    changed: (i64, i64),
    modified: SystemTime,
    len: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            changed: (meta.ctime(), meta.ctime_nsec()),
            modified: meta.modified().ok()?,
            len: meta.len(),
        })
    }
}

/// What a cached icon came from. A change of any part converts the icon again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    plist: FileStamp,
    /// The declared icon file and, if it is there, the file that it leads to.
    /// `None`: the plist declares no usable icon.
    declared: Option<(PathBuf, Option<FileStamp>)>,
}

impl Stamp {
    /// True if the files are as they were. Only stat calls, no reads. A stat
    /// follows links, so a retargeted link or a file that appeared is a change.
    fn current(&self, bundle: &Path) -> bool {
        FileStamp::of(&info_plist(bundle)).as_ref() == Some(&self.plist)
            && self.declared.as_ref().is_none_or(|(path, stamp)| FileStamp::of(path) == *stamp)
    }

    /// Reads the plist again, and the icon to convert. `None` if the bundle has no `Info.plist`.
    fn read(bundle: &Path) -> Option<(Self, Option<PathBuf>)> {
        let plist = FileStamp::of(&info_plist(bundle))?;
        let declared = declared_icon(bundle).map(|path| {
            let stamp = FileStamp::of(&path);
            (path, stamp)
        });
        Some((Self { plist, declared }, icon_source(bundle)))
    }
}

fn info_plist(bundle: &Path) -> PathBuf {
    bundle.join("Contents/Info.plist")
}

type Entry = (Stamp, Option<String>);

/// The icon of each bundle. A failed conversion is kept as `None` until its stamp changes.
#[derive(Default)]
pub struct EditorIcons {
    cache: Mutex<HashMap<PathBuf, Entry>>,
    /// One lock for each bundle, so concurrent misses share one conversion.
    flights: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl EditorIcons {
    /// The icon of `bundle` as a `data:image/png;base64,` URL, else `None`.
    pub async fn icon(&self, bundle: &Path) -> Option<String> {
        self.icon_with(bundle, sips).await
    }

    async fn icon_with<F, Fut>(&self, bundle: &Path, convert: F) -> Option<String>
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: Future<Output = Option<Vec<u8>>>,
    {
        let flight = self.flights.lock().unwrap().entry(bundle.to_owned()).or_default().clone();
        let _flight = flight.lock().await;

        let cached = self.cache.lock().unwrap().get(bundle).map(|(stamp, _)| stamp.clone());
        let owned = bundle.to_owned();
        // `None`: a hit. `Some(None)`: the bundle has no `Info.plist`.
        let miss = tokio::task::spawn_blocking(move || match cached {
            Some(stamp) if stamp.current(&owned) => None,
            _ => Some(Stamp::read(&owned)),
        })
        .await
        .ok()?;
        let Some(miss) = miss else {
            return self.cache.lock().unwrap().get(bundle).and_then(|(_, icon)| icon.clone());
        };
        let Some((stamp, source)) = miss else {
            self.cache.lock().unwrap().remove(bundle);
            return None;
        };

        let icon = match source {
            Some(path) => convert(path).await.filter(|png| png.starts_with(PNG) && png.len() <= MAX_PNG),
            None => None,
        };
        let url =
            icon.map(|png| format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)));
        self.cache.lock().unwrap().insert(bundle.to_owned(), (stamp, url.clone()));
        url
    }
}

/// Deletes the temp file on each way out of [`sips`].
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Converts `icns` to a 64-pixel PNG with the system `sips`.
async fn sips(icns: PathBuf) -> Option<Vec<u8>> {
    let out = TempFile(std::env::temp_dir().join(format!("zwinger-icon-{}.png", uuid::Uuid::new_v4())));
    let run = tokio::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "png", "-Z", "64"])
        .arg(&icns)
        .arg("--out")
        .arg(&out.0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = run.ok()?;
    let status = match tokio::time::timeout(SIPS_LIMIT, child.wait()).await {
        Ok(status) => status.ok()?,
        Err(_) => {
            // Wait for sips to exit, so that it cannot write the file after `out` removes it.
            let _ = child.kill().await;
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    tokio::fs::read(&out.0).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bundle(icon: &str) -> (PathBuf, PathBuf) {
        let root = crate::test_dir::root().join(format!("zwinger-icons-{}", uuid::Uuid::new_v4()));
        let resources = root.join("Editor.app/Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIconFile".into(), icon.into());
        plist::Value::Dictionary(info).to_file_xml(root.join("Editor.app/Contents/Info.plist")).unwrap();
        (root.join("Editor.app"), resources.canonicalize().unwrap())
    }

    fn png(tag: &str) -> Vec<u8> {
        [PNG, tag.as_bytes()].concat()
    }

    /// Converts by reading the icon file as is, and counts the calls.
    async fn fake(icons: &EditorIcons, bundle: &Path, calls: &AtomicUsize) -> Option<String> {
        icons
            .icon_with(bundle, |path| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                std::fs::read(path).ok()
            })
            .await
    }

    fn url(png: &[u8]) -> Option<String> {
        Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
    }

    #[tokio::test]
    async fn a_hit_converts_nothing_and_a_changed_icon_converts_again() {
        let (bundle, resources) = bundle("Code.icns");
        std::fs::write(resources.join("Code.icns"), png("one")).unwrap();
        let (icons, calls) = (EditorIcons::default(), AtomicUsize::new(0));

        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("one")));
        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("one")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Replaced artwork has a new length.
        std::fs::write(resources.join("Code.icns"), png("second")).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("second")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // An atomic replace with the same length and mtime.
        let next = resources.join("next.icns");
        std::fs::write(&next, png("thirds")).unwrap();
        let old = std::fs::metadata(resources.join("Code.icns")).unwrap().modified().unwrap();
        std::fs::File::options().write(true).open(&next).unwrap().set_modified(old).unwrap();
        std::fs::rename(&next, resources.join("Code.icns")).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("thirds")));
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Removed artwork gives no icon and no conversion.
        std::fs::remove_file(resources.join("Code.icns")).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        std::fs::remove_dir_all(bundle.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_failed_conversion_is_kept_until_the_icon_changes() {
        let (bundle, resources) = bundle("Code");
        // Not a PNG, then too large.
        std::fs::write(resources.join("Code.icns"), b"icns").unwrap();
        let (icons, calls) = (EditorIcons::default(), AtomicUsize::new(0));
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        std::fs::write(resources.join("Code.icns"), [PNG, &vec![0; MAX_PNG]].concat()).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // A bundle that is gone.
        std::fs::remove_dir_all(bundle.parent().unwrap()).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_declared_icon_that_appears_or_a_retargeted_link_converts_again() {
        let (bundle, resources) = bundle("Art.icns");
        let (icons, calls) = (EditorIcons::default(), AtomicUsize::new(0));
        assert_eq!(fake(&icons, &bundle, &calls).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // An update adds the declared file and does not touch Info.plist.
        std::fs::write(resources.join("one.icns"), png("one")).unwrap();
        std::fs::write(resources.join("two.icns"), png("two")).unwrap();
        std::os::unix::fs::symlink("one.icns", resources.join("Art.icns")).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("one")));
        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("one")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The link now leads to a different file of the same length.
        std::fs::remove_file(resources.join("Art.icns")).unwrap();
        std::os::unix::fs::symlink("two.icns", resources.join("Art.icns")).unwrap();
        assert_eq!(fake(&icons, &bundle, &calls).await, url(&png("two")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        std::fs::remove_dir_all(bundle.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_conversion() {
        let (bundle, resources) = bundle("Code.icns");
        std::fs::write(resources.join("Code.icns"), png("one")).unwrap();
        let (icons, calls) = (EditorIcons::default(), AtomicUsize::new(0));
        let (a, b) = tokio::join!(fake(&icons, &bundle, &calls), fake(&icons, &bundle, &calls));
        assert_eq!((a.clone(), b), (url(&png("one")), url(&png("one"))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(bundle.parent().unwrap()).unwrap();
    }
}
