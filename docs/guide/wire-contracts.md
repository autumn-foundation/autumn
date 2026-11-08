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
| producer | `#[event(name = "…")]` | records an event payload's field shape |
| consumer | `#[listener]`, `#[event_consumer]` | fails the build when a read disagrees |

For events, see [Events](#events).

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
build. These breaks do not:

1. **`#[serde(skip_serializing)]` on a response field a caller reads.** The
   Rust field still exists, so the caller compiles; the value never arrives.
2. **`#[serde(skip_deserializing)]` on a request field a caller sets.** The
   caller compiles; the value is silently dropped.
3. **A required request field the request type may leave off the wire.** A
   field carrying `#[serde(skip_serializing_if = …)]` is absent from the body
   whenever the predicate matches, so a call site that does not set it sends a
   request the callee rejects.
4. **A route path that stops taking the parameters the client declares.** The
   URL still builds; it just addresses nothing.

The check also turns the breaks rustc *does* see into a diagnostic that names
the endpoint and the field rather than a bare "no field `x`".

### What is *not* a break, and why

`NewItem { name, ..Default::default() }` is a complete request. Both ends share
the type, so serialization emits every field — the rest initializer sends a
default *value*, not nothing. Adding an ordinary required field to the callee
therefore does not break this caller, and the check does not flag it. Only a
field the request type may keep off the wire (case 3 above) can actually go
missing.

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
name. `#[contract_checked]` reads them back **only to write a better message** —
naming the field a coverage failure is about, which a const-eval message (a
literal) cannot compute for itself. Every assertion is emitted either way, so a
descriptor that is missing, stale or wrong can mislabel a failure but can never
cause or hide one.

`wire_client!` writes a `client.*.json` file: the endpoint marker behind each
method. `#[contract_checked]` writes a `caller.*.json` file: the read-set and
write-set of each call. `autumn contracts export` uses these two to find what
each deployed caller reads and sends.

`#[event]` writes an `event.*.json` file: the topic, the version and the
payload shape. `#[listener]` and `#[event_consumer]` write a
`listener.*.json` file: the event type and the read-set.

The descriptors are proc-macro side effects, not declared build outputs. A
normal build writes nothing for a crate that cargo finds fresh, and the
enriched message then falls back to the plain one. Nothing deletes old files.
So a renamed endpoint leaves an orphan. When two descriptors claim one marker
name, enrichment switches off rather than guessing. The export pass below does
not have these problems.

Set `AUTUMN_CONTRACT_DIR` to write them elsewhere, or to `""` to turn them off.
The macros emit code that reads this variable through `option_env!`. So cargo
rebuilds that crate when the value changes.

## Across releases

`#[contract_checked]` proves that caller and callee agree at one commit. In a
rolling deploy, two releases run at the same time. `autumn contracts` checks
that they agree.

### Publish the contract

```bash
autumn contracts export --out target/contracts.json
```

The export runs a dedicated `cargo check` pass with a new, empty
`AUTUMN_CONTRACT_DIR`. Cargo sees a new value and runs every wire macro again,
so a cached build cannot leave a descriptor out. The pass also turns off a
rustc wrapper, such as `sccache`, for the same reason. In this pass, a
descriptor that cannot be written is a compile error. The bundle is sorted JSON
with no paths and no times. Two exports of the same source give the same bytes.
Store the bundle with each release.

Use `-p` to export some packages only. The default is the whole workspace. The
pass checks libraries and binaries. Callers in tests and examples are not part
of the contract.

After an export, the next normal `cargo check` rebuilds the crates that use
the wire macros once, because the tracked value changed back.

### Check against what is deployed

```bash
autumn contracts check --against deployed.json
```

`--against` takes a bundle file, an `https` URL, or a git ref:

* A URL must use `https`. Pass `--allow-http` to accept plain `http`. A
  redirect from `https` to `http` is refused. The limit is 16 MiB.
* For a git ref, the check exports that commit in a temporary git worktree. It
  uses its own target directory, `target/autumn-contracts-ref`. A `-p` package
  that does not exist at that commit is not checked there.

The check looks at two deploy windows:

| Window | Who runs together |
|---|---|
| callees ship first | new callees, deployed callers |
| callers ship first | new callers, deployed callees |

In each window, each caller uses the types of its own release. The callee
serves with the types of its release. The rules:

* **Route.** The callee must serve the caller's method and path. A deployed
  route that no endpoint serves is a break, also with no known caller.
  Path parameter names do not count: `/items/{id}` and `/items/{item_id}` are
  the same route.
* **Response read.** The callee must produce each field the caller reads.
* **Response decode.** The callee must always produce each field the caller's
  type needs to decode, also when the caller does not read it.
* **Request required.** The caller must always send each field the callee
  requires. A field is sent when the caller's type always serializes it, or
  when the call site sets it.
* **Request dropped.** The callee must accept each field the call site sets.
* **Closed objects.** With `#[serde(deny_unknown_fields)]`, the other side must
  not send an unknown key.
* **Body.** A caller that sends no body must not meet a callee that reads
  one. A body that is a JSON array (`Vec<T>`) must stay an array, and an
  object must stay an object.
* **Field types.** A field's Rust type must not change. Path prefixes do not
  count: `std::string::String` is `String`. A decoder may read `T` as
  `Option<T>`.
* **Duplicate keys.** One object must not hold a field's name and its alias.
  serde refuses it.

Fields match by JSON key and alias, not by Rust name. So a `#[serde(rename)]`
that keeps the key is not a break, and a key rename with `#[serde(alias)]` is
safe for requests that deployed callers send.

So the callee may add an optional request field. It may add a response field
that no closed type and no new caller needs first. It may not:

* remove a response field a deployed caller reads;
* make a request field required that a deployed caller does not send;
* change a path or method without keeping the old route.

### Events across releases

The check also compares events. The producer side counts as the callee, and
the consumer side as the caller. Each consumer decodes with the event type of
its own release. The producer sends with the type of its release. Events
match by topic, so `contact.updated` and `contact.updated.v2` are two events.

The rules:

* **A producer may add fields.** A new field is not a break, unless the
  consumer's type has `#[serde(deny_unknown_fields)]`.
* **A producer may not remove a field any deployed consumer reads.** Remove
  it from the consumers first.
* **A consumer may not start to require a field that deployed producers do
  not send.** Add it to the producer first.
* **A field's type must not change**, with the same rules as a response.
* **A unit event stays a unit.** `struct Ping;` is JSON `null` on the wire.
  An object is another shape, so adding fields to it is a break in both
  orders. Publish a new version instead.
* **A topic must still be sent while a deployed consumer reads it.**
* **A new consumer must not miss a version that deployed producers send.** A
  consumer that reads only v2, next to a producer that sends only v1, loses
  the v1 events. A new consumer of a new event is not a break. It gets no
  events until the producer sends the topic.

A break shows the topic and the consumer:

```
If the callees ship first (new callees, deployed callers): 1 break(s)
  - contact.updated (event): consumer `marketing::on_contact` reads field `email`, which the new producer does not send
```

For a change that no order makes safe, publish a new version beside the old
one. `#[event(name = "contact.updated", version = 2)]` sends on
`contact.updated.v2`. Keep the producer of v1 while a consumer in the deployed
release or in the new release reads it. The report says when no consumer
reads it:

```
Note: Event `contact.updated` v1: no consumer reads it in the deployed release or in this one. It can be dropped.
```

An opaque event (`#[event(opaque)]`) has no shape, so the field rules skip
it. The check knows only the consumers in the bundle, so it does not stop
the removal of a topic that no bundled consumer reads. An event without
`name` is not in the bundle. A bundle from an older CLI has no events. Then
the check finds no deployed consumer to protect.

A bundle with events has format 2. A bundle with no events keeps format 1
and the same bytes. An older CLI refuses format 2.

### Deploy order

| Exit | Verdict |
|---|---|
| 0 | Safe in the declared order. With no `--order`, safe in any order. |
| 1 | Not safe in the declared order. With no `--order`, safe in no order. |
| 2 | Usage error: a flag is wrong. |
| 3 | Safe in one order only. The report names it. |
| 4 | The check could not run: a build, a file, a fetch, or git failed. |

Only exit 0 means "deploy".

Some changes are safe in one order only. Remove a response field: the callers
must first stop reading it, so they ship first. Read a new response field: the
callee must first produce it, so it ships first. The report says which:

```
Verdict: safe only if the callers ship first.
  Deploy mesh-storefront before catalog.
  To declare this order, run with --order caller-first.
```

`--order callee-first` or `--order caller-first` declares the order you plan.
The check then passes when that window is safe.

### CI recipe

Publish the bundle with each release, and check every pull request against it:

```yaml
# .github/workflows/contracts.yml
name: Wire contracts
on:
  release:
    types: [published]
  pull_request:

jobs:
  publish:
    if: github.event_name == 'release'
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install autumn-cli --locked
      - run: autumn contracts export --out contracts.json
      - run: gh release upload "${{ github.event.release.tag_name }}" contracts.json
        env:
          GH_TOKEN: ${{ github.token }}

  check:
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install autumn-cli --locked
      - name: Fetch the deployed contract
        id: fetch
        run: |
          if gh release download --pattern contracts.json --output deployed.json; then
            echo "found=true" >> "$GITHUB_OUTPUT"
          else
            echo "No release has contracts.json yet. Skip the check."
          fi
        env:
          GH_TOKEN: ${{ github.token }}
      - if: steps.fetch.outputs.found == 'true'
        run: autumn contracts check --against deployed.json
```

Notes:

* `gh release download` with no tag gets the latest release. Drafts and
  prereleases are not included. If the deployed release is not the latest, put
  its tag in the command.
* The first run has no `contracts.json` yet. The fetch step then skips the
  check.
* A release that a workflow publishes with `GITHUB_TOKEN` does not start the
  `release` event. Publish with another token, or run the publish job in that
  workflow.
* To check against a git ref, check out the full history:
  `actions/checkout@v4` with `fetch-depth: 0`.
* If a pull request needs a deploy order, the check exits 3. The report names
  the order. Add `--order` to the check step in that pull request. Remove it
  in the next pull request, after the release ships.

## Refusals

A descriptor that is not true of the code is worse than none, so
`#[derive(WireShape)]` refuses what it cannot read:

* generic types, enums, and tuple structs;
* `#[serde(flatten)]` — the flattened keys are not visible here;
* a split `rename(serialize = …, deserialize = …)` — the two directions carry
  different wire names;
* `#[serde(transparent)]` — there is no object on the wire;
* `#[serde(into = …)]`, `#[serde(from = …)]`, `#[serde(try_from = …)]` — the
  wire shape is another type's;
* `#[serde(tag = …)]` on a struct — it adds a key that is not a field;
* `#[cfg_attr(…, serde(…))]` — the macro cannot see the cfg, so it cannot
  know which shape the build has.

`#[serde(with)]` and `#[serde(deserialize_with)]` *are* supported, and taken
seriously: serde bypasses its own missing-field shortcut for such a field, so
even an `Option<T>` becomes mandatory unless a default fills it in.

`#[contract_checked]` refuses a client it cannot find a value for in the
annotated function — otherwise it would pass by checking nothing. It recognises
a client that arrives as a typed parameter, a typed `let`, or a `let`
initialised from the client's own constructor.

## Events

An `#[event]` payload is a wire contract too. `#[event]` derives `WireShape`
and gives the event a topic and a version. A `#[listener]`, or an
`#[event_consumer]` on an outbox topic handler, records its read-set. Each
read becomes a const assertion against the event's field table:

```rust
const _: () = assert!(
    autumn_web::wire::event_delivers(
        <ContactUpdated as autumn_web::events::Event>::SHAPE,
        "email",
    ),
    "event contract broken in `sync_marketing`: reads field `email` of event `ContactUpdated`, …",
);
```

See [Events: wire contracts](events.md#wire-contracts) for the attributes, and
[Events across releases](#events-across-releases) for the deploy rules.

## Limits of the first slice

* One workspace. Both ends must be present at build time.
* Synchronous request/response and events, as JSON. No streaming or
  websockets.
* Path parameters and a JSON body. Query parameters are not part of the
  contract yet.
* A write-set is only visible when the request is an inline struct literal at
  the call site. A request built elsewhere is checked by type, not by field.
* A client has to arrive as a parameter, a typed `let`, or `Client::new(…)`,
  and be called through that name. One reached through a struct field
  (`self.catalog.…`) is not visible, and `#[contract_checked]` refuses rather
  than passing while checking nothing.
* A field's recorded `ty` is the Rust type, not the wire type. So a
  `#[serde(with)]` representation change does not show in the descriptor or in
  the cross-release check. A type alias change reads as a type change.
* `Option<T>` is recognised by name, so a type alias for one reads as required.
* The cross-release check compares two releases: the deployed one and the
  current one. More than two live releases need one check per pair.
* The cross-release check knows only the callers in the bundle. It cannot see
  a client outside the workspace, so it keeps every deployed route.
* The cross-release check compares a nested type by its name only. A change
  inside a nested `WireShape` type does not show.
* An event consumer is checked only for the fields it names. A payload given
  whole to another function is not checked field by field.
* An event without `name` is not in the bundle. A consumer of a hand-written
  `impl Event`, or one that names its event through a `use … as` alias, is
  not in the bundle either. The export warns, and the check names each such
  consumer.
* Two `#[event]` types with one type name in one crate stop the export.
* A type is known by its name in its crate. Two types with one name in one
  crate share one descriptor file.
* A service that is a caller and a callee ships as one unit in each window. A
  chain such as A to B to C can read as unsafe in any order, though B first is
  safe.
* A client marker without a crate path (`get_item_endpoint(id)` after a `use`)
  must have a unique name across the pass. Else the export stops with an
  error.

## Worked example

`examples/mesh-catalog` and `examples/mesh-storefront`. The storefront's README
walks through a seeded breaking change and its fix, and a two-release scenario
against the committed `contracts/release-1.json`.
`scripts/wire-contract-sweep.py` runs the whole mutation set and the
two-release cases. The `Wire contracts` workflow runs it on each pull request
that changes wire code.
