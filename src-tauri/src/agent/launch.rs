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
/// The guest dir of the zwinger CA, bound read-only.
pub const CA_DIR: &str = "/opt/zwinger/ca";
/// The zwinger CA in the guest: the proxy signs its leaf certificates.
pub const CA_CERT: &str = "/opt/zwinger/ca/zwinger-ca.pem";
/// Debian's system CA bundle. The CA env vars point here.
pub const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
/// The zwinger CA as a local CA of Debian, so a later install of
/// `ca-certificates` also adds it to the bundle.
const LOCAL_CA: &str = "/usr/local/share/ca-certificates/zwinger-ca.crt";
/// The CA bundle vars of OpenSSL, Python requests, curl and git. They point
/// at the system bundle, which [`trust_argv`] extends with the zwinger CA.
const CA_ENV: [&str; 4] = ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "GIT_SSL_CAINFO"];
/// Node reads the proxy vars only with `NODE_USE_ENV_PROXY`, and does not
/// read the system trust store. With `NODE_USE_ENV_PROXY`, Node warns at each
/// start that its proxy agent is experimental, which an agent TUI would show.
/// A credential cannot have the name `NODE_OPTIONS`. A wrapper appends the
/// flag to the user's own `NODE_OPTIONS`.
const NODE_NET_ENV: [(&str, &str); 3] =
    [("NODE_USE_ENV_PROXY", "1"), ("NODE_EXTRA_CA_CERTS", CA_CERT), ("NODE_OPTIONS", NODE_FLAG)];
const NODE_FLAG: &str = "--disable-warning=UNDICI-EHPA";

/// What the shield start found: the facts that pick an agent's program.
#[derive(Debug, Clone, Copy, Default)]
pub struct Found<'a> {
    /// pi runs from the host install; else the bundled pi runs.
    pub pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if it runs in the VM.
    pub gemini: Option<&'a str>,
    /// The URL of the shield's proxy. Each wrapper and headless command sets
    /// [`net_env`] for it, so an agent that clears its env keeps it.
    pub proxy: Option<&'a str>,
}

/// The env of every process in the guest: all traffic goes to the shield's
/// proxy at `proxy_url`, and the clients trust the zwinger CA.
pub fn net_env(proxy_url: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        env.push((name.to_owned(), proxy_url.to_owned()));
        env.push((name.to_ascii_lowercase(), proxy_url.to_owned()));
    }
    env.push(("NO_PROXY".to_owned(), String::new()));
    env.push(("no_proxy".to_owned(), String::new()));
    env.extend(NODE_NET_ENV.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())));
    env.extend(CA_ENV.iter().map(|n| ((*n).to_owned(), CA_BUNDLE.to_owned())));
    env
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

/// The net env of `found`'s proxy, then the state env of `kind`.
fn all_env(kind: AgentKind, found: Found) -> Vec<(String, String)> {
    let mut all = found.proxy.map(net_env).unwrap_or_default();
    all.extend(env(kind).iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())));
    all
}

/// `env <net env> <state env> <program>`: the start of a headless argv.
pub fn command(kind: AgentKind, found: Found) -> Option<Vec<String>> {
    let mut argv = vec!["env".to_owned()];
    argv.extend(all_env(kind, found).into_iter().map(|(name, value)| format!("{name}={value}")));
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
        (AgentKind::Gemini, true) => {
            Some(Added { flag: &["--yolo"], user: "-y|--yolo|--approval-mode|--approval-mode=*", subcommands: None })
        }
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
    let env: Vec<String> = all_env(kind, found)
        .iter()
        .filter(|(name, _)| name != "NODE_OPTIONS")
        .map(|(name, value)| format!("{name}={}", quote(value)))
        .collect();
    let mut script = format!("#!/bin/sh\n# zwinger writes this file at each shield start.\nexport {}\n", env.join(" "));
    if found.proxy.is_some() {
        script.push_str(&format!(
            "case \" $NODE_OPTIONS \" in *\" {NODE_FLAG} \"*) ;; \
             *) NODE_OPTIONS=\"${{NODE_OPTIONS:+$NODE_OPTIONS }}{NODE_FLAG}\" ;; esac\nexport NODE_OPTIONS\n"
        ));
    }
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

/// Makes an interactive bash report its folder with OSC 7 at each prompt,
/// so Open in editor knows the folder of a VM shell. `/bin/sh` reads the
/// profile too, so the code has a guard and only POSIX syntax. The path is
/// URL-encoded byte by byte; bash can read a byte as a negative number. A second read adds no second emitter.
const OSC7_EMITTER: &str = r#"if [ -n "${BASH_VERSION-}" ]; then
  __zw_osc7() {
    local LC_ALL=C path= c n i=0
    while [ "$i" -lt "${#PWD}" ]; do
      c=${PWD:i:1}
      case $c in
        [A-Za-z0-9/._~-]) path=$path$c ;;
        *) printf -v n '%d' "'$c"; printf -v c '%%%02X' $((n & 255)); path=$path$c ;;
      esac
      i=$((i + 1))
    done
    printf '\033]7;file://%s%s\007' "${HOSTNAME-}" "$path"
  }
  case ";${PROMPT_COMMAND-};" in
    *";__zw_osc7;"*) ;;
    *) PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND;}__zw_osc7" ;;
  esac
