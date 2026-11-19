//! The one-time, opt-in import of the credentials that builds before the
//! vault file kept in the macOS Keychain (#132), and the cleanup of those
//! items. Nothing here runs at app start: each Keychain access follows a
//! click in Settings. A value goes only from the Keychain into the vault. No
//! report, error or receipt has a value.
//!
//! Only a saved import receipt allows a cleanup. The receipt keeps, for each
//! item, the revision that the import saw and what the import allows. A scan
//! never allows a cleanup.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::cage::Cage;
use crate::cage::manager::{Manager, lock};
use crate::error::{Error, Result};
use crate::network::Source;
use crate::runtime::SandboxRuntime;
use crate::secrets::{self, CredentialRef, Inserted, Owner, SecretValue};
use crate::store;

#[cfg(target_os = "macos")]
mod apple;

#[cfg(all(test, target_os = "macos"))]
mod hosted_smoke;

/// The generic-password service of the old builds. No other service is read or changed.
pub const SERVICE: &str = "zwinger";
/// The proxy CA key of the builds before #113. It is never imported and
/// never a current CA. Only a cleanup that the user confirms for it removes it.
pub const RETIRED_CA: &str = "zwinger/PROXY_CA_KEY";
/// The import receipt in the app data folder: account names, revisions and states only.
pub const FILE: &str = "legacy-keychain.json";
/// The most accounts that one cleanup takes from the UI.
const CLEAN_LIMIT: usize = 4096;

const ERR_USER_CANCELED: i32 = -128;
#[cfg(any(test, debug_assertions))]
const ERR_IO: i32 = -36;
const ERR_NOT_AVAILABLE: i32 = -25291;
const ERR_AUTH_FAILED: i32 = -25293;
const ERR_NO_SUCH_KEYCHAIN: i32 = -25294;
#[cfg(any(test, debug_assertions))]
const ERR_DUPLICATE_ITEM: i32 = -25299;
const ERR_ITEM_NOT_FOUND: i32 = -25300;
const ERR_NO_DEFAULT_KEYCHAIN: i32 = -25307;
const ERR_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// Why a Keychain call failed. It holds an OSStatus only, never item data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    /// The item is not there (errSecItemNotFound).
    Missing,
    /// The user canceled or denied the macOS prompt, or the Keychain is locked.
    Denied(i32),
    /// The value is not UTF-8 text, so the vault cannot hold it.
    NotText,
    /// The item is not the revision, or not the value, that the step expects.
    Changed,
    /// Any other OSStatus.
    Failed(i32),
}

impl SourceError {
    pub fn from_status(code: i32) -> Self {
        match code {
            ERR_ITEM_NOT_FOUND => Self::Missing,
            ERR_USER_CANCELED | ERR_AUTH_FAILED | ERR_INTERACTION_NOT_ALLOWED => Self::Denied(code),
            _ => Self::Failed(code),
        }
    }

    /// The text for the user, from the OSStatus only.
    pub fn message(self) -> String {
        match self {
            Self::Missing => "the old Keychain item is not there".into(),
            Self::Denied(ERR_USER_CANCELED) => {
                "the access to the old Keychain items was canceled; to try again, click the button and select Allow"
                    .into()
            }
            Self::Denied(ERR_INTERACTION_NOT_ALLOWED) => {
                "the Keychain is locked or cannot ask for access now; unlock it, then try again".into()
            }
            Self::Denied(code) => format!("the Mac did not allow access to the old Keychain items (OSStatus {code})"),
            Self::NotText => "the old Keychain item is not text".into(),
            Self::Changed => "the old Keychain item changed; run Import from Keychain again".into(),
            Self::Failed(ERR_NO_SUCH_KEYCHAIN | ERR_NO_DEFAULT_KEYCHAIN | ERR_NOT_AVAILABLE) => {
                "no Keychain is available for this macOS user".into()
            }
            Self::Failed(code) => format!("the Keychain failed with OSStatus {code}"),
        }
    }
}

impl From<SourceError> for Error {
    fn from(e: SourceError) -> Self {
        Error::LegacyKeychain(e.message())
    }
}

/// The generic-password items of service [`SERVICE`], by account. macOS
/// can ask the user for access in each call.
pub trait LegacySource: Send + Sync {
    /// The account of each item, once per item, so two items with one
    /// account give it twice. No item is not an error.
    fn accounts(&self) -> std::result::Result<Vec<String>, SourceError>;
    /// The revision of the one item of `account`, from metadata only. It
    /// names the item and its version: a replaced or changed item has another.
    fn revision(&self, account: &str) -> std::result::Result<String, SourceError>;
    /// The value of the item at `revision`, else [`SourceError::Changed`].
    fn read(&self, account: &str, revision: &str) -> std::result::Result<SecretValue, SourceError>;
    /// Removes the item at `revision`, and with `expected` only if it has
    /// that value. Else [`SourceError::Changed`] and the item stays.
    fn delete(
        &self,
        account: &str,
        revision: &str,
        expected: Option<&SecretValue>,
    ) -> std::result::Result<(), SourceError>;
}

/// What an old account is in the current config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// The cage `cage` has a local credential `name`.
    Importable {
        cage: String,
        name: String,
    },
    Skipped(Reason),
    /// [`RETIRED_CA`].
    RetiredCa,
}

/// Why an item was not imported, changed or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    /// The account is not `<project id>/<NAME>`.
    Format,
    /// No cage has the old project id.
    NoCage,
    /// The cage has no local credential with this name.
    NoCredential,
    /// The cage takes this name from the credential library.
    Library,
    /// The name is a host login of an agent. The vault has no value for it.
    HostLogin,
    /// The item has an empty value.
    Empty,
    /// The item is not text.
    NotText,
    /// The vault could not be read, written or synced.
    Vault,
    /// A Keychain call failed. `code` has the OSStatus.
    Keychain,
    /// The item, the vault value or the config changed after the import.
    Changed,
    /// The import receipt does not allow a removal of the item.
    NotImported,
    /// The item can be removed only after the user confirms it one by one.
    NotConfirmed,
    /// More than one item has this account. None of them is read or removed.
    Ambiguous,
}

/// The state of one item after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// Scan: an import can save it.
    Importable,
    /// Not imported: see the reason. The item stays.
    Skipped,
    /// The retired proxy CA key. It is never imported.
    RetiredCa,
    /// Import: the vault has the value now, and it read back the same.
    Imported,
    /// Import: the vault already had the same value. Nothing changed.
    Same,
    /// Import: the vault has a different value. It stays, and so does the item.
    Conflict,
    /// macOS denied or canceled the access, or the Keychain is locked. The item stays.
    Denied,
    /// The item is not in the Keychain. For a cleanup, this counts as done.
    Missing,
    /// A step failed: see the reason. The item stays.
    Failed,
    /// Cleanup: the item is removed from the Keychain.
    Cleaned,
    /// Cleanup: the item stays: see the reason.
    Kept,
}

/// One item of a report. No value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub account: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// The OSStatus of a denied or failed Keychain call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

impl Row {
    fn new(account: &str, outcome: Outcome) -> Self {
        Self { account: account.to_owned(), outcome, reason: None, code: None }
    }

    fn because(account: &str, outcome: Outcome, reason: Reason) -> Self {
        Self { reason: Some(reason), ..Self::new(account, outcome) }
    }

    fn classified(account: &str, class: &Class) -> Self {
        match class {
            Class::Importable { .. } => Self::new(account, Outcome::Importable),
            Class::Skipped(reason) => Self::because(account, Outcome::Skipped, *reason),
            Class::RetiredCa => Self::new(account, Outcome::RetiredCa),
        }
    }

    /// `unreadable`: the outcome of an item that is not text.
    fn source_error(account: &str, e: SourceError, unreadable: Outcome) -> Self {
        match e {
            SourceError::Missing => Self::new(account, Outcome::Missing),
            SourceError::Denied(code) => Self { code: Some(code), ..Self::new(account, Outcome::Denied) },
            SourceError::NotText => Self::because(account, unreadable, Reason::NotText),
            SourceError::Changed => Self::because(account, Outcome::Kept, Reason::Changed),
            SourceError::Failed(code) => {
                Self { code: Some(code), ..Self::because(account, Outcome::Failed, Reason::Keychain) }
            }
        }
    }
}

/// The result of an import or a cleanup.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub rows: Vec<Row>,
    /// The receipt was written. If not, no cleanup is allowed until an
    /// import saves one. The vault and the Keychain do not depend on it.
    pub saved: bool,
    /// The import that the receipt holds. A confirmed removal names it.
    pub generation: uuid::Uuid,
}

/// What the last import allows a cleanup to do with one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
enum Record {
    /// Imported or the same at `revision`: a cleanup removes this revision
    /// only if the vault holds its value, durably, under the edit lock.
    Verified { revision: String },
    /// Skipped, a conflict or the retired CA at `revision`: a cleanup
    /// removes this revision only if the user confirmed it from the import
    /// result of this receipt.
    Reviewable { revision: String },
    /// A cleanup removed the item or found it gone.
    Cleaned,
}

/// The import receipt. It never holds a value or a digest of a value.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    /// Identifies exactly one import, even if a user moved a previous receipt away.
    generation: uuid::Uuid,
    items: BTreeMap<String, Record>,
}

