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
| Headers whose name *equals* a filter key | `authorization`, `cookie`, `set-cookie`, plus `[log] filter_parameters` — see [exact matching](#names-are-matched-exactly) |
| Query-string parameters matching the filter | `?password=…` → `[FILTERED]` |
| Form and JSON body fields matching the filter | recursively, including `user[password]` bracket keys |
| Encrypted-column names | every column registered by `#[encrypted]` is added to the filter |
| SQL bind parameters echoing a masked value | byte-equal binds become `"masked"`, and are excluded from replay's bind comparison |
| The outcome message, panic payload and backtrace | any masked value quoted back inside them is substring-replaced |
| Bodies that declare structure but do not parse as it | dropped entirely (`skipped`, with a note) — with no keys, there is nothing to match on. Their raw text and string-literal values still seed the echo set, so an outcome quoting the malformed body is scrubbed |

| **Not** masked | Why |
| --- | --- |
| **Database result rows** | The tape is raw `PostgreSQL` protocol bytes. Replay depends on them being exact, and Autumn has no idea which column is a national ID. **This is the big one.** |
| URL path segments | `/users/12345/ssn` is a route, not a parameter — nothing marks a segment sensitive |
| Unstructured bodies | No keys to match against |
| Bind parameters that echo nothing masked | A bind is only blanked when its bytes equal a value redaction already removed |
| Response bodies | Not recorded at all — only the status, message and Problem Details type |
| **SQL statement text** | Stored as your code sent it. Autumn does not run its log-line literal scrubber (`scrub_sql`) here, because rewriting the statement would change the key replay matches tapes on. A value your code *interpolated into the SQL* instead of binding lands in the capsule in the clear — bind your parameters |
| **Backend error payloads** | The raw `ErrorResponse` frames stay in the tape byte for byte. `PostgreSQL` quotes offending data back at you: a unique-violation `DETAIL` names the column *and the value* that collided. The exchange's `error` string is masked where it echoes a value redaction already removed; the recorded bytes are not |

Out of the box the filter covers `password`, `password_confirmation`, `token`,
`secret`, `authorization`, `api_key`, `access_token`, `refresh_token`, `cookie`,
`set-cookie`, `ssn`, `credit_card`, `card_number` and `cvv`.
`[log] filter_parameters` adds to that set — it is one list for every place
Autumn writes request data down, so anything you add for the access log applies
here too. `[log] unfilter_parameters` opts one of the built-in keys back *out*,
which un-masks it here as well.

### Names are matched exactly

A name matches a filter key by **equality**, after normalization — lowercased,
with every non-alphanumeric character removed. So `api_key`, `API-KEY`,
`apiKey` and `api key` are all the same key, but a *prefixed* name is a
different key entirely. It is not a substring or prefix match.

That catches people out on headers, because the ones that carry credentials in
real deployments are almost all prefixed:

| Header | Normalizes to | Matches a default? |
| --- | --- | --- |
| `authorization` | `authorization` | yes |
| `cookie` | `cookie` | yes |
| `x-api-key` | `xapikey` | **no** — recorded verbatim |
| `x-auth-token` | `xauthtoken` | **no** — recorded verbatim |
| `proxy-authorization` | `proxyauthorization` | **no** — recorded verbatim |
| `x-amz-security-token` | `xamzsecuritytoken` | **no** — recorded verbatim |

If your app, your proxy or your SDK sends any of those, add them yourself
before you enable capture:

```toml
[log]
filter_parameters = [
  "x-api-key",
  "x-auth-token",
  "proxy-authorization",
  "x-amz-security-token",
]
```

The same holds for query and body keys: `stripe_secret_key` is not `secret`.
When in doubt, send a request through a route with the dev error page on and
look at what it shows — it uses this same list.

### Handling capsules safely

- Capsule files are written **owner-only** (`0600` on unix), through a temp file
  and a rename, so no reader ever sees a half-written capsule.
- The directory defaults to `tmp/autumn-capsules`, project-relative. **Do not
  commit it**, and do not serve it. `autumn new` ignores `/tmp/` for you; if
  your project predates that, add it (or the capsule directory itself) to
  `.gitignore` before you enable capture.
- `max_capsules` (default 50) prunes oldest-first *before* each write, so an
  error storm cannot fill a disk. A capsule handed to the error reporters is
  pinned from the instant it is written until the whole reporter chain
  finishes, so the path on an `ErrorEvent` always resolves; on top of that a
  bounded number of the newest over-cap files get a one-minute grace (for a
  second process sharing the directory, whose pins this one cannot see). The
  cap is a disk guard, not an exact file count: under a storm the directory
  can briefly hold up to roughly twice `max_capsules`, plus whatever reporters
  still hold pinned.
- Moving a capsule off the failing host moves production data with it. Treat the
  copy the way you would treat the original.
- Turning capture on in production is a deliberate decision. Turning it on in
  staging, or on demand during an incident, gets you most of the value at a
  fraction of the exposure.

### Replay only capsules you trust

A capsule is **input to your own code**. `autumn replay` builds your
application and runs its handlers against the request and the database answers
the file contains, on a machine that is holding your real configuration and
credentials. Replay forces the obvious things offline — sessions are in-memory,
the database is an in-process stub fed from the tape, outbound HTTP and channel
delivery are blocked, and no port is bound — but your handlers, your extractors
and your custom middleware still execute, and they execute against bytes an
attacker chose if the capsule came from somewhere you do not control.

So treat a capsule the way you would treat a request fixture someone emailed
you: replay the ones you recorded (or a colleague did), and if you must replay
one from outside, do it in a sandbox — a container or a scratch checkout —
whose environment holds no production credentials.

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
  otherwise mean "record the failure, then throw it away". Pruning only ever
  deletes files whose names match the capsule pattern — anything else you keep
  in the directory is left alone.

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

`max_capsule_bytes` is not the only ceiling. Four fixed caps exist so that a
pathological request cannot turn capture into an unbounded allocation, and each
one changes what you get back:

| Cap | Limit | What happens |
| --- | --- | --- |
| Clock readings per capsule | 10 000 | A handler that reads `state.clock()` in a loop stops being recorded past the cap and the capsule is marked `truncated` — so replay refuses it |
| Exchanges in flight on one connection | 64 | More pipelined-but-unanswered exchanges than that and the connection gives up: its tape is dropped, noted, and the capsule marked `truncated` |
| A single protocol frame | 8 MiB | A frame larger than this cannot be framed; the connection is treated as unrecordable, exactly like a `COPY` stream — tape dropped, capsule `truncated` |
| Connection memo | 256 entries per bucket, 1 MiB total | The memo is *not* truncation: entries past the cap are simply not remembered, and a replay that then meets a `Bind` against a statement the capsule never described reports it as a **divergence**, not a refusal |

The memo is also bounded on the way *in* to a capsule: copying a connection's
history into the capsule is charged against `max_capsule_bytes` — and capped
well below it, so a fat memo cannot crowd out the request's own traffic. A memo
too large to copy is written down in `notes`; a `max_capsule_bytes` that then
runs out mid-request truncates as above. If you see either on a route you care
about, raise `max_capsule_bytes` rather than guessing at what was lost.

---

## Replaying

```console
$ autumn replay tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
```

| Flag | Meaning |
| --- | --- |
| `-p`, `--package <PKG>` | Package to build and run, for workspaces |
| `--bin <BIN>` | Binary target, for packages with several |
| `--profile <PROFILE>` | Profile forwarded to the app as `AUTUMN_ENV`/`AUTUMN_PROFILE` (defaults to the profile the capsule recorded, else `dev`) |
| `--release` / `--debug` | Cargo build kind for the replay binary (defaults to the build kind the capsule recorded, else a debug build) |
| `--features <FEATURES>`, `--no-default-features` | Cargo features for the replay binary — the capsule cannot record the recording binary's feature set, so pass the failing build's features when they gate code the failure depends on |

The CLI compiles your application — with the same build kind the failing
binary used, so `cfg(debug_assertions)`-gated code and release-only behaviour
(overflow handling, optimizer-dependent timing) line up — and runs it with
`AUTUMN_REPLAY_CAPSULE` set —
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
- outbound HTTP and channel delivery are refused, so replaying a capsule cannot
  call a third-party API or notify anyone;
- no port is bound, and capture is forced off so a replay cannot capsule itself.

What still runs is your code: handlers, extractors, custom middleware, state
initializers and any `Layer` you installed. That is the point — and the reason
to [replay only capsules you trust](#replay-only-capsules-you-trust). It also
means the offline guarantees above cover the framework's own seams, not your
code's: a state initializer that dials an external service — a feature-flag
store, a remote config fetch — will still try to dial it during a replay boot
(see [Limitations](#limitations)).

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
| `reproduced` | Same outcome — status code, message and Problem Details type — and the database traffic matched the tape. The bug is still there. | `0` |
| `mismatch` | The tape lined up but the outcome differs, in the code, the message or the problem type. Usually what you want after a fix. | `1` |
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

### Step-debugging a replay in VS Code

`autumn replay` is a thin wrapper: it compiles your app and runs the binary
with `AUTUMN_REPLAY_CAPSULE` set. Point a debugger at the binary with that
variable and you can step through the failing handler with the database served
from the capsule and the clock replayed — the same code path on every run,
because the inputs are identical every time. "Going back in time" is
restarting the debug session.

With the [CodeLLDB] extension:

```jsonc
// .vscode/launch.json
{
  "type": "lldb",
  "request": "launch",
  "name": "Debug capsule replay",
  "cargo": { "args": ["build", "--bin", "my-app"] },
  "env": {
    "AUTUMN_REPLAY_CAPSULE": "${workspaceFolder}/tmp/autumn-capsules/<id>.json",
    "AUTUMN_ENV": "dev"
  }
}
```

Breakpoint pauses are safe: replay clears the global request timeout (a
deterministic offline run has no wall-clock deadline), and the in-process
stub database waits indefinitely. The verdict still prints when you resume to
completion.

[CodeLLDB]: https://marketplace.visualstudio.com/items?itemName=vadimcn.vscode-lldb

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
dev profile. **Measured serially: the benchmark awaits each request before
issuing the next, so there is never more than one request in flight.** Nothing
below captures contention. Capture takes a process-wide registry lock twice per
request (once to register the scope, once to drop it) and once per database
checkout; under real concurrent load those acquisitions are shared, and these
figures say nothing about what that costs.

Two routes, because they answer different questions:

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

So: **tens of microseconds per request.** As percentages of the same tables,
that is **11.5–12.6%** of a request that does nothing at all (55/479 at p50,
76/606 at p95, 62/491 on the mean) and **2.6–4.2%** of one that talks to a
database once (80/1 922, 62/2 382, 82/1 976). A repeat run put the two p50
deltas at +43 µs and +128 µs instead — 9.0% and 6.7% — so across both runs the
honest ranges are roughly **9–13%** and **3–7%**. That spread *is* the finding:
treat ±50 µs as indistinguishable here, and re-measure rather than quoting these
percentages as a budget.

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
- **Work a handler `tokio::spawn`s is outside the request's clock.** A task
  the handler spawns carries neither the capture scope nor the replay scope
  (task-locals do not cross `spawn`), so its clock reads are not recorded and,
  during replay, are served a stable non-consuming timestamp instead of the
  recorded sequence — they can never shift the handler's own readings, but a
  spawned task whose *result* depends on those reads may still diverge. Work
  the handler awaits inline is fully covered.
- **Same-commit replay is what is tested.** A capsule recorded by a different
  build of the framework warns; a capsule recorded by different *application*
  code will usually diverge, which is the honest outcome rather than a bug.
- **Concurrent connections inside one request** (a `join!` over two checkouts)
  are recorded per connection, but their ordering is not guaranteed to repeat,
  and a different interleaving shows up as a divergence. Connections a request
  uses one after another are fine: tapes are recorded — and handed back on
  replay — in the order the request *first used* each connection, not by
  connection id. Pool contention can produce the same effect without any
  concurrency in your code: a request that checked out twice and happened to be
  handed two *different* connections under load records two tapes, while the
  replay — which has no contention — may serve the whole request from one. That
  is a faithful capsule reporting a divergence, not a corrupt one.
- **`PostgreSQL` only, over plaintext TCP.** Capture frames protocol messages,
  and it cannot frame ciphertext, so a database URL asking for TLS —
  `sslmode=require`, `verify-ca` or `verify-full` — disables database capture,
  as do a Unix-socket URL and a `sqlite` build. `sslmode=prefer`, `disable`, or
  no `sslmode` at all do *not*: Autumn connects in plaintext for those, and
  capture works. When it is off the capsule still records the request, clock and
  outcome, and says in `notes` why it has no tape. A `PostgreSQL` tape handed to
  a `sqlite` build is refused outright.
- **A custom `DatabasePoolProvider` disables database capture.** Autumn will not
  second-guess a pool you built; it logs a warning and notes it on every capsule.
- **`LISTEN`/`NOTIFY` is unsupported on capture-enabled request pools.** The
  notification stream is not available on recorded connections. Autumn's own use
  of it (sharding) runs on a dedicated listener connection and is unaffected.
- **`COPY` streams are not modelled.** A `COPY IN`/`OUT` inverts flow control;
  the connection's tape is dropped and the capsule marked truncated.
- **Shard pools are not recorded.** `[[database.shards]]` connections are built
  separately; a request that checks one out has its capsule noted and truncated.
- **A failing response with a streaming body ends the recording at the response
  head.** An SSE or `Body::from_stream` 5xx keeps running handler code while
  the client reads it; those effects are not on the tape, so the capsule is
  noted and marked truncated rather than presented as replayable.
- **A handler that extracts a subsystem the replay does not boot** — a
  `Mailer`, a `BlobStore` — fails during replay and is reported as a mismatch
  rather than taking the replay process down.
- **Randomness is not recorded.** A handler that draws from `Rng` /
  `state.entropy()` — a fresh UUID, a token — draws *different* bytes during
  replay; if those bytes reach a SQL bind, the replay reports a bind divergence
  naming the statement. Recording the entropy seam is a follow-on slice, like
  outbound HTTP; the divergence is the honest signal until then.
- **Custom exception filters that rewrite failure identity can mis-verdict.**
  The capsule records the outcome where the framework observes failures —
  before the exception-filter chain runs — while replay observes the response
  the full chain produced. The framework's own filters preserve identity, so
  this only matters for a custom `exception_filter` that replaces the status
  or message of a 500 (mismatch against unchanged code) or promotes a non-5xx
  to a 5xx (no capsule at all — the same observation-scope trade-off
  documented for error reporting).
- **State initializers are not fail-closed.** Replay drops the framework's own
  outbound clients — the session store, channels, the mailer, the `reqwest`
  client — but a state initializer is your code and runs as written during the
  replay boot. One that reaches an external service directly (a feature-flag
  SDK with its own HTTP stack, a remote config fetch) will still try to reach
  it; point such initializers at a local or stubbed endpoint when replaying, or
  they become a live dependency the verdict silently depends on.
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
