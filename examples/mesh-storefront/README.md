# `mesh-storefront` — build-checked service contracts (caller half)

The caller half of Autumn's wire-contract example (issue #1755). Its callee is
[`examples/mesh-catalog`](../mesh-catalog). Together they show two Autumn
services in one workspace sharing a contract the compiler checks.

## Prerequisites

Rust 1.88.0+. No database, no config file.

## Quick start

```bash
cargo build -p mesh-storefront      # the contract check runs here
AUTUMN_SERVER__PORT=3001 cargo run -p mesh-catalog   # terminal 1
CATALOG_URL=http://127.0.0.1:3001 cargo run -p mesh-storefront   # terminal 2
curl http://127.0.0.1:3000/items/42
```

## What to look at

`mesh-catalog` marks two handlers with `#[endpoint(service = "catalog")]`. That
reads the request and response types off each signature and emits the contract.

`mesh-storefront` never writes an HTTP call. `wire_client!` generates
`CatalogClient` from the catalog's own endpoint markers, so every method's types
are the catalog's real types. `#[contract_checked]` then holds each call site to
what the catalog actually produces and accepts.

## Seed a breaking change

Add a required field to the catalog's request type:

```diff
 pub struct NewItem {
     pub name: String,
     pub price_cents: u32,
+    /// New, and required.
+    pub sku: String,
     #[serde(default)]
     pub note: Option<String>,
 }
```

`cargo build -p mesh-storefront` now fails, at the call site:

```
error: wire contract broken in `add_item` at `create_item(…)`: omits request
       field `sku`, which endpoint `catalog.create_item` (POST /items) requires
  --> examples/mesh-storefront/src/main.rs:53:10
```

Nothing else changed. The storefront still type-checks — it builds its request
with `..Default::default()`, so `sku` would have gone out empty and the catalog
would have rejected it in production. Undo the diff and the build is green
again.

## Prove it across a mutation set

```bash
python3 examples/mesh-storefront/contract-sweep.py
```

The script applies each seeded mutation to `mesh-catalog`, rebuilds, and reports
how many wire-breaking changes turned the build red and how many compatible
changes were falsely rejected. It restores the file when it finishes.

## Further reading

`docs/guide/wire-contracts.md`.
