# Route Auth Coverage — the Default-Deny Posture Model

The most common web-security failure is not a broken guard — it's a *missing*
one: a handler someone forgot to annotate. Autumn's `#[secured]` and
`#[authorize]` guards are opt-in per handler, so nothing stops an unannotated
route from shipping silently, right up until it is discovered in production,
by an attacker, or never.

`autumn routes audit` closes that gap. It classifies **every** mounted route
at build time and fails the build when any route's exposure was never
declared. The claim it lets you make is simple and strong: **if it compiled,
every endpoint's exposure was declared on purpose.**

## The three route kinds

Every route Autumn can see falls into exactly one of three buckets. There is
no fourth, silent, default state — a route that fits none of these is
`unclassified`, and `unclassified` is what the audit gate fails on.

| Classification | Meaning | How it's declared |
|---|---|---|
| **`gated`** | Guarded by authentication and/or authorization | `#[secured]`, `#[secured("role")]`, `#[authorize]`, or a `#[repository(policy = ...)]` / `scope = ...` auto-API guard |
| **`public`** | Deliberately unauthenticated | `#[public]` |
| **`framework`** | Owned and pre-attributed by Autumn itself | Nothing — health probes, `/actuator/*`, static assets, htmx/OpenAPI/dev-reload routes are classified for you |
| *(none — the failure state)* | **`unclassified`** | No guard, no `#[public]`, not framework-owned |

### `gated` — guarded routes

Anything already carrying `#[secured]` or `#[authorize]` is `gated`
automatically; there is nothing new to add. See the [Authorization
guide](authorization.md) for `#[secured]` vs. `#[authorize]` vs. the `Policy`
trait. The manifest also carries the declared roles/scopes and a `policy`
boolean recording that a record-level check runs:

```rust
use autumn_web::prelude::*;

#[delete("/widgets/{id}")]
#[secured("admin")]
async fn delete_widget(/* … */) -> AutumnResult<()> { /* … */ }
```

```json
{ "path": "/widgets/{id}", "method": "DELETE", "classification": "gated", "roles": ["admin"] }
```

For `#[authorize]` the manifest goes one step further than the boolean: the
separate `authorization_policies` dimension records the `(action, resource)`
binding each attribute declares, so you can read *what* the route is guarded to
do rather than only *that* it is guarded.

```rust
#[get("/posts/{id}/edit")]
#[authorize("update", resource = Post)]
async fn edit_post(post: Post) -> AutumnResult<Markup> { /* … */ }
```

```json
{ "path": "/posts/{id}/edit", "method": "GET", "name": "edit_post", "action": "update", "resource": "Post", "provenance": "provable" }
```

The two are not redundant, and `policy` is the wider of the pair: a route can
set the boolean without contributing a binding — a `#[repository(api = ...,
policy = ...)]` auto-API does exactly that. Both readings, and that superset
relationship, are covered in the [Security Posture
Manifest](security-posture-manifest.md).

A `#[repository(api = ..., policy = ...)]` or `scope = ...` auto-API route is
also `gated` — its guard lives on the generated CRUD handler, not on a
`#[secured]` attribute, but the classifier knows to look there too.

### `public` — deliberately open routes

Use `#[public]` when a route has no guard *on purpose*. It injects no runtime
behavior — it is a compile-time marker that records intent, nothing more:

```rust
use autumn_web::prelude::*;

#[get("/health")]
#[public]
async fn health() -> &'static str {
    "ok"
}
```

Marking a route `#[public]` is exactly as effective at passing the audit gate
as guarding it with `#[secured]` — the point isn't authentication, it's that
*someone looked at this route and decided*.

### `framework` — pre-classified by Autumn

Routes the framework itself mounts (health/readiness/liveness probes,
`/actuator/*`, static assets, htmx JS assets, OpenAPI/Swagger UI, dev
live-reload, the mail preview UI, …) are pre-classified `framework` and never
need an explicit declaration — they aren't part of your application's
attack surface in the same sense, and the framework already knows what they
are.

### The failure state: `unclassified`

A route with no guard, no `#[public]`, and no framework attribution is
`unclassified`. This is the state the whole feature exists to catch:

