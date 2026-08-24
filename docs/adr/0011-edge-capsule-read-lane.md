# ADR 0011: A Separate `autumn-edge` Crate for the Edge Capsule Read Lane

- Status: Accepted
- Date: 2026-08-19
- Deciders: Autumn maintainers
- Tags: edge, wasm, wasi, portability, caching, conformance

## Context

An Autumn app is a native Tokio/Axum binary at a single origin. Issue #1790 asks
for a first slice of something different: opt-in read-path routes compiled into
a portable `wasm32-wasi` artifact a CDN can run, with the origin binary as
authority and fallback. Its acceptance criteria are demanding in a specific way
— they ask for *one codebase* (AC-1), *proven* byte-identity (AC-2), *glue-free*
fallthrough (AC-3), a *framework-mediated* platform seam (AC-4), and *actionable*
build-time refusal (AC-5).

The obvious implementation is to make `autumn-web` itself compile for wasm.
Its dependency reality says otherwise:

- `tokio { features = ["full"] }`, `axum` with default features (hyper), and
  `tower-http` are **unconditional** dependencies.
- The default feature set includes `db`, which pulls `diesel`, `pq-sys` and
  `libsqlite3-sys` — C code that cannot target wasm at all.
- There is no `target_arch` cfg anywhere in `autumn/src` today. Introducing one
  means every one of ~136 modules becomes a place where a wasm build can break.

Meanwhile the acceptance criteria do *not* need most of `autumn-web`. What an
edge route needs is a router, a handful of extractors, a response type, and one
mediated read. Everything else — sessions, auth, CSRF, flash, the database, jobs,
mail, i18n — is precisely what an edge route may not touch.

Two other facts shaped the decision. First, `axum` with `default-features =
false` is wasm-compatible and keeps its `matchit` router, which is the
Cloudflare Workers pattern. Second, `wasmi` is a pure-Rust WebAssembly
interpreter available as an ordinary dev-dependency — so "prove it in CI"
requires no runtime installation and no vendor account.

## Decision

Autumn adds a **new workspace crate, `autumn-edge`**, which is the only Autumn
crate that compiles for both the host target and `wasm32-wasip1`. `autumn-web`
is never compiled for wasm.

The pieces:

1. **`autumn-edge`** carries the edge lane: `EdgeRoute`/`EdgeState`, the
   `EdgeKv` seam and its `EdgeCache` extractor, the `EdgeHandler` bound, the
   router builder, the NDJSON wire protocol, the guest runtime (`serve` /
   `serve_io`), a shared conformance projection, and — behind a native-only
   `host` feature — a `wasmi`-based reference host. Its dependencies are
   `axum` (default features **off**), `tower`, `http`, `futures`, `serde`,
   `serde_json`, `base64`, and the proc-macro crate.

2. **`autumn-macros` gains `#[edge]` / `edge_routes![]`.** `#[edge]` is a marker
   attribute in the `#[public]` mould, detected by the route macro through the
   established attribute-or-marker duality. For an edge-marked route the macro
   emits the usual native companion **`#[cfg(not(target_arch = "wasm32"))]`-
   gated**, plus an ungated `__autumn_edge_route_*` companion returning an
   `EdgeRoute`. Non-edge expansion is byte-identical to what it was before.

3. **`autumn-web` gains a non-default, host-only `edge` feature** providing
   `pub use autumn_edge as edge`, the `CacheEdgeKv` adapter, and
   `AppBuilder::with_edge_kv`. It adds no target-conditional dependency and
   changes nothing about the default build.

4. **The app opts in per module.** An edge-safe module holds only `#[edge]` GET
   routes and imports from `autumn_edge`; origin-only code sits behind
   `#[cfg(not(target_arch = "wasm32"))]`. The manifest splits the same way, with
   `autumn-web` under a `cfg(not(target_arch = "wasm32"))` target section.

5. **`autumn build` emits both artifacts** from one invocation, driven by a
   source scan for `#[edge]`, and `autumn doctor` gains `edge_target` and
   `edge_routes` checks.

### The seam: `EdgeKv` as an ADR-0004 category 2 accelerator

The mediated platform seam (AC-4) is a **new, narrow trait** — one required
method, `get(&str) -> Option<Vec<u8>>` — rather than a widening of
`autumn_web::cache::Cache`. `Cache` is a type-erased, `Arc<dyn Any>`-shaped,
seven-method trait with fill locks and TTLs; none of that can cross a WASI
boundary, and none of it is needed to read bytes.

