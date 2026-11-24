//! The current folder of a VM terminal, read from the guest's process tree.
//! A shell that reports no OSC 7 (a nested zsh, a `/bin/zsh`, any other
//! shell) still has a folder: the guest knows the `cwd` of the foreground
//! process of the terminal. No startup file of the guest changes for it.

/// Gives the folder of the foreground process of the terminal of the PID
/// `$1`, below the proc root `$2`. Field 8 of `stat` is the foreground
/// process group of the terminal (`tpgid`), and its leader has that PID. The
/// name in `stat` can hold spaces and `)`, so the fields start after the
/// last `) `. If the group has no leader now, or the process has no
/// terminal, it gives the folder of the process itself.
const SCRIPT: &str = r#"set -f
read -r s < "$2/$1/stat" || exit 1
set -- "$1" "$2" ${s##*) }
[ "${8:-0}" -gt 0 ] 2>/dev/null && readlink "$2/$8/cwd" || readlink "$2/$1/cwd"
"#;

/// The guest command that prints the current folder of the foreground
/// process of the terminal that the guest PID `pid` started.
pub(crate) fn argv(pid: u32) -> Vec<String> {
    script_argv(pid, "/proc")
}

fn script_argv(pid: u32, root: &str) -> Vec<String> {
    ["/bin/sh", "-c", SCRIPT, "sh"].iter().map(|s| (*s).to_owned()).chain([pid.to_string(), root.to_owned()]).collect()
}

/// The guest folder in the output of [`argv`]. `None`: no absolute path, or
/// a folder that was removed.
pub(crate) fn parse(out: &[u8]) -> Option<String> {
    let out = std::str::from_utf8(out).ok()?;
    let path = out.strip_suffix('\n').unwrap_or(out);
    (path.starts_with('/') && !path.contains('\n') && !path.ends_with(" (deleted)")).then(|| path.to_owned())
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

    fn run(pid: u32, root: &std::path::Path) -> Option<String> {
        let argv = script_argv(pid, root.to_str().unwrap());
        let out = std::process::Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
        out.status.success().then(|| parse(&out.stdout)).flatten()
    }

    #[test]
    fn the_probe_finds_the_folder_of_the_foreground_process() {
        // state ppid pgrp session tty_nr tpgid ...
        let root = proc_root(&[
            (10, "bash", "S 1 10 10 34816 12 4194560 0", "/workspace"),
            (12, "zsh", "S 10 12 10 34816 12 4194304 0", "/workspace/a b"),
            // A name with spaces and a ") ": the fields start after the last one.
            (20, "a) S 1 1 1 1 99 (x", "S 1 20 20 34816 21 0", "/workspace"),
            (21, "vim", "S 20 21 20 34816 21 0", "/workspace/src"),
            // No terminal: tpgid -1.
            (30, "sh", "S 1 30 30 0 -1 0", "/tmp"),
            // The leader of the foreground group has exited.
            (40, "bash", "S 1 40 40 34816 41 0", "/home"),
        ]);
        assert_eq!(run(10, &root).as_deref(), Some("/workspace/a b"));
        assert_eq!(run(12, &root).as_deref(), Some("/workspace/a b"));
        assert_eq!(run(20, &root).as_deref(), Some("/workspace/src"));
        assert_eq!(run(30, &root).as_deref(), Some("/tmp"));
        assert_eq!(run(40, &root).as_deref(), Some("/home"));
        assert_eq!(run(99, &root), None, "no such process");
        assert_eq!(argv(7)[4..], ["7", "/proc"]);
    }

    #[test]
    fn only_an_absolute_existing_folder_parses() {
        assert_eq!(parse(b"/workspace/a b\n").as_deref(), Some("/workspace/a b"));
        assert_eq!(parse(b"/").as_deref(), Some("/"));
        assert_eq!(parse(b"/workspace/gone (deleted)\n"), None);
        assert_eq!(parse(b"workspace\n"), None);
        assert_eq!(parse(b"/a\n/b\n"), None);
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"/\xff\n"), None);
    }
}
