# Edge Capsule First Slice Implementation Plan

> **Status: executed.** This records the slice as it landed (issue #1790), not
> as it was imagined. Where the plan and the code diverged, the code won and the
> divergence is written down. The narrative for users is
> `docs/guide/edge.md`; the design record is
> `docs/adr/0011-edge-capsule-read-lane.md`.

**Goal:** Opt-in read-path routes compile into a portable `wasm32-wasip1` edge
capsule from one `autumn build`, out of the same source that serves them at the
origin — with byte-identity proven in CI, fallthrough that needs no author glue,
one framework-mediated platform seam, and build-time refusal with an actionable
message.

**Architecture:** A new workspace crate, `autumn-edge`, is the only Autumn crate
that compiles for both the host and `wasm32-wasip1`. `autumn-web` is never
compiled for wasm; it gains a non-default, host-only `edge` feature for the
origin half of the seam. `autumn-macros` gains `#[edge]` / `edge_routes![]`,
emitting a wasm-gated native companion plus an ungated edge companion. Apps
split by module: an edge-safe module holds only `#[edge]` GET routes, everything
else sits behind `cfg(not(target_arch = "wasm32"))`. Host and capsule talk
NDJSON over stdio.

**Tech Stack:** Rust / Cargo, `axum` with default features off (keeping
`matchit`), `wasmi` (pure-Rust interpreter) for the reference host and the
conformance harness, `wasm32-wasip1`.

**Acceptance criteria, and where each is discharged:**

| AC | Discharged by |
|----|---------------|
| AC-1 one codebase, one `autumn build` | `#[edge]` + `edge_routes![]` + the CLI's edge step |
| AC-2 byte-identical, proven in CI | `examples/edge-greeting/tests/conformance.rs` + the `edge-conformance` job |
| AC-3 transparent fallthrough, no glue | one `fallthrough` frame with a typed reason; the origin already mounts every edge route |
| AC-4 a framework-mediated seam | `EdgeKv` / `EdgeCache` / `with_edge_kv` / `CacheEdgeKv` |
| AC-5 actionable build-time refusal | the `EdgeHandler` bound's `on_unimplemented`, the macro's refusals, `autumn doctor` |

---

### Phase 1: `autumn-edge` (the crate that compiles both ways)

**Files:** `autumn-edge/{Cargo.toml,README.md}`, `autumn-edge/src/*.rs`,
`autumn-edge/tests/runtime_io.rs`, root `Cargo.toml` members.

- `route.rs` — `EdgeRoute`, `EdgeState`, `EdgeCapability`.
- `kv.rs` — the `EdgeKv` seam plus `InMemoryEdgeKv` (a `BTreeMap`, for
  reproducibility) and `EmptyEdgeKv`.
- `extract.rs` — `EdgeCache`, extracted from a request extension so it works for
  any state type, with a rejection that carries the fallthrough sentinel.
- `handler.rs` — the sealed `EdgeHandler` bound carrying the
  `#[diagnostic::on_unimplemented]` message, and `edge_get`.
- `router.rs` — `build_edge_router` (fallback sets the sentinel) and
  `CapabilityProbe` (a real axum router, so no second matcher can disagree).
- `wire.rs` — protocol version 1: frames, base64 bodies, header
  canonicalisation, credential stripping, the sentinel.
- `runtime.rs` — `serve` / `serve_io`: parse → version → method → capability →
  dispatch, each rung producing a fallthrough rather than a partial answer.
- `conformance.rs` — `VOLATILE_HEADERS`, `project_headers`, `compare`,
  `Verdict`, `ConformanceCase`.
- `host.rs` (feature `host`) — `wasmi` plus a hand-written, deliberately tiny
  WASI shim with a fixed-seed `random_get` and a clock frozen at 0.

**Executed as planned**, with these decisions recorded in the code:
`serve_io` is public and returns `io::Result<()>` (a `-> !` signature fights
testing); `EdgeCache::layer` hands back a ready-made `axum::Extension` so the
injection type stays private; `axum` is pinned directly rather than through
`[workspace.dependencies]`, because a `workspace = true` inherit is
feature-additive and cannot turn default features back off.

### Phase 2: `autumn-macros` — `#[edge]` and `edge_routes![]`

**Files:** `autumn-macros/src/{edge.rs,route.rs,static_route.rs,lib.rs}`,
`autumn/tests/compile-fail/edge_*.rs`.

- `#[edge]` / `#[edge(needs(kv))]` as a marker attribute, with the
  attribute-or-marker duality `#[public]`/`#[secured]` established (expansion
  order is not guaranteed, and an undetected opt-in would silently drop a route
  from the capsule).
- Route-macro refusals: non-GET, `#[secured]`/`#[authorize]`/`#[step_up]`/
  `#[throttle]`, `#[static_get]`, `#[ws]`, `#[oauth2_callback]`.
- For an edge route: `#[cfg(not(target_arch = "wasm32"))]` on the native
  companion, the path helper and the alias; an ungated
  `__autumn_edge_route_{fn}()` companion mounting the *same* handler value
  (including the primitive-output wrapper).
- Non-edge expansion stays byte-identical — asserted by a codegen test.

**Constraint honoured:** `autumn-macros` gained no dependency on `autumn-edge`;
it emits `::autumn_edge::…` paths as text.

### Phase 3: `autumn-web`'s `edge` feature

**Files:** `autumn/Cargo.toml`, `autumn/src/{lib.rs,app.rs,edge_support.rs}`,
`autumn/tests/integration/edge_native.rs`, `deny.toml`.

- `edge = ["dep:autumn-edge"]`, non-default and host-only.
- `pub use autumn_edge as edge`, `CacheEdgeKv`, and
  `AppBuilder::with_edge_kv(Arc<dyn EdgeKv>)` — one line over
  `EdgeCache::layer`.
- `#[edge]` / `edge_routes!` are re-exported **unconditionally**: a route can be
  marked without the feature; the feature is what wires the seam at the origin.

### Phase 4: `autumn build` / `autumn doctor`

**Files:** `autumn-cli/src/{build.rs,doctor.rs,edge_scan.rs,main.rs}`,
`autumn-cli/tests/integration/edge.rs`, `skills/doctor/SKILL.md`.

- A syn-based source scan (no compile) finds `#[edge]` attributes and
  `edge_routes![]` invocations; documented limits in the module docs.
- The edge step runs after the native build and before the static renderer, for
  a release build or `--edge`; `--embed` plus edge routes is refused with an
  actionable message; the artifact is never copied into `dist/` or `static/`.
- Preflight for the target with a `rustup target add wasm32-wasip1` hint.
- Doctor checks `edge_target` and `edge_routes`, both silent on projects with no
  edge routes.

### Phase 5: the fixture, the conformance suite, CI and the docs

**Files:** `examples/edge-greeting/**`, `.github/workflows/ci.yml`,
`EXAMPLES.md`, `scripts/check-examples.sh`, `docs/guide/edge.md`,
`docs/adr/0011-edge-capsule-read-lane.md`, `CHANGELOG.md`, `STABILITY.md`,
`README.md`, `CLAUDE.md`, root `Cargo.toml`.

- `examples/edge-greeting` — five `#[edge]` routes chosen as divergence classes
  (path capture, the KV seam, repeated query keys and a float, a primitive
  return, a panic) plus one origin-only `POST`.
- `tests/conformance.rs` — an isolated `[[test]]` target whose every test is
  `#[ignore]`d. It builds a real capsule with the same command the CLI runs,
  into its own target directory (the outer `cargo test` holds the workspace
  build lock), then drives a 14-case corpus through three lanes.
- The `edge-conformance` CI job: `wasm32-wasip1` toolchain target, a build step,
  a resolved-graph assertion that no tokio/hyper/mio/`autumn-web` reaches the
  capsule, then the suite. Not path-filtered.

**Divergences from the plan, recorded:**

1. **Tier A drives `serve_io`, not `build_edge_router` + `tower::oneshot`.** The
   plan called for the bare router; the suite runs the guest loop instead, which
   is the same code the capsule's `main` runs and also covers the decision
   ladder — so `method_not_edge_eligible` and `missing_capability` are compared
   across lanes rather than assumed. It also keeps `axum`/`tower`/`http` out of
   the example's dev-dependencies.
2. **The fixture is cataloged `tier=experimental`, and the catalog gate learned
   the tier.** `scripts/check-examples.sh` previously required every workspace
   `examples/*` member to be `supported`, which would have enrolled a
   wasm-conformance fixture in the Chromium fleet e2e gate. The gate now accepts
   `supported` or `experimental` for a member (an uncataloged or `excluded`
   member is still a failure), and `EXAMPLES.md` documents the three-way
   distinction.
3. **No "artifact contains no absolute path" assertion.** The artifact does
   embed dependency panic-location strings, which carry the build machine's
   Cargo paths. Asserting their absence would have meant changing the build
   command away from the one `autumn build` runs. The guide documents
   `trim-paths` instead.
4. **The origin's declined-request bodies are not byte-stable**, because RFC
   9457 problem documents embed the request id. The suite asserts the canonical
   *status* for those cases, and full byte-equality only where the edge served.

---

## What this slice deliberately did not do

- Write-path routes at the edge.
- A CDN shim, a vendor binding, or an origin-fetch reverse proxy.
- A second capability beyond `kv`.
- A `wasi:http` / Preview 2 component (named as the migration target in
  ADR-0011).
- Any change to `Route`, `ApiDoc`, the prelude, or middleware ordering.
