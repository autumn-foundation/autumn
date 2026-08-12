# Failure Capsules

A stack trace tells you *where* a request died. A **failure capsule** tells you
*what it was doing*: the request that failed, the rows the database handed back,
the clock readings the handler took, and the response the client got — written
to one JSON file the moment the failure happens, and replayable offline with
`autumn replay`.

```console
$ autumn replay tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
REPRODUCED  /srv/app/tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
  expected: 500 invoice total overflowed
  actual:   500 invoice total overflowed
```

Capsules are written for **failing requests only** — a caught handler panic or a
`5xx`, the same two events the
[error-reporting pipeline](./error-reporting.md) observes. A `4xx` writes
nothing, and a successful request drops its buffer at the response boundary.

Capture is **off by default**, and the next section is why.

---

## Security: a capsule is production data

**A capsule contains a real request and real database rows.** It is not a
sanitized incident report; it is a copy of what one of your users sent and what
your database sent back, written to your disk so it can be replayed byte for
byte. Treat a capsule directory exactly as you would treat a directory of
production database dumps.

Autumn masks what it can identify by *name*, through the same
[`[log] filter_parameters`](./logging-pii.md) list the access log and the dev
error page use. It cannot mask what has no name attached.

| Masked | How |
| --- | --- |
| Headers whose name matches the filter | `authorization`, `cookie`, `set-cookie`, plus `[log] filter_parameters` |
| Query-string parameters matching the filter | `?password=…` → `[FILTERED]` |
| Form and JSON body fields matching the filter | recursively, including `user[password]` bracket keys |
| Encrypted-column names | every column registered by `#[encrypted]` is added to the filter |
| SQL bind parameters echoing a masked value | byte-equal binds become `"masked"`, and are excluded from replay's bind comparison |
| The outcome message, panic payload and backtrace | any masked value quoted back inside them is substring-replaced |
| Bodies that declare structure but do not parse as it | dropped entirely (`skipped`, with a note) — with no keys, there is nothing to match on |

| **Not** masked | Why |
| --- | --- |
| **Database result rows** | The tape is raw `PostgreSQL` protocol bytes. Replay depends on them being exact, and Autumn has no idea which column is a national ID. **This is the big one.** |
| URL path segments | `/users/12345/ssn` is a route, not a parameter — nothing marks a segment sensitive |
| Unstructured bodies | No keys to match against |
| Bind parameters that echo nothing masked | A bind is only blanked when its bytes equal a value redaction already removed |
| Response bodies | Not recorded at all — only the status, message and Problem Details type |

Out of the box the filter covers `password`, `password_confirmation`, `token`,
`secret`, `authorization`, `api_key`, `access_token`, `refresh_token`, `cookie`,
`set-cookie`, `ssn`, `credit_card`, `card_number` and `cvv`.
`[log] filter_parameters` adds to that set — it is one list for every place
Autumn writes request data down, so anything you add for the access log applies
here too. `[log] unfilter_parameters` opts one of the built-in keys back *out*,
which un-masks it here as well.

### Handling capsules safely

- Capsule files are written **owner-only** (`0600` on unix), through a temp file
  and a rename, so no reader ever sees a half-written capsule.
- The directory defaults to `tmp/autumn-capsules`, project-relative. **Do not
  commit it**, and do not serve it — add it to `.gitignore` alongside `tmp/`.
- `max_capsules` (default 50) prunes oldest-first *before* each write, so an
  error storm cannot fill a disk. A capsule written in the last minute is spared
  even when it is over the cap, so a path already handed to a reporter still
  resolves when the reporter gets round to reading it. The cap is a disk guard,
  not an exact file count.
- Moving a capsule off the failing host moves production data with it. Treat the
  copy the way you would treat the original.
- Turning capture on in production is a deliberate decision. Turning it on in
  staging, or on demand during an incident, gets you most of the value at a
  fraction of the exposure.

---

## Enabling capture

```toml
[failure_capture]
enabled = true                    # default: false
dir = "tmp/autumn-capsules"       # default: "tmp/autumn-capsules"
max_body_bytes = 65536            # default: 65536 (64 KiB)
max_capsule_bytes = 1048576       # default: 1048576 (1 MiB)
max_capsules = 50                 # default: 50
```

- **`enabled`** arms the whole feature: the capture layer, the recording
  database pool, and the recording clock. Off, none of it is installed and there
  is nothing to pay for.
- **`dir`** is where capsules land, resolved relative to the process's working
  directory like Autumn's other runtime files.
- **`max_body_bytes`** caps how much request body is copied. A body that
  *declares* more than this is never copied at all (the handler still receives
  it in full); one that grows past it mid-stream has its partial copy dropped.
