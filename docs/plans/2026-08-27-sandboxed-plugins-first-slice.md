# Capability-Sandboxed WASM Plugins — First Slice Plan

> Planning record for issue #1609. Written before the code; kept as the design
> record for the slice. The narrative for users is
> `docs/guide/sandboxed-plugins.md`.

**Goal:** an Autumn app can load a third-party plugin as a portable, sandboxed
artifact whose manifest declares exactly one thing it may do — serve HTTP
requests under a declared prefix — and the runtime makes that declaration true:
deny-by-default ambient authority, hard CPU/memory ceilings, and fault
isolation, all in-process, still one binary.

---

## 1. Brainstorming — how could a framework sandbox a plugin at all?

| # | Option | Verdict |
|---|--------|---------|
| 1 | **wasmtime + WASI preview 2 / component model** | Richest typed boundary, JIT-fast. But it drags a large native codegen stack (cranelift) into every Autumn binary that enables the feature, needs a C-ish build story on some targets, and the component-model tooling is still moving. Rejected for slice 1; revisit when the capability vocabulary grows. |
| 2 | **`wasmi` (pure-Rust interpreter) + a hand-written, minimal WASI preview-1 shim** | Already in this workspace's `Cargo.lock` and already proven in-tree by `autumn-edge`'s reference host. Pure Rust, no codegen backend, embeds in the single binary, deterministic fuel metering and a store memory limiter out of the box. **Chosen.** |
| 3 | **OS process isolation** (subprocess + seccomp/landlock/jail) | Real isolation, but breaks the single-binary deploy promise, is per-OS, and adds IPC + lifecycle management. Rejected. |
| 4 | **Native dynamic loading** (`dlopen` a cdylib) | Zero isolation — this is the status quo with extra steps. Rejected. |
| 5 | **An embedded scripting language** (Rhai/Lua) | Sandboxable, but the artifact is source, not a portable compiled artifact, and it forecloses the "author your plugin in the language you like" promise. Rejected. |
| 6 | **Keep the native `Plugin` trait and audit crates instead** (#1600) | Catches *known* vulnerabilities only; cannot constrain what plugin code does. Complementary, not a substitute. |

**Chosen shape**, borrowing the parts of `autumn-edge` that already work:

- One compiled `wasmi::Module` per artifact; a **fresh store and instance per
  request**, so no state survives a request and no request can see another's.
- **NDJSON over the guest's stdio** as the whole ABI — one JSON frame in, one
  out. Human-readable, debuggable with `cat`, implementable from prose in any
  language that can target wasm, and it needs no bindgen.
- The guest's *entire* ambient authority is the list of host functions the
  linker defines. Anything the linker does not define cannot be imported.

## 2. Reverse brainstorming — how would we ship a sandbox that is really a door?

Each row is a way to fail; each is answered by a control and a test.

| # | How it fails | Control |
|---|--------------|---------|
| R1 | The WASI shim implements `path_open`/`fd_read` against the real filesystem | The shim has **no** filesystem implementation at all. Every fs call is a deny-stub returning `ENOTCAPABLE`, and records a denial. |
| R2 | A permissive linker silently satisfies an unknown import | Closed world. Imports are scanned against an allowlist **before** instantiation; anything else is refused at load, naming the import. |
| R3 | Env vars / args leak the app's secrets | `environ_get`/`environ_sizes_get`/`args_*` answer *empty* and record a denial. |
| R4 | Network egress via `sock_*` | Deny-stubs, denial recorded. |
| R5 | Database access via a bespoke import (`autumn:db/query`) | Not in the allowlist → refused at load (R2). No database seam exists in this slice, by construction. |
| R6 | Infinite loop hangs the host | Fuel metering; exhaustion is a trap, translated into a 5xx **on the plugin's prefix only**. |
| R7 | `memory.grow` bomb takes the host OOM | `StoreLimits` memory ceiling; the grow fails inside the guest. |
| R8 | Host-side buffers grow without bound (a guest writes 4 GiB of stdout with no newline) — unmetered by fuel and by the guest memory ceiling | Bounded pending-line budget; host copies run through a fixed scratch buffer, so a runaway write fails long before its length is copied. |
| R9 | A trap or `proc_exit` aborts the process | The guest runs on a blocking worker; a trap is a `Result`, and a host-side panic is caught at the join boundary. Never `abort`. |
| R10 | wasmi is synchronous, so a slow guest starves the async runtime | Execution is dispatched to `spawn_blocking`, and bounded by a concurrency permit so a flood of plugin requests cannot exhaust the blocking pool. |
| R11 | The plugin forges a session by returning `Set-Cookie` | Response headers pass an allowlist-shaped filter: `set-cookie`, hop-by-hop headers and framing headers are stripped, each strip recorded as a denial. |
| R12 | The plugin *reads* the user's session by echoing `Cookie`/`Authorization` | Credential headers are stripped from the request frame before it crosses the boundary. The plugin has no session capability, so a credential reaching it could only be a liability. |
| R13 | The manifest lies — declares `/safe` but the artifact serves `/admin` | The host mounts the module under the declared prefix and forwards nothing else; a declared route outside the prefix is a load-time refusal. |
| R14 | The artifact is swapped after review | The manifest carries the module's SHA-256; the loader verifies it and refuses a mismatch. |
| R15 | The operator never sees the grant | `autumn plugin inspect` is a consent screen; the loader logs the resolved grant at startup; `autumn plugin-check --sandbox-artifact` reports it alongside conformance. |
| R16 | A future manifest grants a capability this build does not understand, and the old build ignores it | Unknown capability names are a **hard load error**, not a warning. Fail closed. |
| R17 | An unbounded request body is copied into the guest | Per-request body ceiling; over it, the plugin's prefix returns 413 and the guest is never started. |

## 3. Six hats

**White (what is factually true here).** `wasmi` 0.40 is already resolved in
`Cargo.lock` and already carries a working reference host (`autumn-edge`'s
`host.rs`) with fuel, a store limiter and a deny-shaped WASI shim. The plugin
trait hands over the whole `AppBuilder`. `autumn plugin-check` reads a route
manifest from a built binary and checks attribution/prefix/collision. There is
no `autumn plugin add` command in the tree yet, so the consent surface has to be
one we ship here.

**Red (what people will feel).** An operator's fear is not "is this fast" — it
is "what can this thing reach". A binary artifact is opaque and scary; a
manifest they can read in ten seconds, plus a digest, is what converts fear into
a decision. A plugin author's fear is "I cannot debug across an ABI"; NDJSON
frames they can print answer that.

**Black (what will bite us).** An interpreter is 10–100× slower than native per
instruction — the p95 budget is real and must be measured, not assumed. A
hand-written WASI shim is a security surface: every function we add is a
potential hole, so the shim's job is to stay boringly small and to be readable
top to bottom. Two wasm hosts now exist in-tree (edge and plugin) and could
drift. And there is a moral hazard: a sandbox that is 95% right is more
dangerous than none, because people trust it.

**Yellow (what this buys).** Purely additive: the native `Plugin` trait and
every existing plugin are untouched, so there is no migration. The single-binary
deploy story survives. The capability vocabulary can grow one seam at a time
without changing the artifact format. And the sandboxed plugin passes the
*existing* conformance checks, so the ecosystem's contract does not fork.

**Green (ideas worth keeping).** Reuse the NDJSON dialogue from the edge lane —
one wire idea, two substrates. Make `ENOTCAPABLE` (76) the single universal
"no", so a guest author learns one errno. Keep a **denial ledger** per request:
a typed list, logged *and* returned to tests, which is what makes
"observable in logs" provable rather than aspirational. Write the adversarial
test guests in **WAT**, compiled at test time by the pure-Rust `wat` crate — so
the escape suite runs on any CI runner with no wasm toolchain installed.

**Blue (process).** Red → green → refactor, one AC at a time, bottom-up:
manifest → artifact → wire → host → plugin mount → CLI → docs. Then an
adversarial review pass. Then the AC evidence table.

---

## 4. Design

### 4.1 Artifact format (`.autumn-plugin`)

```text
0   "AUTUMNPL"          8 bytes, magic
8   u32 LE              container format version (= 1)
12  u32 LE              manifest length N
16  N bytes             manifest, UTF-8 TOML
16+N  …                 the wasm module, to EOF
```

Deliberately not a tar/zip: one dependency-free reader, and the manifest is at a
fixed offset so `head -c` shows it. The manifest records `sha256` of the module
bytes; the loader verifies it.

### 4.2 Manifest

```toml
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "…"

[[routes]]
method = "GET"
path = "/hello/greet"

[limits]
fuel = 200_000_000
memory_bytes = 33_554_432
max_request_body_bytes = 1_048_576
max_response_bytes = 4_194_304
max_concurrency = 8
wall_clock_ms = 5_000
```

Validation is fail-closed: `http-request` is the only capability this version
knows; every declared route must live under `prefix`; limits must be non-zero
and within host ceilings.

### 4.3 Wire (version 1)

```text
host → guest {"op":"request","wire_version":1,"granted":["http-request"],
              "method":"GET","path":"/hello/greet","query":"","headers":[…],"body_b64":""}
guest → host {"op":"response","status":200,"headers":[["content-type","text/plain"]],"body_b64":"aGk="}
guest → host {"op":"error","detail":"…"}          ; explicit self-reported failure
```

One frame in, one frame out, then the instance is dropped.

### 4.4 Where the code lands

| Path | What |
|------|------|
| `autumn/src/plugin_sandbox/manifest.rs` | manifest types, parse, fail-closed validation |
| `autumn/src/plugin_sandbox/artifact.rs` | container encode/decode + digest verification |
| `autumn/src/plugin_sandbox/wire.rs` | frames, header canonicalization, credential stripping |
| `autumn/src/plugin_sandbox/host.rs` | engine, per-request store, WASI deny-shim, denial ledger, limits |
| `autumn/src/plugin_sandbox/plugin.rs` | `SandboxedPlugin: Plugin` — mounts the prefix, declares routes, logs the grant |
| `autumn-cli/src/plugin_sandbox.rs` | `plugin package`, `plugin inspect`, `plugin-check --sandbox-artifact` |
| `docs/guide/sandboxed-plugins.md` | authoring → packaging → installing, and the trust model |

Behind a non-default `plugin-sandbox` feature on `autumn-web`, so the default
build and every existing app are byte-for-byte unaffected.
