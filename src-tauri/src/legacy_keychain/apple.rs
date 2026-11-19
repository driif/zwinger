//! The file Keychain used by the old keyring macOS backend. Listing reads attributes only.
//! Each operation keeps the selected user-domain Keychain and works on exact item references.

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use core_foundation::base::{CFType, TCFType};
use core_foundation::data::{CFData, CFDataRef};
use core_foundation::date::CFDate;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};
use security_framework::os::macos::keychain::SecKeychain;
#[cfg(not(test))]
use security_framework::os::macos::keychain::SecPreferencesDomain;
use security_framework::os::macos::keychain_item::SecKeychainItem;
use security_framework_sys::base::{SecKeychainAttributeList, SecKeychainItemRef};
use security_framework_sys::item::{kSecAttrAccount, kSecAttrService, kSecValueRef};
use security_framework_sys::keychain_item::{SecKeychainItemDelete, SecKeychainItemFreeContent};
use sha2::{Digest, Sha256};

use super::{LegacySource, SERVICE, SecretValue, SourceError};

// Public Security.framework interfaces from SecItem.h and SecKeychainItem.h missing in the sys crate.
unsafe extern "C" {
    static kSecAttrModificationDate: CFStringRef;
    fn SecKeychainItemCreatePersistentReference(item: SecKeychainItemRef, persistent: *mut CFDataRef) -> i32;
    fn SecKeychainItemCopyContent(
        item: SecKeychainItemRef,
        class: *mut u32,
        attributes: *mut SecKeychainAttributeList,
        length: *mut u32,
        data: *mut *mut c_void,
    ) -> i32;
}

type Result<T> = std::result::Result<T, SourceError>;

fn check(code: i32) -> Result<()> {
    if code == 0 { Ok(()) } else { Err(SourceError::from_status(code)) }
}

fn field(dict: &CFDictionary, key: CFStringRef) -> Result<CFType> {
    let ptr = dict.find(key.cast::<c_void>()).ok_or(SourceError::Changed)?;
    // SAFETY: Security returned a dictionary with CFType keys and values. Retain the value out of the dictionary.
    Ok(unsafe { CFType::wrap_under_get_rule(*ptr) })
}

struct Item {
    handle: SecKeychainItem,
    modified: f64,
    revision: String,
}

impl Item {
    fn from_attrs(dict: &CFDictionary) -> Result<Self> {
        // SAFETY: these are the Security.framework constants for the returned reference and modification date.
        let value = field(dict, unsafe { kSecValueRef })?;
        if value.type_of() != SecKeychainItem::type_id() {
            return Err(SourceError::Changed);
        }
        // SAFETY: the type id was checked above and the new handle retains the item.
        let handle = unsafe { SecKeychainItem::wrap_under_get_rule(value.as_CFTypeRef().cast_mut().cast()) };
        let modified = field(dict, unsafe { kSecAttrModificationDate })?
            .downcast::<CFDate>()
            .ok_or(SourceError::Changed)?
            .abs_time();
        if !modified.is_finite() {
            return Err(SourceError::Changed);
        }
        let mut persistent = ptr::null();
        // SAFETY: handle is retained, and the out pointer receives one owned CFData on success.
        check(unsafe { SecKeychainItemCreatePersistentReference(handle.as_concrete_TypeRef(), &mut persistent) })?;
        if persistent.is_null() {
            return Err(SourceError::Changed);
        }
        let persistent = unsafe { CFData::wrap_under_create_rule(persistent) };
        let mut hash = Sha256::new();
        hash.update(modified.to_bits().to_be_bytes());
        hash.update(persistent.bytes());
        let revision = format!("{:x}", hash.finalize());
        Ok(Self { handle, modified, revision })
    }

    /// Reads the retained item, even if another item with the same account is added during an access prompt.
    fn bytes(&self) -> Result<Vec<u8>> {
        let mut data = ptr::null_mut();
        let mut len = 0;
        // SAFETY: Security writes the length and its allocated buffer. We request no class or attributes.
        check(unsafe {
            SecKeychainItemCopyContent(
                self.handle.as_concrete_TypeRef(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut len,
                &mut data,
            )
        })?;
        if data.is_null() {
            return if len == 0 { Ok(Vec::new()) } else { Err(SourceError::Changed) };
        }
        // SAFETY: the allocated buffer contains len bytes until the matching free.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len as usize) }.to_vec();
        check(unsafe { SecKeychainItemFreeContent(ptr::null_mut(), data) })?;
        Ok(bytes)
    }
}

#[derive(Default)]
pub(super) struct AppleKeychain {
    /// Replaced when a user action lists accounts. The importer's exclusive guard spans the whole action.
    selected: Mutex<Option<SecKeychain>>,
    #[cfg(test)]
    fixture: Option<SecKeychain>,
}

impl AppleKeychain {
    #[cfg(test)]
    pub(super) fn for_keychain(keychain: SecKeychain) -> Self {
        Self { selected: Mutex::new(Some(keychain.clone())), fixture: Some(keychain) }
    }

