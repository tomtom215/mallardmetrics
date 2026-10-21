//! Delivery: a bounded queue, one worker, bounded retries.
//!
//! A receiver that is down must not be able to grow this process. The queue
//! holds at most `capacity` notifications; when it is full the *oldest* is
//! discarded (and counted), because a newer message about the same alert
//! supersedes it. One worker delivers one notification at a time, so a dead
//! receiver costs at most `capacity * max_attempts * timeout` of background
//! work, never a thread or a socket per message.

use super::client::{Client, DeliveryError};
use super::metrics::AlertMetrics;
use super::payload::{Notification, PayloadFormat};
use super::policy::Target;
use super::{Clock, Secret};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, watch};

/// Queue capacity in production.
pub const QUEUE_CAPACITY: usize = 64;

/// How a notification is sent and re-sent.
pub struct DeliveryPolicy {
    pub target: Target,
    pub secret: Option<Secret>,
    pub format: PayloadFormat,
    /// Attempts per notification, including the first.
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles each time.
    pub backoff_base: Duration,
    /// Longest delay between attempts.
    pub backoff_cap: Duration,
}

/// The wait before retry number `attempt` (1 = after the first failure).
///
/// Exponential with "equal jitter": half the delay is fixed, half is random,
/// so retries from many instances do not synchronise and the delay still grows.
/// `unit` is a uniform number in `[0, 1]`; it is a parameter so the schedule is
/// testable.
pub fn backoff(attempt: u32, base: Duration, cap: Duration, unit: f64) -> Duration {
    let doubled = base.saturating_mul(1u32 << attempt.saturating_sub(1).min(20));
    let full = doubled.min(cap);
    let half = full / 2;
    half + half.mul_f64(unit.clamp(0.0, 1.0))
}

/// Sends notifications one at a time with retries.
pub struct Deliverer {
    client: Client,
    policy: DeliveryPolicy,
    clock: Arc<dyn Clock>,
    metrics: Arc<AlertMetrics>,
}

impl Deliverer {
    pub fn new(
        client: Client,
        policy: DeliveryPolicy,
        clock: Arc<dyn Clock>,
        metrics: Arc<AlertMetrics>,
    ) -> Self {
        Self {
            client,
            policy,
            clock,
            metrics,
        }
    }

    /// `scheme://host` of the receiver, for logs.
    pub fn target_display(&self) -> String {
        self.policy.target.display()
    }

