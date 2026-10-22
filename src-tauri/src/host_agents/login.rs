//! The host login of an agent (ADR 0002). zwinger reads the subscription
//! token on the Mac at each shield start and never writes the host Keychain
//! or the host login files. The guest gets a login with placeholders only:
//! the proxy puts the real access token into the `authorization` header of
//! requests to the provider's exact hosts. The refresh token in the guest is
//! an inert literal that no secret replaces, and the proxy blocks the refresh
//! hosts always, so the agent cannot renew or leak the login. The proxy reads
//! the login on the Mac again when it is due, so a new host token applies
//! with no restart. Gemini CLI checks its cached token at the tokeninfo
//! endpoint of `oauth2.googleapis.com`: the proxy answers that one request.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Map, Value};

use crate::agent::{AgentKind, cli};
use crate::error::{Error, Result};
use crate::secrets::{self, Origin, Places, Secret, SecretValue};

/// The Keychain item of Claude Code's login.
const CLAUDE_SERVICE: &str = "Claude Code-credentials";
/// A login file over this size is refused.
const FILE_LIMIT: u64 = 1 << 20;
/// The refresh token in the guest. No secret replaces it.
pub const INERT_REFRESH: &str = "zwinger-host-login-no-refresh";
/// The expiry in the guest (2100-01-01), so the agent does not try a refresh.
pub const FAR_FUTURE_MS: i64 = 4_102_444_800_000;
/// Where Gemini CLI checks its cached token. The proxy answers it.
pub const TOKENINFO_HOST: &str = "oauth2.googleapis.com";
pub const TOKENINFO_PATH: &str = "/tokeninfo";

/// An API host of a login and the hosts where its token is refreshed.
struct Provider {
    /// The key in pi's `auth.json`.
    pi: &'static str,
    host: &'static str,
    refresh: &'static [&'static str],
}

const ANTHROPIC: Provider =
    Provider { pi: "anthropic", host: "api.anthropic.com", refresh: &["console.anthropic.com", "platform.claude.com"] };
/// The Codex backend of a ChatGPT login (`chatgpt.com/backend-api/codex`).
const CHATGPT: Provider = Provider { pi: "openai-codex", host: "chatgpt.com", refresh: &["auth.openai.com"] };
/// The Copilot API of an individual account. Other accounts get their
/// host from the token, see [`copilot_host`].
const COPILOT: Provider =
    Provider { pi: "github-copilot", host: "api.individual.githubcopilot.com", refresh: &[GITHUB_API] };
/// Copilot's refresh host. It is also the API of `gh` and of other GitHub
/// tools, so a shield with pi's Copilot login cannot use them.
pub const GITHUB_API: &str = "api.github.com";
const COPILOT_DOMAIN: &str = ".githubcopilot.com";
/// The Code Assist API of a Google login.
const GOOGLE: Provider =
    Provider { pi: "google-gemini-cli", host: "cloudcode-pa.googleapis.com", refresh: &["oauth2.googleapis.com"] };
/// In this order, pi uses the first provider that has a login.
const PI_PROVIDERS: [&Provider; 4] = [&ANTHROPIC, &CHATGPT, &COPILOT, &GOOGLE];

/// Each host where a login token is refreshed. A host-login secret may not
/// name one.
pub fn is_refresh_host(host: &str) -> bool {
    PI_PROVIDERS.iter().any(|p| p.refresh.contains(&host))
}

/// The provider whose API is at `host`.
fn provider_of(host: &str) -> Option<&'static Provider> {
    PI_PROVIDERS.iter().copied().find(|p| p.host == host || (p.pi == COPILOT.pi && is_copilot_host(host)))
}

