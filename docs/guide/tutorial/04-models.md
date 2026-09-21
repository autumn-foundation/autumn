# Chapter 4: Models and Queries

**Goal:** By the end of this chapter, you will have `Todo` and `NewTodo` model
structs, working CRUD operations (create, list, get-by-ID), and a `/todos`
page that displays todos from the database.

---

## Sections

### Defining the `Todo` Model

Creating `src/models.rs` with the `Todo` struct. Diesel derives:
`Queryable`, `Selectable`, `Serialize`. Mapping struct fields to database
columns with `#[diesel(table_name = todos)]`.

### The `NewTodo` Insertable

A separate struct for inserts: `Insertable` and `Deserialize`. Why Diesel
uses different types for reading vs. writing.

### The `Db` Extractor

How `Db` works: it pulls an async Postgres connection from the pool managed
by `AppState`. Using `&mut *db` to get a mutable reference for queries.

### Listing Todos

Writing the list query: `todos::table.order(...).select(...).load(...)`.
Wiring it into the `/todos` GET handler.

### Creating a Todo

Handling form submissions with `Form<NewTodo>`. Inserting into the database
with `diesel::insert_into`. Redirecting back to the list after creation.

### Fetching a Single Todo

Adding `GET /todos/{id}` with `.find()` and `.first()`. Using
`AutumnError::not_found` for missing records.

### Adding Dependencies

Updating `Cargo.toml` with `diesel`, `diesel-async`, `chrono`, and `serde`.

### Checkpoint

Expected project state with models, queries, and database-backed routes.

---

> **Not written yet.** This chapter's narrative doesn't exist yet. The same
> ground — the same todo app, the same API — is already covered and tested in
> the Getting Started guide's
> ["Define a model"](../getting-started.md#define-a-model) and
> ["Query the database"](../getting-started.md#query-the-database) sections.
> Read those, then continue to
> [Chapter 5 — HTML Templates with Maud](05-templates.md). You can also check
> the finished code in [`examples/todo-app/`](../../../examples/todo-app/).

---

Previous: [Chapter 3 — Database Setup](03-database.md) | Next: [Chapter 5 — HTML Templates with Maud](05-templates.md)
