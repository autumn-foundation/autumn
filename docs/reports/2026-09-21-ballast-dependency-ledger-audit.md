# ⚓ Ballast: weekly dependency ledger audit, 2026-09-21

Third Ballast pass, one week after the second
(`docs/reports/2026-09-14-ballast-dependency-ledger-audit.md`). That pass
re-verified the harness, corrected two of its own earlier drafts on the
"scheduled batch" methodology, and left **seven** open follow-ups (an eighth
item was marked **Retracted** in that report, a git-history false alarm
fully withdrawn, so it isn't an open item to carry forward — an earlier
draft of this paragraph said "eight," miscounting it in, caught by a Codex
review comment on this PR). This pass reruns the harness end to end (same
methodology, same reproduce commands), re-checks every open follow-up, and
surfaces two findings neither prior pass looked for: the actual state of
the open Dependabot queue (not just its recent merge activity), and a
discrepancy between this harness's clean result and GitHub's own native
Dependabot alert count on the default branch (see the evidence section).

## 🎯 Class

Ledger report. No dependency added, removed, bumped, or repinned.

## 📈 Evidence

**Harness re-run, all five graphs, `cargo-deny 0.20.2`** (the exact version
`ci.yml`'s `supply-chain` job pins; installed from the pinned GitHub release
for this pass):

```
./scripts/check-advisories.sh                 → OK, 0 unwaived advisories, all 5 graphs
./scripts/check-advisories.sh --self-test     → OK, RUSTSEC-2020-0071 blocked then waived, all 4 policies
cargo deny check licenses sources             → licenses ok, sources ok  (root graph)
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok  (SQLite graph)
cargo deny check bans                         → bans ok (warn-level; not CI-gated, see below)
```

Identical outcome to both prior passes on every check.

**Waivers, independently re-checked against current upstream state** (not
taken on faith from the pin comment). This is the **root `deny.toml`'s three
unique waivers only** — the ledger carries five unique waived RUSTSEC ids in
total across all policies (`fuzz/deny.toml` repeats the root's
`RUSTSEC-2023-0071` rather than adding a new one, but
`examples/island-flock/deny.toml` carries two of its own,
`RUSTSEC-2024-0370` and `RUSTSEC-2025-0141`, covered separately right after
this table — an earlier draft's "3/3" measurement-table line conflated the
two scopes, flagged by a Codex review comment on this PR):

| RUSTSEC id | Crate | 2026-09-14 | 2026-09-21 | Changed? |
| --- | --- | --- | --- | --- |
| RUSTSEC-2023-0071 | `rsa` | max stable 0.9.10, no fix | max stable still 0.9.10 (`0.10.0-rc.18` is still a release candidate) | No |
| RUSTSEC-2024-0384 | `instant` | max 0.1.13, unmaintained | max still 0.1.13, unmaintained | No |
| RUSTSEC-2026-0253 | `lru` (via `aws-sdk-s3`) | pinned `aws-sdk-s3` 1.122.0; 1.123.0+ needs rust-version 1.91.0 > our 1.88.0 floor | confirmed via `cargo info aws-sdk-s3@1.123.0`: still 1.91.0, latest overall is 1.148.0; 1.122.0 is still the newest MSRV-compatible release | No |

All three review-by dates are **2026-10-01**, now 10 days out — not due this
pass.

**The two `island-flock`-only waivers, re-checked this pass too** (not run
last pass as a distinct check, only inspected via the "no relevant git
activity" angle in follow-up 2): `RUSTSEC-2024-0370` (`proc-macro-error`,
unmaintained) — `cargo info proc-macro-error` still shows max version
1.0.4, unchanged, and the waiver's own reasoning (build-time-only,
`yew-macro`'s proc-macro dependency, never linked into the compiled wasm
output) doesn't depend on anything that moved. `RUSTSEC-2025-0141`
(`bincode`) remains reachability **undetermined**, not "valid" in the same
sense as the other four — see follow-up 2 for why that one stays open
rather than closed. Same review-by date, 2026-10-01, for both.

