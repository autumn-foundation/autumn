---
name: dev
description: >
  Use when the user runs /autumn:dev, asks to start the Autumn development
  server, enable hot reload, or check what's running locally.
argument-hint: "[--package <name>] [--show-config]"
allowed-tools:
  - Bash
  - Read
---

# autumn:dev

Start the Autumn development server with hot reload.

## Pre-flight checks

Before running `autumn dev`, verify:

1. **Database URL is configured** — check `autumn.toml` or the env var
   `AUTUMN_DATABASE__PRIMARY_URL` is set. `autumn dev` will fail at startup
   if no database URL is present. Note: `autumn migrate check` analyzes
   migration SQL files only and does NOT test connectivity — to verify the
   database is reachable, attempt `autumn migrate status` or start the server
   and watch the startup logs. `autumn dev` now auto-loads a project-root
   `.env` file (dev/test profiles) into the `AUTUMN_*` env layer before boot,
   so you can set `AUTUMN_DATABASE__URL` there (copy `.env.example` to `.env`);
   real shell env vars still win, and a malformed `.env` fails loudly.

2. **Tailwind binary is present** — if `autumn setup` has not been run:
   ```bash
   autumn setup
   ```

## Execution

```bash
autumn dev
```

For workspace projects, specify the package:
```bash
autumn dev --package my-app
```

To log all registered routes, tasks, middleware, and config at startup:
```bash
autumn dev --show-config
```

`autumn dev` uses the `dev` profile automatically in debug builds
(`AUTUMN_ENV=dev` is the default).

On trunk-dev (unreleased — not in the published 0.5.0 CLI) there is also
`autumn serve` for a non-watch local daemon: `autumn serve --daemon` /
`stop` / `status` / `restart`, `--release`, and `--bundled-pg` for a managed
local Postgres. See `docs/guide/daemon.md`. Do not suggest it to users on the
published 0.5.0 CLI.

**Do not suggest the daemon to a user on native Windows** (unreleased —
trunk-dev, issue #1616). The daemon lifecycle is built on Unix domain sockets
and POSIX signals, so it is **Tier 2: supported via WSL2** and fails fast on
native Windows with a message naming the policy. `--bundled-pg` implies
`--daemon`, so it is Tier 2 too. On Windows, `autumn dev` is the native way to
run a managed-Postgres app; foreground `autumn serve` is also Tier 1. See
`docs/guide/platform-support.md`.

## What gets served

Once running, tell the user what's available:

| Endpoint | Purpose |
|---|---|
| `http://localhost:3000` | Application root |
| `http://localhost:3000/health` | Simple health check |
| `http://localhost:3000/actuator/health` | Detailed health (JSON) |
| `http://localhost:3000/actuator/tasks` | Scheduled task status |
| `http://localhost:3000/actuator/jobs` | Background job status |
| `http://localhost:3000/static/js/htmx.min.js` | Bundled htmx |

If `autumn-admin-plugin` is installed:
| `http://localhost:3000/admin` | Admin dashboard |

If the `mail` feature is enabled:
| `http://localhost:3000/_autumn/mail` | Mailer preview (dev profile only) |

## Dependency findings on startup (unreleased — trunk-dev, issue #1633)

`autumn dev` reports dependency-policy findings once per run, and only the ones
that turn CI red — findings the app's `deny.toml` **denies**:

```
  ⚠️  1 blocking dependency finding (worst: high) — run `autumn doctor` for detail.
```

A critical advisory gets a startup banner naming the ids instead. Everything
else is silent by design: a clean tree, a fully waived tree, a tree with only
warn-level findings (duplicate or yanked crates), and every state where the
policy could not be evaluated (no cargo-deny, no advisory database, no
`deny.toml`) all print nothing. `autumn doctor` is where those are reported.

Do not read silence as "no audit ran". The audit starts after the initial
build — running it alongside makes its `cargo metadata` contend with Cargo's
package-cache lock — and the watch loop polls for the result without ever
blocking, so nothing here delays startup or a rebuild. The line can therefore
appear a second or two after the server does.

## Hot reload behavior

`autumn dev` watches `src/`, `templates/`, and `static/` for changes and
recompiles + restarts automatically. Tailwind CSS is rebuilt on template
changes.

On trunk-dev (unreleased — not in the published 0.5.0 CLI), when a rebuild
**fails** the browser renders the compiler diagnostics as a full-screen overlay
(under a strict nonce-based CSP) instead of leaving a blank or stale page; it
clears on the next successful rebuild (issue #1115). See
`docs/guide/dev-error-overlay.md`.

### On Windows (unreleased — trunk-dev, issue #1616)

Two behaviours differ, both deliberate:

- **The app is stopped before the rebuild, not after.** A running
  `target\debug\<app>.exe` is locked on Windows, so `cargo build` cannot relink
  over it. The tradeoff is that a failed rebuild leaves the app down, so the
  compile-error overlay above is not available there and the browser falls back
  to a normal reconnect.
- **The stop is cooperative, not a kill.** Windows has no `SIGTERM`, so
  `autumn dev` sets `AUTUMN_SHUTDOWN_SIGNAL_FILE` and creates that file to
  request a drain; the runtime then runs the same graceful shutdown a signal
  triggers on Unix, so `on_shutdown` hooks — including managed Postgres teardown
  — actually run. The wait is the app's own configured budget
  (`prestop_grace_secs + shutdown_timeout_secs`) plus headroom for the hooks
  that run after the drain, not a fixed constant. If the app misses it,
  `autumn dev` force-stops it and prints a warning saying the hooks may not have
  run. If a user reports that warning, the app is genuinely hanging in shutdown;
  it is not a false alarm. The variable is honored on non-Unix targets only.

## Common failures

| Symptom | Fix |
|---|---|
| `Error: Address already in use` | Port 3000 is taken. Set `AUTUMN_SERVER__PORT=3001 autumn dev` or kill the existing process. |
| `Error: connection refused` | Database is not running. Start Postgres first. |
| Compile error shown in terminal | Fix the Rust error; `autumn dev` will retry on next save. |
| `autumn setup` not found | Run `cargo install autumn-cli --version 0.5.0` |
| On Windows: `autumn serve --daemon` / `deploy` refuses with "Tier 2 (WSL2)" | Working as designed (trunk-dev, issue #1616) — those are Unix-native. Run them from a WSL2 shell; see `docs/guide/platform-support.md`. |

## Stopping

`Ctrl+C` stops the server and the Tailwind watcher.