- **`max_capsule_bytes`** caps recorded database traffic. Blowing it marks the
  capsule `truncated`, and a truncated capsule is **refused** by replay rather
  than replayed misleadingly.
- **`max_capsules`** is retention. It is clamped to at least 1: a zero would
  otherwise mean "record the failure, then throw it away".

Every key has an environment override:

| Variable | Sets |
| --- | --- |
| `AUTUMN_FAILURE_CAPTURE__ENABLED` | `failure_capture.enabled` |
| `AUTUMN_FAILURE_CAPTURE__DIR` | `failure_capture.dir` |
| `AUTUMN_FAILURE_CAPTURE__MAX_BODY_BYTES` | `failure_capture.max_body_bytes` |
| `AUTUMN_FAILURE_CAPTURE__MAX_CAPSULE_BYTES` | `failure_capture.max_capsule_bytes` |
| `AUTUMN_FAILURE_CAPTURE__MAX_CAPSULES` | `failure_capture.max_capsules` |

Capsules are named `<timestamp>-<sequence>-<capsule id>.json`, so the directory
sorts chronologically. The capsule id is the request's `X-Request-Id` when it
has one, which is how a capsule on disk is tied back to a log line.

---

## What gets recorded

```json
{
  "format_version": 1,
  "id": "01JB2K7Q8N4W",
  "captured_at": "2026-08-12T10:14:13.882104Z",
  "autumn_version": "0.6.0",
  "app": { "name": "invoices", "profile": "production" },
  "request": {
    "method": "GET",
    "uri": "/invoices/42?token=%5BFILTERED%5D",
    "route": "/invoices/{id}",
    "http_version": "HTTP/1.1",
    "headers": [["host", "app.example"], ["authorization", "[FILTERED]"]],
    "body": "absent",
    "redacted_keys": ["header:authorization", "query:token"]
  },
  "outcome": {
    "status": { "code": 500, "message": "invoice total overflowed", "problem_type": null }
  },
  "clock": ["2026-08-12T10:14:13.881500Z"],
  "db": { "connections": [ { "id": 7, "prologue": [], "statements": [], "catalog": [], "exchanges": [
    { "protocol": "extended", "sql": "SELECT id, total FROM invoices WHERE id = $1",
      "binds": [{ "value": "NDI=" }], "response": "VAAA…", "row_count": 1, "error": null }
  ] } ] },
  "truncated": false,
  "notes": []
}
```

**The request.** The head is snapshotted when the request arrives; the body is
*teed as the handler reads it*, never pre-buffered. That matters for more than
memory: pre-reading a body would let a client drip-feed one and hold a worker
open before the request timeout starts — a slow-loris vector that would exist
only when capture was on. A handler that never finishes reading its body still
gets a capsule, with a note saying the body is incomplete.

**The database.** Recording happens at the **wire**, not at the query API. A
pooled connection is opened through a tee that frames `PostgreSQL` protocol
messages in both directions and groups them into exchanges: the SQL, the bind
parameters, and the raw backend frames — `RowDescription`, every `DataRow`,
`CommandComplete`, `ReadyForQuery` — exactly as they arrived. Nothing about
diesel, the pool or your handler changes.

Attribution rides along with work Autumn was already doing: `Db::checkout`
merges `SET autumn.capsule_request = '<capsule id>'` into the same round trip as
`SET statement_timeout`, and the recorder binds the connection to that capsule
until the next marker replaces it. A checkout with no capture scope sends the
*clearing* form, so background work can never be attributed to whoever held the
connection last.

A capsule also carries the connection's **memo**: the session prologue it was
born with, the `Parse`/`Describe` metadata for statements it had already
prepared, and its `pg_catalog` lookups. Without that, the second request served
by a warm pooled connection would record a `Bind` against a prepared statement a
cold replay could never produce.

**The clock.** Every `state.clock()` reading the request takes is appended in
order, so a handler that stamps `created_at` or expires a token sees the same
times on replay. Readings taken outside a request — schedulers, jobs — pass
straight through.

**Bounds and truncation.** Recorded traffic is charged against
`max_capsule_bytes`. Exceeding it, or hitting anything the recorder refuses to
model (a `COPY` stream, an unframeable connection), stops recording, marks the
capsule `truncated` and drops the affected tape: a *partial* tape is worse than
none, because replay would answer real queries with the wrong bytes. Truncated
capsules are refused by replay with exit code 2. The `notes` array explains
every such decision in plain English.

---

## Replaying

```console
$ autumn replay tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
```

| Flag | Meaning |
| --- | --- |
| `-p`, `--package <PKG>` | Package to build and run, for workspaces |
| `--bin <BIN>` | Binary target, for packages with several |
| `--profile <PROFILE>` | Profile forwarded to the app as `AUTUMN_ENV`/`AUTUMN_PROFILE` (default `dev`) |

