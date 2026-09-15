# 🚦 Semaphore: CI health follow-up — a real RUSTSEC gate escape (already fixed), and a repeat flake hit

Follow-up to `docs/reports/2026-09-14-semaphore-ci-health-followup.md` and the
running investigation in `docs/ci-health/quarantine-ledger.md`. No fix PR from
this pass — both findings below were either already fixed by someone else
before this pass started, or fall short of this role's own rerun-campaign bar
for a determinism PR — but two things happened that the ledger needs to
record accurately.

## 🎯 Verdict path

Unchanged: `trunk-dev` is green, and the required gate developers wait on is
`Test suite` (`test-gate`), fed by `[test, trybuild, test-features,
test-docker]`, plus `Supply chain (cargo-deny)`. `manual-macos-contention-check.yml`
remains dispatch-only — **still zero `workflow_dispatch` runs**, now a 7th
consecutive idle pass (~162.5 hours since it became dispatchable at
2026-09-08T15:07:44Z, checked 2026-09-15T~09:5xZ).

## 🌡️ Symptom

Sampled `ci.yml` `pull_request`-triggered runs from the 2026-09-14 report's
own cutoff (2026-09-14T08:00:19Z) to 2026-09-15T09:39:00Z (~25.6 hours, one
`perPage=100` page whose own span, 2026-09-13T20:09:37Z–2026-09-15T09:39:00Z,
fully covers the window with margin on both ends, so no second page was
needed). 68 runs in window: 50 cancelled, 11 success, 7 failure. Full ID
list, anchored per the 2026-09-14 report's own correction against this same
moving-page problem: **failures (7)** — 34867438081, 34877258106,
34881729206, 34882678393, 34882747248, 34907660727, 34934228774.
**Success (11)** — 34820504735, 34834847027, 34871417092, 34889464887,
34890530960, 34907694152, 34908356868, 34908659972, 34909746568,
34932405295, 34941314264. **Cancelled (50)** — 34832109780, 34832912451,
34833276393, 34833954431, 34834252637, 34834645232, 34860640476,
34861241168, 34861963614, 34862658187, 34863353990, 34864149372,
34864632052, 34865101856, 34865541615, 34866030108, 34866558554,
34867112043, 34868794407, 34869762877, 34870497392, 34870748755,
34875860847, 34876412517, 34876529090, 34878196522, 34884872834,
34888975210, 34889309749, 34906480829, 34925919203, 34927845483,
34930410553, 34937618932, 34938484676, 34939235093, 34940156190,
34940893460, 34941771047, 34942695642, 34943645230, 34944322469,
34945041145, 34946033960, 34947362200, 34948366587, 34949380282,
34950538560, 34952452212, 34953664685. All 7 run-level failures triaged by
job/log inspection (cancelled-run job-level
sampling, per the 2026-09-14 report's methodology, was not repeated this
pass — see Measurement for the scope this leaves uncovered).

**Finding 1 — a real, newly-published RUSTSEC advisory failed the required
`Supply chain (cargo-deny)` gate on every PR whose `Cargo.lock` carried the
affected pin, for about 7 hours, until an unrelated PR's author fixed it in
passing.** Of the 7 run-level failures, 5 failed `Supply chain (cargo-deny)`:
`claude/compassionate-euler-hfl9hp` (run 34867438081, 2026-09-14T16:15:15Z),
`claude/epic-meitner-ftohjq` (run 34877258106, 17:51:24Z),
`dependabot/github_actions/taiki-e/install-action-2.87.11` (run 34881729206,
18:35:30Z), `dependabot/cargo/rust-deps-8077afb676` (run 34882678393,
18:44:48Z), and `dependabot/cargo/diesel-ecosystem-1a91744208` (run
34882747248, 18:45:27Z). Fetched each job's log: `compassionate-euler-hfl9hp`
and `epic-meitner-ftohjq` show the advisory explicitly —

```
error[vulnerability]: TLS 1.3 handshake messages incorrectly accepted across encryption level boundaries
  Cargo.lock:560:1
  rustls 0.23.43 registry+https://github.com/rust-lang/crates.io-index
  ID: RUSTSEC-2026-0285
  Advisory: https://rustsec.org/advisories/RUSTSEC-2026-0285
  Solution: Upgrade to >=0.23.45 (try `cargo update -p rustls`)
```

— pulled in transitively through `hyper-rustls`/`tokio-rustls`/`tonic`/
`reqwest`/`redis`/`lettre`/`tokio-postgres-rustls`, i.e. most of the
workspace's own network stack, not one narrow dependency edge. The other
three failures show the identical dependency-tree shape and the identical
`advisories FAILED` / exit-code-1 pattern truncated at the same point by this
tool's log-tail window — not independently confirmed by the advisory ID text
itself, but strongly consistent with the same cause, not a coincidence of
timing. **Not claimed as universal**: run 34871417092
(`claude/nifty-pascal-nebhrs`, 16:54:08Z — inside the failure window)
passed `Supply chain (cargo-deny)` cleanly, so whether a given open PR hit
this depended on whether that PR's own `Cargo.lock` had picked up the
specific `rustls 0.23.43` pin, not on a repo-wide, every-PR-fails outage.
The `dependabot/cargo/validator-0.21.0` `Supply chain (cargo-deny)` failure
at 23:11:06Z is a **different, pre-existing, unrelated failure** — the
`fuzz/Cargo.lock` `--locked` mismatch already flagged in the 2026-09-14
report — confirmed by its own log, not RUSTSEC-2026-0285.

