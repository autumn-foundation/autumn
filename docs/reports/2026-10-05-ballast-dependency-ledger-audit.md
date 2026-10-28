# ⚓ Ballast: weekly dependency ledger audit, 2026-10-05

Fifth Ballast pass, one week after the fourth
(`docs/reports/2026-09-28-ballast-dependency-ledger-audit.md`), which reran the
harness clean and left nine open follow-ups, including a timing gap it flagged
for the first time: every waiver's review-by date (2026-10-01) would pass four
days *before* this pass could look at it. That gap is exactly what happened,
and it happened alongside something bigger: between 2026-09-29 and
2026-09-30, a human (with Claude Code) went through the open Dependabot queue
this report series had tracked as "a human review/merge decision" for five
consecutive passes and resolved essentially all of it — merging seven of the
nine structural-core PRs, and posting a fully rehearsed negative result on
each of the remaining two. This pass reruns the harness end to end, re-checks
every open follow-up and every overdue waiver, traces the one real duplicate-
version movement, and closes out the two PRs the human review left for
Ballast to finish per its own written recommendation.

## 🎯 Class

Policy. Target: `.github/dependabot.yml` (two `ignore` entries) plus pin
maintenance (review-by dates) in `deny.toml`, `deny-sqlite.toml`,
`fuzz/deny.toml`, and `examples/island-flock/deny.toml`. No dependency added,
removed, or bumped by this pass.

## 📈 Evidence

**Harness re-run, all five graphs, `cargo-deny 0.20.2`** (installed fresh
this session from the exact version `ci.yml`'s `supply-chain` job pins; not
present at session start):

```
./scripts/check-advisories.sh                 → OK, 0 unwaived advisories, all 5 graphs
./scripts/check-advisories.sh --self-test     → OK, RUSTSEC-2020-0071 blocked then waived, all 4 policies
cargo deny check licenses sources             → licenses ok, sources ok  (root graph, 18 license buckets)
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok  (SQLite graph)
cargo deny check bans                         → 90 duplicate-name warnings (warn-level; not CI-gated)
```

Identical clean outcome to all four prior passes on advisories/licenses/
sources. `bans` moved (84 → 90) — traced below.

**The queue, cleared.** Last pass's table of 11 open Dependabot PRs (10 of
them 21–77 days old) is now down to **0**. Checked individually
(`pull_request_read`), not inferred from the open-PR search alone:

| PR | Title | Outcome | When |
| --- | --- | --- | --- |
| #1891 | `actions/checkout` 4→7 | **Merged** | 2026-09-30 |
| #1894 | `tokio-tungstenite` 0.29→0.30 | **Merged** | 2026-09-30 |
| #1895 | `sha1` 0.10.6→0.11.0 | **Merged** | 2026-09-30 |
| #1896 | `x509-parser` 0.16.0→0.18.1 | **Merged** | 2026-09-30 |
| #1898 | `rand_chacha` 0.9.0→0.10.0 | **Closed, not merged** | 2026-09-30 |
| #1899 | `rand` 0.9.4→0.10.2 | **Merged** | 2026-09-29 |
| #2081 | `actions/upload-artifact` 4→7 | **Merged** | 2026-09-29 |
| #2302 | `validator` 0.20.0→0.21.0 | **Merged** | 2026-09-29 |
| #2615 | `dtolnay/rust-toolchain` 1.88.0→1.120.0 | **Closed this pass** | 2026-10-05 |
| #1897 | `matchit` 0.8.4→0.9.2 | **Closed this pass** | 2026-10-05 |
| #2890 | `jsonwebtoken` 10.4.0→11.1.0 | **Merged** | 2026-09-29 |

Nine of eleven were resolved by the repo's own maintainer before this pass
started; this pass resolved the final two. #1898 (`rand_chacha` as its own
PR) was superseded, not ignored: #1899's merge commit message says "chore:
migrate to rand 0.10 / rand_chacha 0.10", and `rand_chacha` is now a **new
direct dependency** in `autumn/Cargo.toml` (previously only transitive via
`rand`) — confirmed by diffing `autumn/Cargo.toml` across that commit, not
assumed from the title. That is the root-graph direct-dependency count's
+1 this week (139 → 140).

**#2890 (`jsonwebtoken` 10→11) merged with the exact fix last pass's
rehearsal called for, verified by re-running the same check.** Last pass
found `jwk_allowed_algorithms` (`autumn/src/auth.rs:1447`) didn't compile
under `--features oauth2` because `jsonwebtoken::jwk::AlgorithmParameters`
went `#[non_exhaustive]` in 11.0 and the match had no wildcard arm. Reading
the merged source today, the match now carries exactly that arm:

