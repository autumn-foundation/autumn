# Platform support

Autumn's promise is that you **develop on macOS, Linux, or Windows and deploy
on Linux**. This page says exactly what that means on Windows, command by
command, so you never have to find out by trial and error.

There are two tiers. Every autumn command is in one of them, `autumn doctor`
tells you which platform you are on and what that implies, and a
`windows-latest` CI job runs the whole Tier 1 journey on every pull request.

## Tier 1 — works natively on Windows

These run on native Windows (PowerShell, `cmd`, Windows Terminal). No WSL2, no
MSYS, no Git Bash required.

| Command | Notes |
| --- | --- |
| `autumn new` | Scaffolds a project natively. `config/master.key` gets no owner-only mode on Windows (there is no `chmod`), so it inherits its directory's ACLs — fine under `%USERPROFILE%`; see below. |
| `autumn doctor` | Runs natively and reports this platform's tier status. |
| `autumn setup` | Downloads the checksum-verified `tailwindcss-windows-x64.exe`. |
| `autumn dev` | Edit/rebuild/reload works natively; the reload stops the app cooperatively so shutdown hooks (managed Postgres teardown) run. |
| `autumn test` | Delegates to `cargo test`, which is first-class on Windows. |
| `autumn serve (foreground)` | Builds and runs the app in the foreground, binding TCP per config. A console-control stop (a supervisor, or Windows shutting down) drains it the way SIGTERM does on Unix. |
| `autumn serve --daemon / stop / status / restart` | Runs natively (#1639). The daemon binds its configured TCP address and records it in `serve.addr`; `stop` drains cooperatively before force-killing. State lives under `%LOCALAPPDATA%`. |
| `autumn serve install-service / uninstall-service` | Registers the daemon as a Windows service that starts at boot and restarts after a crash. Needs an elevated (Administrator) shell, like any service registration. |
| `managed Postgres` | Boots and shuts down cleanly under `autumn dev`, `autumn serve --daemon` and a direct binary run. |
| `autumn deploy check / plan` | Local-only: `plan` renders the unit and step list, `check` grades the config and probes SSH reachability with a portable TCP connect. Validate a deploy config here before running it from WSL2. |

Tier 1 is a **gate, not an aspiration**: `.github/workflows/ci.yml` runs a
`windows-tier1` job on `windows-latest` that walks the whole journey — scaffold
→ `doctor` → `setup` → a dev-loop edit/rebuild/reload → managed Postgres boot
and clean shutdown → the daemon lifecycle (start, status, a served request, a
graceful stop that completes an in-flight request, restart) → service
registration, crash restart and removal — on every pull request into
`trunk-dev`. If a change breaks a Tier 1 command on Windows, that job goes red
before the change merges.

### Four Windows details worth knowing

**A debug build of a managed-Postgres app can exceed the PDB symbol limit.**
`autumn-web`'s dependency graph plus `managed-pg-bundled` (which embeds the
Postgres binaries in your executable) produces more public symbols than a
Windows PDB can hold, and the MSVC linker fails the build outright:

```text
LNK4318: Very long symbol name encountered while producing debug information
LINK : fatal error LNK4319: A PDB limit was hit while adding public symbols
```

It is a linker limit, not a code error. Reduce the debug info for the dev
profile in your project's `Cargo.toml`:

```toml
[profile.dev]
debug = "line-tables-only"   # or `0` if that is still too much
```

`line-tables-only` keeps backtraces useful (file and line, no variable
inspection) at a fraction of the symbol count. Autumn's own `windows-tier1` CI
job builds with `debug = 0` for this reason.

Set it in the **manifest**, not via `CARGO_PROFILE_DEV_DEBUG`. That environment
variable does drop debug info, but autumn's CI hit the limit again with it set,
so the manifest profile — plus `[profile.dev.package."*"]` to cover
dependencies explicitly — is the reliable spelling. If you still hit LNK4319,
add the linker's own remedy:

```toml
# .cargo/config.toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-Clink-arg=/DEBUG:LongSymbolTruncate"]
```

### Three other Windows details worth knowing

**Managed Postgres boots and stops cleanly through every native entry point.**
`autumn dev`, `autumn serve --bundled-pg` (which implies `--daemon`) and running
the built binary directly all provision the cluster and shut it down through the
app's `on_shutdown` hook. The cluster's data dir resolves under `%LOCALAPPDATA%`
unless you set `AUTUMN_MANAGED_PG_DATA_DIR`.

**`autumn new` writes `config/master.key` without an owner-only mode.** On Unix
the scaffolder creates it `0600`; Windows has no equivalent in that code path
today, so the file inherits its directory's ACLs. Inside your user profile
(`%USERPROFILE%`, where `autumn new` puts a project by default) that is
owner-plus-administrators, which is fine. In a shared location such as `C:\dev`
it may not be — check the file's permissions, or keep projects under your
profile. This is a known gap, not a promise; the remote `autumn deploy` actions
are Tier 2 for the same class of reason.


**The dev loop stops the app before rebuilding.** A running
`target\debug\<app>.exe` is locked on Windows, so `cargo build` cannot relink
over it. `autumn dev` therefore stops the old binary *before* building rather
than after. The tradeoff: a failed rebuild leaves the app down, so the
compile-error overlay's "keep serving the stale page" behaviour is not
available and the browser falls back to a normal reconnect.

**Shutdown is cooperative, not a kill.** Windows has no `SIGTERM`, and
terminating a process outright skips the app's `on_shutdown` hooks — which is
how a managed Postgres cluster used to be orphaned on every hot reload. Instead
`autumn dev` sets `AUTUMN_SHUTDOWN_SIGNAL_FILE` and creates that file to request
a shutdown; the runtime drains through exactly the same graceful path a signal
takes on Unix, so your hooks run and the cluster stops cleanly.

