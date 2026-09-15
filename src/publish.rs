//! Best-effort sensory publication, isolated from the safety failure domain.
//!
//! This is a thin stub for GH#42 (safety must not stall on IPC). Full
//! `corpus-ipc` transport remains GH#40. Implementations of
//! [`SensoryPublisher::try_publish`] must return promptly; the supervisor
//! never awaits a consumer and never calls into this module from inside
//! [`crate::safety::SafetyMachine::evaluate`].
//!
//! The outbound queue is explicitly bounded. Full-queue behavior is a
//! documented [`QueueFullPolicy`]; overflow and other loss reasons are
//! counted with a closed [`DropReason`] label set so Prometheus cardinality
//! cannot grow from payload or error strings.

use crate::safety::{SafetyMachine, SafetySnapshot};
use crate::telemetry::{SensoryMapping, TelemetryFrame};
use metrics::{counter, gauge};
use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Why a best-effort publish did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// No Brainstem / corpus-ipc transport is configured (current production).
    Absent,
    /// The publisher worker is gone.
    Disconnected,
    /// Bounded queue is full and the policy rejected the incoming frame.
    SlowConsumer,
    /// The transport returned a send failure.
    SendFailed(String),
}

/// Best-effort sensory publisher. Must not block the safety loop.
pub trait SensoryPublisher: Send + Sync {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError>;
}

/// Brainstem completely absent. Returns immediately.
#[derive(Debug, Default, Clone, Copy)]
pub struct AbsentPublisher;

impl SensoryPublisher for AbsentPublisher {
    fn try_publish(&self, _mapping: &SensoryMapping) -> Result<(), PublishError> {
        record_drop(DropReason::Absent);
        Err(PublishError::Absent)
    }
}

/// Test double that fails every send without blocking.
#[derive(Debug, Clone)]
pub struct FailingPublisher {
    pub reason: String,
}

impl FailingPublisher {
    #[must_use]
    pub fn send_failed() -> Self {
        Self {
            reason: "ipc send failed".to_string(),
        }
    }

    #[must_use]
    pub fn disconnected() -> Self {
        Self {
            reason: "disconnected".to_string(),
        }
    }
}

impl SensoryPublisher for FailingPublisher {
    fn try_publish(&self, _mapping: &SensoryMapping) -> Result<(), PublishError> {
        record_drop(DropReason::SendFailed);
        Err(PublishError::SendFailed(self.reason.clone()))
    }
}

/// Behavior when [`IsolatedPublishQueue`] is at capacity.
///
/// Metric / snapshot labels use [`Self::as_str`] (snake_case). CLI / env
/// parsing uses kebab-case (`drop-oldest`, `reject-newest`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueFullPolicy {
    /// Discard the oldest queued frame so the incoming (newest) frame is kept.
    DropOldest,
    /// Reject the incoming frame and leave the queue unchanged.
    RejectNewest,
}

impl QueueFullPolicy {
    /// All policies, in stable metric order.
    pub const ALL: [Self; 2] = [Self::DropOldest, Self::RejectNewest];

    /// Prometheus `policy` label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DropOldest => "drop_oldest",
            Self::RejectNewest => "reject_newest",
        }
    }

    /// Canonical CLI / env token (kebab-case).
    #[must_use]
    pub const fn as_cli_str(self) -> &'static str {
        match self {
            Self::DropOldest => "drop-oldest",
            Self::RejectNewest => "reject-newest",
        }
    }
}

impl fmt::Display for QueueFullPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_cli_str())
    }
}

impl FromStr for QueueFullPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "drop-oldest" | "drop_oldest" => Ok(Self::DropOldest),
            "reject-newest" | "reject_newest" => Ok(Self::RejectNewest),
            other => Err(format!(
                "unknown sensory-queue full policy '{other}'; expected drop-oldest or reject-newest"
            )),
        }
    }
}

/// Closed set of sensory-frame loss reasons. Never derived from payload data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// Incoming frame rejected because the queue was full ([`QueueFullPolicy::RejectNewest`]).
    RejectNewest,
    /// Oldest queued frame discarded to admit a newer one ([`QueueFullPolicy::DropOldest`]).
    DropOldest,
    /// No transport / publisher is configured ([`AbsentPublisher`]).
    Absent,
    /// The queue consumer was dropped.
    Disconnected,
    /// Transport reported a send failure. The label is static; the error
    /// string is not used as a Prometheus label.
    SendFailed,
}

