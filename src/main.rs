//! `thalamic-relay` supervisor binary.
//!
//! Process plumbing (lockfile, Prometheus, NVML, the async loop) lives in the
//! library crate as private modules. The reusable API is `thalamic_relay::{telemetry, safety, publish}`.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    thalamic_relay::run().await
}
