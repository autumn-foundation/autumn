# Edge capsules — running read-path routes at the CDN

An Autumn app is a native binary at one origin. Some of what it serves does not
need to be: a marketing page, a product listing, a public profile — reads whose
answer is a function of the request and, at most, a cached value. Those can run
at a CDN edge, close to the reader, while the origin keeps serving everything
else.

Autumn's answer is an **edge capsule**: a portable `wasm32-wasip1` artifact
built from your *existing* handlers by the same `autumn build`, with no second
codebase, no vendor SDK, and no rewrite. The origin binary stays the authority —
it still serves every route, including the edge ones — so anything the capsule
cannot answer falls through to it, and you write no glue for that.

Worked example: [`examples/edge-greeting`](../../examples/edge-greeting).
Design record: [ADR-0011](../adr/0011-edge-capsule-read-lane.md).

> **Experimental (issue #1790, first slice).** The `#[edge]` macro surface, the
> `autumn-edge` API, and the wire protocol are not covered by the stability
> policy yet. See [STABILITY.md](../../STABILITY.md).

## In one page

```rust
// src/handlers.rs — compiles for the host AND for wasm32-wasip1
use autumn_edge::prelude::*;

#[get("/greet/{name}")]
#[edge]
pub async fn greet(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

pub fn edge_routes() -> Vec<EdgeRoute> {
    edge_routes![greet]
}
```

```rust
// src/bin/edge-capsule.rs — the whole of a capsule's main
fn main() {
    autumn_edge::serve(my_app::handlers::edge_routes());
}
```

```rust
// src/main.rs — the origin, unchanged except that it also mounts `greet`
autumn_web::app()
    .routes(routes![my_app::handlers::greet, /* … */])
    .run()
    .await;
```

```sh
autumn build     # native binary + target/wasm32-wasip1/release/edge-capsule.wasm
```

That is the whole opt-in: an attribute, a registration macro, a three-line bin.

## The two lanes

```text
           ┌──────────── edge (wasm32-wasip1) ────────────┐
 request → │ capsule: same axum router, same handler code │ → response
           └───────────────────┬──────────────────────────┘
                               │ cannot serve this
                               ▼
           ┌──────────── origin (native binary) ──────────┐
 request → │ the whole app: db, sessions, auth, writes    │ → response
           └──────────────────────────────────────────────┘
```

The edge lane is a *subset* router built from the same `axum` (and therefore the
same `matchit`) the origin uses, from the same path patterns. Path matching
parity is true by construction rather than by a second implementation that has
to be kept honest: percent-encoding, `%2F` inside a segment, trailing slashes
and `{param}` capture all behave identically because it is literally the same
matcher.

The origin still mounts every edge route. That is what makes fallthrough free:
a request the edge declines is forwarded upstream unchanged and lands on a route
that was always there.

## Writing an edge route

### The edge-safe module rule

An `#[edge]` handler lives in a module that compiles for `wasm32-wasip1`, where
`autumn-web` is **not in the dependency graph at all**. Two rules follow, and
the compiler enforces both:

1. **Only `#[edge]` routes in that module.** A plain `#[get]` emits a native
   companion that names `::autumn_web`, and the wasm build stops on it. Put
   origin-only routes in a different module, gated with
   `#[cfg(not(target_arch = "wasm32"))]`.
2. **Import from `autumn_edge`.** `autumn_edge::prelude` carries the route
   macros and exactly the extractors the edge can mediate.

```rust
// src/lib.rs
pub mod handlers;                          // edge-safe: #[edge] GET routes only

#[cfg(not(target_arch = "wasm32"))]
pub mod origin;                            // anything autumn-web offers
```

The manifest splits the same way — this is what keeps tokio, hyper and diesel
out of the capsule:

```toml
[dependencies]
autumn-edge = "0.6"

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
autumn-web = { version = "0.6", features = ["edge"] }
```

### What an edge handler may use

| Available | Not available |
| --- | --- |
| `Path`, `Query`, `HeaderMap` | sessions, auth, CSRF, flash, step-up |
| `EdgeCache` (a mediated KV read) | a database, a repository, writes of any kind |
| any pure rendering, including maud markup you pass through | `Clock`, `Rng`, outbound HTTP, the filesystem |

This is enforced by the type system, not by review. An edge handler must be an
`axum` handler over the unit `EdgeState`, so an extractor that needs real
application state cannot satisfy the `EdgeHandler` bound, and the diagnostic
names the fix:

```text
error[E0277]: `fn(Db) -> impl Future {dashboard}` cannot serve as an `#[edge]` handler
   = note: edge handlers may use extractors that work for any state (e.g. `Path`,
           `Query`, `HeaderMap`, `EdgeCache`) and must not use native-only
           extractors (e.g. `Db`, `Session`, `Clock`, `Rng`)
   = note: remove `#[edge]` from this route, or replace the offending extractor
```

### What the macro refuses

`#[edge]` is a marker like `#[public]`: it injects no runtime guard and does not
rewrite your handler. It refuses, at compile time, every combination the edge
lane could not honour:

| You wrote | Why it is refused |
| --- | --- |
| `#[post]` / `#[put]` / `#[delete]` + `#[edge]` | the edge lane is read-path only; the origin is the authority for every write |
| `#[secured]` / `#[authorize]` / `#[step_up]` / `#[throttle]` + `#[edge]` | the capsule has no session, no auth state, and no shared rate counter |
| `#[intercept(...)]` + `#[edge]` | interceptor layers are origin-only tower middleware; the capsule mounts the bare handler, so the two lanes would serve different bytes |
| an `Extension<T>` parameter + `#[edge]` | the capsule installs no request extensions (`EdgeCache` is the one mediated seam); a missing extension would be served as a 500 instead of falling through |
| `#[static_get]` + `#[edge]` | the route is already pre-rendered and served CDN-side; a capsule adds nothing |
| `#[ws]` / `#[oauth2_callback]` + `#[edge]` | neither is a read-path GET |
| `#[edge(needs(db))]` | `kv` is the only capability the host mediates today |

Attribute order does not matter: `#[edge]` above or below the route macro
produces the same route.

## The one mediated seam: `EdgeCache`

An edge handler that could only render from its request would be a template
engine. The seam that makes the lane useful — and the one that makes *one*
handler source run on two substrates — is a key/value read:

```rust
#[get("/note/{key}")]
#[edge(needs(kv))]
pub async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
    match cache.get_string(&key) {
        Some(note) => format!("{key}: {note}"),
        None       => format!("{key}: (not cached at this replica)"),
    }
}
```

`EdgeCache` arrives through a request extension, so it works for any state type:

| Substrate | What is behind it | Installed by |
| --- | --- | --- |
| Origin | `CacheEdgeKv` over the app's own `Cache` | `AppBuilder::with_edge_kv(...)` (feature `edge`) |
| Edge | a dialogue-backed reader — each `get` is a round trip to the host | the capsule runtime, when the host provides `kv` |

```rust
// src/main.rs
autumn_web::app()
    .routes(routes![/* … */])
    .with_edge_kv(Arc::new(CacheEdgeKv::new(cache)) as Arc<dyn EdgeKv>)
    .run()
    .await;
```

### `EdgeKv` is not a database

It is a replica-local, opportunistic read accelerator in the sense of
[ADR-0004](../adr/0004-externalize-distributed-runtime-state.md) category 2, and
the trait's shape says so — one method, `get`, returning bytes:

- **Reads only.** There is no `put`. The origin owns every write.
- **A miss is always legal.** `None` is a normal answer, not an error. A correct
  handler renders something sensible on a miss, because a different edge replica
  may not have the key yet.
- **Staleness is expected.** No coherence protocol, no invalidation broadcast,
  no read-your-writes across replicas.
- **Never authoritative.** Anything a user's money, permissions or safety
  depends on is a read the origin must serve.

The origin publishes to the seam by doing what it already does: `CacheEdgeKv`
reads through the same `insert_cached` / `get_cached` serde path the rest of the
framework writes through, so caching bytes under a key *is* publishing them.

### The capability check runs before dispatch

`#[edge(needs(kv))]` travels with the route and is checked against the host's
`provided_capabilities` **before** the handler runs. A route whose seam this host
cannot mediate never executes a single line of handler code — it falls through
instead. Half-executing a handler and then discovering it needs a KV is how you
get a duplicated side effect.

## Failure is a fallthrough

Every way the edge can fail to answer lands in one channel, and the host
forwards the original request upstream:

| Reason | When | What the origin does |
| --- | --- | --- |
| `unknown_route` | no edge route matches the path | serves it, or 404s — its answer either way |
| `method_not_edge_eligible` | anything but `GET`/`HEAD` | runs the real handler |
| `missing_capability` | the route needs a seam this host did not provide | serves it, because the origin has the seam |
| `capsule_error` | a trap (a panic), a malformed frame, an unsupported wire version | its normal error handling — a panicking handler becomes the origin's 500 page, not a broken edge response |

None of these require author-written glue. A handler can also decline
explicitly by setting the `x-autumn-edge-fallthrough` response header to one of
those reasons; the runtime converts it and never lets the header — or that
response's body — reach the wire.

## Byte-identity: what is actually guaranteed

> **The claim.** For a request the edge serves, the capsule and the origin
> binary **of the same build** produce the same status, the same body bytes, and
> the same headers *after projection*.

Taken literally, "identical headers" is unachievable and, worse, untestable: the
origin stamps a `Date`, a request id and server-timing spans onto every
response, none of which the edge has any business inventing. So the guarantee is
stated precisely, in one constant both the tests and this page read from —
`autumn_edge::conformance::VOLATILE_HEADERS`:

```text
content-security-policy · date · server-timing · set-cookie · x-request-id
```

Those are dropped from both sides before comparison, names are lowercased and
canonically sorted, and everything else must match exactly. **A header your
handler set is compared value-for-value** — the projection only excuses the
headers the origin's middleware stack adds and the edge lane structurally cannot
emit.

The guarantee is scoped to one build. Two Autumn versions may render the same
handler differently (that is what a release is for); the promise is that *your*
origin and *your* capsule, built together, agree.

### Determinism is your half of the contract

Byte-identity only holds if the handler is a function of its request. The
framework removes the usual sources of drift — the `Clock` and `Rng` extractors
do not exist at the edge, and the *reference host* pins the WASI clock to zero
and seeds `random_get` deterministically — but a real edge host makes no such
promise, and a handler can still be non-deterministic:

- **`HashMap` / `HashSet` iteration order.** Rendering a map's entries in
  iteration order produces different bytes on different runs *and* different
  targets. Use `BTreeMap`, or sort before rendering. (`Query<Vec<(String,
  String)>>` rather than `Query<HashMap<..>>` also preserves repeated keys.)
- **Floats.** Formatting is deterministic in Rust and identical on both targets;
  *accumulating* floats in a different order is not. Fix the order.
- **`usize`.** Both targets in this slice are 32-bit-vs-64-bit different
  (`wasm32` has 32-bit pointers). Rendering a `usize` is fine; relying on
  `usize::MAX`, on a hash of a pointer, or on `size_of::<usize>()` is not.
- **Anything address-derived.** Pointer values, `{:p}`, and default `Hash`
  seeds differ per process.
- **Ambient time and randomness.** `SystemTime::now()`, `getrandom`, and
  anything built on them (`Utc::now()`, `Uuid::new_v4()`) compile at the edge —
  WASI provides both syscalls — and return whatever the host decides. The
  reference host pins them; a CDN host will not. Keep them out of `#[edge]`
  handlers.

The conformance suite runs each side twice and compares it with itself before
comparing lanes, so a handler that manages to be non-deterministic is reported
as that, not as a divergence.

### Known limitation: `paths::*` helpers

The typed path helpers the route macros generate (`paths::greet(…)`) live in the
native companion, which is compiled out for wasm. Inside a capsule, build links
by hand or with a `const`. This is a first-slice limitation, not a design
decision.

## The wire protocol (version 1)

Autumn ships an artifact and a protocol, not a vendor binding. The host and the
capsule talk NDJSON over the capsule's stdio: one JSON object per line,
`\n`-terminated, `op`-tagged. A CDN shim in any language can implement it from
this section alone.

```text
host → guest  {"op":"request","wire_version":1,"provided_capabilities":["kv"],
               "method":"GET","uri":"/x?y=z","headers":[["accept","text/html"]],"body_b64":""}
guest → host  {"op":"kv_get","key":"greeting"}
host → guest  {"op":"kv_value","value_b64":"aGk="}
guest → host  {"op":"response","status":200,"headers":[["content-type","text/plain"]],"body_b64":"aGk="}
guest → host  {"op":"fallthrough","reason":"unknown_route","detail":"no edge route matches /nope"}
```

The dialogue is request-scoped and half-duplex. The guest reads one `request`
frame, may interleave any number of `kv_get`/`kv_value` exchanges, and closes
the exchange with exactly one `response` **or** one `fallthrough`. The loop
repeats until stdin reaches EOF, so one process serves many requests.

- **Header names cross the wire lowercased and canonically sorted**, insertion
  order preserved within a repeated name.
- **Bodies are base64** so a binary response survives a line-oriented protocol.
- **`wire_version` mismatch is a `capsule_error` fallthrough.** A host and an
  artifact from different Autumn versions degrade to origin-serving rather than
  guessing.
- **Anything the capsule writes after its terminal frame is ignored.**

A host implementation you can read top to bottom lives in
`autumn-edge/src/host.rs` — the same one the conformance suite runs.

## Security posture

- **Credentials never reach a capsule.** `cookie`, `authorization` and
  `proxy-authorization` are stripped by the host before the request frame is
  sent, and stripped again by the guest on receipt — a defensive double-strip,
  because a capsule cannot audit the host it runs under.
- **No session, no auth, no CSRF, no database.** Not "discouraged": absent from
  the dependency graph. A route that needs any of them cannot compile as an
  `#[edge]` route.
- **No ambient authority.** A capsule imports only what the dialogue needs. The
  reference host provides `fd_read`/`fd_write` (stdio), `environ_*`,
  `random_get`, `proc_exit` and a few inert calls — and no `path_open`, no
  resolving `fd_prestat_*`, and no socket import. There is no filesystem and no
  network; the only way out is the dialogue. The conformance suite asserts the
  artifact's import list against that allowlist, so it stays true.
- **Bounded execution.** The reference host meters every request with a wasmi
  fuel budget (`autumn_edge::host::FUEL_BUDGET`, ~10⁹ instructions): a capsule
  stuck in a loop becomes a `capsule_error` fallthrough — the origin serves the
  request — instead of a hung host. Any production shim should impose its own
  wall-clock or instruction limit the same way.
- **Fallthrough details never leak.** A declining response's body is a message
  for a developer; it is not forwarded, and neither is the sentinel header.
- **No cookies from the edge.** A handler response carrying `set-cookie` is
  declined at the wire (a `capsule_error` fallthrough): cookies are session
  state, sessions are origin-only, and a cached edge response with a cookie
  would smear one client's state across everyone behind the CDN.
- **The artifact is a program, not an asset.** `autumn build` never copies the
  `.wasm` into `dist/` or `static/`, and you should not either: publishing it as
  a static asset would serve your capsule to browsers instead of running it at
  the edge.

The artifact does embed panic-location strings from its dependencies, which
include the build machine's Cargo paths. If a hermetic artifact matters to you,
build with `--config 'profile.release.trim-paths="all"'`.

## Building and shipping

### `autumn build`

```sh
autumn build            # release: native binary, then the capsule
autumn build --edge     # force the capsule step in a debug build
```

The edge step runs when the project has `#[edge]` routes and the build is a
release build (or `--edge` was passed). It is a second
`cargo build --target wasm32-wasip1 --release --bin edge-capsule` and it prints
what it produced:

```text
🍂 Edge capsule: 2 route(s) (greet, note) → target/wasm32-wasip1/release/edge-capsule.wasm (312 KB)
```

Detection is a source scan, not a compile: the CLI looks for `#[edge]`
attributes and `edge_routes![]` invocations under `src/`. A handler that is
marked but never registered is reported as a warning naming the function — the
one failure mode the type system cannot catch.

`--embed` is refused alongside edge routes in this slice, with an actionable
error rather than a silently skipped step.

### `autumn doctor`

| Check | Result | When |
| --- | --- | --- |
| `edge_target` | **Fail** | the project has `#[edge]` routes and `wasm32-wasip1` is not installed — hinting ``Run `rustup target add wasm32-wasip1` `` |
| `edge_routes` | **Fail** | an `#[edge]` handler also carries an auth/rate guard or `#[intercept]` (the build would fail too; doctor catches it first) |
| `edge_routes` | **Warn** | a handler is marked but never registered with `edge_routes![]`, or `src/bin/edge-capsule.rs` is missing |

`edge_routes` reports `handler @ file:line` for the handler at fault;
`edge_target` names the files that carry edge routes. Both pass with "no
`#[edge]` routes" on a project that has none.

### Deploying

What you deploy is the artifact plus this protocol. A CDN shim — a Worker, a
Lambda, an nginx sidecar — loads the `.wasm`, speaks the NDJSON dialogue on
stdio, answers `kv_get` from its own replica, and forwards any `fallthrough` to
your origin unchanged.

**Autumn does not ship that shim.** Vendor bindings are explicitly out of scope
for this slice (see [ADR-0011](../adr/0011-edge-capsule-read-lane.md)): the
deliverable is a portable artifact and a documented protocol, so no Autumn
release is coupled to a CDN vendor's SDK cadence. `autumn-edge`'s reference host
is the worked specification a shim implements against.

## Proving it, in your own app

The `edge-greeting` example ships the harness this framework uses on itself, and
it is copyable. It drives one request corpus through three lanes — the native
edge lane, a real wasm artifact, and the full origin app — and compares them:

```sh
cargo test -p edge-greeting --test conformance -- --ignored --test-threads=1 --nocapture
```

```text
  Tier A — native edge lane vs wasm capsule

    happy path                                           served 200 (11 bytes)
    kv hit                                               served 200 (53 bytes)
    percent-encoded slash inside a segment               served 200 (15 bytes)
    duplicate query keys                                 served 200 (49 bytes)
    trailing slash                                       Fallthrough(UnknownRoute)
    write method                                         Fallthrough(MethodNotEdgeEligible)
    kv route without the kv capability                   Fallthrough(MissingCapability)
    panicking handler                                    trap → Fallthrough(CapsuleError)

  Tier B — origin app vs wasm capsule

    happy path                                           both 200 (11 bytes)
    kv hit                                               both 200 (53 bytes)
    write method                                         edge method_not_edge_eligible → origin 200
    panicking handler                                    edge capsule_error → origin 500
```

CI runs it on every push in the `edge-conformance` job. It is not path-filtered:
byte-identity is a property of the whole framework, and a change to the router,
a middleware, a macro or a dependency is exactly what could break it.

## Limitations of the first slice

- **`GET`/`HEAD` only.** Writes stay at the origin, by design.
- **One capability: `kv`.** No database, no outbound HTTP, no storage.
- **No compression, no i18n locale prefix, no sessions** in the edge lane. The
  origin's middleware stack does not run there — the capsule serves exactly what
  the handler produced.
- **No CDN shim ships with Autumn.** Artifact plus protocol; see above.
- **`paths::*` helpers are unavailable inside a capsule.**
- **`autumn build --embed` refuses to combine with edge routes.**
- **The wire protocol and the host API are experimental.** They will change; the
  version field is there so a mismatch degrades safely instead of guessing.

## See also

- [`examples/edge-greeting`](../../examples/edge-greeting) — the worked example
  and its conformance suite
- [ADR-0011](../adr/0011-edge-capsule-read-lane.md) — why a separate crate, and
  what was rejected
- [ADR-0004](../adr/0004-externalize-distributed-runtime-state.md) — the
  category-2 framing `EdgeKv` inherits
- [Conditional GET](conditional-get.md) — and `#[static_get]`, for pages that
  can be pre-rendered outright, which is cheaper than any capsule
