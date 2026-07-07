# Desktop Apps with Tauri (`autumn generate tauri`)

`autumn generate tauri` scaffolds a complete `src-tauri/` sub-project that
wraps any existing autumn app in a native desktop shell. The generated project
uses Tauri v2's **sidecar model**: the autumn server binary runs as a supervised
child process and the webview loads the app from a loopback port chosen at
runtime. Your existing routes, Maud/htmx templates, and sessions run entirely
unmodified — the generator is purely additive and never rewrites your app code.

> **See also:** building for **mobile (Android/iOS)** as a thin client against a remote server — [Mobile thin-client apps with Tauri](tauri-mobile-thin-client.md) (`autumn generate tauri --remote-url <URL>`).

## Prerequisites

### External toolchain

Install the Tauri CLI:

```bash
cargo install tauri-cli --version "^2"
```

The Tauri CLI also requires platform-specific toolchain components:

| Platform | Required components |
|----------|-------------------|
| **Linux** | `webkit2gtk-4.1`, `build-essential`, `curl`, `wget`, `file`, `libssl-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev` |
| **macOS** | Xcode Command Line Tools (`xcode-select --install`) |
| **Windows** | [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) and [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) (pre-installed on Windows 11) |

See the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/) for the full up-to-date list.

### autumn feature dependencies

The scaffolded desktop app relies on two autumn features that must be enabled
when building the sidecar binary:

