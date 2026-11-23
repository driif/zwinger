//! The pending asks of one VM. The proxy holds a request that
//! the Ask default decides until the user answers, the deadline passes,
//! the guest goes away, the proxy stops or the policy decides it. Each of
//! these exits settles the ask once: the drop of its [`Ticket`] removes
//! it, which frees its VM slot and its app permit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::error::{Error, Result};

/// How long a request waits for an answer. Then it is blocked.
pub const DEADLINE: Duration = Duration::from_secs(30);
/// The most pending asks of one VM.
pub const VM_PENDING: usize = 32;
/// The most pending asks of the app, over all VMs.
pub const APP_PENDING: usize = 128;

/// A request that waits for the user. It has no header, path or body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ask {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub method: String,
    /// The deadline, in ms since the epoch.
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Block,
}

struct Pending {
    ask: Ask,
    answer: oneshot::Sender<Answer>,
    _permit: OwnedSemaphorePermit,
}

/// The pending asks of one VM, by id.
#[derive(Default)]
pub struct Registry {
    pending: Mutex<HashMap<String, Pending>>,
}

/// An open ask. Its drop settles it: a later answer finds no ask.
pub struct Ticket {
    registry: Arc<Registry>,
    id: String,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.id);
        (self.notify)();
    }
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens an ask. `None` when the VM or the app has no free slot: the
    /// caller blocks the request.
    pub fn open(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        method: &str,
        app: &Arc<Semaphore>,
        expires_at: u64,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<(Ticket, oneshot::Receiver<Answer>)> {
        let (answer, receiver) = oneshot::channel();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut pending = self.lock();
            if pending.len() >= VM_PENDING {
                return None;
            }
            let permit = app.clone().try_acquire_owned().ok()?;
            let ask = Ask { id: id.clone(), host: host.to_owned(), port, method: method.to_owned(), expires_at };
            pending.insert(id.clone(), Pending { ask, answer, _permit: permit });
        }
        notify();
        Some((Ticket { registry: self.clone(), id, notify }, receiver))
    }

    /// Sends the answer and settles the ask. An unknown id fails: the ask
    /// timed out, or its request or VM ended.
    pub fn answer(&self, id: &str, answer: Answer) -> Result<()> {
        let pending = self.lock().remove(id).ok_or(Error::AskNotFound)?;
        // The request may have ended at this moment; the ask is settled either way.
        let _ = pending.answer.send(answer);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Ask> {
        self.lock().get(id).map(|p| p.ask.clone()).ok_or(Error::AskNotFound)
    }

    /// The pending asks, the oldest first.
    pub fn list(&self) -> Vec<Ask> {
        let mut asks: Vec<Ask> = self.lock().values().map(|p| p.ask.clone()).collect();
        asks.sort_by(|a, b| a.expires_at.cmp(&b.expires_at).then_with(|| a.id.cmp(&b.id)));
        asks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    #[test]
    fn each_exit_frees_the_slot_and_the_permit_once() {
        let app = Arc::new(Semaphore::new(2));
        let registry = Arc::new(Registry::default());
        let (ticket, mut receiver) = registry.open("a.io", 443, "GET", &app, 1, quiet()).unwrap();
        assert_eq!(app.available_permits(), 1);
        let id = registry.list()[0].id.clone();
        // An answer settles it: the permit is free before the request takes the answer.
        registry.answer(&id, Answer::Allow).unwrap();
        assert_eq!(app.available_permits(), 2);
        assert_eq!(receiver.try_recv(), Ok(Answer::Allow));
        assert!(matches!(registry.answer(&id, Answer::Block), Err(Error::AskNotFound)));
        drop(ticket);
        assert_eq!(app.available_permits(), 2);

        // A drop (timeout, disconnect, stop) settles it too, and a late answer fails.
        let (ticket, _receiver) = registry.open("a.io", 443, "GET", &app, 1, quiet()).unwrap();
        let id = registry.list()[0].id.clone();
        drop(ticket);
        assert!(registry.list().is_empty());
        assert_eq!(app.available_permits(), 2);
        assert!(matches!(registry.answer(&id, Answer::Allow), Err(Error::AskNotFound)));
    }

    #[test]
    fn the_vm_and_the_app_limits_fail_closed() {
        let app = Arc::new(Semaphore::new(VM_PENDING + 1));
        let registry = Arc::new(Registry::default());
        let tickets: Vec<_> =
            (0..VM_PENDING).map(|_| registry.open("a.io", 443, "GET", &app, 1, quiet()).unwrap()).collect();
        assert!(registry.open("a.io", 443, "GET", &app, 1, quiet()).is_none());
        // Another VM gets the last app permit, then none.
        let other = Arc::new(Registry::default());
        let last = other.open("b.io", 443, "GET", &app, 1, quiet()).unwrap();
        assert!(other.open("b.io", 443, "GET", &app, 1, quiet()).is_none());
        drop(tickets);
        drop(last);
        assert_eq!(app.available_permits(), VM_PENDING + 1);
    }
}