**Graph facts, root workspace** (`cargo deny list --format json`, same
methodology as both prior passes — the deny.toml-scanned graph: default +
every additive feature CI compiles, not `--all-features`, not dev-only
subtrees pulled by other feature combinations):

| Metric | 2026-09-14 | 2026-09-21 | Δ |
| --- | --- | --- | --- |
| Unique crate@version nodes | 791 | 764 | −27 |
| Unique crate names | 704 | 686 | −18 |
| Direct (non-dev) deps referenced by workspace members | 134 | 138 | +4 |
| Workspace members | 33 | 37 | +4 |
| Duplicate crate names (`cargo deny check bans`, warn-level) | 76 | 76 | 0 |

**Correction, from a Codex review comment on this PR**: an earlier draft of
this section attributed the +4 workspace members to the macro-crate split
plus confidential-fields and dunning-batching work "continuing to add
surface" — wrong, and not checked against the actual `Cargo.toml` diffs
before writing it. Verified directly (`git show <commit> -- Cargo.toml`) for
every commit that touched the root `Cargo.toml` since 09-14: the macro-crate
split (`9800221`, #2809) added exactly three members
(`autumn-macros-model`, `autumn-macros-repository`, `autumn-macros-support`);
collaborative fields (`b88f78b`, #1806/#2814) added exactly one
(`examples/collab-notes`) three days earlier. That's the full +4. The
confidential-fields (`0f1b0c0`) and dunning-batching (`6d333f5`) commits
never touch `Cargo.toml`'s `members` array at all — they were named in the
earlier draft only because they were recent and thematically nearby, not
because they were checked.

**Retracted, from a Codex review comment on this PR that turned out to be
right on a much bigger question than wording.** An earlier draft of this
report claimed the duplicate-name count fell from 76 to 68 and that
`bitflags`/`parking_lot`/`parking_lot_core` specifically "dropped out" of
the list — based on `cargo deny list --format json`, the same tool used for
the node/name counts above. That number was wrong, and the mechanism claim
with it. `cargo deny list` is a **license-grouping** view, not the tool this
report's own duplicate-count row is labeled with; the actual duplicate
count comes from `cargo deny check bans`, which this pass re-ran directly:

```
$ cargo deny check bans 2>&1 | grep -c '^warning\[duplicate\]: found'
76
```

**76, not 68 — unchanged from last week, not down 8.** `bans`'s own output
traces the real chain for all three "missing" names, confirming they were
never gone: `bitflags` 1.3.2 → `parking_lot_core` 0.8.6 → `parking_lot`
0.11.2, reached from `reqwest-retry`/`wasm-timer` → `postgresql_archive` →
`postgresql_embedded` → `autumn-web` — the exact same
`managed-pg-bundled`-gated chain this file's own `RUSTSEC-2024-0384`
(`instant`) waiver already documents as in-scope. Independently confirmed
with `cargo tree -p autumn-web --features <deny.toml's exact feature list>
--target all -i bitflags@1.3.2` (rooted at the bottom of the chain — `-i`
inverts to show *dependents*, so focusing on `parking_lot` itself would
omit `parking_lot_core` and `bitflags` below it, which is what an earlier
draft's probe did; caught by a Codex review comment on this PR), which
shows the identical three-crate path — `list`
simply doesn't surface it (root cause not pinned down this pass: `list` and
`check bans` may apply `[graph]`/target resolution differently; worth a
harness-level follow-up rather than more guessing here, see follow-up 9).
So: node/crate-name counts (791→764, 704→686) still come from `cargo deny
list`, consistent with both prior passes' stated methodology, and are left
as reported since nothing in this pass contradicts them directly — but
given `list` just demonstrably missed a real, `bans`-confirmed dependency
chain, they should be read as this tool's view, not a fully independent
cross-check. The **duplicate-name count specifically is corrected to 76,
unchanged**, since `check bans` is this metric's stated source — **not**,
as an earlier draft wrongly claimed, because CI runs it: `ci.yml`'s
`supply-chain` job runs `check-advisories.sh` and `cargo deny check
licenses sources` only, and `deny.toml`'s own `[bans]` comment says so
explicitly ("`bans` ... is intentionally not run in CI"). A Codex review
comment on this PR caught that false claim; `check bans` is the correct
*audit* tool for this metric, a local/self-run check, not a CI gate.

**Duplicate-version breakdown, from `cargo deny check bans`, same three
categories as last week** (76 names, matching last week's 76 — not
independently diffed name-for-name against that report's list, since it
was given as "45 and others" rather than exhaustively): RustCrypto
0.9/0.10-era split (23 names, unchanged: `aes`, `base16ct`, `block-buffer`,
`cipher`, `const-oid`, `crypto-bigint`, `crypto-common`, `der`, `digest`,
`ecdsa`, `elliptic-curve`, `ff`, `group`, `hmac`, `inout`, `md-5`, `p256`,
`pkcs8`, `rfc6979`, `sec1`, `sha2`, `signature`, `spki`); `windows-sys`/
`windows_*` shims (10 names: `windows-sys`, `windows-targets`, and 8
per-target `windows_*` crates); general ecosystem major-version splits (43
names: `base64`, `bitflags`, `cpufeatures`, `darling`/`darling_core`/
`darling_macro`, `downcast-rs`, `etcetera`, `foldhash`, `getrandom`,
`hashbrown`, `heck`, `http`, `http-body`, `lru`, `miniz_oxide`, `nom`,
`num-bigint`, `parking_lot`/`parking_lot_core`, `phf`/`phf_shared`,
`r-efi`, `rand`/`rand_chacha`/`rand_core`, `redox_syscall`, `reqwest`,
`spin`, `strum`/`strum_macros`, `syn`, `thiserror`/`thiserror-impl`,
`toml`/`toml_datetime`, `tower-http`, `tungstenite`, `wasi`, `wasite`,
`webpki-roots`, `whoami`, `winnow`). Still `multiple-versions = "warn"`,
deliberately not CI-gated, per `deny.toml`'s own rationale; not actioned
this pass.

**Scheduled batch, all three graphs** (bare `cargo update --dry-run
--verbose`, never `--workspace` — see last week's report for why that flag
always reports 0 in this repo):

| Graph | 2026-09-14 | 2026-09-21 |
| --- | --- | --- |
| root (`Locking … to latest Rust 1.88.0 compatible versions`) | 74 packages | 92 packages |
| `fuzz/` (rust-version 1.88.0) | 58 packages | 66 packages |
| `examples/island-flock/` (no declared `rust-version`; reflects this sandbox's 1.94.1 toolchain) | 29 packages | 31 packages |

All three grew — but not, as an earlier draft of this line claimed, because
all three lockfiles sat unmoved against a week of upstream releases.
**Corrected, from a Codex review comment on this PR**: this report's own
pain-ledger section (below) names three merges that touched lockfiles in
this window — `5f7a63a` updated both `Cargo.lock` and `fuzz/Cargo.lock`;
`ff39144` and `be63a93` each updated `Cargo.lock` again on top of that.
Only `examples/island-flock/Cargo.lock` was genuinely untouched. So the
root and `fuzz/` batch counts grew *net* of real merged movement this week,
not against a static baseline — the accumulation was partially offset by
those merges, not absent. As established last week: the root graph's
batch material is Dependabot's territory (`directory: /` in
`dependabot.yml`) and actively worked by humans; the `fuzz/` and
`island-flock/` batches remain **uncovered by any process** —
`dependabot.yml` still has no entry for either directory (re-checked this
pass, byte-identical to last week). Not rehearsed this pass — the charter's
"ask before" line on toolchain-adjacent changes and the `island-flock`
rehearsal's real-build requirement (`build-island.sh` + a matching
`wasm-bindgen-cli`, not a bare `cargo check`) mean picking this up needs the
same dedicated pass last week's report scoped, not a rider on this one.

**Supply-chain facts, re-verified**: zero wildcard version ranges and zero
unpinned git refs anywhere in the tree (`grep -rn 'version = "\*"'` /
equivalent `git = "` scan, main workspace + both satellites — all empty,
unchanged from both prior weeks).

**Pain ledger — new finding this pass: the open Dependabot queue itself,
not just recent merge activity.** Both prior passes characterized Dependabot
by what it had *recently merged* (#2617, #2640 the week of 09-08–09-10) and
flagged two stale PRs from a `search_pull_requests author:app/dependabot`
query with no state filter. This pass ran the query **with `is:open`**
specifically, which neither prior pass did, and found 13 currently-open
Dependabot PRs, several materially older than what was previously reported:

| PR | Title | Opened | Last updated | Age today |
| --- | --- | --- | --- | --- |
| #1891 | `actions/checkout` 4→7 | 2026-07-13 | 2026-08-13 | 69 days |
| #1894 | `tokio-tungstenite` 0.29→0.30 | 2026-07-13 | 2026-07-22 | 69 days |
| #1895 | `sha1` 0.10.6→0.11.0 | 2026-07-13 | 2026-08-13 | 69 days |
| #1896 | `x509-parser` 0.16.0→0.18.1 | 2026-07-13 | 2026-08-13 | 69 days |
| #1897 | `matchit` 0.8.4→0.9.2 | 2026-07-13 | 2026-08-13 | 69 days |
| #1898 | `rand_chacha` 0.9.0→0.10.0 | 2026-07-13 | 2026-08-13 | 69 days |
| #1899 | `rand` 0.9.4→0.10.2 | 2026-07-13 | 2026-08-13 | 69 days |
| #2081 | `actions/upload-artifact` 4→7 | 2026-07-19 | 2026-08-18 | 64 days |
| #2179 | Python `django` bump (`benchmarks/runtime/django`) | 2026-08-10 | 2026-08-10 | 42 days |
| #2302 | `validator` 0.20.0→0.21.0 | 2026-08-24 | 2026-09-19 | 28 days |
| #2613 | `actions/attest-build-provenance` 3→4 | 2026-09-07 | 2026-09-07 | 14 days |
| #2615 | `dtolnay/rust-toolchain` 1.88.0→1.120.0 | 2026-09-07 | 2026-09-20 | 14 days |
| #2792 | `taiki-e/install-action` 2.87.5→2.87.11 | 2026-09-14 | 2026-09-14 | 7 days |

Spot-checked #1891 directly (`pull_request_read`): it's `mergeable_state:
unstable` and GitHub has disabled automatic rebases on it ("has been open
for over 30 days"), i.e. it is genuinely stuck, not just quiet.

**Correction, from a Codex review comment on this PR**: an earlier draft
lumped #1891 (`actions/checkout`) in with #1894–#1899 as "seven ungrouped
legacy bumps" that "predate the current grouping config" — wrong for #1891
specifically. `dependabot.yml`'s `github-actions` ecosystem stanza defines
no `groups:` at all (re-checked directly, see the earlier `.github/
dependabot.yml` excerpt), so an individual PR is exactly what that config
asks for — #1891 isn't a grouping-config leftover, it's just old and stuck.
The same applies to #2081 (`actions/upload-artifact`), also a `github-actions`
PR.

**Second correction, from a further Codex review comment on the same
commit**: the first correction above still got the *mechanism* for the
remaining six `cargo` PRs wrong, even after fixing the actions-PR miscount.
It called them "orphaned" from a config change that predates them — but
`rust-deps`' `update-types: [minor, patch]` (see the `dependabot.yml`
excerpt earlier in this report) excludes them for a much simpler, timing-
independent reason: every one of the six is a **semver-major** transition
under Dependabot's own pre-1.0 convention (a change in the leading nonzero
component of a `0.x.y` version counts as major, the same way `1.x→2.x`
would for a stable crate) — `tokio-tungstenite` 0.29→0.30, `sha1`
0.10.6→0.11.0, `x509-parser` 0.16.0→0.18.1, `matchit` 0.8.4→0.9.2,
`rand_chacha` 0.9.0→0.10.0, `rand` 0.9.4→0.10.2. `rust-deps` only groups
`minor`/`patch`, and none of the six matches `axum-ecosystem` or
`diesel-ecosystem`'s patterns either — **narrowed, from a third Codex
review comment on this PR**: an earlier draft of this paragraph said "no
group anywhere in this file covers `major` updates at all," which overstates
it. `axum-ecosystem` and `diesel-ecosystem` both omit `update-types`
entirely, so — per Dependabot's default — they *do* cover major bumps for
their own patterns (`axum*`/`tower*`/`http` and `diesel*`/
`libsqlite3-sys`); it's only the catch-all `rust-deps` group, the one whose
`"*"` pattern would otherwise catch these six, that excludes majors. So the
precise statement is: these six packages have no group that covers them at
a major version, because `rust-deps` is the only group whose pattern
reaches them and its `update-types` filter excludes majors — not that this
file has no major-covering group at all. So these six sitting as individual
PRs isn't evidence anything was
orphaned when the grouping config changed — it's exactly what the *current*
config produces for a major bump on an `rust-deps`-scoped package today,
same as it would have on the day `rust-deps` was created. The real (and
much less alarming) finding is just:
six individual major-bump `cargo` PRs, like the two individual
`github-actions` PRs, have sat un-reviewed for 64–69 days. That's a review-
backlog fact, not a configuration gap — reconciling `dependabot.yml` would
not make any of these six disappear on its own.

Also corrected: the age-range total. #2302 (`validator`) is the one flagged
as stale last week too, now 28 days (was 3+ weeks) and still unresolved,
comment count 1. #2179 (`django`) is now 42 days (was 5+ weeks), still open,
in a directory nothing else in this report's scope touches. #2615 (the
toolchain bump) is correctly still open pending a human "ask before" decision
per the charter, unchanged from last week. #2613 and #2792 are new since last
week's pass and not yet stale. Counting every PR in the table whose age falls
in the 28–69 day band gives **ten**, not eight: the seven at 69 days
(#1891 + the six `cargo` PRs above), #2081 at 64, #2179 at 42, #2302 at 28.

This is **not** a Ballast finding to act on directly — merging, closing, or
nudging any of these 13 PRs is a human call (several are exactly the kind of
major-bump review the `rust-deps` catch-all never covers, and #2615 is
explicitly an "ask before" toolchain change) — but it's a materially
different picture than "two stale PRs" and worth a maintainer's attention:
**10 of the 13** are old enough (28–69 days) to be queue rot rather than
normal review lag: two `github-actions` PRs and six `cargo` major-bump PRs,
all individual by design under the current config, simply unreviewed for
64–69 days, plus #2179 (42 days) and #2302 (28 days).

**Discrepancy surfaced by this pass's own `git push`, not investigated
further — flagged rather than assessed.** Pushing this report's commit
(`cb7abac`) printed GitHub's own remote message: "GitHub found 15
vulnerabilities on autumn-foundation/autumn's default branch (2 high, 9
moderate, 4 low)" pointing at `/security/dependabot`. That is a materially
different number from this pass's own harness result (0 unwaived advisories
across all 5 RustSec-based graphs). I do not have tooling in this session to
enumerate GitHub's native Dependabot alerts (no `list_dependabot_alerts`-
shaped tool is loaded, and the repo's Security tab isn't reachable through
`search_issues`/`search_code`), so I can't say from this pass alone whether
the 15 are: (a) non-Rust ecosystems `cargo-deny` never scans at all — this
repo has an npm manifest under `examples/react-graphql/frontend/`
(`package.json`/`package-lock.json`; **corrected** — an earlier draft of
this paragraph said `examples/island-flock`, which has no npm manifest of
its own, only Cargo manifests and a direct `wasm-bindgen-cli` invocation;
a second Codex review comment caught this same mistake surviving in this
paragraph after the follow-up section below it was already fixed) and a
Python `django` dependency under
`benchmarks/runtime/django` (the same one #2179 above bumps), neither
RustSec-covered; or (b) genuinely GHSA-only advisories for Cargo crates that
haven't propagated into the RustSec advisory database `cargo-deny` consumes,
which would be a real gate gap; or (c) advisories already in this repo's
`[advisories] ignore` waiver lists, which GitHub's dependency graph has no
way to know are deliberately waived. This charter treats "a severity score
without a reachability verdict" as inadmissible, so I'm not scoring these 15
— I'm flagging that the harness this repo actually gates on and the
number GitHub's UI surfaces disagree, by exactly the gap (2 high severity)
that "outdated" vs. "reachable" is supposed to resolve. Recorded as a new,
higher-priority follow-up below rather than guessed at.

## 💡 Mechanism / forcing fact

None, for Ballast to act on directly this pass. Every Tier-1 check reruns
clean, every existing waiver's underlying fact is unchanged and not yet at
its review-by date, and the two structural gaps this charter would want
closed — satellite-graph batch coverage (follow-up 7) and the
Ballast/Dependabot division of labor (follow-up 5) — are still open human
decisions, unresolved one week after they were first raised in the
2026-09-14 report (**corrected from "three weeks" per a Codex review
comment on this PR** — both were "New this pass" in that report, not
carried from the first 09-07 pass, so this is only their second
appearance, one week open, not three). The queue-health finding
above doesn't change the class of this pass: it's evidence for whoever makes
those two decisions, not itself a security response, removal, upgrade, or
addition this pass can rehearse and merge.

## 🔧 Change

None to the dependency graph. This report is the only artifact.

## 📊 Measurement

| Check | 2026-09-14 | 2026-09-21 |
| --- | --- | --- |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean |
| Crate@version nodes / names / direct deps (root) | 791 / 704 / 134 | 764 / 686 / 138 |
| Workspace members | 33 | 37 |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 76 | 76 |
| Scheduled batch, root graph | 74 packages, Dependabot's territory | 92 packages, still Dependabot's territory |
| Scheduled batch, `fuzz/` graph | 58 packages, uncovered by any process | 66 packages, still uncovered |
| Scheduled batch, `island-flock/` graph | 29 packages, uncovered, MSRV unconfirmed | 31 packages, still uncovered, still unconfirmed |
| Open Dependabot PRs | not checked with `is:open` | 13, of which 10 are 28–69 days old |
| Wildcard ranges / unpinned git refs | 0 / 0 | 0 / 0 |
| Existing waivers re-checked (root graph only) | 3/3 | 3/3 |
| Existing waivers re-checked (satellite-only: `island-flock/deny.toml`) | not tallied separately | 2/2 unchanged (`RUSTSEC-2024-0370` still unmaintained/unreachable; `RUSTSEC-2025-0141` still open — reachability undetermined, not "valid", see follow-up 2) |

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

# duplicate-count source of truth — cargo deny check bans, not cargo deny list
cargo deny check bans 2>&1 | grep -c '^warning\[duplicate\]: found'

# reproduce the bitflags/parking_lot_core/parking_lot chain as genuinely
# in-scope (not just present somewhere in Cargo.lock) — exact deny.toml
# feature set, all targets, matching the chain cargo deny check bans traces.
# -i inverts to show DEPENDENTS of the focused package, so it must be rooted
# at the bottom of the chain (bitflags) to show all three crates in one
# probe — rooting it at parking_lot itself only shows what depends on
# parking_lot, omitting parking_lot_core and bitflags below it (caught by a
# Codex review comment on this PR after the first version of this command
# used -i parking_lot@0.11.2, which cannot reproduce the full chain)
cargo tree -p autumn-web --no-default-features --features "ws,presence,flash,cache-moka,maud,htmx,multipart,tailwind,http-client,oauth2,webauthn,openapi,mcp,markdown,db,offline-sync,test-support,telemetry-otlp,redis,i18n,embed-assets,storage,variants,reporting,mail,inbound-mail,inbound-mailgun,inbound-ses,seed,system-info,csv,pdf,system-tests,managed-pg,managed-pg-bundled,tls,acme,edge,plugin-sandbox" -e normal,build --target all -i bitflags@1.3.2

# Dependabot queue health — is:open matters, last week's query omitted it
# (search via the GitHub API/MCP: author:app/dependabot is:open)
```

## Follow-ups still open

1. Waivers sharing the **2026-10-01** review-by date — all **five** unique
   ones (root `deny.toml`'s three; `fuzz/deny.toml`'s repeated
   `RUSTSEC-2023-0071`; and `island-flock/deny.toml`'s
   `RUSTSEC-2024-0370`/`RUSTSEC-2025-0141`, **added to this line per a
   Codex review comment on this PR** — an earlier draft listed only the
   root+fuzz four here, which would have left `proc-macro-error` unscheduled
   for re-review on the date itself): re-checked this pass, all still hold
   (`RUSTSEC-2025-0141`'s reachability remains undetermined rather than
   resolved, per follow-up 2, but its waiver's own review-by date is
   unchanged). 10 days out — revisit properly at that date, not before.
2. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` (`bincode`,
   `yew`-internal, "undetermined" not "unreachable"): still unresolved.
   `git log --since=2026-09-14 -- examples/island-flock
   examples/flock/static/islands` is empty — `build-island.sh` has not
   rerun since at least the last two passes, so there's still been no
   natural point to inspect the compiled `.wasm`'s retained symbols. Still
   open.
3. The NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`
   is still an open human "ask before" question — unchanged in
   `CHANGELOG.md`, `deny.toml`, and `fuzz/deny.toml` since the first pass.
4. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run, third pass
   running. Deferred again — nothing this pass found makes a specific
   candidate hire worth attributing cost to yet, and a blanket instrumented
   build remains expensive relative to that.
5. Human decision on how Ballast and Dependabot should divide
   responsibility (raised 2026-09-14): unresolved, no comment or config
   change found addressing it since. `ci.yml`'s `supply-chain` job already
   catches a new unwaived advisory or disallowed license on every
   Dependabot PR mechanically; what's still uncovered is reachability
   judgment and usage/cost analysis on Dependabot's own bumps specifically.
6. **Narrowed this pass, then corrected twice more during its own review.**
   Last week flagged "two stale Dependabot PRs" as a queue-health
   observation; this pass's `is:open`-filtered query found the real number
   is 13 open, **10** of them 28–69 days old (an earlier draft said "~8",
   undercounting — first Codex correction). Of the seven at 69 days, an
   earlier draft called six of them "orphaned" leftovers of a
   `dependabot.yml` grouping-config change, with #1891 wrongly lumped in as
   a seventh — wrong on both counts, per two further Codex review comments:
   #1891/#2081 are individual because the `github-actions` ecosystem
   defines no groups at all, and the six `cargo` PRs (`tokio-tungstenite`,
   `sha1`, `x509-parser`, `matchit`, `rand_chacha`, `rand`) are individual
   because every one is a semver-major transition under Dependabot's own
   pre-1.0 convention, and `rust-deps` — the only group whose `"*"` pattern
   reaches these six packages — only groups `minor`/`patch` (**narrowed
   further, per a third Codex comment**: `axum-ecosystem`/`diesel-ecosystem`
   do cover majors for their own patterns; it's specifically the catch-all
   group these six fall under that excludes majors, not every group in the
   file), so nothing was ever "orphaned" by a
   config change; this is what the current config has always produced for
   a major bump on a `rust-deps`-scoped package. See the corrected mechanism
   in the evidence section above.
   The real, narrower finding: 8 individual PRs (2 actions, 6 cargo-major)
   have simply sat unreviewed for 64–69 days, plus #2179 (42d) and #2302
   (28d) — a review-backlog fact, not a configuration gap. Still not
   something this pass acts on (closing or merging any of them is a human
   call, and #2615 is explicitly an "ask before" toolchain bump), but the
   scale of the backlog is materially different from what either prior pass
   reported, and is worth a maintainer's attention: either review the eight
   stuck major/actions PRs directly, or make the deliberate "ask before"
   policy call to add a `major`-update-types group (accepting that group's
   larger, batched diffs) if the backlog is the real problem to solve.
7. The `fuzz/` (66 packages) and `island-flock/` (31 packages) scheduled
   batches remain uncovered by any process — `dependabot.yml` unchanged
   since the decision was raised. Same two options as last week: extend
   `dependabot.yml` with two more directory entries, or have Ballast own
   satellite-graph batches on its own cadence. Still a human decision, not
   actioned here. **Restored this pass, caught by a Codex review comment**:
   last week's version of this item also carried a second, separate open
   question that an earlier draft of this report dropped —
   `examples/island-flock/Cargo.toml` still declares no `rust-version` at
   all (re-checked directly: just `edition = "2024"`), unlike the root and
   `fuzz/` graphs (both `1.88.0`). Without one, there's no MSRV floor for
   `cargo update` to respect and no way to state "this batch is MSRV-safe"
   the way the other two graphs' scheduled-batch rows in this report can —
   still an open human decision, not actioned here either.
8. **New this pass, highest priority of the open items.** GitHub's native
   Dependabot alert count for the default branch (15: 2 high, 9 moderate, 4
   low, per the `git push` remote message) does not match this pass's own
   harness result (0 unwaived RustSec advisories across all 5 graphs). Needs
   a human — or a future pass with GitHub Security-tab API access this
   session didn't have — to open `/security/dependabot` directly and join
   each of the 15 against this repo's existing waivers and its non-Rust
   dependency files. **Corrected by a Codex review comment on this PR**: an
   earlier draft named `examples/island-flock/package.json` as one of those
   files — it doesn't exist; `island-flock` has only Cargo manifests and
   invokes `wasm-bindgen-cli` directly with no npm graph of its own. The
   repo's actual tracked npm manifest is
   `examples/react-graphql/frontend/package-lock.json` (plus
   `package.json` beside it) — that's the file to check alongside
   `benchmarks/runtime/django`'s Python deps to determine how many are
   already covered/waived, how many are non-Rust and outside `cargo-deny`'s
   scope entirely, and — most importantly — whether any of the 2 "high"
   alerts are a Cargo-ecosystem advisory that RustSec doesn't yet carry and
   this repo's own advisory gate can't see. That last case would be a real
   gap in the harness this charter otherwise treats as mature, and would
   turn this from a ledger report into a security-response pass.
9. **Retracted, then replaced with a real harness-methodology finding —
   caught by a further Codex review comment on this PR.** An earlier draft
   claimed the duplicate-name count fell 76→68 with `bitflags`/
   `parking_lot`/`parking_lot_core` "dropping out." Wrong: `cargo deny check
   bans` (this metric's actual stated source — **not** a CI gate, per a
   further Codex correction; `ci.yml`'s `supply-chain` job never runs
   `check bans`, only `check-advisories.sh` and `check licenses sources`,
   matching `deny.toml`'s own "`bans` ... intentionally not run in CI"
   comment) still reports 76, unchanged, and directly confirms all three
   names via a real traced dependency path (see "Graph facts" above). The
   real finding underneath the mistake: `cargo deny list --format json` —
   used for this report's node/crate-name counts, per both this and last
   week's stated methodology — silently missed a chain `check bans` finds
   without trouble. That's worth a dedicated harness-level follow-up on its
   own: pin down why `list` and `check bans` disagree (a `[graph]` or target
   resolution difference between the two subcommands is the leading
   hypothesis, unconfirmed), and decide whether `list`-derived node/name
   counts are trustworthy for this report's purposes at all, or whether
   every graph-facts metric should move to a `check bans`-equivalent source.
   Not resolved this pass — flagged here rather than guessed at, per this
   charter's own evidentiary bar.
