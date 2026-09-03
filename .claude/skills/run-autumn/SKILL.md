---
name: run-autumn
description: >
  Build, launch, screenshot and drive a real Autumn app from this workspace.
  Use when asked to run, start, boot, serve, smoke-test or screenshot Autumn
  or one of its examples (todo-app, hello), to confirm a framework change
  actually works in a running server rather than only in tests, or to exercise
  Autumn's HTTP surface — routes, htmx fragments, /health, /actuator, the
  token-secured JSON API, background jobs, or the /mcp tool projection.
---

# Running Autumn

Autumn is a Rust web framework (crate `autumn-web`, in `autumn/`). It has no app
of its own — you exercise it by running one of the **examples** against it, which
compiles the framework from source. `examples/todo-app` is the reference app and
touches the most framework surface: Postgres + Diesel, embedded migrations, Maud +
Tailwind + htmx, a bearer-token JSON API, a tracked background job, and MCP.

Everything here is driven by one script:

```
.claude/skills/run-autumn/driver.sh
```

**All paths below are relative to the workspace root** (`/Users/mark/autumn`),
and every command was run there. Verified on macOS (darwin arm64), Chrome 152,
cargo 1.97.1, Docker 29.7.2.

## Prerequisites

- Rust toolchain (`cargo 1.97.1` here; workspace needs 1.88+, edition 2024).
- **Docker**, for the Postgres container. `hello` needs no Docker.
- `jq` and `curl` (both preinstalled on macOS).
- Google Chrome, for screenshots — only for `driver.sh shot`.

No `apt-get`/`brew` step was needed on this machine; nothing was missing.

## Run (agent path) — start here

Full lifecycle: Postgres container -> fresh DB -> Tailwind -> build -> launch ->
14 HTTP assertions -> screenshot. Leaves the app running.

```bash
./.claude/skills/run-autumn/driver.sh all
```

Expected tail (~60s warm, several minutes on a cold `target/`):

```
  ok   POST /api/todos created id=6 (5 -> 6)
  ok   POST /todos/6/toggle returned an htmx <li> fragment
  ok   CSV export job reached succeeded via /_autumn/jobs/5c6616e2...
  ok   POST /mcp tools/list projected list_json, create_json, scan_json
  ok   GET /_autumn/inspect (dev inspector)
  ok   wrote /Users/mark/autumn/target/autumn-run/todo-app-todos.png (45575 bytes)
==> ALL CHECKS PASSED — app still running at http://127.0.0.1:3100
```

Subcommands (each usable on its own once the app is up):

| Command | Does |
|---|---|
| `driver.sh all` | everything below, in order |
| `driver.sh db` | start/reuse the `autumn-run-pg` Postgres container on :55432 |
| `driver.sh reset` | drop + recreate the `todos` database (see Gotchas) |
| `driver.sh build` | `autumn setup` (Tailwind, once) then `cargo build -p <example>` |
| `driver.sh start` | launch the binary, poll `/health` until healthy |
| `driver.sh seed` | `autumn seed --package <example>` |
| `driver.sh smoke` | re-run just the assertions against a running app |
| `driver.sh shot /todos` | headless-Chrome PNG into `target/autumn-run/` |
| `driver.sh stop` | kill the app, leave Postgres up |
| `driver.sh down` | kill the app **and** remove the container |

Flags and env: `--example <name>` (default `todo-app`), `--port <n>` (default
`3100`), `PG_PORT` (default `55432`), `OUT` (default `target/autumn-run`),
`CHROME` (path to the Chrome binary).

The no-DB example is much faster and needs no Docker — use it to check that the
framework merely boots:

```bash
./.claude/skills/run-autumn/driver.sh all --example hello --port 3101
```

Both examples can run side by side — pidfiles, logs and screenshots are keyed
by example name, and `stop` only kills the one you name.

Screenshots and logs land in `target/autumn-run/` (`<example>.log`,
`<example>.pid`, `<example>-<path>.png`).

## Direct invocation — most framework PRs need only this

Changes under `autumn/src/` are usually confirmed by targeted tests, not by
booting an app. **The crate in `autumn/` is named `autumn-web`** (see Gotchas):

```bash
cargo test -q -p autumn-web --lib config::     # 577 passed, 4619 filtered out
cargo test -q -p todo-app                      # 28 + 6 passed, 2 ignored (Docker/browser)
```

Before pushing, run the compile-only mirror of CI's `lint` + `test` jobs — a
narrow `cargo test -p <pkg>` never links the consolidated `integration_tests`
binary and misses cross-package breaks:

```bash
./scripts/pre-push-check.sh
```

## Inspecting the running app

```bash
# Route table (needs --bin: todo-app has two binaries)
cargo run -q -p autumn-cli -- routes -p todo-app --bin todo-app --user-only

# Ops surface
curl -s http://127.0.0.1:3100/health | jq .
curl -s http://127.0.0.1:3100/actuator/info | jq .
curl -s http://127.0.0.1:3100/actuator/metrics | jq .

# The JSON API is bearer-token-gated; mint one from the open issuance route
TOK=$(curl -s -X POST http://127.0.0.1:3100/api/tokens \
      -H 'content-type: application/json' -d '{"principal_id":"user:42"}')
curl -s http://127.0.0.1:3100/api/todos -H "Authorization: Bearer $TOK" | jq .

# Same routes projected as MCP tools, guarded by the same token
curl -s -X POST http://127.0.0.1:3100/mcp -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq -r '.result.tools[].name'
```

