# Extend replay capsule seams and convert capsules to regression tests (#1634)

Parent: #1598 (deterministic replay capsules — inbound request, DB, clock).
Targets `trunk-dev`. Complexity tier **L**, phased.

## 1. Where #1598 left off

`autumn/src/capsule/` records one failing request as a JSON document holding the
redacted request head/body, the wall-clock and monotonic readings the handler
took, and the PostgreSQL wire traffic it generated. `autumn replay <capsule>`
re-execs the app binary with `AUTUMN_REPLAY_CAPSULE=<path>`, which rebuilds the
router offline (`AppBuilder::run_replay_mode`), serves the clock from
`ReplayClock` and the database from an in-process stub server, and prints a
verdict: `reproduced` / `diverged` / `mismatch` / `refused`.

Everything **outside** that slice is handled by *blocking*, not replaying:
`http_client::block_outbound_for_replay()` fails every outbound call closed, the
job runtime and scheduler never start, no mailer is built, the global cache is
cleared, and randomness has no seam at all in the capsule (though the framework
does already own an injectable `Entropy` source, from the determinism-seam work
in #1797).

So the gap is exactly what #1634 says: any failure whose behaviour depended on
an outbound HTTP response, an enqueue, a cache hit, a mail send, the resolved
tenant, or a generated UUID cannot be reproduced — and even a perfect
reproduction is one-shot, because nothing turns it into a committed test.

---

## 2. Planning

### 2.1 Brainstorming — how could we capture the remaining seams?

1. **Per-seam tape in the capsule document.** One typed list per effect kind,
   appended in call order, served back in order on replay. Mirrors what `clock`
   already does.
2. **One heterogeneous, totally-ordered effect log.** A single `Vec<Effect>`
   preserving cross-seam ordering (HTTP call → enqueue → cache write).
3. **Wrap every seam in an interceptor** using the existing
   `autumn/src/interceptor.rs` traits (`MailInterceptor`, `JobInterceptor`,
   `HttpInterceptor`, `ChannelsInterceptor`).
4. **Record at the transport boundary** (a `reqwest` middleware, a real SMTP
   tee, a Redis proxy) the way DB capture tees the PostgreSQL wire.
5. **Record at the framework choke point** — the one function every path funnels
   through (`RequestBuilder::send`, `job::enqueue`, `cache::get_cached`,
   `Mailer::send`) — reading the ambient `CaptureScope` task-local.
6. **Snapshot-based cache replay**: don't record reads at all, snapshot the
   whole cache and restore it.
7. **Seeded entropy instead of recorded entropy**: record only the seed, replay
   with a `SeededEntropy`.
8. **Record the drawn bytes**, not the seed, so production's `OsEntropy` (which
   has no seed) is reproducible too.
9. **Codegen a standalone Rust test** that inlines the recorded request and
   asserts on the status.
10. **Codegen a thin test that loads the capsule fixture** and drives the same
    replay engine `autumn replay` uses.
11. **Whole-corpus mode as a CLI sweep** (`autumn capsule verify`) that boots the
    app binary once and replays a directory.
12. **Whole-corpus mode as a generated `#[test]`** that iterates the committed
    fixtures under `cargo test`.
13. **Version the capsule format** and refuse mismatches (already exists —
    extend the refusal message so it names the *feature* that is missing).
14. **Per-seam capability flags** in the capsule so an old capsule replays
    partially instead of being refused.

**Chosen:** 1 + 5 + 8 + 10 + 11 + 12 + 13.

* (1) over (2): the DB tape is already per-connection and unordered against the
  clock; a global order would be a lie under concurrency (`join!`ed outbound
  calls have no deterministic interleaving), and per-seam order is what a
  divergence report can actually explain. Cross-seam ordering buys nothing a
  replay can enforce.
* (5) over (3)/(4): interceptors are an *application* extension point installed
  through `AppBuilder`; capture must work with zero app configuration, and an
  interceptor installed by the framework would collide with a user's. The
  transport boundary (4) is unavailable for the cache and for jobs, and for HTTP
  it would mean owning a `reqwest` middleware stack we do not otherwise need.
* (8) over (7): production runs `OsEntropy`, which has no seed to record.
  Recording drawn bytes reproduces the *actual* identifiers the failing request
  minted, which is the whole point — a re-seeded stream would produce different
  UUIDs than the ones in the recorded DB binds.
* (10) over (9): inlining the request would duplicate the capsule's semantics in
  generated code and rot the moment the schema changes; loading the fixture
  keeps exactly one replay engine, so a generated test and `autumn replay` can
  never disagree.
