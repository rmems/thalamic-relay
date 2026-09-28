//! `thalamic-relay` supervisor binary.
//!
//! Process plumbing (lockfile, Prometheus, NVML, the async loop) lives in the
//! library crate as private modules. The reusable API is `thalamic_relay::{telemetry, safety, publish}`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    drive_runtime(thalamic_relay::run())?
}

fn drive_runtime<F: std::future::Future>(future: F) -> Result<F::Output, std::io::Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(future)));
    // A driver call inside spawn_blocking cannot be forcibly cancelled. Do not
    // let Tokio's default unbounded worker shutdown keep the process alive (and
    // its single-instance lock occupied) after the supervisor has stopped.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    match outcome {
        Ok(result) => Ok(result),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn runtime_returns_when_blocking_driver_worker_does_not() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let started = Instant::now();
        let result = drive_runtime(async move {
            tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            42
        });
        assert_eq!(result.unwrap(), 42);
        assert!(started.elapsed() < Duration::from_secs(4));
        release_tx.send(()).unwrap();
    }

    #[test]
    fn runtime_still_shuts_down_when_supervisor_panics() {
        let (release_tx, release_rx) = mpsc::channel();
        let started = Instant::now();
        let outcome = std::panic::catch_unwind(|| {
            let _ = drive_runtime(async move {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    ready_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
                ready_rx.await.unwrap();
                panic!("supervisor failed after NVML worker started");
            });
        });
        assert!(outcome.is_err());
        assert!(started.elapsed() < Duration::from_secs(4));
        release_tx.send(()).unwrap();
    }
}