fi
"#;

/// The script of [`PROFILE`].
pub fn profile() -> String {
    format!(
        "case \":${{PATH-}}:\" in *:{BIN_DIR}:*) ;; *) PATH={BIN_DIR}:${{PATH-}} ;; esac\nexport PATH\n{OSC7_EMITTER}"
    )
}

/// The root exec that writes [`PROFILE`] in the guest's root fs.
pub fn profile_argv() -> Vec<String> {
    let script = format!("mkdir -p /etc/profile.d && printf '%s' \"$1\" > {PROFILE}");
    ["/bin/sh", "-c", &script, "sh", &profile()].map(str::to_owned).into()
}

/// The root exec that adds the zwinger CA to the system trust. With no
/// system bundle, the bundle holds only this CA.
pub fn trust_argv() -> Vec<String> {
    let dir = LOCAL_CA.rsplit_once('/').expect("a dir").0;
    let script = [
        "set -e".to_owned(),
        format!("mkdir -p {dir}"),
        format!("cp {CA_CERT} {LOCAL_CA}"),
        "if command -v update-ca-certificates >/dev/null 2>&1; then".to_owned(),
        "  update-ca-certificates >/dev/null".to_owned(),
        format!("elif [ -f {CA_BUNDLE} ]; then"),
        // The second line of the PEM is unique to the CA, so a second run appends nothing.
        format!("  grep -qF \"$(sed -n 2p {CA_CERT})\" {CA_BUNDLE} || cat {CA_CERT} >> {CA_BUNDLE}"),
        "else".to_owned(),
        // All TLS of the shield goes through the proxy, so a bundle with only
        // the zwinger CA is complete.
        format!("  mkdir -p \"$(dirname {CA_BUNDLE})\""),
        format!("  cp {CA_CERT} {CA_BUNDLE}"),
        "fi".to_owned(),
    ]
    .join("\n");
    ["/bin/sh", "-c", &script].map(str::to_owned).into()
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

    const PROXY: &str = "http://zw:t@host.microsandbox.internal:4000";
    const HOST: Found =
        Found { pi_host: true, gemini: Some("/opt/zwinger/host/gemini/bundle/gemini.js"), proxy: Some(PROXY) };

    /// Runs the wrapper with `sh`, with a program that prints its env and arguments.
    fn run(kind: AgentKind, unprompted: bool, args: &[&str]) -> String {
        let script = wrapper(kind, HOST, unprompted).unwrap();
        let program = program(kind, HOST).unwrap().iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
        let script =
            script.replace(&format!("exec {program}"), "exec printf '%s\\n' \"$HOME$CODEX_HOME$PI_CODING_AGENT_DIR\"");
        let out = std::process::Command::new("/bin/sh")
            .env_clear()
            .arg("-c")
            .arg(script)
            .arg("sh")
            .args(args)
            .output()
            .unwrap();
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
        assert_eq!(
            run(AgentKind::Claude, true, &["-c"]),
            "/opt/zwinger/state/claude\n--dangerously-skip-permissions\n-c\n"
        );
        // The user's own permission flag stays, and nothing is added.
        assert_eq!(
            run(AgentKind::Claude, true, &["--permission-mode", "plan"]),
            "/opt/zwinger/state/claude\n--permission-mode\nplan\n"
        );
        assert_eq!(run(AgentKind::Claude, true, &["mcp", "list"]), "/opt/zwinger/state/claude\nmcp\nlist\n");
        assert_eq!(run(AgentKind::Claude, false, &["-c"]), "/opt/zwinger/state/claude\n-c\n");
        assert!(!wrapper(AgentKind::Claude, HOST, false).unwrap().contains("dangerously"));
    }

    #[test]
    fn codex_wrapper_bypasses_or_keeps_the_sandbox_off() {
        let on = wrapper(AgentKind::Codex, HOST, true).unwrap();
        assert!(on.contains(" CODEX_HOME=/opt/zwinger/state/codex\n"), "{on}");
        assert!(on.ends_with("exec /opt/zwinger/codex/bin/codex \"$@\"\n"), "{on}");
        assert_eq!(
            run(AgentKind::Codex, true, &["resume", "--last"]),
            "/opt/zwinger/state/codex\n--dangerously-bypass-approvals-and-sandbox\nresume\n--last\n"
        );
        assert_eq!(run(AgentKind::Codex, true, &["-a", "on-request"]), "/opt/zwinger/state/codex\n-a\non-request\n");
        assert_eq!(run(AgentKind::Codex, true, &["login"]), "/opt/zwinger/state/codex\nlogin\n");
        // Off: the approvals come back, but Codex's own sandbox stays off.
        assert_eq!(run(AgentKind::Codex, false, &[]), "/opt/zwinger/state/codex\n--sandbox\ndanger-full-access\n");
        assert_eq!(
            run(AgentKind::Codex, false, &["-a", "never"]),
            "/opt/zwinger/state/codex\n--sandbox\ndanger-full-access\n-a\nnever\n"
        );
        assert_eq!(
            run(AgentKind::Codex, false, &["--sandbox=read-only"]),
            "/opt/zwinger/state/codex\n--sandbox=read-only\n"
        );
        assert_eq!(run(AgentKind::Codex, false, &["logout"]), "/opt/zwinger/state/codex\nlogout\n");
    }

    #[test]
    fn pi_and_gemini_wrappers_run_the_found_install() {
        let pi = wrapper(AgentKind::Pi, HOST, true).unwrap();
        assert!(pi.contains(" PI_CODING_AGENT_DIR=/opt/zwinger/state/agent\n"), "{pi}");
        assert!(!pi.contains("add="), "{pi}");
        assert!(
            pi.ends_with(
                "exec node /opt/zwinger/host/pi/dist/cli.js --session-dir /opt/zwinger/state/sessions \"$@\"\n"
            ),
            "{pi}"
        );
        let bundled = wrapper(AgentKind::Pi, Found::default(), true).unwrap();
        assert!(bundled.contains("exec node /opt/zwinger/pi/cli.js "), "{bundled}");
        assert_eq!(
            run(AgentKind::Pi, true, &["-c"]),
            "/opt/zwinger/state/agent\n--session-dir\n/opt/zwinger/state/sessions\n-c\n"
        );

        let gemini = wrapper(AgentKind::Gemini, HOST, true).unwrap();
        assert!(gemini.contains(" HOME=/opt/zwinger/state/gemini\n"), "{gemini}");
        assert!(gemini.ends_with("exec node /opt/zwinger/host/gemini/bundle/gemini.js \"$@\"\n"), "{gemini}");
        assert_eq!(
            run(AgentKind::Gemini, true, &["--resume", "latest"]),
            "/opt/zwinger/state/gemini\n--yolo\n--resume\nlatest\n"
        );
        assert_eq!(
            run(AgentKind::Gemini, true, &["--approval-mode=auto_edit"]),
            "/opt/zwinger/state/gemini\n--approval-mode=auto_edit\n"
        );
        assert_eq!(run(AgentKind::Gemini, false, &[]), "/opt/zwinger/state/gemini\n");
        assert_eq!(wrapper(AgentKind::Gemini, Found::default(), true), None);
    }

    #[test]
    fn odd_paths_are_quoted() {
        let found = Found { pi_host: false, gemini: Some("/opt/zwinger/host/gemini/it's here.js"), proxy: None };
        let gemini = wrapper(AgentKind::Gemini, found, false).unwrap();
        assert!(gemini.ends_with("exec node '/opt/zwinger/host/gemini/it'\\''s here.js' \"$@\"\n"), "{gemini}");
    }

    #[test]
    fn headless_commands_and_continue_flags() {
        assert_eq!(
            command(AgentKind::Claude, Found::default()).unwrap(),
            ["env", "IS_SANDBOX=1", "HOME=/opt/zwinger/state/claude", "/opt/zwinger/claude/claude"]
        );
        let pi = command(AgentKind::Pi, HOST).unwrap();
        assert_eq!(pi[0], "env");
        assert_eq!(
            pi[pi.len() - 3..],
            ["PI_CODING_AGENT_DIR=/opt/zwinger/state/agent", "node", "/opt/zwinger/host/pi/dist/cli.js"]
        );
        for var in [
            format!("HTTPS_PROXY={PROXY}"),
            format!("https_proxy={PROXY}"),
            "NODE_USE_ENV_PROXY=1".to_owned(),
            format!("NODE_EXTRA_CA_CERTS={CA_CERT}"),
            format!("SSL_CERT_FILE={CA_BUNDLE}"),
        ] {
            assert!(pi.contains(&var), "{var} is not in {pi:?}");
        }
        assert_eq!(command(AgentKind::Gemini, Found::default()), None);
        assert_eq!(continue_args(AgentKind::Codex), ["resume", "--last"]);
        assert_eq!(continue_args(AgentKind::Gemini), ["--resume", "latest"]);
    }

    #[test]
    fn a_wrapper_sets_the_proxy_in_a_cleared_env() {
        let script = wrapper(AgentKind::Codex, HOST, false).unwrap();
        assert!(script.contains(&format!(" HTTPS_PROXY='{PROXY}' ")), "{script}");
        let script = script.replace(
            "exec /opt/zwinger/codex/bin/codex",
            "exec printf '%s\\n' \"$HTTPS_PROXY\" \"$NODE_USE_ENV_PROXY\" \"$NODE_EXTRA_CA_CERTS\" \"$NODE_OPTIONS\"",
        );
        let run = |node_options: Option<&str>| {
            let mut command = std::process::Command::new("/bin/sh");
            command.env_clear().arg("-c").arg(&script);
            if let Some(value) = node_options {
                command.env("NODE_OPTIONS", value);
            }
            String::from_utf8(command.output().unwrap().stdout).unwrap()
        };
        let out =
            |node_options: &str| format!("{PROXY}\n1\n{CA_CERT}\n{node_options}\n--sandbox\ndanger-full-access\n");
        assert_eq!(run(None), out(NODE_FLAG));
        // The user's own Node options stay, and the flag is added once.
        assert_eq!(run(Some("--max-old-space-size=4096")), out(&format!("--max-old-space-size=4096 {NODE_FLAG}")));
        assert_eq!(run(Some(&format!("--a {NODE_FLAG}"))), out(&format!("--a {NODE_FLAG}")));
    }

    #[test]
    fn every_process_uses_the_proxy_and_trusts_the_zwinger_ca() {
        let env: std::collections::BTreeMap<_, _> =
            net_env("http://zw:t@host.microsandbox.internal:4000").into_iter().collect();
        for name in ["HTTP_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            assert_eq!(env[name], "http://zw:t@host.microsandbox.internal:4000", "{name}");
        }
        assert_eq!((env["NO_PROXY"].as_str(), env["no_proxy"].as_str()), ("", ""));
        assert_eq!(env["NODE_USE_ENV_PROXY"], "1");
        assert_eq!(env["NODE_EXTRA_CA_CERTS"], CA_CERT);
        assert_eq!(env["NODE_OPTIONS"], "--disable-warning=UNDICI-EHPA");
        assert_eq!(env["GIT_SSL_CAINFO"], CA_BUNDLE);
    }

    #[test]
    fn profile_puts_the_wrappers_first_on_a_login_path() {
        assert!(profile().starts_with(
            "case \":${PATH-}:\" in *:/opt/zwinger/bin:*) ;; *) PATH=/opt/zwinger/bin:${PATH-} ;; esac\nexport PATH\nif "
        ));
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
    fn profile_makes_bash_report_its_folder_once() {
        let root = std::env::temp_dir().join(format!("zwinger-osc7-{}", uuid::Uuid::new_v4()));
        let dir = root.join("a b/\u{e4}%");
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "PROMPT_COMMAND=mine\n{}{}cd \"$1\" && HOSTNAME=vm && eval \"$PROMPT_COMMAND\" && echo \"|$PROMPT_COMMAND\"",
            profile(),
            profile()
        );
        let out = std::process::Command::new("/bin/bash").args(["-c", &script, "bash"]).arg(&dir).output().unwrap();
        let root = root.to_str().unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("\x1b]7;file://vm{root}/a%20b/%C3%A4%25\x07|mine;__zw_osc7\n")
        );
        // A shell that is not bash skips the emitter.
        let script = format!("emulate sh\n{}echo \"|${{PROMPT_COMMAND:-}}|\"", profile());
        let out = std::process::Command::new("/bin/zsh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "||\n", "{}", String::from_utf8_lossy(&out.stderr));
        // `set -u` with no environment: /bin/sh is bash in sh mode on macOS, so only dash tests the guard.
        let file = std::env::temp_dir().join(format!("zwinger-profile-{}.sh", uuid::Uuid::new_v4()));
        std::fs::write(&file, profile()).unwrap();
        let has_dash = std::process::Command::new("dash").arg("-c").arg(":").status().is_ok_and(|s| s.success());
        for shell in std::iter::once("/bin/sh").chain(has_dash.then_some("dash")) {
            let out = std::process::Command::new(shell)
                .env_clear()
                .args(["-c", "set -u; . \"$1\"; echo marker", "sh"])
                .arg(&file)
                .output()
                .unwrap();
            assert!(out.status.success(), "{shell}: {}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(String::from_utf8(out.stdout).unwrap(), "marker\n", "{shell}");
        }
        std::fs::remove_file(file).unwrap();
    }

    /// Runs the trust script with its paths below a temp root. `tools` is a
    /// dir on `PATH` before the system dirs.
    fn trust(root: &Path, tools: &Path) -> std::process::Output {
        let script = trust_argv()[2]
            .replace(CA_CERT, &root.join("ca.pem").display().to_string())
            .replace("/usr/local/share", &root.join("local").display().to_string())
            .replace(CA_BUNDLE, &root.join("bundle.crt").display().to_string());
        std::process::Command::new("/bin/sh")
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
            .arg("-c")
            .arg(script)
            .output()
            .unwrap()
    }

    fn trust_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("zwinger-trust-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::write(root.join("ca.pem"), "-----BEGIN CERTIFICATE-----\nZWINGERCA\n-----END CERTIFICATE-----\n")
            .unwrap();
        root
    }

    #[test]
    fn trust_appends_the_ca_to_the_bundle_once() {
        let root = trust_root();
        std::fs::write(root.join("bundle.crt"), "SYSTEM ROOTS\n").unwrap();
        for _ in 0..2 {
            let out = trust(&root, &root.join("tools"));
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let bundle = std::fs::read_to_string(root.join("bundle.crt")).unwrap();
        assert!(bundle.starts_with("SYSTEM ROOTS\n"), "{bundle}");
        assert_eq!(bundle.matches("ZWINGERCA").count(), 1, "{bundle}");
        let local = std::fs::read_to_string(root.join("local/ca-certificates/zwinger-ca.crt")).unwrap();
        assert!(local.contains("ZWINGERCA"));
    }

    #[test]
    fn trust_prefers_update_ca_certificates() {
        use std::os::unix::fs::PermissionsExt;

        let root = trust_root();
        let tool = root.join("tools/update-ca-certificates");
        std::fs::write(&tool, format!("#!/bin/sh\necho ran > {}\n", root.join("ran").display())).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("bundle.crt"), "SYSTEM ROOTS\n").unwrap();
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(root.join("ran").exists());
        assert_eq!(std::fs::read_to_string(root.join("bundle.crt")).unwrap(), "SYSTEM ROOTS\n");
    }

    #[test]
    fn trust_makes_a_bundle_with_only_the_ca() {
        let root = trust_root();
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let bundle = std::fs::read_to_string(root.join("bundle.crt")).unwrap();
        assert_eq!(bundle.matches("ZWINGERCA").count(), 1, "{bundle}");
        // A later install of ca-certificates picks it up.
        assert!(root.join("local/ca-certificates/zwinger-ca.crt").exists());
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
