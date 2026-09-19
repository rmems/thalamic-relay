//! Best-effort sensory publication, isolated from the safety failure domain.
//!
//! Maps a [`SensoryMapping`] into the canonical `corpus-ipc` wire type
//! [`StimulusBatch`] and publishes [`IpcMessage::Stimuli`] outside
//! [`crate::safety::SafetyMachine::evaluate`]. Implementations of
//! [`SensoryPublisher::try_publish`] must return promptly: the supervisor
//! never awaits a consumer and never calls into this module from inside
//! [`crate::safety::SafetyMachine::evaluate`].
//!
//! The outbound queue is explicitly bounded. Full-queue behavior is a
//! documented [`QueueFullPolicy`]; overflow and other loss reasons are
//! counted with a closed [`DropReason`] label set so Prometheus cardinality
//! cannot grow from payload or error strings.
//!
//! Production uses [`CorpusIpcPublisher`]: a bounded enqueue into a
//! dedicated worker that serializes `IpcMessage` JSON and fire-and-forget
//! UDP-sends it. Send failures, disconnects, slow consumers, and Brainstem
//! absence cannot stall or disable hardware-safety evaluation.

use crate::safety::{SafetyMachine, SafetySnapshot};
use crate::telemetry::{
    SampleValidity, SensoryMapping, TelemetryFrame, TelemetrySource, UnixMillis,
};
use corpus_ipc::{BatchMetadata, IpcMessage, StimulusBatch, Validate};
use metrics::{counter, gauge};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{SocketAddr, UdpSocket};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// Canonical `corpus-ipc` identity stamped into [`BatchMetadata::source`].
pub const SOURCE_IDENTITY: &str = "thalamic-relay";
/// Default UDP destination for [`IpcMessage::Stimuli`] datagrams.
pub const DEFAULT_IPC_ENDPOINT: &str = "127.0.0.1:9900";
/// Default [`StimulusBatch::session_id`] when `--ipc-session-id` is unset.
pub const DEFAULT_IPC_SESSION_ID: &str = "thalamic-relay";

/// Why a best-effort publish did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// No Brainstem / corpus-ipc transport is configured.
    Absent,
    /// The publisher worker is gone.
    Disconnected,
    /// Bounded queue is full and the policy rejected the incoming frame.
    SlowConsumer,
    /// The transport returned a send failure (or the batch failed validation).
    SendFailed(String),
}

/// Best-effort sensory publisher. Must not block the safety loop.
///
/// There is currently no `corpus-ipc` implementation of this trait in-tree.
/// Production uses [`AbsentPublisher`].
pub trait SensoryPublisher: Send + Sync {
    /// Attempt to publish `mapping` without waiting on a consumer.
    ///
    /// Must return promptly. A full queue, absent transport, or send failure
    /// is reported as [`PublishError`]; it must not stall [`crate::safety::SafetyMachine::evaluate`].
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
    /// Reason string returned as [`PublishError::SendFailed`].
    pub reason: String,
}

impl FailingPublisher {
    /// Publisher that reports a generic send failure.
    #[must_use]
    pub fn send_failed() -> Self {
        Self {
            reason: "ipc send failed".to_string(),
        }
    }