/// An exact API host of Copilot: `api.<name>.githubcopilot.com`.
fn is_copilot_host(host: &str) -> bool {
    let Some(name) = host.strip_prefix("api.").and_then(|h| h.strip_suffix(COPILOT_DOMAIN)) else {
        return false;
    };
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The Copilot API host of the token and the end of its `proxy-ep` field
/// (`proxy.business.githubcopilot.com` → `api.business.githubcopilot.com`).
/// pi finds the host the same way: the first `proxy-ep=`, up to `;`.
/// `None` if the field is missing or names no Copilot host: pi then uses
/// the individual host.
fn copilot_endpoint(access: &str) -> Option<(String, usize)> {
    let start = access.find("proxy-ep=")? + "proxy-ep=".len();
    let end = access[start..].find(';').map_or(access.len(), |i| start + i);
    let host = format!("api.{}", access[start..end].strip_prefix("proxy.")?);
    is_copilot_host(&host).then_some((host, end))
}

/// The refresh hosts that the network policy denies: the ones of the
/// providers of the shield's host logins.
pub fn denied_hosts(secrets: &[Secret]) -> Vec<&'static str> {
    let mut denied = Vec::new();
    for secret in secrets.iter().filter(|s| matches!(s.origin, Origin::HostLogin { .. })) {
        for provider in secret.hosts.iter().filter_map(|h| provider_of(h)) {
            for host in provider.refresh {
                if !denied.contains(host) {
                    denied.push(*host);
                }
            }
        }
    }
    denied
}

/// The variable of the agent's host-login secret. Claude Code reads its own;
/// the other agents get the placeholder in a login file.
pub fn secret_name(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "CLAUDE_CODE_OAUTH_TOKEN",
        AgentKind::Codex => "CODEX_HOST_LOGIN",
        AgentKind::Pi => "PI_HOST_LOGIN",
        AgentKind::Gemini => "GEMINI_HOST_LOGIN",
    }
}

/// The login of an agent on the Mac.
pub trait LoginSource: Send + Sync {
    /// The login document, `None` if there is none. The Keychain read has a
    /// timeout.
    fn read(&self, kind: AgentKind) -> Result<Option<String>>;
    /// If a login exists. It reads no secret, so macOS shows no prompt.
    fn exists(&self, kind: AgentKind) -> bool;
}

/// The real logins: the Keychain for Claude Code, files for the others.
pub struct HostLogins {
    pub home: PathBuf,
}

impl HostLogins {
    fn file(&self, kind: AgentKind) -> PathBuf {
        match kind {
            AgentKind::Codex => self.home.join(".codex/auth.json"),
            AgentKind::Pi => self.home.join(".pi/agent/auth.json"),
            _ => self.home.join(".gemini/oauth_creds.json"),
        }
    }
}

impl LoginSource for HostLogins {
    fn read(&self, kind: AgentKind) -> Result<Option<String>> {
        if kind == AgentKind::Claude {
            return read_keychain();
        }
        read_file(&self.file(kind))
    }

    fn exists(&self, kind: AgentKind) -> bool {
        if kind == AgentKind::Claude {
            return keychain_exists();
        }
        self.file(kind).is_file()
    }
}

fn read_file(path: &Path) -> Result<Option<String>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut text = String::new();
    file.take(FILE_LIMIT).read_to_string(&mut text)?;
    Ok(Some(text))
}

