//! Finds the host agents in their known install paths only. It spawns no
//! login shell: the app's `PATH` is short, so the npm and Homebrew folders
//! are checked by name.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::login::{self, LoginSource, LoginState};
use super::{HostFs, profile};
use crate::agent::{AgentKind, pi};
use crate::runtimes;

/// Where the shield's copy of the agent comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The host install, mounted read-only (pi, Gemini CLI).
    Host,
    /// The cached Linux build of the host version (Claude Code, Codex).
    Cached,
    /// The pinned runtime, until the Linux build of the host version is
    /// downloaded.
    Pinned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAgent {
    pub kind: AgentKind,
    pub version: String,
    /// The host path of the install, for display.
    pub path: String,
    pub source: Source,
    /// zwinger pins a hash for the Linux build of this version. Without a
    /// pin, the download needs one confirm.
    pub pinned: bool,
    /// The host has a config dir that the profile can copy.
    pub profile: bool,
    pub login: LoginState,
    /// The canonical package dir of a JS agent, which a shield mounts.
    #[serde(skip)]
    pub package: Option<PathBuf>,
    /// Gemini CLI's entry point, relative to its package dir.
    #[serde(skip)]
    pub entry: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub agents: Vec<HostAgent>,
    /// Antigravity is installed. It is not supported yet.
    pub antigravity: bool,
}

impl Detection {
    pub fn agent(&self, kind: AgentKind) -> Option<&HostAgent> {
        self.agents.iter().find(|a| a.kind == kind)
    }
}

/// The global `node_modules` folders of npm, Homebrew's node, nvm, bun and pnpm.
fn npm_roots(fs: &dyn HostFs) -> Vec<PathBuf> {
    let home = fs.home();
    let mut roots = vec![
        home.join(".npm-global/lib/node_modules"),
        PathBuf::from("/opt/homebrew/lib/node_modules"),
        PathBuf::from("/usr/local/lib/node_modules"),
        home.join(".bun/install/global/node_modules"),
        home.join("Library/pnpm/global/5/node_modules"),
    ];
    let nvm = home.join(".nvm/versions/node");
    let mut versions = fs.list_dir(&nvm);
    // Newest first, by the numbers of `v22.19.0`.
    versions.sort_by_key(|v| std::cmp::Reverse(numbers(v)));
    roots.extend(versions.iter().map(|v| nvm.join(v).join("lib/node_modules")));
    roots
}

fn numbers(version: &str) -> Vec<u64> {
    version.trim_start_matches('v').split('.').map(|n| n.parse().unwrap_or(0)).collect()
}

