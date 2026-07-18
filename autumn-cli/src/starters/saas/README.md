# {{project_name}} — multi-tenant SaaS starter

A complete, runnable multi-tenant SaaS application built from Autumn's shipped
primitives: session-based authentication, row-level multi-tenancy, and
tenant-scoped repositories. Sign up an organisation, log in, and land on a
dashboard that only ever shows your own organisation's data.

## How it works

| Piece | Primitive |
|-------|-----------|
| Signup / login / logout | `Session` + `hash_password`/`verify_password` (bcrypt) |
| Tenant per organisation | `tenant_id` stored in the session at signup/login |
| Row-level isolation | `#[repository(Project, tenant_scoped)]` + `with_tenant` |
| Server-rendered UI | Maud templates + htmx + Tailwind |

A user's `tenant_id` is derived from their organisation name at signup and
stored in the session. The dashboard reads it back and runs every query inside
`with_tenant`, so the `tenant_scoped` `ProjectRepository` filters all reads and
stamps all inserts with the current tenant — enforced in SQL.

## Prerequisites

- Rust 1.88.0+
- PostgreSQL (a `docker-compose.yml` is included for local development)

## Quick start

```bash
docker compose up -d        # start Postgres
autumn migrate              # create the users + projects tables
autumn dev                  # run the app at http://localhost:3000
```

Then open <http://localhost:3000>, sign up an organisation, and you are taken to
a tenant-scoped dashboard. Create a project; a second organisation that signs up
will never see it.

### Success check

```bash
# After signing up in the browser, the dashboard serves 200 OK:
curl -i http://localhost:3000/dashboard --cookie "autumn.sid=<your-session>"
```

## Tests

```bash
cargo test                                          # smoke tests (no Docker)
cargo test -- --include-ignored --test-threads=1    # full flow (needs Docker)
```

## Where to look

- `src/routes/auth.rs` — signup, login, logout (sessions + bcrypt)
- `src/routes/dashboard.rs` — the tenant-scoped dashboard (`with_tenant`)
- `src/repositories.rs` — the `tenant_scoped` repository
- `src/models.rs` / `migrations/` — the `users` and `projects` tables
