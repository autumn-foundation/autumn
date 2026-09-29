---
name: routes
description: >
  Use when the user runs /autumn:routes, asks to list registered routes,
  inspect route handlers, check what endpoints exist, or audit an Autumn
  app's routing table.
argument-hint: "[-p <package>] [--bin <bin>] [--user-only] [--method GET|POST|...] [--filter <pattern>]"
allowed-tools:
  - Bash
  - Read
---

# autumn:routes

Run `autumn routes` to emit a machine-readable list of all registered routes
in the current Autumn project.

## Execution

Run from the project root (directory containing `autumn.toml`). Always use
JSON output. Include `--user-only` by default to hide framework internals,
but omit it when the user explicitly wants to see framework routes (actuator,
health probes, static assets, admin).

In **workspace** or **multi-binary** projects, pass `-p <package>` (and
optionally `--bin <bin>`) so `autumn routes` targets the correct binary;
omitting these in a workspace causes an error when multiple candidates exist.

```bash
# Single-binary project — user routes only
autumn routes --format json --user-only

# Workspace project — target a specific package
autumn routes --format json --user-only -p blog

# Multi-binary package — also specify the binary
autumn routes --format json --user-only -p api --bin server

# When user wants all routes (framework + user)
autumn routes --format json
```

If the user passes `--method` or `--filter`, append them to whichever form is appropriate:

```bash
autumn routes --format json --user-only --method POST --filter /posts
```

Capture stdout, stderr, and exit code.

## Output handling

Parse the JSON array and present a clean table grouped by handler file or
resource:

```
Routes (N total):

  GET    /posts                    routes::posts::list
  GET    /posts/{id}               routes::posts::show
  POST   /posts           [auth]   routes::posts::create
  PATCH  /posts/{id}      [auth]   routes::posts::update
  DELETE /posts/{id}      [auth]   routes::posts::delete_post
```

Mark secured routes with `[auth]` and admin-only with `[admin]` if that
information is present in the JSON.

## Auto-mounted routes

Autumn automatically mounts these — they appear when `--user-only` is
omitted. They do not need to be shown unless the user asks:

- `GET /health`, `GET /actuator/*`, `GET /live`, `GET /ready`, `GET /startup`
- `GET /static/js/htmx.min.js`
- Admin routes when `autumn-admin-plugin` is installed

## When the project has not been built

If `autumn routes` fails because the project has not been compiled, tell the
user to run `cargo build` first, then retry.

## Auth-coverage audit (unreleased — trunk-dev, issues #1604, #1850, #1627)

On trunk-dev, `autumn routes audit` audits every route's authentication
exposure. It prints each route's classification — `gated`, `public`,
`framework`, or `unclassified` — and emits a stable-ordered (by path, then
method) JSON security manifest. It exits non-zero on any `unclassified` (or
omitted) route, so it can gate CI. `autumn new` now wires this into every
scaffolded app's `.github/workflows/ci.yml` by default (right after the
a11y-verify step, reusing its installed CLI), so a fresh app fails CI on day
one if a route is left unclassified.

Mark a deliberately-unauthenticated handler with the new `#[public]` attribute
(mirrors `#[secured]`) to classify it as `public` and clear it from the
`unclassified` set.

An unclassified-route diagnostic now names the offending handler's `file:line`
(from `file!()`/`line!()`) alongside its module, e.g. `POST /widgets (handler
`create_widget` [myapp::widgets] at src/routes/widgets.rs:12)`, so it can be
jumped to directly. See `docs/guide/route-auth-coverage.md` for the full
default-deny posture model and how to classify `gated`/`public`/`framework`
routes.

The manifest (schema v3) carries four dimensions, each tagged with a provenance
class:

- `routes` (`provable`) — the per-route classification above.
- `csrf` (`declared`) — CSRF enforcement per mutating route, from config.
- `security_headers` (`declared`) — effective response headers, from config.
- `authorization_policies` (`provable`) — one `(action, resource)` entry per
  `#[authorize]` binding, plus a `runtime_caveat` recording that which
  `impl Policy<R>` serves the check is a boot fact the build cannot see.

Dimensions that are not yet emitted are named in the manifest's `excluded`
list with the class they will eventually carry. See
`docs/guide/security-posture-manifest.md` for the provenance rubric that
decides a dimension's class.

## Posture diffs and the merge gate (unreleased — trunk-dev, issue #1624)

`autumn routes audit` says what the surface *is*. `autumn routes posture` says
what a change *did to it*:

```bash
# The pull-request gate. Exit 0 = clean or acknowledged, 1 = blocked,
# 2 = the tool could not run.
autumn routes posture diff --base base-posture.json --head security-posture.json

# The digest a release records, and the deploy-time proof.
autumn routes posture digest --manifest security-posture.json
autumn routes posture verify --manifest security-posture.json \
  --expect-digest <digest> --repo owner/repo
```

Only *widening* blocks: a new public or unclassified route, a classification
downgraded, a role requirement dropped, a role added (roles are OR-ed, so one
more admits more callers), a scope removed (scopes are AND-ed), an
`#[authorize]` binding or policy check removed, CSRF enforcement lost, or a
security header no longer emitted. Narrowing and neutral changes annotate and
never block, a security header's *value* changing is reported but never
blocked, and a handler rename or a moved file produces no finding at all.

A widening is acknowledged with one pull-request comment carrying the digest
the report prints — `/ack-posture <digest> <reason>` — which stays valid across
unrelated pushes and stops matching the moment something new widens. That is
also the only escape hatch for a false positive: nothing disables the gate.

`autumn new` scaffolds `.github/workflows/posture-gate.yml`; existing apps
adopt it with `autumn upgrade --apply`. See `docs/guide/posture-gate.md`.

## Comparing expected vs actual routes

When the user is debugging a 404, compare the route table with the requested
path. Common mismatches:
- Parameter syntax: routes use `{id}`, not `:id`
- Missing registration in `main.rs` `routes![...]`
- Method mismatch (POST handler hit with GET)
