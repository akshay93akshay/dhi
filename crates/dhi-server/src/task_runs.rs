//! In-process registry of the task loops this server is running.
//!
//! The registry is the single source of truth for "a worker exists for this
//! task". Stop requests flip the loop's cooperative flag; the hard-kill
//! monitor uses the abort handle after a grace period. A graceful loop removes
//! its own entry on exit, so the registry never keeps a stale worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dhi_core::TaskId;
use dhi_exec::TaskContainers;
use dhi_pty::PtyTools;
use tokio::task::AbortHandle;

struct Entry {
    stop: Arc<AtomicBool>,
    abort: AbortHandle,
}

/// Tracks the live task loops of one server process.
pub struct TaskRunRegistry {
    inner: Mutex<HashMap<TaskId, Entry>>,
    enabled: bool,
    pty: Option<Arc<PtyTools>>,
    containers: Option<Arc<TaskContainers>>,
}

impl Default for TaskRunRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRunRegistry {
    /// A live registry without cleanup handles. The hard-kill monitor still
    /// aborts the loop and marks the task cancelled.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            enabled: true,
            pty: None,
            containers: None,
        }
    }

    /// A live registry that can also close a task's terminals and remove its
    /// container on a hard kill. `main.rs` uses this constructor.
    pub fn with_cleanup(pty: Arc<PtyTools>, containers: Arc<TaskContainers>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            enabled: true,
            pty: Some(pty),
            containers: Some(containers),
        }
    }

    /// A no-op registry for test states that do not exercise cancellation.
    /// Registration, lookup, stop, and abort all return `false`.
    pub fn disabled() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            enabled: false,
            pty: None,
            containers: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, Entry>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds or replaces the entry for a running task. Returns whether the
    /// registry is enabled and accepted the entry.
    pub fn register(
        &self,
        task_id: TaskId,
        stop_flag: Arc<AtomicBool>,
        abort_handle: AbortHandle,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        self.lock().insert(
            task_id,
            Entry {
                stop: stop_flag,
                abort: abort_handle,
            },
        );
        true
    }

    /// Removes an entry, normally called by the loop itself on exit.
    pub fn remove(&self, task_id: TaskId) -> bool {
        self.lock().remove(&task_id).is_some()
    }

    /// Flips the cooperative stop flag. Returns `false` when there is no
    /// registered loop (an orphan left by a restart or a finished loop).
    pub fn request_stop(&self, task_id: TaskId) -> bool {
        let map = self.lock();
        let Some(entry) = map.get(&task_id) else {
            return false;
        };
        entry.stop.store(true, Ordering::SeqCst);
        true
    }

    /// Hard-kills the registered loop without removing its entry.
    pub fn abort(&self, task_id: TaskId) -> bool {
        let map = self.lock();
        let Some(entry) = map.get(&task_id) else {
            return false;
        };
        entry.abort.abort();
        true
    }

    /// Atomically removes and aborts the loop. This is the hard-kill path:
    /// exactly one monitor wins, and a graceful exit that already removed the
    /// entry makes the monitor a no-op.
    pub fn abort_and_remove(&self, task_id: TaskId) -> bool {
        let Some(entry) = self.lock().remove(&task_id) else {
            return false;
        };
        entry.abort.abort();
        true
    }

    pub fn contains(&self, task_id: TaskId) -> bool {
        if !self.enabled {
            return false;
        }
        self.lock().contains_key(&task_id)
    }

    /// The cleanup owners wired by `main.rs`, if any.
    pub fn cleanup_handles(&self) -> (Option<Arc<PtyTools>>, Option<Arc<TaskContainers>>) {
        (self.pty.clone(), self.containers.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stop_flag_and_abort_are_tracked_by_task() {
        let registry = TaskRunRegistry::new();
        let task_id = uuid::Uuid::new_v4();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(std::future::pending::<()>());

        assert!(registry.register(task_id, stop.clone(), handle.abort_handle()));
        assert!(registry.contains(task_id));
        assert!(registry.request_stop(task_id));
        assert!(stop.load(Ordering::SeqCst));
        assert!(registry.abort(task_id));
        assert!(registry.remove(task_id));
        assert!(!registry.contains(task_id));
        assert!(!registry.request_stop(task_id));
        assert!(!registry.abort(task_id));
    }

    #[tokio::test]
    async fn abort_and_remove_is_a_single_winner() {
        let registry = TaskRunRegistry::new();
        let task_id = uuid::Uuid::new_v4();
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        registry.register(
            task_id,
            Arc::new(AtomicBool::new(false)),
            handle.abort_handle(),
        );

        assert!(registry.abort_and_remove(task_id));
        assert!(!registry.contains(task_id));
        assert!(!registry.abort_and_remove(task_id));
        assert_eq!(registry.len(), 0);
    }

    #[tokio::test]
    async fn disabled_registry_is_a_no_op() {
        let registry = TaskRunRegistry::disabled();
        let task_id = uuid::Uuid::new_v4();
        let handle = tokio::spawn(std::future::pending::<()>());

        assert!(!registry.register(
            task_id,
            Arc::new(AtomicBool::new(false)),
            handle.abort_handle()
        ));
        assert!(!registry.contains(task_id));
        assert!(!registry.request_stop(task_id));
        assert!(!registry.abort(task_id));
        assert!(!registry.remove(task_id));
    }
}
