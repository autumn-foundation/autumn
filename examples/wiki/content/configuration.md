+++
title = "Configuration"
description = "Configure the Autumn Wiki via autumn.toml and environment variables."
order = 2
+++

# Configuration

The wiki is configured via `autumn.toml`, profile overrides, and environment
variables. All settings have sensible defaults so the app runs with zero
configuration in development.

## Database

Set the Postgres connection URL:

```toml
[database]
url = "postgres://localhost/wiki_dev"
```

Or use the environment variable:

```bash
DATABASE_URL=postgres://localhost/wiki_dev cargo run -p wiki
```

## Server

```toml
[server]
port = 3000        # default
host = "0.0.0.0"  # default
```

## Logging

```toml
[log]
level = "info"  # trace | debug | info | warn | error
```

## Environment Profiles

Autumn merges configuration files in this order:

| File | When loaded |
|------|-------------|
| `autumn.toml` | Always |
| `autumn-dev.toml` | Only in development mode |

The `autumn-dev.toml` in this example overrides the database URL to point at
a local Docker instance.

## Static Site Generation

Pre-render all `#[static_get]` routes (including these docs pages) to `dist/`:

```bash
cargo run -p autumn-cli -- build -p wiki
```

The generated files are written to `dist/` and can be served as static assets
or baked into a container image.

## Health Endpoints

| Path | Description |
|------|-------------|
| `/health` | Liveness probe (200 OK when the app is up) |
| `/actuator/health` | Detailed health view with pool stats |
| `/actuator/info` | Build and runtime metadata |
| `/actuator/metrics` | Request counts and pool metrics |
