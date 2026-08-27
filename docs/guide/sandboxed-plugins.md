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
| Database, sessions, auth | full access | none exist as grantable capabilities |
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
| two routes that are one route to the router (`/{a}` and `/{b}`) | same |
| a version or route path carrying a control character | both are printed on the consent screen, where an escape sequence can rewrite what you read |
| a digest that is not 64 lowercase hex characters | it is the only thing binding manifest to bytes |

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

`from_file` verifies the digest before the module is compiled, so a file whose
manifest and module have come apart is refused with a message naming the
mismatch. The digest is a binding, not a signature: anyone who can rewrite the
file can recompute it, so reviewing an artifact means **recording the digest
`inspect` printed** and comparing it against the one your deployment loads.
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

### Resource bounds

| Ceiling | What it bounds | What happens at the edge |
|---|---|---|
| `fuel` | instructions executed for one request, **and** host-side bytes copied on the guest's behalf, at 64 bytes per unit | 504 on the plugin's prefix |
| `memory_bytes` | one instance's linear memory | the guest's `memory.grow` fails; usually a trap, then 502 |
| `max_request_body_bytes` | the body forwarded in | 413, guest never started |
| `max_response_bytes` | the frame accepted back | 502 for an oversized frame; 504 for one that never ends |
| `max_concurrency` | instances alive at once | 503 with `Retry-After`; requests are shed, not queued |

`max_concurrency × memory_bytes` is the most memory this plugin can cost the
host at any instant. That product is the number worth reviewing.

Each request gets a **fresh instance**, so no state survives a request and one
request's misbehaviour cannot reach the next.

### Fault isolation

The interpreter runs on a blocking worker, so a spinning guest never stalls the
async runtime, and every failure — trap, `proc_exit`, fuel exhaustion, refused
allocation, malformed frame, no answer at all — comes back as a value. Nothing a
plugin does can abort, exit, or panic the host process.

### What never crosses

Stripped from the **request** before it reaches the guest: `cookie`,
`authorization`, `proxy-authorization`, `www-authenticate`,
`proxy-authenticate`, and the headers that are bearer tokens in practice even
though the RFCs do not call them credentials — `x-csrf-token`, `x-xsrf-token`,
`x-api-key`, `x-auth-token`, `api-key`. The CSRF one is load-bearing: Autumn's
own htmx integration attaches it to every same-origin request, so without it a
plugin route reached from one of your pages would be handed the caller's token.
The sandbox grants no session or auth capability, so a credential reaching a
plugin could only ever be a liability — and a plugin that echoed request headers
would leak one.

On the way back, an **allowlist** — because a deny-list is a blocklist against
a header registry that keeps growing. A plugin may set `content-type`,
`content-language`, `content-disposition`, `cache-control`, `etag`, `expires`,
`last-modified`, `location`, `retry-after`, `vary`, `age`, `accept-ranges`,
`content-range`, and any `x-`-prefixed header of its own. Everything else is
stripped and each strip is logged as a denial — `set-cookie` (a plugin that
could set a cookie could forge a session in your origin),
`strict-transport-security` and `clear-site-data` (origin-wide and persistent),
the framing headers your HTTP stack owns, and the host's own
`x-autumn-sandboxed` / `x-content-type-options`, which a plugin must not be able
to forge. A header name or value carrying `\r\n` is refused outright — that
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
produces one line, not a flood. A Rust guest's `std` touches `environ_sizes_get`
during start-up, so an `environment` denial per request is normal for one and
means exactly what it says: it asked, and it did not get.

---

## Limits of this slice

- The only capability is `http-request`. Database, sessions, mail, storage and
  outbound HTTP are not "not granted" — they do not exist as grantable
  capabilities, so no manifest can ask for them.
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
