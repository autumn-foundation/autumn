# ⚓ Ballast: dependency ledger — 2026-09-04

## 🎯 Class

**Ledger report, plus a scheduled batch.** The dedup candidate found is
gated behind a public-API sign-off this run can't grant itself (see
"Ask-before candidate"), but the lockfile-currency check turned out to be
wrong in its first revision (see "Correction" below) — once run correctly it
surfaced a real routine batch: 32 packages patch/minor-bumped within their
existing `Cargo.toml` ranges, rehearsed and applied in this same PR since
this session is confined to a single branch/PR.

**Correction**: the first revision of this report claimed `cargo update
--dry-run --workspace` showed the lockfile fully current (0 packages behind).
`chatgpt-codex-connector`'s review on this PR caught that `--workspace`
restricts `cargo update`'s scope to workspace *member* packages only (per
`cargo update --help`: "Only update the workspace packages") — the path deps
that never have a newer version to move to — so it was structurally
incapable of finding anything and the "0 packages" result was vacuous, not a
clean bill of health. The unqualified `cargo update --dry-run` actually
audits every crates.io dependency and reported 32 packages behind. Verified
directly, and the batch below is that command's real output, applied and
rehearsed.

## 📈 Evidence

### Tier 1 — deterministic audits (all rerun this cycle)

- **Advisory gate** (`scripts/check-advisories.sh`, cargo-deny 0.20.2 pinned
  per CI): all three audited graphs report `advisories ok` — the workspace
  graph (`deny.toml`, default + Postgres + every additive feature CI builds),
  the SQLite backend graph (`deny-sqlite.toml`), and the day-one scaffold
  graph (`autumn-cli/src/templates/deny.toml.tmpl`). Self-test (injected
  known-vulnerable `time` 0.1.x) still correctly rejects, then accepts once
  waived — the gate is exercising real logic, not rubber-stamping.
- **Licenses / sources**: `cargo deny check licenses sources` on both graphs
  → `licenses ok, sources ok`. No new license class, no unknown registry, no
  git source.
- **Lockfile currency** (corrected — see above): the unqualified `cargo
  update --dry-run` locks 32 packages to newer semver-compatible versions:
  patch/minor bumps (`tower-http` 0.7.0→0.7.1, `hyper` 1.11.0→1.11.1, `h2`
  0.4.18→0.4.19, `lru` 0.18.3→0.18.4, `syn` 3.0.3→3.0.4, `rust_decimal`
  1.42.1→1.43.0, `aws-lc-rs`/`aws-lc-sys`, `diesel`/`diesel_derives`, `mio`,
  `toml`, `tinyvec`, `smallvec`, `tokio-rustls`, `which`, `cc`,
  `find-msvc-tools`, `combine`, `generic-array`, `libredox`,
  `portable-atomic-util`, `proc-macro-error-attr3`/`proc-macro-error3`,
  `async-compression`/`compression-codecs`/`compression-core`,
  `cpufeatures`, `borsh`/`borsh-derive`, `rand`), one same-line downgrade
  (`crypto-common` 0.1.7→0.1.6, still within its own `^0.1` range — cargo's
  resolver settling a different requirement elsewhere in the graph), and 11
  crates dropped entirely as dead weight now that their puller moved past
  needing them (`ahash`, `bitvec`, `bytecheck`, `bytecheck_derive`, `funty`,
  `ptr_meta`, `ptr_meta_derive`, `radium`, `rend`, `rkyv`, `rkyv_derive`,
  `seahash`, `tap`, `wyz` — the old `rkyv`/`bytecheck` serialization stack).
  A further 46 packages remain behind but only across a major-version
  boundary (a real bump needing a forcing fact, not a free `cargo update`).
  This is exactly the "scheduled batch" class from this project's own
  dependency policy — routine, semver-safe, one PR, one rehearsal — so it's
  applied below rather than just reported.