/// On the Keychain worker, so a macOS access prompt that nobody answers ends
/// the wait after `KEYCHAIN_LIMIT`.
#[cfg(target_os = "macos")]
fn read_keychain() -> Result<Option<String>> {
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    let found = secrets::bounded(secrets::KEYCHAIN_LIMIT, || {
        let mut query = security_framework::item::ItemSearchOptions::new();
        query
            .class(security_framework::item::ItemClass::generic_password())
            .service(CLAUDE_SERVICE)
            .load_data(true)
            .limit(1);
        match query.search() {
            Ok(mut items) => match items.pop() {
                Some(security_framework::item::SearchResult::Data(data)) => Ok(Some(data)),
                _ => Ok(None),
            },
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(Error::Keychain(e.to_string())),
        }
    });
    match found {
        Some(found) => Ok(found?.map(|data| String::from_utf8_lossy(&data).into_owned())),
        None => Err(Error::Keychain(
            "no answer from the Keychain; allow access to \"Claude Code-credentials\" in the macOS prompt".into(),
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn read_keychain() -> Result<Option<String>> {
    Ok(None)
}

/// No answer in time counts as no login.
#[cfg(target_os = "macos")]
fn keychain_exists() -> bool {
    secrets::bounded(secrets::KEYCHAIN_LIMIT, || {
        let mut query = security_framework::item::ItemSearchOptions::new();
        query
            .class(security_framework::item::ItemClass::generic_password())
            .service(CLAUDE_SERVICE)
            .load_attributes(true)
            .load_data(false)
            .limit(1);
        query.search().is_ok_and(|items| !items.is_empty())
    })
    .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn keychain_exists() -> bool {
    false
}

/// A host login, read at a shield start.
pub struct HostToken {
    pub kind: AgentKind,
    pub access: SecretValue,
    /// Milliseconds since the epoch. `None` if the login does not say.
    pub expires: Option<i64>,
    /// The provider's API host, which gets the token.
    pub host: String,
    /// The login document, for the guest login. It has the real tokens: it
    /// never leaves the host.
    doc: Value,
    /// pi's provider key.
    pi: Option<&'static str>,
    /// The start of the real token that comes before the placeholder in the
    /// guest: the non-secret claims up to the `proxy-ep` field of a Copilot
    /// token that is not individual. pi reads its API host there. `access`
    /// is the rest, with the signature.
    prefix: String,
    /// The OAuth scopes of a Google login, for the tokeninfo answer.
    pub scope: Option<String>,
}

impl HostToken {
    /// The whole access token, as the provider gets it.
    pub fn full_access(&self) -> SecretValue {
        SecretValue::new(format!("{}{}", self.prefix, self.access.expose()))
    }
}

/// Shows no token: `doc` has the real refresh token.
impl fmt::Debug for HostToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostToken")
            .field("kind", &self.kind)
            .field("access", &self.access)
            .field("expires", &self.expires)
            .field("host", &self.host)
            .field("doc", &"<redacted>")
            .field("pi", &self.pi)
            .field("prefix", &self.prefix)
            .field("scope", &self.scope)
            .finish()
    }
}

fn bad(kind: AgentKind, why: &str) -> Error {
    Error::HostLogin(kind.name().to_owned(), why.to_owned())
}

/// The `exp` claim of a JWT, in milliseconds. The token is not verified:
/// the expiry only decides when to renew.
fn jwt_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["exp"].as_i64().map(|s| s * 1000)
}

/// Parses the login document of the agent.
pub fn parse(kind: AgentKind, text: &str) -> Result<HostToken> {
    let doc: Value = serde_json::from_str(text).map_err(|_| bad(kind, "the login is not valid JSON"))?;
    let access = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(SecretValue::new);
    let mut prefix = String::new();
    let (access, expires, host, pi) = match kind {
        AgentKind::Claude => {
            let oauth = &doc["claudeAiOauth"];
            (access(&oauth["accessToken"]), oauth["expiresAt"].as_i64(), ANTHROPIC.host.to_owned(), None)
        }
        AgentKind::Codex => {
            let token = access(&doc["tokens"]["access_token"]);
            let expires = token.as_ref().and_then(|t| jwt_expiry(t.expose()));
            (token, expires, CHATGPT.host.to_owned(), None)
        }
        AgentKind::Pi => {
            let provider = PI_PROVIDERS
                .iter()
                .find(|p| doc[p.pi]["type"] == "oauth" && doc[p.pi]["access"].is_string())
                .ok_or_else(|| bad(kind, "no subscription login of a supported provider"))?;
            let entry = &doc[provider.pi];
            let mut token = access(&entry["access"]);
            let mut host = provider.host.to_owned();
            let endpoint =
                token.as_ref().filter(|_| provider.pi == COPILOT.pi).and_then(|t| copilot_endpoint(t.expose()));
            if let Some((api, end)) = endpoint.filter(|(api, _)| api != COPILOT.host) {
                let full = token.take().expect("the endpoint is in the token");
                let (start, rest) = full.expose().split_at(end);
                if rest.is_empty() {
                    return Err(bad(kind, "the Copilot token has no signature after its proxy-ep field"));
                }
                (host, prefix) = (api, start.to_owned());
                token = Some(SecretValue::new(rest));
            }
            (token, entry["expires"].as_i64(), host, Some(provider.pi))
        }
        AgentKind::Gemini => (access(&doc["access_token"]), doc["expiry_date"].as_i64(), GOOGLE.host.to_owned(), None),
    };
    let access = access.ok_or_else(|| bad(kind, "the login has no access token"))?;
    let scope = doc["scope"].as_str().map(str::to_owned);
    Ok(HostToken { kind, access, expires, host, doc, pi, prefix, scope })
}

/// The host login of the agent, checked for its expiry.
pub fn token(source: &dyn LoginSource, kind: AgentKind, now: i64) -> Result<HostToken> {
    let text = source.read(kind)?.ok_or_else(|| Error::HostLoginMissing(kind.name().to_owned()))?;
    let token = parse(kind, &text)?;
    if token.expires.is_some_and(|e| e <= now) {
        return Err(Error::HostLoginExpired(kind.name().to_owned()));
    }
    Ok(token)
}

/// What detection shows of a login. A Claude Code login is not read, so
/// that no macOS prompt shows at app start; its expiry shows at a start.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum LoginState {
    Found { expires: Option<i64> },
    Expired,
    None,
}

pub fn state(source: &dyn LoginSource, kind: AgentKind, now: i64) -> LoginState {
    if kind == AgentKind::Claude {
        return if source.exists(kind) { LoginState::Found { expires: None } } else { LoginState::None };
    }
    match token(source, kind, now) {
        Ok(token) => LoginState::Found { expires: token.expires },
        Err(Error::HostLoginExpired(_)) => LoginState::Expired,
        Err(_) => LoginState::None,
    }
}

/// The provider's API host of the agent's login. `None` for pi, whose host
/// depends on its login: the token read then gives the host.
pub fn api_host(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::Claude => Some(ANTHROPIC.host),
        AgentKind::Codex => Some(CHATGPT.host),
        AgentKind::Gemini => Some(GOOGLE.host),
        AgentKind::Pi => None,
    }
}

