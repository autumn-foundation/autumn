# System Tests

Autumn ships first-class _system tests_: integration tests that drive a real
headless Chromium browser against your running application. Unlike HTTP-level
integration tests (which see response bodies but not what the browser does
_after_ a response lands), system tests verify the full browser stack — htmx
swaps, form flows, SSE-driven DOM updates, and navigation — exactly as a real
user would experience them.

```
┌─────────────────────────┐
│   cargo test --features │
│      system-tests       │
└────────────┬────────────┘
             │
     ┌───────▼────────┐     CDP        ┌──────────────┐
     │ SystemTest API │ ◄───────────── │  Chromium    │
     └───────┬────────┘                └──────────────┘
             │ HTTP
     ┌───────▼────────┐
     │ Autumn app     │  (ephemeral port, in-process)
     └────────────────┘
```

---

## Quick start

### 1. Install Chromium

```bash
# Ubuntu / Debian / GitHub Actions ubuntu-latest
sudo apt-get install -y chromium-browser

# macOS (Homebrew)
brew install --cask chromium

# Or set a custom binary path
export AUTUMN_CHROMIUM=/path/to/chrome
```

### 2. Add the feature flag

In your app's `Cargo.toml`:

```toml
[dev-dependencies]
autumn-web = { version = "0.7", features = ["system-tests"] }

[features]
system-tests = ["autumn-web/system-tests"]
```

> **Note:** The `[features]` entry is required because generated tests are guarded
> by `#[cfg(feature = "system-tests")]` and run commands pass `--features system-tests`.
> `autumn generate system-test` adds both sections automatically.

### 3. Generate your first test

```bash
autumn generate system-test TodoFlow
```

This creates `tests/system/todo_flow.rs`:

```rust
#![cfg(feature = "system-tests")]

use autumn_web::prelude::*;
use autumn_web::system_test::SystemTest;

#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn todo_flow_index_renders() {
    let runner = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("system test runner");

    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit /");
    page.expect_text("TodoFlow").await.expect("page title visible");
}
```

### 4. Run it

Generated tests are marked `#[ignore]` by default. Pass `-- --include-ignored`
to actually run them:

```bash
# Run all system tests (requires Chromium)
cargo test --features system-tests -- --include-ignored

# Run a specific test file
cargo test --features system-tests --test todo_flow -- --include-ignored
```

---

## Page API

All `Page` methods return `Result<&Self, SystemTestError>` and can be chained.

| Method | Description |
|--------|-------------|
| `page.visit(path)` | Navigate to a relative path |
| `page.fill(selector, value)` | Fill a form input |
| `page.click(selector)` | Click an element (CSS selector) |
| `page.expect_text(text)` | Assert text appears in the DOM |
| `page.expect_url(pattern)` | Assert URL contains pattern |
| `page.expect_attribute(sel, attr, value)` | Assert element attribute value |
| `page.snapshot()` | Save a PNG screenshot to artifact dir |
| `page.expect_hx_settle()` | Explicitly wait for htmx to finish |
| `page.expect_sse_event(id, predicate)` | Wait for SSE content in DOM |

### Transient navigation errors

Assertion helpers (`expect_text`, `expect_url`, `expect_attribute`,
`expect_sse_event`) poll by evaluating JavaScript on the page. When an action
triggers a full-page navigation — a submit button that redirects, say — Chrome
destroys the JS execution context that an in-flight evaluation was targeting,
and CDP reports errors such as `Cannot find context with specified id` or
`Inspected target navigated or closed`.

These are treated as "not ready yet": the helper keeps polling until its
deadline instead of failing the test on the first unlucky tick. Genuine CDP
errors (`No node with given id found`, for example) still surface immediately,
so a real failure is never converted into a silent timeout.

---

## Custom middleware

Use `.layer(...)` to register app-wide middleware on the router the test
serves. It accepts the same Tower layers as
[`AppBuilder::layer`](middleware.md) and applies them in the same position:

```rust
async fn scope_to_tenant(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(TenantId("acme-corp"));
    next.run(req).await
}

let runner = SystemTest::new()
    .routes(routes![index, create_todo])
    .layer(axum::middleware::from_fn(scope_to_tenant))
    .build()
    .await
    .unwrap();
```

When `.layer()` is called more than once the **first** call is the outermost
layer on ingress — it sees the request first and the response last, matching
`AppBuilder::layer`.

The layer sits inside Autumn's request-ID and session layers and outside
CSRF/CORS, so middleware that reads the request ID or session finds them under
test exactly as in production. (The CSRF layer is off by default in the
harness, so "outside CSRF" only matters when you supply a config via
`.state(...)` that re-enables it.) Layers apply on both the default-state path
and the `.state(...)` override path.

Without this you would have to map each layer onto individual handlers before
passing them to `.routes()`, which tests a stack the real app never serves.

---

## htmx auto-waiting

`click()` automatically waits for htmx to finish settling before returning.
This is implemented by polling `document.querySelectorAll('.htmx-request').length === 0`
with a 2 s default timeout. You can tune it:

```rust
SystemTest::new()
    .routes(routes![index, action])
    .hx_settle_timeout(Duration::from_secs(5))
    .build()
    .await
```

Use `expect_hx_settle()` for an explicit fence after custom JavaScript triggers:

```rust
page.evaluate("htmx.trigger('#myForm', 'submit')").await?;
page.expect_hx_settle().await?;
page.expect_text("Saved").await?;
```

### SSE helper

```rust
page.expect_sse_event("notifications", |text| text.contains("Hello")).await?;
```

`stream_id` can be a bare id (`"notifications"` → `#notifications`) or a
full CSS selector (`"#notifications"`, `".sse-target"`).

