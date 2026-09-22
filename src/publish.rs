//! Best-effort sensory publication, isolated from the safety failure domain.
//!
//! Maps a [`SensoryMapping`] into the canonical `corpus-ipc` wire type
//! [`StimulusBatch`] and publishes [`IpcMessage::Stimuli`] outside
//! [`crate::safety::SafetyMachine::evaluate`]. Implementations of
//! [`SensoryPublisher::try_publish`] must return promptly: the supervisor
//! never awaits a consumer and never calls into this module from inside
//! `evaluate`.
//!
//! Production uses [`CorpusIpcPublisher`]: a bounded `try_send` into a
//! dedicated worker that serializes `IpcMessage` JSON and fire-and-forget
//! UDP-sends it. Send failures, disconnects, slow consumers, and Brainstem
//! absence cannot stall or disable hardware-safety evaluation.

use crate::safety::{SafetyMachine, SafetySnapshot};
use crate::telemetry::{
    SampleValidity, SensoryMapping, TelemetryFrame, TelemetrySource, UnixMillis,
};
use corpus_ipc::{BatchMetadata, IpcMessage, StimulusBatch, Validate};
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{RecvError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread;

/// Canonical `corpus-ipc` identity stamped into [`BatchMetadata::source`].
pub const SOURCE_IDENTITY: &str = "thalamic-relay";
/// Default UDP destination for [`IpcMessage::Stimuli`] datagrams.
pub const DEFAULT_IPC_ENDPOINT: &str = "127.0.0.1:9900";
/// Empty default means the process-unique telemetry boot/session id is used.
pub const DEFAULT_IPC_SESSION_ID: &str = "";
/// Bounded queue depth. A full queue is [`PublishError::SlowConsumer`].
pub const DEFAULT_IPC_QUEUE_CAPACITY: usize = 8;

/// Why a best-effort publish did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// No Brainstem / corpus-ipc transport is configured.
    Absent,
    /// The publisher worker is gone.
    Disconnected,
    /// Bounded queue was full. Its oldest frame was dropped and replaced.
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
        Err(PublishError::SendFailed(self.reason.clone()))
    }
}

/// Bounded non-blocking, drop-oldest queue.
///
/// The safety supervisor uses [`Self::try_enqueue`] (never `recv` / never
/// wait). A slow or missing consumer cannot stall evaluation.
#[derive(Debug, Clone)]
pub struct IsolatedPublishQueue<T> {
    shared: Arc<QueueState<T>>,
}

#[derive(Debug)]
struct QueueState<T> {
    inner: Mutex<QueueInner<T>>,
    ready: Condvar,
    capacity: usize,
}

#[derive(Debug)]
struct QueueInner<T> {
    items: VecDeque<T>,
    receiver_alive: bool,
}

/// Receiving end of an [`IsolatedPublishQueue`].
#[derive(Debug)]
pub struct PublishReceiver<T> {
    shared: Arc<QueueState<T>>,
}

impl<T: Send> IsolatedPublishQueue<T> {
    /// Capacity-1 (minimum) queue plus the receiving end (for tests or a worker).
    #[must_use]
    pub fn bounded(capacity: usize) -> (Self, PublishReceiver<T>) {
        let shared = Arc::new(QueueState {
            inner: Mutex::new(QueueInner {
                items: VecDeque::with_capacity(capacity.max(1)),
                receiver_alive: true,
            }),
            ready: Condvar::new(),
            capacity: capacity.max(1),
        });
        (
            Self {
                shared: Arc::clone(&shared),
            },
            PublishReceiver { shared },
        )
    }

    /// Non-blocking enqueue, replacing the oldest queued item when full.
    ///
    /// Replacement returns [`PublishError::SlowConsumer`] to report the loss,
    /// even though `item` becomes the newest queued snapshot. If the mutex is
    /// momentarily contended, this returns the same error rather than waiting.
    pub fn try_enqueue(&self, item: T) -> Result<(), PublishError> {
        let mut inner = match self.shared.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::WouldBlock) => return Err(PublishError::SlowConsumer),
            Err(TryLockError::Poisoned(_)) => return Err(PublishError::Disconnected),
        };
        if !inner.receiver_alive {
            return Err(PublishError::Disconnected);
        }
        let replaced = inner.items.len() == self.shared.capacity;
        if replaced {
            inner.items.pop_front();
        }
        inner.items.push_back(item);
        self.shared.ready.notify_one();
        if replaced {
            Err(PublishError::SlowConsumer)
        } else {
            Ok(())
        }
    }
}

