# 🚦 Semaphore: CI health follow-up — `Windows Tier 1 journey`'s own stderr fix paid off in under 24h: mechanism confirmed, fix applied

Follow-up to `docs/reports/2026-09-27-semaphore-ci-health-followup.md`. That
pass added stderr capture to `autumn setup`/`build`/`routes`'s cargo-metadata
helpers after two unexplained `Windows Tier 1 journey` failures, and noted
PR #2968 (MinIO→Chainguard) was open and CI-green. This pass: #2968 merged
(2026-09-27T21:05:17Z), and the stderr fix (#2973, merged the same window)
caught two more `Windows Tier 1 journey` failures with the actual `cargo`
error attached — enough to root-cause and fix the job's toolchain
provisioning outright, not just watch for a third occurrence.

## 🎯 Verdict path

Unchanged from the 2026-09-27 report: `Test suite` (`test-gate`, fed by
`[test, trybuild, test-features, test-docker]`) plus `Supply chain
(cargo-deny)` are the aggregated required gates; `Windows Tier 1 journey` is
a separate required check (platform-support policy, issue #1616 AC-3) that
blocks merge on its own, not through the aggregator.
`manual-macos-contention-check.yml` remains dispatch-only — still
`total_count: 0`, now the **19th consecutive idle pass** (~474h, past 19.7
days). Not dispatched this pass: new macOS CI spend needs a human sign-off,
unavailable in this unattended run.

## 🌡️ Symptom

Sampled `ci.yml` `pull_request` runs, `perPage=50`/`status=completed`, page 1:
50 runs spanning 2026-09-28T05:20:45Z–09:47:23Z (~4.4h) — mostly `cancelled`
(rapid superseding pushes on a handful of actively-iterating branches, not a
health signal, consistent with every prior pass's finding) plus a handful of
`success`, and **4 failures**, all triaged at job/log level:

| Run | Branch | Failing job(s) | Finding |
|---|---|---|---|
| 108821476268 (run 36386659629) | `vesper/bugbash-2662-parent-pk-override` | `Windows Tier 1 journey` | **New signature revealed** — see below. |
| 108842089677 (run 36391994302) | `vesper/bugbash-2445-capacity-probe-count` | `Windows Tier 1 journey` | Same signature, ~70 min later. |
| 36393954592 | `claude/busy-cerf-qydnb2` (PR #2987) | `Coverage (workspace)` | Already-tracked `live_upgrade` flake (line 686), new organic data point — see ledger. |
| 36384860076 | `claude/intelligent-ptolemy-urgqa5` | `Lint`, `Test suite` | Branch-owned WIP (own diff), not triaged further. |

The two `Windows Tier 1 journey` hits are this pass's finding. Both now show
the actual `cargo` error, thanks to the 2026-09-27 stderr fix:

```
✗ Failed to read cargo metadata
error: the 'cargo.exe' binary, normally provided by the 'cargo' component, is not applicable to the '1.88.0-x86_64-pc-windows-msvc' toolchain
```

Identical text both times. Both runs' own `autumn doctor` step, immediately
before, passed its `rust_toolchain` check (`"rustc 1.88.0 ≥ MSRV 1.88.0"`) —
the *default* toolchain is fine; it's a different, second toolchain that
fails.

## 🔍 Diagnosis

**Mechanism, confirmed by source — not just the error string.** Every
scaffolded app's `rust-toolchain.toml` pins `channel = "1.88.0"` literally
(`autumn-cli/src/templates/rust-toolchain.toml.tmpl`, substituted from
`Cargo.toml`'s `rust-version = "1.88.0"` via
`autumn-cli/src/new.rs:227`) — a *different* rustup toolchain identity than
`"stable"`, even though `stable` currently happens to resolve to the same
rustc version. Before this pass, `windows-tier1`'s only toolchain-install
step was `dtolnay/rust-toolchain@stable`, which installs and names the
toolchain `stable-x86_64-pc-windows-msvc`. When `autumn setup` first shells
out `cargo metadata` inside the freshly-scaffolded `tier1_app` directory
(`autumn-cli/src/build.rs:875`'s `read_cargo_metadata`), rustup sees that
directory's `rust-toolchain.toml` override and auto-installs the separate
`1.88.0-x86_64-pc-windows-msvc` toolchain on the fly — an implicit,
un-retried network operation buried inside an otherwise-ordinary command,
on a job that's simultaneously compiling the full `autumn-web` +
`managed-pg-bundled` dependency graph (the same job whose `env:` block
already documents fighting a Windows PDB symbol-count limit — this runner
is not idle when the race window opens). `"cargo.exe... not applicable to
the toolchain"` is rustup's failure shape for a toolchain whose on-disk
contents don't match what its own manifest claims, consistent with an
on-demand install that raced or partially completed.

The repo's own `msrv` job already avoids exactly this by installing
`dtolnay/rust-toolchain@1.88.0` directly — `windows-tier1` just never
carried the same fix.

**Test-vs-product verdict: CI/build infrastructure, not a product or test
defect.** Pinning the exact MSRV in every scaffolded project is deliberate,
correct, and already unit-tested
(`rust_toolchain_pins_channel_to_msrv` in `autumn-cli/src/new.rs`). The
defect is entirely in this one CI job's own toolchain provisioning, which
installed `stable` for itself while leaving a *different* pinned toolchain
to be resolved implicitly, mid-journey, with no explicit step, no log
visibility, and no retry.

## 🔧 Treatment

Root-caused, not tolerance-widened. `windows-tier1`'s toolchain step changed
from `dtolnay/rust-toolchain@stable` to `dtolnay/rust-toolchain@1.88.0` —
the exact toolchain the scaffolded app will request, installed up front, so
rustup never needs an on-demand install once the journey begins. This
mirrors the `msrv` job's own existing pattern in the same file; it is not a
new convention. No retry added, no timeout raised, no assertion widened —
the on-demand install path is removed, not made more forgiving. A comment at
the change site records the diagnosis and the four run IDs (two from
2026-09-25/27, two from today) for whoever next touches this job.

Filed as a dated update on the existing "`Windows Tier 1 journey`: `autumn
setup` fails..." entry in `docs/ci-health/quarantine-ledger.md` (opened
2026-09-27), not a new entry — same signature, now root-caused. Not
quarantined: the job runs unchanged otherwise.

## 📊 Measurement

| Item | Before this pass | This pass | After |
|---|---|---|---|
| `Windows Tier 1 journey` cargo error | Hidden (fixed 2026-09-27, not yet exercised) | 2 more organic hits, both now showing the real rustup error | Mechanism confirmed; toolchain-provisioning fix applied |
| `Windows Tier 1 journey` signature count | n=2 | n=4, identical text both new hits | Fix targets the confirmed mechanism directly |
| `live_upgrade`/`cache_stampede`/`sim_fault_plan`/`job_tracking_stores_integration` | Uncampaigned, per prior reports | One more `live_upgrade` line-686 hit (run 36393954592), already-tracked signature, no new mechanism claim | Unchanged — still needs the CI-native rerun harness named in the 2026-09-10 update |
| MinIO quarantine (offsite_backup x2, sqlite_replication_s3) | Open, revisit-by 2026-10-02 | #2968 merged 2026-09-27T21:05:17Z | **Closed** (per that entry's own resolution note) |
| `manual-macos-contention-check.yml` dispatches | 0 (18 idle passes, ~450h) | 0 (19th idle pass, ~474h) | Needs human sign-off for CI spend |

No before/after rerun-rate table for the `Windows Tier 1 journey` fix: this
is CI-infrastructure toolchain provisioning, not test determinism, so there
is nothing to statistically rerun — the next `Windows Tier 1 journey` run on
this PR's own head is the measurement, the same posture already used for
the `postgresql_embedded`/`GITHUB_TOKEN` entry. Revert check: not
applicable in the rerun-campaign sense, but reverting the toolchain-pin edit
would restore the exact on-demand-install path all four occurrences hit.

Local verification: `python3 -c "import yaml; yaml.safe_load(...)"` confirms
the edited `ci.yml` parses; `actionlint` was not available in this sandbox.
No Windows runner available locally to reproduce the original failure or
pre-verify the fix.

**Update, same day**: PR #2994's own `Windows Tier 1 journey` run (job
108894563658) completed `success` at 2026-09-28T11:05:23Z against head
`6f47775` — the first run to install `@1.88.0` up front, with no on-demand
toolchain install and no `cargo.exe`/toolchain error. CI-native confirmation
obtained. Separately, a Codex review comment on #2994 caught that
`scripts/check-msrv.sh` didn't actually guard `windows-tier1`'s new pin
against a future MSRV bump (it only checked that *some* line in `ci.yml`
carried the canonical pin, already satisfied by the `msrv` job alone) — fixed
in the same PR, verified to fail when the pin is reverted and pass
otherwise.

## 🔬 Reproduce

Confirm this pass's sampling:

```
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=ci.yml, event=pull_request, status=completed,
             perPage=50, page=1)
# -> 50 runs in [2026-09-28T05:20:45Z, 2026-09-28T09:47:23Z], 4 failures
```

Confirm the `Windows Tier 1 journey` signature (now with the real error):

```
get_job_logs(job_id=108821476268, return_content=true, tail_lines=120)
get_job_logs(job_id=108842089677, return_content=true, tail_lines=300)
# -> both: "✗ Failed to read cargo metadata" / "error: the 'cargo.exe'
#    binary, normally provided by the 'cargo' component, is not applicable
#    to the '1.88.0-x86_64-pc-windows-msvc' toolchain"
```

Confirm the fix:

```
grep -n "dtolnay/rust-toolchain@1.88.0\|windows-tier1:" .github/workflows/ci.yml
# -> both the `msrv` job and `windows-tier1` now pin @1.88.0
```

Confirm the MinIO quarantine's closure and the macOS harness's idle streak:

```
git log --oneline -1 -- docs/ci-health/quarantine-ledger.md  # e669e97, #2968
actions_list(list_workflow_runs, owner=autumn-foundation, repo=autumn,
             resource_id=manual-macos-contention-check.yml,
             event=workflow_dispatch)
# -> total_count: 0
```
