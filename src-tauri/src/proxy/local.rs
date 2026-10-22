//! The addresses of this Mac and the networks on its links. The proxy blocks
//! them like the private ranges: a public address of the Mac, or of a host
//! on its link, is not the internet. The list is read with `getifaddrs`
//! again after [`REFRESH`]. When a read fails, every address is blocked
//! until a read works again.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The longest use of one read of the interfaces.
const REFRESH: Duration = Duration::from_secs(30);

/// An address of an interface and the length of its on-link prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Net {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix.min(32))).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(self.prefix.min(128))).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

type Read = fn() -> io::Result<Vec<Net>>;

/// The interface list of the Mac, read again when it is due.
pub struct Local {
    read: Read,
    last: Mutex<Option<Last>>,
}

/// The last read: its time, and its list. `None`: the read failed.
struct Last {
    at: Instant,
    nets: Option<Vec<Net>>,
}

/// The list of this Mac, shared by all proxies.
pub static MAC: LazyLock<Local> = LazyLock::new(|| Local::new(interfaces));

impl Local {
    pub fn new(read: Read) -> Self {
        Self { read, last: Mutex::new(None) }
    }

    /// `ip` is an address of the Mac or on one of its links. True for each
    /// address while the list cannot be read.
    pub fn blocks(&self, ip: IpAddr) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        // A failed read is tried again at the next check.
        let due = last.as_ref().is_none_or(|l| l.nets.is_none() || l.at.elapsed() >= REFRESH);
        if due {
            *last = Some(Last { at: Instant::now(), nets: (self.read)().ok() });
        }
        last.as_ref().and_then(|l| l.nets.as_ref()).is_none_or(|nets| nets.iter().any(|n| n.contains(ip)))
    }
}

/// The IPv4 and IPv6 addresses of all interfaces, each with its prefix. An
/// address with no netmask counts as the address alone.
pub fn interfaces() -> io::Result<Vec<Net>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs sets `list` on success; it is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut nets = Vec::new();
    let mut current = list;
    while !current.is_null() {
        // SAFETY: a node of the list that getifaddrs returned, not freed yet.
        let ifa = unsafe { &*current };
        // SAFETY: each pointer is null or points to a sockaddr of the node.
        if let Some(addr) = unsafe { address(ifa.ifa_addr) } {
            let full = if addr.is_ipv4() { 32 } else { 128 };
            // SAFETY: as above. The mask has the family of the address.
            let prefix = unsafe { mask(ifa.ifa_netmask, addr) }.filter(|p| *p > 0).unwrap_or(full);
            nets.push(Net { addr, prefix });
        }
        current = ifa.ifa_next;
    }
    // SAFETY: the list of the getifaddrs call above, freed once.
    unsafe { libc::freeifaddrs(list) };
    Ok(nets)
}

/// The bytes of a sockaddr as `T`. macOS can give a netmask that is shorter
/// than its struct (its `sa_len`); the rest of `T` stays zero.
///
/// SAFETY: `sa` is null or points to a sockaddr of at least `sa_len` bytes.
unsafe fn read_as<T: Copy>(sa: *const libc::sockaddr) -> Option<T> {
    if sa.is_null() {
        return None;
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    // SAFETY: `sa` is not null.
    let len = usize::from(unsafe { (*sa).sa_len }).min(size_of::<T>());
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    let len = size_of::<T>();
    // SAFETY: an all-zero sockaddr is valid.
    let mut out: T = unsafe { std::mem::zeroed() };
    // SAFETY: `len` bytes are readable at `sa` and fit in `out`.
    unsafe { std::ptr::copy_nonoverlapping(sa.cast::<u8>(), (&raw mut out).cast::<u8>(), len) };
    Some(out)
}

/// SAFETY: as [`read_as`].
unsafe fn address(sa: *const libc::sockaddr) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: `sa` is not null.
    match i32::from(unsafe { (*sa).sa_family }) {
        libc::AF_INET => {
            // SAFETY: as [`read_as`].
            let v4: libc::sockaddr_in = unsafe { read_as(sa) }?;
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            // SAFETY: as [`read_as`].
            let v6: libc::sockaddr_in6 = unsafe { read_as(sa) }?;
            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

/// The prefix length of a netmask for `addr`. Its family field can be
/// unset, so the family of `addr` counts.
///
/// SAFETY: as [`read_as`].
unsafe fn mask(sa: *const libc::sockaddr, addr: IpAddr) -> Option<u8> {
    match addr {
        IpAddr::V4(_) => {
            // SAFETY: as [`read_as`].
            let v4: libc::sockaddr_in = unsafe { read_as(sa) }?;
            Some(u32::from_be(v4.sin_addr.s_addr).leading_ones() as u8)
        }
        IpAddr::V6(_) => {
            // SAFETY: as [`read_as`].
            let v6: libc::sockaddr_in6 = unsafe { read_as(sa) }?;
            Some(u128::from_be_bytes(v6.sin6_addr.s6_addr).leading_ones() as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_net_covers_its_prefix() {
        let lan = Net { addr: ip("203.0.113.7"), prefix: 24 };
        assert!(lan.contains(ip("203.0.113.200")) && lan.contains(ip("::ffff:203.0.113.1")));
        assert!(!lan.contains(ip("203.0.114.1")) && !lan.contains(ip("2001:db8::1")));
        let v6 = Net { addr: ip("2a02:1210:5600::42"), prefix: 64 };
        assert!(v6.contains(ip("2a02:1210:5600::9")) && !v6.contains(ip("2a02:1210:5601::9")));
        let host = Net { addr: ip("198.51.100.1"), prefix: 32 };
        assert!(host.contains(ip("198.51.100.1")) && !host.contains(ip("198.51.100.2")));
        assert!(Net { addr: ip("1.2.3.4"), prefix: 0 }.contains(ip("8.8.8.8")));
    }

    #[test]
    fn a_failed_read_blocks_everything() {
        let local = Local::new(|| Ok(vec![Net { addr: ip("203.0.113.7"), prefix: 24 }]));
        assert!(local.blocks(ip("203.0.113.9")));
        assert!(!local.blocks(ip("140.82.121.4")));
        let broken = Local::new(|| Err(io::ErrorKind::Other.into()));
        assert!(broken.blocks(ip("140.82.121.4")));
    }

    #[test]
    fn the_mac_has_its_loopback() {
        let nets = interfaces().unwrap();
        assert!(nets.iter().any(|n| n.addr == ip("127.0.0.1") && n.prefix == 8), "{nets:?}");
        assert!(nets.iter().all(|n| n.prefix > 0));
    }
}
