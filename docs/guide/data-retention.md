# Data Retention for Framework-Owned Data

Autumn creates and fills persistent stores your application never asked for.
The job queue writes a row per job. Tracked jobs write a progress record per
enqueue. Experiments write a sticky assignment per actor. Idempotent requests
cache a response per key. Signed webhooks record a replay marker per delivery.
Sessions and audit archives accumulate for as long as the app runs.

Being batteries-included means owning the lifecycle of all of it. The
`[retention]` section is the one place you declare how long each of those
datasets is kept; Autumn enforces it automatically on a recurring in-process
sweep, with no external cron, and `autumn db retention` shows you exactly what
the policy is and what it is about to delete.

> **This page is about Autumn's own tables and stores.** For retention on
> *your* models, declare
> [`retention(...)` on a `#[repository]`](retention-sweeps.md) — a separate,
> complementary mechanism.

## Quick Start

```toml
# autumn.toml
[retention]
job_history            = "90d"
commit_hooks           = "30d"
experiment_assignments = "365d"
audit_archives         = "400d"
```

That is the whole setup. On the next boot Autumn registers one
fleet-coordinated sweep that runs hourly and enforces every window you set.

Check what it will do before it does it:

```bash
autumn db retention --dry-run
```

```
Dataset                 Retention  Source                Enforced by      Would remove
----------------------  ---------  --------------------  ---------------  ------------
job_history             90d        [retention]           sweep            12483
commit_hooks            30d        [retention]           sweep            51
job_tracking            1d         jobs.tracking.ttl_secs  sweep          902
idempotency             1d         idempotency.ttl_secs  backend ttl      —
experiment_assignments  365d       [retention]           sweep            0
webhook_replay          forever    unset                 backend ttl      —
sessions                1d         session.max_age_secs  backend ttl      —
audit_archives          400d       [retention]           archive rewrite  38

  ℹ idempotency: enforced at write time: records are stored with a TTL capped at 86400s by the backend, not deleted by this sweep
  ℹ webhook_replay: no retention window configured
  ℹ sessions: enforced at write time: records are stored with a TTL capped at 86400s by the backend, not deleted by this sweep
```

