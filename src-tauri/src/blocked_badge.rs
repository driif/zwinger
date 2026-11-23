//! The dock badge: the count of agents that went to blocked while the app
//! was in the background. Focus on the window clears it.

use std::collections::HashSet;
use std::sync::Mutex;

use tauri::{AppHandle, Manager as _, UserAttentionType};

use crate::cage::manager::lock;
use crate::terminal::AgentState;

/// The process ids of the agents that went to blocked in the background.
#[derive(Default)]
pub struct BlockedBadge(Mutex<HashSet<String>>);

impl BlockedBadge {
    /// Keeps `process_id` while it is blocked in the background. Gives the
    /// new count if it changed, and true if the process was added.
    fn note(&self, process_id: &str, blocked_in_background: bool) -> Option<(usize, bool)> {
        let mut blocked = lock(&self.0);
        let changed =
            if blocked_in_background { blocked.insert(process_id.to_owned()) } else { blocked.remove(process_id) };
        changed.then_some((blocked.len(), blocked_in_background))
    }

    fn clear(&self) -> bool {
        let mut blocked = lock(&self.0);
        let had = !blocked.is_empty();
        blocked.clear();
        had
    }
}

/// Updates the badge for the new state of an agent. A new blocked agent
/// also bounces the dock icon once.
pub fn agent_state(app: &AppHandle, process_id: &str, state: Option<AgentState>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let background = !window.is_focused().unwrap_or(true);
    let blocked = state == Some(AgentState::Blocked) && background;
    let Some((count, added)) = app.try_state::<BlockedBadge>().and_then(|badge| badge.note(process_id, blocked)) else {
        return;
    };
    let _ = window.set_badge_count((count > 0).then_some(count as i64));
    if added {
        let _ = window.request_user_attention(Some(UserAttentionType::Informational));
    }
}

/// The window has focus: the user sees the agents.
pub fn focused(app: &AppHandle) {
    if app.try_state::<BlockedBadge>().is_some_and(|badge| badge.clear())
        && let Some(window) = app.get_webview_window("main")
    {
        let _ = window.set_badge_count(None);
    }
}

#[cfg(test)]
mod tests {
    use super::BlockedBadge;

    #[test]
    fn the_count_has_each_agent_blocked_in_the_background_once() {
        let badge = BlockedBadge::default();
        assert_eq!(badge.note("a", true), Some((1, true)));
        assert_eq!(badge.note("a", true), None, "the same agent again");
        assert_eq!(badge.note("b", true), Some((2, true)));
        // Active again, or blocked while the window has focus.
        assert_eq!(badge.note("a", false), Some((1, false)));
        assert_eq!(badge.note("c", false), None);
        assert!(badge.clear());
        assert!(!badge.clear());
        assert_eq!(badge.note("b", false), None);
    }
}