    /// Publisher that reports a disconnected worker.
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
    CapacityTooSmall {
        /// Rejected capacity value.
        capacity: usize,
        /// Minimum accepted capacity ([`QueueConfig::MIN_CAPACITY`]).
        min: usize,
    },
    /// Capacity exceeded the documented maximum.
    CapacityTooLarge {
        /// Rejected capacity value.
        capacity: usize,
        /// Maximum accepted capacity ([`QueueConfig::MAX_CAPACITY`]).
        max: usize,
    },
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
    /// Configured maximum frames retained.
    pub capacity: usize,
    /// Full-queue policy in effect when the snapshot was taken.
    pub policy: QueueFullPolicy,
    /// Frames currently buffered (`<= capacity`).
    pub depth: usize,
    /// Frames accepted into the queue over its lifetime.
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
struct QueueInner<T> {
    buf: VecDeque<T>,
    capacity: usize,
    policy: QueueFullPolicy,
    connected: bool,
    producers: usize,
    enqueued_total: u64,
    dropped_by_reason: [u64; DropReason::ALL.len()],
}

impl<T> QueueInner<T> {
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
/// Dropping the last producer lets a drain worker exit after remaining frames.
#[derive(Debug)]
pub struct IsolatedPublishQueue<T> {
    inner: Arc<Mutex<QueueInner<T>>>,
}

impl<T> Clone for IsolatedPublishQueue<T> {
    fn clone(&self) -> Self {
        let mut inner = self.inner.lock().expect("sensory queue mutex poisoned");
        inner.producers = inner.producers.saturating_add(1);
        drop(inner);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for IsolatedPublishQueue<T> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.producers = inner.producers.saturating_sub(1);
        }
    }
}

/// Receiving end of [`IsolatedPublishQueue`]. Dropping it marks the queue
/// disconnected; remaining frames are counted as [`DropReason::Disconnected`].
#[derive(Debug)]
pub struct SensoryQueueConsumer<T> {
    inner: Arc<Mutex<QueueInner<T>>>,
}

impl<T: Send> IsolatedPublishQueue<T> {
    /// Construct a validated bounded queue plus its consumer.
    ///
    /// Re-validates `config` so an in-module struct literal cannot bypass
    /// [`QueueConfig::new`].
    pub fn new(config: QueueConfig) -> Result<(Self, SensoryQueueConsumer<T>), QueueConfigError> {
        QueueConfig::validate_capacity(config.capacity())?;
        let inner = Arc::new(Mutex::new(QueueInner {
            buf: VecDeque::with_capacity(config.capacity()),
            capacity: config.capacity(),
            policy: config.policy(),
            connected: true,
            producers: 1,
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
    pub fn bounded(capacity: usize) -> Result<(Self, SensoryQueueConsumer<T>), QueueConfigError> {
        Self::new(QueueConfig::new(capacity, QueueFullPolicy::RejectNewest)?)
    }

    /// Bounded queue with an explicit full-queue policy.
    pub fn bounded_with_policy(
        capacity: usize,
        policy: QueueFullPolicy,
    ) -> Result<(Self, SensoryQueueConsumer<T>), QueueConfigError> {
        Self::new(QueueConfig::new(capacity, policy)?)
    }

    /// Non-blocking enqueue. Never waits on a consumer.
    pub fn try_enqueue(&self, item: T) -> Result<(), PublishError> {
        let mut inner = self.inner.lock().expect("sensory queue mutex poisoned");
        if !inner.connected {
            inner.record_drop(DropReason::Disconnected);
            inner.emit_gauges();
            return Err(PublishError::Disconnected);
        }
        if inner.buf.len() < inner.capacity {
            inner.buf.push_back(item);
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
                inner.buf.push_back(item);
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

impl SensoryPublisher for IsolatedPublishQueue<SensoryMapping> {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError> {
        self.try_enqueue(mapping.clone())
    }
}

impl<T> SensoryQueueConsumer<T> {
    /// Non-blocking dequeue. `None` if the queue is empty.
    pub fn try_recv(&self) -> Option<T> {
        let mut inner = self.inner.lock().expect("sensory queue mutex poisoned");
        let item = inner.buf.pop_front();
        inner.emit_gauges();
        item
    }

    fn producers_gone(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.producers == 0)
            .unwrap_or(true)
    }

    fn record_loss(&self, reason: DropReason) {
        let Ok(mut inner) = self.inner.lock() else {
            record_drop(reason);
            return;
        };
        inner.record_drop(reason);
        inner.emit_gauges();
    }
}

impl<T> Drop for SensoryQueueConsumer<T> {
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

/// Production publisher: convert → bounded enqueue → worker encodes/sends.
///
/// [`Self::try_publish`] never waits on UDP, JSON encoding, or Brainstem.
/// The worker thread owns the socket and the receiving end of the queue.
#[derive(Debug, Clone)]
pub struct CorpusIpcPublisher {
    queue: IsolatedPublishQueue<IpcMessage>,
    session_id: Option<String>,
    batch_id: Arc<AtomicU64>,
}

impl CorpusIpcPublisher {
    /// Start a detached UDP worker sending to `endpoint`.
    ///
    /// Binding the local socket is the only fallible step. After this returns,
    /// [`Self::try_publish`] is non-blocking (bounded enqueue with
    /// [`QueueConfig`] policy; overflow is counted, never awaited).
    pub fn spawn(
        endpoint: SocketAddr,
        session_id: Option<String>,
        config: QueueConfig,
    ) -> std::io::Result<Self> {
        let (queue, consumer) = IsolatedPublishQueue::new(config).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid sensory queue config: {err}"),
            )
        })?;
        let socket = UdpSocket::bind(local_bind_for(endpoint))?;
        socket.set_nonblocking(true)?;
        thread::Builder::new()
            .name("corpus-ipc-publish".to_string())
            .spawn(move || run_udp_worker(consumer, socket, endpoint))?;
        Ok(Self {
            queue,
            session_id,
            batch_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Enqueue-only publisher (no socket). For tests that inspect `IpcMessage`.
    pub fn channel(
        session_id: Option<String>,
        config: QueueConfig,
    ) -> Result<(Self, SensoryQueueConsumer<IpcMessage>), QueueConfigError> {
        let (queue, consumer) = IsolatedPublishQueue::new(config)?;
        Ok((
            Self {
                queue,
                session_id,
                batch_id: Arc::new(AtomicU64::new(1)),
            },
            consumer,
        ))
    }
}

impl SensoryPublisher for CorpusIpcPublisher {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError> {
        let batch_id = self.batch_id.fetch_add(1, Ordering::Relaxed);
        let batch = mapping_to_stimulus_batch(mapping, self.session_id.clone(), batch_id);
        if let Err(err) = batch.validate() {
            record_drop(DropReason::SendFailed);
            return Err(PublishError::SendFailed(err.to_string()));
        }
        self.queue.try_enqueue(IpcMessage::Stimuli(batch))
    }
}

fn local_bind_for(dest: SocketAddr) -> SocketAddr {
    match dest {
        SocketAddr::V4(_) => SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
    }
}

fn run_udp_worker(consumer: SensoryQueueConsumer<IpcMessage>, socket: UdpSocket, dest: SocketAddr) {
    loop {
        match consumer.try_recv() {
            Some(msg) => send_dequeued_message(&consumer, &socket, dest, &msg),
            None if consumer.producers_gone() => break,
            None => thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn send_dequeued_message(
    consumer: &SensoryQueueConsumer<IpcMessage>,
    socket: &UdpSocket,
    dest: SocketAddr,
    msg: &IpcMessage,
) {
    let Ok(bytes) = serde_json::to_vec(msg) else {
        tracing::debug!("corpus-ipc: failed to serialize IpcMessage; dropping frame");
        consumer.record_loss(DropReason::SendFailed);
        return;
    };
    if let Err(err) = socket.send_to(&bytes, dest) {
        tracing::debug!("corpus-ipc: UDP send failed ({err}); dropping frame");
        consumer.record_loss(DropReason::SendFailed);
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

/// Map an internal sensory mapping into the canonical [`StimulusBatch`].
///
/// Channel order is the runtime-input inventory order already present in
/// [`SensoryMapping::stimuli`] (no filler observability channels).
/// [`SampleValidity::Valid`] channels carry the normalized `[0, 1]` value
/// with `valid_mask[i] = true`. Missing / invalid / stale channels use the
/// corpus-ipc placeholder `0.0` and `valid_mask[i] = false` so a real zero
/// (valid, normalized `0.0`) is distinct from "no data this tick".
///
/// Timestamp is unix nanoseconds (millis × 1_000_000). Provenance required
/// by GH#41 lives in [`BatchMetadata`]: source identity, acquisition source,
/// cadence, channel names, per-channel validity, and stale thresholds.
#[must_use]
pub fn mapping_to_stimulus_batch(
    mapping: &SensoryMapping,
    session_id: Option<String>,
    batch_id: u64,
) -> StimulusBatch {
    let mut values = Vec::with_capacity(mapping.stimuli.len());
    let mut valid_mask = Vec::with_capacity(mapping.stimuli.len());
    let mut channels = Vec::with_capacity(mapping.stimuli.len());
    let mut validity = Vec::with_capacity(mapping.stimuli.len());
    let mut stale_after = Vec::with_capacity(mapping.stimuli.len());

    for stimulus in &mapping.stimuli {
        channels.push(stimulus.name.clone());
        validity.push(validity_name(stimulus.validity).to_string());
        stale_after.push(stimulus.stale_after_ms.to_string());
        match (stimulus.validity, stimulus.normalized) {
            (SampleValidity::Valid, Some(value)) => {
                values.push(value);
                valid_mask.push(true);
            }
            _ => {
                values.push(0.0);
                valid_mask.push(false);
            }
        }
    }

    let mut custom = HashMap::new();
    custom.insert(
        "acquisition_source".to_string(),
        source_name(mapping.acquisition_source).to_string(),
    );
    custom.insert(
        "acquisition_cadence_ms".to_string(),
        mapping.acquisition_cadence_ms.to_string(),
    );
    custom.insert("channels".to_string(), channels.join(","));
    custom.insert("validity".to_string(), validity.join(","));
    custom.insert("stale_after_ms".to_string(), stale_after.join(","));

    StimulusBatch {
        session_id,
        batch_id,
        timestamp: unix_ms_to_ns(mapping.emitted_at_unix_ms),
        values,
        valid_mask: Some(valid_mask),
        metadata: Some(BatchMetadata {
            processing_latency_ns: None,
            source: Some(SOURCE_IDENTITY.to_string()),
            custom,
        }),
    }
}

/// Wrap a mapping as the canonical wire envelope.
#[must_use]
pub fn mapping_to_ipc_message(
    mapping: &SensoryMapping,
    session_id: Option<String>,
    batch_id: u64,
) -> IpcMessage {
    IpcMessage::Stimuli(mapping_to_stimulus_batch(mapping, session_id, batch_id))
}

/// Convert unix milliseconds to nanoseconds.
#[must_use]
pub fn unix_ms_to_ns(unix_ms: UnixMillis) -> u64 {
    unix_ms.saturating_mul(1_000_000)
}

/// Convert [`TelemetrySource`] to string representation for metadata.
#[must_use]
pub const fn source_name(source: TelemetrySource) -> &'static str {
    match source {
        TelemetrySource::Nvml => "nvml",
        TelemetrySource::SoftwareFallback => "software_fallback",
        TelemetrySource::NvmlUnavailable => "nvml_unavailable",
    }
}

/// Convert [`SampleValidity`] to string representation for metadata.
#[must_use]
pub const fn validity_name(validity: SampleValidity) -> &'static str {
    match validity {
        SampleValidity::Valid => "valid",
        SampleValidity::Missing => "missing",
        SampleValidity::Invalid => "invalid",
        SampleValidity::Stale => "stale",
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
    use crate::gpu::HardwareBridge;
    use crate::safety::{BrakeIntent, SafetyState};
    use crate::telemetry::{SignalId, assess, fixtures};
    use std::collections::BTreeSet;
    use std::net::UdpSocket;

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
        mapping.emitted_at_unix_ms = ts;
        mapping
    }

    fn mapping_at_now(frame: &TelemetryFrame) -> SensoryMapping {
        frame.to_sensory_mapping_at(fixtures::NOW)
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
        let (queue, _rx) =
            IsolatedPublishQueue::bounded_with_policy(1, QueueFullPolicy::RejectNewest).unwrap();
        let mapping = mapping_at_now(&healthy_frame());
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
    fn mapping_to_stimulus_batch_carries_timestamp_source_and_validity() {
        let mapping = mapping_at_now(&healthy_frame());
        let batch = mapping_to_stimulus_batch(&mapping, Some("sess-relay".into()), 7);
        batch.validate().expect("mask length must match values");

        assert_eq!(batch.session_id.as_deref(), Some("sess-relay"));
        assert_eq!(batch.batch_id, 7);
        assert_eq!(batch.timestamp, unix_ms_to_ns(fixtures::NOW));
        assert_eq!(
            batch.values.len(),
            4,
            "runtime-input channels only (no observability filler)"
        );
        assert_eq!(
            batch.valid_mask.as_deref(),
            Some([true, true, true, true].as_slice())
        );
        assert!((batch.values[0] - 0.65).abs() < 1e-6);
        assert!((batch.values[1] - 200.0 / 350.0).abs() < 1e-6);
        assert_eq!(batch.values[3], 0.0, "legitimate idle util is a real zero");

        let meta = batch.metadata.expect("provenance metadata required");
        assert_eq!(meta.source.as_deref(), Some(SOURCE_IDENTITY));
        assert_eq!(
            meta.custom.get("acquisition_source").map(String::as_str),
            Some("nvml")
        );
        assert_eq!(
            meta.custom.get("channels").map(String::as_str),
            Some("gpu_temp_c,power_w,gpu_clock_mhz,mem_util_pct")
        );
        assert_eq!(
            meta.custom.get("validity").map(String::as_str),
            Some("valid,valid,valid,valid")
        );
        assert_eq!(
            meta.custom
                .get("acquisition_cadence_ms")
                .map(String::as_str),
            Some("100")
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
            IsolatedPublishQueue::<SensoryMapping>::bounded(0),
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
            IsolatedPublishQueue::<SensoryMapping>::new(invalid),
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
            kept.push(m.emitted_at_unix_ms);
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
        assert_eq!(first.emitted_at_unix_ms, 20);
        assert_eq!(second.emitted_at_unix_ms, 30);
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

    #[test]
    fn missing_channel_is_masked_not_a_real_zero() {
        let mapping = mapping_at_now(&assess(&fixtures::sensor_dropout(), fixtures::NOW));
        let batch = mapping_to_stimulus_batch(&mapping, None, 1);
        let mask = batch.valid_mask.expect("mask present");
        let power = mapping
            .stimuli
            .iter()
            .position(|s| s.signal == SignalId::PowerW)
            .unwrap();
        let util = mapping
            .stimuli
            .iter()
            .position(|s| s.signal == SignalId::MemUtilPct)
            .unwrap();
        assert!(!mask[power], "missing power is not a real reading");
        assert_eq!(batch.values[power], 0.0);
        assert!(mask[util], "valid mem_util_pct=0.0 stays valid");
        assert_eq!(batch.values[util], 0.0);
        assert_eq!(
            batch
                .metadata
                .as_ref()
                .and_then(|m| m.custom.get("validity"))
                .map(String::as_str),
            Some("valid,missing,valid,valid")
        );
    }

    #[test]
    fn stimulus_batch_round_trips_through_published_corpus_ipc_types() {
        let mapping = mapping_at_now(&healthy_frame());
        let message = mapping_to_ipc_message(&mapping, Some("sess-1".into()), 42);
        let json = serde_json::to_value(&message).expect("serialize IpcMessage");
        assert!(
            json.get("Stimuli").is_some(),
            "wire envelope must be IpcMessage::Stimuli, not a local schema"
        );
        let stimuli = json.get("Stimuli").unwrap();
        assert!(stimuli.get("session_id").is_some());
        assert!(stimuli.get("batch_id").is_some());
        assert!(stimuli.get("timestamp").is_some());
        assert!(stimuli.get("values").is_some());
        assert!(stimuli.get("valid_mask").is_some());
        assert!(stimuli.get("metadata").is_some());

        let decoded: IpcMessage = serde_json::from_value(json).expect("decode via corpus-ipc");
        assert_eq!(decoded, message);
    }

    #[test]
    fn corpus_ipc_publisher_slow_consumer_does_not_block_safety() {
        let reject_newest =
            QueueConfig::new(1, QueueFullPolicy::RejectNewest).expect("valid queue config");
        let (publisher, _consumer) =
            CorpusIpcPublisher::channel(Some("sess".into()), reject_newest)
                .expect("valid queue config");
        let mapping = mapping_at_now(&healthy_frame());
        publisher.try_publish(&mapping).unwrap();
        assert_eq!(
            publisher.try_publish(&mapping).unwrap_err(),
            PublishError::SlowConsumer
        );

        let mut machine = SafetyMachine::new();
        let (snap, pub_res) =
            evaluate_then_try_publish(&mut machine, &critical_frame(), &publisher);
        assert_eq!(pub_res, Err(PublishError::SlowConsumer));
        assert_eq!(snap.state, SafetyState::CriticalBraked);
        assert_eq!(snap.intent, BrakeIntent::Apply);
    }

    #[test]
    fn software_only_emits_typed_corpus_ipc_frame_without_gpu() {
        let frame = HardwareBridge::read_telemetry_force(true);
        assert_eq!(frame.source, TelemetrySource::SoftwareFallback);
        let mapping = frame.to_sensory_mapping();
        assert_eq!(
            mapping.acquisition_source,
            TelemetrySource::SoftwareFallback
        );

        let listener = UdpSocket::bind("127.0.0.1:0").expect("bind loopback listener");
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let dest = listener.local_addr().unwrap();
        let publisher =
            CorpusIpcPublisher::spawn(dest, Some("software-only".into()), QueueConfig::default())
                .expect("spawn UDP publisher");

        let mut machine = SafetyMachine::new();
        let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &frame, &publisher);
        assert!(pub_res.is_ok(), "enqueue must succeed: {pub_res:?}");
        assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);

        let mut buf = [0u8; 65_535];
        let (n, _) = listener
            .recv_from(&mut buf)
            .expect("worker must emit at least one typed frame");
        let decoded: IpcMessage =
            serde_json::from_slice(&buf[..n]).expect("payload is corpus-ipc IpcMessage JSON");
        let IpcMessage::Stimuli(batch) = decoded else {
            panic!("expected IpcMessage::Stimuli, got {decoded:?}");
        };
        assert_eq!(batch.session_id.as_deref(), Some("software-only"));
        assert_eq!(batch.batch_id, 1);
        assert!(!batch.values.is_empty());
        let meta = batch.metadata.expect("provenance");
        assert_eq!(meta.source.as_deref(), Some(SOURCE_IDENTITY));
        assert_eq!(
            meta.custom.get("acquisition_source").map(String::as_str),
            Some("software_fallback")
        );
    }

    #[test]
    fn hysteresis_still_reaches_release_while_publisher_fails() {
        use crate::safety::{ActuatorOutcome, BRAKE_FRACTION, FakeActuator, SafetyActuator};

        let mut machine = SafetyMachine::new();
        let publisher = FailingPublisher::send_failed();
        let fake = FakeActuator::new();

        let (snap, pub_res) =
            evaluate_then_try_publish(&mut machine, &critical_frame(), &publisher);
        assert_eq!(
            pub_res,
            Err(PublishError::SendFailed("ipc send failed".into()))
        );
        assert_eq!(snap.intent, BrakeIntent::Apply);
        fake.apply_emergency_brake(BRAKE_FRACTION).unwrap();
        let snap = machine.record_actuator(ActuatorOutcome::Applied);
        assert!(snap.brake_engaged);

        let ok = healthy_frame();
        let _ = evaluate_then_try_publish(&mut machine, &ok, &publisher);
        let _ = evaluate_then_try_publish(&mut machine, &ok, &publisher);
        let (third, pub_res) = evaluate_then_try_publish(&mut machine, &ok, &publisher);
        assert!(pub_res.is_err());
        assert_eq!(third.state, SafetyState::Recovering);
        assert_eq!(third.intent, BrakeIntent::Release);
        fake.release_emergency_brake().unwrap();
        let snap = machine.record_actuator(ActuatorOutcome::Released);
        assert!(!snap.brake_engaged);
        assert!(!fake.is_engaged());
    }

    #[test]
    fn zero_capacity_queue_is_rejected_by_validation() {
        assert!(matches!(
            IsolatedPublishQueue::<SensoryMapping>::bounded(0),
            Err(QueueConfigError::CapacityTooSmall { capacity: 0, .. })
        ));
        // Capacity 1 still accepts one frame, then reports a slow consumer.
        let (queue, consumer) =
            IsolatedPublishQueue::bounded_with_policy(1, QueueFullPolicy::RejectNewest)
                .expect("capacity 1 is valid");
        let mapping = healthy_frame().to_sensory_mapping_at(fixtures::NOW);
        queue.try_enqueue(mapping.clone()).unwrap();
        assert_eq!(
            queue.try_enqueue(mapping.clone()),
            Err(PublishError::SlowConsumer)
        );
        let received = consumer.try_recv().expect("one queued frame");
        assert_eq!(received.acquisition_source, mapping.acquisition_source);
        assert_eq!(received.stimuli.len(), mapping.stimuli.len());
    }

    #[test]
    fn successful_try_publish_does_not_mutate_safety_snapshot() {
        let (queue, consumer) = IsolatedPublishQueue::bounded(4).expect("capacity 4 is valid");
        let mut machine = SafetyMachine::new();
        let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &healthy_frame(), &queue);
        assert!(pub_res.is_ok());
        assert_eq!(snap.state, SafetyState::HealthyReal);
        let received = consumer.try_recv().expect("published frame");
        assert_eq!(
            received.acquisition_source,
            crate::telemetry::TelemetrySource::Nvml
        );
        assert!(consumer.try_recv().is_none());
    }

    #[test]
    fn last_producer_drop_marks_queue_closed_for_worker() {
        let (queue, consumer) = IsolatedPublishQueue::<SensoryMapping>::bounded(1).unwrap();
        let clone = queue.clone();
        assert!(!consumer.producers_gone());
        drop(queue);
        assert!(!consumer.producers_gone());
        drop(clone);
        assert!(consumer.producers_gone());
    }

    #[test]
    fn corpus_ipc_publisher_records_send_failed_on_invalid_batch() {
        let oversized = "x".repeat(1025);
        let (publisher, _consumer) =
            CorpusIpcPublisher::channel(Some(oversized), QueueConfig::default())
                .expect("valid queue config");
        let err = publisher
            .try_publish(&mapping_at_now(&healthy_frame()))
            .unwrap_err();
        assert!(
            matches!(err, PublishError::SendFailed(_)),
            "expected SendFailed, got {err:?}"
        );
    }

    #[test]
    fn udp_worker_records_send_failed_and_exits_when_producers_drop() {
        let dest: SocketAddr = "255.255.255.255:9".parse().expect("broadcast destination");
        let socket = UdpSocket::bind("0.0.0.0:0").expect("bind ephemeral UDP socket");
        socket.set_nonblocking(true).unwrap();
        let (queue, consumer) =
            IsolatedPublishQueue::<IpcMessage>::new(QueueConfig::default()).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            run_udp_worker(consumer, socket, dest);
            let _ = done_tx.send(());
        });

        let mapping = mapping_at_now(&healthy_frame());
        queue
            .try_enqueue(mapping_to_ipc_message(&mapping, Some("sess".into()), 1))
            .unwrap();

        let started = std::time::Instant::now();
        loop {
            if queue.snapshot().dropped(DropReason::SendFailed) >= 1 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "worker did not record send_failed"
            );
            thread::sleep(Duration::from_millis(5));
        }

        drop(queue);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker should exit after the last producer drops");
    }
}