impl<T> PublishReceiver<T> {
    /// Receive the next queued item, waiting only on the worker side.
    pub fn recv(&self) -> Result<T, RecvError> {
        let mut inner = self.shared.inner.lock().map_err(|_| RecvError)?;
        loop {
            if let Some(item) = inner.items.pop_front() {
                return Ok(item);
            }
            if Arc::strong_count(&self.shared) == 1 {
                return Err(RecvError);
            }
            inner = self.shared.ready.wait(inner).map_err(|_| RecvError)?;
        }
    }

    /// Receive without waiting.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut inner = self.shared.inner.try_lock().map_err(|err| match err {
            TryLockError::WouldBlock => TryRecvError::Empty,
            TryLockError::Poisoned(_) => TryRecvError::Disconnected,
        })?;
        inner.items.pop_front().ok_or_else(|| {
            if Arc::strong_count(&self.shared) == 1 {
                TryRecvError::Disconnected
            } else {
                TryRecvError::Empty
            }
        })
    }
}

impl<T> Drop for PublishReceiver<T> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.shared.inner.lock() {
            inner.receiver_alive = false;
            inner.items.clear();
        }
        self.shared.ready.notify_all();
    }
}

impl SensoryPublisher for IsolatedPublishQueue<SensoryMapping> {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError> {
        self.try_enqueue(mapping.clone())
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
}

impl CorpusIpcPublisher {
    /// Start a detached UDP worker sending to `endpoint`.
    ///
    /// Binding the local socket is the only fallible step. After this returns,
    /// [`Self::try_publish`] is non-blocking (`try_send`).
    pub fn spawn(
        endpoint: SocketAddr,
        session_id: Option<String>,
        capacity: usize,
    ) -> std::io::Result<Self> {
        let (queue, rx) = IsolatedPublishQueue::bounded(capacity);
        let socket = UdpSocket::bind(local_bind_for(endpoint))?;
        socket.set_nonblocking(true)?;
        thread::Builder::new()
            .name("corpus-ipc-publish".to_string())
            .spawn(move || run_udp_worker(rx, socket, endpoint))?;
        Ok(Self { queue, session_id })
    }

