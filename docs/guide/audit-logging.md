# Audit Logging

Some actions in an application are security-sensitive or compliance-relevant:
deleting a post, changing a user's role, exporting personal data, issuing a
refund. When someone later asks *"who deleted this, and when?"* an ordinary
`tracing::info!` line buried in your request logs is not a good enough answer.
Autumn ships a small, dedicated **audit log** for exactly these events —
append-only, structured, and kept separate from ordinary application logging.

An audit write records **who** did **what**, to **which resource**, from **where**,
and whether it **succeeded** — one immutable [`AuditEvent`] fanned out to one or
more [`AuditSink`]s. The "who" is filled in for you: Autumn's request-scoped
**current actor** carries the authenticated principal, so a handler auto-attributes
the acting user with no per-call session re-extraction.

Audit logging is new in 0.6.0. For a runnable end-to-end demo, see the
[`examples/reddit-clone`](../../examples/reddit-clone) example, whose `delete_post`
moderation handler records one actor-attributed `post.delete` event.

## Prerequisites

- An `autumn-web` application (see [Getting Started](getting-started.md)).
- The audit types live in `autumn_web::audit`; [`AuditEvent`] and [`AuditStatus`]
  are also re-exported from `autumn_web::prelude`, so `use autumn_web::prelude::*;`
  is enough for the event-construction API.
- Actor auto-attribution builds on the [authorization](authorization.md) layer:
  it is the auth resolution that publishes the current principal, so the acting
  user is only auto-attributed on authenticated (`#[secured]` / policy-guarded)
  requests. Unauthenticated requests simply have no actor.

## Recording an event

An [`AuditEvent`] is a plain struct — actor, action, target, optional caller IP,
and a [`AuditStatus`] outcome. Build one with [`AuditEvent::new`] (it stamps the
UTC timestamp for you) and hand it to the [`AuditLogger`] installed in
application state via the [`write_from_state`] helper:

```rust,ignore
use autumn_web::prelude::*; // AuditEvent, AuditStatus

let event = AuditEvent::new(
    actor,                 // who — a user id / service-account id / API-key id
    "post.delete",         // action — a canonical, dotted verb string
    post.id.to_string(),   // target resource id
    None,                  // caller IP, if you have it (Option<IpAddr>)
    AuditStatus::Success,  // outcome
);

// Best-effort: a sink hiccup should not fail an action the user already
// performed. `write_from_state` is a no-op (returns Ok) if no logger is
// installed, so it is always safe to call.
let _ = autumn_web::audit::write_from_state(&state, event).await;
```

Use a stable, greppable `action` vocabulary (`post.delete`, `user.role.update`,
`export.create`) — these strings are what a compliance query filters on months
later. Record **failures** too: pass [`AuditStatus::Failure`] when an authorized
attempt was made but the action did not complete, so a burst of failures is
itself an auditable signal.

In the reddit-clone example this lives at the end of the `delete_post` handler
in `examples/reddit-clone/src/routes/posts.rs`, right after the row is deleted:

```rust,ignore
#[secured]
#[delete("/r/{sub_slug}/posts/{post_slug}")]
pub async fn delete_post(/* … */) -> AutumnResult<Response> {
    let post = load_post_and_authorize(/* … */, "delete").await?;

    repo.delete_by_id(post.id).await?;

    // Audit this moderation action (actor auto-attributed — see below).
    let actor =
        autumn_web::current::Current::actor().unwrap_or_else(|| "unknown".to_string());
    let _ = autumn_web::audit::write_from_state(
        &state,
        AuditEvent::new(actor, "post.delete", post.id.to_string(), None, AuditStatus::Success),
    )
    .await;

    // … broadcast + redirect …
}
```

## Actor auto-attribution

You should almost never pass a hand-plucked user id as the `actor`. Autumn keeps
a request-scoped **current actor** — the same ambient value that seeds
`VersionEntry.actor` for versioned repository writes. When the auth layer
resolves an authenticated principal (during `#[secured]` / policy checks) it
publishes that id onto the request scope, and any code later in the request reads
it with [`Current::actor`]:

```rust,ignore
use autumn_web::current::Current;

// Inside an authenticated handler, this returns the resolved principal's id.
let actor = Current::actor().unwrap_or_else(|| "unknown".to_string());
```

