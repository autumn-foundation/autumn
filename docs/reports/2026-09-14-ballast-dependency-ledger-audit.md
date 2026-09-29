# ⚓ Ballast: weekly dependency ledger audit, 2026-09-14

Second Ballast pass in this repo, one week after the first
(`docs/reports/2026-09-07-ballast-dependency-ledger-audit.md`, merged as
#2612). That pass built the harness — five audited graphs (root workspace,
SQLite backend, scaffold day-one, `fuzz/`, `examples/island-flock/`) and left
four follow-ups. This pass reruns the harness end to end, re-verifies every
follow-up, and reports what changed.

**Corrections, made across three rounds of this PR's own review**: the first
draft claimed a "scheduled batch" opened today would carry an empty lockfile
diff on all three graphs, and that no dependency-only commit exists anywhere
in this repo's history. Both were wrong (a `cargo update` flag misuse, and
never having checked for Dependabot activity). The second draft
over-corrected by treating all three graphs as covered by Dependabot's
existing cadence — also wrong: Dependabot only covers the root graph; the
two satellite graphs' batch material is genuinely uncovered by any process.
The second draft also mis-investigated a `git log` check on the `lru`
waiver and, working from a **shallow git clone**, concluded `trunk-dev` had
been force-pushed/rewritten — a false alarm, fully retracted in this
revision (see follow-up 8 and the waiver section below); the shallow clone
made an ordinary boundary commit look rootless, nothing more. This revision
also narrows an overclaim about the `island-flock` batch's MSRV
compatibility (see "Scheduled batch" below). See "Scheduled batch" and
"Pain ledger" below for the fully corrected evidence, and follow-ups 5–8 for
what's still open. The bottom line is unchanged for a different reason than
any earlier draft gave: **no ledger change clears the impact floor this
pass** — not because there's nothing to do, but because the root graph's
batch material is already someone else's job in progress, and the two
satellites' batch material needs its own rehearsal this pass didn't budget
for.

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

