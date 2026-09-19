# ⛏️ Prospect: does `#[query_budget]`'s handle-tracking already reach job/scheduled-shaped code? (pursue: 4/4 fixtures caught, one small verified `autumn-macros` fix, vs. Keystone's ≤3-day spike line)

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
- **Update (second Codex review round) — a real soundness bug found and
  fixed, not just a documentation gap.** Checking one of the 3 defects named
  above against real code (`autumn-search/src/postgres.rs:365`,
  `PostgresSearchStore::write_documents`), the review pointed out its handle
  binds as `let mut conn = self.conn().await?;` — an async, fallible
  accessor — and that neither `expr_is_handle` nor `chain_root_is_handle`
  peeled `Expr::Await`/`Expr::Try` before checking the method name. Verified
  by reading both functions: confirmed correct. The practical effect is not
  a diagnostic ("this is unprovable") but a **silent false negative** —
  `conn` never enters the tracked-handle set at all, so
  `bind_all(...).execute(&mut conn)` inside the chunk loop at line 575 is
  never recognized as a query, and `#[query_budget]` on this function would
  report a clean build regardless of the real query count. This is strictly
  worse than the tool's own stated soundness contract
  ("Anything the analysis cannot read is reported, never assumed
  query-free") — this case was neither read nor reported, just silently
  dropped.

  Fixed in `autumn-macros/src/query_budget.rs`: `expr_is_handle` now peels
  `Expr::Await`/`Expr::Try` through a new, deliberately *narrower* helper
  (`awaited_expr_is_fresh_handle`) rather than recursing into the full
  `expr_is_handle`. The narrower helper matters — the first attempt at this
  fix (peeling through the full recursive checks, including
  `chain_root_is_handle`) caused two real regressions, both caught by
  **existing** tests before this PR's own fixtures could hide them:
  `query_budget_over_budget.rs` (a compile-fail trybuild fixture) and
  `query_budget::tests::a_deferred_repository_future_is_counted_once` (a
  `#[cfg(test)]` unit test) both started miscounting a plain `.len()` call
  on an already-awaited query *result* as a second query, because peeling
  let the awaited expression fall through to a bare `Expr::Path` and
  re-derive handle-ness from `chain_root_is_handle`'s unrelated "future
  built but not yet awaited" tracking. The shipped fix omits `Expr::Path`
  from the peeled check specifically to avoid this — see the inline
  comments on `expr_is_handle`, `awaited_expr_is_fresh_handle`, and
  `chain_root_is_handle` in the diff for the full reasoning.

  New regression fixture: `query_budget_await_try_accessor_n_plus_one.rs`,
  reproducing `write_documents`' exact shape (diesel-async
  `query.execute(&mut conn)` argument-style, `conn` bound via
  `self.conn().await?`). Confirmed the bug pre-fix (compiled clean — the
  false negative) and the fix post-fix (compile-fails with the standard
  N+1 diagnostic), by literally stashing and restoring the
  `autumn-macros` change and rerunning the trybuild suite both ways.

  **This changes the report's framing in one place:** "zero
  `autumn-macros` changes" (in the title's parenthetical and the Verdict
  below) was true through the first review round but is no longer the
  overall state of this PR — a small, targeted, test-covered fix landed
  as part of verifying a reviewer's finding. It does not change the
  **verdict** (still pursue, still no design-level engineering needed for
  the job/scheduled generalization question itself), but it does mean the
  walker is measurably more correct after this assay than before it, for
  any code — job/scheduled-shaped or not — that obtains a handle through
  an async/fallible accessor.
- **Update (third Codex review round) — the first fix over-promoted a
  bare `.await`.** Adding `Expr::Await`/`Expr::Try` to `expr_is_handle`
  peeled *both* shapes: `self.conn().await?` (the target) and a bare
  `self.conn().await` with no `?`. The bare shape yields `Result<Conn, E>`
  — the `Result` itself, not the handle inside it — but was being promoted
  to a handle anyway, so a later `result.is_err()` or `.unwrap()` on it got
  miscounted as a database query, a real false positive on otherwise valid
  code (verified by reading the match arms; a fallible accessor's `.await`
  alone never unwraps the `Result`, only `?` does). Fixed by removing the
  `Expr::Await` arm from `expr_is_handle` itself — only `Expr::Try` is
  peeled at that level, which still reaches the inner `Await` node through
  `awaited_expr_is_fresh_handle`'s own (safe, because already inside a
  confirmed `?`-unwrapped context) `Expr::Await` arm. New compile-pass
  fixture: `query_budget_bare_await_not_promoted.rs`, pinning that
  `result.is_err()` after a bare `store.conn().await` stays a
  `#[query_budget(0)]`-clean build. CHANGELOG entry added per this
  workspace's own contribution rule (a user-visible `#[query_budget]`
  behavior change under `## [Unreleased]` → `### Fixed`), flagged as
  missing by the same review pass.
