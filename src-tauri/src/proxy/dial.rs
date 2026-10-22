//! The one way the proxy connects upstream. It resolves the host once on
//! the Mac, checks every address, and connects only to a checked address,
//! so a name that resolves to a private address (DNS rebinding) is refused.
//! Each upstream connection holds a permit of its shield and of the app.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::decide::is_always_blocked;
use super::local;

/// The longest wait for the name and the TCP connect together, and for the
/// permits before them.
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
/// The upstream connections of one shield at once, idle ones included.
const SHIELD_UPSTREAMS: usize = 128;
/// The upstream connections of all shields at once.
const APP_UPSTREAMS: usize = 1024;

static APP: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(APP_UPSTREAMS)));

/// The caps of the upstream connections of one proxy.
#[derive(Clone)]
pub struct Limits {
    app: Arc<Semaphore>,
    shield: Arc<Semaphore>,
}

/// The permits of one upstream connection. A drop gives them back.
pub struct Permits {
    _app: OwnedSemaphorePermit,
    _shield: OwnedSemaphorePermit,
}

impl Limits {
    /// A new shield cap, under the cap of the app.
    pub fn shield() -> Self {
        Self { app: APP.clone(), shield: Arc::new(Semaphore::new(SHIELD_UPSTREAMS)) }
    }

    #[cfg(test)]
    pub fn new(app: &Arc<Semaphore>, shield: usize) -> Self {
        Self { app: app.clone(), shield: Arc::new(Semaphore::new(shield)) }
    }

    /// The permits of the shield that connections hold.
    #[cfg(test)]
    pub fn used(&self) -> usize {
        SHIELD_UPSTREAMS - self.shield.available_permits()
    }

    /// Waits for a permit of the shield, then of the app.
    pub async fn acquire(&self, host: &str) -> Result<Permits, DialError> {
        let acquire = async {
            let shield = self.shield.clone().acquire_owned().await.expect("never closed");
            let app = self.app.clone().acquire_owned().await.expect("never closed");
            Permits { _app: app, _shield: shield }
        };
        tokio::time::timeout(CONNECT_LIMIT, acquire).await.map_err(|_| DialError::Busy(host.into()))
    }
}

pub type Resolved = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;

pub trait Resolve: Send + Sync + 'static {
    fn resolve(&self, host: String, port: u16) -> Resolved;

    /// The floor of the addresses: the always-blocked ranges and the
    /// addresses and links of the Mac.
    fn is_blocked(&self, ip: IpAddr) -> bool;
}

/// The resolver and the interfaces of the Mac.
pub struct System;

impl Resolve for System {
    fn resolve(&self, host: String, port: u16) -> Resolved {
        Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), port)).await?.collect()) })
    }

    fn is_blocked(&self, ip: IpAddr) -> bool {
        is_always_blocked(ip) || local::MAC.blocks(ip)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("{0} resolves to an always-blocked address")]
    Blocked(String),
    #[error("{0} has no address: {1}")]
    NoAddress(String, io::Error),
    #[error("cannot connect to {0}: {1}")]
    Connect(String, io::Error),
    #[error("cannot connect to {0} in time")]
    Timeout(String),
    #[error("too many connections are open; {0} waited too long")]
    Busy(String),
}

/// The addresses of `host`, each checked. One blocked address blocks all.
pub async fn checked(resolver: &dyn Resolve, host: &str, port: u16) -> Result<Vec<SocketAddr>, DialError> {
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => resolver.resolve(host.to_owned(), port).await.map_err(|e| DialError::NoAddress(host.into(), e))?,
    };
    if addrs.is_empty() {
        return Err(DialError::NoAddress(host.into(), io::ErrorKind::NotFound.into()));
    }
    if addrs.iter().any(|a| resolver.is_blocked(a.ip())) {
        return Err(DialError::Blocked(host.into()));
    }
    Ok(addrs)
}

/// Connects to the first checked address that answers. `pinned` are the
/// addresses that the CONNECT check found: they are checked again, and the
/// name is not resolved again.
pub async fn dial(
    resolver: &dyn Resolve,
    host: &str,
    port: u16,
    pinned: Option<Vec<SocketAddr>>,
) -> Result<TcpStream, DialError> {
    let connect = async {
        let addrs = match pinned {
            Some(addrs) if addrs.iter().any(|a| resolver.is_blocked(a.ip())) => {
                return Err(DialError::Blocked(host.into()));
            }
            Some(addrs) => addrs,
            None => checked(resolver, host, port).await?,
        };
        let mut last = io::Error::from(io::ErrorKind::NotFound);
        for addr in addrs {
            match TcpStream::connect(addr).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last = e,
            }
        }
        Err(DialError::Connect(host.into(), last))
    };
    tokio::time::timeout(CONNECT_LIMIT, connect).await.map_err(|_| DialError::Timeout(host.into()))?
}

