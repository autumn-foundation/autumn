# `mesh-catalog` — build-checked service contracts (callee half)

The callee half of Autumn's wire-contract example (issue #1755). Two handlers,
each marked `#[endpoint(service = "catalog")]`, which emits the contract its
caller is compiled against.

The story, the seeded breaking change and the mutation sweep live with the
caller: [`examples/mesh-storefront`](../mesh-storefront).

## Prerequisites

Rust 1.88.0+. No database, no config file.

## Quick start

```bash
AUTUMN_SERVER__PORT=3001 cargo run -p mesh-catalog
curl http://127.0.0.1:3001/items/42
```

The port override keeps it clear of the storefront's default 3000.
