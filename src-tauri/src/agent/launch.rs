//! How each agent starts in the VM: the env that keeps its state in the
//! shield, its program, and its flags. The task runs use it for headless
//! calls; the agent sessions and the shield terminals use the wrappers that
//! it writes, which are on `PATH` at [`BIN_DIR`].

use std::path::Path;

use super::{AgentKind, claude, codex, gemini, pi};

/// The guest dir of the wrappers, bound read-only.
pub const BIN_DIR: &str = "/opt/zwinger/bin";
/// The `PATH` of each PTY exec: the wrappers first, then the image's `PATH`.
pub const PATH: &str = "/opt/zwinger/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Debian's `/etc/profile` resets `PATH` for login shells, then sources
/// this dir. So this file puts the wrappers back on `PATH`.
pub const PROFILE: &str = "/etc/profile.d/zwinger.sh";

/// What the shield start found: the facts that pick an agent's program.
#[derive(Debug, Clone, Copy, Default)]
pub struct Found<'a> {
    /// pi runs from the host install; else the bundled pi runs.
    pub pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if it runs in the VM.
    pub gemini: Option<&'a str>,
}

/// The env that keeps the agent's state in the shield and never the host's.
pub fn env(kind: AgentKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        // The guest runs as root. Claude Code refuses
        // `--dangerously-skip-permissions` as root unless IS_SANDBOX=1 is set.
        AgentKind::Claude => &[("IS_SANDBOX", "1"), ("HOME", claude::HOME)],
        AgentKind::Codex => &[("CODEX_HOME", codex::HOME)],
        AgentKind::Pi => &[("PI_CODING_AGENT_DIR", pi::AGENT_DIR)],
        AgentKind::Gemini => &[("HOME", gemini::HOME)],
    }
}

/// The program and its first arguments. `None` if Gemini CLI does not run in the VM.
pub fn program(kind: AgentKind, found: Found) -> Option<Vec<String>> {
    Some(match kind {
        AgentKind::Claude => vec![format!("{}/claude", claude::DIR)],
        AgentKind::Codex => vec![format!("{}/bin/codex", codex::DIR)],
        AgentKind::Pi => vec!["node".to_owned(), pi::cli(found.pi_host)],
        AgentKind::Gemini => vec!["node".to_owned(), found.gemini?.to_owned()],
    })
}

/// `env <state env> <program>`: the start of a headless argv.
pub fn command(kind: AgentKind, found: Found) -> Option<Vec<String>> {
    let mut argv = vec!["env".to_owned()];
    argv.extend(env(kind).iter().map(|(name, value)| format!("{name}={value}")));
    argv.extend(program(kind, found)?);
    Some(argv)
}

/// The arguments that continue the agent's last session in the harness.
pub fn continue_args(kind: AgentKind) -> &'static [&'static str] {
    match kind {
        AgentKind::Claude | AgentKind::Pi => &["-c"],
        AgentKind::Codex => &["resume", "--last"],
        AgentKind::Gemini => &["--resume", "latest"],
    }
}

/// The flag that a wrapper adds, and the user arguments that keep it out.
struct Added {
    flag: &'static [&'static str],
    /// `case` patterns of the user's own permission or sandbox flags.
    user: &'static str,
    /// `case` patterns of the first argument: a subcommand that starts no session.
    subcommands: Option<&'static str>,
}

const CODEX_SUBCOMMANDS: Option<&str> = Some("login|logout|mcp");