impl DropReason {
    /// All reasons, in stable metric-id order. Cardinality is this length.
    pub const ALL: [Self; 5] = [
        Self::RejectNewest,
        Self::DropOldest,
        Self::Absent,
        Self::Disconnected,
        Self::SendFailed,
    ];

    /// Stable index into [`QueueSnapshot::dropped_by_reason`].
    #[must_use]
    pub const fn as_id(self) -> usize {
        match self {
            Self::RejectNewest => 0,
            Self::DropOldest => 1,
            Self::Absent => 2,
            Self::Disconnected => 3,
            Self::SendFailed => 4,
        }
    }

    /// Prometheus `reason` label. Closed vocabulary; never input data.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RejectNewest => "reject_newest",
            Self::DropOldest => "drop_oldest",
            Self::Absent => "absent",
            Self::Disconnected => "disconnected",
            Self::SendFailed => "send_failed",
        }
    }
}

/// Invalid [`QueueConfig`] constructor input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueConfigError {
    /// Capacity was zero or otherwise below the minimum.
    CapacityTooSmall { capacity: usize, min: usize },
    /// Capacity exceeded the documented maximum.
    CapacityTooLarge { capacity: usize, max: usize },
}

impl fmt::Display for QueueConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityTooSmall { capacity, min } => {
                write!(
                    f,
                    "sensory queue capacity {capacity} is below minimum {min}"
                )
            }
            Self::CapacityTooLarge { capacity, max } => {
                write!(f, "sensory queue capacity {capacity} exceeds maximum {max}")
            }
        }
    }
}

impl std::error::Error for QueueConfigError {}

/// Validated outbound sensory-queue configuration.
///
/// Fields are private so a caller cannot assemble an invalid capacity and
/// pass it to [`IsolatedPublishQueue::new`]. Use [`QueueConfig::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    capacity: usize,
    policy: QueueFullPolicy,
}

impl QueueConfig {
    /// Production default: a few seconds of 100 ms ticks.
    pub const DEFAULT_CAPACITY: usize = 32;
    /// Smallest accepted capacity (the queue is never unbounded).
    pub const MIN_CAPACITY: usize = 1;
    /// Largest accepted capacity (caps memory; not a performance target).
    pub const MAX_CAPACITY: usize = 4096;

    /// Validate `capacity` and pair it with `policy`.
    pub fn new(capacity: usize, policy: QueueFullPolicy) -> Result<Self, QueueConfigError> {
        Self::validate_capacity(capacity)?;
        Ok(Self { capacity, policy })
    }

    /// Reject zero and oversized capacities.
    pub fn validate_capacity(capacity: usize) -> Result<usize, QueueConfigError> {
        if capacity < Self::MIN_CAPACITY {
            return Err(QueueConfigError::CapacityTooSmall {
                capacity,
                min: Self::MIN_CAPACITY,
            });
        }
        if capacity > Self::MAX_CAPACITY {
            return Err(QueueConfigError::CapacityTooLarge {
                capacity,
                max: Self::MAX_CAPACITY,
            });
        }
        Ok(capacity)
    }

    /// Maximum frames retained.
    #[must_use]
    pub const fn capacity(self) -> usize {
        self.capacity
    }

    /// Deterministic behavior when [`Self::capacity`] is reached.
    #[must_use]
    pub const fn policy(self) -> QueueFullPolicy {
        self.policy
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            capacity: Self::DEFAULT_CAPACITY,
            policy: QueueFullPolicy::DropOldest,
        }
    }
}

/// Point-in-time queue gauges/counters for tests and Prometheus text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub capacity: usize,
    pub policy: QueueFullPolicy,
    pub depth: usize,
    pub enqueued_total: u64,
    /// Counts indexed by [`DropReason::as_id`]. Length is [`DropReason::ALL`].
    pub dropped_by_reason: [u64; DropReason::ALL.len()],
}

