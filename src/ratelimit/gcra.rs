use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use tokio::time::{Instant, sleep};

use super::{Decision, ReqInfo};

/// A GCRA (Generic Cell Rate Algorithm) rate limiter over an abstract resource.
///
/// Each request consumes some amount of the resource, given by [`cost`]. The two
/// concrete uses differ *only* in their parameters and cost function:
/// - request-rate limiting: `cost = 1` per request,
/// - bandwidth limiting: `cost = response size in bytes`.
///
/// State is a single theoretical arrival time (`tat`). Capacity is only ever
/// consumed on serve start ([`on_start`]); there is nothing to release on serve
/// end, so it wakes purely on the clock.
///
/// `tat` is kept in an [`Arc<AtomicI64>`] (nanoseconds relative to `base`) so the
/// metrics endpoint can read the live bucket level lock-free and compute the
/// remaining capacity online — see [`GcraObserver`]. The limiter itself remains
/// single-owner; the atomic is only ever written from the scheduler task.
///
/// [`cost`]: GcraLimiter::cost
/// [`on_start`]: GcraLimiter::on_start
pub struct GcraLimiter {
    /// Emission interval per unit of resource, `T = 1 / rate`.
    per_unit: Duration,
    /// Burst tolerance `tau = burst * T` (the bucket's full drain time).
    tolerance: Duration,
    /// Resource consumed by a request (e.g. `1` for req-rate, size for bandwidth).
    cost: fn(&ReqInfo) -> u32,
    /// Origin for the nanosecond offsets stored in `tat`.
    base: Instant,
    /// Theoretical arrival time, as nanoseconds since `base`.
    tat: Arc<AtomicI64>,
}

impl GcraLimiter {
    /// Build a limiter allowing a sustained `rate` units/second with up to
    /// `burst` units consumed back-to-back. `cost` maps a request to the number
    /// of units it consumes.
    pub fn new(rate: f64, burst: u32, cost: fn(&ReqInfo) -> u32) -> Self {
        let per_unit = Duration::from_secs_f64(1.0 / rate);
        let tolerance = per_unit.saturating_mul(burst);
        Self {
            per_unit,
            tolerance,
            cost,
            base: Instant::now(),
            tat: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Time the theoretical arrival time advances if this request is served.
    fn increment(&self, info: &ReqInfo) -> Duration {
        self.per_unit.saturating_mul((self.cost)(info))
    }

    /// The theoretical arrival time as an absolute [`Instant`].
    fn tat(&self) -> Instant {
        self.base + Duration::from_nanos(self.tat.load(Ordering::Relaxed).max(0) as u64)
    }

    /// Read-only check: is a request allowed to start right now?
    ///
    /// Allowed when `tat + increment <= now + tolerance`. Otherwise the wakeup
    /// sleeps exactly until that becomes true.
    pub fn check(&self, now: Instant, info: &ReqInfo) -> Decision {
        let target = self.tat() + self.increment(info);
        let threshold = now + self.tolerance;
        if target <= threshold {
            Decision::Ready
        } else {
            Decision::Blocked(Box::pin(sleep(target.saturating_duration_since(threshold))))
        }
    }

    /// Commit a grant: advance the theoretical arrival time by the request's cost.
    pub fn on_start(&mut self, now: Instant, info: &ReqInfo) {
        let tat = self.tat().max(now) + self.increment(info);
        self.tat.store(tat.saturating_duration_since(self.base).as_nanos() as i64, Ordering::Relaxed);
    }

    /// GCRA has no per-serve state to release.
    pub fn on_end(&mut self, _info: &ReqInfo) {}

    /// A lock-free handle for reading the bucket's remaining capacity.
    pub fn observer(&self) -> GcraObserver {
        GcraObserver {
            per_unit: self.per_unit,
            tolerance: self.tolerance,
            base: self.base,
            tat: self.tat.clone(),
        }
    }
}

/// Lock-free view of a [`GcraLimiter`]'s remaining bucket capacity, computed
/// online against the current clock. Reports units (requests for req-rate,
/// bytes for traffic), so a full bucket equals the configured burst.
pub struct GcraObserver {
    per_unit: Duration,
    tolerance: Duration,
    base: Instant,
    tat: Arc<AtomicI64>,
}

impl GcraObserver {
    /// Remaining units in the bucket at `now`, capped at the burst capacity.
    pub fn remaining(&self, now: Instant) -> u64 {
        let tat = self.base + Duration::from_nanos(self.tat.load(Ordering::Relaxed).max(0) as u64);
        let used = tat.saturating_duration_since(now);
        let free = self.tolerance.saturating_sub(used);
        (free.as_secs_f64() / self.per_unit.as_secs_f64()) as u64
    }
}