* (14) rejected: a partial replay that *looks* authoritative is precisely the
  "spurious pass" the compatibility AC forbids.

### 2.2 Reverse brainstorming — how do we make this fail?

Asked as: *what would guarantee this feature is worthless or harmful?*

| Failure we could cause | Guard we will build |
| --- | --- |
| Replay silently dials a live third party | Replay serves HTTP from the tape; an **unrecorded** request is a divergence + error, never a live call. The existing `block_outbound_for_replay` fail-closed stays as the backstop for anything the tape does not answer. |
| Replay actually enqueues a job / sends mail | Enqueue and mail are *asserted*, never executed, in replay mode; the recorded call is consumed from the tape and a `Ok(())` is synthesised. |
| Generated tests leak secrets into the app repo | Generation copies the **already-redacted capsule bytes** and never re-reads anything live. A test asserts the copied fixture is byte-identical to the source capsule, and that a capsule carrying `redacted_keys` still carries them after conversion. |
| A capsule recorded before this change replays "successfully" with all its new seams empty | `CAPSULE_FORMAT_VERSION` bumps 2 → 3. A v2 capsule is *refused*, not read. |
| A capsule with effects replays on a build that ignores them | Same version gate, pointed the other way. |
| A stale/oversized effect tape blows up memory on a failing request | Every effect buffer is count-bounded (`MAX_EFFECTS_PER_KIND`) and body-bounded (`max_body_bytes`); overflow marks the capsule truncated, and truncated capsules are already refused. |
| A reproduction that never touched the recorded effect looks perfect | Unconsumed effects are divergences too — exactly the rule the DB tape already applies (`UnconsumedExchanges`). |
| Generated tests need Docker/Postgres and rot in CI | The generated test drives `axum::Router` in-process. Where the capsule has DB traffic, generation refuses and says so, rather than emitting a test that needs a database. |
| Two capsules generate the same test module name | Slug derived from the capsule id; collision is an error unless `--force`. |
| The corpus mode passes because it found zero capsules | Empty corpus is an explicit failure, not a vacuous pass. |
| Effect capture slows every request in production | Recording is behind `current_scope()`, which is `None` unless `[failure_capture] enabled = true`; the fast path is one task-local probe, which #1598's overhead test already covers. |

### 2.3 Six hats

**White (facts).** Seams that exist today: `http_client::RequestBuilder::send`
(single choke point, already replay-aware), `job::enqueue*` (all funnel to an
`enqueue` free function), `cache::get_cached` / `insert_cached` (used by
`#[cached]`, `fragment`, `layer`, `read_through` — every cache path),
`Mailer::send`, `tenancy::with_tenant` / `Tenant`, and `entropy::Entropy` on
`AppState` (already injectable, already used by `SeededEntropy` in sim tests).
`CaptureScope` is reachable from all of them via the `CAPSULE_SCOPE`
task-local. Replay mode is a process-global one-shot (`AUTUMN_REPLAY_CAPSULE`),
so a process-global replay tape is consistent with what is already there.

**Red (instinct).** The risky part is not capture, it is the *generated test*:
it is the piece most likely to be a demo that nobody can actually use, because
the generator cannot know the app's route table. Be honest about that seam —
generate a compiling file with an explicit, documented router hook rather than
pretending to infer it.

**Black (what breaks).** (a) The effect tape must not deadlock: seams are called
from inside async code holding no lock, and every buffer must be a short
`Mutex` critical section — never held across an `.await`. (b) `get_cached` is
generic over `V`; the tape can only carry JSON, so a cache hit whose value was
never serialisable cannot be served — that must be a divergence, not a silent
miss. (c) Job *execution* capsules need a capture scope around a job run, which
today only exists around an HTTP request. (d) Redaction currently keys off the
inbound request's filtered parameters; an outbound body must be masked through
the same `ParameterFilter`, or capsules start carrying downstream API keys.
(e) Bumping the format version invalidates every capsule already on disk in a
deployed app — acceptable and documented, but must be in the changelog.

**Yellow (upside).** Every seam recorded is one more class of production failure
that reproduces offline; and because the generated test drives the *same*
`capsule::execute` engine, the corpus doubles as an upgrade gate for free — the
issue's "run the capsule suite against a new autumn version before deploying".

**Green (creative).** `ReplayFixtures` — hand the generated test the
`ReplayClock` and `ReplayEntropy` as a single value it plugs into the existing
`TestApp::with_clock` / `with_entropy` builders. No new test harness, no
parallel replay path, and the app's own `routes![...]` list stays where the
developer already maintains it.

