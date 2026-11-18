//! How each agent starts in the VM: the env that keeps its state in the
//! project, its program, and its flags. The task runs use it for headless
//! calls; the agent sessions and the project terminals use the wrappers that
//! it writes, which are on `PATH` at [`BIN_DIR`].

use std::path::Path;

use super::{AgentKind, claude, codex, gemini, pi};
use crate::host_agents::detect::HostAgent;
use crate::terminal::output::AGENT_OSC;

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
pub(crate) const CA_ENV: [&str; 4] = ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "GIT_SSL_CAINFO"];
/// Node reads the proxy vars only with `NODE_USE_ENV_PROXY`, and does not
/// read the system trust store. With `NODE_USE_ENV_PROXY`, Node warns at each
/// start that its proxy agent is experimental, which an agent TUI would show.
/// A credential cannot have the name `NODE_OPTIONS`. A wrapper appends the
/// flag to the user's own `NODE_OPTIONS`.
const NODE_NET_ENV: [(&str, &str); 3] =
    [("NODE_USE_ENV_PROXY", "1"), ("NODE_EXTRA_CA_CERTS", CA_CERT), ("NODE_OPTIONS", NODE_FLAG)];
const NODE_FLAG: &str = "--disable-warning=UNDICI-EHPA";

/// What the cage start found: the facts that pick an agent's program.
#[derive(Debug, Clone, Copy, Default)]
pub struct Found<'a> {
    /// pi runs from the host install; else the bundled pi runs.
    pub pi_host: bool,
    /// The guest path of Gemini CLI's entry point, if it runs in the VM.
    pub gemini: Option<&'a str>,
    /// The URL of the cage's proxy. Each wrapper and headless command sets
    /// [`net_env`] for it, so an agent that clears its env keeps it.
    pub proxy: Option<&'a str>,
}

/// The env of every process in the guest: all traffic goes to the cage's
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

/// The env that keeps the agent's state in the cage and never the host's.
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

/// The argv of the Mac's install of an agent in a session with no cage
/// (ADR 0008): the binary that detection ran, or `node <package>/<entry>`.
/// Nothing isolates it, so it gets no bypass flag, no zwinger wrapper and
/// no env: the agent uses the Mac's own config and login. `None` if the
/// detection has no runnable form, or a relative binary, which would resolve
/// in the session folder or by `PATH`.
pub fn host_argv(found: &HostAgent, resume: bool) -> Option<Vec<String>> {
    let mut argv = match found.kind {
        AgentKind::Claude | AgentKind::Codex => {
            vec![found.bin.as_ref().filter(|bin| bin.is_absolute())?.to_string_lossy().into_owned()]
        }
        AgentKind::Pi | AgentKind::Gemini => {
            let entry = match found.kind {
                AgentKind::Pi => pi::HOST_ENTRY,
                _ => found.entry.as_deref()?,
            };
            vec!["node".to_owned(), found.package.as_ref()?.join(entry).to_string_lossy().into_owned()]
        }
    };
    if resume {
        argv.extend(continue_args(found.kind).iter().map(|a| (*a).to_owned()));
    }
    Some(argv)
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

/// `unprompted` is the project setting "Agents run without permission
/// prompts". It is acceptable because the cage is the sandbox: the VM has
/// only the project's mounts and allowed hosts.
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

/// The script that an agent hook runs with the agent's state. It writes
/// [`AGENT_OSC`] to the terminal, never to the hook's stdout.
const STATUS: &str = "zwinger-agent-status";
const CLAUDE_STATUS: &str = "claude-status.json";
const PI_STATUS: &str = "pi-status.js";
/// The wrapper adds the status hooks only to an agent in a terminal: a
/// headless call from a tool cannot change its parent's state.
const TTY: &str = "[ -t 0 ] && [ -t 1 ]";

/// The flag that adds the status hooks, and the arguments that start no
/// interactive session.
fn status_flag(kind: AgentKind) -> Option<(String, &'static str, &'static str)> {
    match kind {
        AgentKind::Claude => Some((
            format!("--settings {BIN_DIR}/{CLAUDE_STATUS}"),
            "agents|attach|auth|auto-mode|config|doctor|gateway|import|install|logs|mcp|plugin|plugins|purge|respawn|rm|setup-token|stop|kill|ultrareview|update|upgrade",
            "-p|--print",
        )),
        AgentKind::Pi => Some((
            format!("-e {BIN_DIR}/{PI_STATUS}"),
            "install|remove|uninstall|update|list|config|auth|mcp",
            "-p|--print",
        )),
        AgentKind::Codex | AgentKind::Gemini => None,
    }
}

/// [`AGENT_OSC`] after its ESC.
fn agent_osc() -> &'static str {
    std::str::from_utf8(&AGENT_OSC[1..]).expect("ASCII")
}

/// The argument of [`STATUS`] that picks the state from the tool name in
/// the hook input on stdin: `AskUserQuestion` waits for the user, each
/// other tool runs.
const TOOL: &str = "tool";

fn status_script() -> String {
    format!(
        "#!/bin/sh\n# zwinger writes this file at each cage start.\n\
         state=$1\n\
         if [ \"$state\" = {TOOL} ]; then\n  \
           case \"$(cat)\" in *'\"tool_name\":\"AskUserQuestion\"'*|*'\"tool_name\": \"AskUserQuestion\"'*) state=blocked ;; \
           *) state=active ;; esac\n\
         fi\n\
         printf '\\033{}%s\\007' \"$state\" > /dev/tty 2>/dev/null || true\n",
        agent_osc()
    )
}

/// Claude Code settings with the status hooks only. `--settings` adds them
/// to the hooks of the user's settings.
fn claude_status() -> String {
    let set =
        |state: &str| serde_json::json!([{ "type": "command", "command": format!("{BIN_DIR}/{STATUS} {state}") }]);
    let hooks = serde_json::json!({
        "UserPromptSubmit": [{ "hooks": set("active") }],
        // One hook for all tools: two matching hooks would run in parallel.
        "PreToolUse": [{ "hooks": set(TOOL) }],
        // After the user approves a permission, the tool runs.
        "PostToolUse": [{ "hooks": set("active") }],
        // Not `idle_prompt`: it comes after `Stop`, when the turn is done.
        "Notification": [{ "matcher": "permission_prompt|elicitation_dialog", "hooks": set("blocked") }],
        "Stop": [{ "hooks": set("done") }],
    });
    serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks })).expect("JSON")
}

