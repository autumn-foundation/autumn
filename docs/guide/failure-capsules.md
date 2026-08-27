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
| The resolved client identity | `client_addr`, `client_host` and `client_scheme` are *derived from* `Forwarded` / `X-Forwarded-*` / `X-Real-IP` / `Host`, so filtering any header a field could come from drops that field too — otherwise a masked `X-Forwarded-Host` would reappear verbatim one key away. When a filtered source actually supplied a value, the capsule is also **refused by replay**: it cannot reproduce the identity the handler saw, and a handler that branches on `ClientHost` would answer differently |
| SQL bind parameters echoing a masked value | byte-equal binds become `"masked"`, and are excluded from replay's bind comparison |
| The outcome message, panic payload and backtrace | any masked value quoted back inside them is substring-replaced. Values shorter than four characters (a CVV, a PIN) are masked only where they stand as a whole token, so a short secret is removed without shredding timestamps and identifiers |
| Credentials *inside* a masked header | the token after an auth scheme (`Bearer …`), what a `Basic` credential decodes to (the `user:password` pair and the password alone), each value of an auth-param list (`Signature=…`), and each cookie value join the echo set on their own, because that is the form a handler extracts and may echo. Auth-param values are masked only where they stand as whole tokens, since the list mixes secrets with metadata (`qop=auth`) that would otherwise shred prose. Usernames and cookie *names* are not recorded at all — they are ordinary words |
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
  A capsule whose body went uncopied — or which the handler stopped reading
  partway through — is **refused** by replay rather than replayed with a
  shorter one: the handler would be judged on input the failing request never
  had, and the resulting `mismatch` reads as "the bug is gone".
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
  "format_version": 3,
  "id": "01JB2K7Q8N4W",
  "captured_at": "2026-08-12T10:14:13.882104Z",
  "autumn_version": "0.7.0",
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
  "effects": {
    "http": [
      { "method": "POST", "url": "https://tax.example/quote",
        "request_headers": [["authorization", "[FILTERED]"]],
        "request_body": { "text": "{\"total\":9900}" },
        "status": 503, "response_headers": [], "response_body": { "text": "upstream busy" },
        "error": null }
    ],
    "jobs": [{ "name": "send_receipt", "payload": { "order": 42 }, "delay_secs": null,
               "error": null }],
    "cache": [{ "op": "get", "key": "tax_rate:CA", "value": "eyJyYXRlIjowLjA5fQ==" }],
    "mail": [],
    "tenant": { "id": "acme" },
    "random": [{ "bytes": "3q2+7wAAAAAAAAAAAAAAAA==" }]
  },
  "job": null,
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

**The effect seams.** Beyond the request, the clock and the database, a capsule
records every framework effect the failing run produced. Each is captured at the
one choke point every code path funnels through, so a capsule cannot miss an
effect because your handler reached it a different way:

| Seam | Captured at | Replayed as |
| --- | --- | --- |
| Outbound HTTP | `http_client::RequestBuilder::send` | The recorded response is handed back — but only to a call that matches the recording's method, URL, caller-set headers *and* body; **no socket is opened**. Outbound webhook deliveries are covered here too, they send through the same client |
| Job enqueue | every `job::enqueue*` entry point | The enqueue is *asserted* against the recording and returns `Ok(())`; **nothing is written to a queue and no job runs** |
| Cache | `cache::get_cached` / `insert_cached` | A recorded hit is served from the capsule and a recorded miss replays as a miss; a write lands in the tape, so a read-back in the same run finds it |
| Mail | `Mailer::send` | The send is asserted against the recorded recipients, subject, sender and body; **nothing is delivered** |
| Tenancy | `tenancy::extract_tenant_from_parts` | The recorded tenant is served without consulting live tenant configuration |
| Randomness | `state.entropy()` | Every draw replays byte-for-byte, so the session id, CSRF token, request id or job id the failing request minted reappears |

**Randomness is recorded in the clear, by construction.** The bytes a request
drew *are* the session id, the CSRF token, the reset token it minted — that is
what makes replay reproduce them — so they cannot be masked and still replay,
and no `filter_parameters` entry suppresses them. Treat a capsule from a route
that mints credentials the way you would treat the credential itself: it is the
single place in the document where redaction cannot help you. (The same is
already true of database result rows, which the capsule records verbatim.) If
that is unacceptable for a particular route, capture is per-application: leave
`[failure_capture]` off, or prune the capsule directory aggressively.

