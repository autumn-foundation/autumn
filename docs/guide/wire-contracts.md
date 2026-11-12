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

A nested field is a dotted path. `item.details.color` reads `details.color`.
`details: Details { color: Some(c), ..Default::default() }` sets it. Each set
also holds the prefix, `details`.

The const assertions check only the top-level fields. In one build, the
caller and the callee use the same nested type. So rustc checks each nested
field. Each top-level field becomes a const assertion against the callee's
own const field table:

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
  "response_type": "Item",
  "handler": "get_item",
  "response_path": "Item"
}
```

Types are described once, in their own `type.*.json` file. A type file holds
the crate and the module of the source file. The endpoint holds each type's
path as written, for example `shared::Item`. `#[contract_checked]` reads them
back **only to write a better message** —
naming the field a coverage failure is about, which a const-eval message (a
literal) cannot compute for itself. Every assertion is emitted either way, so a
descriptor that is missing, stale or wrong can mislabel a failure but can never
cause or hide one.

`wire_client!` writes a `client.*.json` file: the endpoint marker behind each
method. `#[contract_checked]` writes a `caller.*.json` file: the read-set and
write-set of each call. `autumn contracts export` uses these two to find what
each deployed caller reads and sends.

In the export pass only, each `routes![…]` call writes a `routes.*.json`
file: the handlers it registers.

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

The export finds each type by its path, its crate, and the module of its
source file:

* A path that starts with a crate name (`shared::Item`) is that crate's
  type. A renamed dependency counts too.
* A bare name (`Item`) is the type defined in the same module, if there is
  one. Else it is the one type of that name in the crate or in a crate it
  depends on.
* When more than one type can be meant, the export stops. Write the full
  path in the signature, for example `Json<crate::dto::Item>`.

`--from <dir>` joins a directory of descriptors that a build already wrote.
No build runs, and there is no dependency graph, so a bare name can come
from any crate in the directory. `--from <file>` reads a bundle file.

Use `-p` to export some packages only. The default is the whole workspace. The
pass checks libraries and binaries. Callers in tests and examples are not part
of the contract.

