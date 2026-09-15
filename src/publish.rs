//! Best-effort sensory publication, isolated from the safety failure domain.
//!
//! This is a thin stub for GH#42 (safety must not stall on IPC). Full
//! `corpus-ipc` transport remains GH#40. Implementations of
//! [`SensoryPublisher::try_publish`] must return promptly; the supervisor
//! never awaits a consumer and never calls into this module from inside
//! [`crate::safety::SafetyMachine::evaluate`].

use crate::safety::{SafetyMachine, SafetySnapshot};
use crate::telemetry::{SensoryMapping, TelemetryFrame};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

/// Why a best-effort publish did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// No Brainstem / corpus-ipc transport is configured (current production).
    Absent,
    /// The publisher worker is gone.
    Disconnected,
    /// Bounded queue is full (slow consumer). The frame is dropped.
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
        Err(PublishError::SendFailed(self.reason.clone()))
    }
}

/// Bounded non-blocking enqueue. A full queue is [`PublishError::SlowConsumer`].
///
/// The safety supervisor uses [`Self::try_enqueue`] (never `recv` / never
/// wait). A slow or missing consumer cannot stall evaluation.
#[derive(Debug, Clone)]
pub struct IsolatedPublishQueue {
    tx: SyncSender<SensoryMapping>,
}

impl IsolatedPublishQueue {
    /// Capacity-1 queue plus the receiving end (for tests or a worker).
    #[must_use]
    pub fn bounded(capacity: usize) -> (Self, Receiver<SensoryMapping>) {
        let (tx, rx) = mpsc::sync_channel(capacity.max(1));
        (Self { tx }, rx)
    }

    /// Non-blocking enqueue. Drops the frame when the consumer is slow.
    pub fn try_enqueue(&self, mapping: SensoryMapping) -> Result<(), PublishError> {
        self.tx.try_send(mapping).map_err(|err| match err {
            TrySendError::Full(_) => PublishError::SlowConsumer,
            TrySendError::Disconnected(_) => PublishError::Disconnected,
        })
    }
}

impl SensoryPublisher for IsolatedPublishQueue {
    fn try_publish(&self, mapping: &SensoryMapping) -> Result<(), PublishError> {
        self.try_enqueue(mapping.clone())
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

    fn critical_frame() -> TelemetryFrame {
        let mut raw = fixtures::healthy_real();
        raw.gpu_temp_c = Some(90.0);
        assess(&raw, fixtures::NOW)
    }

    fn healthy_frame() -> TelemetryFrame {
        assess(&fixtures::healthy_real(), fixtures::NOW)
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
        let (queue, _rx) = IsolatedPublishQueue::bounded(1);
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
}