/// `unprompted` is the shield setting "Agents run without permission
/// prompts". It is acceptable because the shield is the sandbox: the VM has
/// only the shield's mounts and allowed hosts.
fn added(kind: AgentKind, unprompted: bool) -> Option<Added> {
    match (kind, unprompted) {
        (AgentKind::Claude, true) => Some(Added {
            flag: &["--dangerously-skip-permissions"],
            user: "--dangerously-skip-permissions|--permission-mode|--permission-mode=*",
            subcommands: Some("mcp|config|doctor|update"),
        }),
        (AgentKind::Codex, true) => Some(Added {
            flag: &["--dangerously-bypass-approvals-and-sandbox"],
            user: "--dangerously-bypass-approvals-and-sandbox|--yolo|--full-auto|-a|--ask-for-approval|--ask-for-approval=*|-s|--sandbox|--sandbox=*",
            subcommands: CODEX_SUBCOMMANDS,
        }),
        // Codex's own sandbox does not work in the VM.
        (AgentKind::Codex, false) => Some(Added {
            flag: &["--sandbox", "danger-full-access"],
            user: "--dangerously-bypass-approvals-and-sandbox|--yolo|--full-auto|-s|--sandbox|--sandbox=*",
            subcommands: CODEX_SUBCOMMANDS,
        }),
        (AgentKind::Gemini, true) => Some(Added {
            flag: &["--yolo"],
            user: "-y|--yolo|--approval-mode|--approval-mode=*",
            subcommands: None,
        }),
        _ => None,
    }
}