    fn select(&self) -> Result<SecKeychain> {
        #[cfg(test)]
        {
            // A unit test without an explicit fixture must never fall back to the developer's Keychain.
            self.fixture.clone().ok_or(SourceError::Changed)
        }
        #[cfg(not(test))]
        {
            SecKeychain::default_for_domain(SecPreferencesDomain::User).map_err(|e| SourceError::from_status(e.code()))
        }
    }

    fn selected(&self) -> Result<SecKeychain> {
        self.selected.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or(SourceError::Changed)
    }

    fn query(keychain: &SecKeychain, account: Option<&str>) -> Result<Vec<CFDictionary>> {
        let mut query = ItemSearchOptions::new();
        query
            .keychains(std::slice::from_ref(keychain))
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .load_attributes(true)
            .limit(Limit::All);
        if let Some(account) = account {
            query.account(account).load_refs(true);
        }
        match query.search() {
            Ok(rows) => rows
                .into_iter()
                .map(|row| match row {
                    SearchResult::Dict(dict) => Ok(dict),
                    _ => Err(SourceError::Changed),
                })
                .collect(),
            Err(e) if SourceError::from_status(e.code()) == SourceError::Missing => Ok(Vec::new()),
            Err(e) => Err(SourceError::from_status(e.code())),
        }
    }

    fn account(dict: &CFDictionary) -> Result<String> {
        // SAFETY: these constants identify CFString attributes of generic passwords.
        let service = field(dict, unsafe { kSecAttrService })?.downcast::<CFString>().ok_or(SourceError::Changed)?;
        if service != SERVICE {
            return Err(SourceError::Changed);
        }
        field(dict, unsafe { kSecAttrAccount })?
            .downcast::<CFString>()
            .map(|s| s.to_string())
            .ok_or(SourceError::Changed)
    }

    fn item(&self, account: &str) -> Result<Item> {
        let rows = Self::query(&self.selected()?, Some(account))?;
        match rows.as_slice() {
            [] => Err(SourceError::Missing),
            [dict] if Self::account(dict)? == account => Item::from_attrs(dict),
            _ => Err(SourceError::Changed),
        }
    }

    fn verified(&self, account: &str, revision: &str) -> Result<Item> {
        let item = self.item(account)?;
        if revision != item.revision {
            return Err(SourceError::Changed);
        }
        Ok(item)
    }
}

impl LegacySource for AppleKeychain {
    fn accounts(&self) -> Result<Vec<String>> {
        let keychain = self.select()?;
        *self.selected.lock().unwrap_or_else(|e| e.into_inner()) = Some(keychain.clone());
        // Keep one name per physical item. The importer reports duplicate names and skips those accounts.
        // Entries without a string account cannot identify an app credential.
        Ok(Self::query(&keychain, None)?.iter().filter_map(|row| Self::account(row).ok()).collect())
    }

    fn revision(&self, account: &str) -> Result<String> {
        let item = self.item(account)?;
        // Legacy file Keychains store modification times in seconds. Only issue a revision once that second
        // is over, so an edit after the user saw this revision cannot share its timestamp.
        let remaining = item.modified.floor() + 1.0 - CFDate::now().abs_time();
        if remaining <= 0.0 {
            return Ok(item.revision);
        }
        if remaining > 1.0 {
            return Err(SourceError::Changed);
        }
        std::thread::sleep(Duration::from_secs_f64(remaining));
        let item = self.verified(account, &item.revision)?;
        if item.modified.floor() + 1.0 > CFDate::now().abs_time() {
            return Err(SourceError::Changed);
        }
        Ok(item.revision)
    }

    fn read(&self, account: &str, revision: &str) -> Result<SecretValue> {
        let item = self.verified(account, revision)?;
        let bytes = item.bytes()?;
        // A macOS prompt could have allowed time for the account to change.
        self.verified(account, revision)?;
        String::from_utf8(bytes).map(SecretValue::new).map_err(|_| SourceError::NotText)
    }

    fn delete(&self, account: &str, revision: &str, expected: Option<&SecretValue>) -> Result<()> {
        let item = self.verified(account, revision)?;
        if let Some(expected) = expected {
            if item.bytes()? != expected.expose().as_bytes() {
                return Err(SourceError::Changed);
            }
            self.verified(account, revision)?;
        }
        // The legacy API has no atomic compare-value-and-delete. Another program can edit the same item
        // between this last check and deletion. Never re-resolve the account for the delete itself.
        // SAFETY: the item comes from the scoped query and remains retained across the status-returning delete.
        check(unsafe { SecKeychainItemDelete(item.handle.as_concrete_TypeRef()) })
    }
}
