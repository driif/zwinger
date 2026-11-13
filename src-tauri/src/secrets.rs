use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::cage;
use crate::error::{Error, Result};
use crate::host_agents::login;

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

/// The vault account of a secret value.
pub fn account(cage_id: &str, name: &str) -> String {
    format!("{cage_id}/{name}")
}

/// Stores secret values per project.
pub trait Vault: Send + Sync {
    fn set(&self, cage_id: &str, name: &str, value: &SecretValue) -> Result<()>;
    fn get(&self, cage_id: &str, name: &str) -> Result<Option<SecretValue>>;
    /// Deleting an absent value is not an error.
    fn delete(&self, cage_id: &str, name: &str) -> Result<()>;
    /// The [`account`]s of all stored values. Returns no values.
    fn accounts(&self) -> Result<Vec<String>>;
}

/// The folder of the vault file, in the app data folder.
const VAULT_DIR: &str = "vault";
const VAULT_FILE: &str = "credentials.json";
/// The temp files of a write. A file with this prefix is from a write that
/// did not end, and the next write removes it.
const TEMP_PREFIX: &str = ".credentials.json.tmp-";
const FORMAT: u32 = 1;

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    values: BTreeMap<String, String>,
}

/// The values in one JSON file that only the user can read and write
/// (mode 0600, in a folder with mode 0700). Any process of the user can read
/// the file. Each write replaces the file atomically.
pub struct FileVault {
    dir: PathBuf,
    file: PathBuf,
    /// Held for each read and each read-modify-write.
    lock: Mutex<()>,
    /// Makes the next write fail after the temp file is written.
    #[cfg(test)]
    fail_rename: AtomicBool,
}

impl FileVault {
    /// Creates `<data>/vault` with mode 0700 if it does not exist.
    pub fn new(data: &Path) -> Result<Self> {
        let dir = data.join(VAULT_DIR);
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(vault_io(&dir, e)),
            _ => {}
        }
        // A crash can leave a temp file with all values, also deleted ones.
        remove_temps(&dir)?;
        Ok(Self {
            file: dir.join(VAULT_FILE),
            dir,
            lock: Mutex::default(),
            #[cfg(test)]
            fail_rename: AtomicBool::new(false),
        })
    }

    fn load(&self) -> Result<BTreeMap<String, String>> {
        if !private(&self.dir, 0o700)? {
            return Ok(BTreeMap::new());
        }
        // O_NOFOLLOW and the mode check on the open handle: the file that is
        // checked is the file that is read.
        let mut file = match fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&self.file) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(mode_error(&self.file, 0o600)),
            Err(e) => return Err(vault_io(&self.file, e)),
        };
        let meta = file.metadata().map_err(|e| vault_io(&self.file, e))?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err(mode_error(&self.file, 0o600));
        }
        let mut text = Vec::new();
        io::Read::read_to_end(&mut file, &mut text).map_err(|e| vault_io(&self.file, e))?;
        // The serde_json message can quote a value, so only its place is shown.
        let stored: Stored = serde_json::from_slice(&text).map_err(|e| {
            Error::Vault(format!("{} is not valid at line {}, column {}", self.file.display(), e.line(), e.column()))
        })?;
        if stored.version != FORMAT {
            return Err(Error::Vault(format!("{} has unknown version {}", self.file.display(), stored.version)));
        }
        Ok(stored.values)
    }

    fn save(&self, values: BTreeMap<String, String>) -> Result<()> {
        remove_temps(&self.dir)?;
        let temp = self.dir.join(format!("{TEMP_PREFIX}{}", std::process::id()));
        let json = serde_json::to_vec_pretty(&Stored { version: FORMAT, values })?;
        let written = self.write_temp(&temp, &json).and_then(|()| {
            #[cfg(test)]
            if self.fail_rename.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("test failure"));
            }
            fs::rename(&temp, &self.file)
        });
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(vault_io(&self.file, e));
        }
        // The rename is durable only when the folder is synced.
        fs::File::open(&self.dir).and_then(|dir| dir.sync_all()).map_err(|e| vault_io(&self.dir, e))
    }

    fn write_temp(&self, temp: &Path, json: &[u8]) -> io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(temp)?;
        file.write_all(json)?;
        file.sync_all()
    }

    fn locked(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `false` if `path` does not exist. An error if it is a symlink or if group
/// or other have a permission bit.
fn private(path: &Path, expected: u32) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(vault_io(path, e)),
    };
    if meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0 {
        return Err(mode_error(path, expected));
    }
    Ok(true)
}

fn mode_error(path: &Path, expected: u32) -> Error {
    Error::VaultMode { path: path.display().to_string(), expected: format!("{expected:o}") }
}

fn remove_temps(dir: &Path) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| vault_io(dir, e))?;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn vault_io(path: &Path, e: io::Error) -> Error {
    Error::Vault(format!("{}: {e}", path.display()))
}

impl Vault for FileVault {
    fn set(&self, cage_id: &str, name: &str, value: &SecretValue) -> Result<()> {
        let _lock = self.locked();
        let mut values = self.load()?;
        values.insert(account(cage_id, name), value.expose().to_owned());
        self.save(values)
    }

    fn get(&self, cage_id: &str, name: &str) -> Result<Option<SecretValue>> {
        let _lock = self.locked();
        Ok(self.load()?.remove(&account(cage_id, name)).map(SecretValue::new))
    }