/// The receipt format. Version 1 had no revisions, so it allows nothing.
const FORMAT: u32 = 2;
const PENDING: &str = "legacy-keychain.pending";

#[derive(Deserialize)]
struct Head {
    version: u32,
}

/// The receipt. `None`: no import saved one. A file that is not a valid
/// receipt of a known version is an error: no step changes it.
fn load(data: &Path) -> Result<Option<Receipt>> {
    let path = data.join(FILE);
    let Some(mut file) = store::open_data_file(data, FILE)? else { return Ok(None) };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    // The serde message is not shown: only the file and the fix.
    let invalid = || {
        Error::LegacyKeychain(format!(
            "{} is not a receipt that this version can read, so zwinger does not use or change it; move it away, then run Import from Keychain again",
            path.display()
        ))
    };
    match serde_json::from_slice::<Head>(&bytes).map_err(|_| invalid())?.version {
        1 => Ok(None),
        FORMAT => {
            let receipt = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
            Ok(store::open_data_file(data, PENDING)?.is_none().then_some(receipt))
        }
        _ => Err(invalid()),
    }
}

/// A renamed receipt can be visible even if syncing its directory failed. Block its use
/// before publishing it, and only unlock it once the receipt is durable. A crash can
/// leave the guard in place, in which case a new import is required.
fn activate_receipt(data: &Path, publish: impl FnOnce() -> Result<()>) -> Result<()> {
    store::write_synced(data, PENDING, &())?;
    publish()?;
    std::fs::remove_file(data.join(PENDING))?;
    Ok(())
}

/// What Settings shows with no Keychain access.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    /// An import saved a receipt.
    pub ran: bool,
    /// Imported and still in the Keychain at the import.
    pub imported: Vec<String>,
    pub cleaned: Vec<String>,
    /// The items that a cleanup can check and remove.
    pub pending: Vec<String>,
}

/// Reads only the receipt. It never touches the Keychain.
pub fn status(data: &Path) -> Result<StatusView> {
    let receipt = load(data)?;
    let ran = receipt.is_some();
    let items = receipt.map(|r| r.items).unwrap_or_default();
    let with = |want: fn(&Record) -> bool| -> Vec<String> {
        items.iter().filter(|(_, record)| want(record)).map(|(account, _)| account.clone()).collect()
    };
    let verified = with(|r| matches!(r, Record::Verified { .. }));
    Ok(StatusView { ran, imported: verified.clone(), cleaned: with(|r| *r == Record::Cleaned), pending: verified })
}

/// Resolves an old `<project id>/<NAME>` account against the current cages.
/// The conversion of #118 gave each old project a cage with the same id, so
/// the project id is the cage id. A name has no `/`, so the last `/` ends
/// the id, as in the vault. An account never maps to the library.
pub fn classify(account: &str, cages: &[Cage]) -> Class {
    if account == RETIRED_CA {
        return Class::RetiredCa;
    }
    let Some((id, name)) = account.rsplit_once('/') else {
        return Class::Skipped(Reason::Format);
    };
    if id.is_empty() || secrets::check_name(name).ok().as_deref() != Some(name) {
        return Class::Skipped(Reason::Format);
    }
    let Some(cage) = cages.iter().find(|c| c.id == id) else {
        return Class::Skipped(Reason::NoCage);
    };
    if cage.network.uses(&CredentialRef { source: Source::Local, name: name.to_owned() }) {
        return Class::Importable { cage: id.to_owned(), name: name.to_owned() };
    }
    if cage.network.uses(&CredentialRef::library(name)) {
        return Class::Skipped(Reason::Library);
    }
    if cage.logins.iter().any(|login| login.name == name && !login.origin.is_user()) {
        return Class::Skipped(Reason::HostLogin);
    }
    Class::Skipped(Reason::NoCredential)
}

fn classify_now<R: SandboxRuntime>(manager: &Manager<R>, account: &str) -> Class {
    classify(account, &lock(&manager.state).cages)
}

/// The listed accounts, sorted, and the accounts of more than one item.
struct Listing {
    accounts: BTreeSet<String>,
    ambiguous: BTreeSet<String>,
}

fn listed(source: &dyn LegacySource) -> Result<Listing> {
    let mut listing = Listing { accounts: BTreeSet::new(), ambiguous: BTreeSet::new() };
    for account in source.accounts()? {
        if !listing.accounts.insert(account.clone()) {
            listing.ambiguous.insert(account);
        }
    }
    Ok(listing)
}

/// Lists the accounts and resolves them. Reads no value, changes nothing
/// and allows no cleanup.
pub(crate) fn scan<R: SandboxRuntime>(manager: &Manager<R>, source: &dyn LegacySource) -> Result<Vec<Row>> {
    let listing = listed(source)?;
    let state = lock(&manager.state);
    Ok(listing
        .accounts
        .iter()
        .map(|account| {
            if listing.ambiguous.contains(account) {
                Row::because(account, Outcome::Failed, Reason::Ambiguous)
            } else {
                Row::classified(account, &classify(account, &state.cages))
            }
        })
        .collect())
}

/// Copies the value of each importable item into the vault of its cage,
/// unless the vault already has a value, and saves a new receipt. Reads no
/// other value and removes no item. A denied or failed item does not stop
/// the others, and it loses what an earlier import allowed for it.
pub(crate) fn import<R: SandboxRuntime>(manager: &Manager<R>, source: &dyn LegacySource) -> Result<Report> {
    let prior = load(&manager.data)?;
    let generation = uuid::Uuid::new_v4();
    let mut prior_items = BTreeMap::new();
    if let Some(prior) = prior {
        prior_items = prior.items;
        // Revoke the previous import before any prompt can pause or interrupt this one.
        let empty = Receipt { version: FORMAT, generation, items: BTreeMap::new() };
        store::write_synced(&manager.data, FILE, &empty)?;
    }
    let listing = listed(source)?;
    // Only the record of a removed item that is still gone stays.
    let mut items: BTreeMap<String, Record> = prior_items
        .into_iter()
        .filter(|(account, record)| *record == Record::Cleaned && !listing.accounts.contains(account))
        .collect();
    let mut rows = Vec::with_capacity(listing.accounts.len());
    for account in &listing.accounts {
        let (row, record) = if listing.ambiguous.contains(account) {
            (Row::because(account, Outcome::Failed, Reason::Ambiguous), None)
        } else {
            import_one(manager, source, account)
        };
        if let Some(record) = record {
            items.insert(account.clone(), record);
        }
        rows.push(row);
    }
    let receipt = Receipt { version: FORMAT, generation, items };
    let saved = activate_receipt(&manager.data, || store::write_synced(&manager.data, FILE, &receipt)).is_ok();
    Ok(Report { rows, saved, generation: receipt.generation })
}

/// One item: its row, and what a cleanup can do with it.
fn import_one<R: SandboxRuntime>(
    manager: &Manager<R>,
    source: &dyn LegacySource,
    account: &str,
) -> (Row, Option<Record>) {
    let revision = match source.revision(account) {
        Ok(revision) => revision,
        Err(e) => return (Row::source_error(account, e, Outcome::Failed), None),
    };
    let review = |row: Row| (row, Some(Record::Reviewable { revision: revision.clone() }));
    if let class @ (Class::Skipped(_) | Class::RetiredCa) = classify_now(manager, account) {
        return review(Row::classified(account, &class));
    }
    let value = match source.read(account, &revision) {
        Ok(value) => value,
        Err(SourceError::NotText) => return review(Row::because(account, Outcome::Skipped, Reason::NotText)),
        Err(e) => return (Row::source_error(account, e, Outcome::Failed), None),
    };
    if value.expose().is_empty() {
        return review(Row::because(account, Outcome::Skipped, Reason::Empty));
    }
    let failed = || (Row::because(account, Outcome::Failed, Reason::Vault), None);
    // Under the lock of the credential edits, so no bind, unbind or edit
    // runs between the check and the write. The config can have changed
    // while macOS asked for access, so it is checked again.
    let _edit = lock(&manager.secret_edit);
    let (cage, name) = match classify_now(manager, account) {
        Class::Importable { cage, name } => (cage, name),
        other => return review(Row::classified(account, &other)),
    };
    let owner = Owner::cage(&cage);
    if manager.settle(&owner, &name).is_err() {
        return failed();
    }
    // A failed write is not undone: an earlier value can be there, and a
    // value that was written is equal to the item, so a retry finds `Same`.
    let outcome = match manager.vault.insert_new(&owner, &name, &value) {
        Ok(Inserted::New) => Outcome::Imported,
        Ok(Inserted::Same) => Outcome::Same,
        Ok(Inserted::Conflict) => return review(Row::new(account, Outcome::Conflict)),
        Err(_) => return failed(),
    };
    match manager.vault.durable_value(&owner, &name) {
        Ok(Some(found)) if found == value => {}
        _ => return failed(),
    }
    let key = CredentialRef { source: Source::Local, name };
    manager.publish(&cage, Some((&key, value)));
    (Row::new(account, outcome), Some(Record::Verified { revision }))
}

