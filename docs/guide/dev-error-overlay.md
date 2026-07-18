# Dev Error Overlay

When a handler panics, returns an `Err`, or propagates a `?`, Autumn's dev
profile renders a rich browser overlay instead of a plain 500 page. The overlay
gives you everything you need to diagnose the failure without leaving the tab.

Autumn ships two overlays that share the same dark, red-accented visual
language:

- The **runtime error overlay** (below) for errors raised while a request is
  being served.
- The **compile-error overlay** (see [Compile-error overlay](#compile-error-overlay))
  for `cargo build` failures during `autumn dev`.

## What it shows

| Section | Contents |
|---------|----------|
| **Error** | Status code, reason phrase, and the error message |
| **Stack trace** | Parsed Rust frames; workspace frames are expandable with ~10 lines of source context around the failing line |
| **Request** | Method, path, matched route pattern, request ID, query string, path params |
| **Headers** | Scrubbed request headers (sensitive keys replaced with `[FILTERED]`) |
| **Cookies** | Parsed session cookies, scrubbed by the same rules as headers |
| **SQL Queries** | Statements, bind counts, and durations — populated by autumn-harvest when present |

## Activation

The overlay is active automatically when:

1. The **dev profile** is in use (`AUTUMN_ENV=dev` or `--profile dev`, or when
   running `cargo run` without an explicit profile, which defaults to dev).
2. The request's `Accept` header prefers HTML (browser navigation).

API clients (`Accept: application/json`) always receive RFC 7807 Problem Details
regardless of profile.

The overlay is **never** shown in production. Two independent guards enforce
this: a runtime profile check and a `#[cfg(debug_assertions)]` guard on
backtrace capture. See [ADR 0006](../adr/0006-dev-error-overlay.md) for the
full reasoning.

## Triggering the overlay

### Handler `Err` return

```rust
#[get("/posts/{id}")]
async fn get_post(Path(id): Path<i32>) -> AutumnResult<Markup> {
    if id < 0 {
        return Err(AutumnError::bad_request_msg("id must be positive"));
    }
    // ...
}
```

Hit `/posts/-1` in a browser — the overlay pops up showing the bad-request
error, the request path, and (if autumn-harvest is wired up) any queries that
ran before the error.

### `?` propagation

```rust
#[get("/data")]
async fn load_data(db: Db) -> AutumnResult<Markup> {
    let rows = db.run(|conn| load_all(conn)).await?;  // ? becomes AutumnError
    // ...
}
```

Any `std::error::Error` propagated via `?` is wrapped as a 500. The overlay
shows the backtrace captured at the conversion point, with source context for
workspace frames.

### Intentional test route (e.g. `examples/reddit-clone`)

The `examples/reddit-clone` app ships a `/dev/trigger-error` route that panics
on purpose. Visit it in `cargo run -p reddit-clone` to see the overlay in action:

```
GET http://localhost:3000/dev/trigger-error
```

The route is registered only when the dev profile is active; it returns 404 in
production.

## Opt-out

If you prefer the plain 500 page without the badge overlay, set the profile to
production:

```toml
# autumn.toml
[app]
profile = "production"
```

Or at runtime:

```sh
AUTUMN_ENV=production cargo run
```

You can also provide a custom `ErrorPageRenderer` that renders whatever HTML you
prefer — the badge is only injected by the default pipeline when `is_dev` is
true.

## Sensitive parameter filtering

The overlay scrubs headers and cookies using the same `ParameterFilter` rules
configured in `autumn.toml`:

```toml
[log]
filter_parameters = ["pin", "ssn"]        # add to default list
unfilter_parameters = ["authorization"]   # remove from default list
```

Default scrubbed keys include `password`, `token`, `secret`, `authorization`,
`api_key`, `access_token`, `cookie`, and others. See
[Logging PII](logging-pii.md) for the full list.

## SQL queries (autumn-harvest)

When `autumn-harvest` is in the dependency graph, it pushes query records to the
overlay via the `DevBadgeContext.sql_queries` field. Each record shows the SQL
statement, bind parameter count, and duration in milliseconds. The overlay shows
a "SQL Queries (N)" section only when at least one query was recorded.

Without autumn-harvest the section is hidden; no configuration is needed.

## Source context

For stack frames inside the project workspace (relative file paths or absolute
paths inside the current directory), the overlay reads the source file from disk
and shows ±5 lines around the failing line, with the failing line highlighted
in red.

**Requirements:**
- The source files must be present on the same machine as the running process
  (true for local dev; not true for container builds where source is absent).
- The binary must be built in debug mode (`cargo run` or `cargo build`).

When source files are absent, the overlay still shows the full stack trace
(file, line, function name) without the inline code context.

## Compile-error overlay

The runtime overlay above only helps once the app is *running*. But during
`autumn dev` the most common failure is a Rust file that no longer compiles —
and historically a failed rebuild left the browser staring at a
connection-refused error or a silently stale page, with the real compiler
errors buried in the terminal.

The **compile-error overlay** fixes that: when a rebuild triggered by
`autumn dev` fails, the previously-built binary keeps running and the browser
renders the compiler diagnostics as a full-screen overlay on top of the page
you were already looking at.

### What triggers it

A `cargo build` failure during an `autumn dev` watch cycle — for example,
saving a `src/**/*.rs` file that doesn't type-check. The dev orchestrator
rebuilds with `--message-format=json`, extracts every **error**-level
diagnostic (warnings are ignored), and writes them into the live-reload state
file. The injected live-reload client polls that state and paints the overlay.

### What it shows

- Every error diagnostic, **in the order the compiler emitted them**.
- For each one: the error code and primary message as a header, the
  `file:line:column` of the primary span, and the compiler's full *rendered*
  diagnostic (the same colored-caret block you see in the terminal, minus the
  color) in a monospace panel.
- A prominent banner at the top. When you are looking at a page served by the
  still-running previous binary, the banner reads:

  > Build failed — you're viewing a stale page. Fix the errors below and save.

  so it's clear the page underneath is out of date (see
  [Stale-page behavior](#stale-page-behavior)).

Compiler output is inserted with `textContent` only (never `innerHTML`), so
arbitrary diagnostic text can't inject markup, and the client is served as an
external script — it needs no inline JavaScript and passes the framework's
default `script-src 'self'` CSP.

### Described mockup

```
┌──────────────────────────────────────────────────────────────────────┐
│ ▎ Build failed — you're viewing a stale page. Fix the errors below    │
│   and save.                                                            │
│                                                                        │
│  ┌──────────────────────────────────────────────────────────────────┐ │
│  │ [E0425] cannot find value `user` in this scope                   │ │
│  │ src/routes/home.rs:42:17                                          │ │
│  │                                                                    │ │
│  │ error[E0425]: cannot find value `user` in this scope             │ │
│  │   --> src/routes/home.rs:42:17                                    │ │
│  │    |                                                               │ │
│  │ 42 |     let name = user.name;                                    │ │
│  │    |                ^^^^ not found in this scope                  │ │
│  └──────────────────────────────────────────────────────────────────┘ │
│  ┌──────────────────────────────────────────────────────────────────┐ │
│  │ [E0308] mismatched types                                          │ │
│  │ src/routes/home.rs:50:9                                           │ │
│  │ ...                                                                │ │
│  └──────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
```

### Auto-dismiss on green

The overlay clears itself automatically. Once you fix the errors and save, the
next rebuild succeeds, the server restarts, and the live-reload state advances
to a normal `full` reload with no `build_error` payload. The client removes the
overlay and reloads the page — you're back to a working app without touching the
browser.

### Stale-page behavior

On a failed rebuild the orchestrator deliberately **does not** stop the old
binary. It keeps serving the last good build so that:

- the browser can still reach the live-reload endpoint (that's how the overlay
  is delivered at all), and
- you keep your scroll position and page state while you fix the error.

The overlay's banner makes the staleness explicit so you don't mistake the
underlying page for the current code.

### Dev-profile-only

Like the runtime overlay, this is a development-only feature. The live-reload
client and its state endpoint are mounted only when `autumn dev` is running
(both `AUTUMN_DEV_RELOAD` and `AUTUMN_DEV_RELOAD_STATE` are set); a production
build never serves the overlay client and never writes a build-error state.

### Known limitation: cold start

The overlay requires a previously-built binary to be running so it can answer
the poll. If the **very first** build when you launch `autumn dev` fails, there
is no server up yet (and the CLI doesn't know which port your app would have
bound), so there is nothing for the browser to talk to. In that case the
compiler errors are printed to the terminal as before. Once any successful
build has started the server, subsequent failed rebuilds show the overlay.

### Known limitation: Windows

The stale-page serving described above is **not available on Windows**. Windows
keeps the running `target/debug/<app>.exe` locked while the process is alive, so
`cargo build` cannot relink over it. To rebuild at all, the dev orchestrator
must stop the old binary *before* running `cargo build` on Windows (Unix/macOS
build first and only stop once a fresh binary is ready).

The consequence: on Windows a **failed** rebuild leaves the app already stopped,
so there is no running server to answer the live-reload poll and paint the
overlay. You get the compiler errors streamed to the terminal plus the client's
normal reconnect behavior once a green build restarts the server. Unix and macOS
get the full stale-page overlay experience. This is a documented, non-regressive
platform limitation — Windows simply falls back to the pre-overlay terminal-only
behavior for failed rebuilds.

See [ADR 0006](../adr/0006-dev-error-overlay.md) for the design reasoning
behind the dev overlays and their production guards.
