//! Generic serve scheduler + rate limiting for the file-serving (`/h/`) route.
//!
//! # Flow
//! 1. The handler determines the response size and, *once the data is ready to
//!    stream*, [`enqueue`]s the request.
//! 2. The body simultaneously waits for slow-drain capacity and enqueues for a
//!    normal grant. Only ready requests are enqueued.
//! 3. Slow-drain capacity starts a paced response while the request remains in
//!    the priority queue. A normal grant upgrades it to full speed and returns
//!    the slow-drain slot.
//! 4. If neither grant arrives before the handler's timeout, dropping the body
//!    removes its [`Ticket`] from the queue and cancels its slow-drain waiter.
//!
//! Concurrency is handled by single ownership: the queue and all limiter state
//! live inside the [`run`] task and are mutated only via messages, so no shared
//! locks are taken on the hot path.
//!
//! [`enqueue`]: SchedulerHandle::enqueue

use std::{
    collections::BTreeMap,
    future::Future,
    num::NonZeroU64,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::future::select_all;
use log::warn;
use tokio::{
    sync::{
        mpsc::{self, Sender, UnboundedSender},
        oneshot, OwnedSemaphorePermit, Semaphore,
    },
    time::Instant,
};

use crate::{Command, metrics::Metrics};

mod concurrency;
mod gcra;

use concurrency::{ConcurrencyLimiter, ConcurrencyObserver};
use gcra::{GcraLimiter, GcraObserver};

// --- Tunable configuration -----------------------------------------------------

/// Tunable limits for the scheduler and rate limiters, supplied from CLI args.
#[derive(Clone, Copy)]
pub struct SchedulerConfig {
    /// Maximum number of requests allowed to sit in the queue; pushing past it
    /// evicts the largest (lowest priority) waiter.
    pub queue_limit: usize,
    /// Request-rate limiter: sustained requests/second.
    pub request_rate: f64,
    /// Request-rate limiter: burst (in requests).
    pub request_burst: u32,
    /// Bandwidth limiter: sustained bytes/second.
    pub traffic_rate: f64,
    /// Bandwidth limiter: burst (in bytes).
    pub traffic_burst: u32,
    /// Hard cap on the number of requests being served concurrently.
    pub concurrency_limit: usize,
    /// Bytes per second emitted by a request before it receives a normal grant.
    pub slow_drain_bandwidth: NonZeroU64,
    /// Hard cap on the number of requests serving in slow-drain mode.
    pub slow_drain_concurrency: usize,
    /// Sustained rejected-requests-per-second tolerated before signalling overload.
    pub overload_rate: f64,
    /// Minimum gap between two overload notifications sent to the RPC server.
    pub overload_notify_interval: Duration,
}

// --- Public request description ------------------------------------------------

/// Information about a request that rate limiters may use for their decisions.
#[derive(Clone, Copy)]
pub struct ReqInfo {
    /// Number of bytes the response will actually transfer (0 for HEAD, the range
    /// length for range requests, otherwise the full file size). Used both as the
    /// scheduling priority (smallest first) and as the bandwidth limiter's cost.
    pub size: u32,
}

/// A future that resolves when a blocked limiter might allow progress.
type Wakeup = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Outcome of asking the limiters whether a request may start.
pub enum Decision {
    /// All limiters allow the request to start now.
    Ready,
    /// At least one limiter blocks; the future resolves when re-checking is worthwhile.
    Blocked(Wakeup),
}

// --- Rate limiter set ----------------------------------------------------------

/// The fixed set of rate limiters. Intentionally a concrete struct (not a
/// `Vec<dyn RateLimiter>`): the set is known at compile time and new limiters are
/// added as additional fields. Serving requires *every* limiter to allow.
struct RateLimiters {
    /// Limits request rate; each request costs 1.
    req_rate_limit: GcraLimiter,
    /// Limits bandwidth; each request costs its response size in bytes.
    traffic_rate_limit: GcraLimiter,
    /// Hard cap on concurrently-serving requests.
    concurrency: ConcurrencyLimiter,
}

impl RateLimiters {
    fn new(config: &SchedulerConfig) -> Self {
        Self {
            req_rate_limit: GcraLimiter::new(config.request_rate, config.request_burst, |_| 1),
            traffic_rate_limit: GcraLimiter::new(config.traffic_rate, config.traffic_burst, |info| info.size),
            concurrency: ConcurrencyLimiter::new(config.concurrency_limit),
        }
    }

    /// Read-only check across all limiters. Returns [`Decision::Ready`] only when
    /// every limiter allows; otherwise a single wakeup that fires as soon as *any*
    /// blocking limiter might allow progress (re-checking then converges).
    fn check(&self, now: Instant, info: &ReqInfo) -> Decision {
        let mut wakeups = Vec::new();
        for decision in [
            self.req_rate_limit.check(now, info),
            self.traffic_rate_limit.check(now, info),
            self.concurrency.check(now, info),
        ] {
            if let Decision::Blocked(wakeup) = decision {
                wakeups.push(wakeup);
            }
        }
        match wakeups.len() {
            0 => Decision::Ready,
            1 => Decision::Blocked(wakeups.pop().expect("len == 1")),
            _ => Decision::Blocked(Box::pin(async move {
                let _ = select_all(wakeups).await;
            })),
        }
    }

    /// Commit a grant on every limiter.
    fn on_start(&mut self, now: Instant, info: &ReqInfo) {
        self.req_rate_limit.on_start(now, info);
        self.traffic_rate_limit.on_start(now, info);
        self.concurrency.on_start(now, info);
    }

    /// Release a finished serve on every limiter.
    fn on_end(&mut self, info: &ReqInfo) {
        self.req_rate_limit.on_end(info);
        self.traffic_rate_limit.on_end(info);
        self.concurrency.on_end(info);
    }

    /// Lock-free read handles for the metrics endpoint.
    fn observers(&self) -> LimiterObservers {
        LimiterObservers {
            req_rate: self.req_rate_limit.observer(),
            traffic_rate: self.traffic_rate_limit.observer(),
            concurrency: self.concurrency.observer(),
        }
    }
}

/// Lock-free read handles into the rate limiters, used by the metrics collector.
struct LimiterObservers {
    req_rate: GcraObserver,
    traffic_rate: GcraObserver,
    concurrency: ConcurrencyObserver,
}

// --- Scheduler messages & state ------------------------------------------------

/// Result delivered to a waiting handler once the scheduler reaches its request.
pub enum GrantResult {
    /// The request may proceed; hold the [`Permit`] for the whole response body.
    Granted(Permit),
    /// The request was evicted from the normal-grant queue (capacity exceeded).
    /// It may continue with a slow-drain grant but can no longer upgrade.
    Evicted,
}

enum SchedMsg {
    Enqueue {
        seq: u64,
        info: ReqInfo,
        grant: oneshot::Sender<GrantResult>,
    },
    Remove {
        key: Key,
    },
    End {
        info: ReqInfo,
    },
}

struct Waiter {
    info: ReqInfo,
    grant: oneshot::Sender<GrantResult>,
}

/// Priority key: smallest response first, then arrival order for stability.
type Key = (u32, u64);

// --- Handles given out to request handlers -------------------------------------

/// Shared handle used by request handlers to talk to the scheduler task.
pub struct SchedulerHandle {
    tx: UnboundedSender<SchedMsg>,
    seq: AtomicU64,
    slow_drain: Arc<SlowDrainLimiter>,
}

impl SchedulerHandle {
    /// Spawn the scheduler task and return a shared handle to it. The `command`
    /// channel is used to notify the RPC server when the queue sheds too many
    /// requests (overload). Registers a lock-free metrics collector exposing the
    /// limiter bucket levels and the live queue size.
    pub fn spawn(config: SchedulerConfig, command: Sender<Command>, refill: Arc<RefillLimiter>, metrics: &mut Metrics) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let slow_drain = SlowDrainLimiter::new(config.slow_drain_bandwidth, config.slow_drain_concurrency);
        let handle = Arc::new(Self {
            tx: tx.clone(),
            seq: AtomicU64::new(0),
            slow_drain: slow_drain.clone(),
        });
        let limiters = RateLimiters::new(&config);
        let overload = OverloadTracker::new(config.overload_rate, config.overload_notify_interval, command);
        let queue_size = Arc::new(AtomicU64::new(0));
        metrics.register_scheduler(SchedulerCollector {
            limiters: limiters.observers(),
            overload: overload.gcra.observer(),
            queue_size: queue_size.clone(),
            refill,
            slow_drain,
        });
        tokio::spawn(run(rx, tx, config, limiters, overload, queue_size));
        handle
    }

    /// Register a request that is ready to serve. The returned [`Ticket`] keeps the
    /// request in the queue until dropped; awaiting the receiver yields a
    /// [`GrantResult`] once the scheduler grants or evicts the request. Only
    /// enqueue a request once its data is available to stream — the queue holds
    /// ready requests exclusively.
    pub fn enqueue(&self, info: ReqInfo) -> (Ticket, oneshot::Receiver<GrantResult>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (grant_tx, grant_rx) = oneshot::channel();
        let _ = self.tx.send(SchedMsg::Enqueue { seq, info, grant: grant_tx });
        (
            Ticket {
                key: (info.size, seq),
                tx: self.tx.clone(),
            },
            grant_rx,
        )
    }

    /// Limiter used to begin a paced response while the normal grant is pending.
    pub fn slow_drain(&self) -> Arc<SlowDrainLimiter> {
        self.slow_drain.clone()
    }
}

