//! Process-local monotonic sample clock and timestamp provenance.
//!
//! Wall-clock source timestamps can jump, repeat, or arrive out of order.
//! Freshness, replay, and sensory emission therefore use a session-scoped
//! [`SampleClock`]: a stable [`SampleClock::session_id`] plus a strictly
//! increasing [`FrameTiming::batch_id`] (corpus-ipc `StimulusBatch` field
//! names). Source wall time is preserved separately and classified against
//! the existing validity policy (future → invalid/stale via
//! [`crate::telemetry`]; duplicate/backward source times are flagged here
//! and do **not** regress the sample sequence).
//!
//! Restart semantics: constructing a new [`SampleClock`] (process boot)
//! yields a new `session_id` and resets `batch_id` to 0. Consumers must
//! key frames by `(session_id, batch_id)`, not `batch_id` alone.
//!
//! Live NVML collectors, software-fallback simulation, and CSV/replay
//! rows share this ordering contract by stamping through the same clock.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Unix-epoch timestamp in milliseconds (UTC). Serializable and suitable for
/// the corpus-ipc mapping surface.
pub type UnixMillis = u64;

/// Wall-clock now as unix milliseconds. Returns 0 if the clock is before epoch.
#[must_use]
pub fn unix_now_ms() -> UnixMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Process-wide discriminator so two clocks built in the same millisecond
/// (tests, or a very fast restart) still get distinct session ids.
static SESSION_DISCRIMINATOR: AtomicU64 = AtomicU64::new(0);

/// Where a frame's source timestamp came from. Orthogonal to
/// [`crate::telemetry::TelemetrySource`] (acquisition path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimestampOrigin {
    /// Wall clock taken at live NVML / hardware acquire.
    LiveAcquire,
    /// Timestamp supplied by a CSV / replay row.
    CsvSource,
    /// Software-fallback / simulated path (explicit `--force-software-only`).
    Simulated,
}

/// Classification of the source wall timestamp relative to receive time and
/// the previous source time in this session.
///
/// Duplicate / backward / missing source times are **flagged**, not used as
/// the sample-sequence key. Future source times are flagged here and also
/// fail the existing sample validity policy (`observed_at > now` → Invalid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceTimeStatus {
    /// Present, not in the future, and strictly after the previous source time
    /// (or the first source time in the session).
    InOrder,
    /// Same source timestamp as the previous stamped frame.
    Duplicate,
    /// Source timestamp earlier than the previous stamped frame.
    Backward,
    /// Source timestamp after receive/`now` (includes implausibly large values).
    Future,
    /// Producer omitted a source timestamp.
    Missing,
}

impl SourceTimeStatus {
    /// Classify `source` against receive time and the last seen source time.
    #[must_use]
    pub fn classify(
        source_unix_ms: Option<UnixMillis>,
        received_at_unix_ms: UnixMillis,
        last_source_unix_ms: Option<UnixMillis>,
    ) -> Self {
        let Some(src) = source_unix_ms else {
            return Self::Missing;
        };
        if src > received_at_unix_ms {
            return Self::Future;
        }
        match last_source_unix_ms {
            Some(prev) if src < prev => Self::Backward,
            Some(prev) if src == prev => Self::Duplicate,
            _ => Self::InOrder,
        }
    }

    /// Duplicate or backward source time (sample clock must still advance).
    #[must_use]
    pub const fn is_source_regression(self) -> bool {
        matches!(self, Self::Duplicate | Self::Backward)
    }
}

/// Per-frame timing stamped by [`SampleClock`]. Field names match corpus-ipc
/// `StimulusBatch` (`session_id`, `batch_id`) so RM-1144 can map without a
/// second schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTiming {
    /// Stable boot/session identifier. New clock ⇒ new id; `batch_id` resets.
    pub session_id: String,
    /// Strictly increasing sample sequence within `session_id` (first frame is 0).
    pub batch_id: u64,
    /// Original source wall time, if the producer supplied one.
    pub source_unix_ms: Option<UnixMillis>,
    /// Receive/assess time at the relay (unix ms). Distinct from source time.
    pub received_at_unix_ms: UnixMillis,
    pub timestamp_origin: TimestampOrigin,
    pub source_time_status: SourceTimeStatus,
}

