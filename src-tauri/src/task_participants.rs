//! The two participants of a task run. The provider kind selects the CLI,
//! its mapper and its mounts; the participant id selects the slot in
//! prompts, files, messages, picks and the manifest.
//!
//! The independent reviewer is no participant: it has the fixed id
//! [`INDEPENDENT`], which is no provider kind and no slot id, so it never
//! shares a file or a pick with a participant, also with their kind and model.

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::session::TaskAgent;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    /// File-safe and unique in the run. It never contains model text.
    pub id: String,
    pub kind: AgentKind,
    /// `None` is the default model of the CLI.
    pub model: Option<String>,
}

/// The id of the independent reviewer of a run.
pub const INDEPENDENT: &str = "independent";

/// The independent reviewer of a run, with its fixed id.
pub fn independent(agent: &TaskAgent) -> Participant {
    Participant { id: INDEPENDENT.to_owned(), kind: agent.kind, model: agent.model.clone() }
}

/// The participants of the slots, in order. Two kinds keep the kind names
/// of older runs; one kind gets `{kind}-1` and `{kind}-2`.
pub fn participants(agents: &[TaskAgent; 2]) -> [Participant; 2] {
    let same = agents[0].kind == agents[1].kind;
    let mut slot = 0;
    agents.clone().map(|agent| {
        slot += 1;
        let name = agent.kind.name();
        Participant {
            id: if same { format!("{name}-{slot}") } else { name.to_owned() },
            kind: agent.kind,
            model: agent.model,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: AgentKind, model: Option<&str>) -> TaskAgent {
        TaskAgent { kind, model: model.map(str::to_owned) }
    }

    #[test]
    fn mixed_kinds_keep_their_names_and_one_kind_gets_slot_ids() {
        let ids = |agents| participants(&agents).map(|p| p.id);
        assert_eq!(ids([agent(AgentKind::Claude, None), agent(AgentKind::Codex, None)]), ["claude", "codex"]);
        assert_eq!(ids([agent(AgentKind::Codex, None), agent(AgentKind::Claude, None)]), ["codex", "claude"]);
        assert_eq!(
            ids([agent(AgentKind::Pi, Some("provider/model.b")), agent(AgentKind::Pi, Some("provider/model.a"))]),
            ["pi-1", "pi-2"]
        );
        assert_eq!(ids([agent(AgentKind::Claude, None), agent(AgentKind::Claude, None)]), ["claude-1", "claude-2"]);
        let pair = participants(&[agent(AgentKind::Codex, Some("a")), agent(AgentKind::Codex, None)]);
        assert_eq!(pair[0].model.as_deref(), Some("a"));
        assert_eq!(pair[1].kind, AgentKind::Codex);
    }

    #[test]
    fn the_independent_reviewer_never_has_a_participant_id() {
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Pi, AgentKind::Gemini] {
            let reviewer = independent(&agent(kind, Some("m")));
            assert_eq!(
                (reviewer.id.as_str(), reviewer.kind, reviewer.model.as_deref()),
                (INDEPENDENT, kind, Some("m"))
            );
            for pair in [[kind, kind], [kind, AgentKind::Claude], [AgentKind::Codex, kind]] {
                let ids = participants(&pair.map(|k| agent(k, Some("m")))).map(|p| p.id);
                assert!(!ids.contains(&reviewer.id), "{ids:?}");
            }
        }
    }
}
