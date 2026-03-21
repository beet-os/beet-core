//! Waker implementation backed by `Arc<AtomicBool>`.
//!
//! Each spawned task gets a `TaskWaker` that shares an atomic flag with
//! the executor.  When a future calls `cx.waker().wake()`, the flag is
//! set and the executor knows to re-poll that task.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{RawWaker, RawWakerVTable, Waker};

/// Shared ready-flag between a task and its waker(s).
#[derive(Clone)]
pub(crate) struct TaskWaker {
    ready: Arc<AtomicBool>,
}

impl TaskWaker {
    /// Create a new waker.  Tasks start ready so they get an initial poll.
    pub fn new() -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Is this task marked ready to poll?
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Clear the ready flag (called by the executor before polling).
    pub fn clear(&self) {
        self.ready.store(false, Ordering::Release);
    }

    /// Build a `core::task::Waker` that sets this flag when woken.
    pub fn waker(&self) -> Waker {
        let arc = self.ready.clone();
        let ptr = Arc::into_raw(arc) as *const ();
        // SAFETY: The vtable functions correctly manage the Arc refcount.
        unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
    }
}

// ---------------------------------------------------------------------------
// RawWaker vtable — converts Arc<AtomicBool> ↔ *const () safely
// ---------------------------------------------------------------------------

const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

/// Clone: increment Arc refcount, return new RawWaker.
unsafe fn clone_fn(ptr: *const ()) -> RawWaker {
    let arc = Arc::from_raw(ptr as *const AtomicBool);
    let cloned = arc.clone();
    core::mem::forget(arc); // keep the original alive
    let new_ptr = Arc::into_raw(cloned) as *const ();
    RawWaker::new(new_ptr, &VTABLE)
}

/// Wake (by value): set flag and drop (decrement refcount).
unsafe fn wake_fn(ptr: *const ()) {
    let arc = Arc::from_raw(ptr as *const AtomicBool);
    arc.store(true, Ordering::Release);
    // arc is dropped here — refcount decremented
}

/// Wake by reference: set flag, don't consume.
unsafe fn wake_by_ref_fn(ptr: *const ()) {
    let arc = Arc::from_raw(ptr as *const AtomicBool);
    arc.store(true, Ordering::Release);
    core::mem::forget(arc); // don't drop — caller still owns it
}

/// Drop: decrement Arc refcount.
unsafe fn drop_fn(ptr: *const ()) {
    let _arc = Arc::from_raw(ptr as *const AtomicBool);
}