This means the handler never re-extracts the `Session` just to name the actor —
the "who" is already resolved and ambiently available. [`Current::actor`] returns
`Option<String>`:

- `Some(id)` inside an authenticated request — the resolved principal.
- `None` inside a request that has not resolved a principal (an anonymous
  request), so an audit call on an unauthenticated path attributes no user.
- Outside any request (a background job, the scheduler, a CLI task) it falls back
  to the process-wide **default actor token**, if one is configured.

For work that runs on behalf of a specific identity outside a request — a job
runner, a scheduled task, an "acting as" flow — set the actor explicitly rather
than defaulting to `"unknown"`:

```rust,ignore
use autumn_web::current::{Current, with_actor};

// A whole scope acts as one identity (background jobs, "on behalf of"):
with_actor("scheduler", async {
    // Current::actor() == Some("scheduler") in here.
    run_retention_sweep().await;
})
.await;

// Or a process-wide default for all non-request contexts:
Current::set_default_actor(Some("scheduler".to_string()));
```

## Sinks and stores

An [`AuditLogger`] fans one event out to every registered [`AuditSink`]; each sink
is an append-only destination. Autumn ships three, so you rarely write your own:

- [`TracingAuditSink`] — emits each event as structured JSON fields on a
  dedicated `autumn.audit` tracing target. **No schema, no migration** — the
  lowest-risk sink and the one the reddit-clone example uses. Route the
  `autumn.audit` target to its own file/collector to keep audit records out of
  your ordinary log stream.
- [`JsonlFileAuditSink`] — appends one JSON object per line to a file in
  append-only mode with an fsync per write, suitable for an immutable on-disk
  archive.
- `ChannelAuditSink` (behind the `ws` feature) — broadcasts events to a
  WebSocket/SSE [channel](websockets.md), useful for a live admin dashboard.

Implement the [`AuditSink`] trait yourself for anything else — a database table,
an S3-backed archive, or a SIEM adapter (Splunk, Datadog, an OpenTelemetry
collector). Every event is treated as an immutable, append-only record; the
logger attempts **all** sinks even if one fails, aggregating errors so a single
broken destination never silently drops the event from the others.

Install the logger once at startup by inserting an [`AuditLogger`] into
application state. The reddit-clone example does this in its `state_initializer`
(`examples/reddit-clone/src/main.rs`):

```rust,ignore
use std::sync::Arc;
use autumn_web::audit::{AuditLogger, TracingAuditSink};

let app = autumn_web::app()
    // …
    .state_initializer(move |state| {
        state.insert_extension(
            AuditLogger::new().with_sink(Arc::new(TracingAuditSink)),
        );
    });
```

[`write_from_state`] looks the logger up by type from state, so this one
registration is all a handler needs. Chain `.with_sink(...)` more than once to
fan out to several destinations at the same time — for example, a tracing sink
for live observability *and* a JSONL file for the durable archive:

```rust,ignore
AuditLogger::new()
    .with_sink(Arc::new(TracingAuditSink))
    .with_sink(Arc::new(JsonlFileAuditSink::new("/var/log/app/audit.jsonl")));
```

## Querying and retention

Because audit events are append-only, *where* they land dictates how you query
and how long you keep them:

- **Tracing sink** → query with whatever your log pipeline offers, filtering on
  the `autumn.audit` target and the `action` / `actor_id` fields; retention is
  your log store's retention.
- **JSONL file** → each line is a self-describing JSON object, so `jq`, `grep`,
  or bulk-loading into a warehouse all work; rotate/retain the file with your
  normal log-rotation tooling.
- **Custom DB sink** → query with SQL and enforce retention by declaring
  `retention(after = "...", basis = created_at)` on the audit table's
  `#[repository(...)]` — see [Data-Retention Sweeps](retention-sweeps.md) —
  rather than hand-writing a scheduled purge job. Keep audit rows immutable:
  grant the app `INSERT`-only, and never `UPDATE`/`DELETE` an existing event.

Audit logging answers *who did what*. It is a natural foundation for adjacent
compliance features such as GDPR data export and right-to-erasure workflows —
those build on the same actor/action record — but they are separate concerns and
out of scope for this guide.
