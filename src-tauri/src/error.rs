use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path is not absolute: {0}")]
    RelativePath(String),
    #[error("path does not exist: {0}")]
    MissingPath(String),
    #[error("path is not a directory: {0}")]
    NotDirectory(String),
    #[error("guest path must be absolute and normalized: {0}")]
    InvalidGuestPath(String),
    #[error("guest path is reserved for zwinger (/opt/zwinger): {0}")]
    ReservedGuestPath(String),
    #[error("guest path overlaps /workspace: {0}")]
    WorkspaceOverlap(String),
    #[error("guest path is already mounted: {0}")]
    DuplicateMount(String),
    #[error("host folders overlap and one is writable: {0} and {1}")]
    MountOverlap(String, String),
    #[error("{0} overlaps the zwinger data folder {1}")]
    AppDataOverlap(String, String),
    #[error("path now resolves to a different place: {0}")]
    PathChanged(String),
    #[error("folder is not verified since the update: {0}; confirm that it is the folder that you added")]
    PathUnverified(String),
    #[error("name must have 1 to 64 characters and no control characters: {0}")]
    InvalidName(String),
    #[error("harness uses {0}, which is not below /workspace or a mount")]
    HarnessTarget(String),
    #[error("harness uses {0}, which is in {1}; change the harness first")]
    HarnessMount(String, String),
    #[error("{0} is not below /workspace or a mount")]
    GuestDirTarget(String),
    #[error("app {0} starts in {1}, which is not below /workspace or a mount")]
    AppCwd(String, String),
    #[error("invalid app {0}: {1}")]
    InvalidApp(String, String),
    #[error("app not found")]
    AppNotFound,
    #[error("app is already running")]
    AppRunning,
    #[error("the app was stopped while it started")]
    AppStopped,
    #[error("the old run of the app did not exit")]
    AppStillRunning,
    #[error("task {0} runs in {1}, which is not below /workspace or a mount")]
    TaskWorkdir(String, String),
    #[error("invalid task {0}: {1}")]
    InvalidTask(String, String),
    #[error("task not found")]
    TaskNotFound,
    #[error("the task has a run that is not done")]
    TaskRunning,
    #[error("run not found")]
    RunNotFound,
    #[error("the run cannot do this now: {0}")]
    RunState(&'static str),
    #[error("the run cannot write its artifacts to {0}: {1}")]
    MirrorUnwritable(String, String),
    #[error("not a round folder: {0}")]
    NotRound(String),
    #[error("the order must name each item once")]
    InvalidOrder,
    #[error("invalid domain {0}: {1}")]
    InvalidDomain(String, String),
    #[error("invalid secret {0}: {1}")]
    InvalidSecret(String, String),
    #[error("secret {0} already exists")]
    DuplicateSecret(String),
    #[error("secret {0} needs a value")]
    SecretValueRequired(String),
    #[error("secret {0} not found")]
    SecretNotFound(String),
    #[error("secret {0} has no value in the Keychain; set it again")]
    SecretMissing(String),
    #[error("Keychain: {0}")]
    Keychain(String),
    #[error("the Keychain did not answer in time; allow access in the macOS prompt, then try again")]
    KeychainTimeout,
    #[error("a Keychain change of secret {0} has not finished; answer the macOS prompt, then try again")]
    SecretPending(String),
    #[error("shield not found")]
    ShieldNotFound,
    #[error("mount not found")]
    MountNotFound,
    #[error("session not found")]
    SessionNotFound,
    #[error("terminal not found")]
    TerminalNotFound,
    #[error("the terminal is already open")]
    TerminalOpen,
    #[error("shield is not running")]
    NotRunning,
    #[error("shield is already started")]
    AlreadyStarted,
    #[error("shield is starting")]
    Starting,
    #[error("shield is stopping")]
    Stopping,
    #[error("session process ended")]
    SessionClosed,
    #[error("the shield image has Node {0}, but pi needs Node 22.19 or newer")]
    NodeTooOld(String),
    #[error("the agent process ended")]
    AgentClosed,
    #[error("unknown runtime: {0}")]
    UnknownRuntime(String),
    #[error("{0} is not installed")]
    RuntimeMissing(String),
    #[error("{0} was installed after the shield started; restart the shield")]
    RuntimeNotMounted(String),
    #[error("{0} is being installed")]
    RuntimeInstalling(String),
    #[error("the install of {0} was cancelled")]
    InstallCancelled(String),
    #[error("no agent runtime for this computer ({0})")]
    UnsupportedArch(String),
    #[error("{0} {1} has no hash that zwinger pins; confirm the download of this version first")]
    UnpinnedVersion(String, String),
    #[error("{0} is already a host agent of this shield")]
    DuplicateHostAgent(String),
    #[error("{0} is not installed on this Mac")]
    HostAgentMissing(String),
    #[error("{0} runs only from this Mac; add it to the shield with Add agent, then restart the shield")]
    NotHostAgent(String),
    #[error("{0} from this Mac does not run in the shield: {1}")]
    HostAgentFailed(String, String),
    #[error("the {0} login on this Mac: {1}")]
    HostLogin(String, String),
    #[error("{0} has no login on this Mac; log in with {0} on the Mac first")]
    HostLoginMissing(String),
    #[error("the {0} login on this Mac has expired; open {0} on the Mac once to renew it, then start the shield again")]
    HostLoginExpired(String),
    #[error("secret {0} is the host login of an agent; change it in the agent's settings")]
    HostLoginSecret(String),
    #[error(
        "{0} is blocked in this shield: a host login of an agent is refreshed there, so {1} cannot reach it; remove {1} or turn that login off"
    )]
    RefreshHostConflict(String, String),
    #[error("the download of {0} does not match its integrity hash")]
    IntegrityMismatch(String),
    #[error("invalid package of {0}: {1}")]
    BadPackage(String, String),
    #[error("{0} shields still run; stop them to restart for the update")]
    ShieldsRunning(usize),
    #[error("MSB_PATH names a VM runtime that does not work: {0}; fix MSB_PATH or remove it, then open zwinger again")]
    DeveloperRuntime(String),
    #[error("update: {0}")]
    Update(String),
    #[error("download: {0}")]
    Download(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sandbox(#[from] microsandbox::MicrosandboxError),
}

impl Error {
    /// The variant name, for the UI to tell errors apart.
    pub fn code(&self) -> String {
        // The derived Debug output starts with the variant name.
        let debug = format!("{self:?}");
        debug.split(['(', ' ', '{']).next().unwrap_or_default().to_owned()
    }

    /// The text that the user reads. The wrapped errors of other crates get
    /// a plain explanation; the rest are already written for the user.
    pub fn message(&self) -> String {
        match self {
            Error::Download(_) => "The download failed. Check the internet connection, then try again.".into(),
            Error::Io(e) => format!("A file operation on this Mac failed: {e}."),
            Error::Json(e) => format!("zwinger could not read its saved data: {e}."),
            Error::Sandbox(e) => sandbox_message(e),
            Error::ShieldsRunning(1) => "A shield still runs. Stop it, then restart to update.".into(),
            other => sentence(&other.to_string()),
        }
    }
}

fn sandbox_message(error: &microsandbox::MicrosandboxError) -> String {
    use microsandbox::MicrosandboxError as E;
    match error {
        E::RuntimeNotInstalled(_) | E::RuntimeIncomplete(_) | E::LibkrunfwNotFound(_) => {
            "The VM runtime is missing or damaged. Download zwinger again and replace the app to reinstall it.".into()
        }
        E::Http(_) => "The download failed. Check the internet connection, then try again.".into(),
        E::Image(e) => {
            format!("zwinger could not get the shield image. Check the internet connection, then try again ({e}).")
        }
        E::BootStart { .. } => format!("The shield VM did not start: {error}."),
        E::Database(_) => "The VM database is not available. Quit zwinger and open it again.".into(),
        E::ExecTimeout(_) => "The command in the shield did not answer in time.".into(),
        _ => format!("The VM runtime failed: {error}."),
    }
}

/// Starts with a capital letter and ends with a full stop. The app name stays lowercase.
fn sentence(text: &str) -> String {
    let mut out = match text.chars().next() {
        Some(first) if !text.starts_with("zwinger") => first.to_uppercase().chain(text.chars().skip(1)).collect(),
        _ => text.to_owned(),
    };
    if !out.ends_with(['.', '?', '!']) {
        out.push('.');
    }
    out
}

/// `{code, message}`: the one place where an error becomes text for the user.
impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("code", &self.code())?;
        map.serialize_entry("message", &self.message())?;
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_a_code_and_a_sentence() {
        assert_eq!(
            serde_json::to_value(Error::InvalidOrder).unwrap(),
            json!({ "code": "InvalidOrder", "message": "The order must name each item once." })
        );
        assert_eq!(
            serde_json::to_value(Error::MissingPath("/x".into())).unwrap(),
            json!({ "code": "MissingPath", "message": "Path does not exist: /x." })
        );
        assert_eq!(Error::ShieldNotFound.message(), "Shield not found.");
        assert_eq!(Error::RunState("paused").code(), "RunState");
    }

    #[test]
    fn keeps_the_app_name_and_the_full_stop() {
        assert_eq!(sentence("zwinger stopped."), "zwinger stopped.");
        assert_eq!(sentence("done?"), "Done?");
        assert_eq!(Error::ShieldsRunning(1).message(), "A shield still runs. Stop it, then restart to update.");
        assert_eq!(Error::ShieldsRunning(2).message(), "2 shields still run; stop them to restart for the update.");
    }

    #[test]
    fn explains_the_errors_of_other_crates() {
        let io = Error::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(io.code(), "Io");
        assert_eq!(io.message(), "A file operation on this Mac failed: denied.");
        let missing = Error::Sandbox(microsandbox::MicrosandboxError::RuntimeNotInstalled("x".into()));
        assert_eq!(missing.code(), "Sandbox");
        assert!(missing.message().starts_with("The VM runtime is missing or damaged."));
        assert!(!missing.message().contains("open it again"));
        let other = Error::Sandbox(microsandbox::MicrosandboxError::Custom("boom".into()));
        assert_eq!(other.message(), "The VM runtime failed: boom.");
        let json = Error::Json(serde_json::from_str::<u8>("x").unwrap_err());
        assert!(json.message().starts_with("zwinger could not read its saved data: "));
    }
}
