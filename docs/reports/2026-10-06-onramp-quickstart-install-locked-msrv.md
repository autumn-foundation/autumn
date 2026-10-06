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