/// Removes the chosen items that the receipt allows. An item in `accounts`
/// or `confirmed` that the import verified goes only if its cage's vault
/// holds its value now. A reviewable item goes only if it is in `confirmed`
/// and `generation` is the receipt's import. Each removal names the revision
/// of the import. An item that is not there counts as done.
pub(crate) fn clean<R: SandboxRuntime>(
    manager: &Manager<R>,
    source: &dyn LegacySource,
    accounts: &[String],
    confirmed: &[String],
    generation: Option<uuid::Uuid>,
) -> Result<Report> {
    if accounts.len() + confirmed.len() > CLEAN_LIMIT {
        return Err(Error::LegacyKeychain(format!("select at most {CLEAN_LIMIT} items for one cleanup")));
    }
    let Some(mut receipt) = load(&manager.data)? else {
        return Err(Error::LegacyKeychain(
            "only a saved import allows a cleanup; run Import from Keychain first".into(),
        ));
    };
    if !confirmed.is_empty() && generation != Some(receipt.generation) {
        return Err(Error::LegacyKeychain(
            "the reviewed items are not from the last import; run Import from Keychain again".into(),
        ));
    }
    let listing = listed(source)?;
    let confirmed: BTreeSet<&str> = confirmed.iter().map(String::as_str).collect();
    let chosen: BTreeSet<&str> = accounts.iter().map(String::as_str).chain(confirmed.iter().copied()).collect();
    // Consume the selected approvals before starting irreversible work. A restart during a
    // macOS prompt then requires a fresh import for these items. Other approvals survive.
    let mut reserved = receipt.clone();
    reserved.items.retain(|account, record| !chosen.contains(account.as_str()) || *record == Record::Cleaned);
    if reserved.items.len() != receipt.items.len() {
        store::write_synced(&manager.data, FILE, &reserved)?;
    }
    let mut rows = Vec::with_capacity(chosen.len());
    for account in chosen {
        let record = receipt.items.get(account).cloned();
        let row = if listing.ambiguous.contains(account) {
            Row::because(account, Outcome::Kept, Reason::Ambiguous)
        } else if !listing.accounts.contains(account) {
            Row::new(account, Outcome::Missing)
        } else {
            match &record {
                Some(Record::Verified { revision }) => clean_verified(manager, source, account, revision),
                Some(Record::Reviewable { revision }) if confirmed.contains(account) => {
                    remove(source, account, revision, None)
                }
                Some(Record::Reviewable { .. }) => Row::because(account, Outcome::Kept, Reason::NotConfirmed),
                Some(Record::Cleaned) | None => Row::because(account, Outcome::Kept, Reason::NotImported),
            }
        };
        match (row.outcome, row.reason) {
            (Outcome::Cleaned | Outcome::Missing, _) if record.is_some() => {
                receipt.items.insert(account.to_owned(), Record::Cleaned);
            }
            // A changed or doubled item needs a new import.
            (_, Some(Reason::Changed | Reason::Ambiguous | Reason::NotImported)) => {
                receipt.items.remove(account);
            }
            _ => {}
        }
        rows.push(row);
    }
    let saved = activate_receipt(&manager.data, || store::write_synced(&manager.data, FILE, &receipt)).is_ok();
    Ok(Report { rows, saved, generation: receipt.generation })
}

fn clean_verified<R: SandboxRuntime>(
    manager: &Manager<R>,
    source: &dyn LegacySource,
    account: &str,
    revision: &str,
) -> Row {
    // Read before locking. A final macOS access prompt during deletion still holds the credential edit lock.
    let value = match source.read(account, revision) {
        Ok(value) => value,
        Err(e) => return Row::source_error(account, e, Outcome::Kept),
    };
    // Held through the delete, so no credential edit changes the config or
    // the vault value between the checks and the delete.
    let _edit = lock(&manager.secret_edit);
    let Class::Importable { cage, name } = classify_now(manager, account) else {
        return Row::because(account, Outcome::Kept, Reason::Changed);
    };
    let owner = Owner::cage(&cage);
    let failed = || Row::because(account, Outcome::Failed, Reason::Vault);
    if manager.settle(&owner, &name).is_err() {
        return failed();
    }
    match manager.vault.durable_value(&owner, &name) {
        Ok(Some(found)) if found == value => {}
        Ok(Some(_)) => return Row::because(account, Outcome::Kept, Reason::Changed),
        Ok(None) => return Row::because(account, Outcome::Kept, Reason::NotImported),
        Err(_) => return failed(),
    }
    remove(source, account, revision, Some(&value))
}

fn remove(source: &dyn LegacySource, account: &str, revision: &str, expected: Option<&SecretValue>) -> Row {
    match source.delete(account, revision, expected) {
        Ok(()) => Row::new(account, Outcome::Cleaned),
        Err(e) => Row::source_error(account, e, Outcome::Kept),
    }
}

/// The Keychain source of the app and the single-flight guard of its steps.
pub struct LegacyKeychain {
    source: Arc<dyn LegacySource>,
    busy: AtomicBool,
}

/// Ends a step: the next click can start one.
struct Release<'a>(&'a AtomicBool);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl LegacyKeychain {
    pub fn new(source: Arc<dyn LegacySource>) -> Self {
        Self { source, busy: AtomicBool::new(false) }
    }

    /// Runs one step at a time. A second click while a step runs, for
    /// example while macOS shows a prompt, gets [`Error::LegacyKeychainBusy`].
    fn exclusive<T>(&self, step: impl FnOnce(&dyn LegacySource) -> Result<T>) -> Result<T> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(Error::LegacyKeychainBusy);
        }
        let _release = Release(&self.busy);
        step(self.source.as_ref())
    }

    pub fn scan<R: SandboxRuntime>(&self, manager: &Manager<R>) -> Result<Vec<Row>> {
        self.exclusive(|source| scan(manager, source))
    }

    pub fn import<R: SandboxRuntime>(&self, manager: &Manager<R>) -> Result<Report> {
        self.exclusive(|source| import(manager, source))
    }

    pub fn clean<R: SandboxRuntime>(
        &self,
        manager: &Manager<R>,
        accounts: &[String],
        confirmed: &[String],
        generation: Option<uuid::Uuid>,
    ) -> Result<Report> {
        self.exclusive(|source| clean(manager, source, accounts, confirmed, generation))
    }
}

/// The source of the app. Making it touches no Keychain.
pub fn system_source() -> Arc<dyn LegacySource> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(fixture::ENV) {
        return Arc::new(fixture::FixtureSource(path.into()));
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(apple::AppleKeychain::default())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(NoSource)
    }
}

/// A system with no Keychain: no items.
#[cfg(not(target_os = "macos"))]
struct NoSource;

#[cfg(not(target_os = "macos"))]
impl LegacySource for NoSource {
    fn accounts(&self) -> std::result::Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn revision(&self, _: &str) -> std::result::Result<String, SourceError> {
        Err(SourceError::Missing)
    }

    fn read(&self, _: &str, _: &str) -> std::result::Result<SecretValue, SourceError> {
        Err(SourceError::Missing)
    }

    fn delete(&self, _: &str, _: &str, _: Option<&SecretValue>) -> std::result::Result<(), SourceError> {
        Err(SourceError::Missing)
    }
}

/// A debug-only source with synthetic items from a JSON file, for checks of
/// the real Settings controls and the Tauri boundary with no Keychain. Set
/// `ZWINGER_LEGACY_KEYCHAIN_FIXTURE` to the file:
/// `{"items": [{"account": "<account>", "revision": "<revision>", "value": "<value>"}], "deny": ["<account>"]}`.
/// Give a replaced item a new `revision`. Two items with one account are
/// ambiguous. A read or delete of a `deny` account acts as a canceled
/// macOS prompt. A delete writes the file again. Release builds do not have it.
#[cfg(debug_assertions)]
mod fixture {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use super::{ERR_DUPLICATE_ITEM, ERR_IO, ERR_USER_CANCELED, LegacySource, SecretValue, SourceError};

    pub const ENV: &str = "ZWINGER_LEGACY_KEYCHAIN_FIXTURE";

    #[derive(Serialize, Deserialize)]
    struct Item {
        account: String,
        revision: String,
        value: String,
    }

    #[derive(Default, Serialize, Deserialize)]
    struct Fixture {
        #[serde(default)]
        items: Vec<Item>,
        #[serde(default)]
        deny: BTreeSet<String>,
    }

    pub struct FixtureSource(pub PathBuf);

    impl FixtureSource {
        fn load(&self) -> Result<Fixture, SourceError> {
            let bytes = std::fs::read(&self.0).map_err(|_| SourceError::Failed(ERR_IO))?;
            serde_json::from_slice(&bytes).map_err(|_| SourceError::Failed(ERR_IO))
        }

        /// The index of the one item of `account`.
        fn find(fixture: &Fixture, account: &str) -> Result<usize, SourceError> {
            let mut found = fixture.items.iter().enumerate().filter(|(_, i)| i.account == account).map(|(n, _)| n);
            match (found.next(), found.next()) {
                (Some(n), None) => Ok(n),
                (None, _) => Err(SourceError::Missing),
                _ => Err(SourceError::Failed(ERR_DUPLICATE_ITEM)),
            }
        }

