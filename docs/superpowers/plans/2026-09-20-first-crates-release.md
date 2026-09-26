# First crates.io release implementation plan

> Execute inline with test-driven development; request an independent code review before delivery.

**Goal:** Qualify 0.2.0 against GH#44, including the outstanding configurable safety policy in GH#47 and consumer hygiene in GH#52.

**Architecture:** Resolve an operator configuration and optional device default power limit into an immutable pure safety policy. The daemon obtains capabilities, validates before starting services, and passes the resolved policy to the state machine; unavailable power policy fails closed for real telemetry. Keep the existing fixed 50% brake strategy and telemetry validity contract.

**Tech Stack:** Rust 2024, exact Rust 1.98.1, clap, NVML, corpus-ipc 0.1.0.

**Spec:** GH#44, GH#47, GH#52 and repository AGENTS.md.

## Global constraints

- No real publish, tag, merge, hardware power mutation, or sibling changes.
- Frozen telemetry CSV header remains unchanged.
- Safety evaluation remains pure and independent of IPC.
- Preserve operator caps, missing/invalid/stale fail-closed behavior and software-only provenance.
- No contributor plan in the packaged archive.

## Tasks

- [x] Add behavior tests for 100 W and 600 W devices, explicit overrides, absent capabilities, malformed limits, configured hysteresis and stale/cadence bounds. Demonstrate failures before implementation.
- [x] Add `src/safety/policy.rs`: validated configuration and immutable effective policy. Default watts derive as 85%/100% of device default; explicit paired overrides must not exceed a known device default. Without either, real frames fail closed. Thermal defaults remain explicit 75/85 C policy choices, not vendor claims. Keep brake fraction fixed at 0.5 for restart compatibility.
- [x] Wire policy classification and recovery into `src/safety.rs`; existing fixture tests select explicit 300/350 W limits. Add CLI/env flags in `src/daemon.rs`, validate before lock/exporter startup, resolve from read-only actuator default limit, print structured effective policy. Check actual binary rejection of invalid CLI/env configuration.
- [x] Update README, rustdoc and safety docs with policy precedence, capability limits, consumer examples and 0.2.0 release procedure. Bump manifest/lockfile and finalize an explicitly unpublished 0.2.0 changelog. Narrow docs package allowlist to consumer files.
- [ ] Run fmt, clippy, all tests, rustdoc, release build, package listing/build, unpacked source tests/example and token-free publish dry-run. Inspect archive metadata and licenses. Run independent consumer against archive.
- [ ] Obtain independent code review and address findings. Commit and open a linked PR; keep publication epic open until release authorization/upload. Report hosted CI and remaining review state honestly.

## Review evidence

Independent review found no Critical or Important defects in the policy and operator-cap changes. Added the RM-1455 cap-ownership regression, tightened release prechecks, and documented the 2 W heuristic and scheduling limitations. Package and PR validation results are recorded in the delivery PR.
