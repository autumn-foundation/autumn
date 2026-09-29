### Fixed

- **pre-push gate:** `scripts/pre-push-check.sh` now compiles the
  feature-gated `integration_tests` binary (issue #2395). The gated clippy leg
  builds `--lib` only and the workspace `--no-run` leg runs with default
  features, so a compile break in a `#![cfg(feature = "…")]` integration
  module (e.g. `acme_end_to_end`, `tls_serving`) previously passed the local
  gate and failed only in CI's feature lanes. One extra `--no-run` link with
  the gated feature set (`+ i18n`, which gates integration modules but no
  panic-gate lib module, and `test-support`, paired the way CI pairs it) and a
  separate `plugin-sandbox` leg close the hole.