```rust
AlgorithmParameters::OctetKey(_) => Err(...),
// `Other` (a `kty` this crate does not recognise) and any variant a
// future `jsonwebtoken` adds — the enum is `#[non_exhaustive]` since
// 11.0 — cannot verify a signature here, so fail closed rather than
// guess an algorithm family.
_ => Err(crate::AutumnError::unauthorized_msg(
    "unsupported jwk key type for id_token verification",
)),
```

Reproduced directly rather than taken on trust: `cargo check -p autumn-web
--no-default-features --features oauth2` is clean today (`Finished` in 71s,
only pre-existing dead-code warnings, no E0004). `fuzz/Cargo.lock` is synced
to `jsonwebtoken 11.1.0` too, so the satellite advisory gate's `--locked`
fetch doesn't go stale — the second gap last pass's rehearsal flagged.

**#1897 and #2615: the maintainer's own rehearsals, finished per their own
recommendation.** Neither PR was merged — both carry a `MEMBER`-authored
comment (2026-09-29/30, via Claude Code) that reproduces a real regression
and explicitly asks for a close + Dependabot ignore:

- **#1897 (`matchit` 0.8.4→0.9.2):** rehearsed by merging trunk-dev locally
  and running the suite against matchit 0.9.2. Finding:
  `plugin_sandbox::manifest::tests::a_route_path_the_router_would_refuse_is_refused_here`
  fails — matchit 0.9 accepts `/hello/a{b}c` (multiple things in one path
  segment) where axum 0.8.9's own matchit 0.8.4 still rejects it, so a
  sandboxed plugin manifest that passes validation under 0.9.2 would panic
  the router at boot. `autumn/Cargo.toml` pins `matchit = "=0.8.4"` on
  purpose, in lockstep with whatever axum itself depends on, specifically so
  the `#[secured]`/conflict-oracle check and axum's own routing agree.
- **#2615 (`dtolnay/rust-toolchain` 1.88.0→1.120.0):** the PR's own CI was
  red on the change itself (`MSRV (1.88.0)`, `Test (*)`, `Windows Tier 1
  journey`, `SemVer check`). `ci.yml`'s MSRV job and trybuild step
  deliberately install the workspace's pinned `rust-version = "1.88.0"`;
  `publish-gate.yml`'s SemVer check deliberately installs 1.94.1 (the
  aws-smithy MSRV floor — later toolchains ICE in cargo-semver-checks, per
  `scripts/check-semver.sh`'s own comment). Moving either via this action
  bump breaks the lane it exists to gate; no merge or source fix changes
  that.

Both comments ended "closing this PR" / "it should be closed, or `matchit`
added to the Dependabot ignore list" — but neither PR was actually closed,
and `.github/dependabot.yml` had no `ignore:` section, so Dependabot was
still free to reopen both on its next weekly run. Six and five days,
respectively, of a written decision sitting unactioned. **This pass finished
it**: added

```yaml
ignore:
  - dependency-name: "matchit"
```

(no `versions:` filter — Dependabot filters only the versions an ignore
rule names, so a scoped `>=0.9` would still leave an 0.8.5/0.8.6 patch
bump eligible, which would desync from axum's own exact `=0.8.4` pin the
same way 0.9.2 would; caught by a Codex review comment on this PR's own
diff and broadened before merge) to the `cargo` ecosystem block and

```yaml
ignore:
  - dependency-name: "dtolnay/rust-toolchain"
```

to the `github-actions` block, each with an inline comment naming the
rehearsal, the mechanism, and the revisit trigger (an axum release that
moves to matchit 0.9; a deliberate MSRV/semver-floor bump done together with
`rust-version`/`check-semver.sh`). Validated with a YAML parse
(`python3 -c "import yaml; yaml.safe_load(...)"`) before relying on it.
Closed both PRs with a comment pointing at the existing rehearsal rather than
re-deriving it, referencing the new ignore entries.

**Duplicate-version increase, traced to the same merge window, same
methodology as last pass's servo-stack trace.** The 6 new duplicate names
(84 → 90) are `x509-parser`, `der-parser`, `asn1-rs`, `asn1-rs-derive`,
`nom`, `oid-registry`:

