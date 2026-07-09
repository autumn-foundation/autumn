---
name: new
description: >
  Use when the user runs /autumn:new, asks to create a new Autumn web
  application, or wants to scaffold a fresh project with the autumn CLI.
argument-hint: "<app-name> [--with-i18n] [--with-seed]"
allowed-tools:
  - Bash
  - Read
  - Write
---

# autumn:new

Create a new Autumn web application. This runs `autumn new`, then `autumn
setup`, and walks the user through the first-run configuration.

## Execution flow

1. Confirm the app name, flags, and target directory:
   ```
   Will create: autumn new <app-name> [--with-i18n] [--with-seed]
   Directory:   ./<app-name>/
   ```
   Pass any flags the user provided (e.g. `--with-i18n`, `--with-seed`) through
   to the command — they can only be applied at creation time, not added later.
2. Ask for confirmation before proceeding.
3. Run with the user's flags:
   ```bash
   autumn new <app-name> [--with-i18n] [--with-seed]
   ```
4. Change into the new directory and run:
   ```bash
   cd <app-name> && autumn setup
   ```
   `autumn setup` downloads the Tailwind CSS binary used during development.
5. Show the generated project structure.
6. Walk through first-run configuration (see below).

## First-run configuration checklist

Present this as an ordered list after the project is created:

```
First-run checklist:

1. Set your database URL in autumn.toml (or via env var):
   [database]
   url = "postgres://localhost:5432/<app-name>_dev"

   Or: export AUTUMN_DATABASE__PRIMARY_URL="postgres://..."

2. Run the initial migration:
   autumn migrate

3. Start the dev server:
   autumn dev
   → App available at http://localhost:3000
   → Health check: http://localhost:3000/health

4. (Production) Set the signing secret before deploying:
   export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"

5. Run autumn doctor --strict before your first deploy.
```

## Flags

- `--with-i18n`: Scaffold the optional i18n module (Fluent translations at
  `i18n/en.ftl`, the `[i18n]` block in `autumn.toml`, and the `i18n` feature
  on `autumn-web`).
- `--with-seed`: Scaffold a stub `src/bin/seed.rs` for database seeding.

## Key files to know

| File | Purpose |
|---|---|
| `README.md` | Generated quickstart — prerequisites and the golden-path commands (configure the `[database]` block in `autumn.toml`, then `autumn migrate` → `autumn dev`) that take a clean checkout to a serving route, plus a CLI reference. Flag-aware: `--with-i18n` / `--with-seed` add sections for their extra steps. |
| `autumn.toml` | Base config (server, database, session, security, logging) |
| `autumn-dev.toml` | Dev profile overrides (auto-detected in debug builds) |
| `src/main.rs` | AppBuilder setup — register routes, tasks, jobs, migrations here |
| `migrations/` | Diesel migrations — one directory per migration |
| `static/` | Static assets served at `/static/` |

## If autumn-cli is not installed

```bash
cargo install autumn-cli --version 0.5.0
```