        /// The index of the item of `account` at `revision`, after the access check.
        fn at(fixture: &Fixture, account: &str, revision: &str) -> Result<usize, SourceError> {
            if fixture.deny.contains(account) {
                return Err(SourceError::Denied(ERR_USER_CANCELED));
            }
            let n = Self::find(fixture, account)?;
            if fixture.items[n].revision == revision { Ok(n) } else { Err(SourceError::Changed) }
        }
    }

    impl LegacySource for FixtureSource {
        fn accounts(&self) -> Result<Vec<String>, SourceError> {
            Ok(self.load()?.items.into_iter().map(|i| i.account).collect())
        }

        fn revision(&self, account: &str) -> Result<String, SourceError> {
            let fixture = self.load()?;
            Self::find(&fixture, account).map(|n| fixture.items[n].revision.clone())
        }

        fn read(&self, account: &str, revision: &str) -> Result<SecretValue, SourceError> {
            let fixture = self.load()?;
            Self::at(&fixture, account, revision).map(|n| SecretValue::new(fixture.items[n].value.clone()))
        }

        fn delete(&self, account: &str, revision: &str, expected: Option<&SecretValue>) -> Result<(), SourceError> {
            let mut fixture = self.load()?;
            let n = Self::at(&fixture, account, revision)?;
            if expected.is_some_and(|e| e.expose() != fixture.items[n].value) {
                return Err(SourceError::Changed);
            }
            fixture.items.remove(n);
            crate::store::write(&self.0, &fixture).map_err(|_| SourceError::Failed(ERR_IO))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_fixture_source_checks_revisions_denies_and_deletes_synthetic_items() {
            let path = crate::cage::testing::temp_dir().join("fixture.json");
            let json = r#"{"items":[
                {"account":"p/A","revision":"1","value":"synthetic-a"},
                {"account":"p/B","revision":"1","value":"synthetic-b"},
                {"account":"p/D","revision":"1","value":"synthetic-d"},
                {"account":"p/D","revision":"2","value":"synthetic-d"}],"deny":["p/B"]}"#;
            std::fs::write(&path, json).unwrap();
            let source = FixtureSource(path.clone());
            assert_eq!(source.accounts().unwrap(), ["p/A", "p/B", "p/D", "p/D"]);
            assert_eq!(source.revision("p/A").unwrap(), "1");
            assert_eq!(source.revision("p/D"), Err(SourceError::Failed(ERR_DUPLICATE_ITEM)));
            assert_eq!(source.read("p/A", "1").unwrap().expose(), "synthetic-a");
            assert_eq!(source.read("p/A", "0"), Err(SourceError::Changed));
            assert_eq!(source.read("p/B", "1"), Err(SourceError::Denied(ERR_USER_CANCELED)));
            assert_eq!(source.delete("p/B", "1", None), Err(SourceError::Denied(ERR_USER_CANCELED)));
            let other = SecretValue::new("synthetic-other");
            assert_eq!(source.delete("p/A", "1", Some(&other)), Err(SourceError::Changed));
            assert_eq!(source.delete("p/D", "1", None), Err(SourceError::Failed(ERR_DUPLICATE_ITEM)));
            source.delete("p/A", "1", Some(&SecretValue::new("synthetic-a"))).unwrap();
            assert_eq!(source.revision("p/A"), Err(SourceError::Missing));
            assert_eq!(FixtureSource(path).accounts().unwrap(), ["p/B", "p/D", "p/D"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::agent::AgentKind;
    use crate::cage::testing::{
        FakeRuntime, MemoryVault, Store, Vault, allow, bearer, credential, host, temp_dir, vault,
    };
    use crate::secrets::{Origin, Places, Secret};

    const A: &str = "SYNTHETIC-132-a";
    const B: &str = "SYNTHETIC-132-b";
    const NEWER: &str = "SYNTHETIC-132-newer";
    const CA: &str = "SYNTHETIC-132-ca";
    const PREFIX: &str = "SYNTHETIC-132-";

    struct Item {
        account: String,
        revision: String,
        value: String,
    }

    /// An in-memory Keychain service with revisions, twins, denials, and a
    /// log of each call. Each put makes a new revision, also with an equal value.
    #[derive(Default)]
    struct FakeSource {
        items: Mutex<Vec<Item>>,
        next: AtomicUsize,
        deny_list: AtomicBool,
        deny_reads: Mutex<BTreeSet<String>>,
        deny_deletes: Mutex<BTreeSet<String>>,
        lists: AtomicUsize,
        reads: Mutex<Vec<String>>,
        deletes: Mutex<Vec<String>>,
    }

    impl FakeSource {
        fn with(items: &[(&str, &str)]) -> Self {
            let source = Self::default();
            for (account, value) in items {
                source.put(account, value);
            }
            source
        }

        /// Replaces the item of `account`.
        fn put(&self, account: &str, value: &str) {
            self.items.lock().unwrap().retain(|i| i.account != account);
            self.twin(account, value);
        }

        /// Adds one more item with this account.
        fn twin(&self, account: &str, value: &str) {
            let revision = format!("rev-{}", self.next.fetch_add(1, Ordering::SeqCst));
            let item = Item { account: account.to_owned(), revision, value: value.to_owned() };
            self.items.lock().unwrap().push(item);
        }

        fn has(&self, account: &str) -> bool {
            self.items.lock().unwrap().iter().any(|i| i.account == account)
        }

        fn len(&self) -> usize {
            self.items.lock().unwrap().len()
        }

        fn reads(&self) -> Vec<String> {
            self.reads.lock().unwrap().clone()
        }

        fn deletes(&self) -> Vec<String> {
            self.deletes.lock().unwrap().clone()
        }

        /// The index of the one item of `account` at `revision`.
        fn at(items: &[Item], account: &str, revision: Option<&str>) -> std::result::Result<usize, SourceError> {
            let found: Vec<usize> = (0..items.len()).filter(|&n| items[n].account == account).collect();
            match found[..] {
                [] => Err(SourceError::Missing),
                [n] if revision.is_none_or(|r| r == items[n].revision) => Ok(n),
                [_] => Err(SourceError::Changed),
                _ => Err(SourceError::Failed(ERR_DUPLICATE_ITEM)),
            }
        }
    }

    impl LegacySource for FakeSource {
        fn accounts(&self) -> std::result::Result<Vec<String>, SourceError> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if self.deny_list.load(Ordering::SeqCst) {
                return Err(SourceError::Denied(ERR_USER_CANCELED));
            }
            Ok(self.items.lock().unwrap().iter().map(|i| i.account.clone()).collect())
        }

        fn revision(&self, account: &str) -> std::result::Result<String, SourceError> {
            let items = self.items.lock().unwrap();
            Self::at(&items, account, None).map(|n| items[n].revision.clone())
        }

        fn read(&self, account: &str, revision: &str) -> std::result::Result<SecretValue, SourceError> {
            self.reads.lock().unwrap().push(account.to_owned());
            if self.deny_reads.lock().unwrap().contains(account) {
                return Err(SourceError::Denied(ERR_INTERACTION_NOT_ALLOWED));
            }
            let items = self.items.lock().unwrap();
            Self::at(&items, account, Some(revision)).map(|n| SecretValue::new(items[n].value.clone()))
        }

        fn delete(
            &self,
            account: &str,
            revision: &str,
            expected: Option<&SecretValue>,
        ) -> std::result::Result<(), SourceError> {
            self.deletes.lock().unwrap().push(account.to_owned());
            if self.deny_deletes.lock().unwrap().contains(account) {
                return Err(SourceError::Denied(ERR_USER_CANCELED));
            }
            let mut items = self.items.lock().unwrap();
            let n = Self::at(&items, account, Some(revision))?;
            if expected.is_some_and(|e| e.expose() != items[n].value) {
                return Err(SourceError::Changed);
            }
            items.remove(n);
            Ok(())
        }
    }

    /// App data with a manager that a test can restart on the same files.
    struct World {
        root: PathBuf,
        vault: Arc<MemoryVault>,
        manager: Manager<FakeRuntime>,
    }

    impl World {
        fn new() -> Self {
            let root = temp_dir();
            let vault = vault();
            let manager = Self::boot(&root, &vault);
            Self { root, vault, manager }
        }

        fn boot(root: &Path, vault: &Arc<MemoryVault>) -> Manager<FakeRuntime> {
            Manager::new(FakeRuntime::default(), Store::new(root.to_owned()), vault.clone(), root.to_owned(), host())
                .unwrap()
        }

        /// A new app process on the same app data and vault.
        fn restart(&mut self) {
            self.manager = Self::boot(&self.root, &self.vault);
        }

        fn cage(&self) -> String {
            self.manager.create_cage("Synthetic").unwrap()
        }

        /// A local credential `name` of the cage with no stored value.
        fn local(&self, cage: &str, name: &str) {
            let host = format!("{}.fixture.invalid", name.to_lowercase().replace('_', "-"));
            credential(&self.manager, cage, &host, name, Some("seed")).unwrap();
            self.vault.delete(&Owner::cage(cage), name).unwrap();
        }

        fn value(&self, owner: Owner<'_>, name: &str) -> Option<String> {
            self.vault.get(&owner, name).unwrap().map(|v| v.expose().to_owned())
        }

        fn import(&self, source: &FakeSource) -> Report {
            let report = import(&self.manager, source).unwrap();
            assert_no_value(&report);
            report
        }

        fn try_clean(
            &self,
            source: &FakeSource,
            accounts: &[&str],
            confirmed: &[&str],
            generation: Option<uuid::Uuid>,
        ) -> Result<Report> {
            let owned = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
            clean(&self.manager, source, &owned(accounts), &owned(confirmed), generation)
        }

        fn clean(&self, source: &FakeSource, accounts: &[&str]) -> Report {
            let report = self.try_clean(source, accounts, &[], None).unwrap();
            assert_no_value(&report);
            report
        }

        fn confirm(&self, source: &FakeSource, confirmed: &[&str], generation: uuid::Uuid) -> Report {
            let report = self.try_clean(source, &[], confirmed, Some(generation)).unwrap();
            assert_no_value(&report);
            report
        }

        fn receipt_has_no_value(&self) {
            for entry in std::fs::read_dir(&self.root).unwrap().flatten() {
                if entry.path().is_file() {
                    let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                    assert!(!text.contains(PREFIX), "{} has a value", entry.path().display());
                }
            }
        }
    }

    fn assert_no_value(value: impl Serialize) {
        let json = serde_json::to_string(&value).unwrap();
        assert!(!json.contains(PREFIX), "a value crossed the boundary");
    }

    fn outcomes(report: &Report) -> Vec<(String, Outcome, Option<Reason>)> {
        report.rows.iter().map(|r| (r.account.clone(), r.outcome, r.reason)).collect()
    }

    fn sorted(mut rows: Vec<(String, Outcome, Option<Reason>)>) -> Vec<(String, Outcome, Option<Reason>)> {
        rows.sort();
        rows
    }

    fn account(cage: &str, name: &str) -> String {
        format!("{cage}/{name}")
    }

    fn login(name: &str) -> Secret {
        Secret {
            name: name.into(),
            hosts: vec!["api.openai.com".into()],
            places: Places { header_fields: vec!["authorization".into()], query: false, body: false },
            origin: Origin::HostLogin { agent: AgentKind::Codex },
        }
    }

    #[test]
    fn classify_resolves_the_old_account_format_against_the_config() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "LOCAL");
        world.manager.set_library_credential("SHARED", SecretValue::new(NEWER), false).unwrap();
        let rule = allow(&world.manager, &cage, "shared.fixture.invalid");
        world.manager.set_rule_credential(&cage, &rule, "SHARED", bearer(), Source::Library, None).unwrap();
        let mut cages = lock(&world.manager.state).cages.clone();
        cages[0].logins.push(login("OPENAI_LOGIN"));
        let mut nested = Cage::new("old/group".into(), "Nested".into());
        nested.network = cages[0].network.clone();
        cages.push(nested);

