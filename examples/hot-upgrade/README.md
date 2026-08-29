# hot-upgrade — in-place upgrades with verified state migration

Two binaries, one worked example of Autumn's `SIGUSR2` in-place upgrade
(issue #1674):

* `hot-upgrade-v1` — the running build. Designates a block of live state
  (`Stats { hits, note }`, version 1).
* `hot-upgrade-v2` — the newly-built one. Its state shape gained an `upgrades`
  counter, so it declares both shapes and the `state_migration!` between them.

Upgrading v1 into v2 keeps the listening socket, every queued connection, and
the contents of the live state. The migration is total by construction: delete
a field mapping from `v2.rs` and the binary does not compile.

The narrative is [`docs/guide/hot-upgrades.md`](../../docs/guide/hot-upgrades.md).

## Prerequisites

- Rust 1.88.0+
- Linux or another Unix (the handoff is `SIGUSR2` + a file-descriptor pass)
- No database, no configuration file

## Quick start

```console
$ cargo build -p hot-upgrade
$ AUTUMN_UPGRADE_BINARY=target/debug/hot-upgrade-v2 ./target/debug/hot-upgrade-v1 &
$ curl localhost:3000/note/hello
v1 hits=1 note=hello upgrades=0 pid=4242

$ kill -USR2 4242          # the pid from the line above

$ curl localhost:3000/
v2 hits=1 note=hello upgrades=1 pid=4310
```

Same port, same socket, no restart — and `note=hello`, written into the old
build's memory, is read back out of the new build's.

## Routes

| Route | Effect |
|---|---|
| `GET /` | Read the live state (works while the process drains) |
| `GET /bump` | Increment the counter; `503` while the state is frozen for a handover |
| `GET /note/{value}` | Put a value in the live state — the value an upgrade must carry |

## Success proof

```console
$ cargo test -p hot-upgrade --test live_upgrade
```

Boots the real v1 binary, drives it with eight concurrent HTTP clients,
upgrades it to the real v2 binary mid-load, and asserts: zero refused
connections, zero failed reads, both builds served part of the load, the
pre-upgrade value survived, the migration ran exactly once, the counter never
went backwards, and the cutover latency spike stayed inside the
graceful-restart drain window.
