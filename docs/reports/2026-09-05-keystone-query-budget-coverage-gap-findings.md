# 🏛️ Keystone [findings]: `#[query_budget]`'s documented scope gap matches 8/8 recent Ledger N+1 fixes

- Status: Findings memo (not an RFC — see Reversibility)
- Date: 2026-09-05
- Author: Keystone (architecture review agent)

## 🎯 Scope

System boundary examined: how N+1/unbounded-scan defects enter this codebase
and how they're caught, using the compile-time `#[query_budget]` gate
(`docs/guide/query-budgets.md`, `autumn/tests/compile-fail/query_budget_n_plus_one.rs`)
as the reference mechanism, against the Ledger persona's own audit history.

Reproduce: `git log --oneline | grep -i ledger` for the fix commits; the
per-fix write-ups live under `docs/reports/2026-0{8,9}-*-ledger-*`. Scope
check for the two named adoption gaps:
`grep -rn 'query_budget\|query_cost\|query_exempt' autumn-cli/src/generate/scaffold.rs autumn-admin-plugin/src` —
zero hits in both, confirmed 2026-09-05.

## 📈 Evidence (Tier 2 — repository record)

Eight independent N+1/unbounded-scan defects, each found by a dedicated
manual profiling audit (fixture + `EXPLAIN (ANALYZE, BUFFERS)` + targeted
patch) and each in a **different** subsystem, over 20 days:

| Date | Target | Shape | Reachable via `#[query_budget]` today? |
|---|---|---|---|
| 08-14 | `pg_claim_next_job` (job runtime) | expensive single statement, no loop | No — not a query-count defect at all (see below) |
| 08-15 | `Mailer::send_list_mail` | loop-shaped, mail send path | No — invoked from job/mail dispatch, no `Db`-typed route handler in the call chain |
| 08-16 | scaffold `index`/nested-list codegen | loop-free but unbounded (`SELECT … ORDER BY id`, no `LIMIT`) inside a real `#[get]` route handler | **Technically yes — but never wired in**: `autumn-cli`'s scaffold generator emits zero `#[query_budget]`/`#[query_cost]`/`#[query_exempt]` annotations on any generated route (grep, 0 hits) |
| 08-18 | `PgSyncBackend::gc_tombstones` | loop-shaped, operator-triggered maintenance sweep | No — not a route handler |
| 08-25 | `PostgresSearchStore::write_documents` | loop-shaped, backfill job body | No — explicitly "background jobs" (documented exclusion) |
| 08-31 | `AdminModel::execute_action` bulk delete | loop-shaped, inside a real `POST /admin/{slug}/actions` route | **Technically yes — but never wired in**: `autumn-admin-plugin` has zero query-budget annotations anywhere (grep, 0 hits); doc also lists "plugin code" as excluded |
| 09-01 | media-room reaper phase 2 | loop-shaped, `spawn_room_reaper_loop` background tick | No — explicitly "background jobs" (documented exclusion) |
| 09-03 | `ledger_as_of`/`ledger_diff` | loop-free but unbounded (`SELECT … ORDER BY seq`, no `LIMIT`/`as_of` filter) | No — not a query-count defect at all (see below) |

8/8. Not one of these would have failed a `cargo build` under the tool that
already exists in this repository for exactly this defect class, and the
reasons split into two clean buckets, both already named in
`docs/guide/query-budgets.md` by whoever wrote it:

1. **5 of 8** (08-15, 08-18, 08-25, 09-01, and 08-14's *shape* if it had been
   loop-based) sit inside background jobs, scheduled tasks, or a maintenance
   sweep — the doc's own "Scope of the first slice" section already excludes
   these **deliberately**. This was a considered decision at ship time, not
   an oversight — Keystone is not re-litigating it here, only pointing out
   it is where over half the audited defects have landed since.
2. **2 of 8** (08-16, 08-31) are real route handlers the tool's static
   analysis can already reach — and would have refused to build, per the
   documented "loop with a per-row query → unbounded → build fails"
   behavior — except neither the CLI scaffold generator nor the admin
   plugin ever applies the annotation. This is a pure adoption gap, not a
   technical limitation of the analysis.
