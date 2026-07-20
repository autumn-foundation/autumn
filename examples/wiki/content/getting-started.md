+++
title = "Getting Started"
description = "Set up and run the Autumn Wiki example from scratch."
order = 1
+++

# Getting Started

This guide walks you through setting up the Autumn Wiki example, which
demonstrates Autumn's mutation hooks, generated repositories, and Markdown
documentation pages.

## Prerequisites

- Rust 1.88.0 or later
- PostgreSQL (or Docker for a quick start)

## Installation

Clone the repository and navigate to the wiki example:

```bash
git clone https://github.com/autumn-foundation/autumn
cd autumn
```

## Running Locally

Start PostgreSQL, then launch the app:

```bash
# Download Tailwind CSS (first time only)
cargo run -p autumn-cli -- setup

# Start Postgres
docker compose -f examples/wiki/docker-compose.yml up -d

# Run the wiki
cargo run -p wiki
```

Open <http://localhost:3000> in your browser.

## Project Layout

| Path | Purpose |
|------|---------|
| `src/main.rs` | App entry point |
| `src/models.rs` | `Page` and `Revision` models |
| `src/repositories.rs` | Generated repository + API routes |
| `src/hooks.rs` | `PageHooks`: slug generation, revision auditing |
| `src/routes/pages.rs` | HTML templates and route handlers |
| `src/routes/docs.rs` | Markdown docs routes (this feature!) |
| `content/` | Embedded Markdown documentation |
| `migrations/` | Diesel database migrations |

## Next Steps

- Read the [Configuration](/docs/configuration) guide.
- Explore the JSON API at `/api/v1/pages`.
- Check the `/actuator/health` endpoint for runtime status.
