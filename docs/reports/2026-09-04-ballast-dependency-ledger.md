# ⚓ Ballast: dependency ledger — 2026-09-04

## 🎯 Class

**Ledger report.** No ledger change this cycle — every check below came back
clean or already covered by an existing waiver, and the one real dedup
candidate found is gated behind a public-API sign-off this run can't grant
itself (see "Ask-before candidate").

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
- **Lockfile currency**: `cargo update --dry-run --workspace` → *"Locking 0
  packages to latest Rust 1.88.0 compatible versions"*. Every direct pin in
  `Cargo.toml` is already at the newest version its own semver range allows;
  the 76 packages `--verbose` reports "behind latest" are all held back by a
  major-version boundary (a real bump, not a free `cargo update`), so there is
  no routine patch/minor batch to take this cycle.
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

None. No `Cargo.toml`/`Cargo.lock` diff in this cycle — see Class above.

## 📊 Measurement (baseline for the next cycle)

| Metric | Value |
|---|---|
| Workspace members | 28 |
| Resolved packages (incl. workspace) | 857 |
| External (non-workspace) packages | 829 |
| Unique external crate names | 729 |
| Crate names resolved at >1 version | 84 |
| Extra copies beyond one-per-name | 100 |
| Direct `[workspace.dependencies]` entries | 52 |
| Advisories (workspace / sqlite / scaffold graphs) | ok / ok / ok |
| Licenses / sources (workspace / sqlite graphs) | ok+ok / ok+ok |
| Lockfile packages behind their own semver range | 0 |
| Active advisory ignores, all review-by 2026-10-01 | 3 (RUSTSEC-2023-0071, RUSTSEC-2024-0384, RUSTSEC-2026-0253) |

## 🔬 Reproduce

```bash
# Advisory gate (all three graphs + self-test)
./scripts/check-advisories.sh
./scripts/check-advisories.sh --self-test

# Licenses / sources, both graphs
cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources

# Lockfile currency (0 packages = nothing behind within semver range)
cargo update --dry-run --workspace

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