- **Duplicate-version census** (`cargo metadata`, default-feature graph — 28
  workspace members, 857 resolved packages, 829 external): 729 unique
  external crate names, 84 of them resolved at more than one version, 100
  "extra" copies beyond one-per-name. `deny.toml`'s own `[bans]` comment
  already accepts this as "pervasive and cosmetic (RustCrypto old/new,
  windows-sys target shims)" and deliberately keeps `multiple-versions =
  "warn"` out of the CI gate. Spot-checking the list confirms most of it is
  exactly that shape (`elliptic-curve`/`ecdsa`/`p256`/`sec1`/`base16ct`
  0.12-vs-0.13 RustCrypto family; `windows-sys`/`windows_*_gnu` etc.
  0.52-vs-0.60/0.61 target shims; `syn` 1/2/3, `darling` four versions —
  proc-macro build-time weight, but each pinned by a different macro-heavy
  dependency, not by us). One entry is *not* that shape — see below.

### Tier 2 — pain ledger

- Last Ballast PR (#2435, 2026-09-02, two days before this run) closed the
  only two open findings from the prior audit: a stale RUSTSEC-2026-0173
  ignore entry cargo-deny no longer needed (`validator_derive` had already
  migrated off `proc-macro-error2`) and a yanked `chacha20` 0.10.1 (dev-only,
  via `testcontainers`), patched to 0.10.2. Both graphs have reported zero
  warnings since. Nothing new has landed in the three days between that PR
  and this run.
- Three advisory ignores remain active in `deny.toml`/`deny-sqlite.toml`, and
  **all three share the same review-by date: 2026-10-01** (about four weeks
  from today) — `RUSTSEC-2023-0071` (rsa Marvin-attack timing sidechannel, no
  fixed release, RSA-JWT path only), `RUSTSEC-2024-0384` (`instant`
  unmaintained, build-time only via `postgresql_embedded`), and
  `RUSTSEC-2026-0253` (`lru` 0.16.4's non-panic-safe `pop()`, pinned in via
  the MSRV-frozen `aws-sdk-s3` 1.122, unreachable because the S3-cache
  callsite never calls `pop()`). None is reachable, so none is a fire today,
  but a future Ballast run around **2026-10-01** should re-check: has `rsa`
  shipped a fix, has an `instant` successor emerged, and — the more
  actionable one — does a newer `aws-sdk-s3` compatible with this workspace's
  1.88.0 MSRV ceiling exist yet (which would let the `lru` 0.16.4 copy
  disappear entirely rather than staying pinned).

## 💡 Ask-before candidate (not executed) — corrected

**Upgrade, `reqwest` 0.12 → 0.13** — the one duplicate in the census that
isn't cosmetic RustCrypto/windows noise. **Correction**: the first revision
of this report understated the 0.12 footprint and overstated the win;
`chatgpt-codex-connector`'s review on this PR caught both, verified directly
against the graph below and corrected here.

- Three workspace members pin `reqwest = "0.12"` **directly and
  independently** of one another — not just `autumn/Cargo.toml:393`
  (`http-client`/`acme`/S3-signing-cert-fetch) but also
  `autumn-media-plugin/Cargo.toml:44` and `example-e2e/Cargo.toml:13`. All
  three would need to move, not one.
- A fourth path to 0.12 is transitive and outside this workspace's control:
  under the full `deny.toml`-audited feature set (`managed-pg-bundled`
  included), `postgresql_archive` → `reqwest-retry` → `reqwest` resolves to
  0.12 as well (confirmed via `cargo tree -e normal,build -p autumn-web
  --features <the deny.toml set> -i reqwest@0.12.28`). Converging our own
  three pins to 0.13 would **not** remove this copy from the audited graph —
  it depends on `reqwest-retry`/`postgresql_archive` shipping their own bump.
  The original report's default-feature `cargo metadata` run missed this
  entirely because it didn't enable `managed-pg-bundled`.
- `autumn-cli/Cargo.toml` (two spots) pins `reqwest = "0.13"`, and
  `chromiumoxide 0.9` (via `system-tests`) also resolves `reqwest` 0.13.
- **The "3-node prune" claim was wrong.** Both crates named as exclusive to
  the 0.12 subtree are retained independently: `ryu` is also pulled in by
  `aws-smithy-types` (the AWS SDK stack, unrelated to reqwest), and
  `serde_urlencoded` is `autumn`'s own direct dependency
  (`autumn/Cargo.toml:384`), not solely a reqwest transitive. Neither would
  disappear from the graph under any of the changes above. The only node
  that would actually leave, even in the best case (all three of our own
  pins converged, and `reqwest-retry` unrelated), is the `reqwest` 0.12
  crate itself — a 1-node prune, and one that doesn't clear even under the
  best case because of the `postgresql_archive` path. **This candidate does
  not currently clear the Impact Floor** and the recommendation below is
  narrowed accordingly: still worth converging our *own* three pins for
  consistency (one fewer major version to reason about in code we control),
  but it is not the dedup win originally claimed, and a real graph-level
  collapse also needs `reqwest-retry`/`postgresql_archive` to move upstream
  first — track that as the actual forcing-fact gap, not something this
  workspace can close alone.
- **Why this still isn't a PR**: `autumn/src/http_client.rs` (the
  `autumn_web::http::Client` wrapper Autumn ships as its own public API)
  returns `reqwest::StatusCode` and `Option<&reqwest::Url>` directly from
  public methods (`Response::status()`, `Response::url()`), and takes
  `reqwest::Client`/`reqwest::ClientBuilder` in several `pub(crate)`/public
  constructors. Per this project's own dependency-hygiene policy, "bumping
  any dependency whose types appear in this project's public API" is an
  **ask-before** action — reqwest 0.12→0.13 is a semver-major jump for a 0.x
  crate and could change the shape of `StatusCode`/`Url` a downstream
  `autumn_web` consumer's code touches. Flagging here rather than opening the
  PR unasked. If the change is wanted: the migration surface is at least 12
  files under `autumn/src` calling `reqwest::` directly (`http_client.rs`,
  `acme/dns/http.rs`, `alerts.rs`, `auth.rs`, `auth/password.rs`,
  `capsule/schema.rs`, `inbound_mail.rs`, `interceptor.rs`,
  `replication/s3.rs`, `security/captcha.rs`, `shadow/transport.rs`,
  `sync/engine.rs`, `test.rs`), plus whatever `autumn-media-plugin` and
  `example-e2e` touch directly — the reqwest 0.13 changelog/migration notes
  would need to be read against each call site before any rehearsal, per
  this policy's Upgrade-class bar.

## 🔧 Change

**Scheduled batch applied**: unqualified `cargo update` (no manifest changes,
no version-range edits — every package moved within its own existing
`Cargo.toml` constraint), **with one exclusion found by rehearsal**: see
below.

**Rehearsal caught a real regression before it reached CI** (this is what
the rehearsal step is for): `./scripts/pre-push-check.sh` — this repo's own
compile-only mirror of CI's `lint` + `test` jobs — failed on `cargo clippy
--workspace --all-targets -- -D warnings`. Root cause: the batch's
`generic-array` 0.14.7 → 0.14.9 bump deprecates
`GenericArray::<T, N>::from_slice`, which this repo's `-D warnings` gate
promotes to a hard error at 8 call sites across
`autumn/src/{credentials,encryption,mail,push/encryption}.rs` (all going
through `sha1`'s/`aes-gcm`'s re-export of `generic-array`). Confirmed by
reading the diagnostic and the `generic-array` 0.14.9 changelog entry for
that deprecation. **Fix**: excluded just that one package from the batch —
`cargo update -p generic-array --precise 0.14.7` — keeping the other 31
updates. This is a real, if unfortunate, illustration of why a "scheduled
batch" still needs the compile rehearsal and isn't a rubber stamp: a
patch-range bump inside someone else's semver contract still broke us.

Re-ran the advisory/license/source gate against the corrected lockfile
(`./scripts/check-advisories.sh`, `cargo deny check licenses sources` on
both graphs) — still `ok`. Targeted `cargo clippy -p autumn-web --features
acme,mail` (the smallest reproduction of the four affected modules) against
the corrected lockfile: **`Finished` clean, no errors** — confirms the
`generic-array` exclusion actually fixes the regression it was meant to fix.
A full `./scripts/pre-push-check.sh` re-run (covering the rest of the
workspace, not just the four affected modules) was started after this
targeted confirmation; its actual result is recorded in the next commit,
not claimed here ahead of it landing.

## 📊 Measurement

| Metric | Before | After |
|---|---|---|
| Workspace members | 28 | 28 |
| Advisories (workspace / sqlite / scaffold graphs) | ok / ok / ok | ok / ok / ok |
| Licenses / sources (workspace / sqlite graphs) | ok+ok / ok+ok | ok+ok / ok+ok |
| Lockfile entries behind their own semver range (correct command) | 32 | 1 (`generic-array`, excluded by rehearsal) |
| Cargo.lock line count | 10111 | see commit |
| Packages removed entirely (dead transitive weight) | — | 14 |

Duplicate-version census, direct-dependency count, and the reqwest
ask-before analysis are unaffected by this batch (none of the 32 bumps touch
`reqwest`, `ryu`, or `serde_urlencoded`) — those numbers from the original
census stand:

| Metric | Value |
|---|---|
| Resolved packages (incl. workspace) | 857 |
| External (non-workspace) packages | 829 |
| Unique external crate names | 729 |
| Crate names resolved at >1 version | 84 |
| Extra copies beyond one-per-name | 100 |
| Direct `[workspace.dependencies]` entries | 52 |
| Active advisory ignores, all review-by 2026-10-01 | 3 (RUSTSEC-2023-0071, RUSTSEC-2024-0384, RUSTSEC-2026-0253) |

## 🔬 Reproduce

```bash
# Advisory gate (all three graphs + self-test)
./scripts/check-advisories.sh
./scripts/check-advisories.sh --self-test

