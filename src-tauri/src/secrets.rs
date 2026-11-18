use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::cage;
use crate::error::{Error, Result};
use crate::host_agents::{self, login};
use crate::network::Source;

/// Names that the image, the shell or microsandbox set in the guest. A secret
/// with one of these names would replace them.
const RESERVED: &[&str] = &[
    "PATH",
    "HOME",
    "TERM",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "OLDPWD",
    "HOSTNAME",
    "LANG",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "NODE_EXTRA_CA_CERTS",
    "NODE_VERSION",
    "YARN_VERSION",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "NODE_USE_ENV_PROXY",
    "NODE_OPTIONS",
];

/// The config of a secret. The value is in the vault, never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    pub name: String,
    pub hosts: Vec<String>,
    pub places: Places,
    #[serde(default, skip_serializing_if = "Origin::is_user")]
    pub origin: Origin,
}

/// Where the value of a secret comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Origin {
    /// The vault: the user sets the value.
    #[default]
    User,
    /// The agent's login on the Mac, read at each cage start. The vault
    /// never has its value, and the user cannot edit it.
    HostLogin { agent: AgentKind },
}

impl Origin {
    pub fn is_user(&self) -> bool {
        *self == Self::User
    }
}

/// The request locations where the proxy replaces the placeholder with the
/// value. Only the named header fields get the value, so that a host that
/// echoes other request headers cannot reflect it back to the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredPlaces")]
pub struct Places {
    pub header_fields: Vec<String>,
    pub query: bool,
    pub body: bool,
}

