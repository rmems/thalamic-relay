//! Library consumer example: validate simulated telemetry and evaluate safety
//! without a GPU or the `thalamic-relay` daemon.
//!
//! ```sh
//! cargo run --example software_only
//! ```

use thalamic_relay::safety::{SafetyMachine, SafetyState};
use thalamic_relay::telemetry::{TelemetrySource, assess, fixtures};

fn main() {
    let frame = assess(&fixtures::software_fallback(), fixtures::NOW);
    assert_eq!(frame.source, TelemetrySource::SoftwareFallback);

    let mut machine = SafetyMachine::new();
    let snap = machine.evaluate(&frame);
    assert_eq!(snap.state, SafetyState::SimulatedSoftwareOnly);
    assert!(!snap.desired_brake);

    println!(
        "software-only frame → {} (desired_brake={})",
        snap.state.as_str(),
        snap.desired_brake
    );
}