/// The host-login secret of the agent: its placeholder goes into the
/// `authorization` header of requests to the provider's `host` only.
pub fn secret(kind: AgentKind, host: &str) -> Secret {
    Secret {
        name: secret_name(kind).to_owned(),
        hosts: vec![host.to_owned()],
        places: Places { header_fields: vec!["authorization".to_owned()], query: false, body: false },
        origin: Origin::HostLogin { agent: kind },
    }
}

/// A file of a guest login, below the shield's state dir.
#[derive(Debug, PartialEq, Eq)]
pub struct GuestFile {
    /// The folders below the state dir.
    pub dir: &'static [&'static str],
    pub name: &'static str,
    pub body: Vec<u8>,
}

fn json_file(dir: &'static [&'static str], name: &'static str, value: &Value) -> GuestFile {
    GuestFile { dir, name, body: serde_json::to_vec_pretty(value).expect("JSON value") }
}

/// The fields `names` of `from` that are set.
fn keep(from: &Value, names: &[&str]) -> Map<String, Value> {
    names.iter().filter(|name| !from[**name].is_null()).map(|name| ((*name).to_owned(), from[*name].clone())).collect()
}

/// The guest login of the agent, from an allow-list of fields: the real
/// non-secret fields, the placeholder for the access token, the inert
/// refresh token and a far expiry. A field that the list does not name
/// never reaches the guest. Claude Code needs no file: it reads the
/// placeholder from `CLAUDE_CODE_OAUTH_TOKEN`.
pub fn guest_files(token: &HostToken, placeholder: &str, now: i64) -> Vec<GuestFile> {
    match token.kind {
        AgentKind::Claude => Vec::new(),
        AgentKind::Gemini => {
            let mut creds = keep(&token.doc, &["scope", "token_type"]);
            creds.insert("access_token".to_owned(), placeholder.into());
            creds.insert("refresh_token".to_owned(), INERT_REFRESH.into());
            creds.insert("expiry_date".to_owned(), FAR_FUTURE_MS.into());
            vec![json_file(&["gemini", ".gemini"], "oauth_creds.json", &creds.into())]
        }
        AgentKind::Codex => {
            let mut tokens = keep(&token.doc["tokens"], &["id_token", "account_id"]);
            tokens.insert("access_token".to_owned(), placeholder.into());
            tokens.insert("refresh_token".to_owned(), INERT_REFRESH.into());
            let mut auth = keep(&token.doc, &["auth_mode"]);
            auth.insert("OPENAI_API_KEY".to_owned(), Value::Null);
            auth.insert("tokens".to_owned(), tokens.into());
            auth.insert("last_refresh".to_owned(), cli::iso(now).into());
            vec![json_file(&["codex"], "auth.json", &auth.into())]
        }
        AgentKind::Pi => {
            let key = token.pi.expect("a pi token has a provider");
            let mut entry = keep(&token.doc[key], &["type", "availableModelIds"]);
            entry.insert("access".to_owned(), format!("{}{placeholder}", token.prefix).into());
            entry.insert("refresh".to_owned(), INERT_REFRESH.into());
            entry.insert("expires".to_owned(), FAR_FUTURE_MS.into());
            let mut doc = Map::new();
            doc.insert(key.to_owned(), entry.into());
            vec![json_file(&["agent"], "auth.json", &Value::Object(doc))]
        }
    }
}