```
$ cargo tree ... -i x509-parser@0.16.0
x509-parser v0.16.0
└── webauthn-rs-core v0.5.5
    └── webauthn-rs v0.5.5
        └── autumn-web v0.8.0

$ cargo tree ... -i x509-parser@0.18.1
x509-parser v0.18.1
└── autumn-web v0.8.0
```

#1896's merge bumped `x509-parser` as a **direct** dependency of
`autumn-web` (`tls` feature) from 0.16 to 0.18, but `webauthn-rs-core`
(`webauthn` feature) still depends on the old `0.16` line transitively — so
the tree now carries both, and with them both versions of x509-parser's own
`der-parser`/`asn1-rs`/`nom`/`oid-registry` chain. A second commit in the
same window, `82ec5ffb` ("Fail fast when the mTLS CRL set does not cover
every CA in the bundle", #2706 item 1, merged 2026-09-30), is **not** part of
this mechanism — it only added the `verify` feature flag to the existing
x509-parser dependency line, not a version or crate change, confirmed by
reading the diff directly rather than assuming from the commit's proximity.
Not actioned, for the same reason as last week's ammonia/html5ever finding:
`[bans] multiple-versions = "warn"` is deliberately non-blocking, and
deduping means waiting on `webauthn-rs` to move its own x509-parser pin, not
something to force from this side.

**The new-crate-name trace, corrected.** An earlier draft of this report
attributed the 686→687 unique-crate-name increase to `rand_chacha`
"becoming direct." A Codex review comment on this PR's own diff caught that
this doesn't hold: `rand_chacha` was already a name in the graph
(transitively, through `rand`) before #1899's merge, so promoting it to a
direct dependency changes the direct-dependency *count*, not the *set* of
names — it cannot be the source of a name-count delta at all. Re-derived
properly this time, by diffing the full `Cargo.lock` name list against a
commit from before this week's window (`7e33bfc2`, the last commit at or
before 2026-09-28 end-of-day) rather than guessing from proximity:

```
$ comm -13 names_before.txt names_now.txt   # added
card-validate
$ comm -23 names_before.txt names_now.txt   # removed
(empty)
```

One name added, none removed — `card-validate` v2.4.0, confirmed via
`cargo tree -i card-validate` to have exactly one path into the graph:

```
card-validate v2.4.0
└── validator v0.21.0
    └── autumn-web v0.8.0
```

