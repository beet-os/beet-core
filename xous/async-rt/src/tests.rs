//! Unit tests for xous-async-rt.
//!
//! These tests exercise the waker, combinators, and timer logic using
//! synthetic futures that don't require a running Xous kernel.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use crate::waker::TaskWaker;
use crate::{join, join3, join_all, select, Either};

// ---------------------------------------------------------------------------
// Helpers: synthetic futures
// ---------------------------------------------------------------------------

/// A future that resolves to `value` after being polled `polls_needed` times.
struct CountdownFuture {
    remaining: usize,
    value: usize,
}

impl CountdownFuture {
    fn new(polls_needed: usize, value: usize) -> Self {
        Self {
            remaining: polls_needed,
            value,
        }
    }
}

impl Future for CountdownFuture {
    type Output = usize;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
        let this = self.get_mut();
        if this.remaining == 0 {
            Poll::Ready(this.value)
        } else {
            this.remaining -= 1;
            // Wake ourselves so the executor re-polls us.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// A future that never resolves.
struct NeverFuture;

impl Future for NeverFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}

/// Helper: poll a future once with a noop-like waker (uses our TaskWaker).
fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let tw = TaskWaker::new();
    let waker = tw.waker();
    let mut cx = Context::from_waker(&waker);
    Pin::new(f).poll(&mut cx)
}

// ---------------------------------------------------------------------------
// Waker tests
// ---------------------------------------------------------------------------

#[test]
fn waker_starts_ready() {
    let tw = TaskWaker::new();
    assert!(tw.is_ready());
}

#[test]
fn waker_clear_and_wake() {
    let tw = TaskWaker::new();
    tw.clear();
    assert!(!tw.is_ready());

    let waker = tw.waker();
    waker.wake_by_ref();
    assert!(tw.is_ready());
}

#[test]
fn waker_wake_by_value_sets_flag() {
    let tw = TaskWaker::new();
    tw.clear();

    let waker = tw.waker();
    waker.wake(); // consumes waker
    assert!(tw.is_ready());
}

#[test]
fn waker_clone_is_independent() {
    let tw = TaskWaker::new();
    tw.clear();

    let w1 = tw.waker();
    let w2 = w1.clone();
    drop(w1);
    // w2 should still work after w1 is dropped.
    w2.wake();
    assert!(tw.is_ready());
}

#[test]
fn waker_multiple_clones_all_work() {
    let tw = TaskWaker::new();
    tw.clear();

    let w1 = tw.waker();
    let w2 = w1.clone();
    let w3 = w2.clone();

    drop(w1);
    drop(w2);
    w3.wake();
    assert!(tw.is_ready());
}

// ---------------------------------------------------------------------------
// Combinator tests: join
// ---------------------------------------------------------------------------

#[test]
fn join_two_ready_futures() {
    let mut f = join(
        core::future::ready(1u32),
        core::future::ready(2u32),
    );
    match poll_once(&mut f) {
        Poll::Ready((a, b)) => {
            assert_eq!(a, 1);
            assert_eq!(b, 2);
        }
        Poll::Pending => panic!("join of two ready futures should be ready"),
    }
}

#[test]
fn join_waits_for_both() {
    // First future ready immediately, second needs 2 polls.
    let mut f = join(
        CountdownFuture::new(0, 10),
        CountdownFuture::new(2, 20),
    );

    // First poll: a is done, b needs 2 more.
    assert!(matches!(poll_once(&mut f), Poll::Pending));
    // Second poll: b needs 1 more.
    assert!(matches!(poll_once(&mut f), Poll::Pending));
    // Third poll: both done.
    match poll_once(&mut f) {
        Poll::Ready((a, b)) => {
            assert_eq!(a, 10);
            assert_eq!(b, 20);
        }
        Poll::Pending => panic!("should be ready after 3 polls"),
    }
}