`EdgeKv` is explicitly framed as an
[ADR-0004](0004-externalize-distributed-runtime-state.md) **category 2**
component: a replica-local, opportunistic, non-authoritative read accelerator.
No `put`; a miss is always legal; staleness is expected; never a source of
truth. That framing is what makes a per-replica store compliant with Autumn's
distributed-state policy — it is the same argument the fragment cache makes, and
it is why an edge route that *requires* its value to be present does not belong
in the lane.

### Fallthrough is one channel, and it is the host's job

Everything the edge cannot answer — unknown route, non-GET method, an
unprovided capability, a trap, a version mismatch — becomes a single
`fallthrough` frame with a typed reason, and the host forwards the original
request upstream. The origin still mounts every edge route, so there is nothing
for the author to wire (AC-3). Autumn ships **no reverse proxy and no origin
fetch**: forwarding is the host's responsibility, which is exactly the
responsibility a CDN already has.

### Byte-identity is defined, then proven

AC-2's "byte-identical headers" is not literally achievable — the origin stamps
`Date`, `x-request-id` and server-timing spans that the edge lane structurally
cannot emit. This ADR ratifies a **precise** guarantee instead, encoded in one
constant (`conformance::VOLATILE_HEADERS`) that the guide, the tests and the
runtime all read:

> Status and body bytes are compared exactly. Headers are compared after
> projection: the volatile set and the internal fallthrough sentinel are
> dropped, names are lowercased, and the rest is canonically sorted. A header a
> handler set is compared value-for-value.

and it is scoped to **one build**: the origin binary and the edge artifact
produced from the same source tree agree. Nothing is promised across versions —
a release is allowed to change what a handler renders, as long as it changes
both lanes together.

The proof is the `edge-greeting` fixture's conformance suite, which drives one
request corpus through the native edge lane, a real `wasm32-wasip1` artifact
loaded into `wasmi`, and the full origin app, and runs in a dedicated,
unfiltered CI job.

## Design Details

- **Parity by construction, not by re-implementation.** The edge router is built
  from the same `axum`/`matchit` and the same path patterns as the origin, and
  the edge companion mounts the *same* handler value the native companion mounts
  (including the primitive-output wrapper). There is no second matcher and no
  second response path that could drift.
- **The capability check runs before dispatch.** A route whose declared `needs`
  the host did not provide falls through without executing a line of handler
  code, so a partially-executed handler can never produce a duplicated effect.
- **The type system carries the refusal (AC-5).** `autumn build` shells out to
  cargo, so the compiler is the enforcement point. `EdgeHandler` is a sealed
  blanket bound over `axum::handler::Handler<T, EdgeState>` carrying a
  `#[diagnostic::on_unimplemented]` message that names the fix. The macro adds
  the combinations a bound cannot express: non-GET, auth/rate guards,
  `#[static_get]`.
- **Credentials never reach a capsule.** `cookie`, `authorization` and
  `proxy-authorization` are stripped by the host before sending and again by the
  guest on receipt.
- **The sandbox is the import list.** The reference host implements the smallest
  WASI surface a capsule needs and no `path_open`, no resolving `fd_prestat_*`
  and no socket import; the conformance suite asserts the artifact's imports
  against that allowlist.

## Consequences

### Positive

- `autumn-web` is untouched by wasm concerns: no `target_arch` cfg enters its
  ~136 modules, and no future contributor can break a wasm build they cannot
  see.
- One handler source really does serve both substrates, and the fixture proves
  it against a real artifact rather than against a mock.
- The deliverable is a portable artifact plus a documented protocol, so no
  Autumn release is coupled to a CDN vendor's SDK cadence.
- A capsule's dependency graph contains no tokio, no hyper, no `autumn-web` —
  asserted in CI on the resolved graph, not just in a manifest comment.
- The conformance projection gives the framework a reusable definition of
  "reproduced", with a `Verdict` shaped like the failure-replay driver's.

### Negative

- **Two crates to keep aligned.** `autumn-edge` and `autumn-web` must agree on
  `axum` and `matchit` versions; a mismatch would be a silent routing
  divergence. Both resolve to one `axum 0.8.x` in `Cargo.lock`, and `matchit` is
  already exact-pinned.
- **An edge-safe module is a real constraint on layout.** Authors must split
  handlers by substrate, and a plain `#[get]` in the wrong module fails the wasm
  build with an error about `::autumn_web` rather than about the rule it broke.
- **`paths::*` helpers are unavailable inside a capsule** — they live in the
  cfg-gated native companion.
- **`autumn-macros` emits `::autumn_edge::…` paths** for edge routes, so an app
  that marks a route must depend on `autumn-edge`. The macro crate itself does
  not.
