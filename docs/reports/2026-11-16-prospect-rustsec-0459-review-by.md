# ⛏️ Prospect: can the RUSTSEC-2026-0459 waiver be retired on its review-by date? (pursue: diesel 2.4 resolves libsqlite3-sys 0.39.1 vs "no published diesel" line; exposure claim fails, 5 unguarded opens vs 0 line)

## 🎯 Question

**Falsifiable, two parts:**
- **Q1 (blocker):** as of 2026-11-16, can the workspace resolve `libsqlite3-sys >= 0.39.0`
  (bundled SQLite >= 3.54.0) with a published diesel release? The waiver says no:
  diesel 2.3.14 caps it below 0.39, and `links = "sqlite3"` allows only one copy.
- **Q2 (exposure):** is the waiver's exposure claim true in code? The claim is that no Autumn
  path opens a SQLite main file of outside origin while a `-wal` of outside origin sits beside
  it ("Import and restore take one main file, and a restore clears stray sidecars first"). The
  waiver itself says this "was not audited end to end".

**Decision fed:** whether to renew the RUSTSEC-2026-0459 ignore in `deny.toml` and
`deny-sqlite.toml` (and in the generated `deny.toml.tmpl`), or take the fix now.
**Decider:** repo maintainer. The weekly Ballast audit carries the waiver.

## ⚖️ Pre-registration

Committed before any lookup: `docs/reports/assay-prereg/2026-11-16-rustsec-0459-review-by-prereg.md`
(commit `fafbaa8`). The lines:

- **Q1 pursue:** a published, non-yanked diesel admits 0.39.x, AND in a scratch copy the bump
  resolves to one 0.39.x. **Q1 kill:** no such diesel, or resolution fails.
- **Q2 holds:** 0 open paths of outside-origin SQLite files without sidecar removal or refusal
  (or a path the process just created). **Q2 fails:** ≥ 1 such path.
- **Mapping:** Q1 pursue → "take the fix". Q1 kill + Q2 holds → "renew". Q1 kill + Q2 fails →
  "renew only with the gap named".

Time box: one session. Control: the current lockfile (0.38.2) under the same commands.

## 🔍 Prior art

- The waiver text and its 2026-11-09 rehearsal (`cargo update -p libsqlite3-sys` failed on `links`).
- #3671, which added the waiver.
- ADR 0013 and the 2026-11-09 Prospect report: the two deny files must agree, and they do.
- No earlier ledger entry on -0459.

New since the rehearsal: on 2026-11-13, crates.io received diesel 2.4.0, diesel-async 0.10.0
and diesel_migrations 2.4.0. Then libsqlite3-sys 0.40.0 landed on 2026-11-11.

## 🧪 Apparatus

1. A scratch `git worktree` of `7e93005`, with the workspace pin changed to
   `libsqlite3-sys = "0.39"` and `diesel-async` changed to `"0.10"` in 14 manifests (root plus
   examples/benchmarks). Then `cargo update -p diesel -p diesel-async -p diesel_migrations
   -p libsqlite3-sys`.
2. `cargo check` on that tree.
3. A read-only code audit of every SQLite open in `autumn/src` and `autumn-cli/src`, done by a
   subagent. I re-read the two strongest findings (U2 and U3) myself.

**Stubs and limits:**
- No `cargo test` was run, and no `cargo deny` (not installed here).
- No release-mode build was done, and no build of the starters, generators or other examples.
- No empirical exploit of the SQLite bug was attempted. Q2 is a count of code paths, not a
  demonstrated leak.
- The `--features sqlite --tests` check could not be judged (see Assay). The audit did not
  trace gdpr/portability imports beyond a grep.

## 📊 Assay

### Q1: resolution

| Run | Result |
|---|---|
| Control: current lock | `libsqlite3-sys v0.38.2` ← autumn-web, diesel 2.3.14; bundled `SQLITE_VERSION "3.53.2"` (affected) |
| Pin 0.39 alone, `cargo update -p libsqlite3-sys` | **fails**: `links = "sqlite3"` conflict (reproduces the 11-09 rehearsal) |
| + `cargo update -p diesel` | **fails**: diesel-async 0.9.2 requires `diesel ~2.3.9`; diesel_migrations 2.3.2 requires `~2.3.0` |
| + diesel-async 0.10, all four updated together | **resolves**: diesel 2.4.0, diesel-async 0.10.0, diesel_migrations 2.4.0, libsqlite3-sys 0.39.1 (one copy); bundled `SQLITE_VERSION "3.54.1"` (fixed) |

