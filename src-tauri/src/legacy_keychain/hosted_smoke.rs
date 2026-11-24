// This file is included only in macOS unit tests. The normal test suite skips it.
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use security_framework::os::macos::keychain::SecKeychain;
use sha2::{Digest, Sha256};

use super::{LegacySource, Outcome, SERVICE, SecretValue, SourceError, apple, clean, import};
use crate::cage::testing::{Owner, Vault, credential, manager_with_vault};

fn empty_fixture(name: &str) -> PathBuf {
    let var = |name| std::env::var(name).unwrap_or_default();
    assert_eq!(var("GITHUB_ACTIONS"), "true", "hosted CI only");
    assert_eq!(var("RUNNER_OS"), "macOS", "hosted macOS CI only");
    assert_eq!(var("ZWINGER_HOSTED_RUNNER"), "github-hosted", "requires the hosted CI step");
    assert!(var("GITHUB_RUN_ID").parse::<u64>().is_ok_and(|id| id > 0));
    let supplied = PathBuf::from(var("ZWINGER_HOSTED_KEYCHAIN_FIXTURE"));
    assert_eq!(supplied.file_name().unwrap(), "fixture.keychain-db");
    let path = supplied.with_file_name(name);
    let runner_tmp = PathBuf::from(var("RUNNER_TEMP")).canonicalize().unwrap();
    let parent = path.parent().unwrap();
    assert_eq!(parent.parent(), Some(runner_tmp.as_path()));
    assert!(parent.file_name().unwrap().to_str().unwrap().starts_with("zwinger-legacy-smoke-"));
    assert_eq!(path.file_name().unwrap(), name);
    assert_eq!(path.canonicalize().unwrap(), path);
    let meta = std::fs::symlink_metadata(&path).unwrap();
    let directory = std::fs::symlink_metadata(parent).unwrap();
    assert!(meta.is_file() && directory.is_dir());
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!(directory.mode() & 0o777, 0o700);
    assert_eq!(meta.uid(), directory.uid());
    assert_eq!(meta.len(), 20_460);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        "e91b3a68e61aea2ee825ab3140ef03749ac9efdf0a5837cf887b31a3f5df5d04"
    );
    path
}

#[test]
#[ignore = "requires the pinned empty Apple fixture on a GitHub-hosted macOS runner"]
fn only_the_fixture_service_is_imported_and_removed() {
    let path = empty_fixture("fixture.keychain-db");
    let other_path = empty_fixture("other.keychain-db");
    assert_ne!(std::fs::metadata(&path).unwrap().ino(), std::fs::metadata(&other_path).unwrap().ino());
    let _no_ui = SecKeychain::disable_user_interaction().unwrap();
    let mut fixture = SecKeychain::open(&path).unwrap();
    fixture.unlock(Some("password")).unwrap(); // Apple's public empty test fixture.
    let mut other_keychain = SecKeychain::open(&other_path).unwrap();
    other_keychain.unlock(Some("password")).unwrap();

    let (manager, _, cage, vault) = manager_with_vault();
    credential(&manager, &cage, "fixture.invalid", "EXAMPLE", Some("seed")).unwrap();
    vault.delete(&Owner::cage(&cage), "EXAMPLE").unwrap();
    let account = format!("{cage}/EXAMPLE");
    let sentinel_service = "zwinger-hosted-test-other-service";
    fixture.add_generic_password(SERVICE, &account, b"fixture-import").unwrap();
    other_keychain.add_generic_password(SERVICE, &account, b"separate-copy").unwrap();
    fixture.add_generic_password(sentinel_service, &account, b"fixture-sentinel").unwrap();
    fixture.add_generic_password(sentinel_service, "other/account", b"fixture-other").unwrap();

    let source = apple::AppleKeychain::for_keychain(fixture.clone());
    assert_eq!(source.accounts().unwrap(), std::slice::from_ref(&account));
    let result = import(&manager, &source).unwrap();
    assert!(result.saved && result.rows.len() == 1 && result.rows[0].outcome == Outcome::Imported);
    assert!(vault.get(&Owner::cage(&cage), "EXAMPLE").unwrap().is_some_and(|v| v.expose() == "fixture-import"));
    let result = clean(&manager, &source, std::slice::from_ref(&account), &[], Some(result.generation)).unwrap();
    assert!(result.saved && result.rows.len() == 1 && result.rows[0].outcome == Outcome::Cleaned);
    assert!(source.accounts().unwrap().is_empty());
    let remaining = fixture.find_generic_password(sentinel_service, &account).unwrap().0;
    assert!(remaining.as_ref() == b"fixture-sentinel", "other-service synthetic item changed");
    let other = fixture.find_generic_password(sentinel_service, "other/account").unwrap().0;
    assert!(other.as_ref() == b"fixture-other");
    assert!(other_keychain.find_generic_password(SERVICE, &account).unwrap().0.as_ref() == b"separate-copy");

    // Persisted approvals belong to a particular revision, including across adapter restarts.
    fixture.add_generic_password(SERVICE, &account, b"first").unwrap();
    let old = source.revision(&account).unwrap();
    let restarted = apple::AppleKeychain::for_keychain(fixture.clone());
    assert_eq!(restarted.accounts().unwrap(), std::slice::from_ref(&account));
    assert_eq!(restarted.revision(&account).unwrap(), old);
    assert_eq!(restarted.delete(&account, &old, Some(&SecretValue::new("different"))), Err(SourceError::Changed));
    fixture.find_generic_password(SERVICE, &account).unwrap().1.set_password(b"changed").unwrap();
    assert_eq!(restarted.delete(&account, &old, None), Err(SourceError::Changed));
    let revision = restarted.revision(&account).unwrap();
    restarted.delete(&account, &revision, None).unwrap();
    fixture.add_generic_password(SERVICE, &account, b"replacement").unwrap();
    assert_eq!(restarted.delete(&account, &revision, None), Err(SourceError::Changed));
    assert!(fixture.find_generic_password(SERVICE, &account).unwrap().0.as_ref() == b"replacement");
}