#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// Names from a map; each lookup is counted. `loopback` lets a test
    /// reach its own servers on `127.0.0.1`. `interfaces` are the Mac's.
    #[derive(Default)]
    pub struct FakeResolver {
        pub names: Mutex<HashMap<String, Vec<IpAddr>>>,
        pub lookups: Mutex<Vec<String>>,
        pub loopback: bool,
        pub interfaces: Vec<local::Net>,
    }

    impl Resolve for FakeResolver {
        fn is_blocked(&self, ip: IpAddr) -> bool {
            !(self.loopback && ip.is_loopback())
                && (is_always_blocked(ip) || self.interfaces.iter().any(|n| n.contains(ip)))
        }

        fn resolve(&self, host: String, port: u16) -> Resolved {
            self.lookups.lock().unwrap().push(host.clone());
            let found = self.names.lock().unwrap().get(&host).cloned().unwrap_or_default();
            Box::pin(async move { Ok(found.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeResolver;
    use super::*;

    #[tokio::test]
    async fn a_public_name_with_a_private_address_is_blocked() {
        let resolver = FakeResolver::default();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        resolver.names.lock().unwrap().insert("rebind.example".into(), vec![ip("93.184.215.14"), ip("10.0.0.7")]);
        resolver.names.lock().unwrap().insert("meta.example".into(), vec![ip("169.254.169.254")]);
        for host in ["rebind.example", "meta.example", "10.0.0.1", "::1"] {
            assert!(matches!(dial(&resolver, host, 443, None).await, Err(DialError::Blocked(_))), "{host}");
        }
        assert!(matches!(checked(&resolver, "none.example", 443).await, Err(DialError::NoAddress(..))));
        // The name is resolved once per dial.
        assert_eq!(resolver.lookups.lock().unwrap().iter().filter(|h| *h == "rebind.example").count(), 1);
    }

    #[tokio::test]
    async fn the_public_addresses_of_the_mac_and_its_links_are_blocked() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let resolver = FakeResolver {
            interfaces: vec![
                local::Net { addr: ip("203.0.113.7"), prefix: 24 },
                local::Net { addr: ip("2a02:1210:5600::42"), prefix: 64 },
            ],
            ..FakeResolver::default()
        };
        resolver.names.lock().unwrap().insert("neighbour.example".into(), vec![ip("203.0.113.99")]);
        for host in ["neighbour.example", "203.0.113.7", "2a02:1210:5600::1", "::ffff:203.0.113.1"] {
            assert!(matches!(checked(&resolver, host, 443).await, Err(DialError::Blocked(_))), "{host}");
        }
        assert!(checked(&resolver, "203.0.114.1", 443).await.is_ok());
        // A pinned address is checked again.
        let pinned = Some(vec![SocketAddr::new(ip("203.0.113.5"), 443)]);
        assert!(matches!(dial(&resolver, "pinned.example", 443, pinned).await, Err(DialError::Blocked(_))));
        assert!(resolver.lookups.lock().unwrap().iter().all(|h| h != "pinned.example"));
    }

    #[tokio::test]
    async fn the_permits_of_the_shield_and_of_the_app_cap_the_connections() {
        let app = Arc::new(Semaphore::new(2));
        let (a, b) = (Limits::new(&app, 1), Limits::new(&app, 2));
        let wait = Duration::from_millis(50);
        let first = a.acquire("x").await.unwrap();
        // The shield cap: a second connection of `a` waits.
        assert!(tokio::time::timeout(wait, a.acquire("x")).await.is_err());
        let second = b.acquire("y").await.unwrap();
        // The app cap: `b` has a free permit, but the app has none.
        assert!(tokio::time::timeout(wait, b.acquire("y")).await.is_err());
        drop(first);
        let third = tokio::time::timeout(wait, b.acquire("y")).await.unwrap().unwrap();
        drop((second, third));
        assert_eq!(app.available_permits(), 2);
    }
}