/// The first `x.y.z` in the text.
fn version_in(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .map(|w| w.trim_start_matches('v'))
        .find(|w| {
            let parts: Vec<&str> = w.split(['.', '-']).collect();
            parts.len() >= 3 && parts[..3].iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_owned)
}

/// The canonical dir and the version of an npm package that has `entry`.
fn package(fs: &dyn HostFs, dir: &Path, entry: &str) -> Option<(PathBuf, String)> {
    let manifest: Value = serde_json::from_str(&fs.read_to_string(&dir.join("package.json"))?).ok()?;
    let version = version_in(manifest["version"].as_str()?)?;
    if !entry.is_empty() && !fs.is_file(&dir.join(entry)) {
        return None;
    }
    Some((fs.canonicalize(dir)?, version))
}

fn npm(fs: &dyn HostFs, name: &str, entry: &str) -> Option<(PathBuf, String)> {
    npm_roots(fs).iter().find_map(|root| package(fs, &root.join(name), entry))
}

/// The npm package, and the version from `codex --version`: of the npm link
/// next to the package (`<prefix>/bin/codex` for
/// `<prefix>/lib/node_modules`), else of `codex` on `PATH`.
fn codex(fs: &dyn HostFs) -> Option<(PathBuf, String)> {
    let (root, path) = npm_roots(fs).into_iter().find_map(|root| {
        let (path, _) = package(fs, &root.join("@openai/codex"), "")?;
        Some((root, path))
    })?;
    let mut bins: Vec<PathBuf> = root.parent().and_then(Path::parent).map(|p| p.join("bin/codex")).into_iter().collect();
    bins.extend(fs.path_dirs().into_iter().map(|d| d.join("codex")));
    let version = bins.iter().filter(|bin| fs.is_file(bin)).find_map(|bin| version_in(&fs.version(bin)?))?;
    Some((path, version))
}

/// The native installer's link (`~/.local/bin/claude` → `versions/<v>`),
/// then `PATH`. `--version` must run.
fn claude(fs: &dyn HostFs) -> Option<(PathBuf, String)> {
    let link = fs.home().join(".local/bin/claude");
    let mut bins = Vec::new();
    if let Some(target) = fs.read_link(&link) {
        let target = if target.is_absolute() { target } else { link.parent()?.join(target) };
        bins.push(target);
    }
    bins.extend(fs.path_dirs().into_iter().map(|d| d.join("claude")));
    bins.into_iter()
        .filter(|bin| fs.is_file(bin))
        .find_map(|bin| Some((bin.clone(), version_in(&fs.version(&bin)?)?)))
}

/// Gemini CLI's entry points when `package.json` names none that exists:
/// the bundle of newer releases, then the build of older ones.
const GEMINI_ENTRIES: [&str; 2] = ["bundle/gemini.js", "dist/index.js"];

/// The npm package, else Homebrew's formula.
fn gemini(fs: &dyn HostFs) -> Option<(PathBuf, String, String)> {
    let name = "@google/gemini-cli";
    let brew = ["/opt/homebrew/opt/gemini-cli", "/usr/local/opt/gemini-cli"]
        .iter()
        .map(|opt| Path::new(opt).join("libexec/lib/node_modules"));
    npm_roots(fs).into_iter().chain(brew).find_map(|root| gemini_package(fs, &root.join(name)))
}

/// The canonical dir, the version and the entry point of the Gemini CLI
/// package. The entry point is `bin.gemini` of `package.json` (a string or
/// a map), else a known path; it must be a file inside the package.
fn gemini_package(fs: &dyn HostFs, dir: &Path) -> Option<(PathBuf, String, String)> {
    let manifest: Value = serde_json::from_str(&fs.read_to_string(&dir.join("package.json"))?).ok()?;
    let version = version_in(manifest["version"].as_str()?)?;
    let root = fs.canonicalize(dir)?;
    let named = match &manifest["bin"] {
        Value::String(bin) => Some(bin.as_str()),
        bin => bin["gemini"].as_str(),
    };
    let entry = named.into_iter().chain(GEMINI_ENTRIES).find_map(|entry| inside(fs, &root, entry))?;
    Some((root, version, entry))
}

/// The path of the file `entry` relative to `root`, after links, if it is
/// a file below `root`.
fn inside(fs: &dyn HostFs, root: &Path, entry: &str) -> Option<String> {
    let mut rel = PathBuf::new();
    for part in Path::new(entry).components() {
        match part {
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let file = fs.canonicalize(&root.join(rel))?;
    let rel = file.strip_prefix(root).ok()?;
    (fs.is_file(&file) && !rel.as_os_str().is_empty()).then(|| rel.to_string_lossy().into_owned())
}

/// Looks for each host agent and reads what each one has. `data` is the app
/// data dir, where the Linux builds are cached.
pub fn detect(fs: &dyn HostFs, logins: &dyn LoginSource, now: i64, data: &Path) -> Detection {
    let mut agents = Vec::new();
    for kind in super::KINDS {
        let found = match kind {
            AgentKind::Claude => claude(fs).map(|(path, version)| (path, version, None)),
            AgentKind::Codex => codex(fs).map(|(path, version)| (path, version, None)),
            AgentKind::Pi => npm(fs, "@earendil-works/pi-coding-agent", pi::HOST_ENTRY).map(|(path, version)| (path, version, None)),
            AgentKind::Gemini => gemini(fs).map(|(path, version, entry)| (path, version, Some(entry))),
        };
        let Some((path, version, entry)) = found else {
            continue;
        };
        let native = matches!(kind, AgentKind::Claude | AgentKind::Codex);
        let source = match native {
            false => Source::Host,
            true if runtimes::is_cached(data, kind.name(), &version) => Source::Cached,
            true => Source::Pinned,
        };
        agents.push(HostAgent {
            kind,
            path: path.display().to_string(),
            source,
            pinned: !native || runtimes::is_pinned(kind.name(), &version),
            profile: fs.is_dir(&fs.home().join(profile::config_dir(kind))),
            login: login::state(logins, kind, now),
            package: (!native).then_some(path),
            entry,
            version,
        });
    }
    let antigravity = [PathBuf::from("/Applications/Antigravity.app"), fs.home().join("Applications/Antigravity.app")]
        .iter()
        .any(|app| fs.is_dir(app));
    Detection { agents, antigravity }
}

#[cfg(test)]
mod tests {
    use super::super::fake::FakeFs;
    use super::super::login::fake::{FakeLogins, claude_doc};
    use super::*;

    fn home() -> FakeFs {
        FakeFs {
            home: PathBuf::from("/Users/me"),
            path: vec![PathBuf::from("/usr/bin")],
            ..FakeFs::default()
        }
    }

    fn data() -> PathBuf {
        std::env::temp_dir().join(format!("zwinger-detect-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn finds_nothing_on_an_empty_mac() {
        let found = detect(&home(), &FakeLogins::default(), 0, &data());
        assert_eq!(found, Detection::default());
    }

    #[test]
    fn finds_each_agent_in_its_known_path() {
        let mut fs = home();
        let versions = PathBuf::from("/Users/me/.local/share/claude/versions/2.1.310");
        fs.links.insert("/Users/me/.local/bin/claude".into(), versions.clone());
        fs.versions.insert(versions.clone(), "2.1.310 (Claude Code)\n".into());
        fs.dirs.insert("/Users/me/.claude".into());
        fs.file("/opt/homebrew/lib/node_modules/@openai/codex/package.json", r#"{"version":"0.160.0"}"#);
        // The version comes from `codex --version` of the npm link.
        fs.versions.insert("/opt/homebrew/bin/codex".into(), "codex-cli 0.163.0\n".into());
        // nvm: the newest node wins.
        let pi = "/Users/me/.nvm/versions/node/v22.19.0/lib/node_modules/@earendil-works/pi-coding-agent";
        fs.file(&format!("{pi}/package.json"), r#"{"version":"1.4.0"}"#);
        fs.file(&format!("{pi}/dist/cli.js"), "");
        let old = "/Users/me/.nvm/versions/node/v20.1.0/lib/node_modules/@earendil-works/pi-coding-agent";
        fs.file(&format!("{old}/package.json"), r#"{"version":"1.0.0"}"#);
        fs.file(&format!("{old}/dist/cli.js"), "");
        // Homebrew's formula, when npm has none, with the entry point of `bin`.
        let brew = "/opt/homebrew/opt/gemini-cli/libexec/lib/node_modules/@google/gemini-cli";
        fs.file(&format!("{brew}/package.json"), r#"{"version":"0.64.0","bin":{"gemini":"bundle/gemini.js"}}"#);
        fs.file(&format!("{brew}/bundle/gemini.js"), "");
        fs.dirs.insert("/Applications/Antigravity.app".into());

        let logins = FakeLogins::default();
        logins.docs.lock().unwrap().insert(AgentKind::Claude, claude_doc("t", 5));
        let data = data();
        let found = detect(&fs, &logins, 10, &data);
        let summary: Vec<_> = found.agents.iter().map(|a| (a.kind, a.version.as_str(), a.source, a.pinned)).collect();
        assert_eq!(
            summary,
            [
                (AgentKind::Claude, "2.1.310", Source::Pinned, false),
                (AgentKind::Codex, "0.163.0", Source::Pinned, true),
                (AgentKind::Pi, "1.4.0", Source::Host, true),
                (AgentKind::Gemini, "0.64.0", Source::Host, true),
            ]
        );
        let claude = found.agent(AgentKind::Claude).unwrap();
        assert_eq!((claude.path.as_str(), claude.profile), ("/Users/me/.local/share/claude/versions/2.1.310", true));
        assert_eq!(claude.login, LoginState::Found { expires: None });
        assert_eq!(claude.package, None);
        assert_eq!(found.agent(AgentKind::Pi).unwrap().package.as_deref(), Some(Path::new(pi)));
        assert_eq!(found.agent(AgentKind::Gemini).unwrap().entry.as_deref(), Some("bundle/gemini.js"));
        assert_eq!(found.agent(AgentKind::Codex).unwrap().login, LoginState::None);
        assert!(found.antigravity);

        // A cached Linux build of the host version is the source.
        runtimes::fake_install_version(&data, "codex", "0.163.0");
        assert_eq!(detect(&fs, &logins, 10, &data).agent(AgentKind::Codex).unwrap().source, Source::Cached);
    }

    #[test]
    fn needs_the_entry_point_and_a_running_binary() {
        let mut fs = home();
        // A pi package with another layout, and a claude on PATH that does not run.
        fs.file("/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/package.json", r#"{"version":"1.4.0"}"#);
        fs.file("/usr/bin/claude", "");
        let found = detect(&fs, &FakeLogins::default(), 0, &data());
        assert!(found.agents.is_empty());

        fs.versions.insert("/usr/bin/claude".into(), "claude 2.1.306".into());
        let found = detect(&fs, &FakeLogins::default(), 0, &data());
        assert_eq!(found.agent(AgentKind::Claude).map(|a| (a.version.as_str(), a.pinned)), Some(("2.1.306", true)));
    }

    #[test]
    fn codex_needs_a_running_binary() {
        let mut fs = home();
        fs.file("/usr/local/lib/node_modules/@openai/codex/package.json", r#"{"version":"0.163.0"}"#);
        assert!(detect(&fs, &FakeLogins::default(), 0, &data()).agents.is_empty());
        // Not next to the package: `codex` on PATH.
        fs.versions.insert("/usr/bin/codex".into(), "codex-cli 0.163.1".into());
        let found = detect(&fs, &FakeLogins::default(), 0, &data());
        assert_eq!(found.agent(AgentKind::Codex).map(|a| a.version.as_str()), Some("0.163.1"));
    }

    #[test]
    fn gemini_takes_its_entry_point_from_the_package_and_stays_inside_it() {
        let dir = "/usr/local/lib/node_modules/@google/gemini-cli";
        let entry = |manifest: &str, files: &[&str], links: &[(&str, &str)]| {
            let mut fs = home();
            fs.file(&format!("{dir}/package.json"), manifest);
            for file in files {
                fs.file(&format!("{dir}/{file}"), "");
            }
            for (link, target) in links {
                fs.links.insert(format!("{dir}/{link}").into(), target.into());
            }
            let found = detect(&fs, &FakeLogins::default(), 0, &data());
            found.agent(AgentKind::Gemini).and_then(|a| a.entry.clone())
        };
        assert_eq!(entry(r#"{"version":"1.0.0","bin":"./cli/main.js"}"#, &["cli/main.js"], &[]).as_deref(), Some("cli/main.js"));
        assert_eq!(entry(r#"{"version":"1.0.0","bin":{"gemini":"bundle/gemini.js"}}"#, &["bundle/gemini.js"], &[]).as_deref(), Some("bundle/gemini.js"));
        // A path out of the package is refused, and a known path is used.
        assert_eq!(entry(r#"{"version":"1.0.0","bin":"../evil.js"}"#, &["dist/index.js"], &[]).as_deref(), Some("dist/index.js"));
        assert_eq!(entry(r#"{"version":"1.0.0","bin":"/bin/sh"}"#, &["bundle/gemini.js"], &[]).as_deref(), Some("bundle/gemini.js"));
        assert_eq!(entry(r#"{"version":"1.0.0","bin":"x.js"}"#, &["x.js"], &[("x.js", "/etc/x.js")]), None);
        // No entry point: no Gemini CLI.
        assert_eq!(entry(r#"{"version":"1.0.0"}"#, &[], &[]), None);
    }

    #[test]
    fn versions_are_read_from_cli_output() {
        assert_eq!(version_in("2.1.310 (Claude Code)").as_deref(), Some("2.1.310"));
        assert_eq!(version_in("codex-cli 0.163.0").as_deref(), Some("0.163.0"));
        assert_eq!(version_in("v1.4.0-beta.1").as_deref(), Some("1.4.0-beta.1"));
        assert_eq!(version_in("no version 1.2"), None);
    }
}