3. **2 of 8** (08-14, 09-03) are not N+1 at all — one expensive *single*
   statement each, no loop. `query_budget` counts statement count, not
   result-set size or buffer cost, by explicit design ("this slice is query
   count only... allocation, CPU, and latency/cost budgets" are named as
   future work, not this tool's job). These two need a different mechanism
   entirely, not a wider `query_budget`.

## 🧭 Do nothing / decide later

At the current rate (8 in 20 days, ~1 every 2.5 days), this class of defect
keeps being caught only by a dedicated manual audit-and-fix cycle per
subsystem rather than at merge time — a real, currently-paid cost, not a
projection. Nothing about leaving this alone is unsafe: each instance found
so far has been caught and fixed before causing a production incident (no
Tier-1 data exists either way — this is a framework, not an operated
service). The cost is engineering/agent time spent re-discovering the same
mechanism per subsystem, not correctness risk sitting in production today.

## 💡 Mechanism

`#[query_budget]` is a working, already-shipped fitness function for exactly
this defect class, but its blast radius is smaller than the defect
population for two independent, unrelated reasons: an adoption gap (2/8,
cheap to close, no design work needed) and a deliberate scope boundary
(5/8, background/job/plugin code, previously scoped out on purpose). Neither
overlaps with the remaining 2/8, which are a different bug shape the tool
was never meant to catch.

## 🔧 Recommendation — not a decision, and deliberately not an RFC

**Reversibility: two-way door, hours-to-low-single-digit-days per item.**
Every item below is additive to an already-shipped, already-reversible
mechanism (add an annotation to generated code; extend an existing `syn`
walker to one more macro's entry point). None crosses a data-ownership,
team, or external-interface boundary. Per this framework's own rule —
*"Deciding two-way doors by RFC... if reversal costs under ~2
engineer-weeks, the implementing team decides it in a PR description"* —
none of this clears the bar for an RFC. It is recorded here as a findings
memo because the connection between 8 separately-reported bugs and one
existing tool's documented scope section had not been made anywhere before
this pass — each fix was correctly treated as a one-off by the report that
found it.

Concrete, PR-sized items for whoever picks this up next (maintainer, or the
Ledger/Bolt personas):

1. Have `autumn-cli`'s scaffold generator emit `#[query_budget(N)]` (or the
   appropriate `#[query_cost]`) on generated index/show/nested-list
   handlers by default. Closes the 08-16 class at generation time, for
   every future scaffold, not just the one instance already fixed.
2. Annotate `AdminModel::execute_action`'s default per-action loop (and
   audit the rest of `autumn-admin-plugin`'s routes) the same way — the doc
   already states the macro works on "plain helper functions, not just
   routes," so this is annotation, not new capability.
3. Revisit the "background jobs / scheduled tasks / plugin code" exclusion
   as a scoped follow-up, not a re-opening of the whole feature: a short
   spike (≤3 days, within this framework's own spike bar) checking whether
   the existing handle-tracking walker generalizes to a `#[job]`/`#[scheduled]`
   function's own handle argument the same way it already does for `#[get]`
   handlers. If it does, this closes 5/8 of the observed population at the
   same mechanism, same cost model. If it doesn't generalize cleanly, that
   is itself useful evidence for why the original scope-out was correct.
4. Leave 08-14 and 09-03 alone under this tool. They need the
   latency/cost-budget extension `query-budgets.md` already names as its own
   "north star" — a separate, larger effort this memo is not proposing.

## ⚖️ Alternatives considered

- **Do nothing** — legitimate; see baseline above. The cost is real but
  small per instance and has not caused a production incident to date.
- **Write a generic CI/clippy lint for "query call inside a loop"** —
  rejected: this framework already built a more precise, purpose-specific
  version of exactly this check (`#[query_budget]`, which understands
  `preload`, transactions, and repository chains); a second, cruder,
  parallel mechanism would just be two things to keep in sync.
- **RFC to formally re-scope `#[query_budget]` to cover jobs/plugins** —
  rejected per Reversibility above: this is a two-way door, decidable in a
  PR, and elevating it to architecture review would spend the reviewing
  maintainer's scarce attention on a decision whose wrongness is cheap.

## 📊 Trigger to revisit as something bigger

If, after items 1–3 ship, this same defect class keeps appearing in code
paths *already covered* by the widened gate (i.e., the gate has false
negatives, not just gaps in coverage), that would be new evidence the
mechanism itself — not just its adoption — has a hole, and would clear this
framework's evidence bar for a real design conversation. Absent that, no
further architecture-level action is implied by this memo.