The requirements, as published on crates.io:
- diesel 2.4.0: `libsqlite3-sys >=0.17.2, <0.41.0` (2.3.14 had `<0.39.0`); rust-version 1.88.
- diesel-async 0.10.0: `diesel ~2.4.0`.
- diesel_migrations 2.4.0: `diesel ~2.4.0`.
- Workspace MSRV: 1.97.

### Compile cost (beyond the line, sets the productionize estimate)

| Command (scratch tree) | Result |
|---|---|
| `cargo check -p autumn-web --features sqlite` | **pass**, 0 errors, 0 warnings, 1m32s |
| `cargo check -p autumn-web -p autumn-cli --features autumn-web/test-support --lib --bins --tests` (Postgres default, the CI shape) | **pass**, 0 errors, 1 dead-code warning (`openapi_direction.rs:210`, not diesel-related), 10m02s |
| same + `autumn-web/sqlite,autumn-cli/sqlite` (no `test-support`) | `sandbox_header_alloc_gate` fails: it needs `test-support` (feature selection, not diesel) |
| same + `test-support` + sqlite | Pg/Sqlite backend-mixing errors in the Postgres-written integration tests (`factory_fake.rs`, `form_for_derive.rs`, …). **No control run.** The error shape is the backend flip from enabling `sqlite` on the consolidated binary, not a diesel 2.4 API change, but this is not proven |

### Q2: outside-origin SQLite opens (audit)

**5 unguarded, 5 guarded.** The line was 0 unguarded.

| # | Path | Open | Origin of bytes | Sidecar step |
|---|---|---|---|---|
| U1 | `autumn db restore <dir/file>` | `autumn-cli/src/db/sqlite_snapshot.rs:191` via `backup.rs:1084` | operator-named run dir | none for the artifact |
| **U2** | `autumn db restore --offsite` (and `db scrub --artifact`, `scrub.rs:3219`) | same | **every object** under the bucket run prefix, downloaded into a tempdir (`backup.rs:2777-2839`); the only filter is `is_safe_leaf_name` (path traversal) | none. A planted `control.sqlite-wal` lands beside `control.sqlite` before `verify_artifact` opens it `?mode=rw` |
| U3 | replication PITR restore (`replica restore`/`verify`, periodic verifier, fleet `restore_missing`/`restore_to`, lake) | `autumn/src/replication/sqlite.rs:128` via `restore.rs:850` | main file **and** `-wal` both come from the replication destination, by design | old sidecars are cleared (`restore.rs:760`), but the written WAL is itself the input; `page_size` is checked only after open |
| U4 | `DatabaseFleet::import`/`restore_from_file`, `autumn fleet import`/`restore --from` | `autumn/src/db/fleet/sqlite.rs:1821` (`mode=ro`) | caller path | **deliberately** reads the source `-wal` (`import_reads_the_rows_in_the_source_wal`); documented "for files you trust" |
| U5 | restore staging copy | `sqlite_snapshot.rs:317` | copy in the target's existing dir | removes only the staged file. Low severity: needs write access to the db dir plus a PID guess |

The guarded paths:
- **G1:** the final target of `db restore` (`sqlite_snapshot.rs:534-541`).
- **G2:** the replication output publish (`restore.rs:646-669`). It handles `-wal`/`-shm` but not
  `-journal`, which is out of scope here.
- **G3:** the fleet `VACUUM INTO` staging.
- **G4:** fleet publication (`fleet_layout.rs:677`).
- **G5:** the fleet lake attach, which uses `immutable=1`.

I verified U2 and U3 by reading the code. U1, U4 and U5 come from the audit and match the code
it cites.