impl QueueSnapshot {
    /// Sum of every bounded drop reason.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped_by_reason.iter().copied().sum()
    }

    /// Drops recorded for a single closed-set reason.
    #[must_use]
    pub fn dropped(&self, reason: DropReason) -> u64 {
        self.dropped_by_reason[reason.as_id()]
    }

    /// Prometheus text exposition of this snapshot, including every bounded
    /// label so cardinality is visible and overflow is greppable.
    #[must_use]
    pub fn prometheus_exposition(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP sensory_queue_depth Current outbound sensory queue depth\n");
        out.push_str("# TYPE sensory_queue_depth gauge\n");
        out.push_str(&format!("sensory_queue_depth {}\n", self.depth));
        out.push_str("# HELP sensory_queue_capacity Configured outbound sensory queue capacity\n");
        out.push_str("# TYPE sensory_queue_capacity gauge\n");
        out.push_str(&format!("sensory_queue_capacity {}\n", self.capacity));
        out.push_str(
            "# HELP sensory_queue_enqueued_total Sensory frames accepted into the outbound queue\n",
        );
        out.push_str("# TYPE sensory_queue_enqueued_total counter\n");
        out.push_str(&format!(
            "sensory_queue_enqueued_total {}\n",
            self.enqueued_total
        ));
        out.push_str(
            "# HELP sensory_queue_dropped_total Sensory frames dropped (bounded reason label)\n",
        );
        out.push_str("# TYPE sensory_queue_dropped_total counter\n");
        for reason in DropReason::ALL {
            out.push_str(&format!(
                "sensory_queue_dropped_total{{reason=\"{}\"}} {}\n",
                reason.as_str(),
                self.dropped(reason)
            ));
        }
        out.push_str("# HELP sensory_queue_full_policy Configured full-queue policy (one-hot)\n");
        out.push_str("# TYPE sensory_queue_full_policy gauge\n");
        for policy in QueueFullPolicy::ALL {
            let v = if policy == self.policy { 1 } else { 0 };
            out.push_str(&format!(
                "sensory_queue_full_policy{{policy=\"{}\"}} {v}\n",
                policy.as_str()
            ));
        }
        out
    }
}

#[derive(Debug)]
struct QueueInner {
    buf: VecDeque<SensoryMapping>,
    capacity: usize,
    policy: QueueFullPolicy,
    connected: bool,
    enqueued_total: u64,
    dropped_by_reason: [u64; DropReason::ALL.len()],
}

impl QueueInner {
    fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            capacity: self.capacity,
            policy: self.policy,
            depth: self.buf.len(),
            enqueued_total: self.enqueued_total,
            dropped_by_reason: self.dropped_by_reason,
        }
    }

    fn record_drop(&mut self, reason: DropReason) {
        let slot = &mut self.dropped_by_reason[reason.as_id()];
        *slot = slot.saturating_add(1);
        record_drop(reason);
    }

    fn record_enqueue(&mut self) {
        self.enqueued_total = self.enqueued_total.saturating_add(1);
        counter!("sensory_queue_enqueued_total").increment(1);
    }

    fn emit_gauges(&self) {
        export_queue_gauges(self.buf.len(), self.capacity, self.policy);
    }
}

/// Bounded non-blocking enqueue with an explicit full-queue policy.
///
/// The safety supervisor uses [`Self::try_enqueue`] (never wait). A slow or
/// missing consumer cannot stall evaluation. Depth is always `<= capacity`.
#[derive(Debug, Clone)]
pub struct IsolatedPublishQueue {
    inner: Arc<Mutex<QueueInner>>,
}

/// Receiving end of [`IsolatedPublishQueue`]. Dropping it marks the queue
/// disconnected; remaining frames are counted as [`DropReason::Disconnected`].
#[derive(Debug)]
pub struct SensoryQueueConsumer {
    inner: Arc<Mutex<QueueInner>>,
}

impl IsolatedPublishQueue {
    /// Construct a validated bounded queue plus its consumer.
    ///
    /// Re-validates `config` so an in-module struct literal cannot bypass
    /// [`QueueConfig::new`].
    pub fn new(config: QueueConfig) -> Result<(Self, SensoryQueueConsumer), QueueConfigError> {
        QueueConfig::validate_capacity(config.capacity())?;
        let inner = Arc::new(Mutex::new(QueueInner {
            buf: VecDeque::with_capacity(config.capacity()),
            capacity: config.capacity(),
            policy: config.policy(),
            connected: true,
            enqueued_total: 0,
            dropped_by_reason: [0; DropReason::ALL.len()],
        }));
        export_queue_gauges(0, config.capacity(), config.policy());
        register_drop_reason_series();
        counter!("sensory_queue_enqueued_total").increment(0);
        Ok((
            Self {
                inner: Arc::clone(&inner),
            },
            SensoryQueueConsumer { inner },
        ))
    }

