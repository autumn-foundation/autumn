---
name: new
description: >
  Use when the user runs /autumn:new, asks to create a new Autumn web
  application, or wants to scaffold a fresh project with the autumn CLI.
argument-hint: "<app-name> [--starter <name>] [--with-i18n] [--with-seed] [--with <plugin>]"
allowed-tools:
  - Bash
  - Read
  - Write
---

# autumn:new

Create a new Autumn web application. This runs `autumn new`, then `autumn
setup`, and walks the user through the first-run configuration.

## Execution flow

1. Decide whether a **starter** fits. `autumn new <app-name>` alone scaffolds a
   minimal base project; a starter scaffolds a complete, runnable application
   archetype instead. If the user described the *kind* of app they are building
   (a SaaS, a blog or content site) rather than just asking for a new project,
   offer the matching starter before scaffolding the base — reaching a working
   domain-shaped app is usually what they wanted. See "Starters" below.
2. Confirm the app name, flags, and target directory:
   ```
   Will create: autumn new <app-name> [--starter <name>] [--with-i18n] [--with-seed] [--with <plugin>]
   Directory:   ./<app-name>/
   ```
   Pass any flags the user provided (e.g. `--with-i18n`, `--with-seed`) through
   to the command — they can only be applied at creation time, not added later.
   `--with <plugin>` is the exception: a plugin can also be added afterwards
   with `autumn plugin add`, and removed again with `autumn plugin remove`.
3. Ask for confirmation before proceeding.
4. Run with the user's flags:
   ```bash
   autumn new <app-name> [--starter <name>] [--with-i18n] [--with-seed] [--with <plugin>]
   ```
5. Change into the new directory and run:
   ```bash
   cd <app-name> && autumn setup
   ```
   `autumn setup` downloads the Tailwind CSS binary used during development.
6. Show the generated project structure.
7. Walk through first-run configuration (see below). A starter prints its own
   next steps on completion — follow those rather than the generic checklist
   when one was used, since a starter's schema and first screen differ.

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

## Starters

A starter scaffolds a complete application archetype instead of the minimal
base project. Both built-ins are the committed, CI-gated example apps of the
same name, so what the user gets is code that is known to run.

```bash
autumn new <app-name> --starter <name>
autumn new --list-starters          # what ships with this CLI
```

| Starter | Reach for it when | What lands |
|---|---|---|
| `saas` | The user is building a multi-tenant product — accounts, organisations, per-customer data | Session auth, row-level tenancy, a tenant-scoped dashboard |
| `cms` | The user is building a content site, a blog, or replacing WordPress | Posts/pages/custom post types, categories and tags, media library, moderated comments, revisions, roles and capabilities, menus, widgets, themes, plugin hooks, permalinks, feeds, sitemap, REST API |

Notes:

- One starter per project, chosen at creation time. `--starter` is rejected
  alongside `--with-i18n`, `--with-seed`, `--daemon`, `--bundled-pg` or
  `--api` — a starter brings its own composition — but it **does** compose with
  `--with <plugin>`, and each plugin name is resolved and version-checked
  before a byte is written.
- `cms` needs **PostgreSQL 12 or newer** (its full-text column is a stored
  generated column). Its first screen is `/register`, and the **first account
  created owns the site** — there are no default credentials, so tell the user
  to register before anything else.
- A community starter is any git repo or local directory following the same
  manifest format: `autumn new <app> --starter <owner/repo>[@ref]`. Provenance
  is printed and confirmed before anything is fetched; non-interactive use
  needs `--yes`. You are trusting that source's code — say so before running it.
- Full reference: `docs/guide/starters.md`.

## Flags

- `--starter <name>`: Scaffold an application archetype rather than the minimal
  base project. See "Starters" above.
- `--with-i18n`: Scaffold the optional i18n module (Fluent translations at
  `i18n/en.ftl`, the `[i18n]` block in `autumn.toml`, and the `i18n` feature
  on `autumn-web`).
- `--with-seed`: Scaffold a stub `src/bin/seed.rs` for database seeding.
- `--with <plugin>` **(trunk-dev, issue #1631)**: Scaffold the app with that
  plugin already wired — dependency plus builder-chain mount, the same edits
  `autumn plugin add` makes. Repeatable (`--with autumn-admin-plugin --with
  autumn-search`). Every name is resolved and version-checked before any file
  is written, so an unknown or incompatible plugin leaves no half-built
  project. Takes the same names as `autumn plugin list`. Wiring code is all it
  does: no migration is applied and no table is created.
- `--api` **(trunk-dev)**: Scaffold a JSON-first app instead of the HTML/view
  flavor (issue #1847). Handlers return `Json<…>`; `autumn-web` is pinned
  `default-features = false` to a lean set (`db`, `cache-moka`, `http-client`,
  `reporting`, `flash`), dropping the maud/htmx/tailwind view stack. No
  `static/`, `input.css`, `tailwind.config.js`, vendored assets, or Tailwind
  CI/README notes are generated, and the first `cargo run` serves JSON (no
  `autumn setup` Tailwind download needed). `--api` conflicts with `--daemon`
  and `--bundled-pg`, but composes with `--with-i18n` and `--with-seed`.

## Key files to know

| File | Purpose |
|---|---|
| `README.md` | Generated quickstart — prerequisites and the golden-path commands (configure the `[database]` block in `autumn.toml`, then `autumn migrate` → `autumn dev`) that take a clean checkout to a serving route, plus a CLI reference. Flag-aware: `--with-i18n` / `--with-seed` add sections for their extra steps. |
| `.env.example` | Documented template of local env vars. Copy it to `.env` (gitignored) and fill in local values (e.g. `AUTUMN_DATABASE__URL`); Autumn auto-loads `.env` in the dev/test profiles. Real shell env vars always win over `.env`. |
| `autumn.toml` | Base config (server, database, session, security, logging) |
| `autumn-dev.toml` | Dev profile overrides (auto-detected in debug builds) |
| `src/main.rs` | AppBuilder setup — register routes, tasks, jobs, migrations here |
| `migrations/` | Diesel migrations — one directory per migration |
| `static/` | Static assets served at `/static/` |
| `.autumn/scaffold.toml` | **Commit it.** Records which release's scaffold produced this project's framework-owned files (`Dockerfile`, `build.rs`, `autumn.toml`, the toolchain/style configs, the CI workflow) and a digest of each as Autumn wrote it, so a later `autumn upgrade` can tell a template that moved from a file the developer edited — and never overwrite their work. Machine-written; hand-editing it only costs conflict precision (issue #1593). |

## If autumn-cli is not installed

```bash
cargo install autumn-cli --version 0.5.0
```
