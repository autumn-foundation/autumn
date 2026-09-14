# ⚓ Ballast: weekly dependency ledger audit, 2026-09-14

Second Ballast pass in this repo, one week after the first
(`docs/reports/2026-09-07-ballast-dependency-ledger-audit.md`, merged as
#2612). That pass built the harness — five audited graphs (root workspace,
SQLite backend, scaffold day-one, `fuzz/`, `examples/island-flock/`) and left
four follow-ups. This pass reruns the harness end to end, re-verifies every
follow-up, and reports what changed.

**Correction, made during this PR's own review**: the first version of this
report claimed a "scheduled batch" opened today would carry an empty
lockfile diff on all three graphs, and that no dependency-only commit exists
anywhere in this repo's history. Both claims were wrong — the first from a
`cargo update` flag misuse, the second from never having checked for
Dependabot activity. See "Scheduled batch" and "Pain ledger" below for the
corrected evidence. The bottom line is unchanged for a different reason:
**no ledger change clears the impact floor this pass**, not because no batch
material exists, but because Dependabot already runs this repo's scheduled
batch, continuously — see below.

## 🎯 Class

Ledger report. No dependency added, removed, bumped, or repinned.

## 📈 Evidence

**Harness re-run, all five graphs, `cargo-deny 0.20.2`** (the exact version
`ci.yml`'s `supply-chain` job pins; installed from the pinned GitHub release
for this pass, matching CI's own `taiki-e/install-action` source):

```
./scripts/check-advisories.sh                 → OK, 0 unwaived advisories, all 5 graphs
./scripts/check-advisories.sh --self-test     → OK, RUSTSEC-2020-0071 blocked then waived, all 4 policies
cargo deny check licenses sources             → licenses ok, sources ok  (root graph)
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok  (SQLite graph)
cargo deny check bans                         → bans ok (warn-level; not CI-gated, see below)
```

Identical outcome to last week's pass on every check. Reachability status of
the three existing root-graph waivers, independently re-checked against
current upstream state (not taken on faith from the pin comment):

| RUSTSEC id | Crate | Last week | This week | Changed? |
| --- | --- | --- | --- | --- |
| RUSTSEC-2023-0071 | `rsa` | max stable 0.9.10, no fix | max stable still 0.9.10 (`0.10.0-rc.18` exists but is a release candidate, not stable) | No |
| RUSTSEC-2024-0384 | `instant` | max 0.1.13, unmaintained | max still 0.1.13, unmaintained | No |
| RUSTSEC-2026-0253 | `lru` (via `aws-sdk-s3`) | pinned `aws-sdk-s3` 1.122.0; 1.123.0+ needs `rust-version` 1.91.0/1.94.1 > our 1.88.0 floor | confirmed via `cargo info aws-sdk-s3@1.123.0`/`@1.146.1`: still 1.91.0 / 1.94.1 respectively; 1.122.0 is still the newest MSRV-compatible release | No |

No code touching `autumn-storage-s3`/`autumn-media-plugin`'s S3-cache path
(the reachability argument for the `lru` waiver: "cache keys are `String`,
`pop()` is never called") landed since last week
(`git log --since=2026-09-08 -- autumn-storage-s3/ autumn-media-plugin/`:
empty). All three waivers' **review-by 2026-10-01** stands, 17 days out — not
due this pass.

**Graph facts, root workspace** (via `cargo deny list --format json`, same
methodology as last week's baseline):

| Metric | 2026-09-07 | 2026-09-14 | Δ |
| --- | --- | --- | --- |
| Unique crate@version nodes | 743 | 791 | +48 |
| Unique crate names | 665 | 704 | +39 |
| Direct (non-dev) deps referenced by workspace members | 131 | 134 | +3 |
| Workspace members | 28 | 33 | +5 |
| Duplicate crate names (`cargo deny check bans`, warn-level) | 73 | 76 | +3 |

The growth tracks real feature landings this week, not scope creep of the
audit itself: `git log` shows mTLS client-cert auth (#1640/#2703),
build-checked service-to-service typed contracts (#1755/#2729), and new
workspace members for billing/search/dunning work. None of it introduced an
advisory, a new license class, or a wildcard/git-ref pin (re-checked, see
below) — the ledger got heavier by ~5% but stayed clean.

**Duplicate-version list re-examined** (76 names, up from 73): still
dominated by the two categories `deny.toml`'s own comment names — the
RustCrypto 0.9/0.10-era split (`aes`, `base16ct`, `block-buffer`, `cipher`,
`const-oid`, `crypto-bigint`, `crypto-common`, `der`, `digest`, `ecdsa`,
`elliptic-curve`, `ff`, `group`, `hmac`, `inout`, `md-5`, `p256`, `pkcs8`,
`rfc6979`, `sec1`, `sha2`, `signature`, `spki` — 23 names) and per-target
`windows-sys`/`windows_*` shims (8 names) — but a **third, uncounted category**
is now large enough to name explicitly: general ecosystem major-version splits
unrelated to either (`base64`, `bitflags`, `darling`/`darling_core`/
`darling_macro`, `getrandom`, `hashbrown`, `http`, `http-body`, `lru`,
`parking_lot`/`parking_lot_core`, `phf`/`phf_shared`, `rand`/`rand_chacha`/
`rand_core`, `reqwest`, `strum`/`strum_macros`, `syn`, `thiserror`/
`thiserror-impl`, `toml`/`toml_datetime`, `tower-http`, `tungstenite`,
`winnow`, and others — 45 names). This category was already the majority of
the 73 last week too; the header comment's "dominated by RustCrypto and
windows-sys" undersells it. Not a new finding this pass and not actioned
(`multiple-versions = "warn"`, deliberately not CI-gated, per `deny.toml`'s
own rationale) — recorded so a future pass doesn't have to re-derive the
breakdown, and flagged as a candidate line to tighten in `deny.toml`'s own
comment the next time that file is touched for an unrelated reason.

**Scheduled batch — methodology correction, real batch material on all three
graphs**:

`cargo update --dry-run --workspace` is the WRONG probe for "how much batch
material exists." `-w`/`--workspace` is a **package-selection** flag —
`cargo update --help`: "Only update the workspace packages" — it restricts
the update targets to this repo's own first-party workspace-member crates,
not their third-party dependencies. Those first-party crates are local path
packages with no registry version to bump, so `--workspace` reliably reports
"Locking 0 packages" on every graph in this repo regardless of how stale the
lockfile actually is. Both this week's first draft and last week's report
(`docs/reports/2026-09-07-...`, "A scheduled batch would carry an empty
lockfile diff today") used this flag and both were wrong about that specific
claim. The correct probe is the **unscoped** dry run (no `SPEC`, no
`--workspace`) or `-p <crate>` against a package known to be behind:

```
cargo update --dry-run --verbose                                (root)         → Locking 74 packages
(cd fuzz && cargo update --dry-run --verbose)                                  → Locking 58 packages
(cd examples/island-flock && cargo update --dry-run --verbose)                → Locking 29 packages
```

All three are genuine, MSRV-compatible moves (`cargo update`'s own
rust-version-aware resolver already filters out anything requiring a newer
`rust-version` than the crate declares — confirmed with `-p async-compression`
and `-p bitflags` spot checks in `fuzz/`, both real and both respecting the
1.88.0 floor). So real batch material exists on **every** graph this pass —
the opposite of the first draft's conclusion. **Not actioned this pass**, for
a different reason than "nothing to do": this repo already runs a mechanical
scheduled-batch process (Dependabot), continuously — see "Pain ledger" below
— and hand-rolling a competing 74-package Ballast batch today would collide
with that process rather than fill a gap in it. Recorded here mainly so the
next pass doesn't re-derive the flag bug: always probe with a bare
`cargo update --dry-run --verbose` (or explicit `-p` specs), never
`--workspace`, on any of the five graphs.

**Supply-chain facts, re-verified**: zero wildcard version ranges and zero
unpinned git refs anywhere in the tree (`grep -rn 'version = "\*"'` /
equivalent `git = "` scan, main workspace + both satellites — both empty,
unchanged from last week).

**Pain ledger — corrected**: the first draft's "no dependency-only commit
anywhere in this repo's history" was checked only against `git log --all
--oneline | grep -i ballast`, which can only find commits *this agent*
authored — it says nothing about the repo's actual dependency-update
history. `.github/dependabot.yml` has run `cargo` + `github-actions` updates
on this repo since well before either Ballast pass, grouped into `rust-deps`
(all crates except `axum*`/`diesel*`/`tokio`, minor+patch only),
`axum-ecosystem`, and `diesel-ecosystem`, weekly on Mondays (today), with no
`auto-merge` workflow wired to it — every one of these PRs goes through a
human merge, not a bot merge. `search_pull_requests
author:app/dependabot` returns **84 PRs** in this repo's history, most
recently three merged in the four days right before last week's Ballast pass
(#2616, #2617, #2629 — then #2640, a 7-update `rust-deps` batch, merged
2026-09-09, one day *after* last week's report). That is exactly the
"scheduled batch" class this charter describes, already running, already
reviewed one PR at a time per group rather than per bump — the opposite of
the "bot spam" failure mode the charter bans. It also explains part of why
this week's unscoped dry run still finds 74 root-graph packages behind:
Dependabot's `rust-deps` group explicitly excludes `axum*`/`diesel*`/`tokio`
from its patterns, so anything in those families (or anything not yet
swept into an open group PR) accumulates until a human opens/merges the next
one.

Two open Dependabot PRs are stale enough to flag as a queue-health
observation (not something this pass merges or unblocks): **#2302**
(`validator` 0.20.0 → 0.21.0, open since 2026-08-24, 3+ weeks) and **#2179**
(a Python `django` bump in `benchmarks/runtime/django`, open since
2026-08-10, 5+ weeks). **#2615** (`dtolnay/rust-toolchain` 1.88.0 → 1.120.0,
open since 2026-09-07) is a toolchain-version bump — squarely this charter's
own "ask before: any change to build toolchains, language versions" —
correctly still sitting open for a human decision, not something Ballast
should merge or nudge.

## 💡 Mechanism / forcing fact

None, for Ballast to act on directly. Every Tier-1 check reruns clean, every
existing waiver's underlying fact is unchanged, and — corrected from the
first draft — real batch material exists on all three graphs (74/58/29
packages) but is already Dependabot's territory, actively worked (84 PRs of
history, most recently the week between the two Ballast passes). Per the
charter, "staying current" is not a forcing fact for Ballast to open a
*second*, competing batch PR on top of a process that already owns this
cadence. A report, not a bump, is the correct outcome — for a corrected
reason from the first draft's.

## 🔧 Change

None to the dependency graph. This report is the only artifact.

## 📊 Measurement

| Check | 2026-09-07 | 2026-09-14 |
| --- | --- | --- |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean |
| Crate@version nodes / names / direct deps (root) | 743 / 665 / 131 | 791 / 704 / 134 |
| Workspace members | 28 | 33 |
| Duplicate crate names (warn-level) | 73 | 76 |
| Scheduled batch, root graph | 0 packages (wrong methodology — see correction) | 74 packages behind, real (unscoped dry run); not actioned, Dependabot's territory |
| Scheduled batch, `fuzz/` graph | not checked | 58 packages behind, real |
| Scheduled batch, `island-flock/` graph | not checked | 29 packages behind, real |
| Dependabot PRs found in repo history | not checked | 84 (`search_pull_requests author:app/dependabot`) |
| Wildcard ranges / unpinned git refs | 0 / 0 | 0 / 0 |
| Existing waivers still valid on re-check | 3/3 | 3/3 |

## 🔬 Reproduce

```
cargo fetch --locked
(cd fuzz && cargo fetch --locked)
(cd examples/island-flock && cargo fetch --locked)
./scripts/check-advisories.sh
./scripts/check-advisories.sh --self-test

cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources
cargo deny check bans
cargo deny list --format json

# scheduled-batch check — NEVER --workspace (it only targets first-party
# path packages and always reports 0); use a bare unscoped dry run instead
cargo update --dry-run --verbose
(cd fuzz && cargo update --dry-run --verbose)
(cd examples/island-flock && cargo update --dry-run --verbose)

# waiver spot-checks
cargo info rsa
cargo info instant
cargo info aws-sdk-s3@1.123.0
```

## Follow-ups still open

1. Waivers sharing the **2026-10-01** review-by date (root `deny.toml`'s
   three, plus `fuzz/deny.toml`'s `RUSTSEC-2023-0071`): re-checked this pass
   and all still hold; not yet due. Revisit properly at that date.
2. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` (`bincode`,
   `yew`-internal, "undetermined" not "unreachable") is still unresolved —
   `build-island.sh` has not rerun since last week
   (`git log --since=2026-09-08 -- examples/island-flock examples/flock/
   static/islands`: empty besides an unrelated `deny.toml` rewrite), so
   there has been no natural point to inspect the compiled `.wasm`'s
   retained symbols. Still open.
3. The NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`
   is still an open human "ask before" question — no decision recorded
   anywhere in `CHANGELOG.md`, `deny.toml`, or `fuzz/deny.toml` since last
   week. Both satellite `deny.toml`s remain advisories+sources only.
4. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run. Deferred again
   this pass — a full instrumented workspace build is expensive relative to
   this pass's finding (nothing else moved), and is better spent once there
   is a specific candidate hire to attribute cost to rather than as a
   blanket sweep.
5. **New this pass.** A human decision on how Ballast and Dependabot should
   divide responsibility: Dependabot already delivers the mechanical
   "scheduled batch" (grouped, weekly, human-merged), but its PRs get none
   of this charter's reachability join, license-class diffing, or usage
   analysis — a Dependabot PR could in principle bump past a fix that trades
   one advisory for another, or introduce a new license class, with nothing
   in its own pipeline to catch it. Worth deciding whether a future Ballast
   pass should specifically review open/recently-merged Dependabot PRs
   against this charter's evidence bar, rather than treating "batch" as
   Ballast's own job to originate.
6. **New this pass.** Two open Dependabot PRs are stale (#2302, 3+ weeks;
   #2179, 5+ weeks) — a queue-health signal, not something this pass acted
   on. #2615 (a toolchain bump) is correctly held open pending a human "ask
   before" decision.