# Licenses / sources, both graphs
cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources

# Lockfile currency — unqualified, NOT --workspace (--workspace only
# considers the workspace's own path members, which never have a newer
# version to move to, so it always vacuously reports 0)
cargo update --dry-run

# Apply the batch, then re-exclude generic-array (0.14.9 deprecates
# GenericArray::from_slice, which this repo's -D warnings clippy gate
# promotes to a hard error at 8 call sites under autumn/src) BEFORE gating —
# running the bare `cargo update` alone and skipping this step reintroduces
# the exact failure described above.
cargo update
cargo update -p generic-array --precise 0.14.7
./scripts/check-advisories.sh
cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources
./scripts/pre-push-check.sh

# Duplicate-version census
cargo metadata --format-version 1 > /tmp/meta.json
python3 - <<'PY'
import json
d = json.load(open('/tmp/meta.json'))
non_ws = [p for p in d['packages'] if p['id'] not in set(d['workspace_members'])]
names = {}
for p in non_ws:
    names.setdefault(p['name'], set()).add(p['version'])
dups = {n: v for n, v in names.items() if len(v) > 1}
print(len(non_ws), 'external packages;', len(names), 'unique names;', len(dups), 'duplicated')
PY

# reqwest 0.12 vs 0.13 subtree overlap
cargo tree -e normal --package reqwest@0.12.28 --prefix none | sed -E 's/^([a-zA-Z0-9_-]+) v[0-9].*$/\1/' | sort -u > /tmp/rq12.txt
cargo tree -e normal --package reqwest@0.13.4  --prefix none | sed -E 's/^([a-zA-Z0-9_-]+) v[0-9].*$/\1/' | sort -u > /tmp/rq13.txt
comm -23 /tmp/rq12.txt /tmp/rq13.txt   # crate names exclusive to the 0.12 subtree
```