Two things are worth stating plainly about randomness. Autumn records the
*drawn bytes*, not a seed: production runs on the OS CSPRNG, which has no seed
to record, and a re-seeded stream would mint different UUIDs than the ones the
capsule's own SQL binds were bound with. And a handler that calls
`Uuid::new_v4()` directly rather than drawing through `Rng` / `state.entropy()`
is outside the seam — its identifiers will differ on replay. The determinism
gate (`autumn-determinism-gate`) is what keeps framework code on the seam; your
own code should stay on it too.

Effects are redacted through the **same** `[log] filter_parameters` list the
inbound request is. That is deliberate and not merely tidy: an *outbound*
`Authorization` header carries a downstream credential exactly the way an
inbound one carries the caller's, and a job payload or a cache value is as
likely to hold a token as a request body is. Anything masked on one seam is
also masked wherever it is quoted back on another — an outbound body's secret
is scrubbed out of an error message the handler later produced. The
`redacted_keys` manifest names every masked location, effects included
(`http[0].request_header:authorization`, `job[0].api_key`, …). On the outbound
seam a short list of conventional credential headers — `Authorization`,
`Cookie`, `Set-Cookie`, `X-API-Key`, `X-Auth-Token` and a few others — is
masked *whatever* your filter says: the credential there is your
application's, not your caller's, and you would never see the header in a log
to think to name it.

Ordering is **per seam**, not global. Two outbound calls a handler `join!`s
have no deterministic interleaving against each other, let alone against a
cache read on a third task, so a single global order would be a fact the
recording cannot establish — and replay would then report divergences for
interleavings that were never guaranteed.

Each seam is capped at 2 000 entries per capsule; crossing the cap marks the
capsule `truncated`, which replay refuses.

**Job capsules.** A failure *inside* a job execution produces a capsule whose
`job` field names the job and carries its (redacted) payload; `request` then
holds a synthetic descriptor of that entry point rather than a real HTTP
request.

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
- **every config-driven store the request path can reach is forced local** —
  rate limiting, idempotency keys, submit tokens, webhook replay protection,
  the response cache and the job queue. A replayed request *writes* to these
  (it decrements a bucket, takes a key and its in-flight lock, consumes a
  token, inserts a replay key), so pointing them at the recording deployment's
  Redis would make diagnosing a failure change production state — and an
  unreachable backend would manufacture a `429` or `503` the recorded run never
  produced;
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
| `reproduced` | Same outcome — status code, message and Problem Details type — and both the database traffic and the effect tape matched the recording. The bug is still there. | `0` |
| `mismatch` | The tape lined up but the outcome differs, in the code, the message or the problem type. Usually what you want after a fix. | `1` |
| `diverged` | The code asked the database — or an effect seam — something the recording never asked: an unrecorded query, an outbound call the capsule has no response for, an enqueue or mail send the recording never made, a recorded one the run never made. The comparison was not fair, so a divergence outranks a matching status. | `1` |
| `refused` | Nothing was replayed — a truncated capsule, a capsule whose request body was never recorded or only partly read (over `max_body_bytes`, an unparseable structured body, or a handler that abandoned the read), a job capsule whose payload was masked by `[log] filter_parameters`, an unknown `format_version`, an unreadable file, or a `PostgreSQL` tape handed to a `sqlite` build. | `2` |

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

## From capsule to regression test

A replay answers "is this bug still there?" once. Converting the capsule
answers "can it ever come back?": the capsule is copied into your test tree and
an ordinary `#[tokio::test]` is generated beside it, so `cargo test` re-checks
the failure from then on.

```console
$ autumn capsule test tmp/autumn-capsules/20260812T101413.882104-000000-01JB2K7Q.json
wrote tests/capsules/c01jb2k7q8n4w.json
wrote tests/integration/capsule_c01jb2k7q8n4w.rs
wrote tests/integration/capsule_support.rs — add the app's routes to `router` before running the test
registered the modules in tests/integration/mod.rs

Run it with:  cargo test capsule_c01jb2k7q8n4w
The whole corpus:  autumn capsule verify
```

| Flag | Meaning |
| --- | --- |
| `--name <NAME>` | Name the fixture and test yourself instead of slugging the capsule id |
| `--tests-dir <DIR>` | The crate's tests directory (default `tests`) |
| `--force` | Overwrite an existing fixture and test of the same name |

A capsule recorded from a **job** failure is refused here: it has no request to
drive through a router, so there is no router-driven test to generate. Replay it
with `autumn replay`, which dispatches the job's handler.

**Nothing is committed for you.** The files land in the working tree for review;
whether they belong in the repository is your call.