```rust
// Compiles fine. Runs fine. Silently accepts unauthenticated requests
// forever, because nobody decided that on purpose.
#[post("/widgets")]
async fn create_widget(/* … */) -> AutumnResult<()> { /* … */ }
```

`autumn routes audit` fails the build and names it:

```
✗ 1 route(s) have no proven auth posture:
  POST   /widgets  (handler `create_widget` [myapp::widgets] at src/routes/widgets.rs:12)

Add a guard (`#[secured]` / `#[authorize]`) or mark the route deliberately open with `#[public]`.
```

Fix it either way — add a guard, or add `#[public]` — and the gate goes
green.

### Static routes only classify via `#[public]`

`#[static_get]` routes honor exactly one marker: `#[public]`. Stacking
`#[secured]` or `#[authorize]` on one still compiles — the guard expands and
runs on requests that fall through to the dynamic handler — but prerendered
responses are served from the static output *without invoking the handler*,
so no handler-body guard can be proven to cover every response the route
serves. The audit therefore deliberately refuses to call such a route
`gated`: it stays `unclassified`, fails the gate, and contributes no
`authorization_policies` binding. That is the honest outcome, not a gap — a
`gated` label here would claim a per-response guarantee the static serving
path does not provide. A route that needs authentication or authorization
should be a dynamic route (`#[get]`/`#[post]`/…); a static route that is
deliberately open says so with `#[public]`.

## Running the audit

```bash
# Human-readable summary + diagnostic on failure
autumn routes audit

# Write the machine-readable manifest alongside the summary
autumn routes audit --manifest security-manifest.json

# Emit the manifest to stdout instead of the summary (for piping/scripting)
autumn routes audit --json

# In a workspace, target a specific package/bin
autumn routes audit -p blog --bin server
```

Exit code is `0` when every route is classified, `1` otherwise — wire it into
CI as a hard gate exactly like `cargo clippy -- -D warnings`. `autumn new`
scaffolds this into every new project's `.github/workflows/ci.yml`
automatically, right after the CLI install step:

```yaml
- name: Route auth coverage (security manifest)
  run: autumn routes audit
```

Existing apps are unaffected until they add this step (or run the command
themselves) — nothing about `#[secured]`/`#[authorize]` runtime behavior
changes, and there is no breaking change to `autumn-web`'s published surface.

### Raw `merge()`/`nest()` routers can't be proven

`autumn routes audit` can only classify what it can enumerate. Routes
mounted via `AppBuilder::merge()`/`.nest()` with a raw `axum::Router` are
opaque — Axum exposes no API to list a router's routes — so their auth
posture is unprovable and the gate hard-fails rather than silently omitting
them:

```
✗ 1 raw router(s) added via `AppBuilder::merge()`/`nest()` are not enumerable
  and were omitted from the route listing.
Route auth coverage can't be proven while these exist. Mount routes via
`routes![]` (or a plugin's `declare_plugin_routes`) so they are visible and
classifiable.
```

Mount routes through `routes![]`, or — for a plugin nesting a raw router
under a prefix — `declare_plugin_routes` so the covering declarations make
the nest enumerable.

## Reading the manifest

The `routes` dimension of the emitted JSON manifest is `provenance:
"provable"` — every classification is a direct consequence of macro-expanded
code, not a report of configuration. So is `authorization_policies`, which
carries the `#[authorize]` bindings, with a `runtime_caveat` naming the one
step it cannot prove (which `Policy` impl the registry serves at boot). See
[Security Posture Manifest — Provenance Classes](security-posture-manifest.md)
for how the `routes`, `csrf`, `security_headers`, and `authorization_policies`
dimensions are tagged, for the rubric that decides a dimension's class, and for
what `declared` and `runtime-only` mean for the dimensions that aren't (yet)
provable this way.

## See also

- [The Security Posture Gate](posture-gate.md) — diffing this manifest across
  commits, blocking a pull request that widens the surface until a human
  acknowledges it, and verifying the signed manifest at deploy time.
- [`autumn routes` — Route Inspection CLI](routes-cli.md) — the base
  listing command `audit` builds on.
- [Security Posture Manifest — Provenance Classes](security-posture-manifest.md)
  — the manifest's provenance model in depth.
- [Authorization](authorization.md) — `#[secured]`, `#[authorize]`, and the
  `Policy` trait.
