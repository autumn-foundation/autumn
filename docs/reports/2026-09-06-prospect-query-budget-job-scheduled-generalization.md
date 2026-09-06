# ⛏️ Prospect: does `#[query_budget]`'s handle-tracking already reach job/scheduled-shaped code? (pursue: 2/2 fixtures caught, zero code change vs. Keystone's ≤3-day spike line)

## 🎯 Question

`docs/reports/2026-09-05-keystone-query-budget-coverage-gap-findings.md` (item
3) named a concrete, pre-scoped follow-up to its 8/8 finding: "a short spike
(≤3 days) checking whether the existing handle-tracking walker generalizes to
a `#[job]`/`#[scheduled]` function's own handle argument the same way it
already does for `#[get]` handlers. If it does, this closes 5/8 of the
observed [N+1] population at the same mechanism, same cost model. If it
doesn't generalize cleanly, that is itself useful evidence for why the
original scope-out was correct."

**Falsifiable question:** does `#[query_budget]`, unmodified, catch an N+1
reached through the handle-obtaining pattern a `#[job]`/`#[scheduled]`
function is actually shaped like in this codebase — a `state.db()`-style
accessor call inside the body, not a typed `Db`/`…Repository` parameter in
the signature? **Decision:** whether item 3 is a walker-engineering spike (as
framed) or a pure annotation-adoption task (the same bucket as items 1–2, "cheap
to close, no design work needed"). **Decider:** whoever picks up the Keystone
memo next (maintainer, or the Ledger/Bolt personas it names).

## ⚖️ Pre-registration

- **Pursue line:** a fixture matching a real `#[job]`/`#[scheduled]`
  signature shape, annotated with `#[query_budget(N)]` and containing a real
  N+1 through the accessor pattern, fails to build with the framework's
  standard N+1 diagnostic — with **no change to `autumn-macros`**. That would
  mean the walker already generalizes and the remaining work for 5/8 is
  wiring the annotation onto real call sites, not extending the analysis.
- **Kill line:** the same fixture builds clean (a false negative) or fails to
  build for an unrelated reason (macro-composition breakage between
  `#[job]`/`#[scheduled]` and `#[query_budget]`) — evidence the original
  scope-out is load-bearing and real engineering is needed.
- **Conditions:** trybuild fixtures added to the existing consolidated
  `integration_tests` binary (`autumn/tests/compile-fail/`,
  `autumn/tests/compile-pass/`), run via `cargo test -p autumn-web --test
  integration_tests query_budget_compile_fail_tests` / `compile_pass_tests`,
  matching every other `#[query_budget]` fixture's convention (local stub
  types, pinned `.stderr` snapshot). No changes to `autumn-macros` itself.
- **Time box:** same session (hours), well under Keystone's ≤3-day budget.
  Riskiest assumption first: whether the walker's accessor check is
  receiver-type-agnostic (works on *any* `.db()`/`.repo()` call, not just one
  rooted in a signature-typed handle) — read from
  `autumn-macros/src/query_budget.rs` before writing any fixture, then
  confirmed by compiling one.
- **Containment:** additive test fixtures only, on this session's designated
  branch; no production code path touched; the prototype is the fixtures
  themselves, which are cheap, real, and — unlike most Prospect apparatus —
  worth keeping as permanent regression coverage rather than dismantling (see
  Dismantle, below).

## 🔍 Prior art

- `docs/guide/query-budgets.md` ("Where the boundary sits"): documents that
  the analysis tracks a handle "through fields and conventionally-named
  accessors (`self.repo`, `state.db`, `app.pool()`)," and separately that "a
  repository pulled off an application-state extractor by an
  application-specific method (`state.posts()`)" sits **outside** the tracked
  boundary. The same page's "Scope of the first slice" lists "background
  jobs, `#[scheduled]` tasks" as deliberately excluded — the tension between
  these two statements is exactly what item 3 asked to resolve.