`validator` 0.20.0's own dependency list (from #2302's `Cargo.lock` diff)
was `idna, once_cell, regex, serde, serde_derive, serde_json, url,
validator_derive`; 0.21.0's is `card-validate, idna, regex, serde,
serde_derive, serde_json, url, validator_derive` — `once_cell` dropped,
`card-validate` added. `once_cell` didn't disappear from the full lockfile
(other crates still use it), so the net full-lockfile name delta from this
one swap is exactly +1, matching the observed count precisely. `validator`
#2302's bump (not #1899's `rand_chacha` migration) is the real mechanism.

**Graph facts, root workspace**:

| Metric | 2026-09-28 | 2026-10-05 | Δ | Mechanism |
| --- | --- | --- | --- | --- |
| Unique crate@version nodes | 774 | 779 | +5 | x509-parser chain duplication (+6 node-pairs' worth of new versions) partly offset by other movement in the same merge window |
| Unique crate names | 686 | 687 | +1 | `card-validate`, a brand-new name (see below) — `rand_chacha` does **not** explain this: it was already a name in the graph (transitively, via `rand`) before becoming direct, so promoting it changes the direct-dependency count, not the name set |
| Direct (non-dev) deps referenced by workspace members | 139 | 140 | +1 | `rand_chacha` became a **direct** dependency of `autumn-web` during the rand 0.9→0.10 migration (#1899's merge commit: "migrate to rand 0.10 / rand_chacha 0.10") — confirmed by diffing `autumn/Cargo.toml` across that commit, not inferred from the count |
| Workspace members | 37 | 37 | 0 | — |
| Duplicate crate names (`cargo deny check bans`, warn-level) | 84 | 90 | +6 | x509-parser direct-dep bump stranding `webauthn-rs-core`'s old 0.16 chain (traced above) |

**Waivers, independently re-checked against current upstream state — now 4
days past every root/fuzz waiver's review-by date, the timing gap last pass
flagged materializing exactly as predicted.** All five facts are unchanged
from the last four passes, re-verified via `cargo info` rather than assumed
stale-but-fine:

| RUSTSEC id | Crate | Still true today? |
| --- | --- | --- |
| RUSTSEC-2023-0071 | `rsa` | max stable still 0.9.10; `0.10.0-rc.18` still an RC, no fix |
| RUSTSEC-2024-0384 | `instant` | max still 0.1.13, unmaintained |
| RUSTSEC-2026-0253 | `lru` (via `aws-sdk-s3`) | `aws-sdk-s3@1.123.0` confirmed still `rust-version 1.91.0` (latest overall 1.152.0); our pinned 1.122.0 still the newest MSRV (1.88.0)-compatible release |
| RUSTSEC-2024-0370 | `proc-macro-error` (island-flock) | max still 1.0.4, unmaintained |
| RUSTSEC-2025-0141 | `bincode` (island-flock) | reachability still undetermined — `git log --since=2026-09-28 -- examples/island-flock examples/flock/static/islands` is empty (only the unrelated anymap2 config-only commit touched that tree), so `build-island.sh` still hasn't rerun |

Per the charter ("the reason and the revisit trigger ... recorded next to
the pin" — part of VERIFY, not optional once a trigger date arrives), this
pass **re-dated all five** review-by annotations from 2026-10-01 to
2026-11-02 (`deny.toml` ×3, `deny-sqlite.toml` ×3, `fuzz/deny.toml` ×1,
`examples/island-flock/deny.toml` ×2), each with "reconfirmed unchanged
2026-10-05" appended so the next pass can see this was an active check, not
a silently stale date. 2026-11-02 also aligns the whole tree's waivers onto
the same checkpoint as `island-flock`'s existing `RUSTSEC-2026-0319` (anymap2)
entry, added 2026-10-02. Re-ran `./scripts/check-advisories.sh` after editing
to confirm the gate still passes with the new strings (`cargo-deny` matches
waivers by id, not by reason text, but worth confirming nothing else broke).

**Scheduled batch, all three graphs** (bare `cargo update --dry-run
--verbose`, `grep Locking`):

| Graph | 2026-09-28 | 2026-10-05 |
| --- | --- | --- |
| root (Rust 1.88.0) | 86 packages | 98 packages |
| `fuzz/` (Rust 1.88.0) | 71 packages | 78 packages |
| `examples/island-flock/` (no declared `rust-version`; this session's toolchain) | 31 packages | 31 packages |

Root and `fuzz/` both grew, net of this window's real merges (the
nine-PR queue clearance moved a lot of real dependency state even where the
lockfile already matched a merged PR's own regeneration). `island-flock/`
held flat — consistent with the empty `git log` for that tree. Ownership
unchanged from all four prior passes: root is Dependabot's territory;
`fuzz/` and `island-flock/` remain uncovered by any *scheduled-batch*
process (follow-up, still open).

**Supply-chain facts, re-verified**: zero wildcard version ranges, zero
unpinned git refs, main workspace and both satellites — unchanged across all
five passes.

**License inventory**: 18 license buckets in the root graph (`0BSD`,
`Apache-2.0` ×569, `Apache-2.0 WITH LLVM-exception` ×10, `BSD-1-Clause`,
`BSD-2-Clause`, `BSD-3-Clause` ×15, `BSL-1.0`, `CC0-1.0`, `CDLA-Permissive-2.0`,
`ISC`, `LGPL-2.1-or-later`, `MIT` ×688, `MIT-0`, `MPL-2.0` ×6, `PostgreSQL`,
`Unicode-3.0` ×19, `Unlicense`, `Zlib`). `cargo deny check licenses` passing
clean confirms the x509-parser chain's new transitive crates
(`der-parser`/`asn1-rs`/`asn1-rs-derive`/`nom`/`oid-registry`) introduced no
new license class — no manual diff needed beyond that pass/fail signal.

**GitHub-native Dependabot alert count** (follow-up 8, highest priority) —
**moved sharply, in the direction that matters.** This pass's own `git push`
(of the commit carrying this report) printed **16 vulnerabilities (4 high,
10 moderate, 2 low)**, up from last pass's 9 (2 high, 7 moderate, 0 low):
total +7, high **doubled** (2→4), moderate +3, low +2. The two prior passes
had the "high" count flat at 2, which this report called "worth a human's
attention regardless of how the moderates moved" — it just moved. This
session still has no tool that lists the individual alerts (GitHub MCP
tools searched again this pass; nothing exposes the Security tab), so which
specific advisories are behind the new highs is not established here, and
neither is whether they land inside this repo's own `deny.toml`-audited
graph, the scaffold template's graph, a satellite (`fuzz/`,
`island-flock/`), or one of the two non-Rust manifests
(`examples/react-graphql/frontend/package-lock.json`,
`benchmarks/runtime/django`) that this ledger's `cargo deny` harness does
not cover at all. Given `cargo deny check advisories` still reports 0
unwaived findings across all 5 Rust graphs this pass, the two new highs are
either in one of those two uncovered manifests, or GitHub's advisory
database added matches this session's harness hasn't re-synced against.
Escalated to the user directly rather than left for next week's pass to
re-discover.

## 💡 Mechanism / forcing fact

**Policy**, for the two closed PRs: both carried a written, rehearsed,
reproduced regression from the repo's own maintainer, explicitly
recommending a Dependabot ignore and a close — sitting unactioned for 5-6
days with no ignore list in place, meaning Dependabot would reopen both on
its next weekly run. That is a real gap (decided work not yet recorded in
config), not a chore or a fire, and closing it is exactly what "ask before"
does *not* cover (it is not an addition, a hard-domain change, a toolchain
change, or an API-surface bump — it is declining two already-declined
bumps). No forcing fact beyond that for anything else this pass: every
Tier-1 check reruns clean, every waiver's underlying fact is unchanged (now
reconfirmed and re-dated rather than left silently overdue), and the one
real duplicate-version movement traces to an already-merged, already
CI-verified PR with no action available from this side.

## 🔧 Change

- `.github/dependabot.yml`: added an `ignore` entry under the `cargo`
  ecosystem for `matchit` (unversioned — every update is ignored, not just
  0.9+; see Evidence), and an `ignore` entry under the
  `github-actions` ecosystem for `dtolnay/rust-toolchain`, each with an
  inline comment naming the mechanism and the revisit trigger.
- Closed PR #1897 (`matchit` 0.8.4→0.9.2) and PR #2615
  (`dtolnay/rust-toolchain` 1.88.0→1.120.0), each with a comment pointing to
  the maintainer's own existing rehearsal and the new ignore entry.
- `deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`,
  `examples/island-flock/deny.toml`: re-dated five review-by annotations
  (RUSTSEC-2023-0071, RUSTSEC-2024-0384, RUSTSEC-2026-0253 in the first
  three; RUSTSEC-2024-0370, RUSTSEC-2025-0141 in the last) from 2026-10-01 to
  2026-11-02, each marked "reconfirmed unchanged 2026-10-05".
- No dependency added, removed, or bumped by this pass. No Cargo.lock diff.

## 📊 Measurement

| Check | 2026-09-28 | 2026-10-05 |
| --- | --- | --- |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean (18 license buckets, root) |
| Crate@version nodes / names / direct deps (root) | 774 / 686 / 139 | 779 / 687 / 140 |
| Workspace members | 37 | 37 |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 84 | 90 |
| Scheduled batch, root graph | 86 packages | 98 packages |
| Scheduled batch, `fuzz/` graph | 71 packages, uncovered | 78 packages, still uncovered |
| Scheduled batch, `island-flock/` graph | 31 packages, uncovered | 31 packages, still uncovered |
| Open Dependabot PRs | 11, of which 10 were 21–77 days old | **0** |
| Wildcard ranges / unpinned git refs | 0 / 0 | 0 / 0 |
| Waivers re-checked (root/fuzz, 3 unique ids) | 3/3 unchanged, review-by 2026-10-01 | 3/3 unchanged, **re-dated** to 2026-11-02 |
| Waivers re-checked (island-flock, 2 pre-existing) | 2/2 unchanged, review-by 2026-10-01 | 2/2 unchanged, **re-dated** to 2026-11-02 |
| Dependabot `ignore` entries | 0 | 2 (`matchit` unversioned, `dtolnay/rust-toolchain`) |

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
cargo metadata --format-version 1 --no-deps   # direct-dependency count

# scheduled-batch check — never --workspace (always reports 0 in this repo)
cargo update --dry-run --verbose 2>&1 | grep Locking
(cd fuzz && cargo update --dry-run --verbose 2>&1 | grep Locking)
(cd examples/island-flock && cargo update --dry-run --verbose 2>&1 | grep Locking)

# waiver spot-checks
cargo info rsa
cargo info instant
cargo info aws-sdk-s3@1.123.0
cargo info proc-macro-error
cargo info bincode

# duplicate-version mechanism trace (x509-parser direct/transitive split)
cargo tree -p autumn-web --no-default-features --features "ws,presence,flash,cache-moka,maud,htmx,multipart,tailwind,http-client,oauth2,webauthn,openapi,mcp,markdown,db,offline-sync,test-support,telemetry-otlp,redis,i18n,embed-assets,storage,variants,reporting,mail,inbound-mail,inbound-mailgun,inbound-ses,seed,system-info,csv,pdf,system-tests,managed-pg,managed-pg-bundled,tls,acme,edge,plugin-sandbox" -e normal,build --target all -i x509-parser@0.16.0
# (repeat with -i x509-parser@0.18.1)

# jsonwebtoken 11 re-verification (the compile break last pass found)
cargo check -p autumn-web --no-default-features --features oauth2
grep -A2 '^name = "jsonwebtoken"' Cargo.lock fuzz/Cargo.lock

# Dependabot queue (via the GitHub API/MCP)
# search_pull_requests: repo:autumn-foundation/autumn is:open author:app/dependabot
# pull_request_read get/get_comments on each tracked PR number

# dependabot.yml syntax check
python3 -c "import yaml; yaml.safe_load(open('.github/dependabot.yml'))"
```