    /// Bounded queue with [`QueueFullPolicy::RejectNewest`] (legacy `try_send`).
    pub fn bounded(capacity: usize) -> Result<(Self, SensoryQueueConsumer), QueueConfigError> {
        Self::new(QueueConfig::new(capacity, QueueFullPolicy::RejectNewest)?)
    }

    /// Bounded queue with an explicit full-queue policy.
    pub fn bounded_with_policy(
        capacity: usize,
        policy: QueueFullPolicy,
    ) -> Result<(Self, SensoryQueueConsumer), QueueConfigError> {
        Self::new(QueueConfig::new(capacity, policy)?)
    }

    /// Non-blocking enqueue. Never waits on a consumer.
    pub fn try_enqueue(&self, mapping: SensoryMapping) -> Result<(), PublishError> {
        let mut inner = self.inner.lock().expect("sensory queue mutex poisoned");
        if !inner.connected {
            inner.record_drop(DropReason::Disconnected);
            inner.emit_gauges();
            return Err(PublishError::Disconnected);
        }
        if inner.buf.len() < inner.capacity {
            inner.buf.push_back(mapping);
            inner.record_enqueue();
            inner.emit_gauges();
            return Ok(());
        }
        match inner.policy {
            QueueFullPolicy::RejectNewest => {
                inner.record_drop(DropReason::RejectNewest);
                inner.emit_gauges();
                Err(PublishError::SlowConsumer)
            }
            QueueFullPolicy::DropOldest => {
                let _oldest = inner.buf.pop_front();
                inner.record_drop(DropReason::DropOldest);
                inner.buf.push_back(mapping);
                inner.record_enqueue();
                inner.emit_gauges();
                Ok(())
            }
        }
    }

    /// Current depth, capacity, policy, and counters.
    #[must_use]
    pub fn snapshot(&self) -> QueueSnapshot {
        self.inner
            .lock()
            .expect("sensory queue mutex poisoned")
            .snapshot()
    }
}

impl SensoryPublisher for IsolatedPublishQueue {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError> {
        self.try_enqueue(mapping.clone())
    }
}

impl SensoryQueueConsumer {
    /// Non-blocking dequeue. `None` if the queue is empty.
    pub fn try_recv(&self) -> Option<SensoryMapping> {
        let mut inner = self.inner.lock().expect("sensory queue mutex poisoned");
        let item = inner.buf.pop_front();
        inner.emit_gauges();
        item
    }
}

impl Drop for SensoryQueueConsumer {
    fn drop(&mut self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.connected = false;
        let leftover = inner.buf.len();
        inner.buf.clear();
        if leftover > 0 {
            let slot = &mut inner.dropped_by_reason[DropReason::Disconnected.as_id()];
            *slot = slot.saturating_add(leftover as u64);
        }
        inner.emit_gauges();
        drop(inner);
        for _ in 0..leftover {
            record_drop(DropReason::Disconnected);
        }
    }
}

fn record_drop(reason: DropReason) {
    counter!("sensory_queue_dropped_total", "reason" => reason.as_str()).increment(1);
}

fn register_drop_reason_series() {
    for reason in DropReason::ALL {
        counter!("sensory_queue_dropped_total", "reason" => reason.as_str()).increment(0);
    }
}

fn export_queue_gauges(depth: usize, capacity: usize, policy: QueueFullPolicy) {
    gauge!("sensory_queue_depth").set(depth as f64);
    gauge!("sensory_queue_capacity").set(capacity as f64);
    for p in QueueFullPolicy::ALL {
        gauge!("sensory_queue_full_policy", "policy" => p.as_str()).set(if p == policy {
            1.0
        } else {
            0.0
        });
    }
}