- **`autumn-web/managed-pg-bundled`** (issue #1119 — managed local Postgres):
  embeds the Postgres binaries into the sidecar so the packaged desktop app
  needs no separately-installed database. The sidecar is pointed at a
  persistent per-app data directory via the `AUTUMN_MANAGED_PG_DATA_DIR`
  environment variable, which the generated Tauri shell sets automatically to
  `{app-data-dir}/db`.

- **`autumn-web/embed-assets`** (issue #1004 — single-binary asset embed):
  embeds the `static/` tree (CSS, JS, images, the fingerprint manifest) into
  the release binary so the packaged app has no loose files alongside it.

Both features are already enabled by the generated `stage-sidecar.sh` /
`stage-sidecar.ps1` scripts; you only need to wire `ManagedPostgresPoolProvider`
in your app's pool configuration if you haven't already (see the
[managed Postgres guide](managed-pg.md)).

## Scaffolding

Run the generator from your project root:

```bash
autumn generate tauri
```

The generator is idempotent. Re-running without `--force` errors on existing
files; re-running with `--force` regenerates them deterministically. It never
touches your app's `src/main.rs` or root `Cargo.toml`.

```
src-tauri/
  tauri.conf.json          — Tauri v2 config (productName, identifier, bundle, sidecar ref)
  Cargo.toml               — standalone shell crate with its own [workspace]
  build.rs                 — calls tauri_build::build()
  src/
    main.rs                — #![windows_subsystem = "windows"] + calls lib::run()
    lib.rs                 — sidecar lifecycle glue (see below)
  icons/
    icon.svg               — SVG source; replace with your own, then run `cargo tauri icon`
    32x32.png              }
    128x128.png            } placeholder icons; regenerate from icon.svg with `cargo tauri icon`
    128x128@2x.png         }
    icon.png               }
    icon.ico               — Windows icon
    icon.icns              — macOS icon
  stage-sidecar.sh         — Unix: build autumn sidecar → copy to binaries/
  stage-sidecar.ps1        — Windows: same in PowerShell
  .gitignore               — /target  /binaries  /gen
```

If you previously ran `autumn generate pwa`, the existing `static/icons/icon.svg`
is reused as the Tauri icon source automatically.

## Sidecar lifecycle

`src-tauri/src/lib.rs` implements the full desktop process lifecycle:

1. **Ephemeral port**: `TcpListener::bind("127.0.0.1:0")` picks an OS-assigned
   port; the port number is saved and the listener is dropped before the sidecar
   needs it. No hardcoded ports, no firewall rules.

2. **Spawn sidecar**: the Tauri `setup` hook uses `tauri-plugin-shell` to launch
   the autumn binary with its working directory set to the Tauri resource directory
   (where `autumn.toml` is bundled), plus `AUTUMN_SERVER__HOST=127.0.0.1`,
   `AUTUMN_SERVER__PORT={port}`, `AUTUMN_MANAGED_PG_DATA_DIR={app-data-dir}/db`,
   and `AUTUMN_MANIFEST_DIR={resource-dir}`. Setting the working directory is the
   key mechanism: `AutumnConfig` falls back to a CWD-relative `autumn.toml` lookup
   when the compile-time path is absent on the installed machine.

3. **Wait for ready**: the setup hook sends `GET /` over raw TCP and accepts any
   valid HTTP response (any status code) as the readiness signal, then waits up
   to 30 seconds. Using `GET /` rather than a specific health path avoids a
   duplicate-route panic if your app already defines a `GET /health` handler.

4. **Open window**: once the server is ready, the webview navigates to
   `http://127.0.0.1:{port}`.

5. **Clean shutdown**: on `WindowEvent::Destroyed`, the stored `CommandChild`
   handle is killed. No orphaned server process survives after the window closes.

## App configuration (`autumn.toml`)

`autumn.toml` and any profile override files (`autumn-prod.toml`,
`autumn-production.toml`, etc.) are automatically included in the installer as Tauri
bundle resources (via `bundle.resources` in `tauri.conf.json`). The generated shell
sets the sidecar's working directory to the resource directory, so
`AutumnConfig::load_with_env` finds them on the installed machine via its CWD
fallback.

This means your production `autumn.toml` — auth keys, SEO settings, security
headers, `auto_migrate_in_production`, and anything else you configure there — is
packaged with the installer and takes effect at runtime. Profile files must exist in
the project root at the time you run `cargo tauri build` to be included; Tauri skips
glob entries that match no files. Secrets that should not be committed to source
control (e.g. `database.primary_url`, third-party API keys) should be supplied via
environment variables as normal.

## Building a native installer

From inside `src-tauri/`:

```bash
# Stage the sidecar binary (called automatically by tauri build, but can run standalone):
bash stage-sidecar.sh        # Unix (script uses bash-specific features)
.\stage-sidecar.ps1          # Windows PowerShell

# Build the installer for the host OS:
cargo tauri build
```

Installer outputs by platform:

| Platform | Output |
|----------|--------|
| Linux | `.deb`, `.rpm`, `.AppImage` under `src-tauri/target/release/bundle/` |
| macOS | `.dmg`, `.app` under `src-tauri/target/release/bundle/` |
| Windows | `.msi`, NSIS `.exe` under `src-tauri\target\release\bundle\` |

Cross-compilation is not supported by Tauri out of the box; build on each
target platform or use a CI matrix (GitHub Actions with `ubuntu-latest`,
`macos-latest`, `windows-latest`).

## Customising icons

Replace the placeholder icons with your own:

```bash
# Provide a 1024×1024 PNG or square SVG:
cp my-logo.svg src-tauri/icons/icon.svg
cd src-tauri
cargo tauri icon icons/icon.svg
```

`cargo tauri icon` regenerates all required sizes and formats automatically.

## Dev workflow

`cargo tauri dev` is self-contained — no separate `autumn dev` process is needed
or should be running alongside it:

```bash
cd src-tauri
cargo tauri dev
```

What happens:

1. `beforeDevCommand` (in `tauri.linux.conf.json` / `tauri.macos.conf.json` /
   `tauri.windows.conf.json`) runs the staging script with `"wait": true`,
   building the autumn binary and copying it to `binaries/`.
2. Tauri compiles the shell crate and launches it.
3. The generated `lib.rs` spawns the staged sidecar as a child process on an
   ephemeral loopback port, waits for it to respond to HTTP, then opens the
   webview.

**Do not** run `autumn dev` in a separate terminal alongside `cargo tauri dev`:
the shell always spawns its own sidecar instance, so you would have two server
processes on different ports and the webview would connect to the sidecar, not
the external `autumn dev` server.

For rapid iteration rebuild with `cargo tauri dev` again after changing server
code. Template changes that don't affect the binary (e.g. pure CSS tweaks)
don't require re-staging; re-run to pick them up once the binary is rebuilt.

> **Note:** Tauri's webview live-reload is not applicable here because the server
> is a supervised sidecar, not a URL the webview watches externally. Full
> hot-reload in the sidecar model would require a separate mechanism (e.g.
> `cargo-watch` rebuilding the sidecar binary and signalling the shell to
> restart it) which is out of scope for the generator.

## Relationship to PWA support

`autumn generate tauri` and `autumn generate pwa` are independent. Running both
gives you a Progressive Web App installable from the browser **and** a native
desktop installer from the same server-rendered codebase — no code duplication.
