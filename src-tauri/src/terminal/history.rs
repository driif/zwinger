use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use super::output::RING;

const HEADER: &[u8; 4] = b"ZWH\x02";
const PREFIX: usize = 40;
const MAX_RUNS: usize = 16;

fn valid_id(id: &str) -> io::Result<()> {
    uuid::Uuid::parse_str(id).map_err(|_| io::ErrorKind::InvalidInput)?;
    Ok(())
}

fn dirs(data: &Path, session: &str, create: bool) -> io::Result<(OwnedFd, OwnedFd)> {
    valid_id(session)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let data = rustix::fs::open(data, flags, Mode::empty())?;
    let open = |parent: &OwnedFd, name: &str| -> io::Result<OwnedFd> {
        if create {
            match rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let dir = rustix::fs::openat(parent, name, flags, Mode::empty())?;
        if create {
            rustix::fs::fchmod(&dir, Mode::from_raw_mode(0o700))?;
        }
        Ok(dir)
    };
    let root = open(&data, "terminal-history")?;
    let session = open(&root, session)?;
    Ok((root, session))
}

pub(crate) fn load(data: &Path, session: &str, terminal: &str) -> Option<Vec<Vec<u8>>> {
    valid_id(terminal).ok()?;
    let (_, dir) = dirs(data, session, false).ok()?;
    // A FIFO must not block quit. The opened object, not its path, is checked.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(&dir, terminal, flags, Mode::empty()).ok()?;
    let stat = rustix::fs::fstat(&fd).ok()?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_size > RING as i64 {
        return None;
    }
    let mut bytes = Vec::new();
    File::from(fd).take((RING + 1) as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() < PREFIX || bytes.len() > RING || &bytes[..4] != HEADER {
        return None;
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    let payload = &bytes[PREFIX..];
    if count == 0 || count > MAX_RUNS || Sha256::digest(payload).as_slice() != &bytes[8..PREFIX] {
        return None;
    }
    let mut runs = Vec::with_capacity(count);
    let mut rest = payload;
    for _ in 0..count {
        let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        rest = rest.get(4..)?;
        if len == 0 {
            return None;
        }
        runs.push(rest.get(..len)?.to_vec());
        rest = rest.get(len..)?;
    }
    rest.is_empty().then_some(runs)
}

fn trim(runs: &mut Vec<Vec<u8>>) {
    loop {
        let excess = PREFIX + runs.len() * 4 + runs.iter().map(Vec::len).sum::<usize>();
        if runs.len() <= MAX_RUNS && excess <= RING {
            return;
        }
        let excess = excess.saturating_sub(RING);
        if runs.len() > MAX_RUNS || (runs.len() > 1 && runs[0].len() <= excess) {
            runs.remove(0);
            continue;
        }
        let tail = &runs[0][excess..];
        let cut = tail.iter().position(|byte| *byte == b'\n').map(|at| at + 1);
        // A long final line has no safe newline. Keep its bounded raw tail.
        runs[0] = cut.filter(|at| *at < tail.len()).map_or_else(|| tail.to_vec(), |at| tail[at..].to_vec());
    }
}

fn write_file(
    dir: &OwnedFd,
    name: &str,
    bytes: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let temp = format!(".{name}-{}.tmp", uuid::Uuid::new_v4());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(dir, temp.as_str(), flags, Mode::from_raw_mode(0o600))?;
    let written = (|| {
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        before_rename()?;
        rustix::fs::renameat(dir, temp.as_str(), dir, name)?;
        rustix::fs::fsync(dir)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = rustix::fs::unlinkat(dir, temp.as_str(), AtFlags::empty());
    }
    written
}

pub(crate) fn save(data: &Path, session: &str, terminal: &str, ring: &[u8]) -> io::Result<()> {
    if ring.is_empty() {
        return Ok(());
    }
    valid_id(terminal)?;
    let mut runs = load(data, session, terminal).unwrap_or_default();
    runs.push(ring.to_vec());
    trim(&mut runs);
    let mut payload = Vec::new();
    for run in &runs {
        payload.extend_from_slice(&(run.len() as u32).to_le_bytes());
        payload.extend_from_slice(run);
    }
    let mut bytes = Vec::with_capacity(PREFIX + payload.len());
    bytes.extend_from_slice(HEADER);
    bytes.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&Sha256::digest(&payload));
    bytes.extend_from_slice(&payload);
    let (_, dir) = dirs(data, session, true)?;
    write_file(&dir, terminal, &bytes, || Ok(()))
}

pub(crate) fn remove_terminal(data: &Path, session: &str, terminal: &str) -> io::Result<()> {
    // `save` refuses an invalid ID, so no history exists for it.
    if valid_id(session).is_err() || valid_id(terminal).is_err() {
        return Ok(());
    }
    let (_, dir) = match dirs(data, session, false) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match rustix::fs::unlinkat(&dir, terminal, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn remove_session(data: &Path, session: &str) -> io::Result<()> {
    if valid_id(session).is_err() {
        return Ok(());
    }
    let (root, dir) = match dirs(data, session, false) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        // The store is flat. Never recurse into an unexpected child folder.
        let stat = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let folder = data.join("terminal-history").join(session);
            let message = format!(
                "the terminal history folder {} contains the unexpected folder {:?}. Remove that folder, then delete the project again",
                folder.display(),
                name.to_string_lossy(),
            );
            return Err(io::Error::new(io::ErrorKind::InvalidData, message));
        }
        rustix::fs::unlinkat(&dir, name, AtFlags::empty())?;
    }
    rustix::fs::unlinkat(&root, session, AtFlags::REMOVEDIR)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cage::testing::temp_dir;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    fn ids() -> (String, String) {
        (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string())
    }

    #[test]
    fn round_trip_permissions_and_failed_rename() {
        let data = temp_dir();
        let (session, terminal) = ids();
        save(&data, &session, &terminal, b"prior\n").unwrap();
        assert_eq!(load(&data, &session, &terminal), Some(vec![b"prior\n".to_vec()]));
        let file = data.join("terminal-history").join(&session).join(&terminal);
        for dir in [file.parent().unwrap(), file.parent().unwrap().parent().unwrap()] {
            assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        let (_, dir) = dirs(&data, &session, false).unwrap();
        assert!(write_file(&dir, &terminal, b"replacement", || Err(io::Error::other("interrupted"))).is_err());
        assert_eq!(load(&data, &session, &terminal), Some(vec![b"prior\n".to_vec()]));
    }

    #[test]
    fn bound_and_long_final_line() {
        let data = temp_dir();
        let (session, terminal) = ids();
        let mut bytes = b"old\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', RING + 20));
        bytes.extend_from_slice(b"\nnew\n");
        save(&data, &session, &terminal, &bytes).unwrap();
        assert_eq!(load(&data, &session, &terminal).unwrap()[0], b"new\n");
        save(&data, &session, &terminal, &vec![b'q'; RING + 100_000]).unwrap();
        let saved = load(&data, &session, &terminal).unwrap();
        assert_eq!(saved.last().unwrap().len(), RING - PREFIX - 4);
        assert!(saved.last().unwrap().iter().all(|byte| *byte == b'q'));
    }

    #[test]
    fn later_run_stays_separate_from_clear_and_partial_control_strings() {
        let data = temp_dir();
        let (session, terminal) = ids();
        let first = b"first\x1b]52;unterminated\x1bPalso-unterminated";
        let second = b"\x1b[3Jsecond\n";
        save(&data, &session, &terminal, first).unwrap();
        save(&data, &session, &terminal, second).unwrap();
        assert_eq!(load(&data, &session, &terminal), Some(vec![first.to_vec(), second.to_vec()]));
    }

    #[test]
    fn missing_corrupt_and_oversize_are_absent() {
        let data = temp_dir();
        let (session, terminal) = ids();
        assert!(load(&data, &session, &terminal).is_none());
        save(&data, &session, &terminal, b"good\n").unwrap();
        let file = data.join("terminal-history").join(&session).join(&terminal);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes[PREFIX + 4] ^= 1;
        std::fs::write(&file, &bytes).unwrap();
        assert!(load(&data, &session, &terminal).is_none());
        bytes[3] = 3;
        std::fs::write(&file, &bytes).unwrap();
        assert!(load(&data, &session, &terminal).is_none());
        std::fs::write(&file, vec![b'x'; RING + 1]).unwrap();
        assert!(load(&data, &session, &terminal).is_none());
    }

    #[test]
    fn fifo_and_linked_session_are_not_followed() {
        let data = temp_dir();
        let elsewhere = temp_dir();
        let (session, terminal) = ids();
        let dir = data.join("terminal-history").join(&session);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(&terminal);
        let c_path = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(std::fs::symlink_metadata(&file).unwrap().file_type().is_fifo());
        assert!(load(&data, &session, &terminal).is_none());
        save(&data, &session, &terminal, b"safe\n").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        assert!(save(&data, &session, &terminal, b"secret\n").is_err());
        assert!(load(&data, &session, &terminal).is_none());
        assert!(remove_terminal(&data, &session, &terminal).is_err());
        assert!(remove_session(&data, &session).is_err());
        assert!(!elsewhere.join(terminal).exists());
    }
}