/// Keeps a request registered in the scheduler queue. Dropping it (on timeout,
/// early error, or after a grant) removes the request from the queue; the removal
/// is idempotent, so dropping after a grant is harmless.
pub struct Ticket {
    key: Key,
    tx: UnboundedSender<SchedMsg>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = self.tx.send(SchedMsg::Remove { key: self.key });
    }
}

/// Proof that a serve was granted by every rate limiter. Must be held for the
/// entire response body; dropping it releases the serve (semaphore-style limiters
/// react to this). The body handler keeps it alive inside its response stream for
/// the whole duration of the transfer.
pub struct Permit {
    tx: UnboundedSender<SchedMsg>,
    info: ReqInfo,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let _ = self.tx.send(SchedMsg::End { info: self.info });
    }
}

// --- Slow-drain limiter --------------------------------------------------------

/// Concurrency and pacing configuration for responses waiting on a normal grant.
pub struct SlowDrainLimiter {
    bandwidth: NonZeroU64,
    sem: Arc<Semaphore>,
}

impl SlowDrainLimiter {
    fn new(bandwidth: NonZeroU64, concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            bandwidth,
            sem: Arc::new(Semaphore::new(concurrency)),
        })
    }

    /// Wait for slow-drain capacity. The returned guard releases the slot on drop.
    pub async fn acquire(self: &Arc<Self>) -> SlowDrainPermit {
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("slow-drain semaphore is never closed");
        SlowDrainPermit {
            bandwidth: self.bandwidth,
            _permit: permit,
        }
    }

    fn available(&self) -> u64 {
        self.sem.available_permits() as u64
    }
}

