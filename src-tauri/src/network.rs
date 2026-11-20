//! The network policy of a cage (ADR 0003): one default, a list of Allow
//! and Block rules where Block always wins, and the always-blocked floor in
//! the proxy. A credential belongs to a rule; the proxy injects it only for
//! an Allow rule. On a Block rule it is inactive. The service catalog ships
//! with the app.

use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::cage::{self, Bound};
use crate::error::{Error, Result};
use crate::host_agents::{self, login};
use crate::proxy::decide::{self, Decision};
use crate::runtime::TerminalKind;
use crate::secrets::{self, CredentialRef, Origin, Places};

/// The marks of a credential template that the proxy replaces.
pub const SECRET_MARK: &str = "{secret}";
/// `{basic:<user>}` becomes the Basic credentials of `<user>` and the value.
const BASIC_MARK: &str = "{basic:";
/// The longest template.
const TEMPLATE_LIMIT: usize = 512;
/// The most rules of a cage.
pub(crate) const RULE_LIMIT: usize = 500;
/// The most credentials of one rule.
const CREDENTIAL_LIMIT: usize = 16;
/// Header fields that a credential may not set: the proxy or HTTP owns them.
const OWNED_HEADERS: &[&str] = &[
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "te",
    "upgrade",
    "keep-alive",
    "trailer",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DefaultAction {
    #[default]
    Block,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Allow,
    Block,
}

/// A service of the catalog, or a host: `a.io` or `*.a.io`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Target {
    Service { id: String },
    Host { host: String },
}

/// How the proxy puts the value into a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum Inject {
    /// Sets the header, also when the agent sends none.
    Set { header: String, template: String },
    /// Replaces the placeholder only where the agent sent it.
    Replace { places: Places },
}

/// The value is in the vault, at the account of its [`CredentialRef`]: the
/// cage's own value, or the library value that all cages can attach. Rules
/// of one source can share a name, and with it one value. The guest gets
/// the placeholder in the variable `name`, one for each name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
    pub name: String,
    pub inject: Inject,
    /// A config from before the library has no source: it is local.
    #[serde(default, skip_serializing_if = "Source::is_local")]
    pub source: Source,
}

/// Where the value of a credential is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The value of this cage.
    #[default]
    Local,
    /// The value in the credential library of the app.
    Library,
}

impl Source {
    pub fn is_local(&self) -> bool {
        *self == Self::Local
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub target: Target,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
}

/// A new or changed rule from the UI. The credentials stay.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleDraft {
    pub id: Option<String>,
    pub target: Target,
    pub action: Action,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredPolicy")]
pub struct NetworkPolicy {
    pub default: DefaultAction,
    /// Blocks all requests now and keeps the rules.
    pub offline: bool,
    pub rules: Vec<Rule>,
    /// The host port grant (ADR 0009): the guest reaches `127.0.0.1:<port>`
    /// of the Mac at `host.microsandbox.internal:<port>`, plain HTTP only.
    /// A config from before the grant has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<NonZeroU16>,
}

/// The shapes before the rules: `mode` with an allow list, and before that `allowAll`.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredPolicy {
    Current {
        default: DefaultAction,
        #[serde(default)]
        offline: bool,
        #[serde(default)]
        rules: Vec<Rule>,
        #[serde(default, rename = "hostPort")]
        host_port: Option<NonZeroU16>,
    },
    Mode {
        mode: OldMode,
        domains: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    Legacy {
        allow_all: bool,
        domains: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum OldMode {
    Deny,
    List,
    Public,
}

impl From<StoredPolicy> for NetworkPolicy {
    fn from(stored: StoredPolicy) -> Self {
        let (mode, domains) = match stored {
            StoredPolicy::Current { default, offline, rules, host_port } => {
                return Self { default, offline, rules, host_port };
            }
            StoredPolicy::Mode { mode, domains } => (mode, domains),
            StoredPolicy::Legacy { allow_all, domains } => {
                (if allow_all { OldMode::Public } else { OldMode::List }, domains)
            }
        };
        let (default, offline) = match mode {
            OldMode::Public => (DefaultAction::Allow, false),
            OldMode::List => (DefaultAction::Block, false),
            OldMode::Deny => (DefaultAction::Block, true),
        };
        Self { default, offline, rules: allow_rules(&domains), host_port: None }
    }
}

/// The Allow rules of an old allow list: a service for each service whose
/// domains are all on the list, a host rule for each other domain.
fn allow_rules(domains: &[String]) -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut covered: Vec<&str> = Vec::new();
    for service in CATALOG.iter().filter(|s| s.id != REGISTRIES) {
        if service.domains.iter().all(|d| domains.iter().any(|x| x == d)) {
            rules.push(Rule::new(Target::Service { id: service.id.to_owned() }, Action::Allow));
            covered.extend(service.domains);
        }
    }
    for domain in domains.iter().filter(|d| !covered.contains(&d.as_str())) {
        rules.push(Rule::new(Target::Host { host: domain.clone() }, Action::Allow));
    }
    rules
}

impl Rule {
    pub fn new(target: Target, action: Action) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string(), target, action, credentials: Vec::new() }
    }