**Correction, twice over**: the first draft claimed this `git log` came back
empty; it does not — `git log --since=2026-09-08 -- autumn-storage-s3/
autumn-media-plugin/` returns a real commit and needed inspecting, not
waving off. The second draft inspected it, but from a **shallow clone**
(this session's checkout was `git clone --depth`-limited), which made
`a4c8fb5` (the actual commit, PR #2700) look unreachable and made an
unrelated commit sitting at the shallow boundary (`63e8342`) look like it
had a mismatched-parent diff touching these directories — an artifact of the
shallow boundary treating that commit as rootless, not a real `trunk-dev`
history rewrite. Retracted in full: `git fetch --unshallow` confirms
`a4c8fb5` is and always was a normal ancestor of `origin/trunk-dev`, and
`63e8342`'s real diff (against its real parent) touches only
`CHANGELOG.md`, `README.md`, `compile_fail.rs`, and
`docs/guide/getting-started.md` — nothing in either S3/media directory.
Apologies for the false alarm this cost a review round to catch; the
git-history-rewrite finding is removed (was follow-up 8 in an earlier
revision) and any push notification claiming it stands corrected here.

With full history restored, `a4c8fb5` is the real, relevant commit: it
touches `autumn-media-plugin/` (room-heartbeat feature, config validation,
docs) but not `autumn-storage-s3/` at all, and its only change to
`storage.rs` is a visibility fix (`fn is_tigris_endpoint` →
`pub(crate) fn`), unrelated to caching. Confirmed directly against current
file content too: `grep -n "pop(\|LruCache\|CacheKey"
autumn-storage-s3/src/lib.rs` returns nothing — Autumn's own code never
touches `lru` directly. `lru` is pulled in purely as a transitive dependency
of `aws-sdk-s3`'s own internal S3 Express session cache (both crates'
`Cargo.toml`s pin `aws-sdk-s3 >= 1.122` specifically to get `lru >=
0.16.3`, per their own comments), so the waiver's reachability question
turns on `aws-sdk-s3`'s own vendored behavior, unaffected by any commit in
this repo as long as the `aws-sdk-s3` pin itself doesn't move, which it
hasn't (see the waiver table above). All three waivers' **review-by
2026-10-01** stands, 17 days out — not due this pass.

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

All three counts are real. Root and `fuzz/` both declare `rust-version =
"1.88.0"`, and `cargo update`'s own rust-version-aware resolver filters
candidates against that floor — confirmed with `-p async-compression` and
`-p bitflags` spot checks in `fuzz/`, both real and both respecting 1.88.0.
**Correction**: `examples/island-flock/Cargo.toml` declares **no
`rust-version` at all** (just `edition = "2024"`), so its "Locking 29
packages to latest Rust 1.94.1 compatible versions" message reflects this
sandbox's active toolchain (1.94.1), not a real declared floor — an earlier
revision's "all three are MSRV-compatible" claim overstated confidence for
this graph specifically. The 29-package count is real batch material, but
whether all 29 selections would work against whatever toolchain
`build-island.sh` / CI actually uses for the `wasm32-unknown-unknown` build
is unverified; narrowing the claim to "real, uncertain-MSRV" rather than
"real, MSRV-compatible" for this one graph. So real batch material exists on
**every** graph this pass — the opposite of the first draft's conclusion,
with island-flock's compatibility specifically unconfirmed.

**Correction**: the second draft claimed all three graphs are "Dependabot's
territory" and left all three unactioned on that basis — also wrong.
`.github/dependabot.yml`'s only `package-ecosystem: cargo` entry is
`directory: /`; `fuzz/` and `examples/island-flock/` are each excluded from
the root workspace specifically *because* they're independent workspaces
with their own `Cargo.lock` (see `deny.toml`'s own header), and Dependabot
has no config entry for either directory. Only the **root graph's 74
packages** are genuinely Dependabot's territory. The **58-package `fuzz/`
batch and the 29-package `island-flock/` batch are uncovered by any
process** — not Dependabot's, and not actioned by Ballast this pass either,
so they are a real, open gap rather than a covered one. Recorded as a new
follow-up below (extend `dependabot.yml` with two more `cargo` entries, one
per satellite directory, or have Ballast pick up satellite batches on its
own cadence) rather than actioned in this PR, since evaluating which of
those 58+29 packages needs its own rehearsal is real, separate work this
pass didn't budget for: `fuzz/`'s is `RUSTFLAGS="--cfg fuzzing" cargo
+nightly check --workspace`, but `island-flock/`'s is **not** a bare
`cargo check` — `docs/guide/wasm-islands.md` requires `wasm-bindgen-cli`
pinned to the *exact* `wasm-bindgen` library version the crate resolves to
("a mismatch produces" broken output), and `build-island.sh` does a real
`--release` build through the matching CLI, not a type-check. Since this
batch includes a `wasm-bindgen` bump (`0.2.126` → `0.2.128` among the 29),
a real rehearsal means running `build-island.sh` end to end with a
correspondingly-bumped `wasm-bindgen-cli`, not `cargo check`. Recorded here
mainly so the next pass doesn't re-derive the flag bug: always probe with a
bare `cargo update --dry-run --verbose` (or explicit `-p` specs), never
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
author:app/dependabot` returns **84 PRs** in this repo's history.

**Correction**: the timeline for the most recent ones was wrong in an
earlier revision. Checking each PR directly (`pull_request_read`, not
inferred from search-result `created_at` or from commit messages read too
quickly): **#2616** and **#2629** were each opened, then **closed without
being merged** (`"merged": false`) as Dependabot recreated the same
`rust-deps` group update with a fresher diff — ordinary Dependabot
behavior, not two real landings. Only **#2617** (`tower-http`,
axum-ecosystem) and **#2640** (7-update `rust-deps` batch) were actually
merged, both by a human (`merged_by: madmax983`) at **2026-09-10
18:57–18:59**, two days *after* last week's Ballast pass (2026-09-08) —
not "before" it, and not on 09-09. That is exactly the "scheduled batch"
class this charter describes, already running, already reviewed one PR at
a time per group rather than per bump — the opposite of the "bot spam"
failure mode the charter bans. It also explains part of why this week's
unscoped dry run still finds 74 root-graph packages behind: Dependabot's
`rust-deps` group explicitly excludes `axum*`/`diesel*`/`tokio` from its
patterns, so anything in those families (or anything not yet swept into an
open group PR) accumulates until a human opens/merges the next one.

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

None, for Ballast to act on directly this pass. Every Tier-1 check reruns
clean, every existing waiver's underlying fact is unchanged, and — corrected
twice from the first draft — real batch material exists on all three graphs
(74/58/29 packages): the root graph's 74 is genuinely Dependabot's territory
(84 PRs of history, actively worked, most recently the week between the two
Ballast passes), but the `fuzz/` and `island-flock/` batches (58 + 29
packages) are uncovered by any process — Dependabot's `cargo` config only
targets `directory: /`. Per the charter, "staying current" alone isn't a
forcing fact, so this pass reports the gap rather than rehearsing two
unplanned satellite batches on the spot — see follow-up 7. A report, not a
bump, is the correct outcome, but "nothing to do" was never the accurate
reason and this version doesn't claim it is.

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
| Scheduled batch, root graph | 0 packages (wrong methodology — see correction) | 74 packages behind, real; not actioned, Dependabot's territory (root-only) |
| Scheduled batch, `fuzz/` graph | not checked | 58 packages behind, real; **uncovered by any process** |
| Scheduled batch, `island-flock/` graph | not checked | 29 packages behind, real; **uncovered by any process**; MSRV-compatibility unconfirmed (no declared `rust-version`) |
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
   `yew`-internal, "undetermined" not "unreachable") is still unresolved.
   With full (unshallowed) history, `git log --since=2026-09-08 --
   examples/island-flock examples/flock/static/islands` is genuinely empty
   — an earlier revision of this report misread a shallow-clone artifact
   here too (see the waiver correction above); the real answer was "no
   change" all along. `build-island.sh` genuinely has not rerun since last
   week, so there has been no natural point to inspect the compiled
   `.wasm`'s retained symbols. Still open.
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
5. **New this pass, corrected during review.** A human decision on how
   Ballast and Dependabot should divide responsibility, narrowed from an
   earlier overstatement: `ci.yml`'s `pull_request` trigger fires on every
   PR including Dependabot's, so its `supply-chain` job (`
   scripts/check-advisories.sh` + the blocking root/SQLite license
   allow-list checks) already runs against every root-graph Dependabot PR —
   a new unwaived advisory or a disallowed license class IS caught
   mechanically today. What genuinely has no automated coverage is
   **reachability** (an advisory that's newly *waivable* vs. genuinely fixed
   needs the same human judgment call this charter's `[advisories] ignore`
   entries required) and **usage/cost analysis** (whether a bump actually
   exercises new surface, changes build time, or grows the transitive
   tree). Worth deciding whether a future Ballast pass should specifically
   apply that layer to open/recently-merged Dependabot PRs, rather than
   treating "batch" as Ballast's own job to originate.
6. **New this pass.** Two open Dependabot PRs are stale (#2302, 3+ weeks;
   #2179, 5+ weeks) — a queue-health signal, not something this pass acted
   on. #2615 (a toolchain bump) is correctly held open pending a human "ask
   before" decision.
7. **New this pass.** The `fuzz/` (58 packages) and `island-flock/` (29
   packages) scheduled batches are genuinely uncovered by any process —
   Dependabot's only `cargo` entry targets `directory: /`, not either
   satellite workspace. Needs a human decision: extend `dependabot.yml` with
   two more directory entries, or have Ballast own satellite-graph batches
   on its own cadence (each needs its own rehearsal command; see the
   "Scheduled batch" section above). Separately, `examples/island-flock/
   Cargo.toml` declares no `rust-version` at all, unlike the root and
   `fuzz/` graphs (both `1.88.0`) — worth deciding whether it should have
   one, since without it there's no floor for `cargo update` to respect and
   no easy way to state "this batch is MSRV-safe" the way the other two
   graphs' reports can.
8. **Retracted.** An earlier revision of this report claimed `trunk-dev` had
   been force-pushed/rewritten, based on `a4c8fb5` appearing unreachable and
   `63e8342` appearing to carry a mismatched-parent diff. Both symptoms came
   from this session's own **shallow git clone** (`63e8342` sat at the
   fetch-depth boundary, which makes a commit look rootless and its diff
   look like it touches every long-lived file). `git fetch --unshallow`
   confirms `trunk-dev`'s real history is intact and was never rewritten —
   there was no incident. Left here, marked retracted, rather than deleted
   outright, since a prior revision's PR comments and a push notification
   referenced it and should point at a correction, not a silently vanished
   line.