The CLI compiles your application and runs it with `AUTUMN_REPLAY_CAPSULE` set —
your app, not the CLI, is the only thing that knows its routes, state and
configuration. The app then boots into **replay mode**, which differs from a
normal boot in exactly the ways that keep a replay offline and deterministic:

- the database is an **in-process stub** speaking the `PostgreSQL` protocol over
  an in-memory duplex pipe, answering from the capsule's tape — no socket is
  opened and no live database is contacted;
- the clock serves the capsule's recorded readings, in order;
- sessions are forced to in-memory storage, and no migrations, storage
  preflight, cache backend, job runtime, scheduler, mailer or fail-fast
  configuration gate runs;
- only **sync** event listeners are registered (a durable one needs the job
  runtime);
- no port is bound, and capture is forced off so a replay cannot capsule itself.

Telemetry *is* initialized, so your tracing setup behaves as it normally would.

### The verdict

A verdict is machine-readable JSON on **stdout** and a human summary on
**stderr**, so `autumn replay … | jq` works while you still read the summary:

```json
{
  "verdict": "reproduced",
  "capsule": "/srv/app/tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json",
  "expected": { "status": { "code": 500, "message": "invoice total overflowed", "problem_type": null } },
  "actual":   { "status": { "code": 500, "message": "invoice total overflowed", "problem_type": null } },
  "divergences": [],
  "warnings": []
}
```

| Verdict | Meaning | Exit |
| --- | --- | --- |
| `reproduced` | Same outcome, and the database traffic matched the tape. The bug is still there. | `0` |
| `mismatch` | The tape lined up but the outcome differs. Usually what you want after a fix. | `1` |
| `diverged` | The code asked the database something the recording never asked, so the run was not a fair comparison. A divergence outranks a matching status. | `1` |
| `refused` | Nothing was replayed — a truncated capsule, an unknown `format_version`, an unreadable file, or a `PostgreSQL` tape handed to a `sqlite` build. | `2` |

A `diverged` verdict is not a failure of the tool. It is the tool telling you
that a status matching by luck, while the queries differ, is not a
reproduction.

### A worked divergence

Suppose you "fixed" the bug by adding an eager `SELECT` before the one the
capsule recorded, and replay the old capsule:

```console
$ autumn replay tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
DIVERGED  /srv/app/tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
  expected: 500 invoice total overflowed
  actual:   500 no rows returned
  database divergences (1):
    [sql mismatch] connection 7 exchange 0: the tape expected "SELECT id, total FROM
    invoices WHERE id = $1" next but the code sent "SELECT id, total, currency FROM
    invoices WHERE id = $1"; the statements have been reordered since the recording
```

That is the honest answer: the capsule cannot tell you whether your fix works,
because your fix asks a question the recording never asked. Re-record against
the new code. The same shape appears as `unrecorded query`, `bind mismatch`,
`tape exhausted` and `unknown statement`, each naming the connection, the
position in its tape, and the SQL involved.

Warnings are printed under the verdict and carried in the JSON: a framework
version different from the recording's, a handler reading the clock more times
than the recording did (the last reading is repeated), and the redacted-auth
hint below.

---

## Linking capsules to your error reporter

When capture is on, every `ErrorEvent` carries the capsule that was written for
it, and **the file already exists on disk by the time your reporter runs** —
persistence happens first, before the reporters and before the
`[reporting] enabled` / `sample_rate` gate:

```rust,no_run
use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};

struct SlackReporter;

impl ErrorReporter for SlackReporter {
    fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
        Box::pin(async move {
            if let Some(capsule) = &event.capsule {
                // Safe to read, copy, or upload right now.
                let _ = (&capsule.id, capsule.path.display());
            }
        })
    }
}
```

`event.capsule` is `None` when capture is off, when the request produced nothing
replayable, or when the write failed — a capsule that cannot be written is
logged and dropped, never allowed to turn a `500` into a worse one.

Two consequences worth knowing. Capsule writing is **not** gated on
`[reporting] enabled` or `sample_rate`: an app with delivery turned off, or one
sampling 10% of events, still writes a capsule for every failure. And the write
runs on the blocking pool, not on an async worker, so an error storm against
slow storage cannot stall the workers serving everyone else.

---

## Overhead

Capture is a hot-path feature: with `enabled = true`, *every* request pays for a
scope, a request-head snapshot, a body tee, the attribution marker and the wire
tee. Only failing requests pay for redaction and the write.

Measured by `autumn/tests/integration/failure_capsule_overhead.rs` — 2 000
requests per phase over two interleaved rounds, against a local `PostgreSQL` 16,
dev profile. Two routes, because they answer different questions:

