//! The zwinger CA of this app run: a new ECDSA P-256 key that stays only in
//! memory, and its self-signed certificate, which is written for the guests.
//! The proxy signs one leaf
//! certificate per host and keeps the newest in a bounded cache. It signs
//! with no lock held, and once for concurrent requests of one host.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use lru::LruCache;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::OnceCell;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The file of the CA certificate in [`Ca::dir`], bound read-only into each guest.
pub const CERT_FILE: &str = "zwinger-ca.pem";
/// The leaf certificates that stay cached.
const LEAVES: usize = 512;

pub struct Ca {
    der: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
    leaves: Mutex<LruCache<String, Arc<ServerConfig>>>,
    /// The leaves that are being signed, by host.
    signing: Mutex<HashMap<String, Arc<OnceCell<Arc<ServerConfig>>>>>,
    provider: Arc<CryptoProvider>,
    #[cfg(test)]
    signed: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ca").finish_non_exhaustive()
    }
}

fn ca_error(e: impl std::fmt::Display) -> Error {
    Error::Proxy(format!("the zwinger CA: {e}"))
}

/// The name and constraints of the CA certificate.
fn params() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "zwinger proxy CA");
    name.push(DnType::OrganizationName, "zwinger");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    params
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl Ca {
    /// Makes a new key and writes its certificate to `dir`. The key is never written.
    pub fn load(dir: &Path) -> Result<Self> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(ca_error)?;
        let cert = params().self_signed(&key).map_err(ca_error)?;
        let file = dir.join(CERT_FILE);
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&file, cert.pem()))
            .map_err(|e| ca_error(format!("cannot write the certificate to {}: {e}", file.display())))?;
        Ok(Self {
            der: cert.der().clone(),
            issuer: Issuer::new(params(), key),
            leaves: Mutex::new(LruCache::new(NonZeroUsize::new(LEAVES).expect("not zero"))),
            signing: Mutex::default(),
            provider: provider(),
            #[cfg(test)]
            signed: Default::default(),
        })
    }

    /// The SHA-256 of the certificate DER, as colon hex.
    pub fn fingerprint(&self) -> String {
        Sha256::digest(&self.der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
    }

    /// The folder of [`CERT_FILE`] below the app data.
    pub fn dir(data: &Path) -> PathBuf {
        data.join("ca")
    }

    #[cfg(test)]
    pub fn der(&self) -> &CertificateDer<'static> {
        &self.der
    }

    /// The TLS config that the proxy shows the guest for `host`, with a
    /// leaf certificate for it. HTTP/2 and HTTP/1.1 by ALPN. Concurrent
    /// requests for one host wait for one signature.
    pub async fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>> {
        // The cache is checked under the `signing` lock, and a signed leaf is
        // in the cache before it leaves `signing`: so a host is signed once.
        let cell = {
            let mut signing = self.signing.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(config) = self.leaves.lock().unwrap_or_else(PoisonError::into_inner).get(host) {
                return Ok(config.clone());
            }
            signing.entry(host.to_owned()).or_default().clone()
        };
        let config = cell.get_or_try_init(|| async { self.sign(host) }).await.cloned();
        if let Ok(config) = &config {
            self.leaves.lock().unwrap_or_else(PoisonError::into_inner).put(host.to_owned(), config.clone());
        }
        let mut signing = self.signing.lock().unwrap_or_else(PoisonError::into_inner);
        if signing.get(host).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            signing.remove(host);
        }
        config
    }

    fn sign(&self, host: &str) -> Result<Arc<ServerConfig>> {
        #[cfg(test)]
        self.signed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(ca_error)?;
        let mut params = CertificateParams::new(vec![host.to_owned()]).map_err(ca_error)?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let leaf = params.signed_by(&key, &self.issuer).map_err(ca_error)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(ca_error)?
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone(), self.der.clone()], key)
            .map_err(ca_error)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }

    #[cfg(test)]
    pub fn cached(&self) -> usize {
        self.leaves.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        crate::test_dir::root().join(format!("zwinger-ca-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn each_load_makes_a_new_key_and_writes_only_the_cert() {
        let dir = temp_dir();
        let first = Ca::load(&dir).unwrap();
        let written = std::fs::read_to_string(dir.join(CERT_FILE)).unwrap();
        assert!(written.starts_with("-----BEGIN CERTIFICATE-----"), "{written}");
        assert!(!written.contains("PRIVATE"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let second = Ca::load(&dir).unwrap();
        assert_ne!(first.der(), second.der());
        assert_ne!(first.fingerprint(), second.fingerprint());
        assert_ne!(std::fs::read_to_string(dir.join(CERT_FILE)).unwrap(), written);
    }

    #[test]
    fn the_fingerprint_is_the_sha256_of_the_der_in_colon_hex() {
        let ca = Ca::load(&temp_dir()).unwrap();
        let fp = ca.fingerprint();
        assert_eq!(fp.len(), 32 * 3 - 1, "{fp}");
        let hex: String = Sha256::digest(ca.der()).iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(fp.replace(':', ""), hex);
    }

    #[test]
    fn a_cert_that_cannot_be_written_fails_the_load_with_its_path() {
        let file = temp_dir();
        std::fs::write(&file, "not a dir").unwrap();
        let e = Ca::load(&file).unwrap_err().to_string();
        assert!(e.contains("cannot write the certificate to"), "{e}");
        assert!(e.contains(CERT_FILE), "{e}");
    }

    #[tokio::test]
    async fn leaves_are_cached_per_host_and_bounded() {
        let ca = Ca::load(&temp_dir()).unwrap();
        let a = ca.server_config("a.example").await.unwrap();
        assert!(Arc::ptr_eq(&a, &ca.server_config("a.example").await.unwrap()));
        assert_eq!(a.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
        for i in 0..LEAVES + 5 {
            ca.server_config(&format!("h{i}.example")).await.unwrap();
        }
        assert_eq!(ca.cached(), LEAVES);
        assert!(!Arc::ptr_eq(&a, &ca.server_config("a.example").await.unwrap()));
        assert!(ca.signing.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_for_a_host_share_one_signature() {
        let ca = Arc::new(Ca::load(&temp_dir()).unwrap());
        let tasks: Vec<_> = (0..16)
            .map(|i| {
                let ca = ca.clone();
                let host = if i % 2 == 0 { "a.example" } else { "b.example" };
                tokio::spawn(async move { (host, ca.server_config(host).await.unwrap()) })
            })
            .collect();
        let mut configs: HashMap<&str, Arc<ServerConfig>> = HashMap::new();
        for task in tasks {
            let (host, config) = task.await.unwrap();
            assert!(Arc::ptr_eq(configs.entry(host).or_insert_with(|| config.clone()), &config), "{host}");
        }
        assert_eq!(ca.signed.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(ca.signing.lock().unwrap().is_empty());
    }
}