/// A pi extension. pi asks for no permissions, so it is never blocked.
fn pi_status() -> String {
    format!(
        "// zwinger writes this file at each cage start.\n\
         import {{ writeFileSync }} from \"node:fs\";\n\
         const status = (state) => {{\n  \
           try {{\n    writeFileSync(\"/dev/tty\", `\\x1b{}${{state}}\\x07`);\n  }} catch {{}}\n\
         }};\n\
         export default function (pi) {{\n  \
           pi.on(\"agent_start\", () => status(\"active\"));\n  \
           pi.on(\"agent_settled\", () => status(\"done\"));\n\
         }}\n",
        agent_osc()
    )
}

/// `text` as one word of `sh`.
pub(crate) fn quote(text: &str) -> String {
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
    let mut script = format!("#!/bin/sh\n# zwinger writes this file at each cage start.\nexport {}\n", env.join(" "));
    if found.proxy.is_some() {
        script.push_str(&format!(
            "case \" $NODE_OPTIONS \" in *\" {NODE_FLAG} \"*) ;; \
             *) NODE_OPTIONS=\"${{NODE_OPTIONS:+$NODE_OPTIONS }}{NODE_FLAG}\" ;; esac\nexport NODE_OPTIONS\n"
        ));
    }
    script.push_str("first=$1\n");
    let permission = added(kind, unprompted);
    if let Some(added) = &permission {
        script.push_str("add=1\n");
        if let Some(subcommands) = added.subcommands {
            script.push_str(&format!("case \"$first\" in {subcommands}) add= ;; esac\n"));
        }
        script.push_str(&format!("for arg do\n  case \"$arg\" in {}) add= ;; esac\ndone\n", added.user));
    }
    if let Some((flag, subcommands, other)) = status_flag(kind) {
        script.push_str(&format!("status=\n{TTY} && status=1\ncase \"$first\" in {subcommands}) status= ;; esac\n"));
        script.push_str(&format!("for arg do\n  case \"$arg\" in {other}) status= ;; esac\ndone\n"));
        script.push_str(&format!("[ -n \"$status\" ] && set -- {flag} \"$@\"\n"));
    }
    if let Some(added) = permission {
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

/// The lines around the zwinger CA in [`CA_BUNDLE`].
const BEGIN: &str = "# BEGIN zwinger proxy CA";
const END: &str = "# END zwinger proxy CA";

/// The link to [`LOCAL_CA`] in the dir of [`CA_BUNDLE`], with the name that
/// `update-ca-certificates` gives it.
const CA_LINK: &str = "zwinger-ca.pem";

/// Drops the marked block, and each PEM outside it that is equal to the
/// previous CA (`ARGV[1]`) or the new CA (`ARGV[2]`), from the bundle
/// (`ARGV[3]`). An updater can copy the previous CA outside the block.
const STRIP: &str = r#"FILENAME == ARGV[1] { prev = prev $0 "\n"; next }
FILENAME == ARGV[2] { next_ca = next_ca $0 "\n"; next }
$0 == "BEGIN_MARK" { skip = 1 }
skip { if ($0 == "END_MARK") skip = 0; next }
pem != "" || $0 == "-----BEGIN CERTIFICATE-----" {
  pem = pem $0 "\n"
  if ($0 == "-----END CERTIFICATE-----") { if (pem != prev && pem != next_ca) printf "%s", pem; pem = "" }
  next
}
{ print }
END { printf "%s", pem }"#;

/// The root exec that adds the zwinger CA to the system trust. Each app run
/// has a new CA, so the previous CA is removed from the whole bundle and the
/// marked block holds only the new CA. With no system bundle, the bundle
/// holds only this CA.
///
/// The script writes temp files next to the resolved bundle and the local
/// copy, and renames them over those files. Readers see the complete old or
/// new bundle, and a symlink chain to the bundle stays as it is. The script
/// does not run `update-ca-certificates`: Debian's updater renames its output
/// over the symlink. Instead, with openssl or c_rehash, it makes the CApath
/// links that the updater makes. Each error before the bundle rename puts the local copy and
/// the links back, and the trap removes the temp files.
pub fn trust_argv() -> Vec<String> {
    let local_dir = LOCAL_CA.rsplit_once('/').expect("a dir").0;
    let strip = STRIP.replace("BEGIN_MARK", BEGIN).replace("END_MARK", END);
    let script = [
        "set -e".to_owned(),
        "prev= tmp= local= keep= swapped= pem= hash_link= stale= stage= done=".to_owned(),
        format!("certs=$(dirname {CA_BUNDLE})"),
        "undo() {".to_owned(),
        "  if [ -n \"$swapped\" ]; then".to_owned(),
        format!("    if [ -n \"$keep\" ]; then mv -f \"$keep\" {LOCAL_CA}; else rm -f {LOCAL_CA}; fi"),
        "  fi".to_owned(),
        "  [ -z \"$hash_link\" ] || rm -f \"$hash_link\"".to_owned(),
        format!("  [ -z \"$pem\" ] || rm -f \"$certs/{CA_LINK}\""),
        "  [ -z \"$stale\" ] || for link in \"$stale\"/*; do [ ! -L \"$link\" ] || mv -f \"$link\" \"$certs/\"; done".to_owned(),
        "}".to_owned(),
        "trap 'status=$?; set +e; [ -n \"$done\" ] || undo; rm -rf ${prev:+\"$prev\"} ${tmp:+\"$tmp\"} ${local:+\"$local\"} ${keep:+\"$keep\"} ${stale:+\"$stale\"} ${stage:+\"$stage\"}; exit $status' EXIT".to_owned(),
        // `mv` onto a dir moves the file into it, so each target is a file or
        // missing.
        format!("[ ! -e {LOCAL_CA} ] || [ -f {LOCAL_CA} ] || exit 1"),
        // A fresh image has no bundle and no dir. A dangling link is not
        // fresh: the dir of its target must exist.
        format!("[ -e {CA_BUNDLE} ] || [ -L {CA_BUNDLE} ] || mkdir -p \"$certs\""),
        // Follows the links by hand: `readlink -f` fails on a missing target
        // in some builds. More than 40 links is a cycle.
        format!("real={CA_BUNDLE} hops=0"),
        "while [ -L \"$real\" ]; do".to_owned(),
        "  hops=$((hops + 1)); [ \"$hops\" -le 40 ] || exit 1".to_owned(),
        "  link=$(readlink \"$real\")".to_owned(),
        "  case $link in /*) real=$link ;; *) real=$(dirname \"$real\")/$link ;; esac".to_owned(),
        "done".to_owned(),
        "dir=$(cd -P \"$(dirname \"$real\")\" && pwd -P) || exit 1".to_owned(),
        "real=$dir/$(basename \"$real\")".to_owned(),
        "[ -w \"$dir\" ] || exit 1".to_owned(),
        "[ ! -e \"$real\" ] || [ -f \"$real\" ] || exit 1".to_owned(),
        // OpenSSL clients with only a CApath look up the CA by the hash of its
        // subject.
        "hash=".to_owned(),
        "if command -v openssl >/dev/null 2>&1; then".to_owned(),
        format!("  hash=$(openssl x509 -noout -subject_hash -in {CA_CERT})"),
        "  [ -n \"$hash\" ] || exit 1".to_owned(),
        // Alpine has c_rehash without openssl. Its output is not parsed: the
        // hash is the name of the one link to the CA in a private dir.
        "elif command -v c_rehash >/dev/null 2>&1; then".to_owned(),
        "  stage=$(mktemp -d)".to_owned(),
        format!("  cat {CA_CERT} > \"$stage/ca.pem\""),
        "  c_rehash \"$stage\" >/dev/null".to_owned(),
        "  for link in \"$stage\"/????????.0; do".to_owned(),
        "    [ -L \"$link\" ] && [ \"$(readlink \"$link\")\" = ca.pem ] || continue".to_owned(),
        "    name=${link##*/}".to_owned(),
        "    case ${name%.0} in *[!0-9a-f]*) continue ;; esac".to_owned(),
        "    [ -z \"$hash\" ] || exit 1".to_owned(),
        "    hash=${name%.0}".to_owned(),
        "  done".to_owned(),
        "  [ -n \"$hash\" ] || exit 1".to_owned(),
        "fi".to_owned(),
        format!("[ -z \"$hash\" ] || [ ! -e \"$certs/{CA_LINK}\" ] || [ -L \"$certs/{CA_LINK}\" ] || exit 1"),
        "prev=$(mktemp)".to_owned(),
        // The local copy of the last run is the previous CA.
        format!("[ ! -f {LOCAL_CA} ] || cat {LOCAL_CA} > \"$prev\""),
        "tmp=$(mktemp \"$dir/.zwinger-bundle.XXXXXX\")".to_owned(),
        // `cp -p` keeps the mode and owner of the old bundle. A new bundle is
        // public. All TLS of the cage goes through the proxy, so a bundle with
        // only the zwinger CA is complete.
        "if [ -f \"$real\" ]; then cp -p \"$real\" \"$tmp\"; old=$real; else chmod 0644 \"$tmp\"; old=/dev/null; fi"
            .to_owned(),
        format!("awk '{strip}' \"$prev\" {CA_CERT} \"$old\" > \"$tmp\""),
        format!("{{ echo '{BEGIN}'; cat {CA_CERT}; echo '{END}'; }} >> \"$tmp\""),
        // The local copy keeps the CA in the store for a later update of the
        // ca-certificates package. A failed run puts it back: the next run
        // must know the previous CA.
        format!("mkdir -p {local_dir}"),
        format!("local=$(mktemp {local_dir}/.zwinger-ca.XXXXXX)"),
        format!("cat {CA_CERT} > \"$local\""),
        "chmod 0644 \"$local\"".to_owned(),
        format!(
            "if [ -f {LOCAL_CA} ]; then keep=$(mktemp {local_dir}/.zwinger-keep.XXXXXX); ln -f {LOCAL_CA} \"$keep\"; fi"
        ),
        // The CApath links have the names that `update-ca-certificates` gives
        // them: `zwinger-ca.pem` to the local copy, and the hash link to it.
        // They follow the local copy, so its rename switches the CApath trust.
        "if [ -n \"$hash\" ]; then".to_owned(),
        format!("  [ -L \"$certs/{CA_LINK}\" ] || {{ ln -s {LOCAL_CA} \"$certs/{CA_LINK}\"; pem=1; }}"),
        "  found=".to_owned(),
        "  for link in \"$certs/$hash\".*; do".to_owned(),
        format!("    [ -L \"$link\" ] && [ \"$(readlink \"$link\")\" = {CA_LINK} ] || continue"),
        "    found=1".to_owned(),
        "  done".to_owned(),
        "  if [ -z \"$found\" ]; then".to_owned(),
        "    n=0".to_owned(),
        "    while [ -e \"$certs/$hash.$n\" ] || [ -L \"$certs/$hash.$n\" ]; do n=$((n + 1)); done".to_owned(),
        format!("    ln -s {CA_LINK} \"$certs/$hash.$n\""),
        "    hash_link=$certs/$hash.$n".to_owned(),
        "  fi".to_owned(),
        // A hash link of an older subject moves aside before the swap, so a
        // failed step puts it back. The trap removes it after the swap.
        "  for link in \"$certs\"/????????.*; do".to_owned(),
        format!("    [ -L \"$link\" ] && [ \"$(readlink \"$link\")\" = {CA_LINK} ] || continue"),
        "    case ${link##*/} in \"$hash\".*) continue ;; esac".to_owned(),
        "    [ -n \"$stale\" ] || stale=$(mktemp -d \"$certs/.zwinger-stale.XXXXXX\")".to_owned(),
        "    mv -f \"$link\" \"$stale/\"".to_owned(),
        "  done".to_owned(),
        "fi".to_owned(),
        format!("mv -f \"$local\" {LOCAL_CA}"),
        "swapped=1".to_owned(),
        "mv -f \"$tmp\" \"$real\"".to_owned(),
        "done=1".to_owned(),
    ]
    .join("\n");
    ["/bin/sh", "-c", &script].map(str::to_owned).into()
}

/// Writes the wrapper of each agent in `kinds` and the status hooks to the
/// host dir `dir`, which is bound at [`BIN_DIR`]. Each file is written in
/// place, so the guest sees the new text at once.
pub fn write(dir: &Path, kinds: &[AgentKind], found: Found, unprompted: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let wrappers = kinds.iter().filter_map(|&kind| Some((kind.name(), wrapper(kind, found, unprompted)?)));
    for (name, script) in wrappers {
        let path = dir.join(name);
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    if kinds.contains(&AgentKind::Claude) {
        let path = dir.join(STATUS);
        std::fs::write(&path, status_script())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        std::fs::write(dir.join(CLAUDE_STATUS), claude_status())?;
    }
    if kinds.contains(&AgentKind::Pi) {
        std::fs::write(dir.join(PI_STATUS), pi_status())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mac_agent_runs_the_detected_install_with_no_bypass_flag_and_no_env() {
        use crate::host_agents::detect::Source;
        use crate::host_agents::login::LoginState;
        use std::path::PathBuf;

        let found = |kind, package: Option<&str>, entry: Option<&str>, bin: Option<&str>| HostAgent {
            kind,
            version: "1.0.0".into(),
            path: String::new(),
            source: Source::Host,
            pinned: true,
            profile: true,
            login: LoginState::None,
            package: package.map(PathBuf::from),
            entry: entry.map(str::to_owned),
            bin: bin.map(PathBuf::from),
        };
        let claude = found(AgentKind::Claude, None, None, Some("/Users/me/.local/share/claude/versions/2.1.310"));
        let codex =
            found(AgentKind::Codex, Some("/opt/homebrew/lib/node_modules/@openai/codex"), None, Some("/usr/bin/codex"));
        let pi =
            found(AgentKind::Pi, Some("/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent"), None, None);
        let gemini = found(AgentKind::Gemini, Some("/opt/gemini"), Some("bundle/gemini.js"), None);
        let cases: [(&HostAgent, bool, &[&str]); 8] = [
            (&claude, false, &["/Users/me/.local/share/claude/versions/2.1.310"]),
            (&claude, true, &["/Users/me/.local/share/claude/versions/2.1.310", "-c"]),
            (&codex, false, &["/usr/bin/codex"]),
            (&codex, true, &["/usr/bin/codex", "resume", "--last"]),
            (&pi, false, &["node", "/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"]),
            (&pi, true, &["node", "/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js", "-c"]),
            (&gemini, false, &["node", "/opt/gemini/bundle/gemini.js"]),
            (&gemini, true, &["node", "/opt/gemini/bundle/gemini.js", "--resume", "latest"]),
        ];
        let forbidden = [
            "--dangerously-skip-permissions",
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "-y",
            "--full-auto",
            "--sandbox",
            "danger-full-access",
            "--permission-mode",
            "--approval-mode",
        ];
        for (agent, resume, expected) in cases {
            let argv = host_argv(agent, resume).unwrap();
            assert_eq!(argv, expected, "{:?} resume {resume}", agent.kind);
            for arg in &argv {
                assert!(!forbidden.iter().any(|f| arg.starts_with(f)), "{arg} in {argv:?}");
                // No zwinger wrapper, placeholder, credential or state env.
                assert!(!arg.starts_with(BIN_DIR) && !arg.contains('='), "{arg} in {argv:?}");
                for name in ["IS_SANDBOX", "HOME", "CODEX_HOME", "PI_CODING_AGENT_DIR", "zwinger"] {
                    assert!(!arg.contains(name), "{arg} in {argv:?}");
                }
            }
        }
        // A detection with no runnable form opens nothing.
        assert_eq!(host_argv(&found(AgentKind::Codex, None, None, None), false), None);
        assert_eq!(host_argv(&found(AgentKind::Gemini, Some("/opt/gemini"), None, None), false), None);
        // A relative binary would run from the session folder or by PATH.
        for bin in ["codex", "./codex", "node_modules/.bin/codex", ""] {
            assert_eq!(host_argv(&found(AgentKind::Codex, None, None, Some(bin)), false), None, "{bin:?}");
        }
    }

    const PROXY: &str = "http://zw:t@host.microsandbox.internal:4000";
    const HOST: Found =
        Found { pi_host: true, gemini: Some("/opt/zwinger/host/gemini/bundle/gemini.js"), proxy: Some(PROXY) };

    /// Runs the wrapper with `sh`, with a program that prints its env and arguments.
    fn run_with_tty_check(kind: AgentKind, unprompted: bool, args: &[&str], tty: bool) -> String {
        let script = wrapper(kind, HOST, unprompted).unwrap();
        // The test process has pipes; substitute the TTY check to exercise the interactive branch.
        let script = if tty { script.replace(TTY, "true") } else { script };
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

    fn run(kind: AgentKind, unprompted: bool, args: &[&str]) -> String {
        run_with_tty_check(kind, unprompted, args, false)
    }

    #[test]
    fn status_flags_use_the_original_user_arguments() {
        let run = |kind, args: &[&str]| run_with_tty_check(kind, true, args, true);
        assert_eq!(
            run(AgentKind::Claude, &["-c"]),
            "/opt/zwinger/state/claude\n--dangerously-skip-permissions\n--settings\n/opt/zwinger/bin/claude-status.json\n-c\n"
        );
        assert_eq!(run(AgentKind::Claude, &["mcp", "list"]), "/opt/zwinger/state/claude\nmcp\nlist\n");
        assert_eq!(
            run(AgentKind::Claude, &["-p", "hi"]),
            "/opt/zwinger/state/claude\n--dangerously-skip-permissions\n-p\nhi\n"
        );
        assert_eq!(
            run(AgentKind::Pi, &[]),
            "/opt/zwinger/state/agent\n--session-dir\n/opt/zwinger/state/sessions\n-e\n/opt/zwinger/bin/pi-status.js\n"
        );
        assert_eq!(
            run(AgentKind::Pi, &["-p", "hi"]),
            "/opt/zwinger/state/agent\n--session-dir\n/opt/zwinger/state/sessions\n-p\nhi\n"
        );
    }

    #[test]
    fn claude_wrapper_sets_its_state_and_skips_permissions() {
        assert_eq!(
            wrapper(AgentKind::Claude, Found::default(), true).unwrap(),
            "#!/bin/sh\n# zwinger writes this file at each cage start.\n\
             export IS_SANDBOX=1 HOME=/opt/zwinger/state/claude\n\
             first=$1\n\
             add=1\n\
             case \"$first\" in mcp|config|doctor|update) add= ;; esac\n\
             for arg do\n  case \"$arg\" in --dangerously-skip-permissions|--permission-mode|--permission-mode=*) add= ;; esac\ndone\n\
             status=\n[ -t 0 ] && [ -t 1 ] && status=1\n\
             case \"$first\" in agents|attach|auth|auto-mode|config|doctor|gateway|import|install|logs|mcp|plugin|plugins|purge|respawn|rm|setup-token|stop|kill|ultrareview|update|upgrade) status= ;; esac\n\
             for arg do\n  case \"$arg\" in -p|--print) status= ;; esac\ndone\n\
             [ -n \"$status\" ] && set -- --settings /opt/zwinger/bin/claude-status.json \"$@\"\n\
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
        trust_at(root, tools, &root.join("bundle.crt"))
    }

    /// [`trust`] with the bundle at `bundle`.
    fn trust_at(root: &Path, tools: &Path, bundle: &Path) -> std::process::Output {
        let script = trust_argv()[2]
            .replace(CA_CERT, &root.join("ca.pem").display().to_string())
            .replace("/usr/local/share", &root.join("local").display().to_string())
            .replace(CA_BUNDLE, &bundle.display().to_string());
        std::process::Command::new("/bin/sh")
            .env_clear()
            .env("PATH", format!("{}:{}", tools.display(), root.join("bin").display()))
            .arg("-c")
            .arg(script)
            .output()
            .unwrap()
    }

    /// A temp root with `tools` and `bin`. `bin` has only the commands that
    /// the trust script needs, so a host openssl or updater is not on `PATH`.
    fn trust_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("zwinger-trust-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let commands =
            ["awk", "basename", "cat", "chmod", "cp", "dirname", "ln", "mkdir", "mktemp", "mv", "readlink", "rm"];
        for name in commands {
            let found = ["/usr/bin", "/bin"].map(|dir| Path::new(dir).join(name)).into_iter().find(|p| p.exists());
            std::os::unix::fs::symlink(found.expect(name), root.join("bin").join(name)).unwrap();
        }
        root
    }

    /// Writes the CA of a run with the key `key`, and runs the trust script.
    fn trust_with(root: &Path, key: &str) -> String {
        std::fs::write(root.join("ca.pem"), pem(key)).unwrap();
        let out = trust(root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        std::fs::read_to_string(root.join("bundle.crt")).unwrap()
    }

    /// The bundle has one marked block, with the newest key, and has the
    /// newest key once and the old key nowhere.
    fn assert_one_block(bundle: &str, key: &str, old: &str) {
        assert!(!bundle.contains(old), "{bundle}");
        assert_eq!(bundle.matches(key).count(), 1, "{bundle}");
        assert_eq!(bundle.matches(BEGIN).count(), 1, "{bundle}");
        assert_eq!(bundle.matches(END).count(), 1, "{bundle}");
        let block = &bundle[bundle.find(BEGIN).unwrap()..];
        assert!(block.contains(key), "{bundle}");
        assert!(!block.contains(old), "{bundle}");
        assert!(block.ends_with(&format!("{END}\n")), "{bundle}");
    }

    fn pem(key: &str) -> String {
        format!("-----BEGIN CERTIFICATE-----\n{key}\n-----END CERTIFICATE-----\n")
    }

    /// The names in `dir` of the temp files of the trust script.
    fn temp_files(dir: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().starts_with(".zwinger-"))
            .collect()
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn executable(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn trust_replaces_the_target_of_relative_symlinks() {
        for two_hops in [false, true] {
            let root = trust_root();
            let roots = root.join("roots");
            std::fs::create_dir_all(&roots).unwrap();
            std::fs::write(roots.join("active.crt"), "SYSTEM ROOTS\n").unwrap();
            if two_hops {
                std::os::unix::fs::symlink("active.crt", roots.join("middle.crt")).unwrap();
                std::os::unix::fs::symlink("roots/middle.crt", root.join("bundle.crt")).unwrap();
            } else {
                std::os::unix::fs::symlink("roots/active.crt", root.join("bundle.crt")).unwrap();
            }
            let links: Vec<_> = [root.join("bundle.crt"), roots.join("middle.crt")]
                .into_iter()
                .filter_map(|p| Some((std::fs::read_link(&p).ok()?, p)))
                .collect();
            trust_with(&root, "KEY1");
            let bundle = trust_with(&root, "KEY2");
            assert!(bundle.starts_with("SYSTEM ROOTS\n"), "{bundle}");
            assert_one_block(&bundle, "KEY2", "KEY1");
            for (target, link) in links {
                assert_eq!(std::fs::read_link(&link).unwrap(), target, "{}", link.display());
            }
            assert_eq!(
                std::fs::canonicalize(root.join("bundle.crt")).unwrap(),
                std::fs::canonicalize(roots.join("active.crt")).unwrap()
            );
            assert_eq!(mode(&roots.join("active.crt")), 0o644);
            let local = root.join("local/ca-certificates");
            assert_eq!(std::fs::read_to_string(local.join("zwinger-ca.crt")).unwrap(), pem("KEY2"));
            assert_eq!(mode(&local.join("zwinger-ca.crt")), 0o644);
            for dir in [&root, &roots, &local] {
                assert!(temp_files(dir).is_empty(), "{}", dir.display());
            }
        }
    }

    #[test]
    fn trust_keeps_the_mode_of_the_bundle() {
        use std::os::unix::fs::PermissionsExt;

        let root = trust_root();
        let bundle = root.join("bundle.crt");
        std::fs::write(&bundle, "SYSTEM ROOTS\n").unwrap();
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o640)).unwrap();
        trust_with(&root, "KEY1");
        assert_eq!(mode(&bundle), 0o640);
    }

    #[test]
    fn trust_makes_a_bundle_with_only_the_ca() {
        let root = trust_root();
        let bundle = trust_with(&root, "KEY1");
        assert_eq!(bundle, format!("{BEGIN}\n{}{END}\n", pem("KEY1")));
        assert_eq!(mode(&root.join("bundle.crt")), 0o644);
        // A later install of ca-certificates picks it up.
        assert!(root.join("local/ca-certificates/zwinger-ca.crt").exists());
    }

    #[test]
    fn trust_makes_the_bundle_dir_on_a_fresh_image() {
        let root = trust_root();
        let bundle = root.join("etc/ssl/certs/ca-certificates.crt");
        std::fs::write(root.join("ca.pem"), pem("KEY1")).unwrap();
        let out = trust_at(&root, &root.join("tools"), &bundle);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read_to_string(&bundle).unwrap(), format!("{BEGIN}\n{}{END}\n", pem("KEY1")));
        assert_eq!(mode(&bundle), 0o644);
    }

    #[test]
    fn trust_makes_the_missing_target_of_a_link() {
        let root = trust_root();
        std::fs::create_dir_all(root.join("roots")).unwrap();
        std::os::unix::fs::symlink("roots/active.crt", root.join("bundle.crt")).unwrap();
        let bundle = trust_with(&root, "KEY1");
        assert_eq!(bundle, format!("{BEGIN}\n{}{END}\n", pem("KEY1")));
        assert_eq!(std::fs::read_link(root.join("bundle.crt")).unwrap(), Path::new("roots/active.crt"));
        assert_eq!(mode(&root.join("roots/active.crt")), 0o644);
        assert!(temp_files(&root.join("roots")).is_empty());
    }

    #[test]
    fn trust_stops_at_a_link_into_a_missing_dir() {
        let root = trust_root();
        std::os::unix::fs::symlink("roots/active.crt", root.join("bundle.crt")).unwrap();
        std::fs::write(root.join("ca.pem"), pem("KEY1")).unwrap();
        assert!(!trust(&root, &root.join("tools")).status.success());
        assert!(!root.join("roots").exists());
        assert!(!root.join("local").exists());
        assert_eq!(std::fs::read_link(root.join("bundle.crt")).unwrap(), Path::new("roots/active.crt"));
    }

    #[test]
    fn trust_stops_when_a_target_is_a_dir() {
        for target in ["roots/active.crt", "local/ca-certificates/zwinger-ca.crt"] {
            let root = trust_root();
            std::fs::create_dir_all(root.join(target)).unwrap();
            std::fs::create_dir_all(root.join("roots")).unwrap();
            if !root.join("roots/active.crt").exists() {
                std::fs::write(root.join("roots/active.crt"), "SYSTEM ROOTS\n").unwrap();
            }
            std::os::unix::fs::symlink("roots/active.crt", root.join("bundle.crt")).unwrap();
            std::fs::write(root.join("ca.pem"), pem("KEY1")).unwrap();
            assert!(!trust(&root, &root.join("tools")).status.success(), "{target}");
            assert_eq!(std::fs::read_dir(root.join(target)).unwrap().count(), 0, "{target}");
            if target.starts_with("local") {
                assert_eq!(std::fs::read_to_string(root.join("bundle.crt")).unwrap(), "SYSTEM ROOTS\n");
            }
        }
    }

    #[test]
    fn trust_rotation_keeps_each_root_once_and_one_block() {
        let root = trust_root();
        std::fs::write(root.join("bundle.crt"), format!("SYSTEM ROOTS\n{}", pem("OTHER"))).unwrap();
        trust_with(&root, "KEY1");
        // An updater copied the local copy outside the block.
        let mut text = std::fs::read_to_string(root.join("bundle.crt")).unwrap();
        text.insert_str(0, &pem("KEY1"));
        std::fs::write(root.join("bundle.crt"), text).unwrap();
        trust_with(&root, "KEY2");
        let bundle = trust_with(&root, "KEY3");
        assert_eq!(bundle.matches("SYSTEM ROOTS").count(), 1, "{bundle}");
        assert_eq!(bundle.matches("OTHER").count(), 1, "{bundle}");
        assert!(!bundle.contains("KEY1") && !bundle.contains("KEY2"), "{bundle}");
        assert_one_block(&bundle, "KEY3", "KEY2");
    }

    #[test]
    fn trust_keeps_the_old_text_for_an_open_reader() {
        use std::io::Read;

        let root = trust_root();
        std::fs::write(root.join("bundle.crt"), "SYSTEM ROOTS\n").unwrap();
        let old = trust_with(&root, "KEY1");
        let mut reader = std::fs::File::open(root.join("bundle.crt")).unwrap();
        trust_with(&root, "KEY2");
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, old);
    }

    #[test]
    fn trust_error_keeps_the_old_bundle() {
        let root = trust_root();
        std::fs::write(root.join("bundle.crt"), "SYSTEM ROOTS\n").unwrap();
        let old = trust_with(&root, "KEY1");
        executable(&root.join("tools/awk"), "#!/bin/sh\necho partial\nexit 71\n");
        std::fs::write(root.join("ca.pem"), pem("KEY2")).unwrap();
        let out = trust(&root, &root.join("tools"));
        assert!(!out.status.success());
        assert_eq!(std::fs::read_to_string(root.join("bundle.crt")).unwrap(), old);
        assert!(temp_files(&root).is_empty());
        assert!(temp_files(&root.join("local/ca-certificates")).is_empty());
    }

    #[test]
    fn trust_rename_error_keeps_the_previous_local_copy() {
        let root = trust_root();
        let bundle = root.join("bundle.crt");
        let local = root.join("local/ca-certificates/zwinger-ca.crt");
        // An updater rebuilt the bundle: the previous CA has no block.
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, pem("KEY1")).unwrap();
        let old = format!("SYSTEM ROOTS\n{}", pem("KEY1"));
        std::fs::write(&bundle, &old).unwrap();
        let mv = root.join("tools/mv");
        executable(
            &mv,
            &format!(
                "#!/bin/sh\nfor last do :; done\n[ \"$last\" -ef '{}' ] && exit 1\nexec /bin/mv \"$@\"\n",
                bundle.display()
            ),
        );
        std::fs::write(root.join("ca.pem"), pem("KEY2")).unwrap();
        assert!(!trust(&root, &root.join("tools")).status.success());
        assert_eq!(std::fs::read_to_string(&bundle).unwrap(), old);
        assert_eq!(std::fs::read_to_string(&local).unwrap(), pem("KEY1"));
        assert!(temp_files(&root).is_empty());
        assert!(temp_files(local.parent().unwrap()).is_empty());
        std::fs::remove_file(mv).unwrap();
        assert_eq!(trust_with(&root, "KEY2"), format!("SYSTEM ROOTS\n{BEGIN}\n{}{END}\n", pem("KEY2")));
    }

    /// Runs openssl with `args` in `dir`.
    fn openssl(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("openssl").current_dir(dir).args(args).output().unwrap()
    }

    #[test]
    fn trust_adds_the_ca_to_the_hashed_dir() {
        if std::process::Command::new("openssl").arg("version").output().is_err() {
            return;
        }
        let root = trust_root();
        let path = String::from_utf8(
            std::process::Command::new("sh").args(["-c", "command -v openssl"]).output().unwrap().stdout,
        )
        .unwrap();
        std::os::unix::fs::symlink(path.trim(), root.join("tools/openssl")).unwrap();
        // Each app run has a new key with the same subject. The subject is
        // unique to this test, so no other trusted CA has it.
        let subject = format!("/CN=zwinger test CA {}", uuid::Uuid::new_v4());
        let make_as = |name: &str, subject: &str| {
            let args = ["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-days", "1"];
            let out = openssl(&root, &[&args[..], &["-subj", subject, "-keyout", "/dev/null", "-out", name]].concat());
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        let make = |name: &str| make_as(name, &subject);
        let trusted =
            |name: &str, store: &[&str]| openssl(&root, &[&["verify"][..], store, &[name]].concat()).status.success();
        // Each check names both trust locations, so no default location is
        // used. LibreSSL has no `-no-CAfile` or `-no-CApath`.
        make_as("none.pem", &format!("/CN=no test CA {}", uuid::Uuid::new_v4()));
        std::fs::create_dir(root.join("empty")).unwrap();
        let capath = ["-CAfile", "none.pem", "-CApath", "."];
        let cafile = ["-CAfile", "bundle.crt", "-CApath", "empty"];
        make("ca.pem");
        let hash = openssl(&root, &["x509", "-noout", "-subject_hash", "-in", "ca.pem"]).stdout;
        let hash = String::from_utf8(hash).unwrap();
        let hash = hash.trim();
        // An unrelated CA has the first link name of this hash.
        make_as("other.pem", &format!("/CN=unrelated test CA {}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink("other.pem", root.join(format!("{hash}.0"))).unwrap();
        std::fs::copy(root.join("ca.pem"), root.join("first.pem")).unwrap();
        assert!(!trusted("first.pem", &capath));
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(trusted("first.pem", &capath));
        assert!(trusted("first.pem", &cafile));

        // A failed hash keeps the old trust.
        make("ca.pem");
        let bundle = std::fs::read(root.join("bundle.crt")).unwrap();
        std::fs::remove_file(root.join("tools/openssl")).unwrap();
        executable(&root.join("tools/openssl"), "#!/bin/sh\nexit 73\n");
        assert!(!trust(&root, &root.join("tools")).status.success());
        assert_eq!(std::fs::read(root.join("bundle.crt")).unwrap(), bundle);
        assert!(trusted("first.pem", &capath));
        assert!(!trusted("ca.pem", &capath));

        std::fs::remove_file(root.join("tools/openssl")).unwrap();
        std::os::unix::fs::symlink(path.trim(), root.join("tools/openssl")).unwrap();
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(trusted("ca.pem", &capath));
        assert!(trusted("ca.pem", &cafile));
        assert!(!trusted("first.pem", &capath));
        assert!(!trusted("first.pem", &cafile));
        assert_eq!(std::fs::read_link(root.join(format!("{hash}.0"))).unwrap(), Path::new("other.pem"));
        assert_eq!(std::fs::read_link(root.join(format!("{hash}.1"))).unwrap(), Path::new(CA_LINK));
        assert!(!root.join(format!("{hash}.2")).exists());

        // A new subject: a failed bundle rename keeps the old hash link.
        std::fs::copy(root.join("ca.pem"), root.join("prev.pem")).unwrap();
        let subject = format!("/CN=zwinger next CA {}", uuid::Uuid::new_v4());
        make_as("ca.pem", &subject);
        std::fs::copy(root.join("ca.pem"), root.join("next.pem")).unwrap();
        let bundle = std::fs::read(root.join("bundle.crt")).unwrap();
        let mv = root.join("tools/mv");
        executable(
            &mv,
            &format!(
                "#!/bin/sh\nfor last do :; done\n[ \"$last\" -ef '{}' ] && exit 1\nexec /bin/mv \"$@\"\n",
                root.join("bundle.crt").display()
            ),
        );
        assert!(!trust(&root, &root.join("tools")).status.success());
        assert_eq!(std::fs::read(root.join("bundle.crt")).unwrap(), bundle);
        assert_eq!(std::fs::read_link(root.join(format!("{hash}.1"))).unwrap(), Path::new(CA_LINK));
        assert!(temp_files(&root).is_empty());
        assert!(trusted("prev.pem", &capath));
        std::fs::remove_file(&mv).unwrap();
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(trusted("next.pem", &capath));
        assert!(!root.join(format!("{hash}.1")).exists());
        assert_eq!(std::fs::read_link(root.join(format!("{hash}.0"))).unwrap(), Path::new("other.pem"));
        assert!(temp_files(&root).is_empty());
    }

    #[test]
    fn trust_adds_the_ca_to_the_hashed_dir_with_c_rehash() {
        let Ok(out) = std::process::Command::new("sh").args(["-c", "command -v openssl"]).output() else {
            return;
        };
        let path = String::from_utf8(out.stdout).unwrap();
        if path.trim().is_empty() {
            return;
        }
        let root = trust_root();
        let staged = root.join("staged");
        // Alpine's c_rehash: one relative link per cert, and no openssl on
        // `PATH`.
        let rehash = format!(
            "#!/bin/sh\necho \"$1\" > '{}'\nh=$('{}' x509 -noout -subject_hash -in \"$1/ca.pem\") || exit 1\nln -s ca.pem \"$1/$h.0\"\n",
            staged.display(),
            path.trim()
        );
        executable(&root.join("tools/c_rehash"), &rehash);
        let make = |name: &str, subject: &str| {
            let args = ["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-days", "1"];
            let out = openssl(&root, &[&args[..], &["-subj", subject, "-keyout", "/dev/null", "-out", name]].concat());
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        // An unrelated CAfile, so only the CApath can trust the CA.
        make("none.pem", &format!("/CN=no test CA {}", uuid::Uuid::new_v4()));
        let trusted =
            |name: &str| openssl(&root, &["verify", "-CAfile", "none.pem", "-CApath", ".", name]).status.success();
        let hash_links = || {
            let mut links: Vec<_> = std::fs::read_dir(&root)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".0"))
                .map(|e| (e.file_name(), std::fs::read_link(e.path()).unwrap()))
                .collect();
            links.sort();
            links
        };
        let local = root.join("local/ca-certificates");
        let clean = || {
            assert!(temp_files(&root).is_empty());
            assert!(temp_files(&local).is_empty());
            let stage = std::fs::read_to_string(&staged).unwrap();
            assert!(!Path::new(stage.trim()).exists(), "{stage}");
        };
        let subject = format!("/CN=zwinger test CA {}", uuid::Uuid::new_v4());
        make("ca.pem", &subject);
        std::fs::copy(root.join("ca.pem"), root.join("first.pem")).unwrap();
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(trusted("first.pem"));
        clean();

        // A rotated CA with the same subject.
        make("ca.pem", &subject);
        std::fs::copy(root.join("ca.pem"), root.join("second.pem")).unwrap();
        let out = trust(&root, &root.join("tools"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(trusted("second.pem"));
        assert!(!trusted("first.pem"));
        assert_eq!(hash_links().len(), 1);
        clean();

        // A failed helper or a helper with no link keeps the old trust.
        let failing = [
            "#!/bin/sh\nexit 73\n".to_owned(),
            format!("#!/bin/sh\necho \"$1\" > '{}'\necho 'ca.pem => 0123abcd.0'\n", staged.display()),
        ];
        make("ca.pem", &format!("/CN=zwinger next CA {}", uuid::Uuid::new_v4()));
        let bundle = std::fs::read(root.join("bundle.crt")).unwrap();
        let copy = std::fs::read(local.join("zwinger-ca.crt")).unwrap();
        let links = hash_links();
        for helper in failing {
            std::fs::write(&staged, "").unwrap();
            executable(&root.join("tools/c_rehash"), &helper);
            assert!(!trust(&root, &root.join("tools")).status.success(), "{helper}");
            assert_eq!(std::fs::read(root.join("bundle.crt")).unwrap(), bundle);
            assert_eq!(std::fs::read(local.join("zwinger-ca.crt")).unwrap(), copy);
            assert_eq!(hash_links(), links);
            assert!(trusted("second.pem"));
            assert!(!trusted("ca.pem"));
            assert!(temp_files(&root).is_empty());
            assert!(temp_files(&local).is_empty());
            let stage = std::fs::read_to_string(&staged).unwrap();
            assert!(stage.trim().is_empty() || !Path::new(stage.trim()).exists(), "{stage}");
        }
    }

    #[test]
    fn trust_does_not_run_update_ca_certificates() {
        let root = trust_root();
        let bundle = root.join("bundle.crt");
        let called = root.join("called");
        // A harmless updater that appends, and a Debian-style updater that
        // renames its output over the link.
        let updaters = [
            format!(
                "#!/bin/sh\n: > '{}'\ncat '{}' >> '{}'\n",
                called.display(),
                root.join("ca.pem").display(),
                bundle.display()
            ),
            format!(
                "#!/bin/sh\n: > '{c}'\ncat '{b}' > '{b}.new'\nmv -f '{b}.new' '{b}'\n",
                c = called.display(),
                b = bundle.display()
            ),
        ];
        for updater in updaters {
            let _ = std::fs::remove_file(&bundle);
            std::fs::write(root.join("roots.crt"), "SYSTEM ROOTS\n").unwrap();
            std::os::unix::fs::symlink("roots.crt", &bundle).unwrap();
            executable(&root.join("tools/update-ca-certificates"), &updater);
            trust_with(&root, "KEY1");
            let text = trust_with(&root, "KEY2");
            assert_one_block(&text, "KEY2", "KEY1");
            assert!(!called.exists());
            assert_eq!(std::fs::read_link(&bundle).unwrap(), Path::new("roots.crt"));
        }
    }

    #[test]
    fn trust_stops_at_a_symlink_cycle() {
        let root = trust_root();
        std::os::unix::fs::symlink("loop.crt", root.join("bundle.crt")).unwrap();
        std::os::unix::fs::symlink("bundle.crt", root.join("loop.crt")).unwrap();
        std::fs::write(root.join("ca.pem"), pem("KEY1")).unwrap();
        let names = || {
            let mut names: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
            names.sort();
            names
        };
        let before = names();
        let out = trust(&root, &root.join("tools"));
        assert!(!out.status.success());
        assert_eq!(names(), before);
        assert!(!root.join("local").exists());
        assert_eq!(std::fs::read_link(root.join("bundle.crt")).unwrap(), Path::new("loop.crt"));
    }

    #[test]
    fn claude_status_hooks_give_one_state_per_event() {
        let settings: serde_json::Value = serde_json::from_str(&claude_status()).unwrap();
        let hooks = settings["hooks"].as_object().unwrap();
        let command = |event: &str| {
            let groups = hooks[event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event}: one hook group, no race");
            assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1, "{event}: one hook");
            groups[0]["hooks"][0]["command"].as_str().unwrap().to_owned()
        };
        let state = |name: &str| format!("{BIN_DIR}/{STATUS} {name}");
        assert_eq!(command("UserPromptSubmit"), state("active"));
        // No matcher: the hook runs for each tool and reads the tool name.
        assert!(hooks["PreToolUse"][0].get("matcher").is_none());
        assert_eq!(command("PreToolUse"), state(TOOL));
        assert_eq!(command("PostToolUse"), state("active"));
        assert_eq!(command("Notification"), state("blocked"));
        assert_eq!(hooks["Notification"][0]["matcher"], "permission_prompt|elicitation_dialog");
        assert_eq!(command("Stop"), state("done"));
    }

    #[test]
    fn status_script_picks_the_state_from_the_tool_name() {
        let tty = std::env::temp_dir().join(format!("zwinger-tty-{}", uuid::Uuid::new_v4()));
        let script = status_script().replace("/dev/tty", &tty.display().to_string());
        let run = |arg: &str, input: &str| {
            let mut child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .arg("sh")
                .arg(arg)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
            let out = std::fs::read(&tty).unwrap();
            String::from_utf8(out).unwrap()
        };
        let osc = |state: &str| format!("\x1b{}{state}\x07", agent_osc());
        assert_eq!(run(TOOL, r#"{"tool_name":"AskUserQuestion","tool_input":{}}"#), osc("blocked"));
        assert_eq!(run(TOOL, r#"{"tool_name": "AskUserQuestion"}"#), osc("blocked"));
        assert_eq!(run(TOOL, r#"{"tool_name":"Bash","tool_input":{"command":"AskUserQuestion"}}"#), osc("active"));
        assert_eq!(run(TOOL, r#"{"tool_name":"Read"}"#), osc("active"));
        assert_eq!(run("done", ""), osc("done"));
        std::fs::remove_file(tty).unwrap();
    }

    #[test]
    fn write_makes_executable_wrappers_for_the_given_agents() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("zwinger-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, &[AgentKind::Pi, AgentKind::Claude, AgentKind::Gemini], Found::default(), true).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        names.sort();
        assert_eq!(names, ["claude", "claude-status.json", "pi", "pi-status.js", "zwinger-agent-status"]);
        let mode = std::fs::metadata(dir.join("claude")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(std::fs::metadata(dir.join(STATUS)).unwrap().permissions().mode() & 0o777, 0o755);
        let hooks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(CLAUDE_STATUS)).unwrap()).unwrap();
        assert_eq!(hooks["hooks"]["Stop"][0]["hooks"][0]["command"], format!("{BIN_DIR}/{STATUS} done"));
        let pi = std::fs::read_to_string(dir.join(PI_STATUS)).unwrap();
        assert!(pi.contains("agent_start") && pi.contains("agent_settled"));
        write(&dir, &[AgentKind::Claude], Found::default(), false).unwrap();
        assert!(!std::fs::read_to_string(dir.join("claude")).unwrap().contains("dangerously"));
        let empty = std::env::temp_dir().join(format!("zwinger-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty).unwrap();
        write(&empty, &[], Found::default(), false).unwrap();
        assert_eq!(std::fs::read_dir(empty).unwrap().count(), 0);
    }
}
