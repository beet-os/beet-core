//! Global reactor — centralised SID polling and message dispatch.
//!
//! The reactor owns the "I/O side" of the async runtime:
//!
//! * Services register their SID via [`register`]; messages are buffered
//!   and the registered [`Waker`] is called so the executor re-polls the
//!   waiting task.
//! * Timers register a deadline (in reactor ticks); the reactor wakes
//!   them when the tick count passes the deadline.
//! * The spawn queue lets tasks submit new futures at runtime via
//!   [`Spawner`](crate::Spawner).
//!
//! # Thread safety
//!
//! Xous processes are single-threaded.  The reactor lives in a global
//! `RefCell` and is only ever accessed from the executor's `run()` loop
//! and from futures polled **by that same loop**.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::Waker;

use xous::{MessageEnvelope, SID};

// ---------------------------------------------------------------------------
// Slot ID — handle returned to AsyncServer / Timer
// ---------------------------------------------------------------------------

pub(crate) type SlotId = usize;

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

struct ServerSlot {
    sid: SID,
    queue: VecDeque<MessageEnvelope>,
    waker: Option<Waker>,
    active: bool,
}

struct TimerSlot {
    deadline_tick: u64,
    waker: Option<Waker>,
    active: bool,
}

struct ReactorInner {
    servers: Vec<ServerSlot>,
    timers: Vec<TimerSlot>,
    tick: u64,
    spawn_queue: Vec<Pin<Box<dyn Future<Output = ()>>>>,
}

impl ReactorInner {
    fn new() -> Self {
        Self {
            servers: Vec::new(),
            timers: Vec::new(),
            tick: 0,
            spawn_queue: Vec::new(),
        }
    }

    // -- Server slots -------------------------------------------------------

    fn register_server(&mut self, sid: SID) -> SlotId {
        // Reuse an inactive slot if available.
        for (i, slot) in self.servers.iter_mut().enumerate() {
            if !slot.active {
                slot.sid = sid;
                slot.queue.clear();
                slot.waker = None;
                slot.active = true;
                return i;
            }
        }
        let id = self.servers.len();
        self.servers.push(ServerSlot {
            sid,
            queue: VecDeque::new(),
            waker: None,
            active: true,
        });
        id
    }

    fn unregister_server(&mut self, id: SlotId) {
        if let Some(slot) = self.servers.get_mut(id) {
            slot.active = false;
            slot.queue.clear();
            slot.waker = None;
        }
    }

    /// Fast-path receive: check buffer, then try one non-blocking receive.
    fn try_recv(&mut self, id: SlotId) -> Option<MessageEnvelope> {
        let slot = self.servers.get_mut(id)?;
        if !slot.active {
            return None;
        }
        // Buffered message first (from a prior poll_servers round).
        if let Some(msg) = slot.queue.pop_front() {
            return Some(msg);
        }
        // Direct non-blocking receive — fast path avoids waiting for
        // the next reactor tick.
        match xous::try_receive_message(slot.sid) {
            Ok(Some(msg)) => Some(msg),
            _ => None,
        }
    }

    /// Check if a server slot is still active (server hasn't been destroyed).
    fn server_active(&self, id: SlotId) -> bool {
        self.servers.get(id).map_or(false, |s| s.active)
    }

    fn set_server_waker(&mut self, id: SlotId, waker: Waker) {
        if let Some(slot) = self.servers.get_mut(id) {
            slot.waker = Some(waker);
        }
    }

    /// Poll all registered SIDs.  Buffer incoming messages and wake tasks.
    fn poll_servers(&mut self) -> bool {
        let mut woke_any = false;
        for slot in &mut self.servers {
            if !slot.active {
                continue;
            }
            // Drain all available messages from this SID.
            loop {
                match xous::try_receive_message(slot.sid) {
                    Ok(Some(msg)) => {
                        slot.queue.push_back(msg);
                        if let Some(waker) = slot.waker.take() {
                            waker.wake();
                            woke_any = true;
                        }
                    }
                    _ => break,
                }
            }
        }
        woke_any
    }

    // -- Timer slots --------------------------------------------------------

    fn register_timer(&mut self, delay_ticks: u64) -> SlotId {
        let deadline = self.tick + delay_ticks;
        for (i, slot) in self.timers.iter_mut().enumerate() {
            if !slot.active {
                slot.deadline_tick = deadline;
                slot.waker = None;
                slot.active = true;
                return i;
            }
        }
        let id = self.timers.len();
        self.timers.push(TimerSlot {
            deadline_tick: deadline,
            waker: None,
            active: true,
        });
        id
    }

    fn unregister_timer(&mut self, id: SlotId) {
        if let Some(slot) = self.timers.get_mut(id) {
            slot.active = false;
            slot.waker = None;
        }
    }