/// Evaluate safety first, then attempt publish. Publish cannot change the snapshot.
pub fn evaluate_then_try_publish<P: SensoryPublisher + ?Sized>(
    machine: &mut SafetyMachine,
    frame: &TelemetryFrame,
    publisher: &P,
) -> (SafetySnapshot, Result<(), PublishError>) {
    let snapshot = machine.evaluate(frame);
    let publish_result = publisher.try_publish(&frame.to_sensory_mapping());
    (snapshot, publish_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{BrakeIntent, SafetyState};
    use crate::telemetry::{assess, fixtures};
    use std::collections::BTreeSet;

    fn critical_frame() -> TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(90.0);
        assess(&raw, fixtures::NOW)
    }

    fn healthy_frame() -> TelemetryFrame {
        assess(&fixtures::healthy_real(), fixtures::NOW)
    }

    fn mapping_at(ts: u64) -> SensoryMapping {
        let mut mapping = healthy_frame().to_sensory_mapping();
        mapping.observed_at_unix_ms = ts;
        mapping
    }

    #[test]
    fn absent_brainstem_does_not_block_or_change_safety() {
        let mut machine = SafetyMachine::new();
        let (snap, pub_res) =
            evaluate_then_try_publish(&mut machine, &critical_frame(), &AbsentPublisher);
        assert_eq!(pub_res, Err(PublishError::Absent));
        assert_eq!(snap.state, SafetyState::CriticalBraked);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn failing_publisher_cannot_stall_or_disable_safety() {
        let mut machine = SafetyMachine::new();
        let publisher = FailingPublisher::send_failed();
        for _ in 0..32 {
            let (snap, pub_res) =
                evaluate_then_try_publish(&mut machine, &critical_frame(), &publisher);
            assert!(pub_res.is_err());
            assert_eq!(snap.policy_state, SafetyState::CriticalBraked);
            assert_eq!(snap.intent, BrakeIntent::Apply);
        }

        let (healthy, _) = evaluate_then_try_publish(&mut machine, &healthy_frame(), &publisher);
        assert_eq!(healthy.state, SafetyState::HealthyReal);
    }

    #[test]
    fn slow_consumer_try_enqueue_returns_immediately() {
        let (queue, _rx) = IsolatedPublishQueue::bounded(1).unwrap();
        let mapping = healthy_frame().to_sensory_mapping();
        queue.try_enqueue(mapping.clone()).unwrap();

        let err = queue.try_enqueue(mapping).unwrap_err();
        assert_eq!(err, PublishError::SlowConsumer);

        let mut machine = SafetyMachine::new();
        let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &critical_frame(), &queue);
        assert_eq!(pub_res, Err(PublishError::SlowConsumer));
        assert_eq!(snap.state, SafetyState::CriticalBraked);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn disconnected_queue_does_not_block_safety() {
        let (queue, rx) = IsolatedPublishQueue::bounded(1).unwrap();
        drop(rx);
        let mut machine = SafetyMachine::new();
        let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &critical_frame(), &queue);
        assert_eq!(pub_res, Err(PublishError::Disconnected));
        assert_eq!(snap.state, SafetyState::CriticalBraked);
    }

    #[test]
    fn evaluate_ignores_publish_error_ordering() {
        let mut machine = SafetyMachine::new();
        let missing = assess(&fixtures::nvml_unavailable(), fixtures::NOW);
        let (snap, pub_res) =
            evaluate_then_try_publish(&mut machine, &missing, &FailingPublisher::disconnected());
        assert!(pub_res.is_err());
        assert_eq!(snap.policy_state, SafetyState::TelemetryMissing);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn queue_drop_consumer_counts_leftover_as_disconnected() {
        let (queue, rx) =
            IsolatedPublishQueue::bounded_with_policy(3, QueueFullPolicy::RejectNewest).unwrap();
        queue.try_enqueue(mapping_at(1)).unwrap();
        queue.try_enqueue(mapping_at(2)).unwrap();
        drop(rx);
        let snap = queue.snapshot();
        assert_eq!(snap.depth, 0);
        assert_eq!(snap.dropped(DropReason::Disconnected), 2);
        assert_eq!(
            queue.try_enqueue(mapping_at(3)),
            Err(PublishError::Disconnected)
        );
    }

    #[test]
    fn queue_capacity_is_validated() {
        assert!(matches!(
            QueueConfig::new(0, QueueFullPolicy::DropOldest),
            Err(QueueConfigError::CapacityTooSmall {
                capacity: 0,
                min: 1
            })
        ));
        assert!(matches!(
            IsolatedPublishQueue::bounded(0),
            Err(QueueConfigError::CapacityTooSmall { .. })
        ));
        let over = QueueConfig::MAX_CAPACITY + 1;
        assert!(matches!(
            QueueConfig::new(over, QueueFullPolicy::RejectNewest),
            Err(QueueConfigError::CapacityTooLarge {
                max: QueueConfig::MAX_CAPACITY,
                ..
            })
        ));
        let ok = QueueConfig::new(8, QueueFullPolicy::DropOldest).unwrap();
        assert_eq!(ok.capacity(), 8);
        assert_eq!(ok.policy(), QueueFullPolicy::DropOldest);
        let invalid = QueueConfig {
            capacity: 0,
            policy: QueueFullPolicy::DropOldest,
        };
        assert!(matches!(
            IsolatedPublishQueue::new(invalid),
            Err(QueueConfigError::CapacityTooSmall { capacity: 0, .. })
        ));
    }

    #[test]
    fn queue_depth_never_exceeds_capacity_reject_newest() {
        let cap = 4;
        let (queue, _rx) =
            IsolatedPublishQueue::bounded_with_policy(cap, QueueFullPolicy::RejectNewest).unwrap();
        for i in 0..(cap * 3) {
            let _ = queue.try_enqueue(mapping_at(i as u64));
            let snap = queue.snapshot();
            assert!(
                snap.depth <= cap,
                "depth {} exceeded capacity {cap}",
                snap.depth
            );
        }
        let snap = queue.snapshot();
        assert_eq!(snap.depth, cap);
        assert_eq!(snap.capacity, cap);
        assert_eq!(snap.enqueued_total, cap as u64);
        assert_eq!(snap.dropped(DropReason::RejectNewest), (cap * 2) as u64);
        assert_eq!(snap.dropped_total(), (cap * 2) as u64);
    }

    #[test]
    fn queue_depth_never_exceeds_capacity_drop_oldest() {
        let cap = 3;
        let (queue, rx) =
            IsolatedPublishQueue::bounded_with_policy(cap, QueueFullPolicy::DropOldest).unwrap();
        for i in 0..(cap + 5) {
            queue.try_enqueue(mapping_at(1_000 + i as u64)).unwrap();
            assert!(queue.snapshot().depth <= cap);
        }
        let snap = queue.snapshot();
        assert_eq!(snap.depth, cap);
        assert_eq!(snap.enqueued_total, (cap + 5) as u64);
        assert_eq!(snap.dropped(DropReason::DropOldest), 5);
        assert_eq!(snap.dropped(DropReason::RejectNewest), 0);

        let mut kept = Vec::new();
        while let Some(m) = rx.try_recv() {
            kept.push(m.observed_at_unix_ms);
        }
        assert_eq!(kept, vec![1_005, 1_006, 1_007]);
        assert_eq!(queue.snapshot().depth, 0);
    }

    #[test]
    fn queue_reject_newest_counters_match_induced_pressure() {
        let (queue, _rx) =
            IsolatedPublishQueue::bounded_with_policy(2, QueueFullPolicy::RejectNewest).unwrap();
        assert!(queue.try_enqueue(mapping_at(1)).is_ok());
        assert!(queue.try_enqueue(mapping_at(2)).is_ok());
        assert_eq!(
            queue.try_enqueue(mapping_at(3)),
            Err(PublishError::SlowConsumer)
        );
        assert_eq!(
            queue.try_enqueue(mapping_at(4)),
            Err(PublishError::SlowConsumer)
        );
        let snap = queue.snapshot();
        assert_eq!(snap.depth, 2);
        assert_eq!(snap.enqueued_total, 2);
        assert_eq!(snap.dropped(DropReason::RejectNewest), 2);
        assert_eq!(snap.policy, QueueFullPolicy::RejectNewest);
    }

    #[test]
    fn queue_drop_oldest_counters_match_induced_pressure() {
        let (queue, rx) =
            IsolatedPublishQueue::bounded_with_policy(2, QueueFullPolicy::DropOldest).unwrap();
        queue.try_enqueue(mapping_at(10)).unwrap();
        queue.try_enqueue(mapping_at(20)).unwrap();
        queue.try_enqueue(mapping_at(30)).unwrap();
        let snap = queue.snapshot();
        assert_eq!(snap.depth, 2);
        assert_eq!(snap.enqueued_total, 3);
        assert_eq!(snap.dropped(DropReason::DropOldest), 1);
        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        assert_eq!(first.observed_at_unix_ms, 20);
        assert_eq!(second.observed_at_unix_ms, 30);
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn queue_metrics_exposition_shows_forced_overflow() {
        let (queue, _rx) =
            IsolatedPublishQueue::bounded_with_policy(2, QueueFullPolicy::RejectNewest).unwrap();
        queue.try_enqueue(mapping_at(1)).unwrap();
        queue.try_enqueue(mapping_at(2)).unwrap();
        assert_eq!(
            queue.try_enqueue(mapping_at(3)).unwrap_err(),
            PublishError::SlowConsumer
        );
        let text = queue.snapshot().prometheus_exposition();
        assert!(
            text.contains("sensory_queue_depth 2"),
            "missing depth in exposition:\n{text}"
        );
        assert!(text.contains("sensory_queue_capacity 2"));
        assert!(text.contains("sensory_queue_enqueued_total 2"));
        assert!(text.contains("sensory_queue_dropped_total{reason=\"reject_newest\"} 1"));
        assert!(text.contains("sensory_queue_dropped_total{reason=\"drop_oldest\"} 0"));
        assert!(text.contains("sensory_queue_dropped_total{reason=\"absent\"} 0"));
        assert!(text.contains("sensory_queue_dropped_total{reason=\"disconnected\"} 0"));
        assert!(text.contains("sensory_queue_dropped_total{reason=\"send_failed\"} 0"));
        assert!(text.contains("sensory_queue_full_policy{policy=\"reject_newest\"} 1"));
        assert!(text.contains("sensory_queue_full_policy{policy=\"drop_oldest\"} 0"));
        for reason in DropReason::ALL {
            assert!(
                text.contains(&format!("reason=\"{}\"", reason.as_str())),
                "unbounded or missing reason {}",
                reason.as_str()
            );
        }
    }

    #[test]
    fn queue_drop_reason_labels_are_bounded() {
        let labels: Vec<_> = DropReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "reject_newest",
                "drop_oldest",
                "absent",
                "disconnected",
                "send_failed"
            ]
        );
        let unique: BTreeSet<_> = labels.iter().copied().collect();
        assert_eq!(unique.len(), DropReason::ALL.len());
        for (i, reason) in DropReason::ALL.iter().enumerate() {
            assert_eq!(reason.as_id(), i);
        }
        let failing = FailingPublisher {
            reason: format!("unique-error-{}", 0xDEAD_BEEFu32),
        };
        let _ = failing.try_publish(&mapping_at(1));
        assert_eq!(DropReason::SendFailed.as_str(), "send_failed");
    }

    #[test]
    fn queue_safety_evaluation_continues_when_consumer_stalled() {
        let (queue, _rx) =
            IsolatedPublishQueue::bounded_with_policy(1, QueueFullPolicy::DropOldest).unwrap();
        let mut machine = SafetyMachine::new();
        for _ in 0..16 {
            let (snap, pub_res) =
                evaluate_then_try_publish(&mut machine, &critical_frame(), &queue);
            assert!(pub_res.is_ok(), "drop-oldest admits the newest frame");
            assert_eq!(snap.state, SafetyState::CriticalBraked);
            assert_eq!(snap.intent, BrakeIntent::Apply);
            assert!(queue.snapshot().depth <= 1);
        }
        let snap = queue.snapshot();
        assert_eq!(snap.depth, 1);
        assert_eq!(snap.enqueued_total, 16);
        assert_eq!(snap.dropped(DropReason::DropOldest), 15);
    }

    #[test]
    fn queue_full_policy_from_str_and_display() {
        assert_eq!(
            "drop-oldest".parse::<QueueFullPolicy>().unwrap(),
            QueueFullPolicy::DropOldest
        );
        assert_eq!(
            "reject_newest".parse::<QueueFullPolicy>().unwrap(),
            QueueFullPolicy::RejectNewest
        );
        assert!("bogus".parse::<QueueFullPolicy>().is_err());
        assert_eq!(QueueFullPolicy::DropOldest.to_string(), "drop-oldest");
    }
}