/// Proof that a response may occupy one slow-drain slot.
pub struct SlowDrainPermit {
    bandwidth: NonZeroU64,
    _permit: OwnedSemaphorePermit,
}

impl SlowDrainPermit {
    /// Delay needed to emit `bytes` without exceeding the configured bandwidth.
    pub fn delay_for(&self, bytes: usize) -> Duration {
        Duration::from_secs_f64(bytes as f64 / self.bandwidth.get() as f64)
    }

    /// Bound the interval between generated chunks below the QUIC idle timeout.
    pub fn chunk_len(&self, remaining: usize) -> usize {
        const MAX_PACING_INTERVAL_SECS: u64 = 5;
        let max_len = self.bandwidth.get().saturating_mul(MAX_PACING_INTERVAL_SECS);
        remaining.min(max_len.min(usize::MAX as u64) as usize).max(1)
    }
}

// --- Cache-refill concurrency limiter ------------------------------------------

/// Caps the number of concurrent cache-miss refills. Acquisition is a load + CAS
/// (`fetch_update`): if at capacity, no slot is taken and the caller should behave
/// as if its serve timed out instead of starting a refill.
pub struct RefillLimiter {
    count: AtomicU64,
    limit: u64,
}

impl RefillLimiter {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self { count: AtomicU64::new(0), limit })
    }

    /// Reserve a refill slot, returning a guard that releases it on drop. Returns
    /// `None` when at capacity (refill should be skipped).
    pub fn try_acquire(self: &Arc<Self>) -> Option<RefillPermit> {
        self.count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < self.limit).then_some(n + 1))
            .ok()
            .map(|_| RefillPermit { limiter: self.clone() })
    }

    /// Remaining free refill slots, read lock-free for metrics.
    pub fn available(&self) -> u64 {
        self.limit.saturating_sub(self.count.load(Ordering::Relaxed))
    }
}

