//! Single-threaded cooperative executor with waker-based scheduling.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::reactor;
use crate::waker::TaskWaker;

struct Task {
    future: Pin<Box<dyn Future<Output = ()>>>,
    waker: TaskWaker,
    completed: bool,
}

/// Single-threaded cooperative async executor for Xous services.
///
/// ```rust,ignore
/// let mut exec = Executor::new();
/// exec.spawn(async { /* ... */ });
/// exec.run();
/// ```
///
/// The executor initialises the global reactor on [`run`] and shuts it
/// down when all tasks complete.
pub struct Executor {
    tasks: Vec<Task>,
}

impl Executor {
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// Spawn an async task.  Must be called **before** [`run`].
    pub fn spawn(&mut self, future: impl Future<Output = ()> + 'static) {
        self.tasks.push(Task {
            future: Box::pin(future),
            waker: TaskWaker::new(), // starts ready
            completed: false,
        });
    }

    /// Drive all spawned tasks to completion.
    ///
    /// This is the main event loop:
    ///
    /// 1. **Poll phase** — poll every task whose waker has fired.
    /// 2. **Reactor phase** — `try_receive_message` on all registered
    ///    SIDs, advance timers, wake tasks whose I/O completed.
    /// 3. **Idle phase** — if neither phase made progress, yield the CPU
    ///    via `xous::yield_slice()` so other Xous processes can run.
    pub fn run(&mut self) {
        reactor::init();

        loop {
            let mut any_pending = false;
            let mut made_progress = false;

            // ── Phase 1: poll woken tasks ─────────────────────────────
            for task in self.tasks.iter_mut() {
                if task.completed {
                    continue;
                }
                any_pending = true;

                if !task.waker.is_ready() {
                    continue;
                }
                task.waker.clear();

                let waker = task.waker.waker();
                let mut cx = Context::from_waker(&waker);

                if let Poll::Ready(()) = task.future.as_mut().poll(&mut cx) {
                    task.completed = true;
                    made_progress = true;
                }
            }

            if !any_pending {
                break;
            }

            // ── Phase 2: reactor tick ─────────────────────────────────
            if reactor::poll_all() {
                made_progress = true;
            }

            // ── Phase 3: idle ─────────────────────────────────────────
            if !made_progress {
                xous::yield_slice();
            }
        }

        reactor::shutdown();
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}