- Reading `autumn-macros/src/job.rs` and `autumn-macros/src/scheduled.rs`:
  both re-emit the annotated `ItemFn` completely unchanged (`quote! {
  #input_fn ... }`), with no wrapping `async` block or closure — unlike
  `#[secured]`/`#[step_up]`/`#[authorize]`/`#[throttle]` (which wrap the body)
  or `#[cached]` (which wraps in a closure), all of which the query-budget
  doc explicitly calls out as shapes the walker has to see through. This
  means there is **no macro-composition question at all** between
  `#[query_budget]` and `#[job]`/`#[scheduled]` — that half of item 3's
  framing was already answered by inspection, not worth spending apparatus
  on.
- Reading `autumn-macros/src/query_budget.rs` directly: `signature_handles`
  (line 1499) seeds the tracked-handle set only from parameters whose *type*
  matches `HANDLE_TYPES`/`…Db`/`…Repository` — `AppState` does not qualify,
  confirming a `#[job]`/`#[scheduled]` handler's own parameter is never
  seeded this way (structurally: `job.rs` enforces the handler signature is
  exactly `(AppState, Args[, JobContext])`, so it is not just uncommon for a
  job to take a typed handle argument — it's rejected). But `expr_is_handle`'s
  `Expr::MethodCall` arm (line 1127) returns `true` the moment the method
  name is in `HANDLE_ACCESSORS` (`db`, `repo`, `repository`, `pool`, `conn`,
  `connection`) — **with no check on the receiver at all**. `state.db()` is
  read as handle-producing regardless of what `state` is, or what macro
  wraps the function that contains it.
- Checked 3 of the 5 named background-job defects directly
  (`Mailer::send_list_mail` — `autumn/src/mail.rs:2096`,
  `PgSyncBackend::gc_tombstones` — `autumn/src/sync/server.rs:780`,
  `PostgresSearchStore::write_documents` — `autumn-search/src/postgres.rs:338`):
  none is itself a `#[job]`- or `#[scheduled]`-decorated function. All three
  are plain `&self` trait methods called from job/scheduled dispatch code
  elsewhere. `docs/guide/query-budgets.md` already states `#[query_budget]`
  "also works on plain helper functions, not just routes" — so for these
  three, item 3's own framing ("a `#[job]`/`#[scheduled]` function's own
  handle argument") does not even apply; the open question was never about
  the job/scheduled macros for the majority of the named defects, only about
  whatever function directly issues the query.
- No existing report or fixture tests `#[query_budget]` against an
  accessor-reached (non-parameter) handle; every existing compile-fail
  fixture uses a directly-typed `repo: PgPostRepository`-shaped parameter.
  This is a real gap in coverage, not a re-dig of a closed pit.

## 🧪 Apparatus

Three trybuild fixtures, added to the existing consolidated suite exactly
where every other `#[query_budget]` fixture lives (no new test harness):

- `autumn/tests/compile-fail/query_budget_accessor_handle_n_plus_one.rs` —
  control: an arbitrary two-argument function (non-`#[job]`-shaped) taking a
  local stub `AppState` (deliberately named so it does **not** match
  `HANDLE_TYPES`/`…Db`/`…Repository`), calling `state.db().find_author(id)`
  inside a `for` loop. `#[query_budget(1)]`.
- `autumn/tests/compile-fail/query_budget_job_shaped_accessor_n_plus_one.rs`
  — the real target: identical accessor pattern, but the signature is
  exactly the shape `#[job]` enforces — `async fn(state: AppState, args:
  SendDigestArgs)` — with the loop reading `args.recipient_ids`. `#[query_budget(1)]`.
- `autumn/tests/compile-pass/query_budget_job_shaped_accessor_batched.rs` —
  control: the same job-shaped signature and accessor, with the per-row
  lookup batched ahead of the loop (`state.db().find_recipients(&ids)`).
  Must build clean — proves the analysis is counting, not just always
  rejecting the job shape.

**Stubs list** (what was faked, and why it doesn't undercut the verdict):

- **Update (same-day PR review, Codex bot):** the first pass of this assay
  did not apply the real `#[job]`/`#[scheduled]` attribute macros — only
  functions matching their enforced signature shape. A review comment on
  PR #2546 correctly flagged that this could not prove the two macros
  actually *compose* (only that the mechanism generalizes to the shape),
  and that a break in either macro's expansion would go undetected. Two more
  fixtures were added to close that gap:
  `query_budget_real_job_accessor_n_plus_one.rs` (real `#[job(name = ...)]`,
  real `autumn_web::AppState`, a real `#[derive(Serialize, Deserialize)]`
  args struct) and `query_budget_real_scheduled_accessor_n_plus_one.rs`
  (real `#[scheduled(every = ..., name = ...)]`, single-argument `AppState`
  shape). Both compile-fail with the identical "classic N+1" diagnostic as
  every other fixture in this family. `db()` is added to the real
  `AppState` via a local extension trait rather than a real connection
  pool — the walker's accessor check is a syntactic name match
  (`autumn-macros/src/query_budget.rs`'s `expr_is_handle`), not a type
  resolution, so this is a faithful stand-in without pulling the `db`
  feature's runtime into a compile-time-only fixture. This was the one
  remaining instrument gap named below; it is now closed.
- Only 3 of the 5 named background-job defects were checked against the
  "conventionally-named accessor vs. app-specific method" distinction this
  assay turns on. `spawn_room_reaper_loop` (media plugin) and
  `pg_claim_next_job` were not read closely enough to classify. The "closes
  5/8" number in Keystone's memo is **not** independently confirmed by this
  assay — only that the walker mechanism itself is not the blocker.

## 📊 Assay

Two full runs of the consolidated `integration_tests` binary (`cargo test -p
autumn-web --test integration_tests`, default features `db,maud,htmx,...`):

| Fixture | Expected | Result | Diagnostic |
|---|---|---|---|
| `query_budget_accessor_handle_n_plus_one.rs` | compile-fail | **compile-fail** | `` `#[query_budget(1)]` cannot be proven: a database query (`find_author`) runs inside a loop `` — identical wording/shape to the existing `query_budget_n_plus_one.rs` route-handler fixture |
| `query_budget_job_shaped_accessor_n_plus_one.rs` | compile-fail | **compile-fail** | same diagnostic, pointing at `for id in args.recipient_ids` |
| `query_budget_job_shaped_accessor_batched.rs` | compile-pass | **compile-pass** | clean build |
| `query_budget_real_job_accessor_n_plus_one.rs` (real `#[job]`) | compile-fail | **compile-fail** | same diagnostic, pointing at `for id in args.recipient_ids` |
| `query_budget_real_scheduled_accessor_n_plus_one.rs` (real `#[scheduled]`) | compile-fail | **compile-fail** | same diagnostic, pointing at `for id in ids` |

First run generated fresh `.stderr` snapshots via trybuild's standard
`wip/`-then-promote flow (no snapshot existed yet, matching how every other
compile-fail fixture in this suite was originally added); every subsequent
run, after moving the generated snapshots into `tests/compile-fail/`,
reproduced them byte-for-byte (`query_budget_compile_fail_tests ... ok`,
~5s incremental with the 7-fixture family). `compile_pass_tests` (60
fixtures including the batched control) also passed clean in the same
session.

No worst-case/adversarial probing beyond this: the question is a binary
mechanism-exists/doesn't, not a performance or scale claim, so the two
fixtures plus their batched control are the whole admissible evidence set —
adding more shapes (nested accessors, `if`/`match` branches reaching
`state.db()`) would be re-confirming behavior the existing route-handler
fixtures already pin for the identical walker code path, not testing
anything specific to the job/scheduled shape.

## 🏁 Verdict: **pursue**, with a correction to the premise

**4/4 compile-fail fixtures caught the N+1 (2 signature-shaped, 2 with the
real `#[job]`/`#[scheduled]` attributes), 1/1 control built clean, zero
changes to `autumn-macros`.** Against the pre-set line, this is an
unambiguous pursue —
but the mechanism is not the one item 3 asked about. There is no such thing
as "a `#[job]`/`#[scheduled]` function's own handle argument" to generalize
to: `#[job]` structurally enforces `(AppState, Args[, JobContext])` and
`#[scheduled]` enforces `(AppState)`; neither can ever take a typed
`Db`/`…Repository` parameter the way a route handler does. What actually
carries the coverage into job/scheduled-shaped code is the
**conventionally-named-accessor path** (`state.db()`, `self.repo()`, …),
which the walker already applies with **no check on the receiver's type or
the enclosing attribute** — it is not route-macro-specific and never was.

That reframes the real cost: item 3 is not a walker-engineering spike at
all. It is the **same bucket as Keystone's items 1–2** — pure annotation
adoption on an already-capable mechanism — for exactly the population where
the query-issuing call is reached through one of the six recognized accessor
names (`db`, `repo`, `repository`, `pool`, `conn`, `connection`). Where a
defect's real code instead goes through an app-specific method
(`state.posts()`, as the doc's own excluded example names), the walker still
won't see it — that boundary is real and this assay does not touch it.

## 💰 Cost to productionize

For whoever picks up Keystone's item 3 next:

1. **Audit, not build.** For each of the 5 background-job defects (only 3
   checked here — see stubs list), read the actual handle-obtaining call at
   the query site and classify it: reached via a `HANDLE_ACCESSORS` name
   (already covered, confirmed by this assay) vs. an app-specific accessor
   (still excluded, matches the doc's own named boundary). This is a reading
   task, hours not days.
2. **Annotate.** For the covered subset, add `#[query_budget(N)]` directly to
   the query-issuing function (not necessarily the `#[job]`/`#[scheduled]`
   entry point itself, if it's a plain helper one or more calls deep — the
   doc already supports annotating "plain helper functions, not just
   routes"). No `autumn-macros` change required.
3. **Update the doc.** `docs/guide/query-budgets.md`'s "Scope of the first
   slice" currently reads as a blanket exclusion of "background jobs,
   `#[scheduled]` tasks" — this assay shows that's only true for the
   app-specific-accessor subset. Worth a line correcting it so the next
   reader doesn't re-ask this question.
4. **Gates this must clear:** none beyond the existing `#[query_budget]`
   soundness contract — this is additive annotation on an unmodified
   analysis, the same "PR-sized, two-way door" characterization Keystone's
   memo already gave items 1–2.

Reversibility: same as Keystone's own assessment of items 1–2 — hours, fully
reversible, decidable in a PR.

## 🔬 Reproduce

```bash
cargo test -p autumn-web --test integration_tests query_budget_compile_fail_tests -- --nocapture
cargo test -p autumn-web --test integration_tests compile_pass_tests -- --nocapture
```

Fixtures: `autumn/tests/compile-fail/query_budget_accessor_handle_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_job_shaped_accessor_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_real_job_accessor_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_real_scheduled_accessor_n_plus_one.rs`
(all four with pinned `.stderr` snapshots), and
`autumn/tests/compile-pass/query_budget_job_shaped_accessor_batched.rs`.
Wired into `autumn/tests/integration/compile_fail.rs`'s existing
`query_budget_compile_fail_tests` / `compile_pass_tests` functions.

## 🗄️ Dismantle

Unlike most Prospect apparatus, these fixtures are not thrown away: they are
real regression coverage for a real (if previously unstated) analysis
guarantee — that the walker's accessor tracking doesn't care what macro
wraps the function — and cost nothing to keep in the consolidated suite
where they already live. What does not get built is any change to
`autumn-macros` itself; this report is the map, not a foundation, for
whoever does the annotation-adoption work in item 3.