    /// The service name or the host, as the log reason shows it.
    pub fn label(&self) -> String {
        match &self.target {
            Target::Service { id } => service(id).map_or_else(|| id.clone(), |s| s.name.to_owned()),
            Target::Host { host } => host.clone(),
        }
    }

    pub fn matches(&self, host: &str) -> bool {
        match &self.target {
            Target::Service { id } => service(id).is_some_and(|s| s.domains.iter().any(|d| covers(d, host))),
            Target::Host { host: pattern } => covers(pattern, host),
        }
    }

    /// The rule's credentials go to `host`: the exact host of a host rule,
    /// or a credential host of a service. A Block rule sends nothing.
    pub fn sends_credentials_to(&self, host: &str) -> bool {
        self.action == Action::Allow
            && match &self.target {
                Target::Service { id } => service(id).is_some_and(|s| s.sends_credentials_to(host)),
                Target::Host { host: exact } => !exact.starts_with("*.") && exact == host,
            }
    }

    /// The exact hosts that get the credentials, and the wildcards.
    pub fn credential_hosts(&self) -> Vec<String> {
        match &self.target {
            Target::Service { id } => {
                service(id).map(|s| s.credential_hosts.iter().map(|h| (*h).to_owned()).collect()).unwrap_or_default()
            }
            Target::Host { host } => vec![host.clone()],
        }
    }
}

/// `*.a.io` covers `a.io` and each host below it, as the allow list did.
pub fn covers(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(base) => host == base || host.strip_suffix(base).is_some_and(|rest| rest.ends_with('.')),
        None => pattern == host,
    }
}

impl NetworkPolicy {
    /// Adds the rule, or changes the rule with its id or its target. The
    /// credentials of a changed rule stay. Returns the rule id.
    pub fn put_rule(&mut self, draft: RuleDraft) -> Result<String> {
        let target = normalize_target(draft.target)?;
        let existing = match &draft.id {
            Some(id) => Some(self.rules.iter().position(|r| r.id == *id).ok_or(Error::RuleNotFound)?),
            None => self.rules.iter().position(|r| r.target == target),
        };
        if let Some(i) = existing {
            if self.rules.iter().enumerate().any(|(j, r)| j != i && r.target == target) {
                return Err(Error::InvalidRule("another rule has this target".into()));
            }
            // The kept credentials must fit the new target.
            let changed = Rule { target, action: draft.action, ..self.rules[i].clone() };
            for credential in &changed.credentials {
                check_holder(&changed, &credential.name)?;
            }
            let id = changed.id.clone();
            self.rules[i] = changed;
            return Ok(id);
        }
        if self.rules.len() >= RULE_LIMIT {
            return Err(Error::InvalidRule(format!("a project has at most {RULE_LIMIT} rules")));
        }
        let rule = Rule::new(target, draft.action);
        let id = rule.id.clone();
        self.rules.push(rule);
        Ok(id)
    }

    /// Returns the removed rule.
    pub fn remove_rule(&mut self, rule_id: &str) -> Result<Rule> {
        let i = self.rules.iter().position(|r| r.id == rule_id).ok_or(Error::RuleNotFound)?;
        Ok(self.rules.remove(i))
    }

    pub fn rule_mut(&mut self, rule_id: &str) -> Result<&mut Rule> {
        self.rules.iter_mut().find(|r| r.id == rule_id).ok_or(Error::RuleNotFound)
    }

    /// Each credential value once, by source and name, with its first binding.
    pub fn credentials(&self) -> Vec<&Credential> {
        let mut seen: Vec<&Credential> = Vec::new();
        for credential in self.rules.iter().flat_map(|r| &r.credentials) {
            if !seen.iter().any(|c| c.name == credential.name && c.source == credential.source) {
                seen.push(credential);
            }
        }
        seen
    }

    /// Whether a credential of the rules uses `key`.
    pub fn uses(&self, key: &CredentialRef) -> bool {
        self.rules.iter().flat_map(|r| &r.credentials).any(|c| c.key() == *key)
    }

