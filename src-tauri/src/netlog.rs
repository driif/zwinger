//! Turns microsandbox runtime log lines into network events. An event holds
//! only the host name and the destination address, never a request or a secret.
//!
//! The runtime writes `runtime.log` through microsandbox-runtime's
//! `RotatingLog`: at 10 MiB it renames the file to `runtime.log.1` (and the
//! older ones up to `.4`) and opens a new `runtime.log`. The logs of a
//! sandbox thus stay at about 50 MiB, and zwinger never truncates them: a
//! truncation would desync the writer's byte count. [`tail`] follows the
//! rename by inode.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::mpsc::{self, Receiver};

/// A repeat of the same kind and host within this time is dropped.
const DEDUPE_MS: u64 = 1000;
/// Above this size, `Dedupe` forgets the hosts that it cannot drop anymore.
const DEDUPE_HOSTS: usize = 1000;
/// The lines that `tail` queues before it waits for the reader.
pub const TAIL_QUEUE: usize = 256;
/// A longer log line is dropped. A network line is much shorter.
const TAIL_LINE: usize = 64 << 10;
/// How often `tail` looks for new lines at the end of the log.
const TAIL_POLL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetKind {
    DnsAllowed,
    DnsDenied,
    Tls,
    TlsDenied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetEvent {
    /// Per shield. `parse` sets 0; the manager sets the number.
    pub seq: u64,
    /// Milliseconds since the Unix epoch, from the log line.
    pub at: u64,
    pub kind: NetKind,
    pub host: String,
    pub dst: Option<String>,
}

pub fn parse(line: &str) -> Option<NetEvent> {
    let (stamp, rest) = line.split_once(' ')?;
    let at = epoch_ms(stamp)?;
    let event = |kind, host: &str, dst: Option<&str>| {
        let host = host.trim_end_matches('.');
        (!host.is_empty()).then(|| NetEvent {
            seq: 0,
            at,
            kind,
            host: host.to_owned(),
            dst: dst.map(str::to_owned),
        })
    };
    if rest.contains("DNS query denied by network policy") {
        return event(NetKind::DnsDenied, field(rest, "domain")?, None);
    }
    if let Some((_, query)) = rest.split_once("hickory_net::xfer: enqueueing message:QUERY:[Query { name: Name(\"") {
        return event(NetKind::DnsAllowed, query.split_once('"')?.0, None);
    }
    if rest.contains("engine::tls::proxy: TLS intercept ") {
        return event(NetKind::Tls, field(rest, "sni")?, field(rest, "dst"));
    }
    if rest.contains("TLS egress denied by default policy") {
        return event(NetKind::TlsDenied, field(rest, "sni")?, field(rest, "dst"));
    }
    None
}

/// The value of a ` key=value` field of a tracing line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (_, value) = line.split_once(&format!(" {key}="))?;
    value.split_whitespace().next()
}

