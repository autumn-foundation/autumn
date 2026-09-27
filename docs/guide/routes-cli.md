# `autumn routes` — Route Inspection CLI

`autumn routes` prints every mounted route — method, path, handler name,
registration source, and active middleware — without starting the HTTP server
or connecting to a database.

## Quick start

```bash
# In a single-crate project
autumn routes

# In a Cargo workspace
autumn routes -p blog
```

Output (table format, sorted by path then method):

```
METHOD    PATH                        HANDLER                  SOURCE
DELETE    /admin/{id}                 delete_post              user
GET       /                           list_posts               user
GET       /about                      about                    user
GET       /actuator/health            actuator                 framework
GET       /actuator/info              actuator                 framework
GET       /actuator/metrics           actuator                 framework
GET       /admin                      admin_posts              user
GET       /admin/new                  new_post_form            user
GET       /admin/{id}/edit            edit_post_form           user
GET       /api/posts                  api_list_posts           user
GET       /backoffice/posts           backoffice_list_posts    plugin:autumn-admin
GET       /health                     health                   framework
GET       /posts/{slug}               show_post                user
POST      /admin                      create_post              user
POST      /api/posts                  api_create_post          user
```

## Options

| Flag | Description |
|------|-------------|
| `-p, --package <PKG>` | Workspace package to inspect |
| `[PREFIX]` | Show only routes whose path starts with PREFIX (positional shorthand for `--filter`) |
| `--filter <FILTER>` | Show only routes whose path starts with FILTER |
| `--method <METHOD,...>` | Restrict to one or more HTTP methods (comma-separated, e.g. `GET,POST`) |
| `--user-only` | Hide framework-internal routes (`/actuator/*`, probes, htmx assets) |
| `--format <FORMAT>` | Output format: `table` (default) or `json` |

## Filtering routes

### By path prefix

Positional shorthand:

```bash
autumn routes /api
```

Equivalent long form:

```bash
autumn routes --filter /api
```

### By HTTP method

```bash
autumn routes --method GET
autumn routes --method GET,POST
```

Filters are composable — prefix and method can be combined:

```bash
autumn routes /admin --method POST
```

### User routes only

Hide all framework-managed routes (health probes, actuator, htmx assets,
OpenAPI docs, dev live-reload):

```bash
autumn routes --user-only
```

`--user-only` keeps plugin routes (source `plugin:<name>`); it only strips
`framework` routes.

## JSON output

Emit machine-readable JSON for scripting, CI, or tooling integration:

```bash
autumn routes --format json
```

Each entry in the JSON array follows this schema:

```json
{
  "method": "GET",
  "path": "/api/posts",
  "handler": "api_list_posts",
  "source": "user",
  "middleware": []
}
```

`source` is one of:

| Value | Meaning |
|-------|---------|
| `"user"` | Registered directly by the application |
| `"plugin:<name>"` | Registered by a named Autumn plugin (e.g. `"plugin:autumn-admin"`) |
| `"framework"` | Registered by the Autumn framework itself |

## WebSocket routes

Routes registered with `#[ws]` appear with method `WS`:

```
WS    /ws/chat    chat_handler    user
```

## CI snapshot recipe

Capture a baseline and diff it on every pull request to catch unintended
route changes:

```bash
# Record a baseline (commit this file)
autumn routes --format json > routes-snapshot.json

# In CI — diff against the committed snapshot
autumn routes --format json > routes-current.json
diff routes-snapshot.json routes-current.json
```

Because rows are stable-sorted by path then method, `git diff` and `diff`
output is minimal and easy to review.

## How it works

`autumn routes` compiles your application in debug mode, then runs the
resulting binary with `AUTUMN_DUMP_ROUTES=1`. The binary detects this
variable during `AppBuilder::run()`, collects all registered routes (user,
plugin, and framework), serialises them to JSON, writes to stdout, and
exits 0 — without binding a TCP port, connecting to a database, or
running any startup hooks.

This means:

- **No running server required** — safe to run in CI or pre-deploy scripts.
- **No database needed** — routes are inspected before any pool is created.
- **Reflects real config** — framework route paths (e.g. custom actuator
  prefix) are read from `autumn.toml`, so the output mirrors production.
- **Plugin routes attributed** — routes registered by plugins show
  `source = "plugin:<name>"` rather than `"user"`.

## Known limitations

Routes added via `.merge(router)` or `.nest(prefix, router)` with a raw
`axum::Router` are **not included** in the listing. Axum provides no public API
to enumerate a router's registered routes. `autumn routes` prints a warning to
stderr when such routers are detected:

```
[autumn routes] warning: 1 raw router(s) added via .merge()/.nest() are not
enumerable and are omitted from this listing
```

If your application relies heavily on merged/nested raw routers, use `autumn
routes` output as a partial snapshot and supplement it with manual
documentation for those routes.

## See also

- [Route Auth Coverage — the Default-Deny Posture Model](route-auth-coverage.md)
  — the `audit` subcommand: default-deny classification, the three route
  kinds (`gated`, `public`, `framework`), and CI wiring.
- [Security Posture Manifest — Provenance Classes](security-posture-manifest.md)
  — how `autumn routes audit` tags each manifest dimension as `provable`,
  `declared`, or `runtime-only`.
