//! The current folder of a VM terminal, read from the guest's process tree.
//! A shell that reports no OSC 7 (a nested zsh, a `/bin/zsh`, any other
//! shell) still has a folder: the guest knows the `cwd` of the foreground
//! process of the terminal. No startup file of the guest changes for it.

use std::collections::HashMap;
use std::time::Duration;

/// The longest wait for one probe, from the dial of the exec to its exit.
/// After it, the caller uses the kept folder.
pub(crate) const LIMIT: Duration = Duration::from_millis(500);

/// The most stdout that a probe can give. More is an error.
pub(crate) const OUTPUT_CAP: usize = 8 << 10;

/// For each PID after the proc root `$1`, prints `pid TAB folder NUL`: the
/// folder of the foreground process of the terminal of that PID. Field 8 of
/// `stat` is the foreground process group of the terminal (`tpgid`), and its
/// leader has that PID. The name in `stat` can hold spaces and `)`, so the
/// fields start after the last `) `. If the group has no leader now, or the
/// process has no terminal, it gives the folder of the process itself. A PID
/// that is gone prints nothing.
const SCRIPT: &str = r#"set -f
tpgid() { t=$6; }
root=$1
shift
for p do
  s=
  read -r s 2>/dev/null < "$root/$p/stat"
  [ -n "$s" ] || continue
  t=
  tpgid ${s##*) }
  c=
  [ "${t:-0}" -gt 0 ] 2>/dev/null && c=$(readlink "$root/$t/cwd" 2>/dev/null)
  [ -n "$c" ] || c=$(readlink "$root/$p/cwd" 2>/dev/null) || continue
  printf '%s\t%s\0' "$p" "$c"
done
"#;

/// The guest command that prints the current folder of the foreground
/// process of the terminal that each guest PID of `pids` started.
pub(crate) fn argv(pids: &[u32]) -> Vec<String> {
    script_argv(pids, "/proc")
}

fn script_argv(pids: &[u32], root: &str) -> Vec<String> {
    ["/bin/sh", "-c", SCRIPT, "sh", root]
        .iter()
        .map(|s| (*s).to_owned())
        .chain(pids.iter().map(u32::to_string))
        .collect()
}

/// The guest folder of each PID in the output of [`argv`]. A record with no
/// absolute path, or with a folder that was removed, is left out.
pub(crate) fn parse(out: &[u8]) -> HashMap<u32, String> {
    out.split(|b| *b == 0)
        .filter_map(|record| {
            let (pid, path) = std::str::from_utf8(record).ok()?.split_once('\t')?;
            let ok = path.starts_with('/') && !path.ends_with(" (deleted)");
            ok.then_some((pid.parse().ok()?, path.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A proc root with a process per `(pid, name, stat after the name, cwd)`.
    fn proc_root(processes: &[(u32, &str, &str, &str)]) -> std::path::PathBuf {
        let dir = crate::cage::testing::temp_dir();
        for (pid, name, rest, cwd) in processes {
            let folder = dir.join(pid.to_string());
            std::fs::create_dir(&folder).unwrap();
            std::fs::write(folder.join("stat"), format!("{pid} ({name}) {rest}\n")).unwrap();
            symlink(cwd, folder.join("cwd")).unwrap();
        }
        dir
    }

    fn run(pids: &[u32], root: &std::path::Path) -> HashMap<u32, String> {
        let argv = script_argv(pids, root.to_str().unwrap());
        let out = std::process::Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
        parse(&out.stdout)
    }

    #[test]
    fn one_probe_finds_the_folder_of_the_foreground_process_of_each_pid() {
        // state ppid pgrp session tty_nr tpgid ...
        let root = proc_root(&[
            (10, "bash", "S 1 10 10 34816 12 4194560 0", "/workspace"),
            (12, "zsh", "S 10 12 10 34816 12 4194304 0", "/workspace/a b"),
            // A name with spaces and a ") ": the fields start after the last one.
            (20, "a) S 1 1 1 1 99 (x", "S 1 20 20 34816 21 0", "/workspace"),
            (21, "vim", "S 20 21 20 34816 21 0", "/workspace/src\tx"),
            // No terminal: tpgid -1.
            (30, "sh", "S 1 30 30 0 -1 0", "/tmp"),
            // The leader of the foreground group has exited.
            (40, "bash", "S 1 40 40 34816 41 0", "/home"),
        ]);
        let found = run(&[10, 20, 30, 40, 99], &root);
        let want: HashMap<u32, String> =
            [(10, "/workspace/a b"), (20, "/workspace/src\tx"), (30, "/tmp"), (40, "/home")]
                .map(|(pid, path)| (pid, path.to_owned()))
                .into();
        assert_eq!(found, want, "99 is gone");
        assert_eq!(run(&[12], &root)[&12], "/workspace/a b");
        assert!(run(&[], &root).is_empty());
        assert_eq!(argv(&[7, 8])[4..], ["/proc", "7", "8"]);
    }

    #[test]
    fn only_an_absolute_existing_folder_parses() {
        let found = parse(b"1\t/workspace/a b\n\x002\t/gone (deleted)\x003\trelative\x004\t/\xff\x00x\t/a\x005\t/\x00");
        let want: HashMap<u32, String> = [(1, "/workspace/a b\n"), (5, "/")].map(|(p, s)| (p, s.to_owned())).into();
        assert_eq!(found, want);
        assert!(parse(b"").is_empty());
    }
}