/// Holds a refill slot for the lifetime of a download worker; releases on drop.
pub struct RefillPermit {
    limiter: Arc<RefillLimiter>,
}

impl Drop for RefillPermit {
    fn drop(&mut self) {
        self.limiter.count.fetch_sub(1, Ordering::AcqRel);
    }
}

// --- Scheduler task ------------------------------------------------------------

async fn run(
    mut rx: mpsc::UnboundedReceiver<SchedMsg>,
    tx: UnboundedSender<SchedMsg>,
    config: SchedulerConfig,
    mut limiters: RateLimiters,
    mut overload: OverloadTracker,
    queue_size: Arc<AtomicU64>,
) {
    let mut queue: BTreeMap<Key, Waiter> = BTreeMap::new();

    loop {
        // Dispatch as many requests as the limiters currently allow. Every queued
        // request is ready to serve, so the smallest-key entry (smallest response,
        // then arrival order) is always the next candidate — an O(log n) peek.
        let wakeup = loop {
            let now = Instant::now();
            let Some((_, waiter)) = queue.first_key_value() else {
                break None; // Queue empty.
            };
            let info = waiter.info;
            match limiters.check(now, &info) {
                Decision::Ready => {
                    let (_, waiter) = queue.pop_first().expect("queue just observed non-empty");
                    // Skip handlers that already gave up so we don't consume capacity
                    // for a serve that will never happen.
                    if waiter.grant.is_closed() {
                        continue;
                    }
                    // Commit before building the permit so that on_start and the
                    // permit's eventual End (on_end) are paired exactly 1:1 — even
                    // if `send` races a handler drop, the returned permit's End
                    // releases the capacity we just took.
                    limiters.on_start(now, &info);
                    let _ = waiter.grant.send(GrantResult::Granted(Permit { tx: tx.clone(), info }));
                }
                Decision::Blocked(wakeup) => break Some(wakeup),
            }
        };
        queue_size.store(queue.len() as u64, Ordering::Relaxed);

        // Wait for a queue change or for a blocked limiter to free up.
        match wakeup {
            Some(wakeup) => {
                tokio::select! {
                    biased;
                    msg = rx.recv() => match msg {
                        Some(msg) => apply(msg, &mut queue, &mut limiters, &config, &mut overload, &queue_size),
                        None => break,
                    },
                    _ = wakeup => {}
                }
            }
            None => match rx.recv().await {
                Some(msg) => apply(msg, &mut queue, &mut limiters, &config, &mut overload, &queue_size),
                None => break,
            },
        }
    }
}

