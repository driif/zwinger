//! The phases of a shield start, their durations, and the progress that the UI shows.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::agent::AgentKind;
use crate::shield::manager::{State, Vm, lock};

/// Gets the shield id, the phase that begins, and the ms since the start began.
pub type ProgressSink = Box<dyn Fn(&str, &Phase, u64) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "step", content = "agent", rename_all = "camelCase")]
pub enum Phase {
    /// The checks and the vault reads.
    Vault,
    /// The host agents: their profiles, logins and runtimes.
    Prepare,
    /// The copy of an agent profile.
    Profile(AgentKind),
    /// The CA load and the proxy start.
    Proxy,
    Boot,
    /// The checks and the setup in the guest after the boot. They run at
    /// once, so the start begins only this phase; each has its own timing.
    Guest,
    Node,
    /// The TUI probe of the host pi.
    Tui,
    /// The probe of the host Gemini CLI.
    Gemini,
    /// The login shell profile with the agent commands.
    Setup,
    /// The proxy CA in the guest's CA bundle.
    Trust,
    /// The network log and the agent commands.
    Wrappers,
    /// The autostart apps, after the shield runs.
    Apps,
}

impl Phase {
    fn label(&self) -> String {
        match self {
            Phase::Vault => "vault".into(),
            Phase::Prepare => "prepare".into(),
            Phase::Profile(kind) => format!("profile {}", kind.name()),
            Phase::Proxy => "proxy".into(),
            Phase::Boot => "boot".into(),
            Phase::Guest => "guest".into(),
            Phase::Node => "node".into(),
            Phase::Tui => "tui".into(),
            Phase::Gemini => "gemini".into(),
            Phase::Setup => "setup".into(),
            Phase::Trust => "trust".into(),
            Phase::Wrappers => "wrappers".into(),
            Phase::Apps => "apps".into(),
        }
    }
}

/// The duration of a phase of the last start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Timing {
    pub phase: Phase,
    pub ms: u64,
}

/// The durations of the phases of one start, in the order they ended.
/// Phases that run at the same time each have their own duration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Timings {
    pub phases: Vec<Timing>,
    pub total_ms: u64,
}

impl Timings {
    pub(crate) fn add(&mut self, phase: Phase, took: Duration) {
        self.phases.push(Timing { phase, ms: ms(took) });
    }

    pub(crate) fn finish(&mut self, started: Instant) {
        self.total_ms = ms(started.elapsed());
    }

    /// The log line, such as `start a1: vault 12ms, profile claude 790ms, ..., total 3700ms`.
    pub(crate) fn line(&self, id: &str) -> String {
        let phases: Vec<String> = self.phases.iter().map(|t| format!("{} {}ms", t.phase.label(), t.ms)).collect();
        format!("start {id}: {}, total {}ms", phases.join(", "), self.total_ms)
    }
}

pub(crate) fn ms(took: Duration) -> u64 {
    u64::try_from(took.as_millis()).unwrap_or(u64::MAX)
}

/// Marks the phase that a start begins: in the shield's status and through the sink.
pub(crate) struct Progress<V> {
    pub(crate) id: String,
    pub(crate) started: Instant,
    pub(crate) state: Arc<Mutex<State<V>>>,
    pub(crate) sink: Arc<OnceLock<ProgressSink>>,
}

impl<V> Progress<V> {
    pub(crate) fn begin(&self, phase: Phase) {
        // Only a start that still runs: a failed start already set its status.
        match lock(&self.state).vms.get_mut(&self.id) {
            Some(Vm::Starting(current)) => *current = phase,
            _ => return,
        }
        if let Some(sink) = self.sink.get() {
            sink(&self.id, &phase, ms(self.started.elapsed()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_line_names_each_phase() {
        let mut timings = Timings::default();
        timings.add(Phase::Vault, Duration::from_millis(12));
        timings.add(Phase::Profile(AgentKind::Claude), Duration::from_millis(790));
        timings.total_ms = 802;
        assert_eq!(timings.line("a1"), "start a1: vault 12ms, profile claude 790ms, total 802ms");
        assert_eq!(
            serde_json::to_value(Phase::Profile(AgentKind::Claude)).unwrap(),
            serde_json::json!({ "step": "profile", "agent": "claude" })
        );
        assert_eq!(serde_json::to_value(Phase::Boot).unwrap(), serde_json::json!({ "step": "boot" }));
    }
}
