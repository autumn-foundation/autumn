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
job_tracking           = "7d"
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
Dataset                 Retention  Source                 Enforced by      Would remove
----------------------  ---------  ---------------------  ---------------  ------------
job_history             90d        [retention]            sweep            12483
job_tracking            7d         [retention]            sweep            902
idempotency             1d         idempotency.ttl_secs   backend ttl      —
experiment_assignments  365d       [retention]            sweep            0
webhook_replay          forever    unset                  backend ttl      —
sessions                1d         session.max_age_secs   backend ttl      —
audit_archives          400d       [retention]            archive rewrite  38
```

## Every Framework-Owned Dataset

| Dataset | What it is | Where it lives | Default (unset) | Enforced by |
|---|---|---|---|---|
| `job_history` | Finished job rows (`completed`/`failed`) | `autumn_jobs` | **forever** | sweep |
| `job_tracking` | Tracked-job progress/result records | `autumn_job_tracking` | `jobs.tracking.ttl_secs` (24h) | sweep |
| `idempotency` | Stored `Idempotency-Key` responses | memory / Redis | `idempotency.ttl_secs` (24h) | backend TTL |
| `experiment_assignments` | Sticky actor → variant assignments | `autumn_experiment_assignments` | **forever** | sweep |
| `webhook_replay` | Inbound webhook replay markers | memory / Redis | the endpoint's `replay_window_secs` (24h) | backend TTL |
| `sessions` | Server-side session records | memory / Redis | `session.max_age_secs` (24h) | backend TTL |
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
sweep of a years-old table finishes over several ticks. The cutoff is computed
against the database's clock, not the app's, so a skewed replica cannot widen
a window.

`job_history` only ever matches rows in a **terminal** state with a recorded
`finished_at`. A job that is enqueued, running, or waiting on a retry is
never touched, no matter how old its row is.

**`backend ttl`** — the storage backend expires the record itself. Setting a
window here does not schedule a delete; it *caps the TTL the record is written
with*, so nothing older than the window can exist to purge. Because the cap is
a `min`, it can only ever shorten a lifetime.

> The in-memory session and idempotency stores are development backends. The
> in-memory session store has no expiry at all; run Redis (or a custom
> `SessionStore`) in production, as the session guide already recommends.

**`archive rewrite`** — the JSONL audit archive is rewritten without the stale
entries: streamed to a sibling temp file, `fsync`ed, then atomically renamed
over the original, all under the sink's write lock. A crash mid-purge leaves
the original intact. A line that cannot be decoded as an audit event is
**kept**, never discarded — a retention sweep must not be the thing that
silently drops a record it merely failed to parse.

Append-only is the *write* contract for audit sinks; retention is a separate,
operator-declared bound that GDPR's storage-limitation principle can require.
Custom `AuditSink` implementations inherit a default `purge_before` that
reports "unsupported", so a sink that forwards to a SIEM says so in the report
instead of implying an empty archive.

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

### One exception: webhook replay protection

A replay marker's lifetime *is* the replay-rejection window — once the marker
is gone, a captured request replayed after that point is accepted again.
Letting a compliance knob silently shorten it would weaken a security control
through a door nobody would think to look behind.

So `retention.webhook_replay` shorter than any configured endpoint's
`replay_window_secs` **fails boot** with an error naming both keys. Lower
`replay_window_secs` if you really want a shorter window. (Endpoints
registered in code rather than in `autumn.toml` are not visible to that check;
the same rule applies to them by convention.)

## Legal Hold

Data covered by a GDPR legal-hold registration is **never** removed by a
retention sweep:

```rust
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};

let registry = GdprRegistry::new()
    .register(ModelRegistration::retain(
        "autumn_jobs",
        "litigation hold 2026-CV-1",
    ));
```

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

## The CLI

```bash
# What is kept, and how much is eligible for purge right now:
autumn db retention

# What a sweep would remove, without removing it:
autumn db retention --dry-run

# Enforce the policy now:
autumn db retention --purge

# One dataset at a time, with detail:
autumn db retention --dry-run --dataset job_history

# Machine-readable, for a compliance report or a CI check:
autumn db retention --json
```

The command compiles and runs your application binary. That is deliberate: the
policy depends on your app's resolved config, its GDPR registrations, and its
audit sinks, none of which a standalone CLI can see. Running it in-app means
the report and the enforcement come from one code path and cannot drift.

`--purge` runs the policy you already declared — it deletes nothing a
scheduled sweep would not have deleted an hour later — so it has no separate
production guard. The exit status is non-zero if any dataset failed, so a
scripted purge cannot look successful after a partial failure.

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
| `metadata.skipped` | present when a legal hold blocked the sweep |
| `metadata.error` | present when the sweep failed |

A dataset held back by a legal hold is recorded even though nothing was
deleted — "the policy wanted to delete this and did not" is exactly what a
compliance reviewer needs to see. Dry runs write no record; they delete
nothing, so they are not sweeps.

The same information is emitted as a structured `retention_sweep` line on the
`autumn.audit` tracing target, so it lands in your log pipeline even with no
`AuditLogger` installed.

## Configuration Reference

```toml
[retention]
sweep_interval         = "1h"    # how often the sweep runs (default "1h")
job_history            = "90d"
job_tracking           = "7d"
idempotency            = "2d"
experiment_assignments = "365d"
webhook_replay         = "3d"
sessions               = "30d"
audit_archives         = "400d"
```

Durations use the same syntax as `#[scheduled(every = ...)]`: `s`, `m`, `h`,
`d`, optionally compound (`"1h 30m"`). An unparseable or zero window fails
boot rather than being silently ignored — a policy you believe is enforced but
is not is worse than no policy.

Every key has an environment override:

| Environment variable | Key |
|---|---|
| `AUTUMN_RETENTION__SWEEP_INTERVAL` | `retention.sweep_interval` |
| `AUTUMN_RETENTION__JOB_HISTORY` | `retention.job_history` |
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
replica executes it per tick, however many replicas are running. It runs in
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
