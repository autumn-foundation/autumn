### Fixed

- **mTLS tooling under `mode = "required"` (issue #2706):** three follow-ups
  so `doctor`, the routes manifest, and the posture diff agree with the live
  listener —
  - `autumn doctor` now **fails** a `[server.tls.client_auth]` whose
    `required_paths` is not an array of strings (e.g. `required_paths =
    "/internal/"`) instead of reading it as zero paths and passing an app
    that cannot start;
  - the routes manifest's `mtls_required` flag is `true` for **every** route
    under `mode = "required"`, not just the ones a `required_paths` prefix
    matches;
  - `autumn routes posture diff` reads `required` mode as covering every
    route, so removing a `required_paths` entry while locking the listener
    no longer raises a **widening** `mtls_required_path_removed` finding.
