use std::{future::pending, sync::Arc};

use tokio::{sync::Semaphore, time::Instant};

use super::{Decision, ReqInfo};

/// A concurrency limiter: a hard cap on the number of requests *actively* being
/// served at once.
///
/// Semaphore-style — capacity is taken on serve start ([`on_start`]) and returned
/// on serve end ([`on_end`]). It "wakes when another serve ends": that event is
/// delivered to the scheduler as an `End` message, which re-drives the queue, so
/// this limiter's own wakeup is simply [`pending`] (it never frees on its own).
///
/// [`on_start`]: ConcurrencyLimiter::on_start
/// [`on_end`]: ConcurrencyLimiter::on_end
pub struct ConcurrencyLimiter {
    sem: Arc<Semaphore>,
}

impl ConcurrencyLimiter {
    /// Allow at most `limit` requests to be served concurrently.
    pub fn new(limit: usize) -> Self {
        Self { sem: Arc::new(Semaphore::new(limit)) }
    }

    /// Ready while a permit is available. When full, the wakeup never fires on its
    /// own: the only thing that frees capacity is a serve ending, which the
    /// scheduler observes as an `End` message and uses to re-evaluate the queue.
    pub fn check(&self, _now: Instant, _info: &ReqInfo) -> Decision {
        if self.sem.available_permits() > 0 {
            Decision::Ready
        } else {
            Decision::Blocked(Box::pin(pending::<()>()))
        }
    }

    /// Take a permit. The scheduler only calls this after [`check`] reported
    /// [`Decision::Ready`] in the same await-free turn, so a permit is guaranteed
    /// available; `forget` keeps it taken until [`on_end`] returns it.
    ///
    /// [`check`]: ConcurrencyLimiter::check
    /// [`on_end`]: ConcurrencyLimiter::on_end
    pub fn on_start(&mut self, _now: Instant, _info: &ReqInfo) {
        self.sem.try_acquire().expect("permit available (verified by check())").forget();
    }

    /// Return a permit when a serve finishes.
    pub fn on_end(&mut self, _info: &ReqInfo) {
        self.sem.add_permits(1);
    }

    /// A lock-free handle for reading the number of remaining permits.
    pub fn observer(&self) -> ConcurrencyObserver {
        ConcurrencyObserver { sem: self.sem.clone() }
    }
}

/// Lock-free view of a [`ConcurrencyLimiter`]'s free capacity.
pub struct ConcurrencyObserver {
    sem: Arc<Semaphore>,
}

impl ConcurrencyObserver {
    /// Number of serves that may currently start before hitting the cap.
    pub fn available(&self) -> u64 {
        self.sem.available_permits() as u64
    }
}
