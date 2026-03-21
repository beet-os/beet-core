//! Async timer future backed by the reactor's tick counter.
//!
//! Each call to `yield_slice()` in the executor's idle phase counts as
//! one reactor tick (~1 ms on typical QEMU virt).  For wall-clock
//! precision, a production system should replace the tick source with
//! the ticktimer service.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::reactor;

/// A future that completes after approximately `ticks` reactor ticks.
///
/// ```rust,ignore
/// // Wait ~100 ticks (roughly 100 ms on QEMU virt)
/// Timer::after(100).await;
/// ```
pub struct Timer {
    slot: Option<reactor::SlotId>,
    delay: u64,
}

impl Timer {
    /// Create a timer that fires after `ticks` reactor ticks.
    pub fn after(ticks: u64) -> Self {
        Self {
            slot: None,
            delay: ticks,
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();

        // Lazily register with the reactor on first poll.
        let slot = match this.slot {
            Some(id) => id,
            None => {
                let id = reactor::register_timer(this.delay);
                this.slot = Some(id);
                id
            }
        };

        if reactor::timer_expired(slot) {
            // Clean up the slot.
            reactor::unregister_timer(slot);
            this.slot = None;
            Poll::Ready(())
        } else {
            reactor::set_timer_waker(slot, cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            reactor::unregister_timer(slot);
        }
    }
}