    /// One attempt. Each attempt carries a fresh timestamp and signature.
    async fn attempt(&self, n: &Notification, body: &[u8]) -> Result<u16, DeliveryError> {
        let timestamp = self.clock.now().timestamp();
        let signature = self
            .policy
            .secret
            .as_ref()
            .map(|s| super::sign::signature(s.expose(), timestamp, body));
        let timestamp_text = timestamp.to_string();
        let user_agent = concat!("mallard-metrics/", env!("CARGO_PKG_VERSION"));
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/json"),
            ("User-Agent", user_agent),
            ("X-Mallard-Event", n.kind.as_str()),
            ("X-Mallard-Delivery", &n.delivery_id),
            ("X-Mallard-Timestamp", &timestamp_text),
        ];
        if let Some(sig) = &signature {
            headers.push(("X-Mallard-Signature", sig));
        }
        self.client.post(&self.policy.target, &headers, body).await
    }

    /// Deliver `n`, retrying transient failures until `max_attempts`, a
    /// permanent failure, or `closing` turning true. Updates the counters.
    pub async fn deliver(
        &self,
        n: &Notification,
        closing: &mut watch::Receiver<bool>,
    ) -> Result<u32, DeliveryError> {
        let body = n.body(self.policy.format);
        let host = self.policy.target.display();
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.attempt(n, &body).await {
                Ok(status) => {
                    let at = u64::try_from(self.clock.now().timestamp()).unwrap_or(0);
                    self.metrics.sent(n.kind, at);
                    tracing::info!(
                        kind = n.kind.as_str(),
                        target = %host,
                        status,
                        attempt,
                        "Webhook notification delivered"
                    );
                    return Ok(attempt);
                }
                Err(e) => {
                    let last = attempt >= self.policy.max_attempts || !e.retryable();
                    if last || *closing.borrow() {
                        self.metrics.failed(n.kind);
                        tracing::warn!(
                            kind = n.kind.as_str(),
                            target = %host,
                            attempt,
                            error = %e,
                            "Webhook notification abandoned"
                        );
                        return Err(e);
                    }
                    let wait = backoff(
                        attempt,
                        self.policy.backoff_base,
                        self.policy.backoff_cap,
                        rand::random_range(0.0..=1.0),
                    );
                    tracing::warn!(
                        kind = n.kind.as_str(),
                        target = %host,
                        attempt,
                        retry_in_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                        error = %e,
                        "Webhook delivery failed; will retry"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        _ = closing.changed() => {
                            self.metrics.failed(n.kind);
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
}

struct Inner {
    queue: parking_lot::Mutex<VecDeque<Notification>>,
    capacity: usize,
    wake: Notify,
    closed: AtomicBool,
    metrics: Arc<AlertMetrics>,
}

/// The producer side of the queue. Cloning shares the queue.
#[derive(Clone)]
pub struct Notifier {
    inner: Arc<Inner>,
}

impl Notifier {
    pub fn new(capacity: usize, metrics: Arc<AlertMetrics>) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: parking_lot::Mutex::new(VecDeque::new()),
                capacity: capacity.max(1),
                wake: Notify::new(),
                closed: AtomicBool::new(false),
                metrics,
            }),
        }
    }

    /// Queue `n` without waiting. Never blocks the evaluator: a full queue
    /// discards its oldest entry, and a closed one discards `n`.
    pub fn enqueue(&self, n: Notification) {
        if self.inner.closed.load(Ordering::Acquire) {
            self.inner.metrics.dropped(n.kind);
            return;
        }
        let depth = {
            let mut queue = self.inner.queue.lock();
            if queue.len() >= self.inner.capacity
                && let Some(old) = queue.pop_front()
            {
                self.inner.metrics.dropped(old.kind);
                tracing::warn!(
                    kind = old.kind.as_str(),
                    "Webhook queue full; discarded the oldest notification"
                );
            }
            queue.push_back(n);
            queue.len()
        };
        self.inner.metrics.set_queue_depth(depth);
        self.inner.wake.notify_one();
    }

    /// Take the next notification, if any.
    pub fn pop(&self) -> Option<Notification> {
        let (item, depth) = {
            let mut queue = self.inner.queue.lock();
            (queue.pop_front(), queue.len())
        };
        self.inner.metrics.set_queue_depth(depth);
        item
    }

    /// Queue length.
    pub fn len(&self) -> usize {
        self.inner.queue.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stop accepting work and count whatever is still queued as dropped.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        while let Some(n) = self.pop() {
            self.inner.metrics.dropped(n.kind);
        }
        self.inner.wake.notify_waiters();
        self.inner.wake.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Run the delivery loop until [`Notifier::close`] and `closing` say stop.
    pub async fn run_worker(self, deliverer: Deliverer, mut closing: watch::Receiver<bool>) {
        loop {
            let next = loop {
                if let Some(n) = self.pop() {
                    break Some(n);
                }
                if self.is_closed() || *closing.borrow() {
                    break None;
                }
                tokio::select! {
                    () = self.inner.wake.notified() => {}
                    _ = closing.changed() => {}
                }
            };
            let Some(n) = next else { return };
            // The outcome is already counted and logged by `deliver`.
            let _ = deliverer.deliver(&n, &mut closing).await;
            if self.is_closed() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::metrics::Kind;
    use super::*;

    #[test]
    fn backoff_doubles_stays_under_the_cap_and_is_jittered() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        for attempt in 1..=10 {
            let lo = backoff(attempt, base, cap, 0.0);
            let hi = backoff(attempt, base, cap, 1.0);
            let full = base.saturating_mul(1 << (attempt - 1)).min(cap);
            assert_eq!(hi, full, "attempt {attempt}");
            assert_eq!(lo, full / 2, "attempt {attempt}");
            assert!(hi <= cap);
        }
        assert_eq!(backoff(1, base, cap, 1.0), Duration::from_secs(1));
        assert_eq!(backoff(3, base, cap, 1.0), Duration::from_secs(4));
        assert_eq!(backoff(30, base, cap, 1.0), cap);
        assert_eq!(
            backoff(500, base, cap, 7.0),
            cap,
            "no overflow, unit clamped"
        );
    }

    fn note(kind: Kind) -> Notification {
        let mut n = Notification::test(chrono::Utc::now());
        n.kind = kind;
        n
    }

    #[test]
    fn a_full_queue_discards_the_oldest_and_counts_it() {
        let metrics = Arc::new(AlertMetrics::default());
        let q = Notifier::new(2, Arc::clone(&metrics));
        q.enqueue(note(Kind::Report));
        q.enqueue(note(Kind::Alert));
        q.enqueue(note(Kind::Resolved));
        assert_eq!(q.len(), 2);
        assert_eq!(metrics.dropped_count(Kind::Report), 1);
        assert_eq!(q.pop().unwrap().kind, Kind::Alert, "the oldest went");
        assert_eq!(q.pop().unwrap().kind, Kind::Resolved);
    }

    #[test]
    fn a_closed_queue_refuses_and_counts() {
        let metrics = Arc::new(AlertMetrics::default());
        let q = Notifier::new(4, Arc::clone(&metrics));
        q.enqueue(note(Kind::Alert));
        q.close();
        assert!(q.is_empty());
        assert_eq!(metrics.dropped_count(Kind::Alert), 1);
        q.enqueue(note(Kind::Alert));
        assert!(q.is_empty());
        assert_eq!(metrics.dropped_count(Kind::Alert), 2);
    }
}