/// `text` as one word of `sh`.
fn quote(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-=:".contains(&b)) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The POSIX `sh` script of the agent's command in the VM. `None` if
/// Gemini CLI does not run in the VM.
pub fn wrapper(kind: AgentKind, found: Found, unprompted: bool) -> Option<String> {
    let mut program: Vec<String> = program(kind, found)?.iter().map(|p| quote(p)).collect();
    if kind == AgentKind::Pi {
        program.extend(["--session-dir".to_owned(), pi::SESSION_DIR.to_owned()]);
    }
    let env: Vec<String> = env(kind).iter().map(|(name, value)| format!("{name}={}", quote(value))).collect();
    let mut script = format!(
        "#!/bin/sh\n# zwinger writes this file at each shield start.\nexport {}\n",
        env.join(" ")
    );
    if let Some(added) = added(kind, unprompted) {
        script.push_str("add=1\n");
        if let Some(subcommands) = added.subcommands {
            script.push_str(&format!("case \"$1\" in {subcommands}) add= ;; esac\n"));
        }
        script.push_str(&format!("for arg do\n  case \"$arg\" in {}) add= ;; esac\ndone\n", added.user));
        script.push_str(&format!("[ -n \"$add\" ] && set -- {} \"$@\"\n", added.flag.join(" ")));
    }
    script.push_str(&format!("exec {} \"$@\"\n", program.join(" ")));
    Some(script)
}

/// The script of [`PROFILE`].
pub fn profile() -> String {
    format!("case \":$PATH:\" in *:{BIN_DIR}:*) ;; *) PATH={BIN_DIR}:$PATH ;; esac\nexport PATH\n")
}

/// The root exec that writes [`PROFILE`] in the guest's root fs.
pub fn profile_argv() -> Vec<String> {
    let script = format!("mkdir -p /etc/profile.d && printf '%s' \"$1\" > {PROFILE}");
    ["/bin/sh", "-c", &script, "sh", &profile()].map(str::to_owned).into()
}

/// Writes the wrapper of each agent in `kinds` to the host dir `dir`, which
/// is bound at [`BIN_DIR`]. Each file is written in place, so the guest
/// sees the new text at once.
pub fn write(dir: &Path, kinds: &[AgentKind], found: Found, unprompted: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for &kind in kinds {
        let Some(script) = wrapper(kind, found, unprompted) else {
            continue;
        };
        let path = dir.join(kind.name());
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: Found = Found { pi_host: true, gemini: Some("/opt/zwinger/host/gemini/bundle/gemini.js") };

    /// Runs the wrapper with `sh`, with a program that prints its env and arguments.
    fn run(kind: AgentKind, unprompted: bool, args: &[&str]) -> String {
        let script = wrapper(kind, HOST, unprompted).unwrap();
        let program = program(kind, HOST).unwrap().iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
        let script = script.replace(&format!("exec {program}"), "exec printf '%s\\n' \"$HOME$CODEX_HOME$PI_CODING_AGENT_DIR\"");
        let out = std::process::Command::new("/bin/sh").env_clear().arg("-c").arg(script).arg("sh").args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn claude_wrapper_sets_its_state_and_skips_permissions() {
        assert_eq!(
            wrapper(AgentKind::Claude, Found::default(), true).unwrap(),
            "#!/bin/sh\n# zwinger writes this file at each shield start.\n\
             export IS_SANDBOX=1 HOME=/opt/zwinger/state/claude\n\
             add=1\n\
             case \"$1\" in mcp|config|doctor|update) add= ;; esac\n\
             for arg do\n  case \"$arg\" in --dangerously-skip-permissions|--permission-mode|--permission-mode=*) add= ;; esac\ndone\n\
             [ -n \"$add\" ] && set -- --dangerously-skip-permissions \"$@\"\n\
             exec /opt/zwinger/claude/claude \"$@\"\n"
        );
        assert_eq!(run(AgentKind::Claude, true, &["-c"]), "/opt/zwinger/state/claude\n--dangerously-skip-permissions\n-c\n");
        // The user's own permission flag stays, and nothing is added.
        assert_eq!(run(AgentKind::Claude, true, &["--permission-mode", "plan"]), "/opt/zwinger/state/claude\n--permission-mode\nplan\n");
        assert_eq!(run(AgentKind::Claude, true, &["mcp", "list"]), "/opt/zwinger/state/claude\nmcp\nlist\n");
        assert_eq!(run(AgentKind::Claude, false, &["-c"]), "/opt/zwinger/state/claude\n-c\n");
        assert!(!wrapper(AgentKind::Claude, HOST, false).unwrap().contains("dangerously"));
    }

    #[test]
    fn codex_wrapper_bypasses_or_keeps_the_sandbox_off() {
        let on = wrapper(AgentKind::Codex, HOST, true).unwrap();
        assert!(on.contains("export CODEX_HOME=/opt/zwinger/state/codex\n"), "{on}");
        assert!(on.ends_with("exec /opt/zwinger/codex/bin/codex \"$@\"\n"), "{on}");
        assert_eq!(
            run(AgentKind::Codex, true, &["resume", "--last"]),
            "/opt/zwinger/state/codex\n--dangerously-bypass-approvals-and-sandbox\nresume\n--last\n"
        );
        assert_eq!(run(AgentKind::Codex, true, &["-a", "on-request"]), "/opt/zwinger/state/codex\n-a\non-request\n");
        assert_eq!(run(AgentKind::Codex, true, &["login"]), "/opt/zwinger/state/codex\nlogin\n");
        // Off: the approvals come back, but Codex's own sandbox stays off.
        assert_eq!(run(AgentKind::Codex, false, &[]), "/opt/zwinger/state/codex\n--sandbox\ndanger-full-access\n");
        assert_eq!(run(AgentKind::Codex, false, &["-a", "never"]), "/opt/zwinger/state/codex\n--sandbox\ndanger-full-access\n-a\nnever\n");
        assert_eq!(run(AgentKind::Codex, false, &["--sandbox=read-only"]), "/opt/zwinger/state/codex\n--sandbox=read-only\n");
        assert_eq!(run(AgentKind::Codex, false, &["logout"]), "/opt/zwinger/state/codex\nlogout\n");
    }

    #[test]
    fn pi_and_gemini_wrappers_run_the_found_install() {
        let pi = wrapper(AgentKind::Pi, HOST, true).unwrap();
        assert!(pi.contains("export PI_CODING_AGENT_DIR=/opt/zwinger/state/agent\n"), "{pi}");
        assert!(!pi.contains("add="), "{pi}");
        assert!(
            pi.ends_with("exec node /opt/zwinger/host/pi/dist/cli.js --session-dir /opt/zwinger/state/sessions \"$@\"\n"),
            "{pi}"
        );
        let bundled = wrapper(AgentKind::Pi, Found::default(), true).unwrap();
        assert!(bundled.contains("exec node /opt/zwinger/pi/cli.js "), "{bundled}");
        assert_eq!(run(AgentKind::Pi, true, &["-c"]), "/opt/zwinger/state/agent\n--session-dir\n/opt/zwinger/state/sessions\n-c\n");

        let gemini = wrapper(AgentKind::Gemini, HOST, true).unwrap();
        assert!(gemini.contains("export HOME=/opt/zwinger/state/gemini\n"), "{gemini}");
        assert!(gemini.ends_with("exec node /opt/zwinger/host/gemini/bundle/gemini.js \"$@\"\n"), "{gemini}");
        assert_eq!(run(AgentKind::Gemini, true, &["--resume", "latest"]), "/opt/zwinger/state/gemini\n--yolo\n--resume\nlatest\n");
        assert_eq!(run(AgentKind::Gemini, true, &["--approval-mode=auto_edit"]), "/opt/zwinger/state/gemini\n--approval-mode=auto_edit\n");
        assert_eq!(run(AgentKind::Gemini, false, &[]), "/opt/zwinger/state/gemini\n");
        assert_eq!(wrapper(AgentKind::Gemini, Found::default(), true), None);
    }

    #[test]
    fn odd_paths_are_quoted() {
        let found = Found { pi_host: false, gemini: Some("/opt/zwinger/host/gemini/it's here.js") };
        let gemini = wrapper(AgentKind::Gemini, found, false).unwrap();
        assert!(gemini.ends_with("exec node '/opt/zwinger/host/gemini/it'\\''s here.js' \"$@\"\n"), "{gemini}");
    }

    #[test]
    fn headless_commands_and_continue_flags() {
        assert_eq!(
            command(AgentKind::Claude, Found::default()).unwrap(),
            ["env", "IS_SANDBOX=1", "HOME=/opt/zwinger/state/claude", "/opt/zwinger/claude/claude"]
        );
        assert_eq!(
            command(AgentKind::Pi, HOST).unwrap(),
            ["env", "PI_CODING_AGENT_DIR=/opt/zwinger/state/agent", "node", "/opt/zwinger/host/pi/dist/cli.js"]
        );
        assert_eq!(command(AgentKind::Gemini, Found::default()), None);
        assert_eq!(continue_args(AgentKind::Codex), ["resume", "--last"]);
        assert_eq!(continue_args(AgentKind::Gemini), ["--resume", "latest"]);
    }

    #[test]
    fn profile_puts_the_wrappers_first_on_a_login_path() {
        assert_eq!(
            profile(),
            "case \":$PATH:\" in *:/opt/zwinger/bin:*) ;; *) PATH=/opt/zwinger/bin:$PATH ;; esac\nexport PATH\n"
        );
        // As Debian's /etc/profile does: it resets PATH, then sources profile.d.
        let login = format!("PATH=/usr/bin:/bin\n{}{}echo \"$PATH\"", profile(), profile());
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(login).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "/opt/zwinger/bin:/usr/bin:/bin\n");
        let argv = profile_argv();
        assert_eq!(argv[..2], ["/bin/sh", "-c"]);
        assert_eq!(argv[2], "mkdir -p /etc/profile.d && printf '%s' \"$1\" > /etc/profile.d/zwinger.sh");
        assert_eq!(argv[3..], ["sh".to_owned(), profile()]);
        assert!(PATH.starts_with(&format!("{BIN_DIR}:")));
    }

    #[test]
    fn write_makes_executable_wrappers_for_the_given_agents() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("zwinger-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, &[AgentKind::Pi, AgentKind::Claude, AgentKind::Gemini], Found::default(), true).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        names.sort();
        assert_eq!(names, ["claude", "pi"]);
        let mode = std::fs::metadata(dir.join("claude")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        write(&dir, &[AgentKind::Claude], Found::default(), false).unwrap();
        assert!(!std::fs::read_to_string(dir.join("claude")).unwrap().contains("dangerously"));
    }
}