/// Process-local monotonic sample sequence plus session/boot identity.
#[derive(Debug, Clone)]
pub struct SampleClock {
    session_id: String,
    next_batch_id: u64,
    last_source_unix_ms: Option<UnixMillis>,
}

impl Default for SampleClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SampleClock {
    /// New session: unique `session_id`, `batch_id` starts at 0.
    #[must_use]
    pub fn new() -> Self {
        Self::with_session_id(generate_session_id())
    }

    /// Test/replay helper with an explicit session id. Sequence still starts at 0.
    #[must_use]
    pub fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            next_batch_id: 0,
            last_source_unix_ms: None,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Next `batch_id` that [`Self::stamp`] will assign (not yet consumed).
    #[must_use]
    pub fn peek_batch_id(&self) -> u64 {
        self.next_batch_id
    }

    /// Stamp one emitted frame. Always advances `batch_id`, even when the
    /// source timestamp is missing, duplicate, backward, future, or huge.
    pub fn stamp(
        &mut self,
        source_unix_ms: Option<UnixMillis>,
        received_at_unix_ms: UnixMillis,
        timestamp_origin: TimestampOrigin,
    ) -> FrameTiming {
        let source_time_status = SourceTimeStatus::classify(
            source_unix_ms,
            received_at_unix_ms,
            self.last_source_unix_ms,
        );
        let batch_id = self.next_batch_id;
        self.next_batch_id = self
            .next_batch_id
            .checked_add(1)
            .expect("sample sequence overflow");
        if let Some(src) = source_unix_ms {
            self.last_source_unix_ms = Some(src);
        }
        FrameTiming {
            session_id: self.session_id.clone(),
            batch_id,
            source_unix_ms,
            received_at_unix_ms,
            timestamp_origin,
            source_time_status,
        }
    }
}

/// Age of the last **received** sample in seconds. `None` received_at is 0.
///
/// This is the intended freshness basis: receive/emit time, not source wall
/// time. Duplicate or regressing source timestamps must not move this gauge.
/// `now < received_at` saturates to 0 (wall-clock jump).
#[must_use]
pub fn freshness_seconds(received_at: Option<UnixMillis>, now: UnixMillis) -> f64 {
    received_at
        .map(|ts| now.saturating_sub(ts) as f64 / 1000.0)
        .unwrap_or(0.0)
}

/// Monotonic freshness using [`Instant`] (immune to wall-clock jumps).
#[must_use]
pub fn freshness_seconds_monotonic(received_at: Option<Instant>, now: Instant) -> f64 {
    received_at
        .map(|ts| now.saturating_duration_since(ts).as_secs_f64())
        .unwrap_or(0.0)
}

/// Parse a CSV source-timestamp cell. Empty / whitespace → [`None`] (missing).
///
/// Accepts decimal unix milliseconds. Values that overflow `u64` fail to parse.
/// `u64::MAX` is a valid "very large" fixture, not a parse error.
pub fn parse_source_timestamp_field(field: &str) -> Result<Option<UnixMillis>, String> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<UnixMillis>()
        .map(Some)
        .map_err(|e| format!("invalid source timestamp {trimmed:?}: {e}"))
}

