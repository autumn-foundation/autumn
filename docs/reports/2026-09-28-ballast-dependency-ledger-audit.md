# ⚓ Ballast: weekly dependency ledger audit, 2026-09-28

Fourth Ballast pass, one week after the third
(`docs/reports/2026-09-21-ballast-dependency-ledger-audit.md`), which reran
the harness clean, corrected two of its own methodology claims mid-review,
and left nine open follow-ups. This pass reruns the harness end to end (same
methodology, same reproduce commands), re-checks every open follow-up, runs
a concrete rehearsal against the newest Dependabot PR in the queue (a
`jsonwebtoken` major bump, opened after last week's pass), and traces a real
mechanism behind this week's duplicate-version increase rather than just
reporting the delta.

## 🎯 Class

Ledger report. No dependency added, removed, bumped, or repinned.

## 📈 Evidence

**Harness re-run, all five graphs, `cargo-deny 0.20.2`** (installed fresh
this session — not present at session start — from the exact version
`ci.yml`'s `supply-chain` job pins):

```
./scripts/check-advisories.sh                 → OK, 0 unwaived advisories, all 5 graphs
./scripts/check-advisories.sh --self-test     → OK, RUSTSEC-2020-0071 blocked then waived, all 4 policies
cargo deny check licenses sources             → licenses ok, sources ok  (root graph)
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok  (SQLite graph)
cargo deny check bans                         → 84 duplicate-name warnings (warn-level; not CI-gated)
```

Identical clean outcome to all three prior passes on the advisories/licenses/
sources checks. `bans` moved (76 → 84), traced below — the first
duplicate-count movement any of the four passes has seen.

**Waivers, independently re-checked against current upstream state** (root
`deny.toml`'s three unique waivers, plus `island-flock/deny.toml`'s two —
`fuzz/deny.toml` repeats the root's `RUSTSEC-2023-0071` rather than adding a
new one):

| RUSTSEC id | Crate | 2026-09-21 | 2026-09-28 | Changed? |
| --- | --- | --- | --- | --- |
| RUSTSEC-2023-0071 | `rsa` | max stable 0.9.10, no fix (`0.10.0-rc.18` still RC) | max stable still 0.9.10 (`0.10.0-rc.18` still the newest release) | No |
| RUSTSEC-2024-0384 | `instant` | max 0.1.13, unmaintained | max still 0.1.13, unmaintained | No |
| RUSTSEC-2026-0253 | `lru` (via `aws-sdk-s3`) | pinned `aws-sdk-s3` 1.122.0; 1.123.0+ needs rust-version 1.91.0 | confirmed via `cargo info aws-sdk-s3@1.123.0`: still 1.91.0 (latest overall 1.150.0); 1.122.0 still the newest MSRV-compatible release under our 1.88.0 floor | No |
| RUSTSEC-2024-0370 | `proc-macro-error` (island-flock only) | max 1.0.4, unmaintained | max still 1.0.4 | No |
| RUSTSEC-2025-0141 | `bincode` (island-flock only) | reachability undetermined | still undetermined — `git log --since=2026-09-21 -- examples/island-flock examples/flock/static/islands` is empty, so there's still been no rebuild to inspect | No |

All five review-by dates are **2026-10-01 — now 3 days out**. See follow-up 1
for a timing gap this proximity surfaces for the first time.

**Graph facts, root workspace** (`cargo deny list --format json` for
node/name counts, `cargo metadata --no-deps` for the direct-dependency
count — kept as a separate, independently-sourced number rather than folded
into the `list`/`bans` question in follow-up 9):

| Metric | 2026-09-21 | 2026-09-28 | Δ | Mechanism |
| --- | --- | --- | --- | --- |
| Unique crate@version nodes | 764 | 774 | +10 | see duplicate-version finding below |
| Unique crate names | 686 | 686 | 0 | — |
| Direct (non-dev) deps referenced by workspace members | 138 | 139 | +1 | `933097b` (#2938, merged 2026-09-24) named `pq-src` directly in `autumn/Cargo.toml`'s `db` feature (`>=0.3, <0.3.12`) to pin around a glibc-incompatible bundled-libpq release; previously only a transitive edge via `pq-sys`. Confirmed by reading the commit, not inferred from the count alone. |
| Workspace members | 37 | 37 | 0 | — |
| Duplicate crate names (`cargo deny check bans`, warn-level) | 76 | 84 | +8 | traced below |

**Duplicate-version increase, traced to a specific merged PR, not just
counted.** The 8 new duplicate names are exactly the servo HTML/CSS parsing
stack: `html5ever`, `cssparser`, `markup5ever`, `string_cache`,
`string_cache_codegen`, `phf_codegen`, `phf_generator`, `web_atoms`. Two
`autumn-web` dependencies pull two different major lines of this stack:

```
$ cargo tree -p autumn-web <deny.toml's full feature list> -e normal,build --target all -i html5ever@0.39.0
html5ever v0.39.0
└── css-inline v0.21.2 (mail feature)
    └── autumn-web

$ cargo tree -p autumn-web <same> -i html5ever@0.40.1
html5ever v0.40.1
└── ammonia v4.2.0 (markdown feature)
    └── autumn-web
```

Neither dependency changed its own `Cargo.toml` version constraint this
window — checked directly: root `Cargo.toml` still pins `ammonia = "4.1"`
at the workspace level (`autumn/Cargo.toml`'s `css-inline = { version =
"0.21", ... }` likewise unchanged), and 4.2.0 simply satisfies the existing
`^4.1` requirement. The split is upstream: PR #2889 (`chore(deps): bump the rust-deps group across 1
directory with 2 updates`, Dependabot, merged 2026-09-23) bumped `ammonia`
4.1.4 → 4.2.0, and ammonia's own changelog for 4.2.0 states "chore: upgrade
to html5ever 0.40.0" and "chore: upgrade to cssparser 0.38.0". `css-inline`
hasn't moved off the older html5ever 0.39.0/cssparser 0.37.0 chain. This is a
real, traced mechanism — a routine, already-reviewed-and-merged minor/patch
group bump (exactly the "scheduled batch" cadence this charter wants) had a
transitive side effect of widening the duplicate-version count by 8. Not
actioned: `[bans] multiple-versions = "warn"` is deliberately non-blocking
per `deny.toml`'s own rationale, and deduping would mean waiting on either
`css-inline` or `ammonia` to converge upstream, not something to force from
this side.

**Scheduled batch, all three graphs** (bare `cargo update --dry-run
--verbose`, reading the `Locking N packages to latest ... compatible
versions` line — never `--workspace`, which always reports 0 in this repo):

| Graph | 2026-09-21 | 2026-09-28 |
| --- | --- | --- |
| root (Rust 1.88.0) | 92 packages | 86 packages |
| `fuzz/` (Rust 1.88.0) | 66 packages | 71 packages |
| `examples/island-flock/` (no declared `rust-version`; this sandbox's 1.94.1 toolchain) | 31 packages | 31 packages |

Root fell (partially offset by this window's real merges — #2889, #2891
`infer` 0.16.0→0.22.0, and the `pq-src` fix). `fuzz/` also grew net of real
merged movement, not against a static baseline as an earlier draft of this
paragraph claimed: `git show --stat ba32d11 -- fuzz/Cargo.lock` and `git show
--stat 933097b -- fuzz/Cargo.lock` both touch it directly — #2891's own
regeneration commit message says so explicitly ("chore(deps): regenerate
fuzz/Cargo.lock for infer 0.22.0 — the Supply chain (cargo-deny) job runs
with `--locked` against the fuzz workspace, so the root bump has to carry
the fuzz lockfile with it"), and #2938 carries a matching "record the pq-src
edge in fuzz/Cargo.lock" commit. `island-flock/` held flat, and its
`git log --since=2026-09-21` is genuinely empty (verified the same way).
Ownership unchanged from all three prior passes: root is Dependabot's
territory (`directory: /`); `fuzz/` and `island-flock/` remain uncovered by
any *scheduled-batch* process (follow-up 7) even though `fuzz/Cargo.lock`
itself does get carried along by root-graph bumps that happen to touch a
crate it shares.

**Supply-chain facts, re-verified**: zero wildcard version ranges
(`grep -rn 'version = "\*"'`) and zero unpinned git refs (`git = "`) anywhere
in the tree, main workspace and both satellites — unchanged across all four
passes.

**Pain ledger — a real fire this window, already resolved before this pass
started.** `933097b` (#2938, merged 2026-09-24): `pq-src` 0.3.12
(libpq 18.6, published 2026-09-24) calls `timingsafe_bcmp`, which glibc does
not provide — every binary linking the bundled libpq failed with an
undefined symbol, breaking the scaffold gate on every PR and on `trunk-dev`
the same day the bad release shipped. Fixed same-day by pinning
`pq-src` to `>=0.3, <0.3.12` directly in `autumn-web`'s `db` feature (the
`+1` direct-dependency edge in the graph-facts table above). This is exactly
the "vulnerability you can reach is a fire" pattern the charter describes,
except the defect was a build break rather than a CVE — reached fast,
scoped narrowly, fixed same day, pin documented inline with a removal
condition ("remove when a fixed release is out"). Not a Ballast action
(already merged before this pass ran), but worth recording as evidence the
pain-ledger process the charter asks for is actually happening in practice.

**Dependabot open-PR queue, re-run with `is:open`** (`search_pull_requests`,
`author:app/dependabot is:open`): **11** open PRs, down from 13 last pass.
Three merged since 09-21 (`#2613` `actions/attest-build-provenance`, `#2792`
`taiki-e/install-action`, `#2179` `django`) — but only `#2179` was actually
flagged **stale** last pass (42 days then); last pass's own report described
`#2613` (14 days then) and `#2792` (7 days then) as new and explicitly "not
yet stale," so it would misstate that report to call all three stale here.
One new PR opened (`#2890`, `jsonwebtoken` 10.4.0 → 11.1.0, opened
2026-09-21, i.e. after last week's pass ran).

| PR | Title | Opened | Age today |
| --- | --- | --- | --- |
| #1891 | `actions/checkout` 4→7 | 2026-07-13 | 77 days |
| #1894 | `tokio-tungstenite` 0.29→0.30 | 2026-07-13 | 77 days |
| #1895 | `sha1` 0.10.6→0.11.0 | 2026-07-13 | 77 days |
| #1896 | `x509-parser` 0.16.0→0.18.1 | 2026-07-13 | 77 days |
| #1897 | `matchit` 0.8.4→0.9.2 | 2026-07-13 | 77 days |
| #1898 | `rand_chacha` 0.9.0→0.10.0 | 2026-07-13 | 77 days |
| #1899 | `rand` 0.9.4→0.10.2 | 2026-07-13 | 77 days |
| #2081 | `actions/upload-artifact` 4→7 | 2026-07-19 | 71 days |
| #2302 | `validator` 0.20.0→0.21.0 | 2026-08-24 | 35 days |
| #2615 | `dtolnay/rust-toolchain` 1.88.0→1.120.0 | 2026-09-07 | 21 days |
| #2890 | `jsonwebtoken` 10.4.0→11.1.0 | 2026-09-21 | 7 days |

The eight-PR structural core (2 GitHub Actions bumps, 6 Cargo semver-major
bumps — the ones with no covering group at all) is unchanged and simply
7–8 days older, for the same structural reason established
over the last two passes: `dependabot.yml`'s `github-actions` ecosystem
defines no groups at all, and the `rust-deps` catch-all group only covers
`minor`/`patch` updates, so a pre-1.0 major bump on a `rust-deps`-scoped
package (or any `github-actions` bump) is individual by design, not
orphaned. Not something this pass acts on — a human review/merge decision on
each, same as every prior pass concluded. Counting every PR in the table
aged 21 days or older (not just the structural eight) gives **ten**: the
eight-PR core plus `#2302` (35 days) and `#2615` (21 days) — same total as
last pass's ten, just a week further along, not a narrower backlog.

**New this pass — rehearsed the newest queue entry rather than only listing
it, since #2890 is exactly the shape of Upgrade this charter gates: a
semver-major bump with a live changelog to read.** `jsonwebtoken` 10→11 is a
plain stable-crate major bump (unlike last week's six 0.x "major" PRs), so
`rust-deps`'s `update-types: [minor, patch]` filter excludes it the ordinary
way, same mechanism, nothing new there. Rehearsed on a scratch copy of
`autumn/Cargo.toml` (`jsonwebtoken = { version = "10.1.0", ... }` →
`"11.1.0"`), `cargo update -p jsonwebtoken --precise 11.1.0`, inspected the
`Cargo.lock` diff, then reverted both files via `git checkout` before doing
anything else with them:

```
$ git diff Cargo.lock
 name = "jsonwebtoken"
-version = "10.4.0"
+version = "11.1.0"
 source = "registry+https://github.com/rust-lang/crates.io-index"
-checksum = "eba32bf..."
+checksum = "e75fe14..."
 dependencies = [
  "base64 0.22.1",
  ...                      # every other dependency name/version identical
```

**Finding: this bump changes nothing else in the graph.** `rsa` stays at
0.9.10 under the `rust_crypto` feature Autumn already selects — the
`RUSTSEC-2023-0071` waiver's ingress path (`rsa 0.9.10 -> jsonwebtoken ->
autumn-web`) is untouched, so the hypothesis that this bump might close that
waiver (worth checking specifically, since it shares a review-by date with
this pass) is **false**, checked rather than assumed. Cross-checked v11's
changelog breaking-change list (`insecure_disable_signature_validation`
removed, `EncodingKey.inner`/`DecodingKey.as_bytes`/`try_get_hmac_secret`
renamed or removed, `JwkUtils` renamed `KeyUtils`, `Jwk.thumbprint` now
returns `Result`) against every `jsonwebtoken::` call site in
`autumn/src/auth.rs` and `autumn/src/tenancy.rs` (`decode_header`,
`DecodingKey::from_jwk`/`from_secret`, `Validation::new`/`default`,
`decode`, `encode`, `EncodingKey::from_secret`, `Header::new`, the `jwk`
types) — none of Autumn's usage touches a removed or renamed API. So this
bump is very likely a clean, low-risk compile (not run through the full
suite — no forcing fact surfaced that would justify spending that budget
this pass). Per the charter's Upgrade-class bar, a major needs a forcing
fact beyond "staying current," and this rehearsal found none: no advisory
closes, no EOL/security-support window applies, no graph shrinks. **This PR
stays a human review call, same as the other six major-bump PRs in the
queue** — but now with an actual rehearsal result attached instead of being
another unexamined row in a table.

**Discrepancy from follow-up 8, re-observed via this pass's own `git push`,
and the count changed.** Last pass's `git push` printed "15 vulnerabilities
(2 high, 9 moderate, 4 low)" from GitHub's native Dependabot alert scan;
this pass's push of the report commit printed **"9 vulnerabilities (2 high,
7 moderate)"** — total down 6, moderate down 2, low down to 0, high
unchanged at 2. That move happened during a week where several Cargo
dependencies were genuinely bumped on `trunk-dev` (`ammonia` 4.1.4→4.2.0,
`infer` 0.16.0→0.22.0, `clap` 4.6.6→4.6.7, plus the `pq-src` pin) — but this
session has no tool to enumerate which of the 15 actually closed, so that
temporal overlap is exactly that and no more: it does not by itself
distinguish "some of the 15 were real GHSA-tracked Cargo advisories that
resolved when their vulnerable version left the branch" from "GitHub's alert
state or advisory data changed for reasons unrelated to this week's Cargo
bumps." Both remain open explanations. Not scored, per the same evidentiary
bar as last pass, and this time not leaned on either — the count change is
recorded as an unclassified fact, not evidence for either hypothesis, until
someone with Security-tab access joins the specific alerts against specific
merges. The two "high" alerts being unchanged across both counts is the one
piece worth a human's attention regardless of how the moderates moved.

## 💡 Mechanism / forcing fact

None, for Ballast to act on directly this pass — the fourth consecutive
pass reaching this conclusion. Every Tier-1 check reruns clean, every
existing waiver's underlying fact is unchanged, the one rehearsed Upgrade
candidate in the queue (`jsonwebtoken` 10→11) has no forcing fact, and the
open follow-ups are still open human decisions (division of labor with
Dependabot, satellite-graph batch ownership, the NCSA license-class
question, the GitHub-native alert-count discrepancy).

## 🔧 Change

None to the dependency graph. This report is the only artifact.

## 📊 Measurement

| Check | 2026-09-21 | 2026-09-28 |
| --- | --- | --- |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean |
| Crate@version nodes / names / direct deps (root) | 764 / 686 / 138 | 774 / 686 / 139 |
| Workspace members | 37 | 37 |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 76 | 84 |
| Scheduled batch, root graph | 92 packages | 86 packages |
| Scheduled batch, `fuzz/` graph | 66 packages, uncovered | 71 packages, still uncovered |
| Scheduled batch, `island-flock/` graph | 31 packages, uncovered | 31 packages, still uncovered |
| Open Dependabot PRs | 13, of which 10 are 28–69 days old | 11, of which 10 are 21–77 days old (3 merged: #2613, #2792, #2179; 1 new: #2890, rehearsed this pass) |
| Wildcard ranges / unpinned git refs | 0 / 0 | 0 / 0 |
| Existing waivers re-checked (root graph, 3) | 3/3 unchanged | 3/3 unchanged |
| Existing waivers re-checked (satellite-only, `island-flock/deny.toml`, 2) | 2/2 unchanged | 2/2 unchanged |

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
cargo update --dry-run --verbose | grep Locking
(cd fuzz && cargo update --dry-run --verbose | grep Locking)
(cd examples/island-flock && cargo update --dry-run --verbose | grep Locking)

# waiver spot-checks
cargo info rsa
cargo info instant
cargo info aws-sdk-s3@1.123.0
cargo info proc-macro-error
cargo info bincode

# duplicate-version mechanism trace (servo HTML/CSS stack split)
cargo tree -p autumn-web --no-default-features --features "ws,presence,flash,cache-moka,maud,htmx,multipart,tailwind,http-client,oauth2,webauthn,openapi,mcp,markdown,db,offline-sync,test-support,telemetry-otlp,redis,i18n,embed-assets,storage,variants,reporting,mail,inbound-mail,inbound-mailgun,inbound-ses,seed,system-info,csv,pdf,system-tests,managed-pg,managed-pg-bundled,tls,acme,edge,plugin-sandbox" -e normal,build --target all -i html5ever@0.39.0
# (repeat with -i html5ever@0.40.1, and -i bitflags@1.3.2 for the follow-up 9 chain)

# jsonwebtoken 10->11 rehearsal (reverted via git checkout after inspection)
# in autumn/Cargo.toml: jsonwebtoken = { version = "11.1.0", ... }
cargo update -p jsonwebtoken --precise 11.1.0
git diff Cargo.lock
git checkout -- autumn/Cargo.toml Cargo.lock

# Dependabot queue health (search via the GitHub API/MCP: author:app/dependabot is:open)
```

## Follow-ups still open

1. **Timing gap surfaced for the first time this pass.** All five waivers
   share review-by date 2026-10-01 — now 3 days out. This weekly cadence's
   next scheduled pass lands 2026-10-05, four days *after* the review-by
   date, so nobody will actually look at these on the date itself unless a
   human (or an off-cycle pass) checks in between. Not actioned here — the
   charter says "revisit properly at that date, not before," and this pass
   is before it — but flagging the gap explicitly rather than letting the
   date quietly slip past between two weekly passes.
2. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` (`bincode`,
   reachability undetermined): still unresolved. `build-island.sh` has not
   rerun since at least the last three passes (`git log --since=2026-09-21`
   for that tree is empty), so there's still no natural point to inspect the
   compiled `.wasm`'s retained symbols. Still open.
3. The NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`:
   still an open human "ask before" question, unchanged in `CHANGELOG.md`,
   `deny.toml`, and `fuzz/deny.toml` since the first pass.
4. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run, fourth pass
   running. Deferred again for the same reason as before — nothing found
   this pass makes a specific candidate hire worth attributing cost to yet.
5. Human decision on how Ballast and Dependabot should divide
   responsibility (raised 2026-09-14, still open two passes later): no
   comment or config change found addressing it. This pass's `jsonwebtoken`
   rehearsal is a concrete example of the uncovered half — `ci.yml`'s
   `supply-chain` job already catches a new unwaived advisory or license on
   every Dependabot PR mechanically, but the reachability/forcing-fact
   judgment this pass just did by hand for one PR isn't run on the other
   ten automatically.
6. Open Dependabot PR queue: 11 (was 13), 10 of them 21–77 days old — same
   ten PRs as last pass (the eight-PR structural core plus `#2302` and
   `#2615`), all just a week older, plus the new #2890 now rehearsed (see
   evidence). Still a human call whether to merge, close, or act on any of
   them; #2615 remains explicitly an "ask before" toolchain bump.
7. The `fuzz/` (71 packages) and `island-flock/` (31 packages) scheduled
   batches remain uncovered by any process — `dependabot.yml` unchanged
   since the decision was first raised. Same two options as every prior
   pass: extend `dependabot.yml` with two more directory entries, or have
   Ballast own satellite-graph batches on its own cadence. Still a human
   decision. `examples/island-flock/Cargo.toml` still declares no
   `rust-version` (re-checked: just `edition = "2024"`), so there's still no
   MSRV floor for its scheduled-batch row to be checked against.
8. **Still highest priority, and the count moved (cause unestablished).**
   GitHub's native Dependabot alert count for the default branch went 15
   (2 high, 9 moderate, 4 low) → **9 (2 high, 7 moderate, 0 low)** between
   last pass's push and this one (see evidence section). This session
   cannot attribute that move to this week's Cargo bumps or to anything
   else — no tool here enumerates which of the 15 closed, so the change is
   recorded as a fact, not linked to a cause. This session's GitHub MCP
   tools were checked again (`get_me`, `search_pull_requests`,
   `pull_request_read`, and a fresh `ToolSearch` for anything
   alert/vulnerability-shaped) and still none expose the Security tab's
   Dependabot alerts directly, so the 6 that closed and the 9 that remain
   (2 high) still can't be joined against this repo's waivers or its two
   non-Rust manifests (`examples/react-graphql/frontend/package-lock.json`,
   `benchmarks/runtime/django`'s Python deps) from this session. The two
   high-severity alerts being unchanged across both counts is worth a
   human's first look regardless of what explains the moderates moving.
   week.
9. The `cargo deny list` vs `cargo deny check bans` discrepancy flagged last
   pass (the former missed the `bitflags`/`parking_lot_core`/`parking_lot`
   chain that the latter found): re-tested this pass by re-running the same
   `-i bitflags@1.3.2` inverted-tree probe — the chain is still present and
   still correctly counted in this week's 84-duplicate `bans` output. That
   confirms no regression, but the actual open question (why `list` and
   `bans` can disagree at all — a `[graph]`/target-resolution difference
   between the two subcommands is the leading unconfirmed hypothesis) has
   not been investigated further this pass. Still open.