**Blue (process).** Order of work, each phase red → green → refactor:
0. Effect schema + version gate. 1. HTTP + jobs. 2. Cache + mail + tenancy.
3. Randomness. 4. Conversion + corpus. 5. Docs + changelog. Every phase lands
its tests first, and each phase compiles and passes on its own.

---

## 3. Design

### 3.1 Capsule document (v3)

```
Capsule {
  format_version: 3,
  …existing…,
  job:     Option<CapsuleJob>,   // present ⇒ the entry point was a job, not the request
  effects: CapsuleEffects,
}

CapsuleEffects {
  http:   Vec<HttpEffect>,   // method, url, request head/body, status, response head/body, error
  jobs:   Vec<JobEffect>,    // name, payload, delay
  cache:  Vec<CacheEffect>,  // Get{key,hit,value} | Insert{key,value}
  mail:   Vec<MailEffect>,   // to/cc/bcc, from, subject, body, delivered
  tenant: Option<TenantEffect>,
  random: Vec<RandomEffect>, // base64 of every byte draw, in draw order
}
```

All fields `#[serde(default)]`, all byte fields base64, everything
round-trippable. `CAPSULE_FORMAT_VERSION` 2 → 3.

### 3.2 Capture

`CaptureScope` gains one `Mutex<EffectBuffer>`. Each seam's choke point does

```rust
if let Some(scope) = crate::capsule::current_scope() { scope.record_*(…) }
```

after the effect resolves. Bodies are redacted through the scope's
`ParameterFilter` (headers by name, structured JSON bodies by key) using the
existing `redact` helpers, so outbound `Authorization` headers and downstream
credentials are masked by the same rules as inbound ones. Buffers are
count-bounded; overflow marks the capsule truncated.

Job-scoped capsules: `job::execute` opens a `CaptureScope` for the job run when
capture is enabled, records `Capsule::job = Some(CapsuleJob { name, payload })`,
and persists on a failing run through the same `persist` path.

### 3.3 Replay

A process-global `ReplayEffects` (installed by `run_replay_mode`, exactly like
`block_outbound_for_replay`) holds one cursor per seam plus the shared
`DivergenceLog`. Each seam's choke point checks it *before* doing anything live:

* HTTP: next recorded exchange must match method+url ⇒ serve the recorded
  response; mismatch or exhaustion ⇒ `EffectDivergence` + `ClientError`.
* Jobs: next recorded enqueue must match name+payload ⇒ `Ok(())` without
  touching a queue.
* Cache: recorded `Get` for the key ⇒ serve `RawCacheBytes`; unrecorded key ⇒
  divergence and a miss.
* Mail: next recorded send must match recipients+subject ⇒ `Ok(())` without
  delivering.
* Tenancy: the recorded tenant is pre-installed.
* Entropy: `ReplayEntropy` serves the recorded byte stream; over-reads repeat
  the last block and warn, mirroring `ReplayClock`.

Unconsumed effects at the end are divergences, like `UnconsumedExchanges`.
`ReplayOutcome` gains `effect_divergences: Vec<EffectDivergence>`; the verdict is
`Diverged` if *either* list is non-empty.

### 3.4 Capsule → regression test

`autumn capsule test <capsule> [--name] [--tests-dir] [--router] [--force]`:

1. loads and validates the capsule (refusing a truncated one, or one whose DB
   tape a plain `cargo test` cannot serve),
2. copies the capsule **bytes verbatim** to `<tests-dir>/capsules/<slug>.json`,
3. writes `<tests-dir>/integration/capsule_<slug>.rs` — a `#[tokio::test]` that
   `include_str!`s the fixture and calls
   `RegressionCase::assert_reproduces(router)`,
4. registers the module in `<tests-dir>/integration/mod.rs`,
5. scaffolds `<tests-dir>/integration/capsule_support.rs` (the router hook) when
   it does not exist yet, and never overwrites it.

`autumn capsule verify [--dir]` is the whole-corpus mode: it boots the app
binary once with `AUTUMN_REPLAY_CORPUS=<dir>`, replays every capsule in the
directory and exits non-zero if any capsule diverged, mismatched or was refused.
An empty corpus is a failure.

### 3.5 Compatibility

`CapsuleError::VersionMismatch` already refuses on any mismatch. This slice
extends the message to name the direction (older/newer) and points at the
documented policy in `docs/guide/failure-capsules.md`: capsules are replayable
by the format version that wrote them; a capsule is a debugging artefact with
the lifetime of a release, and the committed corpus is re-recorded (or
re-generated) on a format bump.