Three properties are worth knowing, because they are what make the generated
test trustworthy:

* **Zero live dependencies.** Everything the replayed handler touches comes from
  the capsule — the clock, randomness, outbound HTTP, jobs, cache, mail and the
  tenant — and the database comes from the same **in-process** stub server
  `autumn replay` uses, rebuilt out of the recorded wire frames. No network, no
  database, no queue, no Docker. It runs under plain `cargo test`.
* **Redaction survives conversion.** The fixture is the capsule's own bytes,
  copied verbatim: nothing is re-derived, re-read from a live system, or
  re-serialized. Whatever redaction removed on the way to disk is exactly what
  stays removed on the way into your repository. A capsule that was safe to hold
  is safe to commit; one that was not, still is not — read it before you commit
  it, the same way you would read a log excerpt.
* **One replay engine.** The generated test drives the same
  `capsule::execute` the CLI does, rather than re-deriving the comparison in
  generated code, so a committed test and `autumn replay` can never disagree
  about what a reproduction is.

The one thing generation *cannot* infer is your route table, so it scaffolds a
router hook once and then leaves it alone:

```rust,ignore
// tests/integration/capsule_support.rs
use autumn_web::capsule::regression::RegressionContext;
use autumn_web::test::TestApp;

pub fn router(ctx: &RegressionContext<'_>) -> axum::Router {
    TestApp::new()
        .routes(autumn_web::routes![checkout, charge])
        .with_clock(ctx.clock())
        .with_entropy(ctx.entropy())
        .build()
        .into_router()
}
```

For a capsule whose handler queried a database, add the stub pool from the same
context — still offline:

```rust,ignore
let mut app = TestApp::new().routes(autumn_web::routes![checkout]);
if let Ok(pool) = ctx.db_pool() {
    app = app.with_db(pool);
}
app.with_clock(ctx.clock()).with_entropy(ctx.entropy()).build().into_router()
```

A generated test fails on a **mismatch** (the outcome changed — usually the bug
is fixed and the test has done its job, so re-record or delete it) and on a
**divergence** (the handler's database traffic or effects changed underneath the
capsule). The panic message is the full report, so CI says *what* changed.

### The whole corpus

```console
$ autumn capsule verify
ok          tests/capsules/c01jb2k7q8n4w.json
ok          tests/capsules/checkout_500.json

2 capsule(s) in tests/capsules, 0 unusable by this build
running the generated tests: cargo test capsule_
...
the corpus replays clean
```

Two halves, in order. First the corpus-level questions `cargo test` cannot
answer: that the directory exists, that it is not empty — an empty corpus is
reported as a **failure**, never as a vacuous pass — and that every committed
capsule is still readable and replayable by *this* build. Then the generated
tests themselves, via `cargo test capsule_`. Pass `--check-only` for the first
half alone.

Running them by delegation rather than re-implementing replay in the CLI is the
point: the generated tests drive the same `capsule::execute` engine `autumn
replay` does, so there is exactly one replay engine and no way for two of them
to disagree.

To sweep the corpus programmatically (a whole-corpus `#[test]` of your own, say),
`RegressionCase::corpus(dir)` lists every committed capsule in chronological
order.

---

## Compatibility across Autumn versions

A capsule declares the format version that wrote it, and a build replays a
capsule **only** at its own version. Anything else is refused, with a message
that names the direction:

```console
$ autumn replay tmp/autumn-capsules/old.json
REFUSED  tmp/autumn-capsules/old.json
  capsule format version 2 is older than the version this build understands (3), so
  replaying it would judge the handler against effects the document cannot describe.
  Re-record the capsule with this build, or replay it with the Autumn version that
  wrote it — see the "Compatibility across Autumn versions" section of the
  failure-capsule guide.
```

Refusing is the point. A reader that tolerated an older document would rebuild
an application shape production never ran — no effect tape at all, in the case
of version 2 — and then report a verdict on it. A spurious `reproduced` (or a
spurious "the bug is gone") is worse than no verdict, so the gate is absolute.

What this means in practice:

* **A capsule is a debugging artefact with the lifetime of a release.** Capsules
  sitting in `[failure_capture] dir` when you upgrade Autumn are not portable
  across the upgrade; replay them before you deploy the new version, or re-record
  the failure afterwards.
* **A committed corpus is re-recorded, not migrated.** After an Autumn upgrade
  that bumps the format, `autumn capsule verify` reports every committed capsule
  as `UNREADABLE` and exits non-zero — deliberately, so the corpus cannot quietly
  stop testing anything. Re-record the failures that still matter and convert
  them again.