fn apply(
    msg: SchedMsg,
    queue: &mut BTreeMap<Key, Waiter>,
    limiters: &mut RateLimiters,
    config: &SchedulerConfig,
    overload: &mut OverloadTracker,
    queue_size: &AtomicU64,
) {
    match msg {
        SchedMsg::Enqueue { seq, info, grant } => {
            queue.insert((info.size, seq), Waiter { info, grant });
            // Cap the queue: pushing past the limit evicts the largest (lowest
            // priority) waiter so smallest-first dispatch is preserved.
            if queue.len() > config.queue_limit
                && let Some((_, waiter)) = queue.pop_last()
            {
                let _ = waiter.grant.send(GrantResult::Evicted);
                overload.record_rejection();
            }
        }
        SchedMsg::Remove { key } => {
            queue.remove(&key);
        }
        SchedMsg::End { info } => limiters.on_end(&info),
    }
    queue_size.store(queue.len() as u64, Ordering::Relaxed);
}

/// Tracks rejected (evicted) requests with a GCRA bucket sized to one minute of
/// the configured rate. When sustained rejections exhaust the bucket, the RPC
/// server is told we are overloaded — at most once per `notify_interval`.
struct OverloadTracker {
    gcra: GcraLimiter,
    command: Sender<Command>,
    notify_interval: Duration,
    last_notify: Option<Instant>,
}

impl OverloadTracker {
    fn new(rate: f64, notify_interval: Duration, command: Sender<Command>) -> Self {
        let burst = (rate * 60.0).ceil() as u32;
        Self {
            gcra: GcraLimiter::new(rate, burst, |_| 1),
            command,
            notify_interval,
            last_notify: None,
        }
    }

    fn record_rejection(&mut self) {
        let now = Instant::now();
        let info = ReqInfo { size: 1 };
        match self.gcra.check(now, &info) {
            Decision::Ready => self.gcra.on_start(now, &info),
            Decision::Blocked(_) => {
                if self.last_notify.is_none_or(|last| now.duration_since(last) >= self.notify_interval) {
                    self.last_notify = Some(now);
                    warn!("Server overloaded!");
                    let _ = self.command.try_send(Command::Overload);
                }
            }
        }
    }
}

/// Prometheus collector exposing the live state of the serve scheduler's rate
/// limiters and queue. All values are read lock-free from shared handles: the
/// GCRA bucket levels are computed online against the current clock, so nothing
/// extra is kept inside `Metrics`. Intended to be scraped infrequently.
pub struct SchedulerCollector {
    limiters: LimiterObservers,
    overload: GcraObserver,
    queue_size: Arc<AtomicU64>,
    refill: Arc<RefillLimiter>,
    slow_drain: Arc<SlowDrainLimiter>,
}

impl std::fmt::Debug for SchedulerCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SchedulerCollector")
    }
}

impl prometheus_client::collector::Collector for SchedulerCollector {
    fn encode(&self, mut encoder: prometheus_client::encoding::DescriptorEncoder) -> Result<(), std::fmt::Error> {
        use prometheus_client::{encoding::EncodeMetric, metrics::gauge::ConstGauge};

        let now = Instant::now();
        for (name, help, value) in [
            ("concurrency_available", "Remaining concurrent serve slots", self.limiters.concurrency.available()),
            ("request_rate_remaining", "Remaining requests in the request-rate bucket", self.limiters.req_rate.remaining(now)),
            ("traffic_rate_remaining", "Remaining bytes in the traffic-rate bucket", self.limiters.traffic_rate.remaining(now)),
            ("overload_remaining", "Remaining rejections in the overload bucket", self.overload.remaining(now)),
            ("queue_size", "Number of requests waiting in the serve queue", self.queue_size.load(Ordering::Relaxed)),
            ("refill_available", "Remaining concurrent cache-miss refill slots", self.refill.available()),
            ("slow_drain_available", "Remaining concurrent slow-drain slots", self.slow_drain.available()),
        ] {
            let gauge = ConstGauge::new(value as i64);
            let metric_encoder = encoder.encode_descriptor(name, help, None, gauge.metric_type())?;
            gauge.encode(metric_encoder)?;
        }
        Ok(())
    }
}