/// The answer of the tokeninfo endpoint for the token: its scopes and
/// expiry, read on the Mac. `None` if the login names no scope.
pub fn tokeninfo(token: &HostToken, now: i64) -> Option<Value> {
    let scope = token.scope.as_deref()?;
    let expires = token.expires.unwrap_or(now + 3_600_000);
    let seconds = ((expires - now) / 1000).max(0);
    Some(serde_json::json!({
        "scope": scope,
        "exp": (expires / 1000).to_string(),
        "expires_in": seconds.to_string(),
        "access_type": "offline",
    }))
}

#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    /// Logins from a map; a missing kind has no login.
    #[derive(Default)]
    pub struct FakeLogins {
        pub docs: Mutex<HashMap<AgentKind, String>>,
    }

    impl LoginSource for FakeLogins {
        fn read(&self, kind: AgentKind) -> Result<Option<String>> {
            Ok(self.docs.lock().unwrap().get(&kind).cloned())
        }

        fn exists(&self, kind: AgentKind) -> bool {
            self.docs.lock().unwrap().contains_key(&kind)
        }
    }

    pub fn claude_doc(token: &str, expires: i64) -> String {
        json!({ "claudeAiOauth": { "accessToken": token, "refreshToken": "rt-secret", "expiresAt": expires, "scopes": ["user:inference"], "subscriptionType": "max" } }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::fake::*;
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn jwt(exp_s: i64) -> String {
        let part = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.sig", part(json!({ "alg": "RS256" })), part(json!({ "exp": exp_s })))
    }

    fn files(token: &HostToken) -> Vec<(String, Value)> {
        guest_files(token, "$MSB_X$", NOW)
            .into_iter()
            .map(|f| (format!("{}/{}", f.dir.join("/"), f.name), serde_json::from_slice(&f.body).unwrap()))
            .collect()
    }

    #[test]
    fn claude_uses_the_keychain_token_and_needs_no_file() {
        let token = parse(AgentKind::Claude, &claude_doc("sk-ant-oat-1", NOW + 1000)).unwrap();
        assert_eq!(
            (token.access.expose(), token.expires, token.host.as_str()),
            ("sk-ant-oat-1", Some(NOW + 1000), "api.anthropic.com")
        );
        assert!(guest_files(&token, "$MSB_X$", NOW).is_empty());
        let secret = secret(token.kind, api_host(AgentKind::Claude).unwrap());
        assert_eq!(secret.name, "CLAUDE_CODE_OAUTH_TOKEN");
        assert_eq!(
            (secret.hosts, secret.places.header_fields),
            (vec!["api.anthropic.com".to_owned()], vec!["authorization".to_owned()])
        );
        assert!(!secret.places.query && !secret.places.body);
    }

    #[test]
    fn codex_keeps_only_the_listed_fields() {
        let access = jwt(NOW / 1000 + 3600);
        let doc = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": "sk-old",
            "tokens": { "id_token": "id.jwt.real", "access_token": access, "refresh_token": "rt-secret", "account_id": "acc-1", "extra": "x-secret" },
            "last_refresh": "2025-01-01T00:00:00Z",
            "agent_identity": "secret-key",
        });
        let token = parse(AgentKind::Codex, &doc.to_string()).unwrap();
        assert_eq!((token.expires, token.host.as_str()), (Some(NOW + 3_600_000), "chatgpt.com"));
        let [(path, auth)]: [_; 1] = files(&token).try_into().unwrap();
        assert_eq!(path, "codex/auth.json");
        assert_eq!(
            auth,
            json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": { "id_token": "id.jwt.real", "account_id": "acc-1", "access_token": "$MSB_X$", "refresh_token": INERT_REFRESH },
                "last_refresh": cli::iso(NOW),
            })
        );
    }

    #[test]
    fn pi_takes_the_first_supported_provider_and_drops_the_others() {
        let doc = json!({
            "openai": { "type": "api_key", "key": "sk-1" },
            "github-copilot": { "type": "oauth", "refresh": "gho-secret", "access": "tid-secret", "expires": NOW + 1000, "availableModelIds": ["gpt-5"], "extra": "x-secret" },
            "google-gemini-cli": { "type": "oauth", "refresh": "g-r", "access": "g-a", "expires": NOW + 1000, "projectId": "p-1" },
        });
        let token = parse(AgentKind::Pi, &doc.to_string()).unwrap();
        assert_eq!((token.access.expose(), token.host.as_str()), ("tid-secret", "api.individual.githubcopilot.com"));
        let [(path, auth)]: [_; 1] = files(&token).try_into().unwrap();
        assert_eq!(path, "agent/auth.json");
        assert_eq!(
            auth,
            json!({ "github-copilot": { "type": "oauth", "availableModelIds": ["gpt-5"], "access": "$MSB_X$", "refresh": INERT_REFRESH, "expires": FAR_FUTURE_MS } })
        );
        assert!(matches!(
            parse(AgentKind::Pi, r#"{"openai":{"type":"api_key","key":"k"}}"#),
            Err(Error::HostLogin(..))
        ));
    }

    #[test]
    fn copilot_takes_its_api_host_from_the_token() {
        let doc = |access: &str| {
            json!({ "github-copilot": { "type": "oauth", "refresh": "r", "access": access, "expires": NOW + 1000 } })
                .to_string()
        };
        let login = |access: &str| parse(AgentKind::Pi, &doc(access)).unwrap();
        // The guest's access names the host for pi; the proxy adds the rest.
        for (access, host, guest, value) in [
            (
                "tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=x:sig",
                "api.business.githubcopilot.com",
                "tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com$MSB_X$",
                ";st=x:sig",
            ),
            (
                "tid=1;proxy-ep=proxy.enterprise.githubcopilot.com;8kp=1:sig",
                "api.enterprise.githubcopilot.com",
                "tid=1;proxy-ep=proxy.enterprise.githubcopilot.com$MSB_X$",
                ";8kp=1:sig",
            ),
        ] {
            let token = login(access);
            assert_eq!(token.host, host);
            assert_eq!(files(&token)[0].1["github-copilot"]["access"], guest);
            assert_eq!(token.access.expose(), value);
            assert_eq!(format!("{}{}", token.prefix, token.access.expose()), access);
        }
        assert!(matches!(
            parse(AgentKind::Pi, &doc("tid=1;proxy-ep=proxy.business.githubcopilot.com")),
            Err(Error::HostLogin(..))
        ));
        // A missing, individual or foreign host: the individual host, and
        // the guest gets only the placeholder.
        for access in [
            "tid=1;exp=2",
            "tid=1;proxy-ep=proxy.individual.githubcopilot.com;x:sig",
            "proxy-ep=proxy.evil.com;",
            "proxy-ep=proxy.x.githubcopilot.com.evil.com",
            "proxy-ep=proxy.*.githubcopilot.com",
            "proxy-ep=api.githubcopilot.com",
        ] {
            let token = login(access);
            assert_eq!(
                (token.host.as_str(), token.access.expose()),
                ("api.individual.githubcopilot.com", access),
                "{access}"
            );
            assert_eq!(files(&token)[0].1["github-copilot"]["access"], "$MSB_X$");
        }
        // Each Copilot host denies the refresh host.
        let business = secret(AgentKind::Pi, "api.business.githubcopilot.com");
        assert_eq!(denied_hosts(&[business]), ["api.github.com"]);
    }

    #[test]
    fn gemini_gets_a_placeholder_login_and_a_local_tokeninfo() {
        let doc = json!({ "access_token": "ya29.secret", "refresh_token": "1//secret", "expiry_date": NOW + 60_000,
            "scope": "openid https://www.googleapis.com/auth/cloud-platform", "token_type": "Bearer", "id_token": "id.secret" });
        let token = parse(AgentKind::Gemini, &doc.to_string()).unwrap();
        assert_eq!((token.access.expose(), token.host.as_str()), ("ya29.secret", "cloudcode-pa.googleapis.com"));
        let [(path, creds)]: [_; 1] = files(&token).try_into().unwrap();
        assert_eq!(path, "gemini/.gemini/oauth_creds.json");
        assert_eq!(
            creds,
            json!({ "scope": "openid https://www.googleapis.com/auth/cloud-platform", "token_type": "Bearer",
                "access_token": "$MSB_X$", "refresh_token": INERT_REFRESH, "expiry_date": FAR_FUTURE_MS })
        );
        let info = tokeninfo(&token, NOW).unwrap();
        assert_eq!(info["expires_in"], "60");
        assert_eq!(info["scope"], "openid https://www.googleapis.com/auth/cloud-platform");
        assert!(!info.to_string().contains("secret"));

        let debug = format!("{token:?}");
        assert!(!debug.contains("ya29.secret") && !debug.contains("1//secret"), "{debug}");
    }

    #[test]
    fn a_missing_or_expired_login_fails_and_detection_shows_it() {
        let logins = FakeLogins::default();
        assert!(matches!(token(&logins, AgentKind::Claude, NOW), Err(Error::HostLoginMissing(_))));
        assert_eq!(state(&logins, AgentKind::Pi, NOW), LoginState::None);
        logins.docs.lock().unwrap().insert(AgentKind::Claude, claude_doc("t", NOW - 1));
        assert!(matches!(token(&logins, AgentKind::Claude, NOW), Err(Error::HostLoginExpired(_))));
        // The Keychain is not read for detection.
        assert_eq!(state(&logins, AgentKind::Claude, NOW), LoginState::Found { expires: None });
        let pi = |expires: i64| {
            json!({ "github-copilot": { "type": "oauth", "access": "a", "expires": expires } }).to_string()
        };
        logins.docs.lock().unwrap().insert(AgentKind::Pi, pi(NOW - 1));
        assert_eq!(state(&logins, AgentKind::Pi, NOW), LoginState::Expired);
        logins.docs.lock().unwrap().insert(AgentKind::Pi, pi(NOW + 5));
        assert_eq!(state(&logins, AgentKind::Pi, NOW), LoginState::Found { expires: Some(NOW + 5) });
    }

    #[test]
    fn refresh_hosts_follow_the_providers_of_the_host_logins() {
        for host in [
            "console.anthropic.com",
            "platform.claude.com",
            "auth.openai.com",
            "api.github.com",
            "oauth2.googleapis.com",
        ] {
            assert!(is_refresh_host(host), "{host}");
        }
        assert!(!is_refresh_host("api.anthropic.com"));
        let claude = secret(AgentKind::Claude, "api.anthropic.com");
        let mut user = claude.clone();
        user.origin = Origin::User;
        assert!(denied_hosts(&[user]).is_empty());
        assert_eq!(denied_hosts(&[claude.clone(), claude]), ["console.anthropic.com", "platform.claude.com"]);
    }
}