* **The version bumps only for changes a previous reader cannot tolerate.**
  Adding a field `serde` would happily ignore still counts when it is
  *semantic*: version 2 added the database-role list, and version 3 added the
  effect tape and the job entry point.

| Format version | Added |
| --- | --- |
| 1 | The request, the clock, the database tape and the outcome |
| 2 | The configured database roles |
| 3 | The effect tape (outbound HTTP, jobs, cache, mail, tenancy, randomness) and job-scoped capsules |

Because the corpus replays a *committed* recording against *current* code, it
doubles as an upgrade gate: run it against a new Autumn version before you
deploy that version, and a behavioural change in the framework shows up as a
divergence rather than as a production incident.

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

What capsules do not do, stated plainly:

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
- **Randomness is recorded only through the framework's seam.** Draws through
  `Rng` / `state.entropy()` replay byte-for-byte. A handler that calls
  `uuid::Uuid::new_v4()` or the OS RNG *directly* bypasses the seam and draws
  different bytes on replay; if those bytes reach a SQL bind, the replay reports
  a bind divergence naming the statement. That divergence is the honest signal —
  move the call onto the seam to fix it.
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
- **Channels/SSE, blob storage and feature flags have no seam yet.** A handler
  that publishes to a channel, writes a blob or evaluates a remote flag during
  replay is not served from the capsule; the effect either no-ops against the
  in-process backend replay installs or reaches a live service, and a
  flag-dependent branch may then differ from the recording. Those seams are a
  later slice.
- **A cache hit whose value was never serializable cannot be served.** The
  capsule carries cache values as the JSON bytes `insert_cached` already
  produces; an in-process-only value (stored through the untyped
  `cache::insert`) is recorded as a keyed read with no value, and the replayed
  read is a miss. It is on the tape, so it is not reported as an *unrecorded*
  key — but the handler takes the miss branch.
- **A run that resolved two different tenants is not replayable.** A capsule
  holds one tenant context; a run that switched tenants mid-flight is noted and
  marked truncated rather than replayed against whichever one happened to be
  first.
- **Effects on `tokio::spawn`ed tasks are outside the tape**, for exactly the
  reason clock reads are: the task-local does not cross `spawn`. Under `autumn
  replay` such a task's outbound call is refused by the process-wide block
  rather than served from the capsule. A **generated regression test** has no
  such block — it runs inside your ordinary `cargo test` process, where a
  process-wide, permanent outbound block would break every other test — so an
  outbound call from a spawned task there reaches the network. Mail handed to
  `Mailer::deliver_later` is spawned for the same reason and behaves the same
  way. Keep the effects a capsule needs on the awaited path.
- **A transactional enqueue makes a capsule unreplayable.** `enqueue_on_conn`
  (and its delayed and absolute variants) is two recorded effects for one
  action on the `postgres` backend: the enqueue itself, and the job-row INSERT
  on the caller's connection, which the database tape records. Replay serves
  the first and can never issue the second — it answers the enqueue from the
  tape before any job client is reached, and it starts no job runtime to
  rebuild the statement with. Rather than report an unchanged request as
  `diverged`, the capsule notes the enqueue and marks itself incomplete.
- **A job capsule whose payload was redacted is refused.** A job handler is
  handed its payload *verbatim*, so unlike an effect there is no wildcard
  reading of `[FILTERED]` at that boundary: the handler would parse or branch
  on the placeholder. Redaction is not reversible, so the capsule is refused
  by name rather than replayed against input production never had.
- **An effect still in flight when the recording ends is not an outcome.** A
  seam takes its tape position when the effect *starts*; a future cancelled
  before it finishes — the losing branch of a `tokio::select!`, a timeout —
  never completes its slot. Rather than persist the placeholder as a recorded
  backend failure, the capsule notes it and marks itself incomplete.
- **Only `get_cached` / `insert_cached` are on the cache seam.** The untyped
  `cache::get` / `cache::insert`, and `Cache::invalidate` / `Cache::clear`, are
  not: during a replay they reach whatever backend is installed (none, under
  `autumn replay`, which clears the global cache). A handler that invalidates a
  key during a replayed run is doing it for real.
- **Only outbound HTTP made through the framework client is on the seam.** A
  handler — or a framework subsystem such as CAPTCHA verification — that builds
  its own `reqwest` client bypasses both the capsule and, in a generated test,
  the block. `autumn replay` cannot see those calls either; it only blocks the
  framework's own client.
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
