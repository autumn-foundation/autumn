# Operating Background Jobs

Autumn ships a built-in jobs dashboard in `autumn-admin-plugin` for operators
who need to inspect and recover request-triggered `#[job]` work without adding a
separate queue UI.

## Enable the dashboard

Register jobs normally and mount the admin plugin:

```rust,ignore
use autumn_admin_plugin::AdminPlugin;
use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() -> AutumnResult<()> {
    autumn_web::app()
        .jobs(jobs![send_welcome_email])
        .plugin(AdminPlugin::new())
        .run()
        .await
}
```

The dashboard is available at `GET /admin/jobs` by default. If the plugin is
mounted with another prefix, use that prefix, for example `/backoffice/jobs`.
It uses the same session role check as the rest of the admin panel and renders
CSRF tokens for every mutating action.

## What it shows

The dashboard renders four paginated lists, newest-first:

- Enqueued jobs waiting for a worker. On the Redis backend this list also
  covers jobs parked on a full concurrency slot; those rows are marked
  "waiting on a concurrency slot". The marker is a sample: a parked job
  returns to its queue every ~100 ms to retry, so a read can catch it
  unmarked. The row stays listed either way. The Enqueued counter includes
  parked jobs, so it can read higher than `/actuator/jobs`, which reports
  `queued` and `blocked_on_concurrency` separately.
- Running jobs currently executing in this runtime.
- Completed jobs from the last 24 hours.
- Terminally failed jobs from the last 7 days.

Each row includes the job name, lifecycle timestamps, attempt count, principal
id, correlation id, and last error. The default backend extracts principal and
correlation values from common JSON payload fields:

- Principal: `principal_id`, `principal`, or `user_id`.
- Correlation: `correlation_id` or `request_id`.

Failed errors are truncated in the table and expandable in place. Registered
scheduled tasks are listed below ad-hoc jobs with their schedule expression,
next run time when available, and last run status.

## Operator actions

Failed jobs can be retried or discarded only after automatic attempts are
exhausted. A job attempt that fails with attempts remaining is tracked as
retrying/delayed work and stays out of the terminal failed list, so operators do
not accidentally enqueue a duplicate while the framework retry is already
sleeping. Retrying keeps the old failed row as a retried lifecycle entry and
enqueues a new job with the original payload. Discarding removes the job from
the active failed list.

Enqueued jobs can be canceled before a worker starts them. The default local
runtime checks the cancel marker during its atomic start transition, so a cancel
either wins cleanly or the job is already running and the action is rejected.

## Runtime backends

The built-in local job runtime installs a bounded, process-local
`JobAdminMemoryBackend` automatically. It is designed for the default admin UI:
reads are in-memory, status lists are bounded, and completed/failed windows are
filtered at snapshot time.

When `jobs.backend = "redis"` is enabled, Autumn installs a Redis-backed
dashboard backend automatically. It reads the framework queue, processing,
completed, and dead-letter keys directly, so `/admin/jobs` reflects cluster
state instead of only the process serving the admin request. Retry, discard, and
cancel actions mutate the same Redis storage used by workers.

For cluster-wide durable queues or external job storage, install a custom
backend by inserting `JobAdminBackendEntry` into `AppState`:

```rust,ignore
use std::sync::Arc;
use autumn_web::job::{JobAdminBackend, JobAdminBackendEntry};

let backend: Arc<dyn JobAdminBackend> = Arc::new(MyDurableJobAdminBackend::new());
state.insert_extension(JobAdminBackendEntry(backend));
```

Custom backends provide the same read and operate surface:

- `snapshot(query)` returns paginated enqueued, running, completed, and failed
  pages plus schedule summaries.
- `retry(id)` retries a failed job.
- `discard(id)` removes a failed job from active operator attention.
- `cancel(id)` cancels an enqueued job that has not started.

## Refresh cost

The counter fragment at `/admin/jobs/counters` refreshes with htmx at most every
2 seconds. The local backend clamps page size to 100 rows and keeps at most
1,000 lifecycle entries, so each refresh is a bounded in-memory scan rather than
an unbounded queue traversal.

The Redis backend uses `LLEN`/`ZCARD` for active queue counters and reads only
the bounded completed/dead-letter history window for recent completed and failed
counts. Durable custom backends should preserve that property with indexed
status/time-window queries or similarly bounded history reads.

For low-level counters, `GET /actuator/jobs` remains available and reports
per-job queued, in-flight, success, failure, dead-letter, and last-error data.
