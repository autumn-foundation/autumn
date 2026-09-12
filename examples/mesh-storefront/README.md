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

Stop the catalog serializing a response field the storefront reads:

```diff
 pub struct Item {
     pub id: String,
+    #[serde(skip_serializing)]
     pub name: String,
     pub price_cents: u32,
 }
```

`cargo build -p mesh-storefront` now fails, at the call site:

```
error[E0080]: evaluation panicked: wire contract broken in `show_item` at
       `get_item(…)`: reads response field `name`, which endpoint
       `catalog.get_item` (GET /items/{id}) no longer produces
  --> examples/mesh-storefront/src/main.rs:32:24
```

Nothing else changed, and the storefront still type-checks: the Rust field is
still there, so `item.name` compiles — the value simply never arrives. Undo the
diff and the build is green again.

The same holds on the request side. Delete the `request_id` line from
`add_item`'s `NewItem { … }` and the build fails naming that field: the catalog
requires it, and drops it from the body when it is empty, so a `..rest`
initializer would have sent a request the catalog rejects.

What is deliberately *not* an error: adding an ordinary required field to
`NewItem`. Both services share the type, so the storefront's
`..Default::default()` sends a default value for it and the body is complete.

## Prove it across a mutation set

```bash
python3 scripts/wire-contract-sweep.py
```

The script applies each seeded mutation to `mesh-catalog`, rebuilds, and reports
how many wire-breaking changes turned the build red and how many compatible
changes were falsely rejected. It restores the file when it finishes.

## Further reading

`docs/guide/wire-contracts.md`.
