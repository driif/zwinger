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
    #[error("guest path overlaps /workspace: {0}")]
    WorkspaceOverlap(String),
    #[error("guest path is already mounted: {0}")]
    DuplicateMount(String),
    #[error("host folders overlap and one is writable: {0} and {1}")]
    MountOverlap(String, String),
    #[error("path now resolves to a different place: {0}")]
    PathChanged(String),
    #[error("name must have 1 to 64 characters and no control characters: {0}")]
    InvalidName(String),
    #[error("harness uses {0}, which is not below /workspace or a mount")]
    HarnessTarget(String),
    #[error("harness uses {0}, which is in {1}; change the harness first")]
    HarnessMount(String, String),
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
