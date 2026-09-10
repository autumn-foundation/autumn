//! Coverage inventory for the confidential-fields end-to-end leak sentinel.
//!
//! Sink probes use these stable markers when the end-to-end harness collects
//! PostgreSQL rows, logs, backups, replay capsules, and server-managed key
//! stores. The repository-hygiene gate keeps this inventory synchronized with
//! the machine-readable threat model.

const PROTECTED_SINK_MARKERS: &[&str] = &[
    "protected-sink:postgresql",
    "protected-sink:logs",
    "protected-sink:backups",
    "protected-sink:replay_capsules",
    "protected-sink:server_managed_key_stores",
];

#[test]
fn confidential_fields_end_to_end_leak_sentinel_covers_each_sink_once() {
    let mut markers = PROTECTED_SINK_MARKERS.to_vec();
    markers.sort_unstable();
    markers.dedup();
    assert_eq!(markers.len(), PROTECTED_SINK_MARKERS.len());
}