---

## Failure artifacts

On any assertion failure, autumn writes two files to
`target/system-tests/<test-name>/`:

| File | Description |
|------|-------------|
| `<label>.png` | Full-page screenshot at the moment of failure |
| `<label>.html` | Complete HTML of the page at the moment of failure |

```
target/system-tests/
└── expect_text/
    ├── expect_text.png
    └── expect_text.html
```

Override the output directory:

```rust
SystemTest::new()
    .artifact_dir("/tmp/my-artifacts")
    .build()
    .await
```

---

## Browser resolution

The harness looks for Chromium in this order:

1. `AUTUMN_CHROMIUM` environment variable (full path)
2. `PLAYWRIGHT_BROWSERS_PATH` directory (scans `chromium-*/`)
3. `PATH` lookup for `chrome`, `google-chrome`, `google-chrome-stable`,
   `chromium`, `chromium-browser` (`.exe` on Windows)
4. Well-known install locations:
   - `/usr/bin/chromium-browser` (Ubuntu/Debian)
   - `/usr/bin/chromium`
   - `/usr/bin/google-chrome`
   - `/usr/bin/google-chrome-stable`
   - `/snap/bin/chromium`
   - `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` (macOS)
   - `/Applications/Chromium.app/Contents/MacOS/Chromium` (macOS)
   - `%ProgramFiles%`, `%ProgramFiles(x86)%` and `%LOCALAPPDATA%` under
     `Google\Chrome\Application\chrome.exe` (and the `Chrome Beta` /
     `Chromium` variants) on Windows

On Linux and macOS each candidate is confirmed with a `--version` probe run
against its own throwaway `--user-data-dir`, so a Chrome already running on
the host cannot abort the probe on a locked profile directory. A binary that
exits 0 without printing anything (a launcher shim, say) is still accepted.

**Windows:** `chrome.exe` is a GUI-subsystem binary — it writes nothing to the
parent console, and `--version` is not an early-exit switch there, so running
it starts a real browser rather than answering. The harness therefore does not
execute the candidate on Windows at all: an existing `.exe` at a known
location is accepted, and `autumn doctor` reports `version unavailable`
instead of falsely claiming no browser is installed. Windows install
locations (`%ProgramFiles%`, `%ProgramFiles(x86)%`, `%LOCALAPPDATA%` under
`…\Application\chrome.exe`) are searched alongside the Unix ones.

`autumn doctor` and the `SystemTest` runner share this resolution code, so the
two can never disagree about whether this host can run system tests.

Run `autumn doctor` to check whether a browser is detected:

```
🍂 autumn doctor

  ✅ system_test_browser  Chromium for system tests: Chromium 122.0.6261.111 (/usr/bin/chromium-browser)
```

---

## Test isolation

Each test boots an independent app server on an ephemeral port; the server is
shut down when the `SystemTestRunner` is dropped. Database state is **not**
automatically isolated — use the same teardown patterns from the
[integration tests guide](testing.md):

```rust
// Explicit truncate before each browser test
async fn truncate_todos(pool: &Pool<AsyncPgConnection>) {
    diesel::delete(todos::table).execute(&mut *pool.get().await.unwrap()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn add_todo_flow() {
    let db = TestDb::shared().await;
    truncate_todos(&db.pool()).await;

    let state = AppState::for_test()
        .with_pool(db.pool())
        .with_profile("test");

    let runner = SystemTest::new()
        .routes(routes![index, create_todo])
        .state(state)
        .build()
        .await
        .unwrap();

    let page = runner.page().await.unwrap();
    // ...
}
```

Transaction-based rollback (from [#807](https://github.com/autumn-foundation/autumn/issues/807))
is not yet available for system tests because the browser drives a real HTTP
server on a separate TCP connection; per-test transaction wrapping would
require a proxy layer not yet shipped.

---

## GitHub Actions setup

```yaml
name: System Tests

on: [push, pull_request]

jobs:
  system-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Install Chrome
        # browser-actions/setup-chrome works reliably on all ubuntu-latest
        # versions. Alternatively, Google Chrome is pre-installed on GitHub
        # runners and the harness finds it at /usr/bin/google-chrome
        # automatically — you can omit this step if that binary is present.
        uses: browser-actions/setup-chrome@latest
      - name: Run system tests
        run: cargo test --features system-tests -- --include-ignored
        env:
          RUST_LOG: info
```

> **Tip:** Rust runs integration tests in parallel by default. If your system
> tests share state (e.g. a single test database or process-global store),
> pass `--test-threads=1` to serialise them. If they are fully isolated you
> can raise concurrency with `--test-threads=N`; see _Test isolation_ above.

---

## Parallelisation caveats

- Each test spawns its own browser **page** (tab), not a new browser process.
  The `SystemTestRunner` holds one `Browser` instance; call `runner.page()`
  multiple times for concurrent tabs within a single test.
- Across tests, each `SystemTest::build()` launches a **separate** browser
  process, which can be expensive. For many tests in one binary, consider
  sharing a browser via a `tokio::sync::OnceCell<SystemTestRunner>`
  (`page()` takes `&self` so no mutex is needed).
- `--test-threads=1` is the safest default for system tests that share a
  single test database.

---

## Integrating with `autumn generate scaffold`

When you run `autumn generate scaffold Post title:String body:Text`, the
scaffold generator also emits a smoke test in `tests/<model>.rs`. A companion
happy-path system test is **not** generated by default (to avoid requiring
Chromium in basic scaffolding workflows), but you can add one immediately:

```bash
autumn generate system-test Post
```

---

## Checking browser availability

```bash
autumn doctor
# or the dedicated check:
autumn system-test check   # planned; use `autumn doctor` today
```
