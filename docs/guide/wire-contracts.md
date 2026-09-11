# Wire contracts

Two Autumn services in one Cargo workspace share one compiler-checked contract:
the callee's handler signatures. A breaking change to a request or response
fails the caller's build, at the call site, naming the field — instead of
failing a customer's request.

There is no IDL, no codegen step, and nothing to keep in sync by hand.

## The shape of it

| Where | What | Does |
|---|---|---|
| callee | `#[derive(WireShape)]` | records a DTO's serde-visible field shape |
| callee | `#[endpoint(service = "…")]` | marks a handler and emits its contract |
| caller | `wire_client!` | generates the typed client |
| caller | `#[contract_checked]` | fails the build when a call site disagrees |

## Callee

```rust
use autumn_web::prelude::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub price_cents: u32,
}

#[endpoint(service = "catalog")]
#[get("/items/{id}")]
#[public]
pub async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> {
    // …
}
```

`#[endpoint]` reads the handler's own signature: the `Json<T>` parameter is the
request shape, the `Json<T>` in the return type is the response shape, and the
route attribute supplies the method and path. It emits a marker type —
`get_item_endpoint` — and writes the endpoint's JSON descriptor.

**`#[endpoint]` must sit above the route attribute.** A route attribute placed
outermost expands first and rewrites the signature, leaving nothing to read.

## Caller

```rust
use autumn_web::http::Client;
use autumn_web::prelude::*;

wire_client! {
    name = CatalogClient,
    endpoints = [
        mesh_catalog::get_item_endpoint(id),
        mesh_catalog::create_item_endpoint,
    ],
}

#[contract_checked(client = CatalogClient)]
#[get("/items/{id}")]
#[public]
async fn show_item(id: Path<String>, http: Client) -> AutumnResult<Markup> {
    let catalog = CatalogClient::new("http://catalog:3001", http);
    let item = catalog.get_item(&*id, NoBody).await?;
    Ok(html! { h1 { (item.name) } })
}
```

Each `endpoints` entry names a marker and, in parentheses, the path parameters
its route takes. Request and response types come from the marker's associated
types, so they are the callee's real types. A body-less endpoint takes `NoBody`.

## What the check adds over the type checker

Both ends share the same Rust types, so a removed field already breaks the
build. Three breaks do not:

1. **A new required request field.** The caller builds its request with
   `..Default::default()`. Adding a required field to the callee still
   compiles; the request 400s in production.
2. **`#[serde(skip_serializing)]` on a response field a caller reads.** The
   Rust field still exists, so the caller compiles; the value never arrives.
3. **`#[serde(skip_deserializing)]` on a request field a caller sets.** The
   caller compiles; the value is silently dropped.

The check also turns the breaks rustc *does* see into a diagnostic that names
the endpoint and the field rather than a bare "no field `x`".

## How it works

`#[contract_checked]` reads the function it is on and collects, per call site:

* the **read-set** — every response field the caller names, off a binding
  (`item.name`), a destructuring `let`, the call expression itself, or inside a
  macro body (`html! { (item.name) }`);
* the **write-set** — every request field an inline struct literal sets.

Each becomes a const assertion against the callee's own const field table:

```rust
const _: () = assert!(
    autumn_web::wire::has_field(
        <get_item_endpoint as Endpoint>::RESPONSE_FIELDS,
        "name",
    ),
    "wire contract broken in `show_item` at `get_item(…)`: …",
);
```

The assertion is a cross-crate compile-time fact, so rustc rebuilds the caller
whenever the callee's table changes. Nothing can go stale.

## The build artifact

`#[endpoint]` and `#[derive(WireShape)]` also write JSON descriptors under
`<workspace>/target/autumn-contracts/`:

```json
{
  "service": "catalog",
  "name": "get_item",
  "endpoint_ident": "get_item_endpoint",
  "krate": "mesh-catalog",
  "method": "GET",
  "path": "/items/{id}",
  "request_type": "NoBody",
  "response_type": "Item"
}
```

Types are described once, in their own `type.*.json` file, and referenced by
name. `#[contract_checked]` reads them back to name a *missing required* field,
which a const-eval message — which must be a literal — cannot compute on its
own. Everything else is held by the const assertions, so a missing descriptor
degrades the diagnostic and never the guarantee.

Set `AUTUMN_CONTRACT_DIR` to write them elsewhere, or to `""` to turn them off.

## Refusals

A descriptor that is not true of the code is worse than none, so
`#[derive(WireShape)]` refuses what it cannot read: generic types, enums, tuple
structs, `#[serde(flatten)]`, and a split
`rename(serialize = …, deserialize = …)`.

`#[contract_checked]` refuses a client it cannot find a value for in the
annotated function — otherwise it would pass by checking nothing. It recognises
a client that arrives as a typed parameter, a typed `let`, or a `let`
initialised from the client's own constructor.

## Limits of the first slice

* One workspace. Both ends must be present at build time.
* Synchronous request/response, JSON over HTTP. No streaming, websockets, or
  events.
* Path parameters and a JSON body. Query parameters are not part of the
  contract yet.
* A write-set is only visible when the request is an inline struct literal at
  the call site. A request built elsewhere is checked by type, not by field.
* No cross-version proof. Whether the set of versions live during a rolling
  deploy is mutually compatible is the next slice, not this one.

## Worked example

`examples/mesh-catalog` and `examples/mesh-storefront`. The storefront's README
walks through a seeded breaking change and its fix, and
`examples/mesh-storefront/contract-sweep.py` runs the whole mutation set.
