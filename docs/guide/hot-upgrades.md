# In-Place Upgrades (Hot Upgrade)

An Autumn app can swap itself to a newly-built binary **while it is running** —
without dropping a connection, without a load balancer, and without rebuilding
its in-memory state from cold.

```console
$ cargo build --release                 # build the new binary over the old path
$ kill -USR2 $(pidof my-app)            # upgrade in place
```

The running process hands its **listening socket** and a designated block of
**typed in-memory state** to the new build, waits for that build to serve, and
only then drains itself. Under sustained load the cutover refuses zero
connections and fails zero reads; writes to the designated state block are
refused with a retryable `503` for the moment it takes the new build to come
up, rather than being accepted and thrown away.

What makes this different from every "zero-downtime" deploy that is really a
process replacement: the old→new state carry-over is a function the **compiler
proves total**. A shape change that forgets a field does not build.

> **Linux/Unix, TCP listeners.** The handoff is a `SIGUSR2` plus a
> file-descriptor pass. A Unix-socket or TLS listener cannot be handed over in
> this release and the upgrade is refused with an error in the log rather than
> silently degraded. Fleet-wide rolling deploys are a different tool — see
> [Fleet deploys](fleet-deploys.md).

---

## The worked example

`examples/hot-upgrade/` is two binaries: `hot-upgrade-v1`, the running build,
and `hot-upgrade-v2`, the newly-built one — whose live-state shape has an extra
field, so it carries a migration.

**v1** designates a block of state and registers it with the app:

```rust
use autumn_web::prelude::*;
use autumn_web::upgrade::{LiveState, LiveStateHandle};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stats {
    hits: u64,
    note: String,
}

impl LiveState for Stats {
    const VERSION: u32 = 1;
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, bump, set_note])
        .with_live_state(Stats::default())
        .run()
        .await;
}
```

Handlers reach it through the app state:

```rust
#[get("/bump")]
async fn bump(State(state): State<AppState>) -> AutumnResult<String> {
    let stats = state
        .live_state::<Stats>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("live state is not installed"))?;
    stats
        .write(|s| {
            s.hits += 1;
            s.hits
        })
        .map(|hits| format!("hits={hits}\n"))
        // Refused while the state is frozen for a handover: the client's retry
        // lands on the successor instead of being lost here.
        .map_err(|frozen| AutumnError::service_unavailable_msg(frozen.to_string()))
}
```

**v2** bumps the shape — a new `upgrades` counter — so it declares both shapes
and the migration between them:

```rust
#[derive(Debug, Serialize, Deserialize)]
struct StatsV1 {
    hits: u64,
    note: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stats {
    hits: u64,
    note: String,
    upgrades: u64,
}

impl LiveState for StatsV1 { const VERSION: u32 = 1; }
impl LiveState for Stats { const VERSION: u32 = 2; }

autumn_web::state_migration! {
    from StatsV1 as old => Stats {
        hits: old.hits,
        note: old.note,
        upgrades: 1,
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, bump, set_note])
        .with_live_state_from::<StatsV1, _>(Stats::default())
        .run()
        .await;
}
```

Run it:

```console
$ cargo build -p hot-upgrade
$ AUTUMN_UPGRADE_BINARY=target/debug/hot-upgrade-v2 \
    ./target/debug/hot-upgrade-v1 &
$ curl localhost:3000/note/hello
v1 hits=1 note=hello upgrades=0 pid=4242
$ kill -USR2 4242
$ curl localhost:3000/
v2 hits=1 note=hello upgrades=1 pid=4310
```

Same port, same socket, no restart — and `note=hello`, written to the old
build's memory, is being read out of the new build's.

`cargo test -p hot-upgrade --test live_upgrade` proves it under load: eight
concurrent clients across the cutover, zero refused connections, zero failed
reads, the pre-upgrade value intact, and the cutover latency spike measured.

---

## The guarantee: a migration that cannot be partial

`state_migration!` has exactly two forms, and neither can express "map the
fields I remembered."

**A struct shape** maps every field, because what it emits is a struct literal:

```rust
autumn_web::state_migration! {
    from StatsV1 as old => Stats {
        hits: old.hits,
        note: old.note,
        upgrades: 1,
    }
}
```

