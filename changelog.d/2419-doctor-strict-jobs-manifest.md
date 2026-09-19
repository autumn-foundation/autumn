### Fixed

- **`autumn doctor` no longer trusts a corrupt jobs manifest (issue #2419):**
  `resolve_declared_queues` read a `[jobs.fleet] manifest`'s `queues` array with
  `filter_map(toml::Value::as_str)`, silently dropping any non-string element —
  so `queues = ["critical", "thumbnails", 1]` was read as `["critical",
  "thumbnails"]`, and the topology-aware queue-coverage check could report Pass
  on a deployment with an uncovered queue. A manifest whose `queues` array is
  present but is not an array of strings is now a hard failure naming the
  manifest path (the reader is as strict as the emitter: `autumn jobs manifest`
  refuses to write exactly this shape), while a manifest that genuinely says
  nothing — unreadable, unparseable, or no `queues` key — still falls through to
  the inline `declared_queues` list as before. The strict parse is a single shared
  implementation in `autumn-cli/src/jobs.rs` (`parse_manifest_queues`), used by
  both the emitter-side validator and doctor's reader, so the two sides cannot
  drift apart again.