    /// Enqueue-only publisher (no socket). For tests that inspect `IpcMessage`.
    #[must_use]
    pub fn channel(
        session_id: Option<String>,
        capacity: usize,
    ) -> (Self, PublishReceiver<IpcMessage>) {
        let (queue, rx) = IsolatedPublishQueue::bounded(capacity);
        (Self { queue, session_id }, rx)
    }
}

impl SensoryPublisher for CorpusIpcPublisher {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError> {
        let session_id = self
            .session_id
            .clone()
            .or_else(|| Some(mapping.session_id.clone()));
        let batch = mapping_to_stimulus_batch(mapping, session_id, mapping.batch_id);
        if let Err(err) = batch.validate() {
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

fn run_udp_worker(rx: PublishReceiver<IpcMessage>, socket: UdpSocket, dest: SocketAddr) {
    while let Ok(msg) = rx.recv() {
        let Ok(bytes) = serde_json::to_vec(&msg) else {
            tracing::debug!("corpus-ipc: failed to serialize IpcMessage; dropping frame");
            continue;
        };
        match socket.send_to(&bytes, dest) {
            Ok(_) => {}
            Err(err) => {
                tracing::debug!("corpus-ipc: UDP send failed ({err}); dropping frame");
            }
        }
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
    use std::net::UdpSocket;
    use std::time::Duration;

    fn critical_frame() -> TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(90.0);
        assess(&raw, fixtures::NOW)
    }

    fn healthy_frame() -> TelemetryFrame {
        assess(&fixtures::healthy_real(), fixtures::NOW)
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
        let (queue, rx) = IsolatedPublishQueue::bounded(1);
        let mapping = mapping_at_now(&healthy_frame());
        queue.try_enqueue(mapping.clone()).unwrap();

        let err = queue.try_enqueue(mapping.clone()).unwrap_err();
        assert_eq!(err, PublishError::SlowConsumer);

        let mut machine = SafetyMachine::new();
        let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &critical_frame(), &queue);
        assert_eq!(pub_res, Err(PublishError::SlowConsumer));
        assert_eq!(snap.state, SafetyState::CriticalBraked);
        assert_eq!(snap.intent, BrakeIntent::Apply);

        let newest = rx.try_recv().expect("replacement remains queued");
        assert_eq!(newest.batch_id, mapping.batch_id);
    }

    #[test]
    fn disconnected_queue_does_not_block_safety() {
        let (queue, rx) = IsolatedPublishQueue::bounded(1);
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
        let (publisher, _rx) = CorpusIpcPublisher::channel(Some("sess".into()), 1);
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
    fn capacity_one_drops_oldest_and_keeps_newest_sequence() {
        let (publisher, rx) = CorpusIpcPublisher::channel(None, 1);
        let mut mapping = mapping_at_now(&healthy_frame());
        mapping.session_id = "boot-a".into();

        for batch_id in 10..=12 {
            mapping.batch_id = batch_id;
            let result = publisher.try_publish(&mapping);
            if batch_id == 10 {
                assert_eq!(result, Ok(()));
            } else {
                assert_eq!(result, Err(PublishError::SlowConsumer));
            }
        }

        let IpcMessage::Stimuli(batch) = rx.try_recv().expect("newest frame retained") else {
            panic!("expected stimuli");
        };
        assert_eq!(batch.session_id.as_deref(), Some("boot-a"));
        assert_eq!(batch.batch_id, 12);
    }

    #[test]
    fn paused_consumer_burst_retains_bounded_newest_tail() {
        let (queue, rx) = IsolatedPublishQueue::bounded(3);
        for value in 0..10 {
            let _ = queue.try_enqueue(value);
        }
        assert_eq!(rx.try_recv(), Ok(7));
        assert_eq!(rx.try_recv(), Ok(8));
        assert_eq!(rx.try_recv(), Ok(9));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reconnect_uses_new_receiver_and_preserves_frame_identity() {
        let (old, old_rx) = CorpusIpcPublisher::channel(None, 1);
        drop(old_rx);
        let mut mapping = mapping_at_now(&healthy_frame());
        mapping.session_id = "boot-after-reconnect".into();
        mapping.batch_id = 41;
        assert_eq!(old.try_publish(&mapping), Err(PublishError::Disconnected));

        let (connected, rx) = CorpusIpcPublisher::channel(None, 1);
        connected.try_publish(&mapping).unwrap();
        let IpcMessage::Stimuli(batch) = rx.try_recv().unwrap() else {
            panic!("expected stimuli");
        };
        assert_eq!(batch.session_id.as_deref(), Some("boot-after-reconnect"));
        assert_eq!(batch.batch_id, 41);
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
        let publisher = CorpusIpcPublisher::spawn(dest, Some("software-only".into()), 8)
            .expect("spawn UDP publisher");

        let mut machine = SafetyMachine::new();
        let (snap, mut pub_res) = evaluate_then_try_publish(&mut machine, &frame, &publisher);
        // The worker may briefly own the queue mutex while it begins waiting;
        // retry exactly as the supervisor will on its next telemetry tick.
        for _ in 0..100 {
            if pub_res.is_ok() {
                break;
            }
            std::thread::yield_now();
            pub_res = publisher.try_publish(&mapping);
        }
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
        assert_eq!(batch.batch_id, frame.batch_id);
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
    fn zero_capacity_queue_still_accepts_one_then_reports_slow_consumer() {
        let (queue, rx) = IsolatedPublishQueue::bounded(0);
        let mapping = healthy_frame().to_sensory_mapping_at(fixtures::NOW);
        queue.try_enqueue(mapping.clone()).unwrap();
        assert_eq!(
            queue.try_enqueue(mapping.clone()),
            Err(PublishError::SlowConsumer)
        );
        let received = rx.try_recv().unwrap();
        assert_eq!(received.acquisition_source, mapping.acquisition_source);
        assert_eq!(received.stimuli.len(), mapping.stimuli.len());
    }

    #[test]
    fn successful_try_publish_does_not_mutate_safety_snapshot() {
        let (queue, rx) = IsolatedPublishQueue::bounded(4);
        let mut machine = SafetyMachine::new();
        let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &healthy_frame(), &queue);
        assert!(pub_res.is_ok());
        assert_eq!(snap.state, SafetyState::HealthyReal);
        let received = rx.try_recv().unwrap();
        assert_eq!(
            received.acquisition_source,
            crate::telemetry::TelemetrySource::Nvml
        );
        assert!(rx.try_recv().is_err());
    }
}