**This was already found and fixed, independent of this ledger's own
tracking**, by PR #2790 (`2acf14d`, merged 2026-09-14T23:07:59Z, ~6h53m after
the earliest failure logged above), whose primary subject is an unrelated
`examples/cms` editor UX fix — a second commit on that same PR, titled "fix:
bump rustls to 0.23.45 to close RUSTSEC-2026-0285," ran `cargo update -p
rustls --precise 0.23.45` (patch-level, no API break) against both the root
workspace `Cargo.lock` and, in a follow-up commit, `fuzz/`'s separate
excluded-workspace lockfile. This is exactly the "red CI is work now" posture
this repo's own conventions already call for — whoever hit the failure fixed
it in place — and CI-natively confirmed: every `Supply chain (cargo-deny)`
run sampled after 23:07:59Z in this window (`dependabot/cargo/diesel-ecosystem`
re-run at 23:11:34Z, `claude/compassionate-euler-hfl9hp` re-run at
23:19:49Z, `dependabot/cargo/rust-deps` re-run at 23:23:44Z) passed. No
action needed from this pass beyond recording it accurately in the ledger
(below) as Tier-1 escape-analysis evidence, matching the MinIO/Docker-Hub
entry's own precedent.

**Finding 2 — `job_tracking_stores_integration::postgres_backend_persists_tracked_job_and_expires_it`
hit again, on the exact same assertion as its one prior occurrence.** Run
34934228774 (branch `claude/wizardly-wright-pkv2ly`, job `Test (Docker)`,
completed 2026-09-15T07:08:43Z): `test result: FAILED. 378 passed; 1 failed`,
panic at `autumn/tests/integration/job_tracking_stores_integration.rs:264:5`:
`"record should be past its configured TTL"` — identical panic site and
message to the 2026-09-11 first occurrence (run 34517281816). Per this
ledger's own stated policy for this entry ("escalate to a rerun campaign
only if a repeat signature appears"), this is that repeat signature: n=1→n=2,
same exact assertion both times, ~4 days apart, both organic (neither run
touches the job-tracking code itself — `claude/wizardly-wright-pkv2ly`'s own
PR title is about an unrelated AES-256-GCM cipher cache). See Diagnosis and
Treatment below.

Zero hits this pass on `live_upgrade` (all three tracked signatures) or
`cache_stampede` — checked across all 7 run-level failures and their full
logs; `sim_fault_plan` likewise zero hits, and (positive evidence, not
absence) `sim_fault_plan_pg::fail_db_checkout_fires_on_the_target_ordinal_under_transactional_isolation`
and `sqlite_replication_s3::replicates_to_and_restores_from_a_real_s3_endpoint`
both passed in the same `Test (Docker)` run that hit Finding 2 above, per
that job's own full log.

## 🔍 Diagnosis

**Finding 1 (RUSTSEC-2026-0285)**: neither a test defect nor a product
defect in this repo's own code — a genuine, externally-disclosed
vulnerability in a transitive TLS dependency, caught by exactly the
mechanism (`cargo-deny` against `Cargo.lock`) built to catch it, and fixed
by a patch-level version bump with no API break. Working as intended. The
only property worth naming for CI-health purposes: because `cargo-deny`
checks the *lockfile*, not the PR's own diff, a newly-published advisory
against a pinned transitive dependency fails the required gate on every open
PR carrying that pin simultaneously and without warning — structurally the
same shape as the MinIO/Docker-Hub outage this ledger already documents
(`docs/ci-health/quarantine-ledger.md`'s "offsite_backup"/"sqlite_replication_s3"
closed entry), just triggered by a security disclosure landing in the
advisory database instead of an image vanishing from a registry. No
coordination-gap escape this time — a single PR fixed it once, cleanly, no
colliding reconciliation commits — so this is a smaller, cleaner instance of
the same class, not a repeat of that specific incident's coordination
failure.

**Finding 2 (`job_tracking_stores_integration`)**: test-vs-product verdict
not re-rendered this pass — the ledger's existing entry already carries two
candidate mechanisms from the first occurrence (a discrete host clock step,
now demoted as unlikely; and a worker-refresh race, where
`run_job_handler_inner`'s `mark_running`/`settle_success` calls legitimately
rewrite `expires_at` via `PgJobTrackingStore::update` if either lands inside
the test's fixed 1200ms sleep window, pushing the TTL past the check point
with no clock disagreement needed at all). Nothing in today's log
contradicts either candidate, and this second occurrence doesn't by itself
distinguish between them — both remain hypotheses from reading the source,
not confirmed by an isolating experiment. What this occurrence *does* do is
clear the ledger's own bar for moving this out of "n=1, not campaigned":
a same-signature repeat is exactly the trigger condition the entry names for
escalating priority.

## 🔧 Treatment

No fix PR. Finding 1 needed none by the time this pass ran — already fixed,
CI-natively confirmed, nothing further to do beyond the ledger entry below.
Finding 2 does not clear the hard gate for a determinism PR yet: two organic
hits four days apart is a repeat signature, not a same-commit rerun-rate
baseline (`<k>/<n>`) — the hard gate calls for measuring before touching
anything, and jumping to a code change off n=2 organic hits, however
suggestive the repeated stack trace, would be exactly the "retry in
disguise" this role exists to refuse. Recommendation, unchanged in kind from
the `live_upgrade`/`cache_stampede` entries but now newly applicable to this
one too: this needs its own rerun harness — a `--test-threads=1` (or
otherwise isolated) loop of
`autumn/tests/integration/job_tracking_stores_integration.rs`'s
`postgres_backend_persists_tracked_job_and_expires_it` against a real
testcontainers Postgres, ≥20 iterations, to get a same-commit rerun rate
before any fix is attempted. Unlike the macOS cluster, this one doesn't need
a human-gated spend decision (it's a Docker-Postgres test, already running
in every Docker sweep) or a fresh workflow file — it can run locally with
the repo's existing tooling. Recording the recommendation here rather than
building the harness this pass, given the time this pass already spent
tracing Finding 1's log chain across five branches.

- **Recommendation for a human, unchanged from the last six passes**:
  dispatch `manual-macos-contention-check.yml` (`samples: "20"`) against a
  `trunk-dev` commit at or after `8fae8af`. ~162.5 hours idle since it became
  dispatchable is now a full week without even the partial evidence it could
  be producing for the `live_upgrade`/`cache_stampede`/`sim_fault_plan`
  investigation.
- **No action needed** on the `dependabot/cargo/validator-0.21.0` PR's own
  `fuzz/Cargo.lock` staleness — that PR's own responsibility to rebase, per
  the 2026-09-14 report.
- **Ledger updated**: new Tier-1 escape-analysis closed entry for
  RUSTSEC-2026-0285 (below), and the `job_tracking_stores_integration` entry
  updated with today's repeat hit and escalated status.

## 📊 Measurement

No rerun campaign this pass — organic sampling only, same as every prior
daily pass. This pass did **not** repeat the 2026-09-14 report's cancelled-run
job-level sampling (checking whether a `cancelled`-overall run hid a
job-level `failure`, as run 34774043482 did in that pass) — of the 50
cancelled runs in this window, none were checked at the job level, so a
hidden failure inside one of them (on any tracked signature, or on
`Supply chain`) cannot be ruled out for this pass, only for the 18 runs that
resolved to `success`/`failure` and were actually inspected. Flagged here so
this gap doesn't silently read as "checked and clean" the way the 2026-09-14
report's own first draft mistakenly did before its correction.

| Item | This pass | Status |
|---|---|---|
| `live_upgrade` (3 signatures) | No occurrence in any log actually inspected this pass | Unchanged; harness still undispatched, 7th pass |
| `cache_stampede` | No occurrence in any log actually inspected this pass | Unchanged, undiagnosed |
| `sim_fault_plan` | No occurrence; `sim_fault_plan_pg` sibling passed | Unchanged, undiagnosed |
| `job_tracking_stores_integration` | **Second organic hit, same exact signature** (run 34934228774) | **Escalated**: repeat signature per the entry's own stated trigger; still not campaigned |
| RUSTSEC-2026-0285 / `Supply chain (cargo-deny)` | 5 run-level failures observed within a ~2.5h failure span (16:15:15Z–18:45:27Z); fixed by PR #2790 at 23:07:59Z (~6h53m after the earliest failure, making the full incident-to-fix window ~7h); 3 post-fix runs confirmed green | Closed as Tier-1 escape analysis (below); no action needed |
| `manual-macos-contention-check.yml` dispatches | 0 → 0 | 7th consecutive idle pass, ~162.5h |
| Cancelled-run job-level check | Not performed this pass | Gap, not a zero-hit finding — see note above |

## 🔬 Reproduce

```
actions_list(list_workflow_runs, ci.yml, event=pull_request, status=completed,
             perPage=100, page=1)
# → filtered to created_at in [2026-09-14T08:00:19Z, 2026-09-15T09:39:00Z]
# each failure's jobs via list_workflow_jobs(run_id, filter=latest)
# each failing job's log via get_job_logs(job_id, return_content=true, tail_lines>=60)
#   (a 15-20 line tail truncates before the actual failure — the cargo-deny
#   job's post-job git/cleanup steps run after the failure line and are long
#   enough to push it out of a short tail)
```

Confirm the RUSTSEC fix (already landed, for the record):

```
git show 2acf14d -- Cargo.lock | grep -A3 'name = "rustls"'
# → version 0.23.43 → 0.23.45
git log origin/trunk-dev --oneline --since=2026-09-14T15:00:00 --until=2026-09-15T10:30:00 -- Cargo.lock deny.toml
```

Confirm the harness is still undispatched:

```
actions_list(list_workflow_runs, manual-macos-contention-check.yml, event=workflow_dispatch)
# → total_count: 0
```
