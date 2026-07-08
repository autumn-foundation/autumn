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
   and watch the startup logs.

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

## Hot reload behavior

`autumn dev` watches `src/`, `templates/`, and `static/` for changes and
recompiles + restarts automatically. Tailwind CSS is rebuilt on template
changes.

## Common failures

| Symptom | Fix |
|---|---|
| `Error: Address already in use` | Port 3000 is taken. Set `AUTUMN_SERVER__PORT=3001 autumn dev` or kill the existing process. |
| `Error: connection refused` | Database is not running. Start Postgres first. |
| Compile error shown in terminal | Fix the Rust error; `autumn dev` will retry on next save. |
| `autumn setup` not found | Run `cargo install autumn-cli --version 0.5.0` |

## Stopping

`Ctrl+C` stops the server and the Tailwind watcher.
