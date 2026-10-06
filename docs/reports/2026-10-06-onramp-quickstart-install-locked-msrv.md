# 🛣️ Onramp: `cargo install autumn-cli` fails on the advertised MSRV

## 🎯 Journey

First run, step 1 of the README quickstart (`cargo install autumn-cli --version 0.8.0`).
It is the front door: README.md:36 and docs/guide/getting-started.md:88.
The README badge and "Requirements" both advertise **Rust 1.88.0+**.

Harness (already committed, unchanged by this report): `scripts/check-quickstart.sh`,
run by `.github/workflows/quickstart-gate.yml` as a stable and a 1.88.0 leg.

## 📈 Evidence (Tier 1, clean-room run)

Quickstart Gate, "README quickstart vs published crates (1.88.0)", run
37397354336 on trunk-dev @ 979b35e: **hard-fails at step 1 (install) after 4 s**;
the 8 later steps are skipped. The stable leg passes (all 9 steps). The 1.88.0
leg has been red on every trunk push since 2026-10-05.

```
error: failed to compile `autumn-cli v0.8.0`
Caused by:
  rustc 1.88.0 is not supported by the following package:
    uuid@1.27.0 requires rustc 1.89.0
  Try re-running `cargo install` with `--locked`
```

Local reproduction (rustc 1.88.0, fresh CARGO_HOME): same error, 9 s.

## 💡 Hypothesis

Plain `cargo install` ignores the `Cargo.lock` that `autumn-cli` ships and
re-resolves every dependency to the newest semver-compatible release. A
transitive dependency (`uuid` 1.27.0) raised its rustc floor to 1.89 after
0.8.0 was published, so the documented command no longer builds on the
documented minimum toolchain. The shipped lockfile pins `uuid 1.26.1`, which
`--locked` would use. Cargo's own message names the fix; the README does not.

## 🔧 Change (docs layer, plus the harness line that mirrors it)

Add `--locked` to the documented install command in README.md and
docs/guide/getting-started.md, and to the install phase of
`scripts/check-quickstart.sh`, so the gate keeps running the README verbatim.
No public API, flag or default changes.

## 📊 Baseline (before)

| | 1.88.0 leg |
|---|---|
| Steps passing / total | 0 / 9 |
| Failure line | step 1, `cargo install autumn-cli --version 0.8.0` |
| Time to failure | 4 s (CI), 9 s (local) |
| Question-log tally | none filed; no users counted, and this report does not claim any |

## 📊 After (local re-run, rustc 1.88.0, fresh CARGO_HOME)

| | Before | After |
|---|---|---|
| `cargo install autumn-cli --version 0.8.0` | ✗ at 9 s | — |
| `cargo install autumn-cli --version 0.8.0 --locked` | — | ✓ 7m33s |
| `autumn new my-app` + `cargo build` (steps 2–4 equivalent) | skipped | ✓ 2m19s |
| 1.88.0 leg of the gate | 0 / 9 | install + new + build reproduced green locally; the full 9-step leg (setup, serve, scaffold, migrate) was **not** run here and needs the gate's own run |

The gate workflow only runs on trunk pushes, schedule and dispatch, so the
after-measurement from the gate itself is the next trunk run, or a
`workflow_dispatch` on this branch.

Compatibility: docs and a CI script only. No crate, public API or flag is
touched. `check-docs-versions.sh` and `check-docs-cli.sh` pass.

## Impact floor

Clears bullet 1: a hard failure on the documented path (1.88.0 leg red today).

## Not addressed

Whether a user on a newer-than-1.88 but older-than-latest toolchain hits the
same wall with future dependency bumps. `--locked` covers it for the shipped
lockfile; the 0.8.x lockfile itself still pins what it pins.

## 🔬 Reproduce

```bash
rustup toolchain install 1.88.0 --profile minimal
cargo +1.88.0 install autumn-cli --version 0.8.0           # ✗ uuid 1.27.0 needs 1.89
cargo +1.88.0 install autumn-cli --version 0.8.0 --locked  # ✓
```
