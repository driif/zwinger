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
    #[error("shield not found")]
    ShieldNotFound,
    #[error("mount not found")]
    MountNotFound,
    #[error("session not found")]
    SessionNotFound,
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
    #[error("agent not found")]
    AgentNotFound,
    #[error("the agent process ended")]
    AgentClosed,
    #[error("chat not found")]
    ChatNotFound,
    #[error("pi: {0}")]
    Agent(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sandbox(#[from] microsandbox::MicrosandboxError),
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
