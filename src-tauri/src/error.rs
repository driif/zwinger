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
    #[error(
        "the mount {guest} ({host}) of cage {cage} overlaps the zwinger data folder {dir}. Remove it in Edit > Mounts, then try again"
    )]
    MountAppData { cage: String, guest: String, host: String, dir: String },
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
    #[error("{0} is the project folder; choose a different folder for the harness")]
    HarnessIsProject(String),
    #[error("The folder is in a read-only mount; choose another folder or make the mount read-write")]
    HarnessInReadonlyMount,
    #[error("{0} is not below /workspace or a mount")]
    GuestDirTarget(String),
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
    #[error("the run cannot resume: {0}")]
    Resume(String),
    #[error("The app could not read the run record ({0})")]
    RunRecord(String),
    #[error("commit or PR is not available: {0}")]
    RunGit(String),
    #[error("only a run of this app session can be shared")]
    RunNotShareable,
    #[error("the run cannot do this now: {0}")]
    RunState(&'static str),
    #[error("the run cannot write its artifacts to {0}: {1}")]
    MirrorUnwritable(String, String),
    #[error("zwinger cannot follow the folders of the project: {0}")]
    FolderWatch(String),
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
    #[error("rule not found")]
    RuleNotFound,
    #[error("the ask is no longer pending: it timed out, or its request or cage ended")]
    AskNotFound,
    #[error("invalid network profile: {0}")]
    InvalidProfile(String),
    #[error("network profile not found")]
    ProfileNotFound,
    #[error("invalid rule: {0}")]
    InvalidRule(String),
    #[error("a host port must be 1-65535, not {0}")]
    InvalidHostPort(u16),
    #[error("invalid published port: {0}")]
    InvalidPublishedPort(String),
    #[error("port {0} of this Mac is in use; pick a different port")]
    HostPortInUse(u16),
    #[error("the network proxy of the cage does not start, so the cage does not start: {0}")]
    Proxy(String),
    #[error("secret {0} has no stored value; set it again")]
    SecretMissing(String),
    #[error("credential vault: {0}")]
    Vault(String),
    #[error(
        "the credential vault \"{path}\" can be read by other users; in Terminal, run `chmod {expected} {argument}`, then try again",
        argument = crate::agent::launch::quote(.path)
    )]
    VaultMode { path: String, expected: String },
    #[error(
        "the credential vault \"{path}\" belongs to a different user; ask an administrator of this Mac to make you its owner, then try again"
    )]
    VaultOwner { path: String },
    #[error("the credential vault \"{path}\" is a symbolic link; move the link to a different folder, then try again")]
    VaultLink { path: String },
    #[error("the credential vault \"{path}\" is not a {kind}; move it to a different folder, then try again")]
    VaultType { path: String, kind: &'static str },
    /// A Keychain failure of the import of old credentials. The text comes
    /// from the OSStatus only, never from an item.
    #[error("{0}")]
    LegacyKeychain(String),
    #[error("an import or cleanup of the old Keychain items already runs; wait until it ends")]
    LegacyKeychainBusy,
    #[error("library credential {0} not found; add it in Settings first")]
    LibraryCredentialNotFound(String),
    #[error("library credential {0} already exists; rotate it instead")]
    LibraryCredentialExists(String),
    #[error("library credential {name} is attached in {}; detach it there first", .cages.join(", "))]
    LibraryCredentialInUse { name: String, cages: Vec<String> },
    #[error("project not found")]
    SessionNotFound,
    #[error("the cage of this project is missing")]
    CageNotFound,
    #[error("{owner} uses cage {cage}, which is not in cages.json; restore cages.json, then open zwinger again")]
    MissingCage { owner: String, cage: String },
    #[error("this session has no cage; pick a cage for it or a default cage for its workspace")]
    NoCage,
    #[error("cage {cage} runs for {}; restart it to add this session", names(.members))]
    RestartToAdd { cage: String, members: Vec<Member>, tasks: Vec<String>, generation: u64 },
    #[error("cage {cage} also runs for {}; a stop ends their VM terminals and task runs", names(.members))]
    StopShared { cage: String, members: Vec<Member>, tasks: Vec<String>, generation: u64 },
    #[error("a stop of cage {cage} ends the VM terminals and the task runs of this session")]
    StopAlone { cage: String, tasks: Vec<String>, generation: u64 },
    #[error("the sessions of the VM changed; look at them and try again")]
    StaleRestart,
    #[error("this session has a cage, so an agent with no cage does not open in it; open the agent in the cage")]
    HasCage,
    #[error("cage {0} is in use: {1}")]
    CageInUse(String, String),
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("the workspace has sessions; move them to a different workspace first")]
    WorkspaceNotEmpty,
    #[error("the last workspace cannot be deleted")]
    LastWorkspace,
    #[error(
        "zwinger could not read or back up {0}, so it does not save workspace changes; move the file away, then open zwinger again"
    )]
    WorkspacesLocked(String),
    #[error("mount not found")]
    MountNotFound,
    #[error("the removal can no longer be undone")]
    UndoExpired,
    #[error("the removal cannot be undone: {0}")]
    UndoConflict(String),
    #[error("terminal process not found")]
    ProcessNotFound,
    #[error("terminal not found")]
    TerminalNotFound,
    #[error("the terminal is already open")]
    TerminalOpen,
    #[error("invalid pane layout: {0}")]
    InvalidPaneLayout(String),
    #[error("cage is not running")]
    NotRunning,
    #[error("cage is already started")]
    AlreadyStarted,
    #[error("could not delete the agent state of VM {key} at {path} ({source}); delete it by hand")]
    VmStateLeft { key: String, path: String, source: std::io::Error },
    #[error(
        "the agent state of VM {key} is still at {legacy} and not at {target}. Restart zwinger to move it. If it does not move, move or delete one of the two, then restart zwinger"
    )]
    VmStateNotMoved { key: String, legacy: String, target: String },
    #[error(
        "zwinger does not use {path} for agent state, because it is a link or not a folder. Move or delete it, then restart zwinger"
    )]
    VmStatePath { path: String },
    #[error(
        "zwinger does not read {path}, because it is a link or not a regular file. Move it away or replace it with the file, then restart zwinger"
    )]
    StoreFile { path: String },
    #[error("the VM key {0:?} is not a valid folder name")]
    BadVmKey(String),
    #[error(
        "the VMs of an earlier app run in {homes} are not stopped, so zwinger does not delete, reset or release agent state now. Start a cage or restart zwinger to stop them, then try again"
    )]
    NotReaped { homes: String },
    #[error("cage is starting")]
    Starting,
    #[error("cage is stopping")]
    Stopping,
    #[error("session process ended")]
    SessionClosed,
    #[error("the cage image has Node {0}, but pi needs Node 22.19 or newer")]
    NodeTooOld(String),
    #[error("the agent process ended")]
    AgentClosed,
    #[error("unknown runtime: {0}")]
    UnknownRuntime(String),
    #[error("{0} is not installed")]
    RuntimeMissing(String),
    #[error("{0} was installed after the cage started; restart the cage")]
    RuntimeNotMounted(String),
    #[error("{0} is being installed")]
    RuntimeInstalling(String),
    #[error("{0} {1} is in use: a running VM or an agent on this Mac uses it")]
    RuntimeInUse(String, String),
    #[error("{0} {1} is the pinned version; it stays")]
    RuntimePinned(String, String),
    #[error("the install of {0} was cancelled")]
    InstallCancelled(String),
    #[error("no agent runtime for this computer ({0})")]
    UnsupportedArch(String),
    #[error("{0} {1} has no hash that zwinger pins; confirm the download of this version first")]
    UnpinnedVersion(String, String),
    #[error("{0} is already a host agent of this project")]
    DuplicateHostAgent(String),
    #[error("{0} is not installed on this Mac")]
    HostAgentMissing(String),
    #[error("{0} runs only from this Mac; add it to the project with Add agent, then restart the cage")]
    NotHostAgent(String),
    #[error("a cage cannot use {0} from this Mac; the cage runs its own copy")]
    NoHostUse(String),
    #[error("{0} from this Mac does not run in the cage: {1}")]
    HostAgentFailed(String, String),
    #[error("the {0} login on this Mac: {1}")]
    HostLogin(String, String),
    #[error("{0} has no login on this Mac; log in with {0} on the Mac first")]
    HostLoginMissing(String),
    #[error("{0}")]
    HostLoginExpired(String),
    #[error(
        "{0} is blocked in this cage: a host login of an agent is refreshed there, so {1} cannot reach it; remove {1} or turn that login off"
    )]
    RefreshHostConflict(String, String),
    #[error("the download of {0} does not match its integrity hash")]
    IntegrityMismatch(String),
    #[error("invalid package of {0}: {1}")]
    BadPackage(String, String),
    #[error("{0} cages still run; stop them to restart for the update")]
    CagesRunning(usize),
    #[error("MSB_PATH names a VM runtime that does not work: {0}; fix MSB_PATH or remove it, then open zwinger again")]
    DeveloperRuntime(String),
    #[error("no editor is on; turn one on in Settings, Editors")]
    NoEditor,
    #[error("{0} did not open: {1}")]
    EditorFailed(String, String),
    #[error("{0} has no command-line launcher at {1}; install the app again")]
    EditorLauncherMissing(String, String),
    #[error("the folder of the terminal is unknown or gone, so the relative path {0} does not open")]
    LinkFolderUnknown(String),
    #[error("the cage of the terminal is not running, so {0} does not open")]
    LinkVmStopped(String),
    #[error("{0} is not in a folder that the cage shares with this Mac")]
    LinkNotShared(String),
    #[error("file not found: {0}")]
    LinkMissing(String),
    #[error("{0} is not a file")]
    LinkNotFile(String),
    #[error("a line and a column start at 1")]
    LinkZeroPosition,
    #[error("unknown editor: {0}")]
    UnknownEditor(String),
    #[error(
        "a cage needs {}-{} CPUs and {}-{} MiB of memory, not {cpus} CPUs and {memory_mib} MiB",
        crate::cage::CPUS.start(), crate::cage::CPUS.end(),
        crate::cage::MEMORY_MIB.start(), crate::cage::MEMORY_MIB.end()
    )]
    InvalidResources { cpus: u32, memory_mib: u32 },
    #[error("accent color must be #rrggbb: {0}")]
    InvalidAccent(String),
    #[error("invalid theme: {0}")]
    InvalidTheme(String),
    #[error("invalid preference change: {0}")]
    InvalidPreference(String),
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