/// The header fields of a config that had only `headers: bool`.
const DEFAULT_HEADER_FIELDS: [&str; 2] = ["authorization", "x-api-key"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPlaces {
    header_fields: Option<Vec<String>>,
    headers: Option<bool>,
    query: bool,
    body: bool,
}

impl From<StoredPlaces> for Places {
    fn from(stored: StoredPlaces) -> Self {
        let header_fields = match (stored.header_fields, stored.headers) {
            (Some(fields), _) => fields,
            (None, Some(false)) => Vec::new(),
            (None, _) => DEFAULT_HEADER_FIELDS.map(String::from).to_vec(),
        };
        Self { header_fields, query: stored.query, body: stored.body }
    }
}

/// An HTTP field name (RFC 9110 token).
pub fn is_token(field: &str) -> bool {
    !field.is_empty() && field.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// The trimmed name of a secret or credential: a variable name that the
/// project does not set.
pub fn check_name(name: &str) -> Result<String> {
    let name = name.trim();
    let invalid = |reason: &str| Error::InvalidSecret(name.to_owned(), reason.to_owned());
    let mut chars = name.chars();
    let valid_name = chars.next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !valid_name {
        return Err(invalid("use A-Z, 0-9 and _, and do not start with a digit"));
    }
    if RESERVED.contains(&name) || name.starts_with("MSB_") {
        return Err(invalid("the cage sets this variable"));
    }
    Ok(name.to_owned())
}

/// Lowercases and dedupes the header fields, and needs one place.
pub fn normalize_places(name: &str, mut places: Places) -> Result<Places> {
    let invalid = |reason: &str| Error::InvalidSecret(name.to_owned(), reason.to_owned());
    let mut fields: Vec<String> = Vec::new();
    for field in &places.header_fields {
        let field = field.trim().to_ascii_lowercase();
        if !is_token(&field) {
            return Err(invalid(&format!("{field:?} is not a valid header field name")));
        }
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    places.header_fields = fields;
    if places.header_fields.is_empty() && !places.query && !places.body {
        return Err(invalid("select at least one place"));
    }
    Ok(places)
}

/// The text that the guest sees in the variable instead of the value. The
/// closing `$` keeps one placeholder from being a prefix of another, such
/// as `KEY` of `KEY_2`.
pub fn placeholder(name: &str) -> String {
    format!("$MSB_{name}$")
}

impl Secret {
    /// Trims the name and normalizes the hosts, then checks the rules.
    pub fn normalize(mut self) -> Result<Self> {
        self.name = check_name(&self.name)?;
        let name = &self.name;
        let invalid = |reason: &str| Error::InvalidSecret(name.to_owned(), reason.to_owned());
        let mut hosts: Vec<String> = Vec::new();
        for host in &self.hosts {
            let host = cage::domain(host)?;
            if host.starts_with("*.") {
                return Err(invalid("use exact hosts, not wildcards"));
            }
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        if hosts.is_empty() {
            return Err(invalid("add at least one host"));
        }
        // The refresh hosts never get a host token, so the agent cannot renew
        // the login or swap it for a new one.
        if let Origin::HostLogin { .. } = self.origin {
            if let Some(host) = hosts.iter().find(|h| login::is_refresh_host(h)) {
                return Err(invalid(&format!("a host login may not reach the refresh host {host}")));
            }
            if self.places.query || self.places.body {
                return Err(invalid("a host login goes into header fields only"));
            }
        }
        self.hosts = hosts;
        self.places = normalize_places(&self.name, self.places)?;
        Ok(self)
    }

    pub fn placeholder(&self) -> String {
        placeholder(&self.name)
    }
}

pub fn check_unique(secrets: &[Secret]) -> Result<()> {
    for (i, secret) in secrets.iter().enumerate() {
        if secrets[..i].iter().any(|s| s.name == secret.name) {
            return Err(Error::DuplicateSecret(secret.name.clone()));
        }
    }
    Ok(())
}

/// The value of a secret. It cannot be serialized, and `Debug` does not show it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// The owner of a vault value: one cage, or the credential library. The
/// library is a separate map, so no cage id can name it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Owner<'a> {
    Cage(Cow<'a, str>),
    Library,
}

impl<'a> Owner<'a> {
    pub fn cage(id: &'a str) -> Self {
        Self::Cage(Cow::Borrowed(id))
    }

    pub fn into_owned(self) -> Owner<'static> {
        match self {
            Self::Cage(id) => Owner::Cage(Cow::Owned(id.into_owned())),
            Self::Library => Owner::Library,
        }
    }
}

/// A stored value: its owner and its name.
pub type Account = (Owner<'static>, String);

/// The value of a credential: its source and its name. Each value lookup
/// uses it, so one name of two sources never gets the other value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialRef {
    pub source: Source,
    pub name: String,
}

impl CredentialRef {
    pub fn library(name: &str) -> Self {
        Self { source: Source::Library, name: name.to_owned() }
    }

    /// The vault owner of the value when the cage `cage_id` uses it.
    pub fn vault_key<'a>(&self, cage_id: &'a str) -> Owner<'a> {
        match self.source {
            Source::Local => Owner::cage(cage_id),
            Source::Library => Owner::Library,
        }
    }

    pub fn account(&self, cage_id: &str) -> Account {
        (self.vault_key(cage_id).into_owned(), self.name.clone())
    }
}

/// Stores secret values per project and in the library.
pub trait Vault: Send + Sync {
    fn put(&self, owner: &Owner, name: &str, entry: &VaultEntry) -> Result<()>;
    fn entry(&self, owner: &Owner, name: &str) -> Result<Option<VaultEntry>>;
    fn set(&self, owner: &Owner, name: &str, value: &SecretValue) -> Result<()> {
        self.put(owner, name, &VaultEntry { value: value.clone(), saved_at: Some(now_ms()) })
    }
    #[cfg(test)]
    fn get(&self, owner: &Owner, name: &str) -> Result<Option<SecretValue>> {
        Ok(self.entry(owner, name)?.map(|entry| entry.value))
    }
    /// Deleting an absent value is not an error.
    fn delete(&self, owner: &Owner, name: &str) -> Result<()>;
    /// The [`Account`]s of all stored values. Returns no values.
    fn accounts(&self) -> Result<Vec<Account>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultEntry {
    pub value: SecretValue,
    pub saved_at: Option<i64>,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// The folder of the vault file, in the app data folder.
const VAULT_DIR: &str = "vault";
const VAULT_FILE: &str = "credentials.json";
/// The temp files of a write. A file with this prefix is from a write that
/// did not end, and the next write removes it.
const TEMP_PREFIX: &str = ".credentials.json.tmp-";
/// Version 3 adds the `library` map. An older build refuses the file, so it
/// cannot save the file without the library values.
const FORMAT: u32 = 3;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredValue {
    Legacy(String),
    Dated { value: String, saved_at: i64 },
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    /// The cage values by `<cage id>/<name>`. An entry of version 1 or 2
    /// stays a cage value, also when its cage id starts with `@`.
    values: BTreeMap<String, StoredValue>,
    /// The library values by name.
    #[serde(default)]
    library: BTreeMap<String, StoredValue>,
}

impl Stored {
    fn map(&mut self, owner: &Owner) -> (&mut BTreeMap<String, StoredValue>, String) {
        match owner {
            Owner::Cage(id) => (&mut self.values, format!("{id}/")),
            Owner::Library => (&mut self.library, String::new()),
        }
    }

    fn remove(&mut self, owner: &Owner, name: &str) -> Option<StoredValue> {
        let (map, prefix) = self.map(owner);
        map.remove(&format!("{prefix}{name}"))
    }
}

/// The values in one JSON file that only the user can read and write
/// (mode 0600, in a folder with mode 0700, both owned by the user). Any
/// process of the user can read the file. Each write replaces the file
/// atomically. Each step after the folder check is relative to the checked
/// folder, so a changed path cannot redirect it.
pub struct FileVault {
    dir: PathBuf,
    file: PathBuf,
    /// The uid that must own the folder and the file: the effective uid.
    owner: u32,
    /// Held for each read and each read-modify-write.
    lock: Mutex<()>,
    /// Makes the next write fail after the temp file is written.
    #[cfg(test)]
    fail_rename: AtomicBool,
}

impl FileVault {
    /// Creates `<data>/vault` with mode 0700 if it does not exist.
    pub fn new(data: &Path) -> Result<Self> {
        // SAFETY: geteuid has no preconditions and cannot fail.
        Self::open(data, unsafe { libc::geteuid() })
    }

    fn open(data: &Path, owner: u32) -> Result<Self> {
        let dir = data.join(VAULT_DIR);
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(vault_io(&dir, e)),
            _ => {}
        }
        let vault = Self {
            file: dir.join(VAULT_FILE),
            dir,
            owner,
            lock: Mutex::default(),
            #[cfg(test)]
            fail_rename: AtomicBool::new(false),
        };
        // A crash can leave a temp file with all values, also deleted ones.
        if let Some(dir) = vault.open_dir()? {
            vault.remove_temps(&dir)?;
        }
        Ok(vault)
    }

    /// The checked folder, or `None` if it does not exist.
    fn open_dir(&self) -> Result<Option<OwnedFd>> {
        let fd = match rustix::fs::open(&self.dir, host_agents::dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => {
                let link = fs::symlink_metadata(&self.dir).is_ok_and(|m| m.file_type().is_symlink());
                return Err(if link { link_error(&self.dir) } else { not_a(&self.dir, "folder") });
            }
            Err(e) => return Err(vault_io(&self.dir, e.into())),
        };
        let stat = rustix::fs::fstat(&fd).map_err(|e| vault_io(&self.dir, e.into()))?;
        self.check(&self.dir, &stat, FileType::Directory, 0o700)?;
        Ok(Some(fd))
    }

    /// The type, owner and mode of one object of the vault.
    fn check(&self, path: &Path, stat: &Stat, kind: FileType, mode: u32) -> Result<()> {
        if FileType::from_raw_mode(stat.st_mode) != kind {
            return Err(not_a(path, if kind == FileType::Directory { "folder" } else { "file" }));
        }
        if stat.st_uid != self.owner {
            return Err(Error::VaultOwner { path: path.display().to_string() });
        }
        if u32::from(stat.st_mode) & 0o077 != 0 {
            return Err(mode_error(path, mode));
        }
        Ok(())
    }

    /// The checked folder and its values. A missing folder or file is an empty vault.
    fn load(&self) -> Result<(Option<OwnedFd>, Stored)> {
        let Some(dir) = self.open_dir()? else {
            return Ok((None, Stored::default()));
        };
        // O_NOFOLLOW and the checks on the open handle: the file that is
        // checked is the file that is read. O_NONBLOCK: a FIFO cannot hang the open.
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let fd = match rustix::fs::openat(&dir, VAULT_FILE, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok((Some(dir), Stored::default())),
            Err(Errno::LOOP) => return Err(link_error(&self.file)),
            Err(e) => return Err(vault_io(&self.file, e.into())),
        };
        let stat = rustix::fs::fstat(&fd).map_err(|e| vault_io(&self.file, e.into()))?;
        self.check(&self.file, &stat, FileType::RegularFile, 0o600)?;
        let mut text = Vec::new();
        io::Read::read_to_end(&mut fs::File::from(fd), &mut text).map_err(|e| vault_io(&self.file, e))?;
        // The serde_json message can quote a value, so only its place is shown.
        let stored: Stored = serde_json::from_slice(&text).map_err(|e| {
            Error::Vault(format!("{} is not valid at line {}, column {}", self.file.display(), e.line(), e.column()))
        })?;
        if !(1..=FORMAT).contains(&stored.version) {
            return Err(Error::Vault(format!("{} has unknown version {}", self.file.display(), stored.version)));
        }
        Ok((Some(dir), stored))
    }

    fn save(&self, dir: Option<OwnedFd>, mut stored: Stored) -> Result<()> {
        let dir = dir.ok_or_else(|| vault_io(&self.dir, io::ErrorKind::NotFound.into()))?;
        self.remove_temps(&dir)?;
        // The rename replaces only a valid file.
        match rustix::fs::statat(&dir, VAULT_FILE, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                return Err(link_error(&self.file));
            }
            Ok(stat) => self.check(&self.file, &stat, FileType::RegularFile, 0o600)?,
            Err(Errno::NOENT) => {}
            Err(e) => return Err(vault_io(&self.file, e.into())),
        }
        let temp = format!("{TEMP_PREFIX}{}", std::process::id());
        stored.version = FORMAT;
        let json = serde_json::to_vec_pretty(&stored)?;
        // EXCL: a failed create leaves the object of another writer, so only
        // a temp file that this write created is removed.
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let file = rustix::fs::openat(&dir, temp.as_str(), flags, Mode::from_raw_mode(0o600))
            .map_err(|e| vault_io(&self.file, e.into()))?;
        let written = write_temp(file, &json).and_then(|()| {
            #[cfg(test)]
            if self.fail_rename.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("test failure"));
            }
            Ok(rustix::fs::renameat(&dir, temp.as_str(), &dir, VAULT_FILE)?)
        });
        if let Err(e) = written {
            let _ = rustix::fs::unlinkat(&dir, temp.as_str(), AtFlags::empty());
            return Err(vault_io(&self.file, e));
        }
        // The rename is durable only when the folder is synced.
        rustix::fs::fsync(&dir).map_err(|e| vault_io(&self.dir, e.into()))
    }

    /// Removes the temp files of writes that did not end. Another object
    /// with the temp prefix, such as a link or a file of another user, stays.
    fn remove_temps(&self, dir: &OwnedFd) -> Result<()> {
        let entries = Dir::read_from(dir).map_err(|e| vault_io(&self.dir, e.into()))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_bytes().starts_with(TEMP_PREFIX.as_bytes()) {
                continue;
            }
            let ours = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
                FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile && stat.st_uid == self.owner
            });
            if ours {
                let _ = rustix::fs::unlinkat(dir, name, AtFlags::empty());
            }
        }
        Ok(())
    }

    fn locked(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn write_temp(file: OwnedFd, json: &[u8]) -> io::Result<()> {
    let mut file = fs::File::from(file);
    file.write_all(json)?;
    file.sync_all()
}

fn mode_error(path: &Path, expected: u32) -> Error {
    Error::VaultMode { path: path.display().to_string(), expected: format!("{expected:o}") }
}

fn link_error(path: &Path) -> Error {
    Error::VaultLink { path: path.display().to_string() }
}

fn not_a(path: &Path, kind: &'static str) -> Error {
    Error::VaultType { path: path.display().to_string(), kind }
}

fn vault_io(path: &Path, e: io::Error) -> Error {
    Error::Vault(format!("{}: {e}", path.display()))
}

impl Vault for FileVault {
    fn put(&self, owner: &Owner, name: &str, entry: &VaultEntry) -> Result<()> {
        let _lock = self.locked();
        let (dir, mut stored) = self.load()?;
        let value = match entry.saved_at {
            Some(saved_at) => StoredValue::Dated { value: entry.value.expose().to_owned(), saved_at },
            None => StoredValue::Legacy(entry.value.expose().to_owned()),
        };
        let (map, prefix) = stored.map(owner);
        map.insert(format!("{prefix}{name}"), value);
        self.save(dir, stored)
    }

    fn entry(&self, owner: &Owner, name: &str) -> Result<Option<VaultEntry>> {
        let _lock = self.locked();
        Ok(self.load()?.1.remove(owner, name).map(|stored| match stored {
            StoredValue::Legacy(value) => VaultEntry { value: SecretValue::new(value), saved_at: None },
            StoredValue::Dated { value, saved_at } => {
                VaultEntry { value: SecretValue::new(value), saved_at: Some(saved_at) }
            }
        }))
    }

    fn delete(&self, owner: &Owner, name: &str) -> Result<()> {
        let _lock = self.locked();
        let (dir, mut stored) = self.load()?;
        if stored.remove(owner, name).is_none() {
            return Ok(());
        }
        self.save(dir, stored)
    }

    fn accounts(&self) -> Result<Vec<Account>> {
        let _lock = self.locked();
        let stored = self.load()?.1;
        // A cage value's name has no `/`, so the last `/` ends the cage id.
        let cages = stored.values.into_keys().filter_map(|key| {
            let (id, name) = key.rsplit_once('/')?;
            Some((Owner::Cage(Cow::Owned(id.to_owned())), name.to_owned()))
        });
        Ok(cages.chain(stored.library.into_keys().map(|name| (Owner::Library, name))).collect())
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Default)]
    pub struct MemoryVault {
        pub values: Mutex<HashMap<Account, SecretValue>>,
        pub dates: Mutex<HashMap<Account, Option<i64>>>,
        /// When set, this many more writes succeed, and the writes after
        /// them fail and keep the values.
        pub writes_left: Mutex<Option<usize>>,
        pub gets: AtomicUsize,
        /// Makes the next set change the value and then fail.
        pub fail_after_write: AtomicBool,
    }

    impl MemoryVault {
        fn write(&self) -> Result<()> {
            match self.writes_left.lock().unwrap().as_mut() {
                Some(0) => Err(Error::Vault("write failed".into())),
                Some(left) => {
                    *left -= 1;
                    Ok(())
                }
                None => Ok(()),
            }
        }
    }

    impl Vault for MemoryVault {
        fn put(&self, owner: &Owner, name: &str, entry: &VaultEntry) -> Result<()> {
            self.write()?;
            let account = (owner.clone().into_owned(), name.to_owned());
            self.values.lock().unwrap().insert(account.clone(), entry.value.clone());
            self.dates.lock().unwrap().insert(account, entry.saved_at);
            if self.fail_after_write.swap(false, Ordering::SeqCst) {
                return Err(Error::Vault("folder sync failed".into()));
            }
            Ok(())
        }

        fn entry(&self, owner: &Owner, name: &str) -> Result<Option<VaultEntry>> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            let account = (owner.clone().into_owned(), name.to_owned());
            Ok(self.values.lock().unwrap().get(&account).cloned().map(|value| VaultEntry {
                value,
                saved_at: self.dates.lock().unwrap().get(&account).copied().flatten(),
            }))
        }

        fn delete(&self, owner: &Owner, name: &str) -> Result<()> {
            self.write()?;
            let account = (owner.clone().into_owned(), name.to_owned());
            self.values.lock().unwrap().remove(&account);
            self.dates.lock().unwrap().remove(&account);
            Ok(())
        }

        fn accounts(&self) -> Result<Vec<Account>> {
            Ok(self.values.lock().unwrap().keys().cloned().collect())
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn file_vault() -> (FileVault, PathBuf) {
        let data = std::env::temp_dir().join(format!("zwinger-vault-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&data).unwrap();
        let vault = FileVault::new(&data).unwrap();
        let dir = vault.dir.clone();
        (vault, dir)
    }

    fn c(id: &str) -> Owner<'_> {
        Owner::cage(id)
    }

    fn cage_accounts(vault: &dyn Vault) -> Vec<String> {
        let accounts = vault.accounts().unwrap();
        accounts.into_iter().map(|(owner, name)| format!("{owner:?}/{name}")).collect()
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_file_vault_round_trips_multi_line_values() {
        let (vault, dir) = file_vault();
        assert_eq!(mode(&dir), 0o700);
        let pem = "-----BEGIN KEY-----\nabc/def+\n\"quoted\"\n-----END KEY-----\n";
        vault.set(&c("p/1"), "KEY", &SecretValue::new(pem)).unwrap();
        vault.set(&c("p/1"), "OTHER", &SecretValue::new("x")).unwrap();
        assert_eq!(mode(&dir.join(VAULT_FILE)), 0o600);

        let again = FileVault::new(dir.parent().unwrap()).unwrap();
        assert_eq!(again.get(&c("p/1"), "KEY").unwrap().unwrap().expose(), pem);
        assert_eq!(
            again.accounts().unwrap(),
            [(Owner::Cage("p/1".into()), "KEY".to_owned()), (Owner::Cage("p/1".into()), "OTHER".to_owned())]
        );
        let json: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(VAULT_FILE)).unwrap()).unwrap();
        assert_eq!(json["version"], 3);
        assert_eq!(json["values"]["p/1/KEY"]["value"], pem);
        assert!(json["values"]["p/1/KEY"]["saved_at"].as_i64().is_some());
    }

    #[test]
    fn the_file_vault_reads_old_plain_string_entries_without_a_date() {
        let (vault, dir) = file_vault();
        let file = dir.join(VAULT_FILE);
        fs::write(&file, r#"{"version":1,"values":{"p/KEY":"old"}}"#).unwrap();
        chmod(&file, 0o600);
        let old = vault.entry(&c("p"), "KEY").unwrap().unwrap();
        assert_eq!(old.value.expose(), "old");
        assert_eq!(old.saved_at, None);
        vault.set(&c("p"), "OTHER", &SecretValue::new("new")).unwrap();
        assert_eq!(vault.entry(&c("p"), "KEY").unwrap(), Some(old));
    }

    #[test]
    fn a_missing_file_is_an_empty_vault() {
        let (vault, dir) = file_vault();
        assert_eq!(vault.get(&c("p"), "KEY").unwrap(), None);
        assert!(vault.accounts().unwrap().is_empty());
        vault.delete(&c("p"), "KEY").unwrap();
        assert!(!dir.join(VAULT_FILE).exists());
    }

    #[test]
    fn delete_removes_one_value_and_an_absent_value_is_no_error() {
        let (vault, _) = file_vault();
        vault.set(&c("p"), "A", &SecretValue::new("1")).unwrap();
        vault.set(&c("p"), "B", &SecretValue::new("2")).unwrap();
        vault.delete(&c("p"), "A").unwrap();
        vault.delete(&c("p"), "A").unwrap();
        vault.delete(&c("q"), "B").unwrap();
        assert_eq!(vault.accounts().unwrap(), [(Owner::Cage("p".into()), "B".to_owned())]);
    }

    #[test]
    fn wide_modes_and_symlinks_are_refused() {
        let (vault, dir) = file_vault();
        vault.set(&c("p"), "KEY", &SecretValue::new("v")).unwrap();
        let file = dir.join(VAULT_FILE);

        chmod(&file, 0o644);
        let error = vault.get(&c("p"), "KEY").unwrap_err();
        assert!(
            matches!(&error, Error::VaultMode { expected, path } if expected == "600" && Path::new(path) == file),
            "{error:?}"
        );
        assert!(vault.set(&c("p"), "KEY", &SecretValue::new("w")).is_err());
        chmod(&file, 0o600);

        chmod(&dir, 0o755);
        assert!(matches!(vault.accounts(), Err(Error::VaultMode { expected, .. }) if expected == "700"));
        chmod(&dir, 0o700);

        let real = dir.join("real.json");
        fs::rename(&file, &real).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        assert!(matches!(vault.get(&c("p"), "KEY"), Err(Error::VaultLink { .. })));
        assert!(matches!(vault.set(&c("p"), "KEY", &SecretValue::new("w")), Err(Error::VaultLink { .. })));
    }

    #[test]
    fn an_unknown_version_or_bad_json_is_an_error_that_shows_no_value() {
        let (vault, dir) = file_vault();
        let file = dir.join(VAULT_FILE);
        let write = |text: &str| {
            fs::write(&file, text).unwrap();
            chmod(&file, 0o600);
        };
        write(r#"{"version":4,"values":{}}"#);
        assert!(matches!(vault.get(&c("p"), "KEY"), Err(Error::Vault(m)) if m.contains("unknown version 4")));
        write(r#"{"version":1,"values":{"p/KEY":"sk-hidden" "#);
        let error = vault.get(&c("p"), "KEY").unwrap_err();
        assert!(matches!(error, Error::Vault(_)) && !error.to_string().contains("sk-hidden"), "{error}");
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_and_leaves_no_temp_file() {
        let (vault, dir) = file_vault();
        vault.set(&c("p"), "KEY", &SecretValue::new("old")).unwrap();
        vault.fail_rename.store(true, Ordering::SeqCst);
        assert!(matches!(vault.set(&c("p"), "KEY", &SecretValue::new("new")), Err(Error::Vault(_))));
        assert_eq!(vault.get(&c("p"), "KEY").unwrap().unwrap().expose(), "old");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [VAULT_FILE]);
    }

    /// A foreign object with the temp name of this process makes the write
    /// fail, and the object stays.
    #[test]
    fn a_temp_name_collision_fails_the_write_and_keeps_the_object() {
        let (vault, dir) = file_vault();
        vault.set(&c("p"), "KEY", &SecretValue::new("old")).unwrap();
        let outside = std::env::temp_dir().join(format!("zwinger-sentinel-{}", uuid::Uuid::new_v4()));
        fs::write(&outside, "sentinel").unwrap();
        let link = dir.join(format!("{TEMP_PREFIX}{}", std::process::id()));
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(matches!(vault.set(&c("p"), "KEY", &SecretValue::new("new")), Err(Error::Vault(_))));
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "sentinel");
        assert_eq!(vault.get(&c("p"), "KEY").unwrap().unwrap().expose(), "old");
        fs::remove_file(&outside).unwrap();
    }

    #[test]
    fn a_new_vault_removes_stale_temp_files() {
        let (_, dir) = file_vault();
        let stale = dir.join(format!("{TEMP_PREFIX}1"));
        fs::write(&stale, "old values").unwrap();
        FileVault::new(dir.parent().unwrap()).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn the_next_write_removes_stale_temp_files() {
        let (vault, dir) = file_vault();
        let stale = dir.join(format!("{TEMP_PREFIX}1"));
        fs::write(&stale, "partial").unwrap();
        vault.set(&c("p"), "KEY", &SecretValue::new("v")).unwrap();
        assert!(!stale.exists());
        assert_eq!(vault.get(&c("p"), "KEY").unwrap().unwrap().expose(), "v");
    }

    fn other_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }.wrapping_add(1)
    }

    #[test]
    fn the_check_refuses_each_wrong_owner_type_and_mode_alone() {
        let (vault, dir) = file_vault();
        vault.set(&c("p"), "KEY", &SecretValue::new("v")).unwrap();
        let file = dir.join(VAULT_FILE);
        for (path, kind, mode) in [(&dir, FileType::Directory, 0o700), (&file, FileType::RegularFile, 0o600)] {
            let good = rustix::fs::lstat(path).unwrap();
            vault.check(path, &good, kind, mode).unwrap();
            let mut foreign = good;
            foreign.st_uid = other_uid();
            assert!(matches!(vault.check(path, &foreign, kind, mode), Err(Error::VaultOwner { .. })));
            let mut wide = good;
            wide.st_mode |= 0o040;
            assert!(matches!(vault.check(path, &wide, kind, mode), Err(Error::VaultMode { .. })));
            let other = if kind == FileType::Directory { FileType::RegularFile } else { FileType::Directory };
            assert!(matches!(vault.check(path, &good, other, mode), Err(Error::VaultType { .. })));
        }
    }

    /// A foreign owner fails each read and write before it reads, replaces
    /// or cleans anything.
    #[test]
    fn a_foreign_owner_fails_each_entry_point_and_changes_nothing() {
        let (mut vault, dir) = file_vault();
        vault.set(&c("p"), "KEY", &SecretValue::new("v")).unwrap();
        let file = dir.join(VAULT_FILE);
        let before = fs::read(&file).unwrap();
        let stale = dir.join(format!("{TEMP_PREFIX}1"));
        fs::write(&stale, "partial").unwrap();

        vault.owner = other_uid();
        let owner = |r: Result<()>| assert!(matches!(r, Err(Error::VaultOwner { .. })), "{r:?}");
        owner(vault.get(&c("p"), "KEY").map(drop));
        owner(vault.accounts().map(drop));
        owner(vault.set(&c("p"), "KEY", &SecretValue::new("w")));
        owner(vault.delete(&c("p"), "KEY"));
        owner(FileVault::open(dir.parent().unwrap(), other_uid()).map(drop));
        assert_eq!(fs::read(&file).unwrap(), before);
        assert!(stale.exists());

        vault.owner = other_uid().wrapping_sub(1);
        assert_eq!(vault.get(&c("p"), "KEY").unwrap().unwrap().expose(), "v");
    }

    /// A temp-named link, folder or file of another kind stays, and so does
    /// the target of the link. A vault folder that is a link is refused.
    #[test]
    fn foreign_objects_and_their_targets_stay() {
        let (vault, dir) = file_vault();
        let outside = std::env::temp_dir().join(format!("zwinger-sentinel-{}", uuid::Uuid::new_v4()));
        fs::write(&outside, "sentinel").unwrap();
        let link = dir.join(format!("{TEMP_PREFIX}7"));
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let folder = dir.join(format!("{TEMP_PREFIX}8"));
        fs::create_dir(&folder).unwrap();
        vault.set(&c("p"), "KEY", &SecretValue::new("v")).unwrap();
        FileVault::new(dir.parent().unwrap()).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink() && folder.is_dir());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "sentinel");

        // The vault file is a folder: refused on read and on write, and it stays.
        let file = dir.join(VAULT_FILE);
        fs::rename(&file, dir.join("saved")).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(matches!(vault.get(&c("p"), "KEY"), Err(Error::VaultType { kind: "file", .. })));
        assert!(vault.set(&c("p"), "KEY", &SecretValue::new("w")).is_err());
        assert!(file.is_dir());
        fs::remove_dir(&file).unwrap();

        // A FIFO does not hang the read.
        assert!(std::process::Command::new("mkfifo").arg(&file).status().unwrap().success());
        chmod(&file, 0o600);
        assert!(matches!(vault.get(&c("p"), "KEY"), Err(Error::VaultType { kind: "file", .. })));
        fs::remove_file(&file).unwrap();
        fs::rename(dir.join("saved"), &file).unwrap();

        // The vault folder is a link to a valid folder: refused, and its entries stay.
        let real = dir.with_file_name("real-vault");
        fs::rename(&dir, &real).unwrap();
        std::os::unix::fs::symlink(&real, &dir).unwrap();
        let stale = real.join(format!("{TEMP_PREFIX}9"));
        fs::write(&stale, "partial").unwrap();
        assert!(matches!(FileVault::new(dir.parent().unwrap()), Err(Error::VaultLink { .. })));
        assert!(matches!(vault.get(&c("p"), "KEY"), Err(Error::VaultLink { .. })));
        assert!(matches!(vault.set(&c("p"), "KEY", &SecretValue::new("w")), Err(Error::VaultLink { .. })));
        assert!(stale.exists());
    }

    #[test]
    fn library_values_sit_beside_old_entries_and_keep_their_dates() {
        let (vault, dir) = file_vault();
        let file = dir.join(VAULT_FILE);
        fs::write(&file, r#"{"version":1,"values":{"p/OLD":"old","p/NEW":{"value":"new","saved_at":5}}}"#).unwrap();
        chmod(&file, 0o600);
        vault.set(&Owner::Library, "SHARED", &SecretValue::new("shared")).unwrap();

        let again = FileVault::new(dir.parent().unwrap()).unwrap();
        assert_eq!(cage_accounts(&again), ["Cage(\"p\")/NEW", "Cage(\"p\")/OLD", "Library/SHARED"]);
        assert_eq!(
            again.entry(&c("p"), "OLD").unwrap().unwrap(),
            VaultEntry { value: SecretValue::new("old"), saved_at: None }
        );
        assert_eq!(again.entry(&c("p"), "NEW").unwrap().unwrap().saved_at, Some(5));
        assert!(again.entry(&Owner::Library, "SHARED").unwrap().unwrap().saved_at.is_some());
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(json["library"]["SHARED"]["value"], "shared");
        let library = CredentialRef::library("SHARED");
        assert_eq!(library.account("p"), (Owner::Library, "SHARED".to_owned()));
        let local = CredentialRef { source: Source::Local, name: "SHARED".into() };
        assert_eq!(local.account("p"), (Owner::Cage("p".into()), "SHARED".to_owned()));
    }

    /// A version 2 entry of a cage with the id `@library` stays that cage's
    /// value. A library value with the same name is a different value.
    #[test]
    fn a_legacy_at_library_cage_and_the_library_stay_apart() {
        let (vault, dir) = file_vault();
        let file = dir.join(VAULT_FILE);
        fs::write(&file, r#"{"version":2,"values":{"@library/KEY":{"value":"cage","saved_at":1}}}"#).unwrap();
        chmod(&file, 0o600);
        let legacy = c("@library");
        assert_eq!(vault.get(&Owner::Library, "KEY").unwrap(), None);
        vault.set(&Owner::Library, "KEY", &SecretValue::new("library")).unwrap();
        vault.set(&Owner::Library, "KEY", &SecretValue::new("rotated")).unwrap();

        let again = FileVault::new(dir.parent().unwrap()).unwrap();
        assert_eq!(cage_accounts(&again), ["Cage(\"@library\")/KEY", "Library/KEY"]);
        assert_eq!(again.get(&legacy, "KEY").unwrap().unwrap().expose(), "cage");
        assert_eq!(again.get(&Owner::Library, "KEY").unwrap().unwrap().expose(), "rotated");
        again.delete(&legacy, "KEY").unwrap();
        assert_eq!(again.get(&Owner::Library, "KEY").unwrap().unwrap().expose(), "rotated");
        assert_eq!(cage_accounts(&again), ["Library/KEY"]);
    }

    /// An older build refuses version 3, so it cannot save the file without
    /// the library map.
    #[test]
    fn a_library_value_makes_a_version_3_file() {
        let (vault, dir) = file_vault();
        vault.set(&Owner::Library, "KEY", &SecretValue::new("v")).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(VAULT_FILE)).unwrap()).unwrap();
        assert_eq!(json["version"], 3);
        assert_eq!(json["values"], serde_json::json!({}));
    }

    pub fn secret(name: &str, hosts: &[&str]) -> Secret {
        Secret {
            name: name.into(),
            hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
            places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
            origin: Origin::User,
        }
    }

    #[test]
    fn normalize_trims_and_dedupes_hosts() {
        let normal =
            secret(" OPENAI_API_KEY ", &[" API.openai.com", "api.openai.com", "example.com"]).normalize().unwrap();
        assert_eq!(normal.name, "OPENAI_API_KEY");
        assert_eq!(normal.hosts, ["api.openai.com", "example.com"]);
        assert_eq!(normal.placeholder(), "$MSB_OPENAI_API_KEY$");
        assert!(secret("_X1", &["a.io"]).normalize().is_ok());
    }

    #[test]
    fn normalize_rejects_bad_names() {
        for name in ["", "openai", "1KEY", "A-B", "A B", "KÜ", "PATH", "HOME", "TERM", "MSB_X"] {
            assert!(matches!(secret(name, &["a.io"]).normalize(), Err(Error::InvalidSecret(..))), "{name:?}");
        }
    }

    #[test]
    fn normalize_rejects_bad_hosts_and_places() {
        for hosts in [&[][..], &["*"], &["localhost"], &["https://a.io"], &["*.openai.com"]] {
            assert!(secret("KEY", hosts).normalize().is_err(), "{hosts:?}");
        }
        let mut none = secret("KEY", &["a.io"]);
        none.places.header_fields.clear();
        assert!(matches!(none.clone().normalize(), Err(Error::InvalidSecret(..))));
        none.places.body = true;
        assert!(none.normalize().is_ok());
    }

    #[test]
    fn normalize_lowercases_dedupes_and_checks_header_fields() {
        let mut key = secret("KEY", &["a.io"]);
        key.places.header_fields = vec![" Authorization".into(), "X-Api-Key".into(), "authorization".into()];
        assert_eq!(key.normalize().unwrap().places.header_fields, ["authorization", "x-api-key"]);
        for field in ["", " ", "x api", "x:y", "kü", "a\nb"] {
            let mut key = secret("KEY", &["a.io"]);
            key.places.header_fields = vec!["authorization".into(), field.into()];
            assert!(matches!(key.normalize(), Err(Error::InvalidSecret(..))), "{field:?}");
        }
    }

    #[test]
    fn placeholders_are_not_prefixes_of_each_other() {
        let key = secret("KEY", &["a.io"]).placeholder();
        let key_2 = secret("KEY_2", &["a.io"]).placeholder();
        assert!(!key_2.starts_with(&key) && !key.starts_with(&key_2));
    }

    #[test]
    fn round_01_places_are_migrated() {
        let places = |json: &str| serde_json::from_str::<Places>(json).unwrap().header_fields;
        assert_eq!(places(r#"{"headers":true,"query":false,"body":false}"#), ["authorization", "x-api-key"]);
        assert_eq!(places(r#"{"query":true,"body":false}"#), ["authorization", "x-api-key"]);
        assert!(places(r#"{"headers":false,"query":true,"body":false}"#).is_empty());
        assert_eq!(places(r#"{"headerFields":["x-goog-api-key"],"query":false,"body":false}"#), ["x-goog-api-key"]);
    }

    #[test]
    fn host_logins_reach_no_refresh_host_and_use_headers_only() {
        let mut login = secret("CLAUDE_CODE_OAUTH_TOKEN", &["api.anthropic.com"]);
        login.origin = Origin::HostLogin { agent: AgentKind::Claude };
        assert!(login.clone().normalize().is_ok());
        for host in [
            "console.anthropic.com",
            "platform.claude.com",
            "auth.openai.com",
            "api.github.com",
            "oauth2.googleapis.com",
        ] {
            let mut bad = login.clone();
            bad.hosts.push(host.into());
            assert!(matches!(bad.normalize(), Err(Error::InvalidSecret(..))), "{host}");
            // A user secret may name them.
            assert!(secret("GITHUB_TOKEN", &[host]).normalize().is_ok());
        }
        let mut body = login.clone();
        body.places.body = true;
        assert!(matches!(body.normalize(), Err(Error::InvalidSecret(..))));
        let json = serde_json::to_value(&login).unwrap();
        assert_eq!(json["origin"], serde_json::json!({ "kind": "hostLogin", "agent": "claude" }));
        assert_eq!(serde_json::from_value::<Secret>(json).unwrap(), login);
    }

    #[test]
    fn duplicates_are_rejected() {
        let secrets = [secret("A", &["a.io"]), secret("B", &["a.io"]), secret("A", &["b.io"])];
        assert!(matches!(check_unique(&secrets), Err(Error::DuplicateSecret(n)) if n == "A"));
        assert!(check_unique(&secrets[..2]).is_ok());
    }

    #[test]
    fn value_is_redacted_and_config_has_no_value() {
        let value = SecretValue::new("sk-very-secret");
        assert!(!format!("{value:?}").contains("sk-very"));
        let json = serde_json::to_string(&secret("KEY", &["a.io"])).unwrap();
        assert_eq!(
            json,
            r#"{"name":"KEY","hosts":["a.io"],"places":{"headerFields":["authorization"],"query":false,"body":false}}"#
        );
    }
}
