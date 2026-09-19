# 🚦 Semaphore: CI health follow-up, 2026-09-05

Follow-up to `docs/reports/2026-09-04-semaphore-ci-health-census.md`. That
census closed with two open items — a macOS-vs-ubuntu timing cluster on
three tests, pending a load-faithful rerun campaign, and "escape mining: not
run this pass." This pass runs the escape mining (free — GitHub API only, no
new CI spend) and finds a real, structural one; ships the rerun campaign as a
harness the census could only describe; and does not open a fix PR, because
the hard gate for one still isn't cleared.

## 🎯 Verdict path

Unchanged from the 2026-09-04 census, with one correction: as of this
report, **`trunk-dev`'s own tip (`66106a5c`) is red** — `Test
(ubuntu-latest/windows-latest/macos-latest)` all fail 4 `autumn-macros` unit
tests with `compile_error!{"route macros can only be applied to
functions"}` (confirmed via the Actions API: push run on `66106a5c`,
conclusion `failure`). This blocks every open PR's ability to prove a clean
merge against current `trunk-dev`, independent of what each PR itself
changes — already confirmed on PR #2510 by that PR's own author. Two fix
PRs are open (#2517, #2518, see Diagnosis) and this report does not
duplicate a third.

## 🌡️ Symptom — an escape, found by git ancestry, not sampling

**Harness**: `git merge-base --is-ancestor`, GitHub PR API (`base.sha`), zero
CI spend. This is Tier 1 evidence in the sense that matters most — it isn't
a rate, it's a structural fact about the commit graph, deterministically
checkable by anyone with the repo:

```
$ git merge-base --is-ancestor f29d4b4a 02177caf && echo ancestor
ancestor
$ git log --oneline f29d4b4a..02177caf | wc -l
4
```

**PR #2488** ("Gate `#[secured]`/`#[step_up]`/`#[throttle]` before body
extraction", #1668) merged into `trunk-dev` at `2026-09-04T18:24:15Z`. Its
recorded PR `base.sha` is `f29d4b4a` — a commit that is **4 commits behind**
`02177caf` (**PR #2484**, "Fix OpenAPI response schema loss when body guards
expand before route macro", merged `2026-09-04T09:28:23Z`, over 9 hours
*before* #2488 merged). #2488's branch was opened `2026-09-03T18:53:12Z`,
before #2484 even existed, and — per its `base.sha` — was never updated
against `trunk-dev` before it merged. Its own CI run therefore validated
`#2488`'s diff against a `trunk-dev` state that did not yet contain #2484's
change to `api_doc::infer_response_body`, not the `trunk-dev` state the
merge actually produced. The two changes are individually correct and each
passed its own PR's tests; they do not compose, and nothing in the pipeline
ever ran them together before merging #2488. `trunk-dev` went red the moment
the second one landed, and stayed red for hours before anyone noticed —
PR #2517's own description opens with "`trunk-dev` is currently red on all
three `Test` platforms... This blocks CI on every open PR."

This is exactly Law 1's shape: **#2488's green was true of a state that
never existed on `trunk-dev`.** The check that gated its merge answered a
question ("does this diff work against the base I last tested?") that is
not the question merging actually asks ("does this diff work against
current `trunk-dev`?"). I could not find a branch-protection setting exposed
through the tools available to this session (no `get_branch_protection`
equivalent in the connected GitHub MCP server) to confirm directly whether
"require branches to be up to date before merging" is enabled — the
`base.sha` staleness is the evidence, not a settings read — so file this as
a strong inference, not a confirmed setting, and verify it in the repo's
branch protection UI before treating the recommendation below as fully
diagnosed.

**A second, cheaper cost from the same incident**: PR #2517 and PR #2518
were opened **87 seconds apart** (`01:41:45Z` / `01:43:12Z`), by the same
author, fixing the **identical** root cause with different mechanisms — one
teaches the parser to accept a guard's leading preamble items
(`parse_async_handler_with_preamble`), the other re-emits a redundant marker
const from the two guard macros that were missing it. Both are `mergeable_state: clean` as of this report. Neither references the other. This
isn't a CI-suite defect, but it's the same failure-fatigue family Law 3
warns about: two independent full review-and-fix cycles spent on one bug
because nothing signaled "already being fixed" — cheap to avoid with a
comment on the tracking issue, not something this report can fix by itself.

Continuing evidence on the 2026-09-04 macOS cluster: no new organic
`pull_request` CI failures matching `live_upgrade` / `cache_stampede` /
`sim_fault_plan` since the previous census's sampling window closed
(10:12 UTC 2026-09-04) — but `trunk-dev` being red for large stretches of
the last ~20 hours (see above) means `test` jobs on affected PRs were often
failing on the unrelated `autumn-macros` error before ever reaching the
`hot-upgrade` crate or the `integration_tests` binary, which suppresses the
sample rather than confirming absence. Treat this as "no new information,"
not "the cluster went away."

## 🔍 Diagnosis

**Root-cause category (escape)**: merge-queue/branch-protection gap —
CI's required checks can pass against a stale base and still gate the
merge, per the `base.sha` evidence above. **Test-vs-product verdict**: this
is a pipeline-configuration defect, not a test defect — no individual test
is flaky or wrong here; the *gate* let two individually-green diffs combine
into a red `trunk-dev` with nothing re-verifying the combination.

**Root-cause category (trunk-dev breakage itself, already diagnosed by
others)**: a genuine product bug, not a flake — #2517's and #2518's own
descriptions independently converge on the same mechanism: `#2488` moved
`#[secured]`/`#[step_up]`/`#[throttle]`'s runtime checks into sibling
`FromRequestParts` gate items instead of the handler body, but `#2484`'s
schema-recovery logic (`api_doc::infer_response_body`) and every guard
macro's own item-parser were still written for the single-`ItemFn` shape
`#2488` stopped producing. This is not this report's fix to make — two
correctly-diagnosed, non-duplicate-of-mine fix PRs are already in flight —
but it is worth recording as confirmation that the census's "test-vs-product
verdict rendered first" discipline matters even for bugs Semaphore doesn't
personally triage: neither #2517 nor #2518 proposes touching a test's
tolerance to route around the compile error, both fix the macro codegen
itself.

## 🔧 Treatment

**No fix PR opened.** Nothing here clears the hard gate for one: the escape
is a configuration/process gap I can diagnose but not fix without changing
branch protection, which this role must ask before touching; the
`trunk-dev` breakage already has two adequate, non-duplicate-needing fix
PRs in flight; and the macOS timing cluster still has no rerun-campaign
evidence (a fix would be laundering a timeout/tolerance change without the
baseline this role requires).

What *is* shipped, as a harness (Acceptable outcome #4):

1. `.github/workflows/manual-macos-contention-check.yml` — the Tier 1
   load-faithful rerun protocol the 2026-09-04 census could only describe,
   now a `workflow_dispatch` job a human can fire with one click: 10 (or 5,
   or 20) independent fresh `macos-latest` VMs, each running the unfiltered
   CI command (`cargo test --workspace --no-fail-fast`) against a pinned
   commit, classifying each of the three target tests' result per sample and
   uploading the full log. It does not run automatically and this report
   does not dispatch it — 10 macOS runners is real spend, and "new CI
   spend" is this role's own ask-before item. **Needs a sign-off to run,
   and needs a green `trunk-dev` commit to pin to** (see the workflow's own
   `sha` input description).
2. `docs/ci-health/quarantine-ledger.md` — the formal ledger with an intake
   form (test, owner, diagnose-by date, rerun baseline, mechanism, linked
   issue, skip mechanism) that the 2026-09-04 census flagged as this repo's
   "one open admissions gap." Opens with zero open entries (the one
   pre-existing quasi-quarantine, `cancelled_release_does_not_leak_lock`,
   was already fixed and un-quarantined in #2479) plus the three
   under-investigation macOS tests tracked without a skip, since none of
   them has been quarantined — only investigated.

## 📊 Measurement

No before/after rerun statistics this pass — none of the three tests above
was touched, and the escape finding is a one-time structural fact (git
ancestry), not a rate that needs an N. Ledger delta: 0 → 0 open entries, 0 →
1 closed entry (backfilled), 0 → 3 under-investigation entries tracked.

## 🔬 Reproduce

Escape finding:
```
git fetch origin trunk-dev
git merge-base --is-ancestor f29d4b4a 02177caf && echo "confirmed stale base"
git log --oneline f29d4b4a..02177caf
```

macOS contention campaign: dispatch
`.github/workflows/manual-macos-contention-check.yml` with `sha` pinned to a
`trunk-dev` commit where `cargo test --workspace` completes (i.e. after
#2517 or #2518 merges) — **requires sign-off first, per this role's
ask-before rule on new CI spend.**

## Recommendation (needs a human, not this report)

Confirm and, if unset, enable "require branches to be up to date before
merging" (or equivalent — a merge queue that always re-tests the actual
merge result) on `trunk-dev`'s branch protection. That single change would
have converted this incident from "`trunk-dev` red for hours, two duplicate
fix PRs" into "`#2488`'s own CI catches the conflict before merge." This is
listed under this role's own "ask before: changing merge requirements,
required checks, or branch protection" — flagged here, not changed here.