    fn delete(&self, cage_id: &str, name: &str) -> Result<()> {
        let _lock = self.locked();
        let mut values = self.load()?;
        if values.remove(&account(cage_id, name)).is_none() {
            return Ok(());
        }
        self.save(values)
    }

    fn accounts(&self) -> Result<Vec<String>> {
        let _lock = self.locked();
        Ok(self.load()?.into_keys().collect())
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Default)]
    pub struct MemoryVault {
        pub values: Mutex<HashMap<String, SecretValue>>,
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
        fn set(&self, cage_id: &str, name: &str, value: &SecretValue) -> Result<()> {
            self.write()?;
            self.values.lock().unwrap().insert(account(cage_id, name), value.clone());
            if self.fail_after_write.swap(false, Ordering::SeqCst) {
                return Err(Error::Vault("folder sync failed".into()));
            }
            Ok(())
        }

        fn get(&self, cage_id: &str, name: &str) -> Result<Option<SecretValue>> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(self.values.lock().unwrap().get(&account(cage_id, name)).cloned())
        }

        fn delete(&self, cage_id: &str, name: &str) -> Result<()> {
            self.write()?;
            self.values.lock().unwrap().remove(&account(cage_id, name));
            Ok(())
        }

        fn accounts(&self) -> Result<Vec<String>> {
            Ok(self.values.lock().unwrap().keys().cloned().collect())
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn file_vault() -> (FileVault, PathBuf) {
        let data = std::env::temp_dir().join(format!("zwinger-vault-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&data).unwrap();
        let vault = FileVault::new(&data).unwrap();
        let dir = vault.dir.clone();
        (vault, dir)
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
        vault.set("p/1", "KEY", &SecretValue::new(pem)).unwrap();
        vault.set("p/1", "OTHER", &SecretValue::new("x")).unwrap();
        assert_eq!(mode(&dir.join(VAULT_FILE)), 0o600);

        let again = FileVault::new(dir.parent().unwrap()).unwrap();
        assert_eq!(again.get("p/1", "KEY").unwrap().unwrap().expose(), pem);
        assert_eq!(again.accounts().unwrap(), ["p/1/KEY", "p/1/OTHER"]);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(VAULT_FILE)).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["values"]["p/1/KEY"], pem);
    }

    #[test]
    fn a_missing_file_is_an_empty_vault() {
        let (vault, dir) = file_vault();
        assert_eq!(vault.get("p", "KEY").unwrap(), None);
        assert!(vault.accounts().unwrap().is_empty());
        vault.delete("p", "KEY").unwrap();
        assert!(!dir.join(VAULT_FILE).exists());
    }

    #[test]
    fn delete_removes_one_value_and_an_absent_value_is_no_error() {
        let (vault, _) = file_vault();
        vault.set("p", "A", &SecretValue::new("1")).unwrap();
        vault.set("p", "B", &SecretValue::new("2")).unwrap();
        vault.delete("p", "A").unwrap();
        vault.delete("p", "A").unwrap();
        vault.delete("q", "B").unwrap();
        assert_eq!(vault.accounts().unwrap(), ["p/B"]);
    }

    #[test]
    fn wide_modes_and_symlinks_are_refused() {
        let (vault, dir) = file_vault();
        vault.set("p", "KEY", &SecretValue::new("v")).unwrap();
        let file = dir.join(VAULT_FILE);

        chmod(&file, 0o644);
        let error = vault.get("p", "KEY").unwrap_err();
        assert!(matches!(&error, Error::VaultMode { expected, .. } if expected == "600"), "{error:?}");
        assert!(error.to_string().contains(&format!("chmod 600 {}", file.display())));
        assert!(vault.set("p", "KEY", &SecretValue::new("w")).is_err());
        chmod(&file, 0o600);

        chmod(&dir, 0o755);
        assert!(matches!(vault.accounts(), Err(Error::VaultMode { expected, .. }) if expected == "700"));
        chmod(&dir, 0o700);

        let real = dir.join("real.json");
        fs::rename(&file, &real).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        assert!(matches!(vault.get("p", "KEY"), Err(Error::VaultMode { .. })));
    }

    #[test]
    fn an_unknown_version_or_bad_json_is_an_error_that_shows_no_value() {
        let (vault, dir) = file_vault();
        let file = dir.join(VAULT_FILE);
        let write = |text: &str| {
            fs::write(&file, text).unwrap();
            chmod(&file, 0o600);
        };
        write(r#"{"version":2,"values":{}}"#);
        assert!(matches!(vault.get("p", "KEY"), Err(Error::Vault(m)) if m.contains("unknown version 2")));
        write(r#"{"version":1,"values":{"p/KEY":"sk-hidden" "#);
        let error = vault.get("p", "KEY").unwrap_err();
        assert!(matches!(error, Error::Vault(_)) && !error.to_string().contains("sk-hidden"), "{error}");
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_and_leaves_no_temp_file() {
        let (vault, dir) = file_vault();
        vault.set("p", "KEY", &SecretValue::new("old")).unwrap();
        vault.fail_rename.store(true, Ordering::SeqCst);
        assert!(matches!(vault.set("p", "KEY", &SecretValue::new("new")), Err(Error::Vault(_))));
        assert_eq!(vault.get("p", "KEY").unwrap().unwrap().expose(), "old");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [VAULT_FILE]);
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
        vault.set("p", "KEY", &SecretValue::new("v")).unwrap();
        assert!(!stale.exists());
        assert_eq!(vault.get("p", "KEY").unwrap().unwrap().expose(), "v");
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