- **The `edge` feature is linted through workspace feature unification**, not a
  dedicated `-p autumn-web --features` lane: `examples/edge-greeting` (a
  workspace member) enables it, so the lint job's `cargo clippy --workspace`
  compiles `edge_support.rs`'s panic-gate deny block. The module carries the
  deny block without a `check-panic-gate.sh` manifest entry because that
  script's feature-reachability check cannot see unification (details in the
  module's header comment).

### Risks

- A capsule that is byte-identical today can stop being so through a dependency
  bump nobody associates with the edge lane. Mitigated by making the CI job
  unfiltered — it runs on every push, not only on `examples/` changes.
- The wire protocol is a public interface the moment a shim exists. Mitigated by
  a version field whose mismatch is a fallthrough, and by marking the protocol
  experimental.
- Non-determinism in an author's handler (`HashMap` iteration, accumulated
  floats) reads as a conformance failure. Mitigated by running each side twice
  and reporting self-inequality as "handler nondeterministic", not as a
  divergence.

## Alternatives Considered

### 1. cfg-gate `autumn-web` itself for `wasm32`

Rejected. It requires moving every native-only dependency into a
`cfg(not(target_arch = "wasm32"))` section and gating the module tree in
`lib.rs`, which turns ~136 modules into surfaces where a wasm build can silently
rot — a permanent tax paid by every contributor, most of whom will never build
for wasm and none of whom would see the breakage locally. It also makes the
minimal edge surface an *emergent* property of a hundred cfg decisions rather
than a stated one. A separate crate makes "what can run at the edge" a fact you
can read in one manifest.

### 2. A generated shim crate per app

Rejected for this slice. Generating a second crate from the app's route table
would remove the edge-safe-module rule, but it needs a stable component ABI to
call handlers across the boundary and a code generator that has to reproduce
extractor semantics. Deferred to the reactor-ABI slice, where an exported
function replaces the stdio loop; the wire protocol is versioned so that
migration is a protocol bump rather than a rewrite.

### 3. A `wasi:http` / WASI Preview 2 component

Rejected *for now*, and named as the migration target. Preview 2's
`wasi:http/incoming-handler` is the right long-term shape — it removes the
hand-rolled dialogue entirely — but component-model tooling (`wasm-tools`,
`cargo-component`, `wasmtime` as a host) is a heavier dependency than a
pure-Rust interpreter, and the CDN runtimes this slice targets still speak
Preview 1. The NDJSON protocol is deliberately boring so it can be retired.

### 4. Prerender + KV hybrid (no wasm at all)

Rejected: it fails AC-4 and hollows out AC-1. Pre-rendering pages into a KV the
CDN reads is genuinely cheaper for pages that *can* be pre-rendered — and Autumn
already offers it as `#[static_get]`, which this ADR keeps recommending first.
But it cannot run a handler at the edge, so a route whose answer depends on the
request (a path parameter, a query, a header) is out of reach, and there is no
mediated seam to speak of — only a pre-populated blob store.

### 5. Widen `autumn_web::cache::Cache` to be the edge seam

Rejected. `Cache` is `Arc<dyn Any>`-shaped with fill locks and TTLs; none of it
crosses a WASI boundary. Widening it would also make every existing `Cache`
implementor responsible for edge semantics it does not have. `EdgeKv` is the
narrowest trait that lets the same handler source run on both substrates, and
`CacheEdgeKv` adapts one to the other in a dozen lines.

### 6. Ship a CDN shim (Worker/Lambda binding) in-tree

Rejected for this slice. A vendor binding would couple Autumn's release cadence
to a CDN SDK and would have to be maintained per vendor. The reference host in
`autumn-edge` is a worked specification instead: a shim author has one file to
read and a protocol to implement.

## Non-Goals

- Write-path routes at the edge. The origin is the authority for every write.
- Non-WASI runtimes, vendor bindings, or client-side islands (see
  `docs/guide/wasm-islands.md` for the unrelated Yew island pattern).
- Automatic data replication into the edge KV. The origin publishes by caching;
  distribution is the platform's job.
- Compression, sessions, i18n locale prefixes or any other middleware behaviour
  at the edge. The capsule serves exactly what the handler produced.

## Follow-On Work

- A reactor-ABI slice: export a function instead of running a stdio loop, so a
  host can call a warm instance per request.
- A second capability (an outbound HTTP fetch, or a signed-URL mint) to prove
  the capability model generalises beyond `kv`.
- Add `edge` to a CI clippy lane and to `scripts/check-panic-gate.sh`'s
  manifest, so `edge_support.rs`'s deny block is enforced by a named job.
- A `wasi:http` component target once the CDN runtimes Autumn users deploy to
  speak Preview 2.
- Origin-side observability for fallthroughs: a counter per reason would tell an
  operator whether the edge lane is actually earning its keep.
