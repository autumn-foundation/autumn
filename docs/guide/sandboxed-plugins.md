# Sandboxed Plugins

> **Experimental.** Behind the non-default `plugin-sandbox` feature on
> `autumn-web`. The native [`Plugin`](../plugins.md) trait is unchanged and
> fully supported; nothing here affects an app that does not opt in.

A native Autumn plugin is full-trust: its `build(self, app)` receives the whole
`AppBuilder`, and from there it can run migrations, read your credentials, open
sockets, and crash your process. That is the right trade for a plugin you wrote
or a first-party crate you already trust. It is the wrong trade for a plugin you
found on crates.io ten minutes ago.

A **sandboxed plugin** is the other trade. It ships as a portable artifact — a
`wasm32-wasip1` module plus a manifest — and the runtime enforces the manifest:

- it serves HTTP under **one** declared prefix, and only the routes it declares;
- it has **no** filesystem, **no** network, **no** environment, **no** database;
- it runs under hard per-request CPU and memory ceilings;
- a trap, an exit, a runaway loop or a memory bomb is a 5xx on its own prefix
  and nothing else.

Everything above is a property of the runtime, not a promise by the author.

---

## The trust model, in one table

| | Native `Plugin` | Sandboxed plugin |
|---|---|---|
| What `build` receives | the whole `AppBuilder` | nothing — the framework mounts it from the manifest |
| Filesystem, network, env | full process authority | none, and each attempt is logged |
| Sessions, auth, filesystem | full access | none exist as grantable capabilities |
| Database, cache, outbound HTTP, jobs | full access | only with an explicit grant, and only within it: plugin-owned tenant-scoped tables, a per-(plugin, tenant) key namespace, declared hostnames, declared job types |
| Routes it can mount | anywhere | exactly its declared `(method, path)` list, under its prefix |
| A panic | takes the process down | 502 on its own prefix |
| An infinite loop | hangs a worker | 504 on its own prefix, after its fuel budget |
| Language | Rust, compiled into your binary | anything that targets `wasm32-wasip1` |
| Speed | a function call | an interpreter, plus one instance per request |
| Review surface | the crate's source and its whole dependency tree | one page of TOML and a digest |

Native plugins remain the full-trust path, and first-party plugins stay on it.
Sandboxing is for code you have not audited and do not intend to.

---

## Authoring a plugin

The whole ABI is one JSON object in and one JSON object out, over the guest's
stdio. There is no SDK to depend on and no bindgen step: a guest reads one line
from stdin, writes one line to stdout, and exits.

```text
host  → guest  {"op":"request","wire_version":1,"granted":["http-request"],
                "method":"GET","route":"/hello/{name}","path":"/hello/ada","query":"",
                "path_params":[["name","ada"]],"headers":[["accept","text/html"]],"body_b64":""}
guest → host   {"op":"response","status":200,
                "headers":[["content-type","text/plain"]],"body_b64":"aGkgYWRh"}
```

A guest that cannot answer says so, and the host turns it into a 502:

```text
guest → host   {"op":"error","detail":"no handler for that route"}
```