        let class = |account: &str| classify(account, &cages);
        assert_eq!(class(&account(&cage, "LOCAL")), Class::Importable { cage: cage.clone(), name: "LOCAL".into() });
        assert_eq!(class("old/group/LOCAL"), Class::Importable { cage: "old/group".into(), name: "LOCAL".into() });
        assert_eq!(class(&account(&cage, "SHARED")), Class::Skipped(Reason::Library));
        assert_eq!(class(&account(&cage, "OPENAI_LOGIN")), Class::Skipped(Reason::HostLogin));
        assert_eq!(class(&account(&cage, "GONE")), Class::Skipped(Reason::NoCredential));
        assert_eq!(class("unknown-project/LOCAL"), Class::Skipped(Reason::NoCage));
        for bad in ["malformed", "/LOCAL", "p/", "p/lower", "p/ LOCAL", "p/PATH", ""] {
            assert_eq!(class(bad), Class::Skipped(Reason::Format), "{bad:?}");
        }
        assert_eq!(class(RETIRED_CA), Class::RetiredCa);
        // The retired key stays retired, also next to a cage with the id `zwinger`.
        cages.push(Cage::new("zwinger".into(), "Named zwinger".into()));
        assert_eq!(classify(RETIRED_CA, &cages), Class::RetiredCa);
    }

    #[test]
    fn status_and_restarts_never_touch_the_keychain() {
        let mut world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let source = FakeSource::with(&[(&account(&cage, "KEY"), A)]);
        for _ in 0..2 {
            let view = status(&world.root).unwrap();
            assert!(!view.ran && view.imported.is_empty() && view.pending.is_empty());
            world.restart();
        }
        assert_eq!(source.lists.load(Ordering::SeqCst), 0);
        assert!(source.reads().is_empty() && source.deletes().is_empty());
        assert!(!world.root.join(FILE).exists());
        assert_eq!(world.value(Owner::cage(&cage), "KEY"), None);
    }