#[test]
fn join3_all_ready() {
    let mut f = join3(
        core::future::ready(1),
        core::future::ready(2),
        core::future::ready(3),
    );
    match poll_once(&mut f) {
        Poll::Ready((a, b, c)) => {
            assert_eq!((a, b, c), (1, 2, 3));
        }
        Poll::Pending => panic!("should be ready"),
    }
}

#[test]
fn join3_mixed_readiness() {
    let mut f = join3(
        CountdownFuture::new(1, 10),
        core::future::ready(20usize),
        CountdownFuture::new(3, 30),
    );

    // Need 4 polls total (max of 1+1, 0+1, 3+1).
    assert!(matches!(poll_once(&mut f), Poll::Pending));
    assert!(matches!(poll_once(&mut f), Poll::Pending));
    assert!(matches!(poll_once(&mut f), Poll::Pending));
    match poll_once(&mut f) {
        Poll::Ready((a, b, c)) => assert_eq!((a, b, c), (10, 20, 30)),
        Poll::Pending => panic!("should be ready after 4 polls"),
    }
}

// ---------------------------------------------------------------------------
// Combinator tests: join_all
// ---------------------------------------------------------------------------

#[test]
fn join_all_empty() {
    let futures: Vec<Pin<Box<dyn Future<Output = u32>>>> = vec![];
    let mut f = join_all(futures);
    match poll_once(&mut f) {
        Poll::Ready(results) => assert!(results.is_empty()),
        Poll::Pending => panic!("empty join_all should be immediately ready"),
    }
}

#[test]
fn join_all_multiple() {
    let futures: Vec<Pin<Box<dyn Future<Output = usize>>>> = vec![
        Box::pin(core::future::ready(1)),
        Box::pin(core::future::ready(2)),
        Box::pin(core::future::ready(3)),
    ];
    let mut f = join_all(futures);
    match poll_once(&mut f) {
        Poll::Ready(results) => assert_eq!(results, vec![1, 2, 3]),
        Poll::Pending => panic!("all-ready join_all should resolve in one poll"),
    }
}

#[test]
fn join_all_mixed_readiness() {
    let futures: Vec<Pin<Box<dyn Future<Output = usize>>>> = vec![
        Box::pin(CountdownFuture::new(0, 1)),
        Box::pin(CountdownFuture::new(2, 2)),
        Box::pin(CountdownFuture::new(1, 3)),
    ];
    let mut f = join_all(futures);

    assert!(matches!(poll_once(&mut f), Poll::Pending));
    assert!(matches!(poll_once(&mut f), Poll::Pending));
    match poll_once(&mut f) {
        Poll::Ready(results) => assert_eq!(results, vec![1, 2, 3]),
        Poll::Pending => panic!("should be ready"),
    }
}

// ---------------------------------------------------------------------------
// Combinator tests: select
// ---------------------------------------------------------------------------

#[test]
fn select_left_wins() {
    let mut f = select(core::future::ready(42u32), NeverFuture);
    match poll_once(&mut f) {
        Poll::Ready(Either::Left(v)) => assert_eq!(v, 42),
        _ => panic!("left should win"),
    }
}

#[test]
fn select_right_wins() {
    let mut f = select(NeverFuture, core::future::ready(99u32));
    match poll_once(&mut f) {
        Poll::Ready(Either::Right(v)) => assert_eq!(v, 99),
        _ => panic!("right should win"),
    }
}

#[test]
fn select_both_ready_left_wins() {
    // When both are ready, left is checked first.
    let mut f = select(core::future::ready(1u32), core::future::ready(2u32));
    match poll_once(&mut f) {
        Poll::Ready(Either::Left(v)) => assert_eq!(v, 1),
        _ => panic!("left should win when both ready"),
    }
}

#[test]
fn select_neither_ready() {
    let mut f = select(NeverFuture, NeverFuture);
    assert!(matches!(poll_once(&mut f), Poll::Pending));
}

// ---------------------------------------------------------------------------
// Executor tests (using synthetic futures, no Xous kernel needed)
// ---------------------------------------------------------------------------