    /// The rules that a stored config must meet.
    pub fn check(&self) -> Result<()> {
        for (i, rule) in self.rules.iter().enumerate() {
            if self.rules[..i].iter().any(|r| r.id == rule.id || r.target == rule.target) {
                return Err(Error::InvalidRule(format!("{} is in the list twice", rule.label())));
            }
            normalize_target(rule.target.clone())?;
            if rule.credentials.len() > CREDENTIAL_LIMIT {
                return Err(Error::InvalidRule(format!("a rule has at most {CREDENTIAL_LIMIT} credentials")));
            }
            for (j, credential) in rule.credentials.iter().enumerate() {
                if rule.credentials[..j].iter().any(|c| c.name == credential.name) {
                    return Err(Error::DuplicateSecret(credential.name.clone()));
                }
                credential.clone().normalize()?;
                check_holder(rule, &credential.name)?;
            }
        }
        self.check_sources()
    }

    /// One recipient gets one value per placeholder and per set header, so
    /// the value never depends on the rule or credential order. Allow rules
    /// whose credential hosts overlap, and one Allow rule, may not bind one
    /// name, or one `Set` header, to credentials of two sources.
    fn check_sources(&self) -> Result<()> {
        let allowed: Vec<&Rule> =
            self.rules.iter().filter(|r| r.action == Action::Allow && !r.credentials.is_empty()).collect();
        for (i, rule) in allowed.iter().enumerate() {
            for other in &allowed[..=i] {
                let host = if std::ptr::eq(*other, *rule) {
                    rule.label()
                } else {
                    match overlap(&rule.credential_hosts(), &other.credential_hosts()) {
                        Some(host) => host,
                        None => continue,
                    }
                };
                for credential in &rule.credentials {
                    for theirs in other.credentials.iter().filter(|c| c.source != credential.source) {
                        // One rule has each name once, so a name match is of two rules.
                        if theirs.name == credential.name {
                            return Err(Error::InvalidSecret(
                                credential.name.clone(),
                                format!(
                                    "{} and {} both send it to {host}, one from the library and one from this cage; use one source",
                                    other.label(),
                                    rule.label()
                                ),
                            ));
                        }
                        if let Some(header) = set_header(credential).filter(|h| Some(h) == set_header(theirs).as_ref())
                        {
                            return Err(Error::InvalidSecret(
                                credential.name.clone(),
                                format!(
                                    "it and {} both set the {header} header for {host}, one from the library and one from this cage; use one source",
                                    theirs.name
                                ),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// The header that a `Set` credential sets, trimmed and in lowercase.
fn set_header(credential: &Credential) -> Option<String> {
    match &credential.inject {
        Inject::Set { header, .. } => Some(header.trim().to_ascii_lowercase()),
        Inject::Replace { .. } => None,
    }
}

/// A host that both lists cover, if any. A list has exact hosts and `*.x` wildcards.
fn overlap(a: &[String], b: &[String]) -> Option<String> {
    let base = |p: &str| p.strip_prefix("*.").unwrap_or(p).to_owned();
    a.iter().find_map(|x| {
        b.iter().find_map(|y| {
            if covers(x, &base(y)) {
                Some(base(y))
            } else if covers(y, &base(x)) {
                Some(base(x))
            } else {
                None
            }
        })
    })
}

/// A credential needs a rule that names where it goes: an exact host, or a
/// service with credential hosts. A user wildcard can cover hosts of other
/// owners, so it takes no credential. On a Block rule the credential is
/// inactive: the proxy never injects it.
pub fn check_holder(rule: &Rule, name: &str) -> Result<()> {
    let takes = match &rule.target {
        Target::Host { host } => !host.starts_with("*."),
        Target::Service { id } => service(id).is_some_and(Service::takes_credentials),
    };
    if !takes {
        return Err(Error::InvalidSecret(
            name.to_owned(),
            format!("{} takes no credential; add it to a rule of one exact host", rule.label()),
        ));
    }
    Ok(())
}

/// An exact host, or `*.x` where `x` is a registrable domain, which has one owner.
fn is_owned(pattern: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(base) => psl::domain_str(base) == Some(base),
        None => true,
    }
}

fn normalize_target(target: Target) -> Result<Target> {
    match target {
        Target::Service { id } => match service(&id) {
            Some(_) => Ok(Target::Service { id }),
            None => Err(Error::InvalidRule(format!("unknown service {id}"))),
        },
        Target::Host { host } => Ok(Target::Host { host: cage::domain(&host)? }),
    }
}

impl Credential {
    /// Trims and lowercases, then checks the rules.
    pub fn normalize(mut self) -> Result<Self> {
        self.name = secrets::check_name(&self.name)?;
        let invalid = |reason: &str| Error::InvalidSecret(self.name.clone(), reason.to_owned());
        self.inject = match self.inject {
            Inject::Set { header, template } => {
                let header = header.trim().to_ascii_lowercase();
                if !secrets::is_token(&header) {
                    return Err(invalid(&format!("{header:?} is not a valid header field name")));
                }
                if OWNED_HEADERS.contains(&header.as_str()) {
                    return Err(invalid(&format!("the proxy owns the {header} header")));
                }
                let template = template.trim().to_owned();
                if template.len() > TEMPLATE_LIMIT || template.chars().any(|c| c.is_control()) {
                    return Err(invalid("the template must be one short line"));
                }
                if !template.contains(SECRET_MARK) && basic_user(&template).is_none() {
                    return Err(invalid("the template needs {secret} or {basic:<user>}"));
                }
                Inject::Set { header, template }
            }
            Inject::Replace { places } => Inject::Replace { places: secrets::normalize_places(&self.name, places)? },
        };
        Ok(self)
    }

    pub fn placeholder(&self) -> String {
        secrets::placeholder(&self.name)
    }

    pub fn key(&self) -> CredentialRef {
        CredentialRef { source: self.source, name: self.name.clone() }
    }
}

/// The `<user>` of a `{basic:<user>}` mark, with the text before and after it.
pub fn basic_user(template: &str) -> Option<(&str, &str, &str)> {
    let start = template.find(BASIC_MARK)?;
    let rest = &template[start + BASIC_MARK.len()..];
    let end = rest.find('}')?;
    Some((&template[..start], &rest[..end], &rest[end + 1..]))
}

/// A service of the catalog.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub id: &'static str,
    pub name: &'static str,
    /// A lucide icon name.
    pub icon: &'static str,
    /// Exact hosts and `*.` wildcards.
    pub domains: &'static [&'static str],
    /// The hosts that get a credential of the service rule: exact hosts, or
    /// `*.x` where `x` is a registrable domain. Empty: the service takes no
    /// credential.
    pub credential_hosts: &'static [&'static str],
    /// The header and template that the credential dialog proposes.
    pub credential: Option<(&'static str, &'static str)>,
}

impl Service {
    fn takes_credentials(&self) -> bool {
        !self.credential_hosts.is_empty() && self.credential_hosts.iter().all(|h| is_owned(h))
    }

    fn sends_credentials_to(&self, host: &str) -> bool {
        self.takes_credentials() && self.credential_hosts.iter().any(|h| covers(h, host))
    }
}

const REGISTRIES: &str = "registries";

pub const CATALOG: &[Service] = &[
    Service {
        id: "github",
        name: "GitHub",
        icon: "github",
        domains: &[
            "github.com",
            "api.github.com",
            "codeload.github.com",
            "*.githubusercontent.com",
            "objects.githubusercontent.com",
            "ghcr.io",
        ],
        credential_hosts: &["github.com", "api.github.com"],
        credential: Some(("authorization", "Basic {basic:x-access-token}")),
    },
    Service {
        id: "npm",
        name: "npm",
        icon: "package",
        domains: &["registry.npmjs.org", "*.npmjs.org"],
        credential_hosts: &["registry.npmjs.org"],
        credential: Some(("authorization", "Bearer {secret}")),
    },
    Service {
        id: "yarn",
        name: "Yarn",
        icon: "package",
        domains: &["registry.yarnpkg.com", "repo.yarnpkg.com"],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: "pypi",
        name: "PyPI",
        icon: "package",
        domains: &["pypi.org", "files.pythonhosted.org"],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: "crates",
        name: "crates.io",
        icon: "box",
        domains: &["crates.io", "static.crates.io", "index.crates.io"],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: "go",
        name: "Go modules",
        icon: "box",
        domains: &["proxy.golang.org", "sum.golang.org"],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: REGISTRIES,
        name: "Common package registries",
        icon: "boxes",
        domains: &[
            "registry.npmjs.org",
            "*.npmjs.org",
            "registry.yarnpkg.com",
            "repo.yarnpkg.com",
            "pypi.org",
            "files.pythonhosted.org",
            "crates.io",
            "static.crates.io",
            "index.crates.io",
            "proxy.golang.org",
            "sum.golang.org",
        ],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: "docker",
        name: "Docker Hub and GHCR",
        icon: "container",
        domains: &[
            "registry-1.docker.io",
            "auth.docker.io",
            "index.docker.io",
            "production.cloudflare.docker.com",
            "ghcr.io",
            "pkg-containers.githubusercontent.com",
        ],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: "anthropic",
        name: "Anthropic API",
        icon: "sparkles",
        domains: &["api.anthropic.com", "statsig.anthropic.com"],
        credential_hosts: &["api.anthropic.com"],
        credential: Some(("x-api-key", "{secret}")),
    },
    Service {
        id: "openai",
        name: "OpenAI API",
        icon: "sparkles",
        domains: &["api.openai.com", "chatgpt.com"],
        credential_hosts: &["api.openai.com"],
        credential: Some(("authorization", "Bearer {secret}")),
    },
    Service {
        id: "gemini",
        name: "Google Gemini",
        icon: "sparkles",
        // oauth2.googleapis.com has Gemini CLI's tokeninfo check of its login.
        domains: &["generativelanguage.googleapis.com", "cloudcode-pa.googleapis.com", "oauth2.googleapis.com"],
        credential_hosts: &["generativelanguage.googleapis.com"],
        credential: Some(("x-goog-api-key", "{secret}")),
    },
    Service {
        id: "copilot",
        name: "GitHub Copilot",
        icon: "bot",
        domains: &["api.githubcopilot.com", "api.individual.githubcopilot.com", "api.business.githubcopilot.com"],
        credential_hosts: &[],
        credential: None,
    },
    Service {
        id: "huggingface",
        name: "Hugging Face",
        icon: "brain",
        domains: &["huggingface.co", "*.huggingface.co", "*.hf.co"],
        credential_hosts: &["huggingface.co"],
        credential: Some(("authorization", "Bearer {secret}")),
    },
];

pub fn service(id: &str) -> Option<&'static Service> {
    CATALOG.iter().find(|s| s.id == id)
}

/// A host that an agent of the session needs, and the proxy's decision on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentNeed {
    pub agent: AgentKind,
    /// The catalog service of the host.
    pub service: &'static str,
    pub host: String,
    pub allowed: bool,
}

/// The hosts that the agents of the session and its cage need: pi the provider of its Mac
/// login, and npm for the extensions of its Mac profile; Claude Code the
/// Anthropic API; Codex the OpenAI API; Gemini CLI Google Gemini. An agent
/// with a Mac login needs the hosts of the login. The agents are the ones
/// of the host agents, the terminals and the tasks. `allowed` is the
/// decision of the proxy on the name, before the address check.
pub fn agent_needs(bound: Bound<'_>) -> Vec<AgentNeed> {
    let (session, cage) = (bound.session, bound.cage);
    let terminals = session.terminals.iter().filter_map(|t| match t.kind {
        TerminalKind::Agent { agent, .. } => Some(agent),
        TerminalKind::Shell | TerminalKind::Host | TerminalKind::HostAgent { .. } => None,
    });
    let used: Vec<AgentKind> = cage
        .host_agents
        .iter()
        .map(|a| a.kind)
        .chain(terminals)
        .chain(session.tasks.iter().flat_map(|t| t.agents.iter().map(|a| a.kind)))
        .collect();
    let mut needs: Vec<(AgentKind, String)> = Vec::new();
    for kind in host_agents::KINDS.into_iter().filter(|k| used.contains(k)) {
        let logins: Vec<String> = cage
            .logins
            .iter()
            .filter(|l| matches!(l.origin, Origin::HostLogin { agent } if agent == kind))
            .flat_map(|l| l.hosts.clone())
            .collect();
        let fallback = match kind {
            AgentKind::Claude => Some("anthropic"),
            AgentKind::Codex => Some("openai"),
            AgentKind::Gemini => Some("gemini"),
            AgentKind::Pi => None,
        };
        if !logins.is_empty() {
            needs.extend(logins.into_iter().map(|h| (kind, h)));
        } else if let Some(host) = fallback.and_then(service).and_then(|s| s.domains.first()) {
            needs.push((kind, (*host).to_owned()));
        }
        if kind == AgentKind::Pi && cage.host_agent(kind).is_some_and(|a| a.profile) {
            needs.push((kind, "registry.npmjs.org".to_owned()));
        }
    }
    let refresh = login::denied_hosts(&cage.logins);
    let tokeninfo = cage.logins.iter().any(|l| l.origin == Origin::HostLogin { agent: AgentKind::Gemini });
    // The agents use HTTPS, so port 443; no proxy port is known here.
    let ctx = decide::Context { policy: &cage.network, refresh: &refresh, tokeninfo, listener: 0 };
    needs
        .into_iter()
        .filter_map(|(agent, host)| {
            let service = CATALOG.iter().find(|s| s.id != REGISTRIES && s.domains.iter().any(|d| covers(d, &host)))?;
            let allowed = matches!(decide::decide(&ctx, &host, 443, None), Decision::Allow(_) | Decision::TokenInfo);
            Some(AgentNeed { agent, service: service.id, host, allowed })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(host: &str) -> Target {
        Target::Host { host: host.into() }
    }

    fn targets(policy: &NetworkPolicy) -> Vec<(Target, Action)> {
        policy.rules.iter().map(|r| (r.target.clone(), r.action)).collect()
    }

    #[test]
    fn old_modes_migrate() {
        let policy = |json: &str| serde_json::from_str::<NetworkPolicy>(json).unwrap();
        let public = policy(r#"{"mode":"public","domains":["a.io"]}"#);
        assert_eq!((public.default, public.offline), (DefaultAction::Allow, false));
        assert_eq!(targets(&public), [(host("a.io"), Action::Allow)]);

        // A service whose domains are all listed becomes the service.
        let list = policy(
            r#"{"mode":"list","domains":["pypi.org","x.io","files.pythonhosted.org","*.a.io","registry.npmjs.org"]}"#,
        );
        assert_eq!((list.default, list.offline), (DefaultAction::Block, false));
        assert_eq!(
            targets(&list),
            [
                (Target::Service { id: "pypi".into() }, Action::Allow),
                (host("x.io"), Action::Allow),
                (host("*.a.io"), Action::Allow),
                (host("registry.npmjs.org"), Action::Allow),
            ]
        );
        let deny = policy(r#"{"mode":"deny","domains":["x.io"]}"#);
        assert_eq!((deny.default, deny.offline), (DefaultAction::Block, true));
        assert_eq!(targets(&deny), [(host("x.io"), Action::Allow)]);
        let legacy = policy(r#"{"allowAll":true,"domains":[]}"#);
        assert_eq!(legacy.default, DefaultAction::Allow);
        // The new shape reads back as it is.
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(policy(&json), list);
    }

    #[test]
    fn rules_match_hosts_services_and_wildcards() {
        let wildcard = Rule::new(host("*.a.io"), Action::Allow);
        assert!(wildcard.matches("a.io") && wildcard.matches("x.y.a.io"));
        assert!(!wildcard.matches("evila.io") && !wildcard.matches("a.io.evil.com"));
        let github = Rule::new(Target::Service { id: "github".into() }, Action::Allow);
        assert!(github.matches("raw.githubusercontent.com") && github.matches("api.github.com"));
        assert!(!github.matches("gist.github.com"));
        assert!(github.sends_credentials_to("github.com"));
        assert!(!github.sends_credentials_to("raw.githubusercontent.com"));
        let yarn = Rule::new(Target::Service { id: "yarn".into() }, Action::Allow);
        assert!(check_holder(&yarn, "KEY").is_err());
        // On a Block rule a credential is inactive: allowed, and sent nowhere.
        let blocked = Rule::new(host("a.io"), Action::Block);
        assert!(check_holder(&blocked, "KEY").is_ok() && !blocked.sends_credentials_to("a.io"));
    }

    #[test]
    fn a_credential_goes_only_to_hosts_of_one_owner() {
        // A user wildcard can cover the hosts of other owners.
        for pattern in ["*.a.io", "*.github.com"] {
            let rule = Rule::new(host(pattern), Action::Allow);
            assert!(check_holder(&rule, "KEY").is_err(), "{pattern}");
            assert!(!rule.sends_credentials_to(&pattern[2..]));
        }
        let exact = Rule::new(host("api.github.com"), Action::Allow);
        assert!(check_holder(&exact, "KEY").is_ok() && exact.sends_credentials_to("api.github.com"));
        assert!(!exact.sends_credentials_to("gist.api.github.com"));

        // The GitHub service sends to its exact credential hosts only.
        let github = Rule::new(Target::Service { id: "github".into() }, Action::Allow);
        assert!(check_holder(&github, "KEY").is_ok());
        assert!(github.sends_credentials_to("api.github.com") && !github.sends_credentials_to("gist.github.com"));

        // A catalog wildcard counts only on a registrable domain.
        let service = |credential_hosts| Service {
            id: "x",
            name: "X",
            icon: "box",
            domains: &["*.github.com"],
            credential_hosts,
            credential: None,
        };
        let owned = service(&["*.github.com"]);
        assert!(owned.takes_credentials() && owned.sends_credentials_to("gist.github.com"));
        assert!(!owned.sends_credentials_to("github.io"));
        for credential_hosts in [&["*.api.github.com"][..], &["api.github.com", "*.co.uk"], &[]] {
            let other = service(credential_hosts);
            assert!(
                !other.takes_credentials() && !other.sends_credentials_to("x.api.github.com"),
                "{credential_hosts:?}"
            );
        }
        // Each wildcard credential host of the catalog is on a registrable domain.
        for s in CATALOG.iter().filter(|s| !s.credential_hosts.is_empty()) {
            assert!(s.takes_credentials(), "{}", s.id);
        }
    }

    #[test]
    fn agent_needs_follow_the_agents_and_the_rules() {
        use crate::agent::AgentKind;
        use crate::cage::HostAgentUse;

        let mut session = crate::session::Session::new("/tmp", Some("c".into()), []).unwrap();
        let mut cage = crate::cage::Cage::new("c".into(), "tmp".into());
        cage.host_agents = vec![
            HostAgentUse { kind: AgentKind::Pi, profile: true, login: true },
            HostAgentUse { kind: AgentKind::Codex, profile: false, login: false },
        ];
        cage.logins = vec![login::secret(AgentKind::Pi, "api.anthropic.com")];
        session.terminals = vec![crate::session::TerminalRecord {
            id: "t".into(),
            kind: TerminalKind::Agent { agent: AgentKind::Gemini, resume: false },
        }];
        cage.network.rules = vec![
            Rule::new(Target::Service { id: "anthropic".into() }, Action::Allow),
            Rule::new(host("registry.npmjs.org"), Action::Allow),
            Rule::new(Target::Service { id: "openai".into() }, Action::Block),
        ];
        let need = |agent, service, host: &str, allowed| AgentNeed { agent, service, host: host.into(), allowed };
        assert_eq!(
            agent_needs(Bound { session: &session, cage: &cage }),
            [
                need(AgentKind::Codex, "openai", "api.openai.com", false),
                need(AgentKind::Pi, "anthropic", "api.anthropic.com", true),
                need(AgentKind::Pi, "npm", "registry.npmjs.org", true),
                need(AgentKind::Gemini, "gemini", "generativelanguage.googleapis.com", false),
            ]
        );
    }

    #[test]
    fn put_rule_changes_a_rule_of_the_same_target() {
        let mut policy = NetworkPolicy::default();
        let draft = |target, action| RuleDraft { id: None, target, action };
        let id = policy.put_rule(draft(host(" Example.COM "), Action::Allow)).unwrap();
        assert_eq!(policy.put_rule(draft(host("example.com"), Action::Block)).unwrap(), id);
        assert_eq!(targets(&policy), [(host("example.com"), Action::Block)]);
        for bad in [host("*.com"), host("*"), Target::Service { id: "nope".into() }] {
            assert!(policy.put_rule(draft(bad, Action::Allow)).is_err());
        }
        let other = policy.put_rule(draft(host("b.io"), Action::Allow)).unwrap();
        let moved = RuleDraft { id: Some(other), target: host("example.com"), action: Action::Allow };
        assert!(matches!(policy.put_rule(moved), Err(Error::InvalidRule(_))));
        policy.check().unwrap();
    }

    #[test]
    fn credentials_are_checked() {
        let set = |header: &str, template: &str| Credential {
            name: "KEY".into(),
            inject: Inject::Set { header: header.into(), template: template.into() },
            source: Source::Local,
        };
        assert_eq!(
            set(" Authorization ", "Bearer {secret}").normalize().unwrap().inject,
            Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() }
        );
        assert!(set("authorization", "Basic {basic:x-access-token}").normalize().is_ok());
        for (header, template) in
            [("host", "{secret}"), ("x api", "{secret}"), ("x-key", "no mark"), ("x-key", "a\r\nb: {secret}")]
        {
            assert!(set(header, template).normalize().is_err(), "{header} {template}");
        }
        assert_eq!(basic_user("Basic {basic:u}!"), Some(("Basic ", "u", "!")));
    }

    fn with_key(target: Target, source: Source) -> Rule {
        let mut rule = Rule::new(target, Action::Allow);
        rule.credentials.push(Credential {
            name: "KEY".into(),
            inject: Inject::Set { header: "authorization".into(), template: "Bearer {secret}".into() },
            source,
        });
        rule
    }

    #[test]
    fn one_recipient_cannot_get_one_name_from_two_sources() {
        let openai = || Target::Service { id: "openai".into() };
        let pairs = [
            (with_key(host("api.openai.com"), Source::Library), with_key(openai(), Source::Local)),
            (
                with_key(host("huggingface.co"), Source::Local),
                with_key(Target::Service { id: "huggingface".into() }, Source::Library),
            ),
        ];
        for (a, b) in pairs {
            for rules in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
                let policy = NetworkPolicy { rules, ..Default::default() };
                assert!(matches!(policy.check(), Err(Error::InvalidSecret(n, _)) if n == "KEY"));
            }
            // One source, or a Block rule, is valid.
            let same = with_key(b.target.clone(), a.credentials[0].source);
            assert!(NetworkPolicy { rules: vec![a.clone(), same], ..Default::default() }.check().is_ok());
            let mut blocked = b.clone();
            blocked.action = Action::Block;
            assert!(NetworkPolicy { rules: vec![a, blocked], ..Default::default() }.check().is_ok());
        }
        // Two recipients may use one name of two sources.
        let rules = vec![with_key(host("a1.io"), Source::Library), with_key(host("a2.io"), Source::Local)];
        assert!(NetworkPolicy { rules, ..Default::default() }.check().is_ok());
        assert_eq!(overlap(&["*.a.io".into()], &["x.a.io".into()]).as_deref(), Some("x.a.io"));
        assert_eq!(overlap(&["a.io".into()], &["b.a.io".into()]), None);
    }

    fn keyed(target: Target, credentials: &[(&str, &str, Source)]) -> Rule {
        let mut rule = Rule::new(target, Action::Allow);
        for (name, header, source) in credentials {
            rule.credentials.push(Credential {
                name: (*name).into(),
                inject: Inject::Set { header: (*header).into(), template: "Bearer {secret}".into() },
                source: *source,
            });
        }
        rule
    }

    /// Two names of two sources that set one header for one recipient are
    /// refused in each order, also when the header differs in case or space.
    #[test]
    fn one_recipient_cannot_get_one_set_header_from_two_sources() {
        let (local, library) = (Source::Local, Source::Library);
        let refused = |rules: Vec<Rule>| {
            let result = NetworkPolicy { rules, ..Default::default() }.check();
            assert!(
                matches!(&result, Err(Error::InvalidSecret(_, m)) if m.contains("authorization header")),
                "{result:?}"
            );
        };
        let ok = |rules: Vec<Rule>| assert!(NetworkPolicy { rules, ..Default::default() }.check().is_ok());
        for (a, b) in [(local, library), (library, local)] {
            // One Allow rule.
            refused(vec![keyed(host("a.io"), &[("A", "authorization", a), ("B", " Authorization ", b)])]);
            refused(vec![keyed(host("a.io"), &[("B", "AUTHORIZATION", b), ("A", "authorization", a)])]);
            // Two overlapping Allow rules, in both orders.
            let first = keyed(host("api.openai.com"), &[("A", "authorization", a)]);
            let second = keyed(Target::Service { id: "openai".into() }, &[("B", "Authorization", b)]);
            refused(vec![first.clone(), second.clone()]);
            refused(vec![second.clone(), first.clone()]);
            // Separate recipients, other headers, or a Block rule are valid.
            ok(vec![first.clone(), keyed(host("a2.io"), &[("B", "authorization", b)])]);
            ok(vec![keyed(host("a.io"), &[("A", "authorization", a), ("B", "x-api-key", b)])]);
            let mut blocked = second.clone();
            blocked.action = Action::Block;
            ok(vec![first, blocked]);
        }
        // One source may set one header twice.
        for source in [local, library] {
            ok(vec![keyed(host("a.io"), &[("A", "authorization", source), ("B", "authorization", source)])]);
            ok(vec![
                keyed(host("api.openai.com"), &[("A", "authorization", source)]),
                keyed(Target::Service { id: "openai".into() }, &[("B", "authorization", source)]),
            ]);
        }
    }

    #[test]
    fn a_local_credential_keeps_its_old_json_and_a_library_one_names_its_source() {
        let old = r#"{"name":"KEY","inject":{"mode":"set","header":"authorization","template":"Bearer {secret}"}}"#;
        let credential: Credential = serde_json::from_str(old).unwrap();
        assert_eq!(credential.source, Source::Local);
        assert_eq!(serde_json::to_string(&credential).unwrap(), old);
        let library = Credential { source: Source::Library, ..credential };
        let json = serde_json::to_value(&library).unwrap();
        assert_eq!(json["source"], "library");
        assert_eq!(serde_json::from_value::<Credential>(json).unwrap(), library);
    }

    #[test]
    fn the_host_port_grant_is_optional_and_never_zero() {
        let policy = |json: &str| serde_json::from_str::<NetworkPolicy>(json);
        // A config from before the grant has none, and writes none.
        let old = r#"{"default":"block","offline":false,"rules":[]}"#;
        let before = policy(old).unwrap();
        assert_eq!(before.host_port, None);
        assert_eq!(serde_json::to_string(&before).unwrap(), old);
        let granted = policy(r#"{"default":"block","hostPort":11434}"#).unwrap();
        assert_eq!(granted.host_port, NonZeroU16::new(11434));
        let json = serde_json::to_string(&granted).unwrap();
        assert!(json.contains(r#""hostPort":11434"#), "{json}");
        assert_eq!(policy(&json).unwrap(), granted);
        for bad in ["0", "65536", "-1", "\"80\""] {
            assert!(policy(&format!(r#"{{"default":"block","hostPort":{bad}}}"#)).is_err(), "{bad}");
        }
    }
}