Bodies are base64 so a binary response survives a line-oriented protocol.
Request headers arrive lower-cased and sorted, with credentials already
stripped (see [What never crosses](#what-never-crosses)).

In Rust, a complete guest needs `serde`, `serde_json` and `base64` — and
nothing from Autumn:

```rust
use std::io::{BufRead as _, Write as _};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Request {
    method: String,
    route: String,
    path_params: Vec<(String, String)>,
}

#[derive(Serialize)]
struct Response<'a> {
    op: &'a str,
    status: u16,
    headers: Vec<(&'a str, &'a str)>,
    body_b64: String,
}

fn main() {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return;
    }
    let request: Request = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(_) => return,
    };

    let name = request
        .path_params
        .iter()
        .find(|(key, _)| key == "name")
        .map_or("world", |(_, value)| value.as_str());
    let status = if request.method == "GET" && request.route == "/hello/{name}" {
        200
    } else {
        404
    };

    let response = Response {
        op: "response",
        status,
        headers: vec![("content-type", "text/plain; charset=utf-8")],
        body_b64: base64::engine::general_purpose::STANDARD.encode(format!("hi {name}\n")),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap_or_default());
    let _ = stdout.flush();
}
```

Build it as a **command** (it must export `_start`):

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
```

### What a guest will find missing

The sandbox implements the WASI preview-1 surface, but almost all of it says
no. `std::fs`, `std::net`, `std::env::var` and `std::env::args` all compile and
all fail. Two calls answer rather than refuse, and both are deliberately not
ambient:

- `SystemTime::now()` returns a **fixed** instant. The host's clock is not a
  capability a plugin was granted.
- `random_get` returns a **deterministic** stream, seeded from the request. The
  same request replays byte-for-byte, so you can reproduce a bug from the
  request alone — but it is not entropy, and nothing security-relevant may be
  derived from it. (You hold no capability that would make a secret useful,
  which is the point.)

Both make a plugin a function of its request, which is what lets an author
reproduce a bug from the request alone.

---

## Packaging

The manifest is the whole review surface. Write it next to your crate:

```toml
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
# Stamped by `autumn plugin package` — write anything 64-hex-characters long here.
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[routes]]
method = "GET"
path = "/hello/{name}"

[limits]
fuel = 200_000_000          # CPU ceiling for one request, in wasm fuel units
memory_bytes = 33_554_432   # linear-memory ceiling for one request's instance
max_request_body_bytes = 1_048_576
max_response_bytes = 4_194_304
max_concurrency = 8         # requests this plugin may execute at once
request_body_timeout_ms = 5_000  # how long the host waits for a body
```

Then bind the two together:

```bash
autumn plugin package \
  --manifest plugin.toml \
  --module target/wasm32-wasip1/release/hello.wasm \
  --out hello.autumn-plugin
```

`package` computes the module's SHA-256 and stamps it into the manifest — you
never type a digest — and it loads the module into the same sandbox the runtime
uses before writing anything. A plugin that could not run is refused at your
desk rather than at an operator's boot.

### The manifest fails closed

Every one of these is a hard error, not a warning:

| Written | Why it is refused |
|---|---|
| a key this build does not know | your reading of the manifest and the runtime's would differ |
| a capability name this build does not know | an older host must never silently drop a newer grant |
| `wire_version` other than `1` | a host and an artifact from different versions must not guess |
| a route outside `prefix` | the prefix is a containment boundary |
| `prefix = "/"`, `/{tenant}`, `/a//b`, `/a/../b` | a boundary that matches dynamically is not one |
| no `[[routes]]` | a sandboxed plugin serves exactly what it declares |
| a zero or oversized limit | a zero ceiling means "cannot run", not "no limit" |
| `memory_bytes × max_concurrency` over 1 GiB | that product, not its factors, is what the plugin can cost the host |
| a route path the router would refuse — `:id`, `*rest`, `{id`, `{*rest}/more` | `axum::Router::route` *panics* on one, so a manifest that passed would take the app down at boot |
| a module importing an allowlisted WASI name with the wrong signature, or exporting a `_start` that is not `() -> ()` | it would load and then fail on every request, as a gateway error nobody can explain from outside |
| a module with more than 4096 data + element segments, or more than 16 MiB of them | every request re-instantiates the module, and that copying happens before the first guest instruction — so it is bounded at load rather than discovered per request |
| a module that exports no linear memory named `memory`, or whose *initial* memory is already over the manifest's ceiling | every host function reads and writes through that export, so without it the plugin loads and then fails every request |
| two routes that are one route to the router (`/{a}` and `/{b}`) | same |
| a version or route path carrying a control character | both are printed on the consent screen, where an escape sequence can rewrite what you read |
| a digest that is not 64 lowercase hex characters | it is the only thing binding manifest to bytes |

These rules are enforced when a manifest is parsed *and* again when a host is
built from one, because `SandboxManifest`'s fields are public: a manifest that
was constructed or edited in Rust rather than parsed gets the same answer as one
read off disk.

---

## Installing

Review it first. `autumn plugin inspect` is a consent screen, and it exits
non-zero if the artifact is not fit to install:

```bash
autumn plugin inspect hello.autumn-plugin
```

```text
Sandboxed plugin: autumn-plugin-hello 0.1.0
  module sha256: 9f2c…
  mounts prefix: /hello
  routes it serves (and only these):
    GET /hello/{name}
  capabilities granted:
    http-request — handle HTTP requests routed to this plugin's own prefix (no other authority)
  denied, with no way to ask for it in this version:
    filesystem access, outbound network access, environment variables,
    database access, and any host authority not listed above
  resource ceilings per request:
    cpu 200000000 fuel units, memory 33554432 bytes, request body 1048576 bytes,
    response 4194304 bytes, at most 8 concurrent requests
  artifact sha256 (record this one): 4b7e…
  host functions it imports:
    wasi_snapshot_preview1::fd_read
    wasi_snapshot_preview1::fd_write
    …

✓ loads into this build's sandbox

Plugin conformance: autumn-plugin-hello — PASS
…
```

`--format json` gives the same verdict machine-readably, for a CI gate or a
review diff. The conformance section runs the same route-attribution,
route-prefix, route-collision, sensitive-surface and duplicate-registration
checks `autumn plugin-check` runs against a native plugin — offline, because a
sandboxed plugin's manifest *is* its route table.

Then mount it like any other plugin:

```rust
use std::path::Path;

use autumn_web::plugin_sandbox::SandboxedPlugin;

#[autumn_web::main]
async fn main() {
    let hello = SandboxedPlugin::from_file(Path::new("plugins/hello.autumn-plugin"))
        .expect("the sandboxed plugin loads");

    autumn_web::app()
        .routes(routes![index])
        .plugin(hello)
        .run()
        .await;
}
```

`from_file` verifies the module digest before the module is compiled, so a file
whose manifest and module have come apart is refused with a message naming the
mismatch.

Two digests appear on the review screen, and they answer different questions.
The **module sha256** is the one the manifest declares and the loader verifies:
it answers "are these the author's bytes". It is not the one to write down. What
you are reviewing is the grant as much as the code — the prefix, the routes, the
capabilities, the ceilings — and every one of those lives in the manifest, not
the module. Rewrite them and the module digest is still correct, because the
module really did not change, so an artifact reviewed under a narrow grant can be
deployed under a wide one and still match.

The **artifact sha256** covers the whole container, manifest included, and it is
the one `inspect` labels `record this one`. Reviewing an artifact means recording
that number and comparing it against what your deployment loads. The mount log
carries it too, as `artifact_sha256`, so "what did we agree to run" can be
checked against what was approved rather than merely read.

Either digest is a binding, not a signature: anyone who can rewrite the file can
recompute both. What the artifact digest buys is that the number you wrote down
covers everything you agreed to.
At mount time the resolved grant is written to the log at `info`, so "what did
we agree to run" is answerable from a production log alone.

The app still deploys as a single binary. `wasmi` is a pure-Rust interpreter —
no daemon, no subprocess, no native codegen backend, no extra artifact.

---

## What the runtime enforces

### The manifest is the mount

The router is built from the manifest's `[[routes]]`, one axum route per
declared pair. A request to an undeclared path under the prefix is a 404 the
guest never sees; a request outside the prefix never reaches the plugin's
router at all.

One implication is worth knowing before you write a guest: **a declared `GET`
also serves `HEAD`**. HTTP defines HEAD as GET without a body, and axum's method
router dispatches a HEAD with no HEAD route to the GET one — so the alternative
to naming it is not "HEAD is refused", it is "HEAD is served by an accident the
manifest never mentions". `autumn plugin inspect` and `autumn routes` both list
the implied HEAD, and your guest will see `"method":"HEAD"`; answer it as you
would the GET and the host discards the body for you.

### Resource bounds

| Ceiling | What it bounds | What happens at the edge |
|---|---|---|
| `fuel` | instructions executed for one request, **and** the host-side bytes copied on the guest's behalf — the module's data and element segments, which every request re-instantiates, and the request frame itself, which is cloned and base64-expanded into the guest's stdin — at 64 bytes per unit | 504 on the plugin's prefix |
| `memory_bytes` | one instance's linear memory | the guest's `memory.grow` fails; usually a trap, then 502 |
| `max_request_body_bytes` | the body forwarded in | 413, guest never started |
| `max_response_bytes` | the frame accepted back | 502 for an oversized frame; 504 for one that never ends |
| `max_concurrency` | instances alive at once | 503 with `Retry-After`; requests are shed, not queued |
| `request_body_timeout_ms` | how long a request may take to send its body | 408, and the permit goes back |

The number worth reviewing is `max_concurrency × the per-request footprint`,
where the footprint is the guest's linear memory **plus** every host buffer a
request pins outside it, counted at their simultaneous peak:

| Term | What holds it |
|---|---|
| `memory_bytes` | the guest instance's linear memory |
| `4 × max_request_body_bytes` | the body is buffered, cloned into the frame, and base64-expanded into the NDJSON line that becomes the guest's stdin — all live at once |
| table storage | the instance's tables, bounded to 16,384 references |
| `5 × max_response_bytes` | the response side peaks while the answer is *parsed*, not after: the raw NDJSON line the guest wrote is still live (up to `2 ×`), the base64 field may be copied out of it, and the decoded body is allocated while both are held |

A manifest with a tiny `memory_bytes` and 64 MiB body/response ceilings would
pass a memory-only check and still allocate hundreds of gigabytes, so the
validator checks the whole footprint against a 1 GiB ceiling.

The permit is held from the moment a request is admitted, *before* its body is
read — that is what makes the footprint a bound on the whole request rather than
on the part a guest is running. The cost of that choice is that a client
dribbling a body could otherwise hold a permit without ever starting a guest, so
the read has its own deadline. Unlike the interpreter, an async body read is
genuinely cancellable, so that is a real bound rather than a hopeful one.

Both of the host-side costs a request pays before the guest runs — instantiating
the module and encoding the request frame — are charged against `fuel` *before*
they are performed, so a manifest that declares a large body ceiling and almost
no budget is refused rather than served for free. A price is not a ceiling,
though, and the structurally expensive cases have one of their own at load: see
the segment limits above.

Each request gets a **fresh instance**, so no state survives a request and one
request's misbehaviour cannot reach the next.

### Fault isolation

The interpreter runs on a blocking worker, so a spinning guest never stalls the
async runtime, and every failure — trap, `proc_exit`, fuel exhaustion, refused
allocation, malformed frame, no answer at all — comes back as a value. Nothing a
plugin does can abort, exit, or panic the host process.

### What never crosses

Only an **allowlist** of request headers reaches the guest: `accept`,
`accept-charset`, `accept-encoding`, `accept-language`, `cache-control`,
`content-length`, `content-type`, `if-match`, `if-modified-since`,
`if-none-match`, `if-range`, `if-unmodified-since`, `range`, `user-agent`.
Everything else is dropped.

A denylist of credential headers is a losing game. The RFCs name `Cookie` and
`Authorization`, but every authenticating proxy invents its own —
`Cf-Access-Jwt-Assertion`, `X-Forwarded-User`, `X-Amzn-Oidc-Data`,
`X-Ms-Client-Principal`, `X-Goog-Iap-Jwt-Assertion` — and Autumn's own htmx
integration attaches `X-CSRF-Token` to every same-origin request. Each of those
is a bearer credential the sandbox promised would not cross, and there is no
version of that list that is finished. What a plugin actually needs is content
negotiation and conditional-request metadata, which is short and does not grow.

On the way back, an **allowlist** — because a deny-list is a blocklist against
a header registry that keeps growing. A plugin may set `content-type`,
`content-language`, `content-disposition`, `cache-control`, `etag`, `expires`,
`last-modified`, `location`, `retry-after`, `vary`, `age`, `accept-ranges` and
`content-range` — and nothing else. There is deliberately **no** `x-` escape
hatch: `X-Accel-Redirect` (nginx) and `X-Sendfile` (Apache) make your *reverse
proxy* serve an internal URI or a local file, so a hatch there would hand a
filesystem-free plugin the filesystem one hop upstream. Everything outside the
list is stripped and logged as a denial — `set-cookie` (a plugin that could set
a cookie could forge a session in your origin), `strict-transport-security` and
`clear-site-data` (origin-wide and persistent), the framing headers your HTTP
stack owns, and the host's own `x-autumn-sandboxed` / `x-content-type-options`,
which a plugin must not be able to forge. A header name or value carrying `\r\n` is refused outright — that
would be response splitting.

The **content type** is part of the same boundary, and it is the one most worth
understanding. A response is served from *your* origin, so an
`application/javascript` body under a default `script-src 'self'` is script
execution in your origin, and a `text/html` body is a document in it. Neither is
a capability this slice grants and neither can be made safe by a header your own
security middleware is entitled to overwrite. So the first slice serves data,
not documents:

| Allowed | Refused |
| --- | --- |
| `text/plain`, `text/csv` | `text/html`, `application/xhtml+xml` |
| `application/json` | `application/javascript`, `text/css` |
| `application/octet-stream` | `image/svg+xml` (a document that can script) |
| `image/png`, `image/jpeg`, `image/gif`, `image/webp`, `image/avif` | everything else |

Anything else is a 502 with the type named in the log. Widening this is a later
slice's job, and the honest way to do it is to serve a plugin's documents from
an origin of their own.

Every response — including the 413, the 503 and the 502 the guest never ran for
— carries `x-autumn-sandboxed: <plugin-name>` and
`x-content-type-options: nosniff`, and a response with no `content-type` gets
`application/octet-stream` rather than being left for a browser to guess at.

### Denials are observable

Every refused reach is recorded and logged at `warn` with the capability class
and the operation:

```text
WARN sandboxed plugin was denied a capability it reached for
     plugin="autumn-plugin-hello" capability="filesystem" operation="path_open"
     detail="a sandboxed plugin has no filesystem"
```

Denials are deduplicated per request, so a guest calling `path_open` in a loop
produces one line, not a flood. Capability denials (#1632) are deduplicated the
same way, by `(operation, outcome)` — and every one of them is *counted* in the
activity log even when the line is not repeated. A Rust guest's `std` touches `environ_sizes_get`
during start-up, so an `environment` denial per request is normal for one and
means exactly what it says: it asked, and it did not get.

The log is a surface the plugin can write to, so it is bounded like every other
one. Anything a guest influenced — its own error `detail`, its stderr, the
interpreter's account of a trap — is truncated to 512 characters and has its
control characters escaped before it is logged. A plugin that fails on every
request cannot fill a disk with one, and a `detail` containing a newline or an
ANSI escape reads as text that tried to forge a record rather than as one.

---

## The capability vocabulary

A plugin that can only answer HTTP is a demo. Real ones need to read data, call
an API, queue work and put something on a page — so `capabilities` names five
more words, and `[grants]` says what each is scoped to:

```toml
capabilities = ["http-request", "kv", "http-outbound", "db", "jobs", "render"]

[grants]
hosts     = ["api.example.com"]   # http-outbound may call exactly these
tables    = ["orders"]            # db owns exactly these, tenant-scoped
job_types = ["reindex"]           # jobs may enqueue exactly these
slots     = ["order-summary"]     # render may fill exactly these

[quotas]
kv_reads       = 64               # per request; every quota is operator-set
outbound_calls = 4
```

| Capability | What it grants | What it can never reach |
| --- | --- | --- |
| `kv` | a key/value namespace private to (this plugin, this tenant) | another plugin's or tenant's keys |
| `http-outbound` | the hostnames in `[grants].hosts`, through the framework client | any other host; redirects are not followed for it |
| `db` | the plugin-owned tables in `[grants].tables`, scoped to the active tenant | host-application tables, another tenant's rows, raw SQL |
| `jobs` | enqueueing the job types in `[grants].job_types` | any other type; the record carries this plugin and tenant, so a runner cannot widen it |
| `render` | the host-declared slots in `[grants].slots` | script, style, event handlers, off-origin links |

The grant table and the capability list must agree in **both** directions. A
`hosts` list without `http-outbound` is refused, because the operator read "no
outbound network" in one place and `api.example.com` three lines below it. A
`db` grant naming no table is refused too, because it is authority the consent
screen shows and the runtime can never honour.

### How a plugin asks

Over the channel it already has. A guest writes a call frame to stdout and reads
the answer from stdin — the same NDJSON dialogue it answers requests on:

```text
guest → host  {"op":"call","call":"kv-get","id":1,"key":"cart"}
host  → guest {"op":"call_result","status":"ok","id":1,
               "value":{"kind":"value","value":"one item","found":true}}
guest → host  {"op":"call","call":"http-fetch","id":2,"method":"GET",
               "url":"https://api.example.com/v1/orders"}
host  → guest {"op":"call_result","status":"denied","id":2,
               "reason":"quota-exceeded","detail":"…"}
guest → host  {"op":"response","status":200,…}
```

### What an outbound call may carry

Methods: `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`. Request headers are an
allow-list — `accept`, `accept-language`, `content-type`, `idempotency-key`,
`if-match`, `if-none-match`, `user-agent` — and so are the response headers that
come back: `content-type`, `etag`, `last-modified`, `location`, `retry-after`.
`Host`, `Cookie`, `Authorization` and the `Forwarded`/`X-Forwarded-*` family are
absent on purpose: the first re-points the request past the allow-list at the
connection layer, the next two would carry credentials the sandbox never gave
the plugin, and the last let it forge the provenance of a call the host is
making on its behalf.

**Redirects are never followed for a plugin.** A 3xx comes back as a 3xx. A
granted host that answers `302 Location: https://attacker.test/` would otherwise
walk the request straight out of the allow-list, so the host also re-checks
where the bytes actually came from before the guest sees any of them.

**Capabilities are data, not code.** A plugin granted `db` imports exactly what
a plugin granted nothing imports — `fd_read` and `fd_write`. Growing the
vocabulary never widened the module's import surface, which is why the consent
screen's import list still means what it meant.

Framing: `call` is the **one non-terminal frame** — every other frame ends the
exchange, and this one suspends it. The answer arrives as one NDJSON line
appended to the guest's stdin; the `id` is the guest's to choose and is echoed
back. **Read each answer before making the next call**: a guest that writes calls
and never reads them fills a bounded queue and the request is ended.

A `call_result` carries one of six value kinds: `done` (a write or delete),
`value` (from `kv-get`, with a `found` flag so a stored `null` is not a miss),
`row-id` (from `db-insert`), `rows` (from `db-get` and `db-query`), `http`, and
`job-id`. A `db-query` with `limit: 0` — or no `limit` — returns as many rows as
the `db_rows` quota allows, **and as many as fit 512 KiB**: rows are the one
result whose size the guest chooses, so the byte budget travels into the store
rather than being applied once the whole answer has been built. When it stops an
answer short, `rows` comes back with `"truncated": true` — read it, or a plugin
paging through its own table will read a short page as the end of the table. A
single row is bounded at 256 KiB across its columns when it is *written*, so a
row that was stored can always be read back.

A refusal comes back as a `call_result` the guest can read, not as a trap. A
plugin that hits a ceiling should degrade — render the panel without the live
number — and its author needs to see *which* rule refused:
`capability-not-granted`, `not-in-grant`, `quota-exceeded`, `malformed`,
`unavailable` or `backend-error`.

### Why cross-tenant access is unspellable

The guest names a **logical** thing; the host derives the physical one:

| The guest says | The host uses |
| --- | --- |
| `key: "cart"` | `plugin-kv:<plugin>:<tenant>:cart`, every segment escaped |
| `table: "orders"` | `plugin_<escaped plugin>__orders`, filtered by the active tenant |
| `job_type: "reindex"` | a `PluginJob` stamped with this plugin and this tenant |

The plugin half is hex-escaped rather than tidied, so
`autumn-plugin-shop` owning `orders` is `plugin_autumn_2dplugin_2dshop__orders` —
which looks odd in a schema and is the point: folding punctuation would let a
plugin *named* `shop_orders` owning `v2` land on the table `shop` owning
`orders_v2` already has.

There is no field in the protocol where a tenant, another plugin, or a physical
table name would go. Cross-tenant access is not denied — it cannot be written
down. That is also why a row may not carry a `tenant_id` column: a row that
could set it would be a row that chooses its own tenant. A `row_id` column *is*
accepted and ignored — the id is the row's address and travels in its own field
— so the row you just read from `db-get` writes straight back through
`db-update` without stripping anything.

### Render hooks are trees, not HTML

A granted plugin fills a slot by returning a fragment *tree*, which the host
renders:

```text
guest → {"op":"fragment","nodes":[
           {"node":"element","tag":"p","attributes":[["class","panel"]],
            "children":[{"node":"text","text":"3 orders"}]}]}
host  → <p class="panel">3 orders</p>
```

Not "the guest sends HTML and the host sanitises it". Sanitising is a filter in
front of a parser, and the notable sanitiser bypasses of the last decade have
been *parser differentials* — the filter's HTML parser and the browser's
disagreeing about one input. There is no parser here, so there is nothing to
disagree with: the tag list, the attribute list and the escaping are a function
this framework writes. Nothing rendered needs `unsafe-inline`, so a host page
with a nonce-based CSP keeps it.

A hook that traps, runs out of fuel, answers with a tag the renderer will not
emit, or overruns `render_bytes` produces **no fragment and no error** — the host
omits it and serves the page. A plugin's failure is never the page's.

Both halves have to agree on a slot. The *manifest* names the slots the plugin
will fill, which is what an operator approves; the *application* declares the
slots that exist, which is what stops a plugin appearing somewhere the app never
offered:

```rust,ignore
use std::sync::Arc;
use autumn_web::plugin_sandbox::RenderSlots;

// `SandboxedPlugin` is `Clone`, and a clone shares the host, the permits and
// the activity log — one plugin with one ceiling, registered twice.
let slots = RenderSlots::declaring(["order-summary"]).with(Arc::new(plugin.clone()))?;

// ...in the order-page handler, where `id: String`:
let extra = slots.render("order-summary", &[("order".to_owned(), id)]).await;
```

`with` refuses at boot when a manifest names a slot this app does not declare,
rather than leaving an operator to wonder later why nothing appears. `render`
returns a `String` and never a `Result` — every failure contributes nothing and
is logged.

### Quotas, consent and audit

Every capability carries a per-request ceiling, plus a `calls` budget that bounds
their sum and a `calls_per_second` ceiling shared across a plugin's requests.
Exceeding one denies that call and records it; it does not fail the request, and
it does not touch another plugin or any host route.

An upgrade is the moment a plugin's authority can grow without anyone looking,
so review it as an upgrade:

```bash
autumn plugin inspect shop-0.2.autumn-plugin --against shop-0.1.autumn-plugin
```

It prints exactly what the new manifest asks for that the approved one did not —
new capabilities, hosts, tables, job types, slots, raised quotas — and **exits
non-zero** when there is anything, so an unattended install stops rather than
consenting on your behalf. Asking for *less* is not a prompt.

For "what did this plugin do in the last hour", every capability call — allowed,
denied or over quota — is recorded at the point it happens, into a bounded
per-request ledger. A plugin writes its own quotas, so the ceiling is reachable
by one that wants to reach it: when it is, the summary says so in a line above
the counts, and every number below it is a floor rather than a total.

```rust,ignore
let summary = plugin.activity().summary("shop", Duration::from_secs(3600));
println!("{summary}");   // `Display`: the summary already knows its plugin and window
```

```text
sandboxed plugin `shop` — last 3600s
  performed:
    db-insert × 12
    http-fetch × 3
  denied:
    job-enqueue × 1
  hosts called:
    api.example.com × 3
  targets refused:
    drain-accounts × 1
```

Records carry the *shape* — the key, the table, the host, the job type — and
never a value. An audit surface that logged what a plugin stored would be a
second copy of the tenant data this whole subsystem exists to contain.

### Wiring the backends

Capabilities are honoured against backends you supply, so a host that wires none
still runs a granted plugin — its calls are answered `unavailable` and recorded,
which is a refusal like any other rather than a silent success:

```rust,ignore
use std::sync::Arc;
use autumn_web::plugin_sandbox::{CapabilityServices, KvStore, MemoryKvStore, SandboxedPlugin};

// The `as Arc<dyn KvStore>` is load-bearing: unsizing does not pass through
// `Option`, so `Some(MemoryKvStore::new())` will not coerce on its own.
let plugin = SandboxedPlugin::from_file(path)?.with_services(CapabilityServices {
    kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
    ..CapabilityServices::none()
});
```

The tenant is not one of them: it is resolved per request from the tenancy
middleware, because one compiled plugin serves every tenant.

---

## Limits of this slice

- Sessions, mail and file storage are not "not granted" — they do not exist as
  grantable capabilities, so no manifest can ask for them.
- A plugin's DB and KV backends are whatever the host wires. The framework ships
  in-process reference implementations that enforce the scoping exactly as a
  scoped statement would; a durable Postgres-backed store is the operator's to
  supply through `PluginStore`.
- Outbound calls do not follow redirects on a plugin's behalf, and the allow-list
  is a *name* list: IP literals and single-label names are refused, so
  `localhost` and `127.0.0.1` cannot appear in `[grants].hosts` and a plugin
  cannot be pointed at a local upstream in development. IP-range (SSRF) guarding
  for the app-level client is #1627's.
- The `jobs` capability **enqueues**; nothing in this slice runs the result. The
  record carries the enqueuing plugin and tenant, so a runner built on it cannot
  widen the grant — but there is no wire frame for delivering a job back into a
  guest, and adding one is a later slice.
- The shipped `PluginStore`, `JobSink` and `OutboundHttp` implementations are
  in-process: `MemoryPluginStore` and `MemoryJobSink` are bounded — the job
  queue holds `DEFAULT_JOB_DEPTH` and there is no unbounded spelling, because
  this slice ships no consumer that drains it — and
  `RecordingHttp` answers from a fixed table and is a test double. Wiring a real
  upstream means implementing `OutboundHttp` against the framework client and
  honouring the contract on the trait.
- Upgrading a sandboxed plugin needs an app restart.
- There is no registry; you get artifacts the same way you get any other file.
- An interpreter is slower than native code. For a hello-world route the
  overhead is sub-millisecond, but a sandboxed plugin is not where compute-heavy
  work belongs.
- Existing first-party plugins stay on the native `Plugin` trait.
- Nothing stops a manifest declaring a prefix that shadows a namespace your app
  already uses. Your own static routes still win, but unmatched paths under it
  become the plugin's. The prefix is on the consent screen for exactly this
  reason — read it.

## See also

- [Plugins](../plugins.md) — the native, full-trust plugin trait
- [Edge Capsules](./edge.md) — the other wasm lane: read-path routes compiled
  into a CDN-runnable artifact