    fn timer_expired(&self, id: SlotId) -> bool {
        self.timers
            .get(id)
            .map_or(false, |s| s.active && self.tick >= s.deadline_tick)
    }

    fn set_timer_waker(&mut self, id: SlotId, waker: Waker) {
        if let Some(slot) = self.timers.get_mut(id) {
            slot.waker = Some(waker);
        }
    }

    /// Advance tick, wake expired timers.
    fn poll_timers(&mut self) -> bool {
        self.tick += 1;
        let mut woke_any = false;
        for slot in &mut self.timers {
            if slot.active && self.tick >= slot.deadline_tick {
                if let Some(waker) = slot.waker.take() {
                    waker.wake();
                    woke_any = true;
                }
            }
        }
        woke_any
    }

    // -- Spawn queue --------------------------------------------------------

    fn enqueue_spawn(&mut self, future: Pin<Box<dyn Future<Output = ()>>>) {
        self.spawn_queue.push(future);
    }

    fn drain_spawns(&mut self) -> Vec<Pin<Box<dyn Future<Output = ()>>>> {
        core::mem::take(&mut self.spawn_queue)
    }
}

// ---------------------------------------------------------------------------
// Global access — single-threaded, RefCell-guarded
// ---------------------------------------------------------------------------
//
// On AArch64 hardware (cfg(beetos)): Xous processes are single-threaded.
// A global static + RefCell is correct and safe.
//
// In hosted mode / tests (cfg(not(beetos))): cargo test spawns multiple OS
// threads.  thread_local! gives each thread its own reactor instance so that
// tests running concurrently never share state and never trigger a double-borrow
// panic from RefCell.

#[cfg(beetos)]
struct ReactorCell(RefCell<Option<ReactorInner>>);
// SAFETY: single-threaded on AArch64 hardware.
#[cfg(beetos)]
unsafe impl Sync for ReactorCell {}
#[cfg(beetos)]
static REACTOR: ReactorCell = ReactorCell(RefCell::new(None));

#[cfg(not(beetos))]
std::thread_local! {
    static REACTOR: RefCell<Option<ReactorInner>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut ReactorInner) -> R) -> R {
    #[cfg(beetos)]
    {
        let mut borrow = REACTOR.0.borrow_mut();
        f(borrow.as_mut().expect("xous-async-rt: reactor not initialised (call Executor::run)"))
    }
    #[cfg(not(beetos))]
    {
        REACTOR.with(|cell| {
            let mut borrow = cell.borrow_mut();
            f(borrow.as_mut().expect("xous-async-rt: reactor not initialised (call Executor::run)"))
        })
    }
}

// -- Public (crate) API -----------------------------------------------------

pub(crate) fn init() {
    #[cfg(beetos)]
    { *REACTOR.0.borrow_mut() = Some(ReactorInner::new()); }
    #[cfg(not(beetos))]
    { REACTOR.with(|cell| *cell.borrow_mut() = Some(ReactorInner::new())); }
}

pub(crate) fn shutdown() {
    #[cfg(beetos)]
    { *REACTOR.0.borrow_mut() = None; }
    #[cfg(not(beetos))]
    { REACTOR.with(|cell| *cell.borrow_mut() = None); }
}

// Servers
pub(crate) fn register_server(sid: SID) -> SlotId {
    with(|r| r.register_server(sid))
}
pub(crate) fn unregister_server(id: SlotId) {
    with(|r| r.unregister_server(id))
}
pub(crate) fn try_recv(id: SlotId) -> Option<MessageEnvelope> {
    with(|r| r.try_recv(id))
}
pub(crate) fn server_active(id: SlotId) -> bool {
    with(|r| r.server_active(id))
}
pub(crate) fn set_server_waker(id: SlotId, waker: Waker) {
    with(|r| r.set_server_waker(id, waker))
}

// Timers
pub(crate) fn register_timer(delay_ticks: u64) -> SlotId {
    with(|r| r.register_timer(delay_ticks))
}
pub(crate) fn unregister_timer(id: SlotId) {
    with(|r| r.unregister_timer(id))
}
pub(crate) fn timer_expired(id: SlotId) -> bool {
    with(|r| r.timer_expired(id))
}
pub(crate) fn set_timer_waker(id: SlotId, waker: Waker) {
    with(|r| r.set_timer_waker(id, waker))
}

// Spawn queue
pub(crate) fn enqueue_spawn(future: Pin<Box<dyn Future<Output = ()>>>) {
    with(|r| r.enqueue_spawn(future))
}
pub(crate) fn drain_spawns() -> Vec<Pin<Box<dyn Future<Output = ()>>>> {
    with(|r| r.drain_spawns())
}

// Tick
pub(crate) fn poll_all() -> bool {
    with(|r| {
        let s = r.poll_servers();
        let t = r.poll_timers();
        s || t
    })
}
