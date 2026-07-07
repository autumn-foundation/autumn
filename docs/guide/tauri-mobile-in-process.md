# Mobile Apps with Tauri: In-Process Backend + Remote Database (`autumn generate tauri-mobile`)

`autumn generate tauri-mobile` scaffolds a `src-tauri/` sub-project that wraps
an existing autumn app in a **Tauri v2 mobile shell** for iOS and Android. It
implements the **in-process model** (issue #1507, "Option B"): the Autumn Axum
server runs on a background thread *inside the app process itself*, and the
webview loads the app from `http://127.0.0.1:<port>`. The database is a
**remote Postgres** instance reached over the device's network.

This page covers, in order:

1. why the desktop sidecar model cannot ship on mobile (sandboxing),
2. the in-process architecture the generator emits,
3. remote Postgres pool behavior under flaky mobile networks,
4. App Store / Google Play guideline compliance for this hybrid model,
5. the security model (loopback trust boundary, database TLS, failure modes),
6. scaffolding and building.

For **desktop** apps, use [`autumn generate tauri`](tauri.md) instead — the
sidecar model there bundles a managed local Postgres and needs no network.

## 1. Why the desktop sidecar model cannot ship on mobile

The desktop scaffold runs the autumn server as a **sidecar**: a separate
server binary, declared as `bundle.externalBin` in `tauri.conf.json`, spawned
and supervised by the Tauri shell via `tauri-plugin-shell`. None of that is
possible on mobile:

- **No process spawning.** The iOS app sandbox provides no usable
  `fork`/`exec` for shipping app code as child processes — an App Store app
  gets exactly one process (plus OS-managed extensions). Android technically
  allows `exec` of bundled binaries, but modern targetSdk rules (W^X
  enforcement, `untrusted_app` SELinux policy, the requirement that native
  executables live in the read-only APK library path) make a supervised
  server child fragile to impossible, and Google Play policy treats
  self-managed executable payloads as a red flag.
- **No external sidecars.** Tauri's `externalBin`/`.sidecar(...)` mechanism is
  desktop-only; there is no supported way to bundle and spawn a second
  executable on iOS or Android.
- **Single-process app lifecycle.** Mobile OSes suspend, resume, and kill the
  *app process* as a unit. A child server process would not receive lifecycle
  callbacks and would be killed out from under the shell at unpredictable
  times.

So on mobile the server cannot be a separate binary. It has to be a
**library** the app links and runs inside its own process — which is exactly
what this generator sets up. The scaffold therefore contains **no**
`stage-sidecar.sh`/`.ps1`, **no** `externalBin`, and **no**
`tauri-plugin-shell` dependency.

## 2. The in-process model

### Architecture

The generated `src-tauri/` crate builds as a library
(`crate-type = ["staticlib", "cdylib", "rlib"]` — staticlib for the Xcode
project, cdylib for the Android activity, rlib so `cargo tauri dev` still
works on your desktop) and depends on **your app crate by path, with
embedded assets** (`your-app = { path = "..", features = ["embed-assets"] }`
— on a device there is no on-disk `static/` directory, so CSS, htmx, and the
widgets JS are compiled into the binary). In `src-tauri/src/lib.rs` it:

1. binds `127.0.0.1:0` to pick a free loopback port,
2. configures the server via environment variables at the top of `run()`,
   **before** `tauri::Builder` starts any platform threads (`set_var` is only
   sound while the process has no foreign threads calling `getenv`); only the
   two values that need the sandbox data dir — the blob-storage root and the
   signing secret — are set inside `setup(...)`,
3. spawns the server on a dedicated OS thread with its own tokio runtime,
   from `tauri::Builder::default().setup(...)`,
4. polls `GET /health` until the server answers, then opens the webview at
   `http://127.0.0.1:<port>` (or a visible error page if the server never
   becomes ready).

Environment variables are the **whole** config story for the shell: it stages
no config files, and your app's `autumn.toml` / `autumn-<profile>.toml` are
**not read** when running under the mobile shell (config-file discovery keys
off the compile-time manifest dir of the `#[autumn_web::main]` entry point,
which the in-process `serve()` bypasses, and off the working directory, which
on a device is not your project). Anything you rely on from `autumn.toml`
must be repeated as an `AUTUMN_*` env var in the generated `lib.rs`. The
profile is pinned the same way: `AUTUMN_ENV=dev` in debug builds,
`AUTUMN_ENV=prod` in release builds.

### Annotated `src-tauri/src/lib.rs`

This is what the generator emits (abbreviated; `my_app` is your crate name):

```rust
use std::net::TcpListener;
use tauri::Manager;

// On iOS/Android the platform loads this library through the mobile entry
// point; src/main.rs keeps desktop `cargo tauri dev` working.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 1. Free loopback port: bind :0, read the port, drop the listener.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind a loopback port");
        listener
            .local_addr()
            .expect("failed to read the loopback port")
            .port()
    };

    // 2. Configure the in-process server through the environment — before
    //    tauri::Builder starts platform threads (set_var thread-safety).
    std::env::set_var("AUTUMN_SERVER__HOST", "127.0.0.1");
    std::env::set_var("AUTUMN_SERVER__PORT", port.to_string());
    // Remote Postgres — set your connection string (see sections 3 and 5):
    // std::env::set_var(
    //     "AUTUMN_DATABASE__URL",
    //     "postgres://app_user:PASSWORD@db.example.com:5432/app?sslmode=require",
    // );
    // Small pool for flaky mobile networks (see section 3):
    std::env::set_var("AUTUMN_DATABASE__POOL_SIZE", "2");
    std::env::set_var("AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS", "5");
    // Loopback-only webview traffic: plain HTTP, so Secure cookies must be off,
    // and Host: 127.0.0.1 must be trusted.
    std::env::set_var("AUTUMN_SESSION__SECURE", "false");
    std::env::set_var("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS", "127.0.0.1,localhost");
    // Blob storage into the sandbox; prod defaults it to "disabled".
    std::env::set_var("AUTUMN_STORAGE__BACKEND", "local");
    std::env::set_var("AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION", "true");
    // Dev profile under `cargo tauri … dev`, prod profile in release builds —
    // set explicitly, because in-process serve() bypasses the macro entry
    // point that would otherwise mark release builds.
    if cfg!(debug_assertions) {
        std::env::set_var("AUTUMN_ENV", "dev");
    } else {
        std::env::set_var("AUTUMN_ENV", "prod");
    }
    // ... clears inherited one-off mode flags (AUTUMN_BUILD_STATIC, …) ...

    tauri::Builder::default()
        .setup(move |app| setup(app, port))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn setup(app: &mut tauri::App, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    // 3. Sandbox-private data dir + a persisted per-install signing secret —
    //    the only env values that must wait for the tauri App handle.
    let data_root = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_root)?;
    let blobs_dir = data_root.join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;
    std::env::set_var(
        "AUTUMN_STORAGE__LOCAL__ROOT",
        blobs_dir.to_string_lossy().as_ref(),
    );
    let signing_secret = load_or_generate_signing_secret(&data_root)?;
    std::env::set_var("AUTUMN_SECURITY__SIGNING_SECRET", &signing_secret);

    // 4. The server thread: a dedicated OS thread parks on the server future
    //    for the lifetime of the app. `serve()` is your app's entry point,
    //    extracted into src/lib.rs by the generator (see below).
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for the in-process autumn server");
        runtime.block_on(my_app::serve());
    });

    // 5. Readiness: poll GET /health off the main thread (blocking setup()
    //    can trip the mobile OS startup watchdog), then open the webview.
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        // ... TCP-connect + "GET /health" poll loop, up to 60 s ...
        // on timeout: show_startup_error(&handle, "...") — a visible error
        // page instead of a blank screen (see section 5); on success:
        if let Err(e) = tauri::WebviewWindowBuilder::new(
            &handle,
            "main",
            tauri::WebviewUrl::External(
                format!("http://127.0.0.1:{port}").parse().unwrap(),
            ),
        )
        .title("my-app")
        .build()
        {
            eprintln!("[my-app] Failed to open window: {e}");
            show_startup_error(&handle, "The app window could not be opened.");
        }
    });

    Ok(())
}
```

### The app-side extraction: `src/lib.rs::serve()`

Autumn apps register routes explicitly in `main()` (`routes![...]`), so the
shell crate can only run your server if your app exposes it as a **library
function**. When your `src/main.rs` still matches the stock scaffold shape
(`#[autumn_web::main] async fn main() { ... }`), the generator rewrites it
automatically:

- `src/lib.rs` gets your entire former `main.rs` with the entry point renamed
  to `pub async fn serve()`,
- `src/main.rs` shrinks to a thin caller, so desktop `cargo run` behavior is
  unchanged:

```rust
#[autumn_web::main]
async fn main() {
    my_app::serve().await;
}
```

If you customised `main.rs` (or already have a `src/lib.rs`), the generator
**skips the rewrite and warns** instead of guessing. Do the same two steps by
hand: move the contents of `main.rs` into `src/lib.rs`, replace
`#[autumn_web::main] async fn main() {` with `pub async fn serve() {` (making
any items `serve()` needs stay in the same file), and write the thin `main.rs`
shown above.

### Loopback cleartext on mobile platforms

The webview talks plain HTTP to `127.0.0.1`, which both platforms treat
specially but not identically:

- **iOS**: App Transport Security exempts loopback in recent SDKs, but if you
  hit ATS errors add `NSAllowsLocalNetworking = YES` under
  `NSAppTransportSecurity` in the generated Xcode project's `Info.plist`.
- **Android**: cleartext is blocked by default from API 28. Allow it for
  loopback only, via a `network_security_config.xml` with a
  `<domain-config cleartextTrafficPermitted="true">` entry for `127.0.0.1`
  (preferable to a blanket `android:usesCleartextTraffic="true"`).

Your **database** connection is separate from this: it crosses real networks
and must use TLS. Autumn's pool honors `sslmode` in the connection URL —
`sslmode=require` encrypts the connection (without verifying the server's
certificate identity, matching `libpq`), and `sslmode=verify-full` also
verifies the certificate chain and hostname (against the Mozilla root store,
plus an `sslrootcert=<PEM file>` if your provider uses a private CA). See
section 5 for which to pick.

## 3. Remote Postgres over flaky mobile networks

Autumn's Postgres layer is diesel-async on a **deadpool** connection pool
(`tokio-postgres` underneath). On a phone, the network is hostile to pooled
TCP connections: radios sleep, NATs time out idle flows, the OS switches
between Wi-Fi and cellular mid-session, and iOS suspends the whole process
when the app is backgrounded. The generated defaults and their rationale:

- **`AUTUMN_DATABASE__POOL_SIZE=2`** (framework default: 10) — a phone serves
  exactly one user, so two connections cover a foreground request plus one
  overlapping background job. Every idle pooled connection is a socket that
  will silently die on the next network transition and cost a failed checkout
  to discover; a big server-style pool just means more dead sockets to churn
  through after every Wi-Fi ⇄ cellular hop.
- **`AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS=5`** — this matches the framework
  default; the template pins it explicitly so the mobile posture survives any
  future change to that default. It bounds **new** connection attempts: when
  the radio is off or the link is black-holed, opening a connection fails in
  seconds (surfacing a retryable error to your UI) instead of hanging behind
  a multi-minute OS-level TCP timeout. Caveat: it does **not** bound the
  health check run when a *stale pooled* connection is recycled on checkout —
  a silently dead socket (NAT drop, Wi-Fi ⇄ cellular switch) can make that
  ping wait on the OS TCP timeout. A hard-down network (airplane mode) errors
  fast; a black-holed one may make the first request after the transition
  slow before the pool discards the dead connection.

**Reconnection semantics.** deadpool does not run a background reconnect
loop. Recovery happens **on checkout**: when a handler asks the pool for a
connection, the pool recycles (health-checks) the candidate object and
discards-and-recreates it if the underlying connection is broken. In
practice: after a network transition, the *first* query may pay a
reconnection (or fail once if the break is only discovered mid-query), and
the pool heals itself request by request. There is nothing to restart.

**Suspend/resume.** When the OS backgrounds the app, the server thread is
frozen with the rest of the process, and the OS or the server's NAT peer may
tear down the pooled sockets. On resume nothing needs explicit handling —
the next checkout discards the dead connections — but expect the first
request after a long suspension to be slower (TLS + Postgres handshake) or to
fail once. Design accordingly:

- make write endpoints **idempotent** (client-generated keys, upserts) so a
  retry after "connection reset" is safe;
- retry failed requests once or twice with a short backoff at the UI/htmx
  layer, rather than inside the pool;
- treat "DB unreachable" as a first-class UI state (banner + retry button),
  not an error page.

**Verify it yourself** (simulator or device):

1. launch the app, load a DB-backed page — works;
2. enable airplane mode, trigger a request — should fail within seconds
   (the OS reports the network as down, and new connection attempts are
   bounded by the connect timeout), not hang indefinitely;
3. disable airplane mode, retry — should succeed after one pool recycle;
4. background the app for 10+ minutes, resume, trigger a request — should
   succeed (possibly slower on the first hit, per the recycle caveat above).

**When you need offline**, this model is the wrong tool: it requires network
for every query. That is "Option C" (issue #1508) territory — a local
store with sync — not a pool-tuning problem.

## 4. App Store guideline compliance

An in-process Tauri mobile app is a "hybrid" app: native shell, web-rendered
UI served by compiled-in Rust code. That is an accepted app architecture on
both stores, but two Apple guidelines are worth engineering for explicitly
(reviewed as of mid-2026 — guidelines evolve, so re-check
[Apple's App Review Guidelines](https://developer.apple.com/app-store/review/guidelines/)
and [Google Play's policies](https://support.google.com/googleplay/android-developer/answer/9878810)
before each submission):

- **Apple 4.2 Minimum Functionality** — the "repackaged website" rejection.
  Apps that are just a web page in a shell get rejected. Working in your
  favor: this model's UI is served from *inside the binary*, works without
  loading anything from the internet (only the DB is remote), and launches
  instantly. To stay clearly on the right side: integrate with the platform
  (share sheet, notifications, biometrics where sensible), keep the UI
  responsive when the network is down (see section 3), and don't ship a
  literal mirror of your public website.
- **Apple 2.5.2 (and platform JIT restrictions) — self-contained code** —
  apps may not download or execute code that changes the app's behavior. The
  in-process model complies **by construction**: the server, your routes, and
  all web assets are compiled into the app binary (the generated shell builds
  your app with the `embed-assets` feature, so CSS/JS/static files ship
  inside the executable); nothing executable is downloaded at runtime. Keep
  it that way — do **not** add hot-loaded remote JS bundles, remote
  server-rendered pages in the app webview, or any "over-the-air update"
  mechanism for app logic. Data from your database is fine; *code* is not.
- **Google Play** — the equivalent is the webview-app spam policy
  ("Minimum functionality" under spam policies): pure webview wrappers of
  websites are rejected. The same mitigations as Apple 4.2 apply. Also
  declare your app's network use honestly in the Data safety form (it talks
  to your Postgres host), and keep native code compliant with target API
  requirements (the Tauri toolchain handles W^X etc. because nothing is
  spawned or downloaded).

Practical do/don't summary:

| Do | Don't |
| --- | --- |
| Bundle every asset (HTML/CSS/JS) in the binary | Load UI or JS from a remote server into the app webview |
| Use platform integrations to clear "minimum functionality" | Ship a 1:1 wrapper of your public site |
| Ship new features through store releases | Hot-patch app behavior over the network |
| Use TLS to your database | Embed production DB superuser credentials in the app |

On that last point: the app ships with credentials for *some* Postgres role.
Treat the app as an untrusted client — give it a dedicated role with
least-privilege grants and row-level security, or put an API/auth layer in
front of the database for anything sensitive. A shipped binary can always be
reverse-engineered for its embedded secrets.

## 5. Security: the loopback trust boundary and failure modes

### Any app on the device can reach your server

The in-process server binds `127.0.0.1:<ephemeral port>` — never a public
interface — but **loopback is shared device-wide, not per-app**. On both iOS
and Android, any other installed app can connect to `127.0.0.1:<port>` and
speak plain HTTP to your Autumn server. There is **no authentication between
the webview and the server**: no bearer token, no shared secret. The random
ephemeral port is only a speed bump, not a boundary.

Treat every request as potentially coming from another app on the device:

- **Rely on session authentication** for anything user-scoped — exactly as
  you would for a public web deployment. A co-resident app gets the same
  unauthenticated surface an anonymous internet visitor would (login,
  registration, public routes), nothing more — *if* your handlers enforce
  auth consistently.
- **Do not mount secrets-bearing or admin-only surfaces** in the mobile
  build (admin panels, debug endpoints, signing-secret dumps). If a route
  would be dangerous exposed to localhost malware, don't ship it in the app.
- **Rate-limit login** and treat local brute-force as in scope.
- A future framework option may add a per-launch webview↔server token; today
  the session layer is the boundary.

Two related caveats to know about:

- **Port rebinding (TOCTOU).** The free port is found by binding
  `127.0.0.1:0` and dropping the listener; the in-process server re-binds it
  after config/pool startup. In that window a malicious co-resident app could
  grab the port, and the health probe (which accepts any HTTP response)
  would then point the webview at the impostor. Exploitation requires local
  malware racing an ephemeral port, but be aware the gap exists.
- **The database credentials live in `src-tauri/src/lib.rs`** once you set
  `AUTUMN_DATABASE__URL` — and in the shipped binary. See section 4's
  least-privilege guidance; the app must be treated as an untrusted client.

### Database TLS: `require` vs `verify-full`

`sslmode=require` encrypts the connection but does **not** verify the
server's certificate identity (this matches `psql`/`libpq`, and keeps
self-signed/private-CA servers working) — a network-level attacker who can
redirect your traffic could therefore impersonate the server. For a consumer
app talking to your database across the internet, prefer
**`sslmode=verify-full`**: it verifies the certificate chain and hostname
against the Mozilla root store, or against your provider's CA via
`sslrootcert=/path/to/ca.pem` in the URL. Plain `sslmode` absent or
`disable` sends **everything in cleartext** — never ship that outside a
private network/VPN.

### Failure modes: what the user sees when startup fails

If the server never answers the health poll (60 s) or the window cannot be
built, the shell shows a **visible error page** instead of exiting silently
behind a blank screen. Two framework-level caveats remain, documented here
honestly:

- Some startup failures inside `serve()` (for example a database bootstrap
  error, or — in the dev profile — a failed auto-migration) currently call
  `std::process::exit(1)` from the server thread, which terminates the whole
  app before any error page can render. On a device this is
  indistinguishable from a crash; check the device logs (`Console.app` /
  `adb logcat`) for the autumn error output. Note that Apple discourages
  apps terminating themselves — keep release configuration correct so this
  path never runs in production.
- The generated shell's error page appears only after the health-poll
  timeout elapses; a misconfigured `AUTUMN_DATABASE__URL` whose host
  blackholes traffic means up to 60 s of splash/blank before the message.

## 6. Scaffolding and building

```bash
cd my-app
autumn generate tauri-mobile        # or --dry-run to preview

cargo install tauri-cli --version '^2'

# iOS (macOS host with Xcode):
cd src-tauri
cargo tauri ios init
cargo tauri ios dev                 # simulator; `cargo tauri ios build` for devices

# Android (Android Studio SDK + NDK installed):
cargo tauri android init
cargo tauri android dev             # emulator; `cargo tauri android build` for APK/AAB
```

Before shipping: set `AUTUMN_DATABASE__URL` in `src-tauri/src/lib.rs` (see
the commented block in `run()`), replace the placeholder icons
(`cd src-tauri && cargo tauri icon icons/icon.svg` — or point the command at
your own SVG), and change the `com.example.*` identifier in
`src-tauri/tauri.conf.json`.

`autumn destroy tauri-mobile` removes the generated `src-tauri/` shell; the
extracted `src/lib.rs` + thin `src/main.rs` are left in place (they remain a
perfectly good desktop app layout). Two practical notes:

- Because the documented setup flow **edits** `src-tauri/src/lib.rs` (the
  database URL), destroy will detect the divergence and refuse without
  `--force`. That is deliberate: `--force` deletes the edited file —
  including the credentials you put in it — with no backup. Copy anything
  you want to keep (at minimum your `AUTUMN_DATABASE__URL` line) before
  running `autumn destroy tauri-mobile --force`.
- Destroy never touches your app's `src/` — only the generated shell.

## Relationship to the desktop scaffold and PWA

`autumn generate tauri` (desktop sidecar + bundled local Postgres),
`autumn generate tauri-mobile` (this page), and `autumn generate pwa` are
independent and composable: one server-rendered codebase can ship as a
desktop installer, a mobile app, and an installable PWA.