- **Update (fourth Codex review round) — the same over-promotion bug, one
  layer deeper.** `awaited_expr_is_fresh_handle`'s own `MethodCall` arm
  copied `expr_is_handle`'s two-branch check verbatim: a `HANDLE_ACCESSORS`
  name *or* a `HANDLE_BUILDERS` name chained off a handle. The
  `HANDLE_BUILDERS` branch is wrong here for the same reason the plain
  `Expr::Await` arm was wrong in round three: reaching this helper at all
  means the call was awaited, and the query-cost counter's own rule for an
  awaited builder-named call is that it "really did run" as the terminal
  query ("a user finder may share a builder's name" — e.g. an app defining
  its own `async fn page(&self, n: i64) -> Result<Vec<Post>, _>`, colliding
  with the builder-refinement name `page` in `HANDLE_BUILDERS`). So
  `let rows = repo.page(1).await?;` was correctly counted as one query by
  the counter, but `awaited_expr_is_fresh_handle` *also* promoted `rows`
  itself to a handle, and a later `rows.len()` was miscounted as a second
  query — verified by reading the counter's own comment ("A builder name
  refines the next query rather than issuing one — unless the chain is
  awaited here, in which case the terminal call really did run"). Fixed by
  dropping the `HANDLE_BUILDERS` branch from `awaited_expr_is_fresh_handle`
  entirely — only a `HANDLE_ACCESSORS` name survives an await/`?` peel into
  a fresh handle, since those never issue a query even when awaited. New
  compile-pass fixture: `query_budget_awaited_builder_name_not_promoted.rs`.

- **Update (fifth Codex review round) — the same gap, a different unwrap
  spelling, in a real named file.** `expr_is_handle`'s new `Expr::Try` arm
  covers the `?` operator, but `autumn/src/seed.rs` documents its own
  canonical usage as `let mut db = ctx.conn().await.expect("db
  connection");` — `.expect(...)`, not `?`. Verified against the actual
  file: both the module doc example and `SeedContext::conn`'s own doc
  comment show this exact shape. `.expect(...)` (and `.unwrap()`) are
  ordinary method calls, not `Expr::Try` nodes, so `expr_is_handle`'s
  existing `MethodCall` arm never saw past them to the accessor
  underneath — the same silent-uncounted failure mode as every prior
  round, for a different piece of syntax. Fixed by adding a
  `RESULT_UNWRAP_METHODS = ["expect", "unwrap"]` check to `expr_is_handle`'s
  `MethodCall` arm: when the method is one of these two, the call itself
  is never counted as a query, but its receiver is checked against the
  same narrow `awaited_expr_is_fresh_handle` helper used for `?`. New
  compile-fail fixture: `query_budget_expect_accessor_n_plus_one.rs`,
  mirroring `seed.rs`'s exact doc-comment shape.

  Deliberately narrow, and said so in the code: `.ok()`,
  `.unwrap_or_else(...)`, `.map_err(...)?`, and other combinators remain an
  acknowledged, unaddressed gap — fixing every possible way to unwrap a
  `Result`/`Option` is not this assay's job, and chasing it indefinitely
  would turn a bounded review response into unbounded scope creep. `expect`
  and `unwrap` were fixed because they are the two spellings actually
  present in this codebase's own documented usage; anything else surfaces
  the same way every gap in this family has — silently, on a future
  audit or review — and gets the same treatment when it does.
