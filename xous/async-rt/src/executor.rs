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
/// Tasks can also be spawned at runtime from within other tasks using
/// [`Spawner`]:
///
/// ```rust,ignore
/// let spawner = exec.spawner();
/// exec.spawn(async move {
///     spawner.spawn(async { /* dynamically added task */ });
/// });
/// exec.run();
/// ```
pub struct Executor {
    tasks: Vec<Task>,
    /// Index where the next poll round starts (round-robin fairness).
    poll_start: usize,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            poll_start: 0,
        }
    }

    /// Spawn an async task before starting the executor.
    pub fn spawn(&mut self, future: impl Future<Output = ()> + 'static) {
        self.tasks.push(Task {
            future: Box::pin(future),
            waker: TaskWaker::new(), // starts ready
            completed: false,
        });
    }

    /// Get a [`Spawner`] handle for runtime task creation.
    ///
    /// The spawner is `Clone` and can be moved into async tasks.
    /// New tasks appear on the next executor loop iteration.
    pub fn spawner(&self) -> Spawner {
        Spawner { _private: () }
    }

    /// Drive all spawned tasks to completion.
    ///
    /// The event loop has four phases:
    ///
    /// 1. **Drain spawns** — pick up tasks submitted via [`Spawner`].
    /// 2. **Poll phase** — poll every woken task, round-robin starting
    ///    at a rotating index for fairness.
    /// 3. **Reactor phase** — `try_receive_message` on all registered
    ///    SIDs, advance timers, wake tasks whose I/O completed.
    /// 4. **Idle phase** — if no phase made progress, yield the CPU
    ///    via `xous::yield_slice()`.
    pub fn run(&mut self) {
        reactor::init();

        loop {
            // ── Phase 0: drain runtime spawns ─────────────────────────
            for future in reactor::drain_spawns() {
                self.tasks.push(Task {
                    future,
                    waker: TaskWaker::new(),
                    completed: false,
                });
            }

            let task_count = self.tasks.len();
            let mut any_pending = false;
            let mut made_progress = false;

            // ── Phase 1: poll woken tasks (round-robin) ───────────────
            for i in 0..task_count {
                let idx = (self.poll_start + i) % task_count;
                let task = &mut self.tasks[idx];

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

            // Rotate the start index for next iteration.
            if task_count > 0 {
                self.poll_start = (self.poll_start + 1) % task_count;
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

// ---------------------------------------------------------------------------
// Spawner — runtime task submission handle
// ---------------------------------------------------------------------------

/// Handle for spawning new tasks from within running async code.
///
/// Obtained via [`Executor::spawner`].  Submits futures to the reactor's
/// spawn queue; the executor picks them up on its next loop iteration.
///
/// ```rust,ignore
/// let spawner = exec.spawner();
/// exec.spawn(async move {
///     // spawn a sibling task at runtime
///     spawner.spawn(async {
///         Timer::after(100).await;
///         log::info!("dynamic task done");
///     });
/// });
/// ```
#[derive(Clone)]
pub struct Spawner {
    _private: (),
}

impl Spawner {
    /// Spawn a new task on the executor that owns this spawner.
    pub fn spawn(&self, future: impl Future<Output = ()> + 'static) {
        reactor::enqueue_spawn(Box::pin(future));
    }
}