**A route that does nothing else** (no database), isolating what the request
layer of capture costs — the scope, the registry entry, the head snapshot, the
body tap:

| Phase | p50 | p95 | mean |
| --- | --- | --- | --- |
| capture off | 479 µs | 606 µs | 491 µs |
| capture on | 533 µs | 682 µs | 553 µs |
| delta | +55 µs | +76 µs | +62 µs |

**A route doing one bound `SELECT`** through the pool — the wire tee and the
attribution marker on top of the above:

| Phase | p50 | p95 | mean |
| --- | --- | --- | --- |
| capture off | 1 922 µs | 2 382 µs | 1 976 µs |
| capture on | 2 002 µs | 2 444 µs | 2 059 µs |
| delta | +80 µs | +62 µs | +82 µs |

So: **tens of microseconds per request** — around 10% of a request that does
nothing at all, and 3–6% of one that talks to a database once. A repeat run put
the same p50 deltas at +43 µs and +128 µs, which is the honest measure of how
much run-to-run noise there is here; treat ±50 µs as indistinguishable.

These numbers are *indicative*, measured on CI-class virtualized hardware in an
unoptimized build, with a database on localhost — a real deployment's network
round trip makes the relative cost smaller, not larger. Run it on your own
hardware before treating any of it as a budget:

```console
$ cargo test -p autumn-web --features test-support --test integration_tests \
    -- --ignored --nocapture capture_overhead
```

The design choices behind those numbers are worth knowing, because they are what
you would otherwise have to check for yourself: attribution is merged into a
round trip the checkout was making anyway rather than added as its own (in fact
it replaces an extended-protocol `SET` with a single simple-query batch, which
buys back a good part of what the tee costs); the body is teed as the handler
reads it rather than buffered up front; and a successful request's buffer is
dropped rather than written.

---

## Limitations

This is the first slice. What it does not do, stated plainly:

- **Authenticated and CSRF-protected routes do not replay faithfully.** The
  `authorization` and `cookie` headers are masked, so the replayed request meets
  your auth layer without credentials and stops at a `401`/`403`. Replay
  recognizes that shape and says so rather than leaving you guessing. Capsules
  from unauthenticated routes replay cleanly; for authenticated ones the capsule
  is still a faithful record of what happened, just not a re-runnable one.
- **One request per capsule.** A failure that only appears under a particular
  interleaving of concurrent requests is not reproduced by replaying one of
  them.
- **Same-commit replay is what is tested.** A capsule recorded by a different
  build of the framework warns; a capsule recorded by different *application*
  code will usually diverge, which is the honest outcome rather than a bug.
- **Concurrent connections inside one request** (a `join!` over two checkouts)
  are recorded per connection, but their ordering is not guaranteed to repeat,
  and a different interleaving shows up as a divergence. Connections a request
  uses one after another are fine: tapes are recorded — and handed back on
  replay — in the order the request *first used* each connection, not by
  connection id.
- **`PostgreSQL` only, over plaintext TCP.** A `sslmode` URL, a Unix-socket URL,
  or a `sqlite` build disables database capture: the capsule still records the
  request, clock and outcome, and says in `notes` why it has no tape. A
  `PostgreSQL` tape handed to a `sqlite` build is refused outright.
- **A custom `DatabasePoolProvider` disables database capture.** Autumn will not
  second-guess a pool you built; it logs a warning and notes it on every capsule.
- **`LISTEN`/`NOTIFY` is unsupported on capture-enabled request pools.** The
  notification stream is not available on recorded connections. Autumn's own use
  of it (sharding) runs on a dedicated listener connection and is unaffected.
- **`COPY` streams are not modelled.** A `COPY IN`/`OUT` inverts flow control;
  the connection's tape is dropped and the capsule marked truncated.
- **Shard pools are not recorded.** `[[database.shards]]` connections are built
  separately; a request that checks one out has its capsule noted and truncated.
- **A handler that extracts a subsystem replay does not boot** — a `Mailer`, a
  `BlobStore` — fails during replay and is reported as a mismatch rather than
  taking the replay process down.
- **Only failures are captured.** There is no way to capsule a successful
  request, by design: the buffer for a request that succeeds is dropped at the
  response boundary.

---

## See also

- [Error Reporting](./error-reporting.md) — the pipeline that decides a request
  failed, and the `ErrorEvent` a capsule attaches to.
- [Logging & PII](./logging-pii.md) — `[log] filter_parameters`, the one list
  that governs redaction here too.
- [Cloud-Native Guide](./cloud-native.md) — running Autumn where the disk a
  capsule lands on may not outlive the pod.