- **Update (sixth Codex review round) — a real finding, verified, and then
  reverted rather than patched further.** The review correctly identified
  that `db.pool().get().await?` (a deadpool/bb8-style connection checkout —
  `autumn-cli`'s own generated scaffold tests emit exactly this at
  `autumn-cli/src/generate/scaffold.rs:14419`) is still not caught: `get`
  is not a recognized accessor name, and `awaited_expr_is_fresh_handle`
  only checked the outermost method name, never the receiver. A fix was
  written — recurse into the receiver for any non-accessor terminal name,
  so `get` inherits handle-ness from the `pool` accessor beneath it — and
  it did catch the checkout idiom.

  It also broke `query_budget_job_shaped_accessor_batched.rs`, an
  **existing, already-verified fixture from this same PR's first commit**:
  `state.db().find_recipients(&args.recipient_ids).await?` is syntactically
  identical in shape to `db.pool().get().await?` — "some name, chained off
  an accessor call, then awaited" — but the first is a genuine domain
  query (whose result must not become a handle) and the second is a
  connection checkout (whose result must). Recursing into the receiver
  cannot tell them apart: it fixed the checkout case and silently
  reintroduced round four's exact regression on the query case, caught by
  rerunning the full `compile_pass_tests` suite before the fix was
  committed.

  There is no purely syntactic signal available to this proc macro — no
  type information — that distinguishes "a checkout wrapper" from "a named
  query" once both share this shape. Rather than reach for another naming
  heuristic (a hardcoded list of checkout-sounding names like `get`,
  `acquire`, `checkout`, `take`, …, which would be both incomplete and a
  second parallel guess-list to keep in sync with `HANDLE_ACCESSORS`), the
  attempted fix was reverted and the gap documented in code as a known,
  acknowledged limitation. This is the first of the six review rounds
  where the honest answer was "verified, but not safely fixable this way"
  rather than a fix — consistent with Prospect's own standard that a
  correctly-scoped "no" is a valid outcome, not a lesser one, when the
  alternative is trading a rarer false negative for a more common false
  positive.