/// A member session of a running VM, as an error names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub id: String,
    pub name: String,
}

fn names(members: &[Member]) -> String {
    members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
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
            Error::CagesRunning(1) => "A cage still runs. Stop it, then restart to update.".into(),
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
            format!("zwinger could not get the cage image. Check the internet connection, then try again ({e}).")
        }
        E::BootStart { .. } => format!("The cage did not start: {error}."),
        E::Database(_) => "The VM database is not available. Quit zwinger and open it again.".into(),
        E::ExecTimeout(_) => "The command in the cage did not answer in time.".into(),
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
/// An error that the UI confirms and retries also has `detail`.
impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("code", &self.code())?;
        map.serialize_entry("message", &self.message())?;
        match self {
            Error::RestartToAdd { cage, members, tasks, generation }
            | Error::StopShared { cage, members, tasks, generation } => map.serialize_entry(
                "detail",
                &serde_json::json!({ "cage": cage, "members": members, "tasks": tasks, "generation": generation }),
            )?,
            Error::StopAlone { cage, tasks, generation } => map.serialize_entry(
                "detail",
                &serde_json::json!({ "cage": cage, "members": [], "tasks": tasks, "generation": generation }),
            )?,
            _ => {}
        }
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
        assert_eq!(Error::SessionNotFound.message(), "Project not found.");
        assert_eq!(Error::RunState("paused").code(), "RunState");
        let members = vec![Member { id: "a".into(), name: "A".into() }, Member { id: "b".into(), name: "B".into() }];
        assert_eq!(
            serde_json::to_value(Error::RestartToAdd { cage: "c".into(), members, tasks: vec![], generation: 3 })
                .unwrap(),
            json!({
                "code": "RestartToAdd",
                "message": "Cage c runs for A, B; restart it to add this session.",
                "detail": {
                    "cage": "c",
                    "members": [{ "id": "a", "name": "A" }, { "id": "b", "name": "B" }],
                    "tasks": [],
                    "generation": 3,
                },
            })
        );
    }

    #[test]
    fn a_shared_stop_has_the_members_and_the_generation() {
        let members = vec![Member { id: "b".into(), name: "B".into() }];
        assert_eq!(
            serde_json::to_value(Error::StopShared {
                cage: "c".into(),
                members,
                tasks: vec!["Fix login in A".into()],
                generation: 4
            })
            .unwrap(),
            json!({
                "code": "StopShared",
                "message": "Cage c also runs for B; a stop ends their VM terminals and task runs.",
                "detail": {
                    "cage": "c",
                    "members": [{ "id": "b", "name": "B" }],
                    "tasks": ["Fix login in A"],
                    "generation": 4,
                },
            })
        );
    }

    #[test]
    fn a_stop_alone_has_no_members() {
        assert_eq!(
            serde_json::to_value(Error::StopAlone {
                cage: "c".into(),
                tasks: vec!["Fix login in A".into()],
                generation: 2
            })
            .unwrap()["detail"],
            json!({ "cage": "c", "members": [], "tasks": ["Fix login in A"], "generation": 2 })
        );
    }

    #[test]
    fn keeps_the_app_name_and_the_full_stop() {
        assert_eq!(sentence("zwinger stopped."), "zwinger stopped.");
        assert_eq!(sentence("done?"), "Done?");
        assert_eq!(Error::CagesRunning(1).message(), "A cage still runs. Stop it, then restart to update.");
        assert_eq!(Error::CagesRunning(2).message(), "2 cages still run; stop them to restart for the update.");
    }

    #[test]
    fn each_vault_problem_quotes_the_path_and_gives_one_step() {
        let path = "/a b/vault";
        assert_eq!(
            Error::VaultMode { path: path.into(), expected: "700".into() }.message(),
            "The credential vault \"/a b/vault\" can be read by other users; in Terminal, run `chmod 700 '/a b/vault'`, then try again."
        );
        assert_eq!(
            Error::VaultOwner { path: path.into() }.message(),
            "The credential vault \"/a b/vault\" belongs to a different user; ask an administrator of this Mac to make you its owner, then try again."
        );
        assert_eq!(
            Error::VaultLink { path: path.into() }.message(),
            "The credential vault \"/a b/vault\" is a symbolic link; move the link to a different folder, then try again."
        );
        assert_eq!(
            Error::VaultType { path: path.into(), kind: "folder" }.message(),
            "The credential vault \"/a b/vault\" is not a folder; move it to a different folder, then try again."
        );
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
