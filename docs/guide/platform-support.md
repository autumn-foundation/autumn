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
| `autumn serve (foreground)` | Builds and runs the app in the foreground, binding TCP per config. |
| `managed Postgres` | Boots and shuts down cleanly under `autumn dev` and a direct binary run. Note `autumn serve --bundled-pg` implies `--daemon`, so *that* entry point is Tier 2. |

Tier 1 is a **gate, not an aspiration**: `.github/workflows/ci.yml` runs a
`windows-tier1` job on `windows-latest` that walks the whole journey — scaffold
→ `doctor` → `setup` → a dev-loop edit/rebuild/reload → managed Postgres boot
and clean shutdown — on every pull request into `trunk-dev`. If a change breaks
a Tier 1 command on Windows, that job goes red before the change merges.

### Three Windows details worth knowing

**Managed Postgres has two native entry points, and one that is not.**
`autumn dev` and running the built binary directly both boot and cleanly stop a
managed cluster on native Windows. `autumn serve --bundled-pg` does not, because
`--bundled-pg` implies `--daemon` — and the daemon lifecycle is Tier 2. Use
`autumn dev` on Windows, or run the daemon under WSL2. The cluster's data dir
resolves under `%LOCALAPPDATA%` unless you set `AUTUMN_MANAGED_PG_DATA_DIR`.

**`autumn new` writes `config/master.key` without an owner-only mode.** On Unix
the scaffolder creates it `0600`; Windows has no equivalent in that code path
today, so the file inherits its directory's ACLs. Inside your user profile
(`%USERPROFILE%`, where `autumn new` puts a project by default) that is
owner-plus-administrators, which is fine. In a shared location such as `C:\dev`
it may not be — check the file's permissions, or keep projects under your
profile. This is a known gap, not a promise; `autumn deploy` is Tier 2 for the
same class of reason.


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
takes on Unix, so your hooks run and the cluster stops cleanly. If the app does
not exit within ten seconds, `autumn dev` force-stops it **and prints a warning
saying the hooks may not have run** — degraded, but never silent.

## Tier 2 — supported via WSL2

These are built on Unix primitives — domain sockets, POSIX signals, `ssh`, file
modes, bash. On native Windows they **fail fast** with an error naming this
policy; they are fully supported inside [WSL2](https://learn.microsoft.com/windows/wsl/install).

| Command | Why, and what to do |
| --- | --- |
| `autumn serve --daemon / stop / status / restart` | The daemon lifecycle is built on Unix domain sockets and POSIX signals; run it inside WSL2. |
| `autumn deploy` | Deploys shell out to `ssh`/`sh` and stage secrets with Unix file modes; run them inside WSL2. |
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

### Why the daemon is not ported

`autumn serve --daemon` supervises the app through a Unix domain control socket
and POSIX signals, with process groups for reaping supervised children. Windows
has analogues — named pipes, job objects, Windows Services — but adopting them
is a different lifecycle model, not a port; [the daemon guide](./daemon.md)
already places Windows Service registration out of scope for the same reason.
WSL2 gives Windows developers the exact Linux behaviour the daemon is designed
against, which is also the environment it runs in when deployed.

## `autumn doctor` tells you where you stand

`autumn doctor` includes a `platform_support` check. On Windows it reports both
tiers by command name and flags the platform-specific prerequisites:

```text
✓ platform_support — windows: Tier 1 (native) — autumn new, autumn doctor, ...
                     Tier 2 (WSL2), run these from a WSL2 shell — autumn serve
                     --daemon / stop / status / restart, ... Prerequisites:
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

- Native Windows daemon lifecycle or Windows Service registration — WSL2 is
  the answer for that slice.
- Rewriting the bash contributor gate scripts in `scripts/` — contributor
  tooling stays Tier 2.
- Windows as a **production deploy target**. Autumn deploys to Linux servers.

## Adding a command to the policy

The tier table lives in `autumn-cli/src/platform.rs` and is the single source of
truth: the `doctor` check, every Tier 2 fail-fast message, and this page all
read from it. `autumn-cli/tests/integration/platform_support_policy.rs` fails
the build if this page and that table disagree, so a new Tier 2 command cannot
ship undocumented.
