use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::error::{Error, Result};
use crate::host_agents::login;
use crate::shield;

const SERVICE: &str = "zwinger";
/// The longest wait for a Keychain call, which can show the macOS access prompt.
pub const KEYCHAIN_LIMIT: Duration = Duration::from_secs(60);

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
    /// The agent's login on the Mac, read at each shield start. The vault
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
fn is_token(field: &str) -> bool {
    !field.is_empty() && field.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

impl Secret {
    /// Trims the name and normalizes the hosts, then checks the rules.
    pub fn normalize(mut self) -> Result<Self> {
        let invalid = |name: &str, reason: &str| Error::InvalidSecret(name.to_owned(), reason.to_owned());
        self.name = self.name.trim().to_owned();
        let name = &self.name;
        let mut chars = name.chars();
        let valid_name = chars.next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !valid_name {
            return Err(invalid(name, "use A-Z, 0-9 and _, and do not start with a digit"));
        }
        if RESERVED.contains(&name.as_str()) || name.starts_with("MSB_") {
            return Err(invalid(name, "the shield sets this variable"));
        }
        let mut hosts: Vec<String> = Vec::new();
        for host in &self.hosts {
            let host = shield::domain(host)?;
            if host.starts_with("*.") {
                return Err(invalid(name, "use exact hosts, not wildcards"));
            }
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        if hosts.is_empty() {
            return Err(invalid(name, "add at least one host"));
        }
        // The refresh hosts never get a host token, so the agent cannot renew
        // the login or swap it for a new one.
        if let Origin::HostLogin { .. } = self.origin {
            if let Some(host) = hosts.iter().find(|h| login::is_refresh_host(h)) {
                return Err(invalid(name, &format!("a host login may not reach the refresh host {host}")));
            }
            if self.places.query || self.places.body {
                return Err(invalid(name, "a host login goes into header fields only"));
            }
        }
        self.hosts = hosts;
        let mut fields: Vec<String> = Vec::new();
        for field in &self.places.header_fields {
            let field = field.trim().to_ascii_lowercase();
            if !is_token(&field) {
                return Err(invalid(name, &format!("{field:?} is not a valid header field name")));
            }
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
        self.places.header_fields = fields;
        let Places { header_fields, query, body } = &self.places;
        if header_fields.is_empty() && !query && !body {
            return Err(invalid(name, "select at least one place"));
        }
        Ok(self)
    }

    /// The text that the guest sees in the variable instead of the value. The
    /// closing `$` keeps one placeholder from being a prefix of another, such
    /// as `KEY` of `KEY_2`.
    pub fn placeholder(&self) -> String {
        format!("$MSB_{}$", self.name)
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
pub fn account(shield_id: &str, name: &str) -> String {
    format!("{shield_id}/{name}")
}

/// Stores secret values per shield.
pub trait Vault: Send + Sync {
    fn set(&self, shield_id: &str, name: &str, value: &SecretValue) -> Result<()>;
    fn get(&self, shield_id: &str, name: &str) -> Result<Option<SecretValue>>;
    /// Deleting an absent value is not an error.
    fn delete(&self, shield_id: &str, name: &str) -> Result<()>;
    /// The [`account`]s of all stored values. Reads no values.
    fn accounts(&self) -> Result<Vec<String>>;
    /// A write of the value passed its deadline and still runs.
    fn is_pending(&self, _shield_id: &str, _name: &str) -> bool {
        false
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// One thread that runs its jobs one after another, in submit order.
struct Worker(Sender<Job>);

impl Worker {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("zwinger-keychain".into())
            .spawn(move || {
                for job in rx {
                    job();
                }
            })
            .expect("spawn the Keychain worker");
        Self(tx)
    }

    /// Runs `f` after the jobs before it. `None` if it does not end in
    /// `limit`: it then still runs, and the jobs after it wait for it.
    fn run<T: Send + 'static>(&self, limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = mpsc::channel();
        self.submit(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit).ok()
    }

    fn submit(&self, job: impl FnOnce() + Send + 'static) {
        // The thread never ends, so the send cannot fail.
        let _ = self.0.send(Box::new(job));
    }
}

/// The worker of all Keychain calls, so they run in submit order.
static KEYCHAIN: LazyLock<Arc<Worker>> = LazyLock::new(|| Arc::new(Worker::new()));

/// Runs a Keychain call on the Keychain worker. `None` if it does not end in
/// `limit`, such as when nobody answers the macOS prompt.
pub fn bounded<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    KEYCHAIN.run(limit, f)
}

/// A vault whose calls run on one ordered worker, each with a deadline. A
/// write that passes its deadline still runs, and its account is pending
/// until it ends. A later call on the same account runs after it.
pub struct Ordered {
    inner: Arc<dyn Vault>,
    worker: Arc<Worker>,
    limit: Duration,
    /// Per account, the writes that passed their deadline and still run.
    pending: Arc<Mutex<HashMap<String, usize>>>,
}

impl Ordered {
    /// `inner` runs on the Keychain worker.
    pub fn new(inner: Arc<dyn Vault>) -> Self {
        Self { inner, worker: KEYCHAIN.clone(), limit: KEYCHAIN_LIMIT, pending: Arc::default() }
    }

    /// On a worker of its own, so a slow test vault holds no other test.
    #[cfg(test)]
    pub fn alone(inner: Arc<dyn Vault>, limit: Duration) -> Self {
        Self { inner, worker: Arc::new(Worker::new()), limit, pending: Arc::default() }
    }

    fn read<T: Send + 'static>(&self, f: impl FnOnce(&dyn Vault) -> Result<T> + Send + 'static) -> Result<T> {
        let inner = self.inner.clone();
        self.worker.run(self.limit, move || f(inner.as_ref())).ok_or(Error::KeychainTimeout)?
    }

    /// The job and a caller that gives up agree under the `pending` lock:
    /// either the result is there, or the job sees `late` and clears the mark.
    fn write(&self, account: String, f: impl FnOnce(&dyn Vault) -> Result<()> + Send + 'static) -> Result<()> {
        let (inner, pending) = (self.inner.clone(), self.pending.clone());
        let late = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (job_late, job_account) = (late.clone(), account.clone());
        self.worker.submit(move || {
            let result = f(inner.as_ref());
            let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = tx.send(result);
            if job_late.load(Ordering::SeqCst)
                && let Some(count) = pending.get_mut(&job_account)
            {
                *count -= 1;
                if *count == 0 {
                    pending.remove(&job_account);
                }
            }
        });
        if let Ok(result) = rx.recv_timeout(self.limit) {
            return result;
        }
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        late.store(true, Ordering::SeqCst);
        *pending.entry(account).or_default() += 1;
        Err(Error::KeychainTimeout)
    }
}

impl Vault for Ordered {
    fn set(&self, shield_id: &str, name: &str, value: &SecretValue) -> Result<()> {
        let (id, key, value) = (shield_id.to_owned(), name.to_owned(), value.clone());
        self.write(account(shield_id, name), move |v| v.set(&id, &key, &value))
    }

    fn get(&self, shield_id: &str, name: &str) -> Result<Option<SecretValue>> {
        let (id, key) = (shield_id.to_owned(), name.to_owned());
        self.read(move |v| v.get(&id, &key))
    }

    fn delete(&self, shield_id: &str, name: &str) -> Result<()> {
        let (id, key) = (shield_id.to_owned(), name.to_owned());
        self.write(account(shield_id, name), move |v| v.delete(&id, &key))
    }

    fn accounts(&self) -> Result<Vec<String>> {
        self.read(|v| v.accounts())
    }

    fn is_pending(&self, shield_id: &str, name: &str) -> bool {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.contains_key(&account(shield_id, name))
    }
}

/// The macOS Keychain, one generic password per secret. Its calls have no
/// deadline: the app runs them in [`Ordered`].
#[cfg(target_os = "macos")]
pub struct Keychain;

#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

#[cfg(target_os = "macos")]
impl Keychain {
    fn entry(shield_id: &str, name: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, &account(shield_id, name)).map_err(keychain_error)
    }

    fn query() -> security_framework::item::ItemSearchOptions {
        let mut query = security_framework::item::ItemSearchOptions::new();
        query.class(security_framework::item::ItemClass::generic_password()).service(SERVICE);
        query
    }
}

/// The keyring and OS messages contain no secret data.
#[cfg(target_os = "macos")]
fn keychain_error(e: impl fmt::Display) -> Error {
    Error::Keychain(e.to_string())
}

#[cfg(target_os = "macos")]
impl Vault for Keychain {
    fn set(&self, shield_id: &str, name: &str, value: &SecretValue) -> Result<()> {
        Self::entry(shield_id, name)?.set_password(value.expose()).map_err(keychain_error)
    }

    fn get(&self, shield_id: &str, name: &str) -> Result<Option<SecretValue>> {
        match Self::entry(shield_id, name)?.get_password() {
            Ok(value) => Ok(Some(SecretValue::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    /// Not through keyring, because keyring drops the result of the delete.
    fn delete(&self, shield_id: &str, name: &str) -> Result<()> {
        match Self::query().account(&account(shield_id, name)).delete() {
            Err(e) if e.code() != ERR_SEC_ITEM_NOT_FOUND => Err(keychain_error(e)),
            _ => Ok(()),
        }
    }

    fn accounts(&self) -> Result<Vec<String>> {
        let found =
            Self::query().load_attributes(true).load_data(false).limit(security_framework::item::Limit::All).search();
        match found {
            Ok(items) => Ok(items.iter().filter_map(|item| item.simplify_dict()?.remove("acct")).collect()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(Vec::new()),
            Err(e) => Err(keychain_error(e)),
        }
    }
}

#[cfg(test)]
pub mod fake {
    use std::sync::Condvar;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A vault whose writes wait while it is held, as a Keychain call waits
    /// for the macOS prompt.
    #[derive(Default)]
    pub struct SlowVault {
        pub memory: MemoryVault,
        held: Mutex<bool>,
        released: Condvar,
        /// The writes in the order that they ran: the account, and the value or `None` for a delete.
        pub writes: Mutex<Vec<(String, Option<String>)>>,
        /// Once this many writes ran, the next writes fail, as when the
        /// Keychain denies them.
        pub fail_after: Mutex<Option<usize>>,
    }

    impl SlowVault {
        pub fn hold(&self) {
            *self.held.lock().unwrap() = true;
        }

        pub fn release(&self) {
            *self.held.lock().unwrap() = false;
            self.released.notify_all();
        }

        /// Waits while held, then fails past `fail_after`.
        fn wait(&self) -> Result<()> {
            let held = self.held.lock().unwrap();
            drop(self.released.wait_while(held, |held| *held).unwrap());
            if self.fail_after.lock().unwrap().is_some_and(|n| self.writes.lock().unwrap().len() >= n) {
                return Err(Error::Keychain("denied".into()));
            }
            Ok(())
        }
    }

    impl Vault for SlowVault {
        fn set(&self, shield_id: &str, name: &str, value: &SecretValue) -> Result<()> {
            self.wait()?;
            self.writes.lock().unwrap().push((account(shield_id, name), Some(value.expose().to_owned())));
            self.memory.set(shield_id, name, value)
        }

        fn get(&self, shield_id: &str, name: &str) -> Result<Option<SecretValue>> {
            self.memory.get(shield_id, name)
        }

        fn delete(&self, shield_id: &str, name: &str) -> Result<()> {
            self.wait()?;
            self.writes.lock().unwrap().push((account(shield_id, name), None));
            self.memory.delete(shield_id, name)
        }

        fn accounts(&self) -> Result<Vec<String>> {
            self.memory.accounts()
        }
    }

    #[derive(Default)]
    pub struct MemoryVault {
        pub values: Mutex<HashMap<String, SecretValue>>,
        /// When set, each delete fails and keeps the value.
        pub fail_delete: AtomicBool,
        pub gets: AtomicUsize,
    }

    impl Vault for MemoryVault {
        fn set(&self, shield_id: &str, name: &str, value: &SecretValue) -> Result<()> {
            self.values.lock().unwrap().insert(account(shield_id, name), value.clone());
            Ok(())
        }

        fn get(&self, shield_id: &str, name: &str) -> Result<Option<SecretValue>> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(self.values.lock().unwrap().get(&account(shield_id, name)).cloned())
        }

        fn delete(&self, shield_id: &str, name: &str) -> Result<()> {
            if self.fail_delete.load(Ordering::SeqCst) {
                return Err(Error::Keychain("delete failed".into()));
            }
            self.values.lock().unwrap().remove(&account(shield_id, name));
            Ok(())
        }

        fn accounts(&self) -> Result<Vec<String>> {
            Ok(self.values.lock().unwrap().keys().cloned().collect())
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::fake::SlowVault;
    use super::*;

    const DEADLINE: Duration = Duration::from_millis(50);

    /// Waits until no write of the account is pending.
    pub fn until_settled(vault: &dyn Vault, shield_id: &str, name: &str) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while vault.is_pending(shield_id, name) {
            assert!(std::time::Instant::now() < end, "{shield_id}/{name} stays pending");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_write_past_its_deadline_is_pending_until_it_ends() {
        let slow = Arc::new(SlowVault::default());
        let vault = Ordered::alone(slow.clone(), DEADLINE);
        vault.set("s", "B", &SecretValue::new("b")).unwrap();
        slow.hold();
        assert!(matches!(vault.set("s", "A", &SecretValue::new("1")), Err(Error::KeychainTimeout)));
        assert!(vault.is_pending("s", "A"));
        assert!(!vault.is_pending("s", "B"));
        // A read waits behind the write and passes its deadline too.
        assert!(matches!(vault.get("s", "B"), Err(Error::KeychainTimeout)));
        slow.release();
        until_settled(&vault, "s", "A");
        assert_eq!(vault.get("s", "A").unwrap(), Some(SecretValue::new("1")));
    }

    #[test]
    fn keychain_calls_run_in_submit_order() {
        let slow = Arc::new(SlowVault::default());
        let vault = Ordered::alone(slow.clone(), DEADLINE);
        slow.hold();
        assert!(vault.set("s", "A", &SecretValue::new("1")).is_err());
        assert!(vault.set("s", "A", &SecretValue::new("2")).is_err());
        assert!(vault.delete("s", "B").is_err());
        slow.release();
        until_settled(&vault, "s", "A");
        until_settled(&vault, "s", "B");
        assert_eq!(
            *slow.writes.lock().unwrap(),
            [
                ("s/A".to_owned(), Some("1".to_owned())),
                ("s/A".to_owned(), Some("2".to_owned())),
                ("s/B".to_owned(), None)
            ]
        );
        assert_eq!(vault.get("s", "A").unwrap(), Some(SecretValue::new("2")));
    }

    #[test]
    fn a_write_in_time_leaves_nothing_pending() {
        let vault = Ordered::alone(Arc::new(SlowVault::default()), Duration::from_secs(5));
        vault.set("s", "A", &SecretValue::new("1")).unwrap();
        vault.delete("s", "A").unwrap();
        assert!(!vault.is_pending("s", "A"));
        assert_eq!(vault.accounts().unwrap(), Vec::<String>::new());
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