The wait is your app's **own** budget, not a fixed one: `autumn dev` reads the
same profile-aware `prestop_grace_secs + shutdown_timeout_secs` that
`autumn serve stop` uses, and adds 60 seconds of headroom for the `on_shutdown`
hooks that run after the drain (sized to the managed-Postgres stop ceiling). So
an app that legitimately takes 35 seconds to drain is not cut off at ten. Only
once that budget elapses does `autumn dev` force-stop it — **and print a warning
saying the hooks may not have run.** Degraded, but never silent.

## Tier 2 — supported via WSL2

These are built on Unix primitives — `ssh`, file modes, bash. On native Windows
they **fail fast** with an error naming this policy; they are fully supported
inside [WSL2](https://learn.microsoft.com/windows/wsl/install).

| Command | Why, and what to do |
| --- | --- |
| `autumn deploy up / rollback / status / maintenance` | These reach the host over `ssh`/`sh`, and `up`/`rollback`/`maintenance` stage secrets with Unix file modes; run them inside WSL2. |
| `scripts/*.sh contributor gates` | The contributor gate scripts are bash; run them inside WSL2. |
| `SystemTest browser tests` | Chromium version probing is satisfied by file existence on Windows (#1456); the browser suites themselves are gated behind the `system-tests` feature and are exercised on Linux. |

To use them, install WSL2 and work from a Linux shell:

```powershell
wsl --install
```

Then, inside the WSL2 shell, install the Linux `autumn` CLI and run the Tier 2
command there. Foreground `autumn serve` and the whole Tier 1 journey keep
working natively on the Windows side either way — WSL2 is an addition, not a
migration.

### The daemon used to be here

Until #1639 the daemon lifecycle was Tier 2, on the grounds that it is built on
Unix domain sockets and POSIX signals. WSL2 was a fine answer for a *developer*
and a poor one for an *operator*: it is unavailable or prohibited on many
Windows Server installs, and it gives no boot-time start and no crash
supervision.

It is now Tier 1, with the same observable contract on both platforms and a
`windows-latest` CI job that walks it end to end. The differences are the ones
the platforms force, and they are documented in
[the daemon guide](./daemon.md#windows): a Windows daemon binds its configured
TCP address rather than a Unix socket, `stop` requests the drain through a file
rather than `SIGTERM`, and state is protected by an ACL rather than `0600`.

## `autumn doctor` tells you where you stand

`autumn doctor` includes a `platform_support` check. On Windows it reports both
tiers by command name and flags the platform-specific prerequisites:

```text
✓ platform_support — windows: Tier 1 (native) — autumn new, autumn doctor, ...
                     autumn serve --daemon / stop / status / restart, ...
                     Tier 2 (WSL2), run these from a WSL2 shell — autumn deploy
                     up / rollback / status / maintenance, ... Prerequisites:
                     autumn generate auth --passkeys needs OpenSSL via vcpkg
                     with VCPKG_ROOT set ... Policy: docs/guide/platform-support.md
```

It **passes**, it does not warn — Windows is a supported development platform
and the Tier 2 journeys have a documented answer, so nothing here is a defect.
That is not cosmetic: `autumn doctor --strict` treats any warning as a failure,
so a check that warned on every Windows machine would make `--strict` — used in
scripts and pre-commit gates — exit 1 forever on Windows. On Linux and macOS the
check passes too, noting simply that every journey is native.

### Windows prerequisites

- **`autumn generate auth --passkeys`** needs OpenSSL. On Windows install it
  through `vcpkg` and set `VCPKG_ROOT` so the build can find it — see
  [the generators guide](./generators.md).
- **`autumn serve install-service` / `uninstall-service`** need an elevated
  (Administrator) shell, like any Windows service registration. `autumn doctor`'s
  `daemon_service` check says so when the shell you are in cannot register one —
  and also reports whether a daemon or a registered service is currently running
  for this project.

## Known issues and their tiers

- **#1456 — Chromium version probing on Windows.** `SystemTest` used to run
  `chrome.exe --version` to decide whether a browser was usable. `chrome.exe`
  is a GUI-subsystem binary that writes nothing to the parent console, and
  running it without a private user-data dir aborts when Chrome is already
  open — so the probe reported "browser not found" on a machine that had one.
  **Resolved:** on Windows the probe no longer executes the candidate; an
  existing file with an `.exe` extension is accepted on that evidence alone
  (`autumn/src/browser_detect.rs`). The browser suites themselves stay Tier 2:
  they are gated behind the `system-tests` feature and are exercised on Linux
  CI, where a Chromium binary is provisioned. **Workaround for a Windows
  developer who wants to run them locally:** run them from a WSL2 shell with
  Chromium installed there, or point `AUTUMN_CHROMIUM` at a Windows Chrome/Edge
  binary — the probe now accepts it on existence. See
  [System tests](./system-tests.md).

## Out of scope

- Rewriting the bash contributor gate scripts in `scripts/` — contributor
  tooling stays Tier 2.
- Windows as a **production deploy target**. Autumn deploys to Linux servers.

## Adding a command to the policy

The tier table lives in `autumn-cli/src/platform.rs` and is the single source of
truth: the `doctor` check, every Tier 2 fail-fast message, and this page all
read from it. `autumn-cli/tests/integration/platform_support_policy.rs` fails
the build if this page and that table disagree, so a new Tier 2 command cannot
ship undocumented.
