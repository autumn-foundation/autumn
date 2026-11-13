# Autumn Hello Example

The simplest possible Autumn application. Three routes, no database, no
configuration file — a clean baseline for understanding the Autumn macro and
builder API before adding framework features.

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| `#[get]` | `src/main.rs` | Declares GET route handlers with proc-macro ergonomics |
| `routes![]` | `src/main.rs` | Collects handlers into a type-safe route set |
| `#[autumn_web::main]` | `src/main.rs` | Bootstraps the Tokio runtime and the Autumn app |
| Built-in `/health` | automatic | Framework mounts a health endpoint with no extra code |

## Prerequisites

- Rust 1.97.0+

No database or external services required.

## Quick start

From the **workspace root** (`autumn/`):

```bash
cargo run -p hello
```

The server starts on `http://localhost:3000`.

### Prove it works

```bash
curl http://localhost:3000/
# => Welcome to Autumn!

curl http://localhost:3000/hello
# => Hello, Autumn!

curl http://localhost:3000/hello/world
# => Hello, world!

curl http://localhost:3000/health
# => {"status":"UP"}
```

## Available routes

| Method | Path | Response |
|--------|------|----------|
| GET | `/` | `Welcome to Autumn!` |
| GET | `/hello` | `Hello, Autumn!` |
| GET | `/hello/{name}` | `Hello, <name>!` |
| GET | `/health` | `{"status":"UP"}` |
| GET | `/actuator/health` | Extended health JSON |
| GET | `/actuator/info` | Build and runtime metadata |