#[test]
fn executor_runs_single_task() {
    crate::reactor::init();

    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let mut exec = crate::Executor::new();
    exec.spawn(async move {
        c.store(42, Ordering::Release);
    });
    exec.run();

    assert_eq!(counter.load(Ordering::Acquire), 42);
}

#[test]
fn executor_runs_multiple_tasks() {
    // reactor::init is called by exec.run(), but run() also calls shutdown.
    // We need to test that multiple tasks all complete.
    let counter = Arc::new(AtomicUsize::new(0));

    let mut exec = crate::Executor::new();
    for _ in 0..5 {
        let c = counter.clone();
        exec.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
        });
    }
    exec.run();

    assert_eq!(counter.load(Ordering::Relaxed), 5);
}

#[test]
fn executor_spawner_works() {
    let counter = Arc::new(AtomicUsize::new(0));

    let mut exec = crate::Executor::new();
    let spawner = exec.spawner();

    let c = counter.clone();
    exec.spawn(async move {
        // Spawn a sibling task at runtime.
        let c2 = c.clone();
        spawner.spawn(async move {
            c2.store(99, Ordering::Release);
        });
        c.fetch_add(1, Ordering::Relaxed);
    });
    exec.run();

    // Both tasks ran: original incremented by 1, spawned set to 99.
    // Since they run sequentially (single-threaded), 99 is the final value
    // if the spawned task ran last, or 1 if it ran first.
    // Actually fetch_add(1) on 0 = 1, then store(99) = 99.
    assert_eq!(counter.load(Ordering::Acquire), 99);
}

#[test]
fn executor_empty_runs_immediately() {
    let mut exec = crate::Executor::new();
    exec.run(); // should not hang
}

// ---------------------------------------------------------------------------
// Timer tests (require reactor to be initialised)
// ---------------------------------------------------------------------------

#[test]
fn timer_zero_delay_resolves_after_two_polls() {
    crate::reactor::init();

    // Timer::after(0) registers with deadline = current_tick + 0.
    // The reactor advances tick by 1 each poll_all(), so after one
    // poll_all() the timer should be expired.
    let mut timer = crate::Timer::after(0);

    let tw = TaskWaker::new();
    let waker = tw.waker();
    let mut cx = Context::from_waker(&waker);

    // First poll: registers the timer, checks expired — tick is 0,
    // deadline is 0, so 0 >= 0 → should be ready immediately on first poll.
    match Pin::new(&mut timer).poll(&mut cx) {
        Poll::Ready(()) => {} // expected
        Poll::Pending => {
            // If not ready, do a reactor tick and try again.
            crate::reactor::poll_all();
            match Pin::new(&mut timer).poll(&mut cx) {
                Poll::Ready(()) => {}
                Poll::Pending => panic!("timer(0) should resolve after one reactor tick"),
            }
        }
    }

    crate::reactor::shutdown();
}

#[test]
fn timer_nonzero_delay() {
    crate::reactor::init();

    let mut timer = crate::Timer::after(3);
    let tw = TaskWaker::new();
    let waker = tw.waker();
    let mut cx = Context::from_waker(&waker);

    // First poll: registers timer (deadline = 0 + 3 = 3).
    assert!(matches!(Pin::new(&mut timer).poll(&mut cx), Poll::Pending));

    // Tick 1, 2: still pending.
    crate::reactor::poll_all(); // tick → 1
    assert!(matches!(Pin::new(&mut timer).poll(&mut cx), Poll::Pending));
    crate::reactor::poll_all(); // tick → 2
    assert!(matches!(Pin::new(&mut timer).poll(&mut cx), Poll::Pending));

    // Tick 3: expired!
    crate::reactor::poll_all(); // tick → 3
    match Pin::new(&mut timer).poll(&mut cx) {
        Poll::Ready(()) => {}
        Poll::Pending => panic!("timer(3) should be ready after 3 reactor ticks"),
    }

    crate::reactor::shutdown();
}