Note the `Source` column: `job_tracking`, `idempotency` and `sessions` already
had a 24-hour bound before you wrote anything, so a *longer* `[retention]`
window for them changes nothing. Read [Precedence](#precedence-the-shorter-bound-wins)
before setting those three.

## Every Framework-Owned Dataset

| Dataset | What it is | Where it lives | Default (unset) | Enforced by |
|---|---|---|---|---|
| `job_history` | Finished job rows (`completed`/`failed`/`discarded`) | `autumn_jobs` | **forever** | sweep |
| `commit_hooks` | Finished `#[after_commit]` hook rows (`completed`/`failed`/`after_hook_failed`) | `autumn_repository_commit_hooks` | **forever** | sweep |
| `job_tracking` | Tracked-job progress/result records | `autumn_job_tracking`, or Redis — follows `jobs.backend` | jobs.tracking.ttl_secs (24h by default) | sweep on `postgres`, backend TTL otherwise |
| `idempotency` | Stored `Idempotency-Key` responses | memory / Redis | idempotency.ttl_secs (24h by default) | backend TTL |
| `experiment_assignments` | Sticky actor → variant assignments | `autumn_experiment_assignments` | **forever** | sweep |
| `webhook_replay` | Inbound webhook replay markers | memory / Redis | the endpoint's replay_window_secs (24h by default) | backend TTL |
| `sessions` | Server-side session records | memory / Redis | session.max_age_secs (the session cookie's lifetime) | backend TTL |
| `audit_archives` | Entries in the JSONL audit archive | the sink's file | **forever** | archive rewrite |

**Leaving a dataset unset preserves today's behavior exactly.** With no
`[retention]` section at all, no sweep task is registered, no scheduler loop is
spawned, and no query is ever issued — the framework behaves bit-for-bit as it
did before this feature existed.

### Framework tables deliberately *not* covered

These are framework-owned and persistent, but pruning them would break
correctness rather than bound growth, so they have no retention window:

| Table | Why it is kept forever |
|---|---|
| `__diesel_schema_migrations`, `autumn_migration_checksums` | The migration ledger. Deleting a row makes Autumn re-run or mis-verify a migration. |
| `_autumn_ledger_revisions` | A ledgered entity's tamper-evident hash chain. Removing a link breaks verification of everything after it. |
| `_autumn_version_history` | Restore points for `#[versioned]` models. Bounded by your own policy on the models themselves. |
| `_autumn_shard_directory`, `_autumn_shard_map` | Routing state. Every row is live. |
| `autumn_experiments`, `autumn_experiment_overrides` | Current configuration, not history. |
| `autumn_runtime_config_values`, `autumn_feature_flags` | Current values. |
| `autumn_sync_*` | Offline-sync state with its own tombstone GC. |
| `mail_suppressions`, `mail_unsubscribes` | Compliance records: a suppression you expire is a bounce or an unsubscribe you are about to violate. |
| `api_tokens` | Live credentials; revoke, do not expire silently. |

The `*_changes` audit tables (`autumn_experiment_changes`,
`autumn_runtime_config_changes`, `feature_flag_changes`) are append-only
operator audit logs. They are intentionally out of scope for this slice:
pruning an audit log is a decision that deserves its own explicit knob, not a
side effect of a job-history window.

## The Enforcement Mechanisms

The datasets are genuinely heterogeneous, and the report says so per row
rather than pretending they all work the same way.

**`sweep`** — a scheduled batched `DELETE` against a framework-owned Postgres
table. Rows are deleted in batches of 500, up to 1000 batches per dataset per
run, so one run never holds a long lock or spikes replication lag; a first
sweep of a years-old table finishes over several ticks. Completeness is
established by re-counting the stale rows at the end of the run, not inferred
from the last batch's size, and a run that left rows behind is reported (and
audited) as `truncated` rather than looking like a clean sweep. The cutoff is resolved by the database itself (`NOW() -
<window>`), not by the app process, so a replica with a fast clock cannot
delete rows younger than the window.

Each sweep predicate is deliberately narrower than "old enough", because each
one guards an invariant another subsystem depends on:

- **`job_history`** matches only **terminal** rows (`completed`, `failed`,
  `discarded`) that have a `finished_at`. A job that is enqueued, running, or
  waiting on a retry is never touched, no matter how old its row is — a retry
  goes back to `enqueued` with `finished_at` cleared.

  It also never deletes a row still holding a `#[job(unique, unique_for_ms =
  N)]` dedup key. That uniqueness window is enforced *by the historical row's
  continued existence* — a completed twin is what suppresses a duplicate
  enqueue — so deleting it would silently run the job a second time. The row
  does not record `N` (it is a compile-time attribute, not a column), so there
  is no cutoff at which the sweep could safely take it. **Consequence:** rows
  for TTL-unique jobs are retained regardless of your window. If that matters
  for volume, lower the job's `unique_for_ms`.

  **`failed` is also the dead-letter state.** A `job_history` window therefore
  bounds how long a dead-lettered job stays replayable from the jobs dashboard
  (`POST /admin/jobs/{id}/retry`). If you triage dead letters by hand, set the
  window longer than your triage window, or leave it unset.

- **`experiment_assignments`** matches only assignments belonging to an
  **archived** experiment, or one whose experiment row is gone. A sticky
  assignment is what keeps an actor on one variant while an experiment runs:
  deleting it re-buckets that actor through the *current* weights, which
  contaminates a running experiment's results and can also admit them into a
  sibling experiment in the same exclusion group.

  The line is **restartability**, not whether an experiment has finished.
  `ExperimentService::start` restores a `draft` *or* `concluded` experiment to
  `running` and refuses only `archived` — so a concluded experiment's
  assignments are still live data, and sweeping them before a restart would
  re-bucket every returning actor. If you want a concluded experiment's
  assignments collected, archive it: that is the state the API treats as
  terminal, and it is the state this sweep treats as collectable.

- **`job_tracking`** and **`commit_hooks`** have no such entanglement:
  tracking records are already invisible to reads past their expiry, and a
  terminal hook row is history. `commit_hooks` covers `after_hook_failed`
  alongside `completed`/`failed` — it is terminal and records `finished_at`,
  and a re-enqueue of the same hook id inserts cleanly once the row is gone.

  `job_tracking` is the one dataset whose *mechanism* follows your config:
  tracked-job records live wherever `jobs.backend` puts them. Under
  `postgres` they are rows in `autumn_job_tracking` and a sweep deletes them
  — which is also what lets a legal hold stop the deletion. Under `redis`
  (or the in-memory fallback) there is no table to sweep, so the window is
  applied by capping the record's TTL instead. The report names whichever
  applies rather than claiming a sweep that would find nothing.

**`backend ttl`** — the storage backend expires the record itself, so there is
nothing for a sweep to delete and the report shows `—` rather than a fake
zero. For `idempotency` and `sessions` the window is applied by *capping the
TTL the record is written with*: because the cap is a `min`, it can only ever
shorten a lifetime. `webhook_replay` needs no cap — boot validation (below)
already guarantees the marker's own window is within the retention window.

> Four caveats worth knowing before you set these:
>
> - **A write-time cap cannot reach a record that already exists.** After you
>   shorten one of these windows, records written under the *previous* TTL
>   keep it and age out under it. Enforcement is therefore complete only once
>   the old TTL has elapsed — at most the previous window. The report says so
>   in the dataset's note rather than claiming a bound the data does not yet
>   satisfy. If you need the old records gone sooner, flush the relevant Redis
>   key prefix (`idempotency.redis.key_prefix`, `session.redis.key_prefix`)
>   deliberately, knowing that dropping idempotency records lets an in-flight
>   client retry re-execute.
> - **`sessions` is a login lifetime.** `session.max_age_secs` is the session
>   cookie's `Max-Age` as well as the server-side record's TTL, so
>   `retention.sessions = "1h"` signs every user out after an hour.
> - **A custom `SessionStore` must apply the window itself.** A store
>   installed with `AppBuilder::with_session_store` receives no TTL from the
>   framework — `SessionStore::save` has no TTL parameter — so capping
>   `session.max_age_secs` shortens only the client cookie. A database-backed
>   custom store keeps its server-side rows until *it* expires them. The
>   report's `sessions` note states this; treat the window as a cookie bound
>   plus a contract your store has to honour.
> - **The in-memory session store has no expiry at all**, so with the default
>   `session.backend = "memory"` a `sessions` window bounds only the browser
>   cookie. The report says so in as many words — `NOT enforced` — rather
>   than presenting it as backend-TTL enforcement. Run Redis (or a custom
>   `SessionStore` that honours the window) in production, as the session
>   guide already recommends. The in-memory idempotency store is likewise a
>   development backend.

**`archive rewrite`** — the JSONL audit archive is rewritten without the stale
entries: filtered into a sibling temp file that inherits the archive's
permissions, `fsync`ed, then atomically renamed over the original. A crash
mid-purge leaves the original intact. A line that cannot be decoded as an
audit event is **kept**, never discarded — a retention sweep must not be the
thing that silently drops a record it merely failed to parse.

Append-only is the *write* contract for audit sinks; retention is a separate,
operator-declared bound that GDPR's storage-limitation principle can require.
Custom `AuditSink` implementations inherit a default `purge_before` that
reports "unsupported", so a sink that forwards to a SIEM says so in the report
instead of implying an empty archive.

> Two limits of the archive rewrite: it reads the archive into memory (peak
> roughly twice its size), so keep a very large archive rotated by external
> tooling; the rewritten file must be able to keep the archive's own
> permissions, so a purge refuses rather than renaming a default-mode file
> over an archive you hardened to `0600`; and its lock is per-process, so
> running `autumn db retention
> --purge --dataset audit_archives` *against a live server writing the same
> file* can drop events appended during the rewrite. Let the in-process
> scheduled sweep handle this dataset in a live deployment.

With several sinks installed, a purge can partly succeed. Entries a working
sink removed stay removed, so the report carries that count *and* the failing
sink's error — `rows_removed` never understates a deletion that happened.

## Precedence: the Shorter Bound Wins

Several datasets already had a per-subsystem knob before this section existed:
`jobs.tracking.ttl_secs`, `idempotency.ttl_secs`, `session.max_age_secs`, and
a webhook endpoint's `replay_window_secs`. **They keep their exact meaning and
keep working unchanged.**

A `[retention]` window is an *additional, independent ceiling*. With both set:

```
effective retention = min(subsystem ttl, [retention] window)
```

| `jobs.tracking.ttl_secs` | `retention.job_tracking` | Records kept for | Reported source |
|---|---|---|---|
| `86400` (default) | unset | 24h | `jobs.tracking.ttl_secs` |
| `86400` | `"1h"` | 1h | `[retention]` |
| `86400` | `"30d"` | 24h | `jobs.tracking.ttl_secs` |
| `3600` | unset | 1h | `jobs.tracking.ttl_secs` |

The rule is deliberately one-directional: **there is no configuration in which
adding `[retention]` causes data to be kept longer than it is today.** The
`Source` column of `autumn db retention` always names which setting actually
governs, so a window that is being overridden is visible rather than silently
ineffective.

The same table applies to `idempotency.ttl_secs` / `retention.idempotency` and
to `session.max_age_secs` / `retention.sessions`. For those two the tighter
bound is applied by shortening the record's TTL at write time; for
`job_tracking` it is applied by the sweep, so `jobs.tracking.ttl_secs` keeps
the literal value you configured (a legal hold can then stop the sweep — the
independent `expires_at` cleanup could not have been stopped).

### One exception: webhook replay protection

A replay marker's lifetime *is* the replay-rejection window — once the marker
is gone, a captured request replayed after that point is accepted again.
Letting a compliance knob silently shorten it would weaken a security control
through a door nobody would think to look behind.

So `retention.webhook_replay` shorter than any replay-protected endpoint's
`replay_window_secs` **fails boot** with an error naming both keys. Lower
`replay_window_secs` if you really want a shorter window.

The practical consequence: because the retention window can only ever be
*wider* than the marker's own lifetime, `min()` always resolves to
`replay_window_secs`, and setting `retention.webhook_replay` never changes how
long a marker lives. It is a declaration, checked at boot, that your replay
windows are inside your stated retention policy — not a second enforcement
mechanism. The report shows this by naming `replay_window_secs` as the
`Source`.

## Legal Hold

Data covered by a GDPR legal-hold registration is **never** removed by a
retention sweep:

```rust,no_run
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};

autumn_web::app()
    .state_initializer(|state| {
        state.insert_extension(GdprRegistry::new().register(ModelRegistration::retain(
            "autumn_jobs",
            "litigation hold 2026-CV-1",
        )));
    });
```

The registry has to be installed into application state — a `GdprRegistry`
built and dropped on the floor holds nothing.

That is the same registration that already exempts a table from a GDPR erasure
request. Holding data is a legal obligation that outranks a retention window,
so it is a **veto over the whole dataset**, not a row filter: a sweep that
cannot tell held rows from unheld ones must not delete any of them. The hold
and its reason appear in the report:

```
  ℹ job_history: legal hold: litigation hold 2026-CV-1
```

Only sweep-enforced datasets have a backing table a hold can name; the
TTL-native ones cannot be held this way.

A hold on `autumn_job_tracking` also suppresses the job runner's own
independent `expires_at` cleanup, which predates this policy and is not part
of it. Without that, a hold would be honoured by `autumn db retention` and
quietly violated five minutes later by the maintenance loop — worse than
having no hold at all.

## The CLI

```bash
# What is kept, and how much is eligible for purge right now:
autumn db retention

# What a sweep would remove, without removing it:
autumn db retention --dry-run

# Enforce the policy now (dev/test; see below for other profiles):
autumn db retention --purge

# One dataset at a time, with detail:
autumn db retention --dry-run --dataset job_history

# Machine-readable, for a compliance report or a CI check:
autumn db retention --json
```

It takes `--profile` plus `--package`/`--bin` for a workspace with more than
one binary. Unlike `autumn db backup`, `--profile` here **defaults to `dev`**
rather than reading `AUTUMN_ENV` — it is forwarded *into* the app binary as
`AUTUMN_ENV`, matching `autumn retention` and `autumn task`. Always pass it
explicitly for anything but development:

```bash
autumn db retention --profile prod
```

The command compiles and runs your application binary. That is deliberate: the
policy depends on your app's resolved config, its GDPR registrations, and its
audit sinks, none of which a standalone CLI can see. Running it in-app means
the report and the enforcement come from one code path and cannot drift.

`--dataset` only accepts a registered dataset key, so a typo is rejected
before anything is compiled. The exit status is non-zero if any dataset
failed, so a scripted purge cannot look successful after a partial failure.

**`--purge` against a non-dev/test profile additionally requires `--force`**,
the same guard `autumn db drop` and `autumn db scrub` apply. Nothing about
production needs an on-demand purge — the configured policy already runs
inside the app on its own schedule — so the flag exists for the deliberate
case, not the routine one.

## Observability and the Audit Trail

Every real sweep of a bounded dataset emits an audit record through your
installed [`AuditLogger`](audit-logging.md) — including a sweep that removed
zero rows, because "we enforced the policy and there was nothing to delete" is
a claim a reviewer needs evidence for:

| Field | Value |
|---|---|
| `action` | `retention.sweep` |
| `actor_id` | `autumn:retention` |
| `target_resource_id` | the dataset key |
| `metadata.dataset` | the dataset key |
| `metadata.cutoff` | the RFC-3339 cutoff timestamp |
| `metadata.rows_removed` | rows actually deleted |
| `metadata.eligible_rows` | rows that matched |
| `metadata.truncated` | `true` when the run stopped at its per-run batch cap with rows still stale |
| `metadata.skipped` | present when a legal hold blocked the sweep |
| `metadata.error` | present when the sweep failed |

A dataset held back by a legal hold is recorded even though nothing was
deleted — "the policy wanted to delete this and did not" is exactly what a
compliance reviewer needs to see. Dry runs (including the default read-only
`autumn db retention`) write no record at all; they delete nothing, so they
are not sweeps. A dataset with no window, and one whose backend expires its
own records, are likewise not audited — there is no deletion to attribute.

If no purge-capable `AuditSink` is installed, `audit_archives` reports that in
the `Source`/notes rather than silently claiming an enforced window.

The same information is emitted as a structured `retention_sweep` line on the
`autumn.audit` tracing target, so it lands in your log pipeline even with no
`AuditLogger` installed. `TracingAuditSink` also emits the `metadata` map (as
a JSON object field) for every event that carries one, so a SIEM consuming
that target sees the cutoff and row count too.

## Configuration Reference

```toml
[retention]
sweep_interval         = "1h"    # how often the sweep runs (default "1h")
job_history            = "90d"
commit_hooks           = "30d"
job_tracking           = "7d"
idempotency            = "2d"
experiment_assignments = "365d"
webhook_replay         = "3d"
sessions               = "30d"
audit_archives         = "400d"
```

Four of these keys have a pre-existing 24-hour default bound
(`job_tracking`, `idempotency`, `sessions`) or are validated to be no tighter
than the security control they describe (`webhook_replay`). A value *longer*
than that bound is accepted and has no effect — see
[Precedence](#precedence-the-shorter-bound-wins), and check the `Source`
column of `autumn db retention` to see which setting is actually governing.

Durations use the same syntax as `#[scheduled(every = ...)]`: `s`, `m`, `h`,
`d`, optionally compound (`"1h 30m"`). A window that is not a valid, non-zero
duration fails boot rather than being silently ignored — a policy you believe
is enforced but is not is worse than no policy. Zero is refused because it
would purge a dataset as soon as it was written, and anything beyond ~136
years is refused because the sweep could not apply it faithfully; in both
cases, remove the key entirely to restore today's behavior.

Every key has an environment override:

| Environment variable | Key |
|---|---|
| `AUTUMN_RETENTION__SWEEP_INTERVAL` | `retention.sweep_interval` |
| `AUTUMN_RETENTION__JOB_HISTORY` | `retention.job_history` |
| `AUTUMN_RETENTION__COMMIT_HOOKS` | `retention.commit_hooks` |
| `AUTUMN_RETENTION__JOB_TRACKING` | `retention.job_tracking` |
| `AUTUMN_RETENTION__IDEMPOTENCY` | `retention.idempotency` |
| `AUTUMN_RETENTION__EXPERIMENT_ASSIGNMENTS` | `retention.experiment_assignments` |
| `AUTUMN_RETENTION__WEBHOOK_REPLAY` | `retention.webhook_replay` |
| `AUTUMN_RETENTION__SESSIONS` | `retention.sessions` |
| `AUTUMN_RETENTION__AUDIT_ARCHIVES` | `retention.audit_archives` |

Setting one to the empty string *clears* a window declared in `autumn.toml`,
restoring today's behavior for that one dataset without editing the file.

## Multi-Replica Behavior

The sweep is registered as a
[`coordination = "fleet"` scheduled task](scheduled-multi-replica.md) named
`autumn-retention-sweep`. Under the `postgres` scheduler backend only one
replica executes it per tick, however many replicas are running; the `sqlite`
backend gives the same guarantee across the processes on one host. It runs in
the `worker` and `combined` process roles, not `web`, like every other
scheduled task.

Avoid naming a hand-written `#[scheduled]` task `autumn-retention-sweep`: the
app refuses to boot on the collision rather than spawning two loops competing
for one coordination lock.

## See Also

- [Data-Retention Sweeps](retention-sweeps.md) — `retention(...)` on your own
  `#[repository]` models. This page's counterpart for application data.
- [Audit Logging](audit-logging.md) — the sinks a sweep record lands in, and
  the `audit_archives` dataset itself.
- [Data Scrubbing](data-scrubbing.md) — `autumn db scrub` anonymizes; retention
  deletes. Composable: scrub a copy, retain the original under policy.
- [Database Backups](deployment.md) — compose `autumn db backup` with a purge
  when you need "keep a copy first".
- [Operating Background Jobs](operating-background-jobs.md) — what a
  `job_history` window means for the dead-letter queue you can still replay.