fn generate_session_id() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let disc = SESSION_DISCRIMINATOR.fetch_add(1, Ordering::Relaxed);
    format!("thalamic-{pid}-{nanos}-{disc}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const NOW: UnixMillis = 1_700_000_000_000;

    fn instant_plus(base: Instant, dt: Duration) -> Instant {
        base + dt
    }

    #[test]
    fn sample_clock_is_strictly_increasing_within_one_session() {
        let mut clock = SampleClock::with_session_id("sess-a");
        let mut prev = None;
        for i in 0_u64..8 {
            let t = clock.stamp(
                Some(NOW + i * 100),
                NOW + 1_000,
                TimestampOrigin::LiveAcquire,
            );
            assert_eq!(t.session_id, "sess-a");
            assert_eq!(t.batch_id, i);
            if let Some(p) = prev {
                assert!(t.batch_id > p, "batch_id must be strictly increasing");
            }
            prev = Some(t.batch_id);
        }
        assert_eq!(clock.peek_batch_id(), 8);
    }

    #[test]
    fn duplicate_source_timestamps_do_not_regress_the_sample_clock() {
        let mut clock = SampleClock::with_session_id("sess-dup");
        let a = clock.stamp(Some(NOW), NOW + 10, TimestampOrigin::CsvSource);
        let b = clock.stamp(Some(NOW), NOW + 20, TimestampOrigin::CsvSource);
        assert_eq!(a.source_time_status, SourceTimeStatus::InOrder);
        assert_eq!(b.source_time_status, SourceTimeStatus::Duplicate);
        assert!(b.source_time_status.is_source_regression());
        assert_eq!(a.batch_id, 0);
        assert_eq!(b.batch_id, 1);
        assert_eq!(a.source_unix_ms, b.source_unix_ms);
        assert!(b.received_at_unix_ms > a.received_at_unix_ms);
        assert_eq!(a.session_id, b.session_id);
    }

    #[test]
    fn backward_source_timestamps_do_not_regress_the_sample_clock() {
        let mut clock = SampleClock::with_session_id("sess-back");
        let a = clock.stamp(Some(NOW + 5_000), NOW + 5_000, TimestampOrigin::CsvSource);
        let b = clock.stamp(Some(NOW), NOW + 5_100, TimestampOrigin::CsvSource);
        assert_eq!(a.source_time_status, SourceTimeStatus::InOrder);
        assert_eq!(b.source_time_status, SourceTimeStatus::Backward);
        assert!(b.source_time_status.is_source_regression());
        assert!(b.batch_id > a.batch_id);
        assert!(b.source_unix_ms.unwrap() < a.source_unix_ms.unwrap());
    }

    #[test]
    fn missing_source_timestamp_is_flagged_and_clock_still_advances() {
        let mut clock = SampleClock::with_session_id("sess-miss");
        let a = clock.stamp(Some(NOW), NOW, TimestampOrigin::CsvSource);
        let b = clock.stamp(None, NOW + 100, TimestampOrigin::CsvSource);
        let c = clock.stamp(Some(NOW + 200), NOW + 200, TimestampOrigin::CsvSource);
        assert_eq!(b.source_time_status, SourceTimeStatus::Missing);
        assert_eq!(b.source_unix_ms, None);
        assert_eq!(a.batch_id, 0);
        assert_eq!(b.batch_id, 1);
        assert_eq!(c.batch_id, 2);
        assert_eq!(c.source_time_status, SourceTimeStatus::InOrder);
    }

    #[test]
    fn very_large_source_timestamp_is_future_and_does_not_regress_clock() {
        let mut clock = SampleClock::with_session_id("sess-huge");
        let a = clock.stamp(Some(NOW), NOW, TimestampOrigin::CsvSource);
        let huge = clock.stamp(Some(u64::MAX), NOW, TimestampOrigin::CsvSource);
        let after = clock.stamp(Some(NOW + 1), NOW + 1, TimestampOrigin::CsvSource);
        assert_eq!(huge.source_time_status, SourceTimeStatus::Future);
        assert_eq!(huge.source_unix_ms, Some(u64::MAX));
        assert!(huge.batch_id > a.batch_id);
        assert!(after.batch_id > huge.batch_id);
        // Last seen source is u64::MAX, so NOW+1 is a backward regression of
        // the source clock but the sample sequence still advances.
        assert_eq!(after.source_time_status, SourceTimeStatus::Backward);
    }

    #[test]
    fn future_source_time_relative_to_receive_is_flagged() {
        let mut clock = SampleClock::with_session_id("sess-fut");
        let t = clock.stamp(Some(NOW + 5_000), NOW, TimestampOrigin::LiveAcquire);
        assert_eq!(t.source_time_status, SourceTimeStatus::Future);
        assert_eq!(t.batch_id, 0);
    }

    #[test]
    fn restart_resets_are_distinguishable_by_session_id() {
        let mut boot1 = SampleClock::new();
        let mut boot2 = SampleClock::new();
        assert_ne!(
            boot1.session_id(),
            boot2.session_id(),
            "each SampleClock construction is a new boot/session"
        );
        let a = boot1.stamp(Some(NOW), NOW, TimestampOrigin::LiveAcquire);
        let b = boot2.stamp(Some(NOW), NOW, TimestampOrigin::LiveAcquire);
        assert_eq!(a.batch_id, 0);
        assert_eq!(b.batch_id, 0);
        assert_ne!(a.session_id, b.session_id);
        // Same process, second stamp on boot1 continues that session only.
        let a2 = boot1.stamp(Some(NOW + 1), NOW + 1, TimestampOrigin::LiveAcquire);
        assert_eq!(a2.batch_id, 1);
        assert_eq!(a2.session_id, a.session_id);
    }

    #[test]
    fn csv_and_live_origins_share_the_same_sample_clock_contract() {
        let mut clock = SampleClock::with_session_id("shared-session");
        let live = clock.stamp(Some(NOW), NOW, TimestampOrigin::LiveAcquire);
        let csv = clock.stamp(Some(NOW), NOW + 5, TimestampOrigin::CsvSource);
        let sim = clock.stamp(Some(NOW), NOW + 10, TimestampOrigin::Simulated);
        assert_eq!(live.session_id, csv.session_id);
        assert_eq!(csv.session_id, sim.session_id);
        assert_eq!(live.batch_id, 0);
        assert_eq!(csv.batch_id, 1);
        assert_eq!(sim.batch_id, 2);
        assert_eq!(live.timestamp_origin, TimestampOrigin::LiveAcquire);
        assert_eq!(csv.timestamp_origin, TimestampOrigin::CsvSource);
        assert_eq!(sim.timestamp_origin, TimestampOrigin::Simulated);
        assert_eq!(csv.source_time_status, SourceTimeStatus::Duplicate);
        assert_eq!(sim.source_time_status, SourceTimeStatus::Duplicate);
    }

    #[test]
    fn parse_source_timestamp_field_covers_missing_and_very_large() {
        assert_eq!(parse_source_timestamp_field("").unwrap(), None);
        assert_eq!(parse_source_timestamp_field("   ").unwrap(), None);
        assert_eq!(
            parse_source_timestamp_field("1700000000000").unwrap(),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_source_timestamp_field("18446744073709551615").unwrap(),
            Some(u64::MAX)
        );
        assert!(parse_source_timestamp_field("not-a-time").is_err());
        assert!(parse_source_timestamp_field("-1").is_err());
    }

    #[test]
    fn freshness_seconds_uses_receive_time_not_source_time() {
        // Source is 10s old / duplicate; receive is recent. Freshness follows receive.
        let received = Some(NOW);
        assert!((freshness_seconds(received, NOW) - 0.0).abs() < f64::EPSILON);
        assert!((freshness_seconds(received, NOW + 2_500) - 2.5).abs() < f64::EPSILON);
        // Source regression must not be passed as the freshness instant.
        let source_regressed = Some(NOW - 10_000);
        let from_source = freshness_seconds(source_regressed, NOW);
        let from_receive = freshness_seconds(received, NOW);
        assert!(from_source > from_receive);
        assert!((from_receive - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn freshness_seconds_boundaries() {
        assert_eq!(freshness_seconds(None, NOW), 0.0);
        assert_eq!(freshness_seconds(Some(NOW), NOW), 0.0);
        // Wall clock jumped backward relative to receive time.
        assert_eq!(freshness_seconds(Some(NOW + 5_000), NOW), 0.0);
        // Very large receive vs now: saturating age.
        let age = freshness_seconds(Some(0), u64::MAX);
        assert!(age > 0.0);
        assert!((age - (u64::MAX as f64 / 1000.0)).abs() < 1e-3);
    }

    #[test]
    fn freshness_seconds_monotonic_grows_and_saturates() {
        let t0 = Instant::now();
        let later = instant_plus(t0, Duration::from_millis(1_500));
        let early = freshness_seconds_monotonic(Some(t0), later);
        assert!((early - 1.5).abs() < 1e-6);
        assert_eq!(freshness_seconds_monotonic(None, later), 0.0);
        assert_eq!(freshness_seconds_monotonic(Some(t0), t0), 0.0);
        // now before received (should not happen with Instant; saturates).
        assert_eq!(freshness_seconds_monotonic(Some(later), t0), 0.0);
    }

    #[test]
    fn source_and_receive_times_are_distinguishable_on_frame_timing() {
        let mut clock = SampleClock::with_session_id("sess-split");
        let t = clock.stamp(Some(NOW - 10_000), NOW, TimestampOrigin::CsvSource);
        assert_eq!(t.source_unix_ms, Some(NOW - 10_000));
        assert_eq!(t.received_at_unix_ms, NOW);
        assert_ne!(t.source_unix_ms.unwrap(), t.received_at_unix_ms);
    }
}
