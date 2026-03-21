//! Async combinators: `join` and `select`.
//!
//! Both work with `!Unpin` futures (i.e. any `async` block) — no boxing
//! or `pin!()` required at the call site.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

// ---------------------------------------------------------------------------
// join — drive two futures to completion, return both results
// ---------------------------------------------------------------------------

/// Drive two futures concurrently, returning both results.
///
/// ```rust,ignore
/// let (a, b) = join(server_a.next(), server_b.next()).await;
/// ```
pub fn join<FA, FB>(a: FA, b: FB) -> Join<FA, FB>
where
    FA: Future,
    FB: Future,
{
    Join {
        a: MaybeDone::Pending(a),
        b: MaybeDone::Pending(b),
    }
}

enum MaybeDone<F: Future> {
    Pending(F),
    Done(F::Output),
    Taken,
}

pub struct Join<FA: Future, FB: Future> {
    a: MaybeDone<FA>,
    b: MaybeDone<FB>,
}

impl<FA: Future, FB: Future> Future for Join<FA, FB> {
    type Output = (FA::Output, FB::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We never move the inner futures after this Pin is
        // created.  `MaybeDone::Pending(f)` is only accessed through
        // `Pin::new_unchecked`, and once a future completes it is
        // replaced with `Done(val)` — it is never moved while pinned.
        let this = unsafe { self.get_unchecked_mut() };

        if let MaybeDone::Pending(ref mut f) = this.a {
            if let Poll::Ready(val) = unsafe { Pin::new_unchecked(f) }.poll(cx) {
                this.a = MaybeDone::Done(val);
            }
        }
        if let MaybeDone::Pending(ref mut f) = this.b {
            if let Poll::Ready(val) = unsafe { Pin::new_unchecked(f) }.poll(cx) {
                this.b = MaybeDone::Done(val);
            }
        }

        match (&this.a, &this.b) {
            (MaybeDone::Done(_), MaybeDone::Done(_)) => {
                let a = match core::mem::replace(&mut this.a, MaybeDone::Taken) {
                    MaybeDone::Done(v) => v,
                    _ => unreachable!(),
                };
                let b = match core::mem::replace(&mut this.b, MaybeDone::Taken) {
                    MaybeDone::Done(v) => v,
                    _ => unreachable!(),
                };
                Poll::Ready((a, b))
            }
            _ => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// join3 — same for three futures
// ---------------------------------------------------------------------------

/// Drive three futures concurrently.
pub fn join3<FA, FB, FC>(a: FA, b: FB, c: FC) -> Join3<FA, FB, FC>
where
    FA: Future,
    FB: Future,
    FC: Future,
{
    Join3 {
        a: MaybeDone::Pending(a),
        b: MaybeDone::Pending(b),
        c: MaybeDone::Pending(c),
    }
}

pub struct Join3<FA: Future, FB: Future, FC: Future> {
    a: MaybeDone<FA>,
    b: MaybeDone<FB>,
    c: MaybeDone<FC>,
}

impl<FA: Future, FB: Future, FC: Future> Future for Join3<FA, FB, FC> {
    type Output = (FA::Output, FB::Output, FC::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };

        if let MaybeDone::Pending(ref mut f) = this.a {
            if let Poll::Ready(val) = unsafe { Pin::new_unchecked(f) }.poll(cx) {
                this.a = MaybeDone::Done(val);
            }
        }
        if let MaybeDone::Pending(ref mut f) = this.b {
            if let Poll::Ready(val) = unsafe { Pin::new_unchecked(f) }.poll(cx) {
                this.b = MaybeDone::Done(val);
            }
        }
        if let MaybeDone::Pending(ref mut f) = this.c {
            if let Poll::Ready(val) = unsafe { Pin::new_unchecked(f) }.poll(cx) {
                this.c = MaybeDone::Done(val);
            }
        }

        match (&this.a, &this.b, &this.c) {
            (MaybeDone::Done(_), MaybeDone::Done(_), MaybeDone::Done(_)) => {
                let a = match core::mem::replace(&mut this.a, MaybeDone::Taken) {
                    MaybeDone::Done(v) => v,
                    _ => unreachable!(),
                };
                let b = match core::mem::replace(&mut this.b, MaybeDone::Taken) {
                    MaybeDone::Done(v) => v,
                    _ => unreachable!(),
                };
                let c = match core::mem::replace(&mut this.c, MaybeDone::Taken) {
                    MaybeDone::Done(v) => v,
                    _ => unreachable!(),
                };
                Poll::Ready((a, b, c))
            }
            _ => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// select — return whichever future completes first
// ---------------------------------------------------------------------------

/// Drive two futures concurrently, returning whichever completes first.
///
/// The losing future is **dropped** (cancelled).
///
/// ```rust,ignore
/// match select(server.next(), Timer::after(100)).await {
///     Either::Left(msg)  => handle(msg),
///     Either::Right(())  => log::warn!("timeout!"),
/// }
/// ```
pub fn select<FA, FB>(a: FA, b: FB) -> Select<FA, FB>
where
    FA: Future,
    FB: Future,
{
    Select { a, b }
}

/// Result of [`select`]: which branch completed first.
pub enum Either<A, B> {
    Left(A),
    Right(B),
}

pub struct Select<FA, FB> {
    a: FA,
    b: FB,
}

impl<FA: Future, FB: Future> Future for Select<FA, FB> {
    type Output = Either<FA::Output, FB::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: Same pin-projection guarantee as Join — we never move
        // the inner futures after the outer Pin is created.
        let this = unsafe { self.get_unchecked_mut() };

        if let Poll::Ready(val) = unsafe { Pin::new_unchecked(&mut this.a) }.poll(cx) {
            return Poll::Ready(Either::Left(val));
        }
        if let Poll::Ready(val) = unsafe { Pin::new_unchecked(&mut this.b) }.poll(cx) {
            return Poll::Ready(Either::Right(val));
        }
        Poll::Pending
    }
}