/// Reads a UTC RFC 3339 time, as `2026-10-21T11:25:40.591549Z` or with `+00:00`.
fn epoch_ms(stamp: &str) -> Option<u64> {
    let stamp = stamp
        .strip_suffix('Z')
        .or_else(|| stamp.strip_suffix("+00:00"))?;
    let (date, time) = stamp.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<u64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time = time.splitn(3, ':').map(str::parse::<u64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let millis = format!("{fraction:0<3}").get(..3)?.parse::<u64>().ok()?;
    let seconds = days_since_epoch(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(seconds * 1000 + millis)
}

/// Howard Hinnant's `days_from_civil`, for years from 1970.
fn days_since_epoch(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Drops a repeat of the same kind and host within 1 s, such as the A and
/// AAAA queries of one lookup.
#[derive(Default)]
pub struct Dedupe {
    last: HashMap<(NetKind, String), u64>,
}

impl Dedupe {
    pub fn keep(&mut self, event: &NetEvent) -> bool {
        if self.last.len() > DEDUPE_HOSTS {
            self.last.retain(|_, at| event.at.saturating_sub(*at) < DEDUPE_MS);
        }
        let key = (event.kind, event.host.clone());
        if let Some(at) = self.last.get(&key)
            && event.at.saturating_sub(*at) < DEDUPE_MS
        {
            return false;
        }
        self.last.insert(key, event.at);
        true
    }
}

/// Follows the lines that are written to `path` from now on. At the end of
/// the file, a changed inode of `path` means a rotation: the old file is read
/// to its end, then the new file is read from its start. The lines stop when
/// the receiver is dropped.
pub async fn tail(path: PathBuf) -> std::io::Result<Receiver<String>> {
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(SeekFrom::End(0)).await?;
    let mut ino = inode(&file.metadata().await?);
    let mut log = BufReader::new(file);
    let (tx, rx) = mpsc::channel(TAIL_QUEUE);
    tokio::spawn(async move {
        // A partial last line stays in `line` until its end is written.
        let mut line = Vec::new();
        let mut dropping = false;
        let mut rotated = false;
        loop {
            let Ok(buf) = log.fill_buf().await else {
                return;
            };
            if buf.is_empty() {
                if tx.is_closed() {
                    return;
                }
                if rotated {
                    // The old file is read to its end.
                    if let Ok(file) = tokio::fs::File::open(&path).await
                        && let Ok(meta) = file.metadata().await
                    {
                        ino = inode(&meta);
                        log = BufReader::new(file);
                        line.clear();
                        dropping = false;
                        rotated = false;
                        continue;
                    }
                } else if let Ok(meta) = tokio::fs::metadata(&path).await
                    && inode(&meta) != ino
                {
                    // The writer can still have written to the old file before the rename.
                    rotated = true;
                    continue;
                }
                tokio::time::sleep(TAIL_POLL).await;
                continue;
            }
            let Some(end) = buf.iter().position(|&b| b == b'\n') else {
                let len = buf.len();
                if !dropping && line.len() + len > TAIL_LINE {
                    line.clear();
                    dropping = true;
                } else if !dropping {
                    line.extend_from_slice(buf);
                }
                log.consume(len);
                continue;
            };
            let keep = !dropping && line.len() + end <= TAIL_LINE;
            if keep {
                line.extend_from_slice(&buf[..end]);
            }
            log.consume(end + 1);
            dropping = false;
            if keep {
                let text = String::from_utf8_lossy(&line).trim_end().to_owned();
                if tx.send(text).await.is_err() {
                    return;
                }
            }
            line.clear();
        }
    });
    Ok(rx)
}

#[cfg(unix)]
fn inode(meta: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}

/// No rotation is seen without inodes.
#[cfg(not(unix))]
fn inode(_: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // From docs/reference/microsandbox-runtime-network-log.txt.
    const DNS_DENIED: &str = "2026-10-21T10:48:31.771300+00:00 DEBUG microsandbox_network::engine::dns::forwarder: DNS query denied by network policy domain=google.com";
    const DNS_ALLOWED: &str = "2026-10-21T10:48:31.859587+00:00 DEBUG hickory_net::xfer: enqueueing message:QUERY:[Query { name: Name(\"example.com.\"), query_type: AAAA, query_class: IN }]";
    const TLS: &str = "2026-10-21T10:48:31.934484+00:00 DEBUG microsandbox_network::engine::tls::proxy: TLS intercept sni=example.com dst=104.20.23.154:443 guest_dst=104.20.23.154:443";
    const TLS_ENDED: &str = "2026-10-21T10:48:32.223073+00:00 DEBUG microsandbox_network::engine::tls::proxy: TLS proxy task ended dst=104.20.23.154:443 guest_dst=104.20.23.154:443 error=peer closed connection without sending TLS close_notify: https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof";
    const CA: &str = "2026-10-21T10:48:31.347602+00:00 DEBUG microsandbox_network::engine::tls::state: loaded persisted CA from \"/Users/driif/.microsandbox/tls/ca.crt\"";
    // Synthesized: the reference log has no TLS deny. The format follows the
    // `tracing::debug!` in microsandbox-network `engine/tcp/deny.rs:100`.
    const TLS_DENIED: &str = "2026-10-21T10:48:33.000000Z DEBUG microsandbox_network::engine::tcp::deny: TLS egress denied by default policy sni=evil.example dst=93.184.216.34:443";

    fn event(kind: NetKind, host: &str, dst: Option<&str>, at: u64) -> NetEvent {
        NetEvent {
            seq: 0,
            at,
            kind,
            host: host.into(),
            dst: dst.map(Into::into),
        }
    }

    #[test]
    fn parses_each_line_kind() {
        assert_eq!(parse(DNS_DENIED), Some(event(NetKind::DnsDenied, "google.com", None, 1_792_579_711_771)));
        assert_eq!(parse(DNS_ALLOWED), Some(event(NetKind::DnsAllowed, "example.com", None, 1_792_579_711_859)));
        assert_eq!(
            parse(TLS),
            Some(event(NetKind::Tls, "example.com", Some("104.20.23.154:443"), 1_792_579_711_934))
        );
        assert_eq!(
            parse(TLS_DENIED),
            Some(event(NetKind::TlsDenied, "evil.example", Some("93.184.216.34:443"), 1_792_579_713_000))
        );
    }

    #[test]
    fn ignores_other_lines() {
        for line in [TLS_ENDED, CA, "", "junk", "not-a-time DNS query denied by network policy domain=x.com"] {
            assert_eq!(parse(line), None, "{line}");
        }
    }

    #[test]
    fn reads_both_utc_forms() {
        assert_eq!(epoch_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_ms("2026-10-21T11:25:40.591549Z"), Some(1_792_581_940_591));
        assert_eq!(epoch_ms("2026-10-21T11:25:40.5+00:00"), Some(1_792_581_940_500));
        assert_eq!(epoch_ms("2026-10-21T11:25:40+02:00"), None);
    }

    #[test]
    fn dedupe_drops_repeats_within_a_second() {
        let mut dedupe = Dedupe::default();
        let at = |ms| event(NetKind::DnsDenied, "google.com", None, ms);
        assert!(dedupe.keep(&at(1000)));
        assert!(!dedupe.keep(&at(1000)));
        assert!(!dedupe.keep(&at(1999)));
        assert!(dedupe.keep(&at(2000)));
        assert!(dedupe.keep(&event(NetKind::DnsAllowed, "google.com", None, 2000)));
        assert!(dedupe.keep(&event(NetKind::DnsDenied, "example.com", None, 2000)));
    }

    #[test]
    fn dedupe_drops_the_reference_pairs() {
        let log = include_str!("../../docs/reference/microsandbox-runtime-network-log.txt");
        let mut dedupe = Dedupe::default();
        let kept: Vec<(NetKind, String)> = log
            .lines()
            .filter_map(parse)
            .filter(|e| dedupe.keep(e))
            .map(|e| (e.kind, e.host))
            .collect();
        // The second google.com denial and the second example.com lookup
        // come less than 1 s after the first ones.
        assert_eq!(
            &kept[..4],
            [
                (NetKind::DnsAllowed, "example.com".into()),
                (NetKind::DnsDenied, "google.com".into()),
                (NetKind::Tls, "example.com".into()),
                (NetKind::DnsAllowed, "httpbin.org".into()),
            ]
        );
    }

    fn temp_log() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zwinger-netlog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("runtime.log"), "").unwrap();
        dir.join("runtime.log")
    }

    fn append(path: &std::path::Path, text: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    async fn next(rx: &mut Receiver<String>) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("a line in time")
            .expect("the tail still runs")
    }

    #[tokio::test]
    async fn tail_skips_old_lines() {
        let path = temp_log();
        append(&path, "old 1\nold 2\n");
        let mut rx = tail(path.clone()).await.unwrap();
        append(&path, "new 1\nnew ");
        assert_eq!(next(&mut rx).await, "new 1");
        append(&path, "2\n");
        assert_eq!(next(&mut rx).await, "new 2");
    }

    #[tokio::test]
    async fn tail_follows_a_rotation_without_lost_lines() {
        let path = temp_log();
        let mut rx = tail(path.clone()).await.unwrap();
        append(&path, "a\n");
        assert_eq!(next(&mut rx).await, "a");
        // As `RotatingLog`: the last lines go to the old file, then it is renamed.
        append(&path, "b\n");
        std::fs::rename(&path, path.with_extension("log.1")).unwrap();
        append(&path.with_extension("log.1"), "c\n");
        append(&path, "d\n");
        assert_eq!(next(&mut rx).await, "b");
        assert_eq!(next(&mut rx).await, "c");
        assert_eq!(next(&mut rx).await, "d");
    }

    #[tokio::test]
    async fn tail_drops_a_long_line() {
        let path = temp_log();
        let mut rx = tail(path.clone()).await.unwrap();
        append(&path, &"x".repeat(TAIL_LINE + 1));
        append(&path, &format!("{}\nshort\n", "x".repeat(TAIL_LINE)));
        append(&path, &format!("{}\n", "y".repeat(TAIL_LINE)));
        assert_eq!(next(&mut rx).await, "short");
        assert_eq!(next(&mut rx).await, "y".repeat(TAIL_LINE));
    }
}