After an export, the next normal `cargo check` rebuilds the crates that use
the wire macros or `routes![…]` once, because the tracked value changed back.

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
* **Router.** An `#[endpoint]` that no `routes![…]` in the pass registers is
  not served. Removing a handler from its router is a removed route. A new
  caller of such a route is a break in both orders. A caller that the
  deployed release already left without a route is not a new break. See
  [Limits](#limits-of-the-first-slice) for the cases that the router check
  cannot see.
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
* **Unit.** A unit struct (`struct Empty;`) is JSON `null`. An empty struct
  (`struct Empty {}`) is `{}`. A change from one to the other is a break in
  both orders. This also holds for list elements and nested types.
* **Nested types.** A field whose type is a `WireShape` type is compared
  field by field, with the same rules. A break names the path, for example
  `details.color`. The read and write rules also apply one level down. The
  callee must produce each nested field that the caller reads. The callee
  must accept each nested field that an inline literal sets. This is also
  true for an optional nested field. One build does not check a nested
  field against the caller's own release. So a nested rule reports only a
  change from that release: a field it did not produce or accept is not a
  break.
* **Field types.** A field's Rust type must not change. Path prefixes do not
  count: `std::string::String` is `String`. A decoder may read `T` as
  `Option<T>`.
* **Wire types.** A field's JSON form must not change. This is also true
  when the Rust type stays the same, for example `DateTime<Utc>` that gets
  `#[serde(with = "ts_seconds")]` and `#[wire(type = "integer")]`. A nullable
  form reads the form it wraps. `any` is not compared. See
  [Wire types](#wire-types).
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

The export writes bundle format 4. It records the wire type of each field,
nested types, the `unit` flag, and endpoints that no router registers. An
older CLI refuses it. This CLI reads formats 1 to 4. When one of the two
bundles is format 3 or older, the check does not compare wire types. With a
bundle of format 1 or 2, the check compares nested types by name only. It
does not compare the `unit` flag of a body or a nested type. It compares the
`unit` flag of an event in every format.

Nested reads and writes do not change the format. Each dotted path also
lists its prefix, so an older CLI reads the top-level fields as before. A
bundle from an earlier export has no nested reads or writes. Then the check
compares nested fields as before. Export the deployed release again to get
them.

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
* `#[cfg_attr(…, serde(…))]`, and a field behind `#[cfg(…)]` — the macro
  cannot see the cfg, so it cannot know which shape the build has.

The export refuses a field whose type has no `WireShape` descriptor and is
not a known leaf (a primitive, `String`, a standard container,
`serde_json::Value`, `Uuid`, a `chrono` or `time` type, `Decimal`, `Url`,
and similar). An enum is the usual case. Mark such a field
`#[wire(opaque)]` to compare it by type name only:

```rust
#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    #[wire(opaque)]
    pub status: Status,
}
```

`#[serde(with)]` and `#[serde(deserialize_with)]` *are* supported, and taken
seriously: serde bypasses its own missing-field shortcut for such a field, so
even an `Option<T>` becomes mandatory unless a default fills it in. The
field's wire type is `any` unless `#[wire(type = …)]` names it. See
[Wire types](#wire-types).

`#[contract_checked]` refuses a client it cannot find a value for in the
annotated function — otherwise it would pass by checking nothing. It recognises
a client that arrives as a typed parameter, a typed `let`, or a `let`
initialised from the client's own constructor.

## Wire types

Each field in a descriptor has a `ty` and a `wire_type`. `ty` is the Rust
type as written. `wire_type` is the JSON form that serde writes. A later IDL
export can map it. The cross-release check compares both.

| Rust type | Wire type |
|---|---|
| `String`, `&str`, `char`, `PathBuf`, `Path`, `SocketAddr` | `string` |
| `i8` … `i128`, `u8` … `u128`, `isize`, `usize`, `NonZero*` | `integer` with its width and sign |
| `f32`, `f64`, `serde_json::Number` | `number` |
| `bool` | `boolean` |
| `Vec<T>`, `VecDeque<T>`, `LinkedList<T>`, `BinaryHeap<T>`, `HashSet<T>`, `BTreeSet<T>`, `IndexSet<T>`, `SmallVec`, `[T; N]`, `&[T]` | `array` of `T` |
| `Bytes`, `ByteBuf` | `array` of `integer(u8)` |
| `HashMap<K, V>`, `BTreeMap<K, V>`, `IndexMap<K, V>`, `serde_json::Map` | `map` of `V`, when `K` is a string or an integer. Else `any` |
| `Option<T>` | `nullable` of `T` |
| `Box<T>`, `Rc<T>`, `Arc<T>`, `Cow<T>`, `Cell<T>`, `RefCell<T>`, `Reverse<T>`, `Wrapping<T>` | the wire type of `T` |
| `chrono::DateTime<Tz>` | `string`, format `date-time` |
| `chrono::NaiveDateTime`, `NaiveDate`, `NaiveTime` | `string`, format `local-date-time`, `date`, `local-time` |
| `uuid::Uuid` | `string`, format `uuid` |
| `rust_decimal::Decimal`, `BigDecimal` | `string`, format `decimal` |
| `url::Url`, `IpAddr`, `Ipv4Addr`, `Ipv6Addr` | `string`, format `uri`, `ip`, `ipv4`, `ipv6` |
| a `WireShape` type, `Self` | `object`, with the type id |
| `serde_json::Value`, a tuple, `()`, a `time` crate type, another known leaf | `any` |
| a field with `#[serde(with)]`, `serialize_with` or `deserialize_with` | `any` in that direction |

A `time` crate type is `any`: its JSON form depends on a feature of the
`time` crate, which the macro cannot see. A `DateTime` with no time zone
argument is not chrono's, so it is `any` too.

The macro finds a type by its last name, as the export finds a known leaf.
In a `#[wire(opaque)]` field, a type that the macro does not know, and
`Self`, are `any`.

The check does not compare `any`. It is a form that the macro cannot read,
so the check compares `ty` only.

### Name the wire type

When the macro cannot see the JSON form, name it with `#[wire(type = …)]`:

```rust
#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    #[serde(with = "chrono::serde::ts_seconds")]
    #[wire(type = "integer", format = "unix-seconds")]
    pub created_at: DateTime<Utc>,
    #[wire(type = "string", nullable)]
    pub nickname: Nickname, // type Nickname = Option<String>;
}
```

* `type` is `string`, `integer`, `number`, `boolean` or `any`. Another value
  is a compile error. An `integer` is a signed 64-bit integer.
* `format` goes with `string` or `integer`. It is words of lowercase letters
  and digits, joined by `-`, such as `date-time`, `unix-seconds` or `email`.
* `nullable` makes the type nullable. Use it for an `Option` under another
  name. A request can then leave the field out, as for `Option<T>`. With
  `#[serde(with)]` or `deserialize_with`, the field stays required.
* On an `Option<T>` field with no custom serde function, the wire type is
  nullable without `nullable`. A custom function writes what `type` says, so
  add `nullable` if the function writes `null`.
* The field is opaque. The export does not look for a `WireShape` of its
  Rust type, so a type alias needs no descriptor.

The wire type applies to both directions. With `serialize_with` or
`deserialize_with` on one side only, it applies to that side. The other side
keeps the form that the macro reads off the Rust type, and is not opaque.

## Events

An `#[event]` payload is a wire contract too. `#[event]` derives `WireShape`
and gives the event a topic and a version. A `#[listener]`, or an
`#[event_consumer]` on an outbox topic handler, records its read-set. A
nested read is a dotted path, as for a caller. Each top-level read becomes a
const assertion against the event's field table:

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

## Consumers outside the workspace

The check protects callers in one Cargo workspace. A consumer in another
repository, team or language cannot build against the Rust types. Give it a
protobuf schema of the contract, or an Avro schema of each event (see
[Avro schemas for events](#avro-schemas-for-events)):

```bash
autumn contracts export --proto --out contracts/proto/
```

`--proto` takes the same `-p`, feature and `--from` flags as the bundle
export. It writes text only. The CLI does not need `protoc`. The Rust types
stay the source of truth, and JSON stays the wire format.

The export writes:

| File | Holds |
|---|---|
| `autumn/<service>/<service>.proto` | one `<Service>Service`, its rpcs and their messages |
| `autumn/<crate>/events/events.proto` | one message per `#[event]` of the crate |
| `buf.yaml` | the lint and breaking rules for these files |
| `proto.lock`, beside `--out` | the field numbers (`--lock <path>` moves it) |

```proto
// Rust type `Item`.
message Item {
  string id = 1 [json_name = "id"]; // required
  string name = 2 [json_name = "name"]; // required
  optional string sku = 3 [json_name = "sku"];
}

service CatalogService {
  // GET /items/{id}
  rpc GetItem(GetItemRequest) returns (Item);
}
```

* `json_name` is always the serde wire name. A protobuf JSON parser reads the
  JSON that Autumn sends. A wrapper is the exception. See below.
* The path parameters and the body of an endpoint go into one
  `<Rpc>Request` message. When a body field has the name of a path
  parameter, the body is one field, `body`, so it stays whole.
* The HTTP method and path go in a comment. `--http-annotations` also writes
  `google.api.http` options, with each path parameter named by its proto
  field. The export adds the googleapis dependency to `buf.yaml`. Run
  `buf dep update` to fetch it. A GET or DELETE rule has no `body`.
* An event message has the name of the event type. A comment gives its
  topic.
* Each file holds every message it needs, so a consumer can use one file.
* An endpoint that no router serves is not in the files.
* A response with no body, or a unit struct, is `google.protobuf.Empty`. A
  comment says what Autumn sends: no body, or JSON `null`.
* A Rust type with the name of a generated message (`<Rpc>Request`, a
  wrapper, `<Service>Service`) stops the export. Rename the type.

### Type mapping

| Wire type | Proto |
|---|---|
| `string` (any format) | `string`. With `--well-known-types`, `date-time` is `google.protobuf.Timestamp` |
| `integer` | `int32`, `int64`, `uint32` or `uint64` by width and sign. 128 bits is `google.protobuf.Value`, read as a double |
| `number` | `double`. serde_json writes NaN and infinity as `null`, which reads as 0 |
| `boolean` | `bool` |
| `array` | `repeated T` |
| `map` | `map<string, T>` |
| `object` | a message |
| `nullable` | `optional T`. A message field and `google.protobuf.Value` hold null already, so they stay plain fields |
| `any` | `google.protobuf.Value` |

A format, such as `uuid`, goes in the field comment. When one side of a
field is `any`, for example with a custom serde function on that side only,
the field is `google.protobuf.Value`. So is a field whose two sides have
different forms, for example a `#[wire(type = …)]` on the serializer only.

Protobuf cannot write some shapes in place. The export writes a wrapper
message for each. Its name ends in `Wrapper`, so it is easy to find:

| Shape | Proto |
|---|---|
| `Vec<Vec<T>>` | `repeated TListWrapper` |
| `Vec<Option<T>>` | `repeated NullableTWrapper` |
| `HashMap<String, Option<T>>` | `map<string, NullableTWrapper>` |
| a map of lists or maps | `map<string, TListWrapper>`, `map<string, TMapWrapper>` |
| a list of maps | `repeated TMapWrapper` |
| a list response (`Json<Vec<Item>>`) | `ItemListWrapper` |
| a list request body | a `repeated` field `items` in `<Rpc>Request` |

A wrapper is not the JSON that Autumn sends. Autumn sends the bare value: an
array, a map, or a value or `null`. A protobuf JSON parser expects an object
with a `values` or `value` field, so it refuses Autumn's JSON for a wrapper
field. Wrap the value yourself before you parse it: `[1, 2]` becomes
`{"values": [1, 2]}`. A comment on each wrapper says so. With
`--http-annotations`, a list response has `response_body: "values"`, so a
transcoder uses the bare array.

A `nullable` list or map is a plain `repeated` or `map` field. Protobuf JSON
reads `null` as empty.

### Requests from a protobuf consumer

The schema is made to read Autumn's JSON. A consumer that writes a request
with a protobuf JSON writer must do two things:

* Write fields with default values. By default a writer leaves out `0`,
  `""` and `false`, and serde then refuses a required field. Turn on the
  writer's option to emit unpopulated fields.
* Not send a 64-bit integer field. Protobuf JSON writes `int64` and `uint64`
  as strings, and serde reads an integer only as a JSON number.

### Field numbers

JSON has no field numbers, so the export gives them. A number must never move
after you publish it.

* The lock keeps each number, by message and wire name. It is
  sorted TOML. Commit it.
* A new field gets the next free number. The numbers 19000 to 19999 are not
  used.
* A removed field moves to `reserved`, with its number and its name. No other
  field gets that number. When the field comes back with the same wire name,
  it gets its old number again.
* `#[wire(tag = N)]` on a field sets its number. It wins over the lock, and
  the number it replaces stays reserved. Two
  equal tags on one type, or a tag outside protobuf's range, is a compile
  error. A tag on the number of another field, or on a reserved number,
  stops the export.

```rust
#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    #[wire(tag = 1)]
    pub id: String,
}
```

* The lock is `proto.lock` beside `--out`: `--out contracts/proto/` uses
  `contracts/proto.lock`.
* `--check` writes nothing. It exits 1 when the lock is missing or would
  change, or when a file would change or go away. It reads CRLF as LF. Run
  it in CI, so a lock that is not committed fails the build:

```bash
autumn contracts export --proto --out contracts/proto/ --check
```

The export removes a `.proto` file under `<out>/autumn/` that it wrote
before and does not write now, and names it. A file without the export's
first line stays.

Two exports of the same source give the same bytes. `--from` takes a
published bundle file, so you can export the schema of any release with no
build. This updates the lock. Pass `--lock` with a scratch path to keep the
committed lock as it is:

```bash
autumn contracts export --from release-1.json --proto \
  --out target/proto-1/ --lock target/proto-1.lock
```

A lock key is the full message name. A message that gets a new name, for
example because a second Rust type with the same name is now in the file,
gets new numbers.

### Required fields

proto3 has no `required`. A field that serde requires stays a plain field,
with `// required` in its comment. The `.proto` files are a schema for
consumers. They are not the gate: `autumn contracts check` stays the gate for
a field that a deployed caller reads.

### buf

The `buf.yaml` uses the `STANDARD` lint rules, with three exceptions:

* `RPC_RESPONSE_STANDARD_NAME`: an rpc returns the message that Autumn sends.
* `RPC_REQUEST_RESPONSE_UNIQUE`: two rpcs can return one message.
* `PACKAGE_VERSION_SUFFIX`: the package is the service name.

Breaking changes use `WIRE_JSON`, because JSON is the wire format.

`buf breaking` finds no break in a change that `autumn contracts check`
approves. They differ by design on some changes that the check refuses:

* A new required request field, or a removed required nested field. proto3
  has no required fields.
* A removed response field that a caller reads, and a new field that a new
  caller reads. buf cannot see read-sets.
* A changed path or method, and an endpoint removed from its router. buf
  does not compare comments, and `WIRE_JSON` does not check removed rpcs.

`autumn-cli/tests/integration/contracts_proto.rs` names each case.

### Avro schemas for events

Event data often goes to a broker, a data pipeline or a warehouse loader.
Avro is the usual schema there. Export one Avro schema per event topic:

```bash
autumn contracts export --avro --out contracts/avro/
```

`--avro` takes the same `-p`, feature and `--from` flags as the bundle
export. It writes text only. The CLI has no Avro dependency.

The export writes one `.avsc` file per topic: `contact.updated.avsc`,
`contact.updated.v2.avsc`. A new version is a new topic, so it is a new
subject in a schema registry.

```json
{
  "type": "record",
  "name": "ContactUpdated",
  "namespace": "autumn.core",
  "doc": "topic: contact.updated, version 1",
  "autumn.topic": "contact.updated",
  "fields": [
    { "name": "id", "type": { "type": "string", "logicalType": "uuid" } },
    { "name": "email", "type": "string" },
    { "name": "phone", "type": ["null", "string"], "default": null }
  ]
}
```

* The record name comes from the topic, and the namespace from the crate:
  `autumn.<crate>` (`mesh-catalog` is `autumn.mesh_catalog`). A Rust rename
  of the event type does not change it.
* A field name is the serde wire name. A `#[serde(alias)]` is an Avro
  `aliases` entry, so a rename with an alias resolves in Avro as it does in
  Autumn.
* A nested type is a record. Its name comes from its type id:
  `shop::model::Address` is `autumn.shop.model.Address`. A file defines it
  once, then names it.
* A unit event (`struct Ping;`) is a record with no fields. Autumn sends JSON
  `null` for it. The `doc` says so.
* An opaque event (`#[event(opaque)]`) is `bytes`: the JSON text.
* `autumn.topic` names the topic, and `autumn.endpoint` the endpoint. The
  export removes a stale `.avsc` in `--out` or `--out/endpoints/` only when
  it has one of these keys, and names each removal. It does not look in
  other directories.
* `--check` writes nothing. It exits 1 when a file is missing, would change
  or would go away.
* Two exports of the same source give the same bytes.

`--endpoints` also writes the body of each routed endpoint:
`endpoints/<service>.<endpoint>.request.avsc` and `.response.avsc`. A route
with no body has no file. A list body is an Avro `array` of the record. It is
off by default: `--proto` is the schema for endpoints. Use the same flags in
each run: an export without `--endpoints` removes the body schemas.

#### Avro type mapping

| Wire type | Avro |
|---|---|
| `string` | `string`. `uuid` has `"logicalType": "uuid"`. Another format stays `string` with an `"autumn.format"` key, because Avro has no logical string type for it |
| `integer`, `i8` to `i32`, `u8`, `u16` | `int` |
| `integer`, `i64`, `isize`, `u32` | `long` |
| `integer`, `u64`, `usize`, 128 bits | refused |
| `number` | `double` |
| `boolean` | `boolean` |
| `array`, `map` | `array`, `map` |
| `object` | a named `record` |
| `nullable` | `["null", T]` with `"default": null` |
| `any` | refused, unless the event is `#[event(opaque)]` |

An integer format, such as `unix-seconds`, also goes in `"autumn.format"`.

A field that the JSON can leave out gets a default, so an Avro reader can
read data without it. This is a field with `#[serde(default)]` or
`skip_serializing_if`, or a field on one serde side only. A list gets `[]`, a
map gets `{}`, and another form becomes `["null", T]` with `null`. There,
`null` means that the key is absent. In a body schema, a request field counts
when serde reads it, and a response field when serde writes it.

The export stops on a form that Avro cannot hold, and names each field and
its fix:

* `u64`, `usize` and 128-bit integers. Use a smaller type. Or send the value
  as a string with a serde adapter, such as `serde_with::DisplayFromStr`, and
  name that form with `#[wire(type = "string")]`. Use
  `#[wire(type = "integer")]` only when a custom serializer writes an `i64`.
  On a released field, each of these is a new JSON form, which the check
  reports.
* `any`: `serde_json::Value`, a custom serde function, a `#[wire(opaque)]`
  field. Name the form with `#[wire(type = …)]`.
* A nested type with no shape in the bundle.
* A field whose serialized and deserialized forms differ.
* A wire name that is not an Avro name, such as `price-cents`. An Avro name
  has only ASCII letters, digits and `_`.
* A topic or endpoint that is not a safe file name, such as `../x` or a
  Windows device name like `aux`, and two files that differ only in case.
* Two types that map to one Avro name.

#### The data, not Avro's JSON encoding

Autumn sends plain JSON. Avro's JSON encoding is different: it wraps a union
value (`{"string": "x"}`). So the schema describes the data. A bridge that
writes Avro binary to a broker reads Autumn's JSON and encodes it with the
schema. Do not give Autumn's JSON to an Avro JSON decoder.

#### Registry compatibility

A schema registry checks each new schema of a subject against the last one.
Use the mode that matches the deploy order you check with
`autumn contracts check`:

| `autumn contracts check` | Who ships first | Registry mode |
|---|---|---|
| no `--order` (safe in any order) | either | `FULL` |
| `--order callee-first` | producers | `FORWARD`: old consumers read new data |
| `--order caller-first` | consumers | `BACKWARD`: new consumers read old data |

The check compares the new build with the deployed release only, so use the
non-transitive modes.

The registry is not the gate. `autumn contracts check` stays the gate. Avro
resolution agrees with the check in each window, but they differ by design
on some changes:

* Read-sets. Avro checks only the fields in the schema, and fills a missing
  nullable field with `null`. The check sees that a consumer reads it.
* `deny_unknown_fields`. An Avro reader skips a field it does not know.
* Topics. A new version is a new subject. A registry does not know which
  topics a producer still sends.
* Presence. Avro data holds each field, so a field that the JSON can leave
  out gets a default. The check sees that a deployed consumer needs it.
* Unit events. Fields on a unit event change its JSON from `null` to an
  object. In Avro, nullable fields added to a record with no fields are
  compatible.
* Opaque events. The check does not compare them. Avro compares a record
  with `bytes`.
* Integer widths. Avro promotes `int` to `long`. The check compares Rust
  types: `i32` is not `i64`, so it reports a break in both windows.

`autumn-cli/src/contracts/avro/agreement.rs` names each case.

## Limits of the first slice

* One workspace. Both ends must be present at build time. A consumer outside
  the workspace gets a protobuf or Avro schema, not a build check. See
  [Consumers outside the workspace](#consumers-outside-the-workspace).
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
* A field with `#[serde(with)]`, `serialize_with` or `deserialize_with` has
  the wire type `any`, unless `#[wire(type = …)]` names it. The check does
  not compare `any`, so a new custom form with no `#[wire(type = …)]` does
  not show.
* A field with `#[wire(type = …)]` is opaque. A change inside its Rust type
  does not show.
* `Decimal` and `BigDecimal` are `string` with their default serde
  features. A feature that writes them as a JSON number does not show:
  name the form with `#[wire(type = "number")]`.
* An `integer` override is 64 bits and signed. A custom function that writes
  another width cannot say so.
* `Option<T>` is recognised by name, so a type alias for one reads as
  required, unless the field has `#[wire(type = …, nullable)]`.
* The check still compares `ty`. A type alias change reads as a type change,
  even when the wire type is the same.
* The macro finds a type by its last name. A type of your own named like a
  known type, such as `Decimal`, gets the wire type of the known type.
* A bundle older than format 4 has no wire types. The check compares wire
  types only when both bundles have them.
* The cross-release check compares two releases: the deployed one and the
  current one. More than two live releases need one check per pair.
* The cross-release check knows only the callers in the bundle. It cannot see
  a client outside the workspace, so it keeps every deployed route. Give such
  a client a protobuf schema. See
  [Consumers outside the workspace](#consumers-outside-the-workspace).
* A field with `#[wire(opaque)]` is compared by its type name only. A change
  inside its type does not show.
* A nested type pair reports at most 100 issues. The verdict does not
  change.
* A known leaf is found by its last name. A type of your own named like a
  leaf, such as `Date` or `Value`, needs no descriptor. It is compared by
  name only.
* An event consumer is checked only for the fields it names. A payload given
  whole to another function is not checked field by field.
* An event without `name` is not in the bundle. A consumer of a hand-written
  `impl Event`, or one that names its event through a `use … as` alias, is
  not in the bundle either. The export warns, and the check names each such
  consumer.
* Two `#[event]` types with one type name in one crate stop the export.
* The module of a type is the module of its source file. An inline `mod` is
  not visible. Two types with one name in one source file share one
  descriptor file. A bare name in a file with an inline `mod` that defines a
  type of that name means that type, also when the file imports another. In
  `src/main.rs`, a path such as `crate::dto::Item` is refused when
  `src/main.rs` also has a type of that name: it can be an inline `mod dto`,
  and `src/dto.rs` can belong to the library.
* The router check sees `routes![…]` calls, not the app's router. A
  `routes()` function that is never mounted still counts. A path that can
  name more than one handler counts for each of them.
* The router check judges a crate only when a `routes![…]` in the pass names
  one of its endpoints. Then an endpoint that the crate mounts in another
  way, for example with a hand-written route, reads as not served. A crate
  that no `routes![…]` names counts as served, for example when its router is
  outside the `-p` set. So a router that drops every endpoint of a crate does
  not show.
* A `routes![get_item]` entry with no crate path, for a handler of another
  crate (through a `use`), registers that handler. It does not judge that
  crate, because the name can also be a local function. Write
  `routes![mesh_catalog::get_item]` to have the check judge it.
* The check sees a nested read through field access, a struct pattern, a
  `let`, `if let` or `match` binding, a macro body, a maud `@let` or
  `@if let`, `unwrap`, `clone`, `as_ref`, and the `Option` and `Result`
  callbacks (`item.details.as_ref().map(|d| d.color)`).
* A loop, an index or another method call stops the path.
  `item.details.lines[0].price` reads `details.lines` only.
* The check knows a nested write-set only when the call site sets a named
  field in an inline literal, or in `Some(literal)`. For other values, the
  check uses the type, as for a request built elsewhere.
* A request or response type is found by the path in the signature. A path
  through a re-export, such as `crate::dto::Item` for `shared::Item`, is
  refused when another crate has a type of that name. Write the path where the
  type is defined. The check cannot tell an inline `mod` from a module file
  with no `WireShape` type. So a re-export in such a file can still resolve to
  a type of a parent file.
* A `use … as` alias or a `type` alias is read by its own name. If a local
  type has that name, the check compares that type. Write the path of the type
  itself.
* A user type named `NoBody` and the framework's `NoBody` have one name. A
  bare `NoBody` that the handler's module does not define is refused. Write
  its path, for example `crate::dto::NoBody`.
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
