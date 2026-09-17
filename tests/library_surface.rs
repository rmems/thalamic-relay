//! Compile-time consumer API: telemetry + safety + publish, no daemon plumbing.
//!
//! A downstream crate should be able to validate samples and evaluate
//! [`SafetyMachine`] without NVML, Prometheus, or the process lock.

use thalamic_relay::publish::{
    AbsentPublisher, IsolatedPublishQueue, PublishError, SensoryPublisher,
    evaluate_then_try_publish,
};
use thalamic_relay::safety::{
    ActuatorOutcome, BRAKE_FRACTION, BrakeIntent, FakeActuator, SafetyActuator, SafetyMachine,
    SafetyState, classify_frame,
};
use thalamic_relay::telemetry::{
    SampleValidity, SignalClass, SignalId, TelemetrySource, assess, fixtures, signal_spec,
};

#[test]
fn downstream_crate_evaluates_safety_without_gpu_or_daemon() {
    let frame = assess(&fixtures::healthy_real(), fixtures::NOW);
    assert_eq!(frame.source, TelemetrySource::Nvml);
    assert_eq!(frame.gpu_temp_c.validity, SampleValidity::Valid);

    let mut machine = SafetyMachine::new();
    let snap = machine.evaluate(&frame);
    assert_eq!(snap.state, SafetyState::HealthyReal);
    assert_eq!(snap.intent, BrakeIntent::None);

    let mut hot = fixtures::healthy_real();
    hot.gpu_temp_c = Some(90.0);
    let hot_frame = assess(&hot, fixtures::NOW);
    let snap = machine.evaluate(&hot_frame);
    assert_eq!(snap.state, SafetyState::CriticalBraked);
    assert_eq!(snap.intent, BrakeIntent::Apply);

    let actuator = FakeActuator::new();
    actuator.apply_emergency_brake(BRAKE_FRACTION).unwrap();
    let snap = machine.record_actuator(ActuatorOutcome::Applied);
    assert!(snap.brake_engaged);
    assert!(actuator.is_engaged());
}

#[test]
fn downstream_crate_maps_sensory_inputs_and_publishes_best_effort() {
    let frame = assess(&fixtures::healthy_real(), fixtures::NOW);
    let mapping = frame.to_sensory_mapping_at(fixtures::NOW);
    assert_eq!(mapping.acquisition_source, TelemetrySource::Nvml);
    assert!(
        mapping
            .stimuli
            .iter()
            .all(|s| signal_spec(s.signal).class.includes_runtime_input())
    );
    assert!(
        mapping
            .stimuli
            .iter()
            .any(|s| s.signal == SignalId::GpuTempC && s.classification == SignalClass::Both)
    );

    assert_eq!(
        AbsentPublisher.try_publish(&mapping),
        Err(PublishError::Absent)
    );

    let (queue, consumer) = IsolatedPublishQueue::bounded(1).expect("capacity 1 is valid");
    queue.try_publish(&mapping).unwrap();
    assert_eq!(
        consumer.try_recv().expect("published frame").stimuli.len(),
        mapping.stimuli.len()
    );

    let mut machine = SafetyMachine::new();
    let (snap, pub_res) = evaluate_then_try_publish(&mut machine, &frame, &AbsentPublisher);
    assert_eq!(pub_res, Err(PublishError::Absent));
    assert_eq!(snap.state, SafetyState::HealthyReal);
    assert_eq!(
        classify_frame(&frame).kind,
        thalamic_relay::safety::AssessmentKind::Ok
    );
}
