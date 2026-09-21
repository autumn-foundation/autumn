# Chapter 3: Database Setup

**Goal:** By the end of this chapter, you will have a Postgres database running
in Docker, the Diesel CLI installed, a `todos` table created via migration,
and your Autumn app connecting to the database on startup.

---

## Sections

### Starting Postgres with Docker Compose

Creating `docker-compose.yml` for a local Postgres instance. Starting and
verifying the database.

### Installing the Diesel CLI

`cargo install diesel_cli --no-default-features --features postgres` and
verifying the installation.

### Configuring the Database Connection

Uncommenting the `[database]` section in `autumn.toml` and setting the
primary/write connection URL. How Autumn's config system keeps legacy
single-URL apps valid while also supporting explicit primary/replica topology.

### Creating Your First Migration

`diesel setup` and `diesel migration generate create_todos`. Writing the
`up.sql` (CREATE TABLE) and `down.sql` (DROP TABLE) scripts.

### Running Migrations

`diesel migration run` to apply the migration. Verifying the table exists.

### The `schema.rs` File

How `diesel print-schema` generates the Diesel schema module. Understanding
the `diesel::table!` macro output. Why `schema.rs` is generated, not
hand-written.

### Verifying the Connection

Starting the app with `cargo run` and confirming the "Database pool
configured" log message.

### Checkpoint

Expected project state with database configured and migration applied.

---

> **Not written yet.** This chapter's narrative doesn't exist yet. For the
> actual Postgres setup and `todos` table this chapter's goal describes, see
> [`examples/todo-app/docker-compose.yml`](../../../examples/todo-app/docker-compose.yml)
> and
> [`examples/todo-app/migrations/00000000000000_create_todos/`](../../../examples/todo-app/migrations/00000000000000_create_todos/)
> — that's the reference implementation this tutorial builds toward. The
> Getting Started guide's
> ["Add a database"](../getting-started.md#add-a-database) section gets you
> to the same end state (a configured `autumn.toml`, Postgres running, the
> `todos` table migrated) — but via a different path than this chapter's
> stated goal: a bare `docker run` rather than Docker Compose, and Autumn's
> own `autumn generate migration` / `autumn migrate` rather than typing
> `diesel migration generate` / `diesel migration run` yourself. You still
> need the Diesel CLI installed either way — `autumn migrate` shells out to
> `diesel migration run` under the hood (see that section's Prerequisites).
> Either path works; adapt if you want this chapter's exact tools. Continue to
> [Chapter 4 — Models and Queries](04-models.md) once your database is up.

---

Previous: [Chapter 2 — Routes and Handlers](02-routes.md) | Next: [Chapter 4 — Models and Queries](04-models.md)