- **Update (seventh Codex review round) — a real gap, declined for a
  different reason: the fix itself is structural, not another heuristic.**
  Splitting round five's `.expect()`/`.unwrap()` shape across two
  statements — `let result = ctx.conn().await; let mut db =
  result.unwrap();` — is not caught: the receiver at the unwrap site is
  `Expr::Path("result")`, which `awaited_expr_is_fresh_handle` deliberately
  never matches (round three's fix for exactly this reason — matching a
  bare `Expr::Path` there is what caused `result.is_err()` to be
  miscounted in the first place). Confirmed by tracing the code; this is a
  real, verified gap.

  Unlike rounds two through six, closing it soundly is not a matter of
  peeling one more AST node or narrowing one more branch. It needs a
  *third* binding state alongside "is a handle" and "is not a handle" —
  "is a `Result` that becomes a handle once unwrapped" — carried from the
  first `let` to the second, which means threading a new tracked set
  through every place `handles` is already scoped and restored (`block`'s
  clone-and-restore-declared-names, `enter_binding_scope`, `rebind`, tuple
  destructuring). That is a structural change to the analyzer's state
  model, and this round's evidence is a constructed example, not a cited
  file:line the way rounds two (`postgres.rs:365`), five (`seed.rs`), and
  six (`scaffold.rs:14419`) each were. Declined for now and documented in
  code alongside round six's limitation — not because the finding is
  wrong, but because taking on a structural change to fix an
  unsubstantiated shape is exactly the "unbounded scope creep" this
  report's own round-five update already named as out of bounds.

  Seven review rounds have now each found one distinct edge of the same
  underlying shape (an awaited/unwrapped expression can mean three
  different things — a fresh handle, an unopened `Result`, or an executed
  query's result — and only the first should ever be promoted). Five
  rounds landed a real fix plus a regression fixture pinning it; two
  landed a verified, documented, and deliberately un-fixed limitation
  instead — once because the alternative fix was checked and found to cost
  more than it bought, once because the fix itself would be a structural
  change disproportionate to unsubstantiated evidence.

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
| `query_budget_await_try_accessor_n_plus_one.rs` (`self.conn().await?` shape) | compile-fail | **compiled clean pre-fix (confirmed false negative); compile-fail post-fix** | same diagnostic, naming `execute`, pointing at `for _id in ids` |
| `query_budget_bare_await_not_promoted.rs` (`store.conn().await` — no `?` — then `result.is_err()`) | compile-pass | **compile-pass** | clean build at `#[query_budget(0)]` |
| `query_budget_awaited_builder_name_not_promoted.rs` (`repo.page(1).await?` — `page` is a `HANDLE_BUILDERS` name — then `rows.len()`) | compile-pass | **compile-pass** | clean build at `#[query_budget(1)]` |
| `query_budget_expect_accessor_n_plus_one.rs` (`ctx.conn().await.expect(...)`, `seed.rs`'s documented shape) | compile-fail | **compile-fail** | same diagnostic, naming `execute`, pointing at `for id in ids` |

First run generated fresh `.stderr` snapshots via trybuild's standard
`wip/`-then-promote flow (no snapshot existed yet, matching how every other
compile-fail fixture in this suite was originally added); every subsequent
run, after moving the generated snapshots into `tests/compile-fail/`,
reproduced them byte-for-byte (`query_budget_compile_fail_tests ... ok`,
~5s incremental with the full 9-fixture family). `compile_pass_tests` (62
fixtures including all three controls) also passed clean. The
`autumn-macros` fix was additionally checked against the crate's own
`#[cfg(test)]` suite (`cargo test -p autumn-macros --lib`, 1087 tests) to
catch exactly the kind of regression the first attempt at this fix
introduced — see the stubs-list update above for how that regression was
found and corrected before this report's numbers were finalized.

Worst-case probing beyond the original two accessor shapes came from the
review itself, not from this assay's own design: the `.await?`-wrapped
accessor case was found by a reviewer reading real production code, not by
this assay's own adversarial-input pass. That is itself worth naming
plainly — the original apparatus's "no worst-case probing beyond this"
line (still true of the job/scheduled generalization question) undersold
how much of the surrounding walker code the accessor-tracking mechanism
actually touches, which the review exposed.

## 🏁 Verdict: **pursue**, with a correction to the premise

**4/4 signature/attribute-shaped fixtures caught the N+1 (2 signature-shaped,
2 with the real `#[job]`/`#[scheduled]` attributes), 1/1 control built
clean, with the generalization itself requiring zero `autumn-macros`
changes.** A 5th fixture, added from the review's own reading of real
production code, found a genuine soundness bug in the walker (async/fallible
accessor bindings silently untracked) — fixed with a small, targeted,
regression-tested change; see the stubs-list update above. Against the
pre-set line, this is an unambiguous pursue — but the mechanism is not the
one item 3 asked about. There is no such thing
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
   (already covered, confirmed by this assay — including through
   `.await?`, as of this PR's fix) vs. an app-specific accessor (still
   excluded, matches the doc's own named boundary). This is a reading task,
   hours not days.
2. **Annotate.** For the covered subset, add `#[query_budget(N)]` directly to
   the query-issuing function (not necessarily the `#[job]`/`#[scheduled]`
   entry point itself, if it's a plain helper one or more calls deep — the
   doc already supports annotating "plain helper functions, not just
   routes"). No further `autumn-macros` change required beyond this PR's fix.
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
cargo test -p autumn-macros --lib
```

Fixtures: `autumn/tests/compile-fail/query_budget_accessor_handle_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_job_shaped_accessor_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_real_job_accessor_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_real_scheduled_accessor_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_await_try_accessor_n_plus_one.rs`,
`autumn/tests/compile-fail/query_budget_expect_accessor_n_plus_one.rs`
(all six with pinned `.stderr` snapshots), and
`autumn/tests/compile-pass/query_budget_job_shaped_accessor_batched.rs`,
`autumn/tests/compile-pass/query_budget_bare_await_not_promoted.rs`,
`autumn/tests/compile-pass/query_budget_awaited_builder_name_not_promoted.rs`.
Wired into `autumn/tests/integration/compile_fail.rs`'s existing
`query_budget_compile_fail_tests` / `compile_pass_tests` functions. The
`autumn-macros` fix itself is `expr_is_handle`'s `Expr::Try` arm (no
`Expr::Await` arm at that level — see the third-review-round update above),
the `RESULT_UNWRAP_METHODS` check in its `MethodCall` arm (see the
fifth-review-round update above), plus the new
`awaited_expr_is_fresh_handle` helper (`HANDLE_ACCESSORS`
names only — no `HANDLE_BUILDERS` branch, see the fourth-review-round
update above) and `chain_root_is_handle`'s comment explaining why it
deliberately does *not* peel the same way, all in
`autumn-macros/src/query_budget.rs`.

## 🗄️ Dismantle

Unlike most Prospect apparatus, these fixtures are not thrown away: they are
real regression coverage for a real (if previously unstated) analysis
guarantee — that the walker's accessor tracking doesn't care what macro
wraps the function — and cost nothing to keep in the consolidated suite
where they already live. What does not get built is any change to
`autumn-macros` itself; this report is the map, not a foundation, for
whoever does the annotation-adoption work in item 3.