## Run (human path)

Watch mode, rebuilding on change. Verified up in ~14s; Ctrl-C to stop:

```bash
cd examples/todo-app
AUTUMN_SERVER__PORT=3102 \
AUTUMN_DATABASE__URL='postgres://autumn:autumn@localhost:55432/todos' \
  cargo run -q -p autumn-cli -- dev
```

The README's `docker compose -f examples/todo-app/docker-compose.yml up -d` +
`cargo run -p todo-app` path also works, but binds :3000 and :5432 — see Gotchas.

## Gotchas

- **The crate in `autumn/` is `autumn-web`, not `autumn`.** `cargo test -p autumn`
  fails with ``package ID specification `autumn` did not match any packages``
  and unhelpfully suggests `axum`. Use `-p autumn-web`.
- **Don't run `autumn migrate` before starting the app.** The app auto-migrates on
  the `dev` profile, and its embedded `00000000000000_create_todos` collides with
  the framework's `00000000000000_create_api_tokens`; Autumn resolves this by
  applying it under a *substituted* version (`00000000000000+5c5f0205`). The CLI
  records it under the plain version, so the app then doesn't recognise its own
  migration as applied and dies with `relation "todos" already exists`. Hand the
  app a **virgin database** — that is all `driver.sh reset` does. (`autumn migrate`
  is still fine on a DB the app has never touched, and `migrate status` is
  read-only and always safe.)
- **Ports 3000 and 5432 are the defaults and are very likely taken** (they were
  here, by an unrelated stack). `autumn.toml` is beaten by the env-override layer,
  so `AUTUMN_SERVER__PORT` / `AUTUMN_DATABASE__URL` relocate both without editing
  a tracked file. The driver uses 3100 / 55432.
- **Tailwind fails silently.** Without `target/autumn/tailwindcss`, `build.rs`
  emits `cargo:warning=Tailwind CSS CLI not found` and produces **no CSS** — the
  build still succeeds and the page renders unstyled, so a screenshot looks
  broken for a reason that never appears as an error. Run `autumn setup` first.
  And because `build.rs` only reruns on `src/` or `input.css` changes, a binary
  built *before* `autumn setup` stays CSS-less until you `touch
  examples/todo-app/static/css/input.css`. The driver does both.
- **`autumn migrate` has no `--package`.** It reads `autumn.toml` from the current
  directory, so run it from `examples/todo-app/` (or set `AUTUMN_DATABASE__URL`,
  which makes it CWD-independent). `autumn seed` *does* take `--package`.
- **`autumn routes -p todo-app` fails** with `has multiple binary targets (seed,
  todo-app)`. Add `--bin todo-app`.
- **`/actuator/routes` does not exist** (404, HTML error page — it will break a
  `jq` pipe). The route table comes from the CLI, not the actuator.
- **`GET /` is a 303** to `/todos`, so assert with `curl -L` or expect 303.
- **`/api/*` returns 401 by default.** Mint a token at `POST /api/tokens` first.
  `/todos/summary` is deliberately mounted *outside* that scope and content-
  negotiates HTML vs JSON off `Accept`.
- **Headless Chrome writes the PNG and then does not exit.** Background it, wait
  for the file, kill it — a foreground call just hangs (it blew a 180s timeout
  here). `driver.sh shot` handles this.
- **`/opt/homebrew/bin/chromium` on this machine is a broken shim** pointing at a
  nonexistent `/Applications/Chromium.app`. Use
  `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`, which is the
  driver's default `CHROME`.
- **`autumn seed` is idempotent and silent about it**: `Database already has N
  todo(s); skipping seed`. It still exits 0, so seeding does not guarantee the
  data you expect. `driver.sh reset` before `seed` for a known state.
- **`.claude/` is gitignored here**, and a blanket `*.sh` rule ignores shell
  scripts. `.gitignore` now carries `!.claude/skills/` and
  `!.claude/skills/**/*.sh` so this skill and driver are tracked while
  `.claude/settings.local.json` stays ignored. Don't undo those negations.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Failed to run 00000000000000_create_todos with: relation "todos" already exists` | The DB was pre-migrated by the CLI. `driver.sh reset`, then `driver.sh start`. |
| `Bind for 0.0.0.0:5432 failed: port is already allocated` | Something else owns 5432. `PG_PORT=55433 driver.sh all`. |
| App never becomes healthy; log ends at `Autumn starting` | Postgres isn't reachable. `docker ps -f name=autumn-run-pg`, then `driver.sh db`. |
| `cargo:warning=Tailwind CSS CLI not found`, page renders unstyled | `cargo run -q -p autumn-cli -- setup`, then `touch examples/todo-app/static/css/input.css` and rebuild. |
| ``package ID specification `autumn` did not match any packages`` | Use `-p autumn-web`. |
| `package 'todo-app' has multiple binary targets (seed, todo-app)` | Add `--bin todo-app`. |
| `jq: parse error: Invalid numeric literal` on an actuator URL | You hit a 404 HTML error page — check the path exists (`/actuator/routes` doesn't). |
| `chromium: /Applications/Chromium.app/...: No such file or directory` | Broken Homebrew shim; use the Google Chrome binary (driver's default). |
| Screenshot command hangs forever | Headless Chrome doesn't self-exit. Use `driver.sh shot`. |
| Driver reports `unbound variable` | It runs under `set -u`; a `curl`/`jq` step returned empty. Check `target/autumn-run/<example>.log`. |