The waiver's sentence "a restore clears stray sidecars first" is true only of the **destination**
(G1). It is false of the artifact being verified, and the artifact is the outside-origin file.
Whether a bucket counts as "less trusted" is a policy call. The code gives it no authentication
beyond SHA-256 digests stored in the same bucket.

## 🏁 Verdict

**Pursue: take the fix.**
- **Q1 cleared its pre-registered line.** A published diesel (2.4.0) admits libsqlite3-sys 0.39,
  and the workspace resolves to one 0.39.1 that bundles SQLite 3.54.1. The control (0.38.2,
  SQLite 3.53.2) is the affected build.
- **Q2 failed its line**, with 5 unguarded opens against a line of 0. So the waiver should **not**
  be renewed as written either way: its exposure paragraph is false of the offsite restore and
  replication restore paths.

## 💰 Cost to productionize

The work, line by line from the stubs and limits:
1. **Pins.** Root `Cargo.toml`: `libsqlite3-sys = "0.39"` (or `"0.40"`, which diesel 2.4 also
   admits). Change `diesel-async` `"0.9"` → `"0.10"` in the root, 13 example/benchmark manifests,
   and the `cms` and `saas` starter templates (`autumn-cli/src/starters/*/Cargo.toml.tmpl`).
   Then `cargo update`.
2. **Waivers.** Delete the RUSTSEC-2026-0459 ignore from `deny.toml`, `deny-sqlite.toml`
   (ADR 0013: both in one PR) and `autumn-cli/src/templates/deny.toml.tmpl`, where generated apps
   pre-waive it.
3. **Full gates.** Run `cargo xtask pre-push`, the full test lanes, `test-features`, the trybuild
   goldens and the Docker sweep. diesel 2.4 changes `diesel_derives` and `dsl_auto_type`, so the
   trybuild `.stderr` goldens can move. Run the generator-conformance matrix too, since starters
   change.
4. **The open sqlite + test-support question.** Run it on trunk as a control. If it fails there
   too, it is pre-existing and not a cost of this bump.
5. **Changelog.** A `changelog.d/` fragment. Bumping diesel-async 0.9 → 0.10 is a breaking pin
   for apps that name it, so check whether `docs/migrations/next.md` needs an entry.
6. **Separately, independent of the bump:** the U2 and U3 artifact opens should clear or refuse
   foreign sidecars, or open with `immutable=1` where the file is read-only. This hardening still
   matters after the SQLite fix: a stale or foreign `-wal` changes what "verified" means.

Gates the real build must clear: Ballast (dependency audit, `cargo deny`), Keystone if the
diesel-async 0.10 pin is judged an API break for apps, and the CI lanes above. The scratch tree
is reference only. The production change starts clean from trunk.

## 🔬 Reproduce

```bash
# Q1
curl -sS https://crates.io/api/v1/crates/diesel/2.4.0/dependencies | python3 -c \
  "import json,sys;[print(d['crate_id'],d['req']) for d in json.load(sys.stdin)['dependencies'] if 'sqlite' in d['crate_id']]"
git worktree add --detach ../wt 7e93005 && cd ../wt
sed -i 's/^libsqlite3-sys = { version = "0.38"/libsqlite3-sys = { version = "0.39"/' Cargo.toml
grep -rl 'diesel-async = { version = "0.9"' --include=Cargo.toml . | xargs sed -i 's/diesel-async = { version = "0.9"/diesel-async = { version = "0.10"/'
cargo update -p diesel -p diesel-async -p diesel_migrations -p libsqlite3-sys
cargo tree -i libsqlite3-sys --workspace -e normal --depth 1
grep -m1 'define SQLITE_VERSION ' ~/.cargo/registry/src/*/libsqlite3-sys-0.39.1/sqlite3/sqlite3.h
cargo check -p autumn-web --features sqlite
cargo check -p autumn-web -p autumn-cli --features autumn-web/test-support --lib --bins --tests
# Q2 (the two strongest paths)
sed -n 2777,2840p autumn-cli/src/db/backup.rs; sed -n 1080,1086p autumn-cli/src/db/backup.rs
sed -n 740,815p autumn/src/replication/restore.rs
```

The apparatus has been dismantled: the scratch worktree was removed after the run, and nothing
from it merges.
