# Unified Data-Retention Policy for Framework-Owned Data (#1605)

Planning record for the slice that gives every framework-owned persistent
dataset a declared, enforced, inspectable retention window.

## 1. Problem Restated

Autumn creates tables and stores its users never asked for: `autumn_jobs`,
`autumn_job_tracking`, `autumn_experiment_assignments`, idempotency records,
webhook replay markers, sessions, audit archives. Being batteries-included
means owning their lifecycle. Today retention is two isolated knobs
(`jobs.tracking.ttl_secs`, `idempotency.ttl_secs`) and one hard-coded sweep;
everything else grows forever.

## 2. Brainstorming — Candidate Designs

| # | Idea | Verdict |
|---|---|---|
| A | One `[retention]` config section + a dataset registry + one fleet-coordinated in-process sweep task + a CLI report/dry-run/purge. | **Chosen.** Directly maps onto every AC. |
| B | Reuse `#[repository(..., retention(...))]` (#1342) by declaring framework models. | Rejected: framework tables are not `#[model]`s, and three of the seven datasets are not Postgres at all. |
| C | Per-subsystem knobs (`jobs.retention`, `experiments.retention`, …). | Rejected by AC #1 ("a *single* documented configuration section"); it is the status quo with more knobs. |
| D | CLI-only purge plus a documented cron recipe. | Rejected by AC #2 ("no external cron required"). |
| E | Postgres partition-by-range + `DROP PARTITION`. | Rejected: a migration burden on every existing deployment; AC promises "zero migration burden". |
| F | Database TTL extensions (`pg_cron`, `pg_partman`). | Rejected: not available on every managed Postgres; Autumn must work on plain 14+. |
| G | A `Prunable`-style trait operators implement per dataset. | Rejected: framework datasets are known at compile time; asking the operator to implement anything defeats the point. |

Adjacent ideas kept from the brainstorm and folded into A:

- **Two enforcement mechanisms, one policy.** Postgres-backed datasets are
  *sweep-enforced* (a scheduled `DELETE`). TTL-native stores (Redis/memory
  idempotency, webhook replay, sessions) are *TTL-enforced*: the window caps
  the TTL at write time. One config surface, honest per-dataset mechanics.
- **The sweep audits itself.** AC #6 wants dataset + cutoff + rows removed on
  every run; that record is the compliance artifact, so it goes through the
  same `AuditLogger` an operator already ships to their SIEM.
- **Legal hold is a veto, not a filter.** A GDPR `retain` registration on a
  dataset's backing table skips the whole dataset with a reason, rather than
  trying to subtract rows.
- **Unset means untouched.** Every window defaults to `None`; a config with no
  `[retention]` section produces no sweep task at all.

## 3. Reverse Brainstorming — How Could This Go Badly Wrong?

Deliberate enumeration of failure modes, each with the mitigation that is
actually implemented (or the explicit decision not to).

| # | Failure mode | Mitigation |
|---|---|---|
| R1 | The sweep silently deletes production data an operator did not intend. | Every window is opt-in and unset by default; no window ⇒ no task registered. `--dry-run` reports counts before anything is deleted. |
| R2 | A single unbounded `DELETE` locks a hot table / spikes replication lag. | Batched deletes (`DELETE … WHERE id IN (SELECT … LIMIT n)`), bounded batches per run, resuming next tick. |
| R3 | Deleting rows a legal hold requires keeping. | GDPR `ErasureStrategy::Retain` on a dataset's table vetoes the whole dataset, with the hold reason in the report and the audit record. Covered by a test. |
| R4 | Deleting *live* rows (an enqueued job, an in-flight tracking record). | `job_history` restricts to terminal statuses (`completed`/`failed`) with a non-null `finished_at`; `job_tracking` measures from `updated_at`. |
| R5 | Breaking published `autumn-web` consumers. | Everything additive: new config section (all `Option`), new module, new trait methods with defaults. The one struct field added (`AuditEvent::metadata`) is `#[serde(default)]` and no code in-tree or in the public API constructs `AuditEvent` by struct literal (only `AuditEvent::new`). |
| R6 | Conflicting with `jobs.tracking.ttl_secs` / `idempotency.ttl_secs`. | Documented rule: **the shorter bound wins** (`effective = min(subsystem ttl, retention window)`). Both knobs keep their exact meaning; the unified window is a second, independent ceiling. Tested. |
| R7 | Every replica sweeps at once, N× the delete load. | Registered as a `TaskCoordination::Fleet` scheduled task, reusing the existing scheduler lease. |
| R8 | The sweep panics/errors and takes the app down. | Per-dataset errors are captured into the outcome and logged; one dataset failing never aborts the others or the task. |
| R9 | The CLI reports a policy different from the one the app enforces. | The CLI does not re-implement anything: it compiles and runs the app binary in a one-shot mode that calls the *same* engine function the scheduler calls. |
| R10 | An operator sets a window that can never take effect. | Config validation rejects zero/unparseable durations; the report shows the effective window and its source so a window longer than the subsystem TTL is visible as such. |
| R11 | The audit archive purge corrupts the archive. | Purge is a streaming rewrite to a sibling temp file + `fsync` + atomic rename, under the sink's existing write lock. Unparseable lines are **kept**, never dropped. |
| R12 | A dataset gets added later and is silently missed. | The dataset registry is a single `const` slice; a test asserts the config surface and the registry agree, and the docs table is generated from the same list. |
| R13 | The sweep runs in the `web` role and duplicates worker effort. | It is an ordinary scheduled task, so it inherits the existing `role.runs_workers()` gate. |
| R14 | Timestamps compared against the app's clock rather than the database's. | Postgres cutoffs are computed as `NOW() - INTERVAL` server-side, so a skewed app clock cannot widen a window. |

## 4. Six Thinking Hats

**White (facts).** Seven datasets named by AC #1. Three are Postgres tables
(`autumn_jobs`, `autumn_job_tracking`, `autumn_experiment_assignments`); three
are TTL-native stores (idempotency, webhook replay, sessions) with
memory and Redis backends; one is a file archive (`JsonlFileAuditSink`).
`autumn/src/retention.rs` already exists for *app* models (#1342) and is
explicitly out of scope here. `TaskInfo`/`Schedule::FixedDelay` +
`TaskCoordination::Fleet` already give fleet-coordinated recurring work.
`AppState::pool()`, `AppState::config_arc()`, `state.extension::<T>()` are the
handles the engine needs.

**Red (instinct).** The temptation is to bolt a `DELETE` onto each subsystem.
That is what the issue calls the trap. The thing that will actually make an
operator trust this is the *report* — being able to run one command and see
every dataset, its window, where the window came from, and how many rows are
eligible right now. Design the report first; the sweep is the easy half.

**Black (risk).** Deleting data is irreversible and the blast radius is
production. Every risk in §3 is a black-hat finding. Two stand out: R3 (legal
hold) is a compliance failure, not just a bug, so it gets a test and a veto
rather than a filter; and R9 (CLI/app divergence) is the classic way this
class of feature rots — solved structurally by having exactly one engine.

**Yellow (upside).** One config section replaces "discover six private knobs
and hand-write cron". The report answers "how long do you keep this?" without
reading source. Because the engine is one function, `autumn db retention`,
the scheduled sweep, and the tests all exercise the same code path.

**Green (creativity).** The `min(subsystem ttl, window)` rule turns a
potential conflict into a feature: the unified section is a *ceiling* across
all datasets and the per-subsystem knobs stay as fine-grained floors.
Reporting `enforced_by` per dataset (`sweep` vs `backend_ttl` vs `file_purge`)
makes the heterogeneity legible instead of hiding it behind a fake sweep that
always reports zero.

**Blue (process).** Order of work, each red→green→refactor:
1. `[retention]` config section (parse, defaults, env, validate).
2. Dataset registry + effective-window/precedence pure functions.
3. Legal-hold veto.
4. Postgres sweepers (count + batched purge).
5. Audit-archive purge (`AuditSink::purge_before`) + `AuditEvent::metadata`.
6. TTL capping for the TTL-native datasets.
7. Scheduler wiring in `app.rs` + the one-shot mode.
8. `autumn db retention` CLI.
9. Docker integration tests.
10. Docs page enumerating every framework-owned dataset.

## 5. Decisions

- Module: `autumn/src/data_retention.rs` (`autumn_web::data_retention`), kept
  distinct from `autumn_web::retention` (app models, #1342).
- Config: `[retention]`, one `Option<String>` duration per dataset plus
  `sweep_interval`. Unset ⇒ today's behavior exactly.
- Task: `autumn-retention-sweep`, `Schedule::FixedDelay(sweep_interval)`,
  `TaskCoordination::Fleet`, registered only when ≥1 window is set.
- Precedence: `effective = min(subsystem ttl, unified window)`, documented and
  tested; existing knobs unchanged.
- Legal hold: `GdprRegistry` `Retain` on the dataset's table skips it.
- Audit: one `AuditEvent` per dataset per run, `action = "retention.sweep"`,
  `target_resource_id = <dataset id>`, `metadata = {dataset, cutoff,
  rows_removed, dry_run}`.
- CLI: `autumn db retention [--dataset X] [--dry-run|--purge] [--json]`,
  driven through the app binary so the CLI and the app agree by construction.
