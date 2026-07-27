//! Per-resource operation locks.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use solti_model::TaskId;

/// Weak keyed async locks for one class of per-resource operations.
///
/// Desired-state and runtime operations use distinct instances.
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
