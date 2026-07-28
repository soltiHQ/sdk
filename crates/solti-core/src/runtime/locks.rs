//! # Per-task locks
//!
//! [`TaskLocks`] serializes one operation class by task name.
//! Different names remain independent.
//! The map stores weak references.
//! Stale entries are pruned when a new lock is created.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use solti_model::TaskId;

/// Weak keyed locks for one operation class.
///
/// Desired-state operations use one instance.
/// Runtime operations use another instance.
#[derive(Clone, Default)]
pub(crate) struct TaskLocks {
    locks: Arc<parking_lot::Mutex<HashMap<TaskId, Weak<tokio::sync::Mutex<()>>>>>,
}

impl TaskLocks {
    pub(crate) async fn lock(&self, name: &TaskId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock();
            if let Some(lock) = locks.get(name).and_then(Weak::upgrade) {
                lock
            } else {
                locks.retain(|_, lock| lock.strong_count() > 0);
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(name.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}