Delete the `upgrades` line and the build stops with `missing field `upgrades` in
initializer of `Stats``. There is no `..Default::default()` to reach for: the
grammar has no rule for a rest pattern, so writing one is a macro error.

**An enum shape** maps every variant, by name:

```rust
autumn_web::state_migration! {
    from ModeV1 as old => Mode {
        match old {
            Fast => Mode::Fast,
            Slow(level) => Mode::Slow { level },
        }
    }
}
```

Forget a variant and the `match` is non-exhaustive. Reach for a `_` catch-all
and the macro refuses it — the grammar takes variant *names*, not patterns, so
a wildcard cannot be written at all.

A migration between two shapes that declare the **same** `LiveState::VERSION`
is refused too: they would be indistinguishable on the wire, so the migration
could never run and the old payload would be handed to the new shape's
`Deserialize` — the very loss the migration exists to prevent. Bump `VERSION`
in the same commit that changes the fields.

All five refusals are pinned by negative tests
(`autumn/tests/compile-fail/state_migration_*.rs`), so the guarantee cannot rot.

What the compiler proves is **totality, not truth**: a field mapped to a
constant compiles fine. Totality is what stops a shape change from quietly
dropping state — reviewing what each field is mapped *to* is still yours.

---

## What happens on `SIGUSR2`

1. **Snapshot and freeze.** The designated block is serialized and frozen.
   From here until the successor serves, `handle.write(...)` returns
   `Err(LiveStateFrozen)` instead of accepting a write this process can no
   longer carry. Reads keep working. The freeze starts *before* the successor
   exists, so a client retrying during that window is answered by this same
   process until the successor comes up — usually milliseconds, but bounded by
   `ready_timeout_secs` if the successor hangs, and lifted again if the upgrade
   is abandoned.
2. **Exec the new build**, handing it the listening socket (as the successor's
   stdin, the way `inetd` has passed sockets for decades) plus the snapshot.
3. **Wait for it to serve.** The successor adopts the socket, adopts (and if
   needed migrates) the state, and signals ready once its startup hooks have
   finished. The predecessor is still accepting the whole time. Note the
   successor starts accepting the moment it adopts the socket — before its
   startup hooks finish, exactly as a cold start does — so both builds serve
   for that window.
4. **Drain.** The predecessor stops accepting, finishes its in-flight requests,
   runs its `on_shutdown` hooks, and exits. Connections queued on the shared
   socket are picked up by the successor — the socket is never closed, so
   nothing is refused.

Because the socket never leaves, an upgrade drain skips the two steps a real
shutdown needs: `/ready` is **not** flipped to 503 (there is no load balancer to
deregister from, and the address is still healthy — the successor is behind it)
and the prestop grace is **not** waited out. A probe that hits the draining
process still gets `200`, which is the truth: the address it is probing is
being served.

### When it goes wrong, the old build carries on

Every failure path leaves the old build serving, its state writable again. It
is not quite "nothing happened": a successor that got as far as accepting
connections and then died took those requests with it — but the address stays
up and the state stays whole.

| Failure | Result |
|---|---|
| the new binary is missing (or the path names no file) | refused before the state is even frozen |
| the new binary is present but unrunnable (half-written, wrong architecture) | the spawn fails; the freeze is lifted and nothing else changes |
| the new binary crashes on boot | the wait ends the moment the child exits |
| the new build cannot decode or migrate the state | it refuses to start, so the wait ends the same way |
| the new build hangs during startup | abandoned after `ready_timeout_secs`, and the successor is killed |
| the listener cannot be handed over (Unix socket, TLS) | refused with an error in the log |
| this process supervises a managed Postgres cluster | refused with an error in the log |
| the new build was handed the socket but cannot adopt it (it switched to a Unix socket, say) | it refuses to start rather than serve a different address |
| the new build dropped its `with_live_state(...)` call | it refuses to start rather than throw the carried state away |

A later `SIGUSR2` — after fixing the binary — retries from scratch.

---

## Configuration

```toml
[server.upgrade]
enabled = true            # default: true
ready_timeout_secs = 30   # default: 30
```

With `enabled = false` the signal is logged and ignored — still safer than the
default disposition of `SIGUSR2`, which terminates the process.