    #[test]
    fn a_scan_reads_no_value_changes_nothing_and_reports_twins() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let source = FakeSource::with(&[(&account(&cage, "KEY"), A), (RETIRED_CA, CA), ("unmapped/KEY", B)]);
        source.twin("unmapped/TWIN", A);
        source.twin("unmapped/TWIN", B);
        let rows = scan(&world.manager, &source).unwrap();
        assert_no_value(&rows);
        let got: Vec<_> = rows.iter().map(|r| (r.account.clone(), r.outcome, r.reason)).collect();
        let want = sorted(vec![
            (account(&cage, "KEY"), Outcome::Importable, None),
            ("unmapped/KEY".into(), Outcome::Skipped, Some(Reason::NoCage)),
            ("unmapped/TWIN".into(), Outcome::Failed, Some(Reason::Ambiguous)),
            (RETIRED_CA.into(), Outcome::RetiredCa, None),
        ]);
        assert_eq!(got, want);
        assert!(source.reads().is_empty() && source.deletes().is_empty());
        assert_eq!(world.value(Owner::cage(&cage), "KEY"), None);
        assert!(!world.root.join(FILE).exists());
    }

    /// No import: an equal vault value, a direct call and any generation
    /// remove nothing.
    #[test]
    fn a_scan_with_no_import_never_allows_a_cleanup() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        world.vault.set(&Owner::cage(&cage), "KEY", &SecretValue::new(A)).unwrap();
        let source = FakeSource::with(&[(&key, A), (RETIRED_CA, CA)]);
        scan(&world.manager, &source).unwrap();
        for generation in [None, Some(uuid::Uuid::nil()), Some(uuid::Uuid::new_v4())] {
            let error = world.try_clean(&source, &[&key], &[RETIRED_CA], generation).unwrap_err();
            assert_eq!(error.code(), "LegacyKeychain");
            assert!(world.try_clean(&source, &[&key], &[], generation).is_err());
        }
        assert_eq!(source.lists.load(Ordering::SeqCst), 1);
        assert!(source.reads().is_empty() && source.deletes().is_empty());
        assert_eq!(source.len(), 2);
    }

    #[test]
    fn a_canceled_listing_fails_with_the_status_only_and_can_be_retried() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let source = FakeSource::with(&[(&account(&cage, "KEY"), A)]);
        source.deny_list.store(true, Ordering::SeqCst);
        for result in [scan(&world.manager, &source).map(drop), import(&world.manager, &source).map(drop)] {
            let error = result.unwrap_err();
            assert_eq!(error.code(), "LegacyKeychain");
            assert!(error.message().contains("canceled"), "{}", error.message());
            assert_no_value(&error);
        }
        assert!(source.reads().is_empty() && source.deletes().is_empty());
        assert!(!world.root.join(FILE).exists());
        source.deny_list.store(false, Ordering::SeqCst);
        world.import(&source);
        assert_eq!(world.value(Owner::cage(&cage), "KEY").as_deref(), Some(A));
    }

    #[test]
    fn an_import_adds_new_values_and_keeps_every_other_value_and_item() {
        let world = World::new();
        let cage = world.cage();
        for name in ["FRESH", "SAME", "NEWER"] {
            world.local(&cage, name);
        }
        world.vault.set(&Owner::cage(&cage), "SAME", &SecretValue::new(B)).unwrap();
        world.vault.set(&Owner::cage(&cage), "NEWER", &SecretValue::new(NEWER)).unwrap();
        world.manager.set_library_credential("SHARED", SecretValue::new(NEWER), false).unwrap();
        let rule = allow(&world.manager, &cage, "shared.fixture.invalid");
        world.manager.set_rule_credential(&cage, &rule, "SHARED", bearer(), Source::Library, None).unwrap();
        let [fresh, same, newer, shared] = ["FRESH", "SAME", "NEWER", "SHARED"].map(|n| account(&cage, n));
        let source = FakeSource::with(&[
            (&fresh, A),
            (&same, B),
            (&newer, A),
            (&shared, A),
            ("absent-project/OLD", A),
            ("malformed", A),
            (RETIRED_CA, CA),
        ]);

        let report = world.import(&source);
        let want = sorted(vec![
            (fresh.clone(), Outcome::Imported, None),
            (same.clone(), Outcome::Same, None),
            (newer.clone(), Outcome::Conflict, None),
            (shared.clone(), Outcome::Skipped, Some(Reason::Library)),
            ("absent-project/OLD".into(), Outcome::Skipped, Some(Reason::NoCage)),
            ("malformed".into(), Outcome::Skipped, Some(Reason::Format)),
            (RETIRED_CA.into(), Outcome::RetiredCa, None),
        ]);
        assert_eq!(outcomes(&report), want);
        assert!(report.saved);
        assert!(!report.generation.is_nil());
        assert_eq!(world.value(Owner::cage(&cage), "FRESH").as_deref(), Some(A));
        assert_eq!(world.value(Owner::cage(&cage), "SAME").as_deref(), Some(B));
        assert_eq!(world.value(Owner::cage(&cage), "NEWER").as_deref(), Some(NEWER));
        assert_eq!(world.value(Owner::Library, "SHARED").as_deref(), Some(NEWER));
        assert_eq!(world.value(Owner::cage(&cage), "SHARED"), None);
        assert_eq!(world.value(Owner::cage("zwinger"), "PROXY_CA_KEY"), None);
        assert_eq!(world.value(Owner::Library, "PROXY_CA_KEY"), None);
        // Only the importable items are read. No item is removed.
        let mut read = source.reads();
        read.sort();
        assert_eq!(read, [fresh.clone(), newer.clone(), same.clone()]);
        assert!(source.deletes().is_empty());
        assert_eq!(source.len(), 7);

        let view = status(&world.root).unwrap();
        assert!(view.ran);
        assert_eq!(view.imported, [fresh.clone(), same.clone()]);
        assert_eq!(view.pending, [fresh, same]);
        world.receipt_has_no_value();
    }

    #[test]
    fn a_denied_or_missing_read_keeps_the_item_and_a_retry_imports_it() {
        let mut world = World::new();
        let cage = world.cage();
        for name in ["DENIED", "AVAILABLE", "EMPTY"] {
            world.local(&cage, name);
        }
        let [denied, available, empty] = ["DENIED", "AVAILABLE", "EMPTY"].map(|n| account(&cage, n));
        let source = FakeSource::with(&[(&denied, A), (&available, B), (&empty, "")]);
        source.deny_reads.lock().unwrap().insert(denied.clone());
        let report = world.import(&source);
        let denied_row = report.rows.iter().find(|r| r.account == denied).unwrap();
        assert_eq!((denied_row.outcome, denied_row.code), (Outcome::Denied, Some(ERR_INTERACTION_NOT_ALLOWED)));
        let empty_row = report.rows.iter().find(|r| r.account == empty).unwrap();
        assert_eq!((empty_row.outcome, empty_row.reason), (Outcome::Skipped, Some(Reason::Empty)));
        assert_eq!(world.value(Owner::cage(&cage), "DENIED"), None);
        assert_eq!(world.value(Owner::cage(&cage), "AVAILABLE").as_deref(), Some(B));
        assert!(source.has(&denied) && source.has(&available));

        // The item goes between the listing and the read.
        assert_eq!(import_one(&world.manager, &FakeSource::default(), &denied).0.outcome, Outcome::Missing);

        world.restart();
        source.deny_reads.lock().unwrap().clear();
        let report = world.import(&source);
        let outcome = |a: &str| report.rows.iter().find(|r| r.account == a).unwrap().outcome;
        assert_eq!((outcome(&denied), outcome(&available)), (Outcome::Imported, Outcome::Same));
        assert_eq!(world.value(Owner::cage(&cage), "DENIED").as_deref(), Some(A));
        assert!(source.deletes().is_empty());
    }

    /// A new import that cannot read an item, or does not find it, takes
    /// back what the import before allowed for it.
    #[test]
    fn a_new_import_revokes_the_items_that_it_cannot_verify() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "DENIED");
        world.local(&cage, "GONE");
        let [denied, gone] = ["DENIED", "GONE"].map(|n| account(&cage, n));
        let source = FakeSource::with(&[(&denied, A), (&gone, B)]);
        world.import(&source);
        assert_eq!(status(&world.root).unwrap().pending, [denied.clone(), gone.clone()]);

        source.deny_reads.lock().unwrap().insert(denied.clone());
        source.items.lock().unwrap().retain(|i| i.account != gone);
        world.import(&source);
        source.put(&gone, B);
        source.deny_reads.lock().unwrap().clear();
        assert!(status(&world.root).unwrap().pending.is_empty());
        let report = world.clean(&source, &[&denied, &gone]);
        let want = sorted(vec![
            (denied.clone(), Outcome::Kept, Some(Reason::NotImported)),
            (gone.clone(), Outcome::Kept, Some(Reason::NotImported)),
        ]);
        assert_eq!(outcomes(&report), want);
        assert!(source.deletes().is_empty() && source.len() == 2);
    }

    #[test]
    fn a_vault_failure_keeps_the_item_and_a_retry_imports_it() {
        let mut world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        let source = FakeSource::with(&[(&key, A)]);
        *world.vault.writes_left.lock().unwrap() = Some(0);
        let report = world.import(&source);
        assert_eq!(outcomes(&report), [(key.clone(), Outcome::Failed, Some(Reason::Vault))]);
        assert_eq!(world.value(Owner::cage(&cage), "KEY"), None);
        assert!(status(&world.root).unwrap().imported.is_empty());
        // No cleanup of an item that the import did not verify.
        let report = world.clean(&source, &[&key]);
        assert_eq!(outcomes(&report), [(key.clone(), Outcome::Kept, Some(Reason::NotImported))]);
        assert!(source.has(&key) && source.deletes().is_empty());

        *world.vault.writes_left.lock().unwrap() = None;
        world.restart();
        assert_eq!(outcomes(&world.import(&source)), [(key.clone(), Outcome::Imported, None)]);
        assert_eq!(outcomes(&world.clean(&source, &[&key])), [(key.clone(), Outcome::Cleaned, None)]);
        assert!(!source.has(&key));
        assert_eq!(world.value(Owner::cage(&cage), "KEY").as_deref(), Some(A));
    }

    /// A failed folder sync after the rename leaves a visible value that
    /// may not be durable. Neither the import nor the cleanup trusts it.
    #[test]
    fn a_visible_value_after_a_failed_sync_allows_no_cleanup() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        let source = FakeSource::with(&[(&key, A)]);
        world.vault.fail_after_write.store(true, Ordering::SeqCst);
        assert_eq!(outcomes(&world.import(&source)), [(key.clone(), Outcome::Failed, Some(Reason::Vault))]);
        assert_eq!(world.value(Owner::cage(&cage), "KEY").as_deref(), Some(A));

        world.vault.fail_sync.store(true, Ordering::SeqCst);
        assert_eq!(outcomes(&world.import(&source)), [(key.clone(), Outcome::Failed, Some(Reason::Vault))]);
        assert_eq!(outcomes(&world.clean(&source, &[&key])), [(key.clone(), Outcome::Kept, Some(Reason::NotImported))]);

        world.vault.fail_sync.store(false, Ordering::SeqCst);
        assert_eq!(outcomes(&world.import(&source)), [(key.clone(), Outcome::Same, None)]);
        world.vault.fail_sync.store(true, Ordering::SeqCst);
        assert_eq!(outcomes(&world.clean(&source, &[&key])), [(key.clone(), Outcome::Failed, Some(Reason::Vault))]);
        assert!(source.has(&key) && source.deletes().is_empty());

        world.vault.fail_sync.store(false, Ordering::SeqCst);
        assert_eq!(outcomes(&world.clean(&source, &[&key])), [(key.clone(), Outcome::Cleaned, None)]);
    }

    #[test]
    fn a_visible_receipt_that_failed_to_sync_cannot_authorize_cleanup_after_restart() {
        let mut world = World::new();
        let source = FakeSource::with(&[(RETIRED_CA, CA)]);
        let generation = world.import(&source).generation;
        let receipt = load(&world.root).unwrap().unwrap();
        let result = activate_receipt(&world.root, || {
            store::write(&world.root.join(FILE), &receipt)?;
            Err(std::io::Error::other("simulated post-rename directory sync failure").into())
        });
        assert!(result.is_err());
        world.restart();
        assert!(!status(&world.root).unwrap().ran);
        assert!(world.try_clean(&source, &[], &[RETIRED_CA], Some(generation)).is_err());
        assert!(source.deletes().is_empty());
        let fresh = world.import(&source).generation;
        assert_eq!(world.confirm(&source, &[RETIRED_CA], fresh).rows[0].outcome, Outcome::Cleaned);
    }

    #[test]
    fn a_new_import_revokes_old_approvals_before_the_first_prompt() {
        let mut world = World::new();
        let source = FakeSource::with(&[(RETIRED_CA, CA)]);
        let first = world.import(&source).generation;
        let calls = source.lists.load(Ordering::SeqCst);
        std::fs::set_permissions(&world.root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = import(&world.manager, &source);
        std::fs::set_permissions(&world.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(source.lists.load(Ordering::SeqCst), calls);

        source.deny_list.store(true, Ordering::SeqCst);
        assert!(import(&world.manager, &source).is_err());
        source.deny_list.store(false, Ordering::SeqCst);
        world.restart();
        assert!(world.try_clean(&source, &[], &[RETIRED_CA], Some(first)).is_err());
        assert!(source.has(RETIRED_CA));

        // Even manually moving a receipt away cannot make a previous confirmation valid again.
        std::fs::remove_file(world.root.join(FILE)).unwrap();
        let next = world.import(&source).generation;
        assert_ne!(first, next);
        assert!(world.try_clean(&source, &[], &[RETIRED_CA], Some(first)).is_err());
        assert!(source.deletes().is_empty());
    }

    #[test]
    fn a_failed_receipt_write_allows_no_cleanup_and_a_retry_saves_it() {
        let mut world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        let source = FakeSource::with(&[(&key, A)]);
        let mode = |mode| std::fs::set_permissions(&world.root, std::fs::Permissions::from_mode(mode)).unwrap();
        mode(0o500);
        let report = world.import(&source);
        mode(0o700);
        assert!(!report.saved);
        assert_eq!(outcomes(&report), [(key.clone(), Outcome::Imported, None)]);
        assert!(!status(&world.root).unwrap().ran);
        assert_eq!(world.try_clean(&source, &[&key], &[], None).unwrap_err().code(), "LegacyKeychain");
        assert!(source.has(&key) && source.deletes().is_empty());

        world.restart();
        let report = world.import(&source);
        assert!(report.saved);
        assert_eq!(outcomes(&report), [(key.clone(), Outcome::Same, None)]);
        assert_eq!(status(&world.root).unwrap().pending, [key.clone()]);
        assert_eq!(outcomes(&world.clean(&source, &[&key])), [(key.clone(), Outcome::Cleaned, None)]);
        assert!(!source.has(&key));
        world.receipt_has_no_value();
    }

    /// A receipt that is not a file, not valid or of a newer version stays
    /// as it is, and no step reads, imports or removes an item.
    #[test]
    fn an_unreadable_receipt_stops_each_step_and_stays() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        world.vault.set(&Owner::cage(&cage), "KEY", &SecretValue::new(A)).unwrap();
        let source = FakeSource::with(&[(&key, A)]);
        let path = world.root.join(FILE);
        let newer = format!(r#"{{"version":{},"generation":1,"items":{{}}}}"#, FORMAT + 1);
        let broken = r#"{"version":2,"generation":1,"items":{"p/KEY":{"state":"verified"}}}"#;
        for text in ["{", "[]", newer.as_str(), broken] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(status(&world.root).unwrap_err().code(), "LegacyKeychain");
            assert!(import(&world.manager, &source).is_err());
            assert!(world.try_clean(&source, &[&key], &[], None).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(status(&world.root).unwrap_err().code(), "StoreFile");
        assert!(import(&world.manager, &source).is_err());
        assert!(world.try_clean(&source, &[&key], &[], None).is_err());
        assert_eq!(source.lists.load(Ordering::SeqCst), 0);
        assert!(source.reads().is_empty() && source.deletes().is_empty());

        // A receipt of version 1 has no revisions: it allows nothing, and an import replaces it.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, format!(r#"{{"version":1,"imported":["{key}"],"cleaned":[]}}"#)).unwrap();
        assert!(!status(&world.root).unwrap().ran);
        assert!(world.try_clean(&source, &[&key], &[], None).is_err());
        assert_eq!(outcomes(&world.import(&source)), [(key.clone(), Outcome::Same, None)]);
        assert_eq!(status(&world.root).unwrap().pending, [key]);
    }

    #[test]
    fn a_cleanup_after_a_restart_removes_only_the_imported_revision_that_the_vault_holds() {
        let mut world = World::new();
        let cage = world.cage();
        let names = ["SOURCE_CHANGES", "DEST_CHANGES", "ROTATED", "REPLACED", "MATCHES"];
        for name in names {
            world.local(&cage, name);
        }
        let [source_changes, dest_changes, rotated, replaced, matches] = names.map(|n| account(&cage, n));
        let source =
            FakeSource::with(&[(&source_changes, A), (&dest_changes, B), (&rotated, A), (&replaced, A), (&matches, A)]);
        world.import(&source);
        world.restart();
        assert_eq!(status(&world.root).unwrap().pending.len(), 5);
        source.put(&source_changes, NEWER);
        world.vault.set(&Owner::cage(&cage), "DEST_CHANGES", &SecretValue::new(NEWER)).unwrap();
        // The item and the vault rotate together to one new value.
        source.put(&rotated, NEWER);
        world.vault.set(&Owner::cage(&cage), "ROTATED", &SecretValue::new(NEWER)).unwrap();
        // A new item with the same account and the same value.
        source.put(&replaced, A);

        let report = world.clean(&source, &[&source_changes, &dest_changes, &rotated, &replaced, &matches]);
        let want = sorted(vec![
            (source_changes.clone(), Outcome::Kept, Some(Reason::Changed)),
            (dest_changes.clone(), Outcome::Kept, Some(Reason::Changed)),
            (rotated.clone(), Outcome::Kept, Some(Reason::Changed)),
            (replaced.clone(), Outcome::Kept, Some(Reason::Changed)),
            (matches.clone(), Outcome::Cleaned, None),
        ]);
        assert_eq!(outcomes(&report), want);
        assert_eq!(source.len(), 4);
        assert!(!source.has(&matches));
        assert_eq!(world.value(Owner::cage(&cage), "SOURCE_CHANGES").as_deref(), Some(A));
        assert_eq!(world.value(Owner::cage(&cage), "DEST_CHANGES").as_deref(), Some(NEWER));
        // A changed item needs a new import.
        let view = status(&world.root).unwrap();
        assert_eq!(view.cleaned, [matches]);
        assert!(view.pending.is_empty());
        assert_eq!(outcomes(&world.clean(&source, &[&rotated]))[0].2, Some(Reason::NotImported));
    }

    /// A config edit while macOS asks for the read: the check under the
    /// edit lock sees it.
    #[test]
    fn a_cleanup_checks_the_config_again_under_the_edit_lock() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        let source = FakeSource::with(&[(&key, A)]);
        world.import(&source);
        let rule = {
            let state = lock(&world.manager.state);
            state.cage(&cage).unwrap().network.rules.iter().find(|r| !r.credentials.is_empty()).unwrap().id.clone()
        };
        world.manager.remove_rule(&cage, &rule).unwrap();
        world.vault.set(&Owner::cage(&cage), "KEY", &SecretValue::new(A)).unwrap();
        assert_eq!(outcomes(&world.clean(&source, &[&key])), [(key.clone(), Outcome::Kept, Some(Reason::Changed))]);
        assert!(source.has(&key) && source.deletes().is_empty());
    }

    #[test]
    fn twins_are_never_read_or_removed() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        world.local(&cage, "LATER");
        let [key, later] = ["KEY", "LATER"].map(|n| account(&cage, n));
        let source = FakeSource::with(&[(&later, A)]);
        source.twin(&key, A);
        source.twin(&key, B);
        let report = world.import(&source);
        assert_eq!(
            outcomes(&report),
            sorted(vec![
                (key.clone(), Outcome::Failed, Some(Reason::Ambiguous)),
                (later.clone(), Outcome::Imported, None),
            ])
        );
        assert_eq!(source.reads(), [later.clone()]);
        assert_eq!(world.value(Owner::cage(&cage), "KEY"), None);

        // A twin that comes after the import.
        source.twin(&later, A);
        let report = world.try_clean(&source, &[&key, &later], &[&key], Some(report.generation)).unwrap();
        assert_eq!(
            outcomes(&report),
            sorted(vec![
                (key.clone(), Outcome::Kept, Some(Reason::Ambiguous)),
                (later.clone(), Outcome::Kept, Some(Reason::Ambiguous)),
            ])
        );
        assert!(source.deletes().is_empty() && source.len() == 4);
        assert!(status(&world.root).unwrap().pending.is_empty());
    }

    #[test]
    fn skipped_conflicting_and_retired_items_go_only_when_confirmed_from_the_last_import() {
        let mut world = World::new();
        let cage = world.cage();
        world.local(&cage, "NEWER");
        world.local(&cage, "KEY");
        world.vault.set(&Owner::cage(&cage), "NEWER", &SecretValue::new(NEWER)).unwrap();
        let [newer, key] = ["NEWER", "KEY"].map(|n| account(&cage, n));
        let source =
            FakeSource::with(&[(&newer, A), (&key, A), ("unmapped/OLD", B), ("unmapped/EDITED", B), (RETIRED_CA, CA)]);
        let generation = world.import(&source).generation;
        assert!(!source.reads().iter().any(|a| a == RETIRED_CA));

        // Without a confirmation, nothing that the vault does not hold goes.
        let report = world.clean(&source, &[&newer, "unmapped/OLD", RETIRED_CA]);
        let want = sorted(vec![
            (newer.clone(), Outcome::Kept, Some(Reason::NotConfirmed)),
            ("unmapped/OLD".into(), Outcome::Kept, Some(Reason::NotConfirmed)),
            (RETIRED_CA.into(), Outcome::Kept, Some(Reason::NotConfirmed)),
        ]);
        assert_eq!(outcomes(&report), want);
        assert_eq!(source.len(), 5);
        // A confirmation of another import, or of none, is refused before any Keychain call.
        let lists = source.lists.load(Ordering::SeqCst);
        for stale in [None, Some(uuid::Uuid::nil()), Some(uuid::Uuid::new_v4())] {
            assert!(world.try_clean(&source, &[], &[RETIRED_CA], stale).is_err());
        }
        assert_eq!(source.lists.load(Ordering::SeqCst), lists);

        world.restart();
        let report = world.confirm(&source, &[RETIRED_CA], generation);
        assert_eq!(outcomes(&report), [(RETIRED_CA.to_owned(), Outcome::Cleaned, None)]);
        // A confirmation does not skip the check of a verified item.
        world.vault.set(&Owner::cage(&cage), "KEY", &SecretValue::new(NEWER)).unwrap();
        // An item that changed after the review needs a new import.
        source.put("unmapped/EDITED", NEWER);
        let report = world.confirm(&source, &[&key, &newer, "unmapped/OLD", "unmapped/EDITED"], generation);
        let want = sorted(vec![
            (key.clone(), Outcome::Kept, Some(Reason::Changed)),
            (newer.clone(), Outcome::Cleaned, None),
            ("unmapped/OLD".into(), Outcome::Cleaned, None),
            ("unmapped/EDITED".into(), Outcome::Kept, Some(Reason::Changed)),
        ]);
        assert_eq!(outcomes(&report), want);
        assert!(source.has(&key) && source.has("unmapped/EDITED") && source.len() == 2);
        let again = world.confirm(&source, &["unmapped/EDITED"], generation);
        assert_eq!(outcomes(&again), [("unmapped/EDITED".to_owned(), Outcome::Kept, Some(Reason::NotImported))]);
        // The confirmed removal of a conflict keeps the newer vault value.
        assert_eq!(world.value(Owner::cage(&cage), "NEWER").as_deref(), Some(NEWER));
        assert!(!source.reads().iter().any(|a| a == RETIRED_CA));
    }

    /// Only the retired CA: an import gives the result that a confirmation needs.
    #[test]
    fn an_import_of_only_the_retired_ca_allows_its_confirmed_removal() {
        let world = World::new();
        let source = FakeSource::with(&[(RETIRED_CA, CA)]);
        let report = world.import(&source);
        assert_eq!(outcomes(&report), [(RETIRED_CA.to_owned(), Outcome::RetiredCa, None)]);
        assert!(source.reads().is_empty());
        let report = world.confirm(&source, &[RETIRED_CA], report.generation);
        assert_eq!(outcomes(&report), [(RETIRED_CA.to_owned(), Outcome::Cleaned, None)]);
        assert_eq!(status(&world.root).unwrap().cleaned, [RETIRED_CA]);
        assert!(source.reads().is_empty() && source.len() == 0);
    }

    #[test]
    fn a_partial_cleanup_can_be_retried_and_a_gone_item_counts_as_done() {
        let mut world = World::new();
        let cage = world.cage();
        world.local(&cage, "ALPHA");
        world.local(&cage, "BETA");
        let [alpha, beta] = ["ALPHA", "BETA"].map(|n| account(&cage, n));
        let source = FakeSource::with(&[(&alpha, A), (&beta, B), ("unmapped/COPY", NEWER)]);
        world.import(&source);
        world.import(&source);
        source.deny_deletes.lock().unwrap().insert(beta.clone());
        let report = world.clean(&source, &[&alpha, &beta]);
        let beta_row = report.rows.iter().find(|r| r.account == beta).unwrap();
        assert_eq!((beta_row.outcome, beta_row.code), (Outcome::Denied, Some(ERR_USER_CANCELED)));
        assert!(!source.has(&alpha) && source.has(&beta));
        assert_eq!(status(&world.root).unwrap().pending, [beta.clone()]);

        world.restart();
        source.deny_deletes.lock().unwrap().clear();
        let report = world.clean(&source, &[&alpha, &beta]);
        let want = sorted(vec![(alpha.clone(), Outcome::Missing, None), (beta.clone(), Outcome::Cleaned, None)]);
        assert_eq!(outcomes(&report), want);
        assert!(source.has("unmapped/COPY"));
        let view = status(&world.root).unwrap();
        assert!(view.pending.is_empty());
        assert_eq!(view.cleaned, [alpha.clone(), beta.clone()]);
        // A new import keeps the removed items as removed.
        world.import(&source);
        assert_eq!(status(&world.root).unwrap().cleaned, [alpha, beta]);
        assert_eq!(world.value(Owner::cage(&cage), "ALPHA").as_deref(), Some(A));
        assert_eq!(world.value(Owner::cage(&cage), "BETA").as_deref(), Some(B));
        world.receipt_has_no_value();
    }

    #[test]
    fn imported_values_survive_the_startup_prune_and_detached_items_stay_in_the_keychain() {
        let mut world = World::new();
        let cage = world.cage();
        let detached = world.cage();
        world.local(&cage, "KEY");
        world.local(&detached, "OLD");
        let rule = {
            let state = lock(&world.manager.state);
            let cage = state.cage(&detached).unwrap();
            cage.network.rules.iter().find(|r| !r.credentials.is_empty()).unwrap().id.clone()
        };
        world.manager.remove_rule(&detached, &rule).unwrap();
        let [key, old, deleted] = [account(&cage, "KEY"), account(&detached, "OLD"), "deleted-cage/KEY".to_owned()];
        let source = FakeSource::with(&[(&key, A), (&old, B), (&deleted, B)]);
        let report = world.import(&source);
        let want = sorted(vec![
            (key.clone(), Outcome::Imported, None),
            (old.clone(), Outcome::Skipped, Some(Reason::NoCredential)),
            (deleted.clone(), Outcome::Skipped, Some(Reason::NoCage)),
        ]);
        assert_eq!(outcomes(&report), want);

        world.restart();
        world.restart();
        assert_eq!(world.value(Owner::cage(&cage), "KEY").as_deref(), Some(A));
        assert_eq!(world.value(Owner::cage(&detached), "OLD"), None);
        assert!(source.has(&key) && source.has(&old) && source.has(&deleted));
        assert!(source.deletes().is_empty());
    }

    #[test]
    fn a_concurrent_value_wins_over_the_import() {
        let world = World::new();
        let cage = world.cage();
        world.local(&cage, "KEY");
        let key = account(&cage, "KEY");
        let source = FakeSource::with(&[(&key, A)]);
        // The user saves a value after the listing, while macOS asks for access.
        assert_eq!(classify_now(&world.manager, &key), Class::Importable { cage: cage.clone(), name: "KEY".into() });
        world.vault.set(&Owner::cage(&cage), "KEY", &SecretValue::new(NEWER)).unwrap();
        assert_eq!(import_one(&world.manager, &source, &key).0.outcome, Outcome::Conflict);
        assert_eq!(world.value(Owner::cage(&cage), "KEY").as_deref(), Some(NEWER));
    }

    #[test]
    fn one_step_runs_at_a_time() {
        let world = World::new();
        let legacy = LegacyKeychain::new(Arc::new(FakeSource::default()));
        let inner = legacy.exclusive(|_| Ok(legacy.scan(&world.manager)));
        assert!(matches!(inner, Ok(Err(Error::LegacyKeychainBusy))));
        assert!(matches!(legacy.import(&world.manager).map(|r| r.rows.len()), Ok(0)));
        // A failed step also ends: the next click runs.
        let failed: Result<()> = legacy.exclusive(|_| Err(SourceError::Failed(ERR_IO).into()));
        assert!(failed.is_err());
        assert!(legacy.scan(&world.manager).is_ok());
        assert_eq!(Error::LegacyKeychainBusy.code(), "LegacyKeychainBusy");
    }

    #[test]
    fn a_cleanup_refuses_too_many_accounts_before_any_keychain_call() {
        let world = World::new();
        let source = FakeSource::default();
        let many: Vec<String> = (0..=CLEAN_LIMIT).map(|i| format!("p/K{i}")).collect();
        assert_eq!(clean(&world.manager, &source, &many, &[], None).unwrap_err().code(), "LegacyKeychain");
        assert_eq!(source.lists.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn errors_name_the_status_only() {
        assert_eq!(SourceError::from_status(ERR_ITEM_NOT_FOUND), SourceError::Missing);
        for code in [ERR_USER_CANCELED, ERR_AUTH_FAILED, ERR_INTERACTION_NOT_ALLOWED] {
            assert_eq!(SourceError::from_status(code), SourceError::Denied(code));
        }
        assert_eq!(SourceError::from_status(-34018), SourceError::Failed(-34018));
        assert!(Error::from(SourceError::Failed(-34018)).message().contains("OSStatus -34018"));
        assert!(Error::from(SourceError::Denied(ERR_INTERACTION_NOT_ALLOWED)).message().contains("locked"));
        assert_eq!(
            Row::source_error("p/K", SourceError::Changed, Outcome::Failed),
            Row::because("p/K", Outcome::Kept, Reason::Changed)
        );
    }

    /// The Apple adapter is inspected as source, not run: a test never
    /// starts a Keychain query, a prompt or a change of a Keychain.
    #[test]
    fn the_apple_adapter_finds_items_of_the_old_service_only_and_changes_no_keychain_setting() {
        let adapter = include_str!("legacy_keychain/apple.rs");
        assert!(adapter.contains("impl LegacySource for AppleKeychain"));
        assert!(adapter.contains("SERVICE"));
        assert!(adapter.contains("generic_password"));
        // The by-account calls can find an item of another Keychain than the one that was checked.
        for forbidden in [
            "get_generic_password",
            "delete_generic_password",
            "set_generic_password",
            "set_search_list",
            "set_default",
            "set_access",
            "eprintln",
            "println",
            "dbg!",
        ] {
            assert!(!adapter.contains(forbidden), "{forbidden}");
        }
        assert_eq!(SERVICE, "zwinger");
    }
}