## Follow-ups still open

1. **The timing gap flagged last pass materialized, and this pass's fix is
   itself a timing patch, not a process fix.** Re-dating to 2026-11-02 lands
   on a Monday, so the *next* Ballast pass after that date is 2026-11-02
   itself if the cadence holds a Monday weekly slot — but the same structural
   gap (review-by dates don't line up with pass dates by construction) will
   recur unless review-by dates are chosen to land exactly on a scheduled
   pass date going forward. Not fixed, just not repeated by luck this time.
2. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` (`bincode`,
   reachability undetermined): still unresolved, now five passes running.
   `build-island.sh` still hasn't rerun since before any Ballast pass began.
3. The NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`:
   still open, unchanged since the first pass.
4. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run, fifth pass
   running. Deferred for the same reason as every prior pass.
5. Human decision on how Ballast and Dependabot should divide
   responsibility (raised 2026-09-14): largely answered in practice this
   week, just not in writing — the maintainer directly drove nine of eleven
   queue items to resolution without waiting on a Ballast pass, which is one
   reasonable answer to the division-of-labor question. Still no config or
   doc change records that as the intended pattern going forward; still open
   as a *written* decision even though the de facto one is now evidenced.
6. Open Dependabot PR queue: **0**, first time in five passes. Nothing to
   triage next week unless new PRs open under the groups/ignore rules now in
   place.
7. The `fuzz/` (78 packages) and `island-flock/` (31 packages) scheduled
   batches remain uncovered by any process — `dependabot.yml`'s `updates:`
   list still only has two entries (root `cargo`, root `github-actions`);
   this pass's edit only added `ignore` rules to the existing entries, not
   new directory entries for either satellite. Same two options as every
   prior pass: extend `dependabot.yml`, or have Ballast own satellite-graph
   batches on its own cadence. Still a human decision.
   `examples/island-flock/Cargo.toml` still declares no `rust-version`.
8. **GitHub-native Dependabot alert count — now the thing to act on, not
   just watch.** This pass's push moved it from 9 (2 high, 7 moderate, 0
   low) to **16 (4 high, 10 moderate, 2 low)** — see Evidence — breaking a
   two-pass streak where "high" sat flat at 2. Escalated in this PR's
   description rather than left for next week. Still no tool in this
   session that lists individual alerts or joins them to a specific
   manifest, so whether the new highs are inside this ledger's 5 audited
   Rust graphs (unlikely — `cargo deny check advisories` reports 0 unwaived
   this pass) or in one of the two manifests this harness doesn't cover
   (`examples/react-graphql/frontend/package-lock.json`,
   `benchmarks/runtime/django/requirements.txt`) is still open. Next pass:
   check whether a human has triaged this before re-deriving from scratch.
9. The `cargo deny list` vs `cargo deny check bans` discrepancy (flagged
   2026-09-21, re-tested clean twice since, including the `phf_codegen`/
   `phf_generator` edge found via a Codex review comment last pass): not
   investigated further this pass — this week's `bans` output (90, traced
   fully above) didn't surprise against `list`'s count (779 nodes, consistent
   with bans' own duplicate accounting), so there's no fresh discrepancy to
   chase, but the open "why can the two subcommands disagree at all" question
   from two passes ago remains unanswered.