| Environment variable | Meaning |
|---|---|
| `AUTUMN_UPGRADE_BINARY` | Path to exec on upgrade. Defaults to the path this process was started from, captured at boot (`/proc/self/exe` reports `(deleted)` once a deploy replaces the file, which is why it is read early). |
| `AUTUMN_UPGRADE_DIR` | Where the per-upgrade handoff directory is created. Defaults to the system temp directory. Each one is created `0700`, its files `0600`, and the whole directory is removed once the handover finishes either way. |

The remaining `AUTUMN_UPGRADE_*` variables (`LISTEN_FD`, `STATE_FILE`,
`READY_FILE`, `GENERATION`, `PREDECESSOR_PID`) are the protocol between the two
processes. Autumn sets them for the successor; setting them by hand is not
supported.

Two deploy shapes work naturally:

* **Replace the file, then signal** — `install` or `mv` the new binary over the
  running path (a rename, so the running image is untouched), then `kill -USR2`.
* **Stage it beside, then signal** — start the app with
  `AUTUMN_UPGRADE_BINARY=/opt/app/next`, drop each new build there, then
  `kill -USR2`.

---

## What survives, and what does not

**Survives:** the designated live-state block, and every connection queued on
the listening socket.

**Does not survive** — the process boundary is real:

* Anything not in the designated block: other extension-map entries, in-process
  caches, WebSocket sessions and presence (clients are sent a close frame and
  reconnect to the successor), open database connections, in-memory job state.
* Writes attempted after the snapshot: they are *refused*, not lost — handle
  `Err(LiveStateFrozen)` by returning a retryable `503`, as the example does.
* One block per app. Designating a second is a startup error, because silently
  carrying only one of two designated blocks is exactly the loss this feature
  exists to prevent.
* One version hop. A snapshot is adopted at the current `VERSION`, or migrated
  from the one `Old` version registered with `with_live_state_from`. Anything
  else refuses to start, which abandons the upgrade rather than guessing.

* **Handle `LiveStateFrozen` fail-closed.** If the block holds anything
  security-relevant — rate-limit counters, one-time tokens, replay nonces,
  idempotency keys — a refused write must mean "reject the request", never
  "allow it and skip the bookkeeping": the freeze window would otherwise be a
  window with the limit switched off.
* **Counters are duplicated across the cutover.** Once the successor is up,
  both processes are serving from independent copies of the snapshot, so any
  counter in the block is effectively doubled until the predecessor exits —
  exactly the caveat that already applies across replicas.
* **The snapshot is plaintext on disk during the handoff.** It is written
  `0600` inside a `0700` directory and unlinked as soon as the successor reads
  it, but it is not encrypted and unlinking is not shredding. Don't designate
  secrets as live state; carry them through the same secret store a restart
  would.
* **Bind address changes need a real restart.** An adopted socket is the
  predecessor's: a new `server.host`/`server.port` in the new build is logged
  as a mismatch and ignored, because the socket is already bound.
* **`state_migration!` proves variant *presence*, not payload completeness.**
  An enum arm may bind a variant's payload as `..`, which compiles and drops
  those fields — the macro cannot tell that apart from a deliberate mapping.
* **Both builds are briefly alive.** Between the successor signalling ready and
  the predecessor finishing its drain, two processes are running: background
  workers, schedulers and cron loops overlap for that window, exactly as they do
  during a rolling restart. Work that must not run twice needs the same
  leader-election or idempotency it already needs across replicas.

### Operational notes

* **systemd.** The successor is a child of the running process, so a unit that
  tracks a single main PID will consider the service stopped when the
  predecessor exits, and may kill the successor with it. Use in-place upgrade
  under a supervisor that tolerates this (or none at all); `autumn deploy`'s
  generated unit does process replacement instead, which is unaffected.
* **Managed Postgres** (`managed-pg`) supervises a database child process per
  app process, and that child cannot be handed over: an upgrade is *refused*
  with an error in the log while a managed cluster is running, rather than
  letting the successor start a second postmaster over the same data
  directory.
* **Generations.** Each hop increments `AUTUMN_UPGRADE_GENERATION`, which is
  logged by both processes — the quickest way to confirm from the logs which
  build answered a request.

---

## Related

* [Deploying an Autumn app](deployment.md) — push-button deploys and the
  graceful-restart path.
* [Fleet deploys](fleet-deploys.md) — rolling a *fleet* of replicas.
* [Cloud-native](cloud-native.md) — readiness/liveness probes and drain budgets.
