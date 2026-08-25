//! Panic containment shared by SDK-owned callback boundaries.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) type PanicPayload = Box<dyn Any + Send>;

/// Sticky state shared by every SDK call through one installed callback.
#[derive(Default)]
pub(crate) struct CallbackPanicFuse {
    disabled: AtomicBool,
}

impl CallbackPanicFuse {
    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    /// Disables future calls and returns whether this call tripped the fuse.
    pub(crate) fn trip(&self) -> bool {
        !self.disabled.swap(true, Ordering::AcqRel)
    }
}

pub(crate) fn dispose_panic_payload(payload: PanicPayload) {
    // A user-defined panic payload may panic again from Drop. Forget the
    // replacement payload because dropping it could repeat the same cycle.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(payload);
    }
}

pub(crate) fn report_without_unwind(report: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(report)) {
        dispose_panic_payload(payload);
    }
}
