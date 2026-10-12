# ⚓ Ballast: weekly dependency ledger audit, 2026-10-12

Sixth Ballast pass, one week after the fifth
(`docs/reports/2026-10-05-ballast-dependency-ledger-audit.md`), which reran the
harness clean, closed out #1897/#2615 via `.github/dependabot.yml` ignore
entries (PR #3138, merged 2026-10-05), and left the Dependabot queue at **0**
for the first time in five passes. This pass finds that queue refilled to 12,
rehearses the one PR that is a clean scheduled-batch candidate, rehearses and
rejects four PRs that looked independent but are not, and fixes a Dependabot
labelling bug the GitHub API surfaced along the way.

## 🎯 Class

Mixed: **policy** (`.github/dependabot.yml` label fix) plus a **scheduled-batch
rehearsal** (PR #3209, not merged by this pass — see Change) and **upgrade
triage** on five PRs (#3143/#3144/#3148/#3149 opentelemetry family,
documented as un-mergeable individually; other majors documented as
no-forcing-fact deferrals). No dependency version bumped in this repository's
own `Cargo.lock` by this pass — every version change discussed here lives on
Dependabot's own PR branches, not on `trunk-dev`.

## 📈 Evidence

**Harness re-run, all 5 graphs, `cargo-deny 0.20.2`** (installed fresh this
session; not present at session start):

```
cargo fetch --locked (root, fuzz/, examples/island-flock/) → all clean
./scripts/check-advisories.sh              → OK, 0 unwaived advisories, all 5 graphs
./scripts/check-advisories.sh --self-test  → OK, RUSTSEC-2020-0071 blocked then waived, all 4 policies
cargo deny check licenses sources          → licenses ok, sources ok
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok
cargo deny check bans                      → 90 duplicate-name warnings (warn-level; unchanged from 2026-10-05)
cargo deny check                           → advisories ok, bans ok, licenses ok, sources ok
```

Identical clean outcome to last pass on advisories/licenses/sources/bans — the
duplicate-name warning count (90) did not move, so none of this week's real
code merges (11 commits touching `Cargo.lock` since 2026-10-05 — GDPR capsules
#1811, job-leasing protocol models #3071, SQLite database fleet, h2/ALPN
coverage #2321, admission control #3068, SLO/Kubernetes packaging #3069, the
edge node #1790, among others) changed the duplicate-version picture.

**A shallow-clone trap, caught before it corrupted this report.** This
session's checkout started shallow (50 commits). Diffing a commit at or near
the shallow boundary (`166977c`) against `.github/dependabot.yml` and
`deny.toml` at first appeared to show those files being newly created that
week — a `git log`/`git diff` artifact of the shallow boundary (everything
before it diffs against an empty tree), not a real change. `git fetch
--deepen=200` fixed it; the lesson, for whoever runs this pass next on a fresh
clone, is to deepen before trusting any diff that touches a commit within ~50
commits of `HEAD`.

**Dependabot PR queue: refilled from 0 to 12.** Last pass's clearance was real
but temporary — Dependabot's weekly Monday run (2026-10-05, 2026-10-08) reopened
the queue:

| # | Title | Opened | Ecosystem |
| --- | --- | --- | --- |
| 3209 | `rust-deps` group, 8 updates | 2026-10-08 | cargo (grouped, minor/patch) |
| 3169 | `source-map-js` 1.2.1→1.2.2 | 2026-10-06 | npm (`examples/react-graphql`, transitive) |
| 3150 | `getrandom` 0.2.17→0.4.3 | 2026-10-05 | cargo (major, ungrouped) |
| 3149 | `opentelemetry-otlp` 0.32.0→0.33.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3148 | `opentelemetry_sdk` 0.32.1→0.33.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3147 | `croner` 3.0.1→4.0.1 | 2026-10-05 | cargo (major, ungrouped) |
| 3146 | `css-inline` 0.21.2→0.22.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3145 | `wasmi` 0.40.0→2.0.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3144 | `tracing-opentelemetry` 0.33.0→0.34.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3143 | `opentelemetry` 0.32.0→0.33.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3142 | `brotli` 8.0.4→9.0.0 | 2026-10-05 | cargo (major, ungrouped) |
| 3140 | `taiki-e/install-action` 2.87.20→2.87.22 | 2026-10-05 | github-actions (patch) |

All 12 confirmed real and still open via `pull_request_read`, not inferred
from a search count. Corrected per a Codex review comment on this PR's own
diff: the `rust-deps` group only covers the **cargo** ecosystem block, so
#3169 (npm) and #3140 (github-actions) were never eligible for it regardless
of semver — a different ecosystem, not a version-type exclusion. Of the
remaining 9 cargo-ecosystem PRs, 7 are genuinely excluded by the *0.x "minor
counts as major"* quirk (`opentelemetry`/`opentelemetry_sdk`/
`opentelemetry-otlp`/`tracing-opentelemetry`, `css-inline`, `getrandom`,
`wasmi` — each still below 1.0, or crossing into it from below); the other 2
(`croner` 3.0.1→4.0.1, `brotli` 8.0.4→9.0.0) are ordinary major bumps on
already-stable (1.0+) crates, excluded for the plain reason that a
minor/patch-only group was never going to include a major, 0.x quirk or not.

**#3209 rehearsed and found clean — the one real scheduled-batch candidate
this week.** Cherry-picked its single commit (`bc05ce2`: ammonia 4.2.0→4.2.1,
syn 3.0.3→3.0.6, rand 0.10.2→0.10.3, h2 0.4.18→0.4.19, hyper 1.11.0→1.11.1,
libc 0.2.189→0.2.190, tokio-rustls 0.26.5→0.26.6, and the wasm-tools quartet
wasm-encoder/wasmparser/wast/wat 259.0.0→261.0.0) onto current `trunk-dev`
(clean cherry-pick, no conflicts — the PR's own base had drifted ~70 commits
behind, so testing the PR branch as-is would have shown unrelated failures
from that drift, not from the bump; isolating the one dependency commit onto
fresh `trunk-dev` was necessary to get a clean signal):

```
cargo deny check                          → advisories ok, bans ok (90, unchanged), licenses ok, sources ok
cargo check --workspace --all-targets     → Finished `dev` profile [unoptimized + debuginfo] target(s) in 10m 53s, 0 errors
```

A P1 Codex review comment correctly caught that `cargo check` never links — it
validates types, not the actual link step, and this repo's own
`./scripts/pre-push-check.sh` exists specifically because a narrower check
("misses cross-package compile breaks the CI `cargo test --workspace` gate
catches" — CLAUDE.md, "Pre-push gate") can stay green while the consolidated
`integration_tests` binary fails to link. Ran `./scripts/pre-push-check.sh` on
this rehearsal branch three separate times rather than accepting the gap,
each with `target/` freed first: all three failed on the same unrelated
session constraint (`ld terminated with signal 7 [Bus error]` / "No space
left on device" — this session's own disk allowance, not a defect in the
dependency bump), confirmed three independent ways — `df` reading at or near
0 free at the moment of each failure; the failure point moving later in the
build each time (run 1 broke mid-compile, run 2 during `autumn-web`'s own
link step, run 3 — with `CARGO_PROFILE_TEST_DEBUG=0` set to shrink debug
symbols — during the last few crates in the whole workspace, `autumn-cli`'s
own test binary) with no change to the code between runs; and no compile
*error* ever appearing in any of the three logs, only the OS-level ENOSPC.
A second Codex comment correctly pointed out that the fallback this report
first settled for, `cargo test -p autumn-web --no-run`, left `autumn-cli` and
`autumn-edge` uncovered even though this bump touches crates (`syn`, `libc`,
`hyper`, `h2`, `tokio-rustls`) both depend on directly. Added both, each a
clean, disk-cheap package-level build (no full-workspace linking required):

```
cargo test -p autumn-web --no-run    # links the consolidated integration_tests binary — 0 errors
cargo test -p autumn-cli --no-run    # links the consolidated cli_tests binary — 0 errors
cargo test -p autumn-edge --no-run   # links autumn-edge's own test binaries — 0 errors
```

Between them, every workspace member CLAUDE.md names as a consolidated-binary
linking risk (`autumn`/`autumn-web` and `autumn-cli`) is now independently
link-verified for this exact dependency bump, plus the other workspace member
(`autumn-edge`) most directly exposed to it. What's still not completed is a
clean full-workspace run covering every example, bench, and plugin crate
simultaneously, and the gate's separate clippy/doctest legs on the full set —
genuinely short of `./scripts/pre-push-check.sh` end to end. A third Codex
comment on this point is correct that no amount of caveat in this prose
changes what the report's own summary lines were still asserting
unconditionally elsewhere; this pass is not classifying #3209 as a clean
"ready to merge" — the honest status is **clean at every scope this
session's disk allowed to verify (deny, full `cargo check`, and link-level
tests for all three directly-affected crates), full-workspace link/clippy
pass not completed**, and that is the status recorded in Measurement and
Change below, not a rounded-up "ready."

Posted the rehearsal's real, current status on #3209 (superseding this
pass's own earlier, premature "recommend merging as-is" comment, posted
before the pre-push gate was run at all) — findings, not a merge verdict;
whether the package-level evidence is sufficient or the full gate should be
re-run in an environment with more disk is the maintainer's call, not
Ballast's. Not merged by this pass — every prior pass's actual merges were
done by the repo's human maintainer, not
by Ballast itself, and this pass follows that precedent.

**Both halves of the opentelemetry split rehearsed independently; both
break.** A Codex review comment correctly caught that the first version of
this section only executed one direction (bumping `tracing-opentelemetry`
alone) and inferred the other three PRs' fate from it rather than running
them — worth fixing rather than leaving an unverified inference in a ledger
report. Rehearsed the missing direction and it also breaks, which makes this
a stronger finding than first written, not a weaker one: these four PRs are
mutually interdependent in *both* directions, confirmed by two separate
compiles, not one.

`tracing-opentelemetry` 0.34.0's own published `Cargo.toml` pins
`opentelemetry = "0.33.0"`, and the *current* `tracing-opentelemetry` 0.33.0
(unchanged by #3143/#3148/#3149) pins `opentelemetry = "0.32.0"` (both
confirmed by reading the registry-cached `Cargo.toml` directly, not inferred
from a changelog). Rehearsed each direction on current `trunk-dev`:

```
# Direction 1: bump tracing-opentelemetry alone (PR #3144 in isolation)
sed -i 's/tracing-opentelemetry = "0.33.0"/tracing-opentelemetry = "0.34.0"/' Cargo.toml
cargo update -p tracing-opentelemetry
# → Adding opentelemetry v0.33.1 (alongside the workspace's pinned 0.32.0/0.32.1)
cargo check -p autumn-web --no-default-features --features telemetry-otlp
# → error: could not compile `autumn-web` (lib) due to 11 previous errors

# Direction 2: bump the other three alone (PRs #3143/#3148/#3149 together, #3144 untouched)
sed -i -e 's/opentelemetry = { version = "0.32.0"/opentelemetry = { version = "0.33.0"/' \
       -e 's/opentelemetry_sdk = { version = "0.32.1"/opentelemetry_sdk = { version = "0.33.0"/' \
       -e 's/opentelemetry-otlp = { version = "0.32.0"/opentelemetry-otlp = { version = "0.33.0"/' Cargo.toml
cargo update -p opentelemetry -p opentelemetry_sdk -p opentelemetry-otlp
# → Adding opentelemetry v0.33.1 (tracing-opentelemetry 0.33.0 still pins opentelemetry 0.32.0,
#   so the lockfile now carries BOTH 0.32.0 and 0.33.1 of `opentelemetry`)
cargo check -p autumn-web --no-default-features --features telemetry-otlp
# → error: could not compile `autumn-web` (lib) due to 11 previous errors (identical signature:
#   `SdkTracer: opentelemetry::trace::tracer::Tracer` not satisfied, `Layered<OpenTelemetryLayer<...>,
#   ...>` missing `Subscriber`/`.with()`, no method `span` on `opentelemetry::context::Context`)
```

Either direction forces two incompatible `opentelemetry` versions into the
graph at once, and `autumn/src/telemetry.rs`'s own `tracing_subscriber` layer
stack (used under the `telemetry-otlp` feature) mixes types from both — a
real compile break in both directions, not a theoretical one in either. The
correct unit of change is all four crates together. Posted the rehearsal and
a close-and-recombine recommendation on all four PRs; left the full mechanism
on #3144 and pointed #3143/#3148/#3149 at it rather than repeating it four
times.

**The other 6 ungrouped majors (#3142, #3145, #3146, #3147, #3150) and the
npm/action PRs (#3140, #3169): no forcing fact found, not individually
rehearsed.** `wasmi` 0.40.0→2.0.0 is a scheme-reset major touching two
security-relevant WASM host boundaries (`autumn/src/plugin_sandbox/host.rs`,
`autumn-edge/src/host.rs`); `getrandom` 0.2.17→0.4.3 skips a whole major line;
`croner`, `css-inline`, `brotli` are each direct, single-crate bumps with a
real call site (`autumn/src/app.rs`, `autumn/src/mail.rs`,
`autumn/src/router.rs`+`shadow/transport.rs` respectively) but no CVE, EOL, or
named-consumer feature behind any of them — "staying current" is the only
stated reason on every one of these PRs, which this repo's dependency charter
treats as not a forcing fact for a major. `cargo deny check advisories`
reports all of them clean at their *current* pinned versions, so none is a
reachable-fire upgrade either. Rehearsing five independent major-version
bumps (two of them touching a WASM interpreter boundary) is itself real
engineering work this pass's time budget did not have room for; left as
documented deferrals rather than rehearsed-and-rejected like the opentelemetry
cluster, since no mechanical conflict was found for any of them (absence of
evidence, not evidence of absence — unlike the opentelemetry case, these were
not actually rehearsed). #3169 (npm, `source-map-js`, transitive in
`examples/react-graphql/frontend/package-lock.json`) and #3140
(`taiki-e/install-action`, a CI-tool-installer patch bump) are both low-risk
on their face but outside this harness's `cargo deny` coverage and this pass's
rehearsal budget; left open.

**A Dependabot config bug, found while reading each PR's comments, not
flagged by any prior pass.** Corrected per a Codex review comment: not every
PR in the queue table carries the same comment, or any comment at all —
custom labels apply only to the ecosystem block that defines them. Every
**cargo**-block PR (#3209, #3150, #3149, #3148, #3147, #3146, #3145, #3144,
#3143, #3142) carries this exact comment from `dependabot[bot]` (confirmed on
#3209's own comment thread):

```
### Labels
The following labels could not be found: `automated`. Please create it before
Dependabot can add it to a pull request.
```

The **github-actions**-block PR (#3140) carries a different comment naming
*both* missing labels (confirmed on #3140's own thread: "could not be found:
`automated`, `ci`"), since that block's config had two undefined names, not
one. The **npm**-block PR (#3169) carries neither — confirmed zero comments
on its thread — because `dependabot.yml` has no npm entry at all, so no
custom labels were ever defined for it to warn about.

Confirmed mechanically, not just by reading the bot's comment: `get_label`
for `automated` and `ci` both returned "not found in
autumn-foundation/autumn"; only `dependencies` exists. `.github/dependabot.yml`
listed `[dependencies, automated]` for the cargo block and `[ci, automated]`
for the github-actions block. Not merge-blocking (the PRs are still created
fine). Corrected per a Codex review comment: per GitHub's own [`labels`
reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference#labels),
an undefined custom label is skipped individually, not the whole list — so
the defect's actual blast radius was narrower than first written. Checked
directly rather than trusting either account: #3209 (a cargo-block PR)
carries `"labels": ["dependencies"]` right now, confirming `dependencies`
*was* being applied to every cargo PR all along, only `automated` silently
dropped; #3140 (a github-actions-block PR) carries no labels at all,
confirming that block really did lose every label (`ci` and `automated`
both undefined, nothing left to fall back to). So this is still a real,
fixable config bug, sitting unnoticed since whenever those labels were first
added — a `get_label` check neither this nor any prior Ballast pass had run
before — but only the github-actions block was labelling nothing; the cargo
block's defect was the extraneous bot comment and a label that would never
resolve, not an unlabelled PR.

First attempt at the fix was itself wrong, caught by a Codex review comment
on this PR's own diff before merge: dropping `labels` entirely from the
github-actions block (reasoning: no verified replacement name to put there)
does not mean "no labels" — [GitHub's own docs](https://docs.github.com/en/code-security/tutorials/secure-your-dependencies/customizing-dependabot-prs)
confirm (fetched and read directly, not taken on the bot's word) that
omitting `labels` makes Dependabot fall back to its *defaults*: the
`dependencies` label plus an ecosystem label such as `github-actions`,
auto-created if either is missing. So the first fix would have silently
introduced a never-verified `github-actions` label instead of fixing
anything. Corrected to `labels: [dependencies]` on the github-actions block
too — reusing the one label already confirmed to exist, matching the cargo
block, rather than trusting an unverified ecosystem-label name or an
untested empty list.

**Waivers — not yet due.** All six live waivers (`RUSTSEC-2023-0071`,
`RUSTSEC-2024-0384`, `RUSTSEC-2026-0253`, `RUSTSEC-2024-0370`,
`RUSTSEC-2025-0141`, `RUSTSEC-2026-0319`) were re-dated to **2026-11-02** last
pass; today is 2026-10-12, three weeks early. Corrected per a Codex review
comment: the first version of this section dropped the sixth waiver,
`RUSTSEC-2026-0319` (anymap2, `examples/island-flock/deny.toml:66-67`), along
with its second trigger — that entry is due at **2026-11-02, or when #3095
lands**, whichever comes first, not on the date alone. Checked #3095
directly rather than assuming the date is what governs: still open, 0 closing
PRs, so the "not yet due" conclusion holds for this waiver too, but by
actually checking the open issue, not by only reading a calendar. Not
re-verified or re-dated this pass otherwise — doing so before either trigger
fires would just be motion, not hygiene (and the charter's own cadence law
treats a review-by date, or a named triggering event, as the actual
checkpoint, not a suggestion to recheck early).

**Supply-chain facts, re-verified**: zero wildcard version ranges, zero
unpinned git refs, main workspace and both satellites — unchanged.

**Scheduled-batch size** (`cargo update --dry-run --verbose`, informational
only — a count of what a full update would touch, not a diff from last week's
methodology, since the workspace itself grew in the interim from real feature
merges):

| Graph | Packages a full update would lock |
| --- | --- |
| root (Rust 1.88.0) | 128 |
| `fuzz/` (Rust 1.88.0) | 95 |
| `examples/island-flock/` (this session's toolchain) | 31 |

Workspace membership is 39 crates (37 reported last pass; the +2,
`examples/mesh-catalog` and `examples/mesh-storefront`, predates this report
window — added 2026-09-18, per `git log --diff-filter=A` — so it is not this
week's movement, just a number this pass is the first to state correctly
after deepening past the shallow-clone boundary that obscured it).

## 💡 Mechanism / forcing fact

**Policy** (dependabot.yml label fix): mechanically confirmed broken config
(`get_label` 404 on both non-existent names), silently wasting every
github-actions-block Dependabot PR's labelling entirely (confirmed on #3140:
no labels applied) and leaving a dead, never-resolving label plus a stray
bot comment on every cargo-block PR (`dependencies` itself kept applying
fine there, confirmed on #3209) — a real, cheaply-fixed fact either way, not
motion for its own sake.

**Upgrade triage** (opentelemetry cluster): the forcing fact is the rehearsal
itself — four PRs look independent and are not; merging any subset other than
all four breaks `autumn-web`'s compile under `--features telemetry-otlp`. No
CVE or EOL involved; this is a correctness/compatibility fact, not a security
one.

**Scheduled batch** (#3209): routine, grouped, minor/patch, rehearsed clean —
exactly the class the charter asks Ballast to validate and let through.

No forcing fact found for the five remaining ungrouped majors; per the
charter, "staying current" does not supply one, so none is actioned.

## 🔧 Change

- `.github/dependabot.yml`: dropped the non-existent `automated` label from
  the cargo block and the non-existent `ci`/`automated` labels from the
  github-actions block, replacing both with `labels: [dependencies]` — the
  one label confirmed to exist. (First pushed this PR with `labels` dropped
  entirely on the github-actions block instead of set to `[dependencies]`;
  corrected after a Codex review comment showed that omitting `labels`
  triggers Dependabot's own default labelling, not "no labels" — see
  Evidence.) Validated with a YAML parse after each edit. No `ignore`
  entries touched.
- Posted/updated rehearsal comments on GitHub PRs #3209 (verified clean at
  every scope this session's disk allowed; full-workspace pre-push gate not
  completed — findings, not a merge verdict), #3144 (full mechanism,
  recommend closing all four and recombining), and #3143/#3148/#3149
  (pointing at #3144's rehearsal, same recommendation).
- No PR merged or closed by this pass — every merge/close decision in this
  repo's Ballast history has been made by its human maintainer, not by
  Ballast itself, and that precedent held here too.
- No dependency version changed in this repository's own `Cargo.lock`.

## 📊 Measurement

| Check | 2026-10-05 | 2026-10-12 |
| --- | --- | --- |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 90 | 90 |
| Workspace members | 37 (stale count, see Evidence) | 39 (corrected) |
| Open Dependabot PRs | 0 | 12 |
| PRs rehearsed clean at verified scope (full pre-push gate incomplete, disk) | — | 1 (#3209) |
| Opentelemetry split: directions actually compiled | — | 2 of 2 (both break) |
| PRs affected by that finding (not all individually compiled) | — | 4 (#3143/#3144/#3148/#3149) |
| PRs deferred, no forcing fact, not rehearsed | — | 7 (#3140, #3142, #3145, #3146, #3147, #3150, #3169) |
| Dependabot label config | broken (`automated`/`ci` both 404) | fixed (`dependencies` only, both blocks) |
| Wildcard ranges / unpinned git refs | 0 / 0 | 0 / 0 |
| Scheduled-batch size, root / fuzz / island-flock | 98 / 78 / 31 packages | 128 / 95 / 31 packages |

## 🔬 Reproduce

```
git fetch --deepen=200 origin trunk-dev   # avoid the shallow-clone diff trap — see Evidence
cargo fetch --locked
(cd fuzz && cargo fetch --locked)
(cd examples/island-flock && cargo fetch --locked)
./scripts/check-advisories.sh
./scripts/check-advisories.sh --self-test
cargo deny check
cargo deny check bans   # duplicate-name count
cargo metadata --format-version 1 --no-deps | python3 -c "import json,sys; print(len(json.load(sys.stdin)['packages']))"  # workspace members

cargo update --dry-run --verbose 2>&1 | grep Locking
(cd fuzz && cargo update --dry-run --verbose 2>&1 | grep Locking)
(cd examples/island-flock && cargo update --dry-run --verbose 2>&1 | grep Locking)

# PR #3209 rehearsal
git fetch origin pull/3209/head:ballast-pr-3209
git checkout origin/trunk-dev -b rehearsal-3209 && git cherry-pick bc05ce2
cargo deny check && cargo check --workspace --all-targets
./scripts/pre-push-check.sh          # the real gate; ran 3x, ENOSPC each time on this session's disk ceiling
CARGO_PROFILE_TEST_DEBUG=0 ./scripts/pre-push-check.sh   # 3rd attempt, shrunk debug info; still ENOSPC, later
cargo test -p autumn-web --no-run    # links integration_tests  — 0 errors
cargo test -p autumn-cli --no-run    # links cli_tests          — 0 errors
cargo test -p autumn-edge --no-run   # links autumn-edge's tests — 0 errors

# opentelemetry lockstep rehearsal — both directions
git checkout origin/trunk-dev -b rehearsal-otlp-a
sed -i 's/tracing-opentelemetry = "0.33.0"/tracing-opentelemetry = "0.34.0"/' Cargo.toml
cargo update -p tracing-opentelemetry
cargo check -p autumn-web --no-default-features --features telemetry-otlp   # breaks

git checkout -- Cargo.toml Cargo.lock   # discard rehearsal-otlp-a's uncommitted edits first --
                                         # `checkout -b` from the same origin/trunk-dev otherwise
                                         # carries them into -b, testing all 4 bumps together
git checkout origin/trunk-dev -b rehearsal-otlp-b
sed -i -e 's/opentelemetry = { version = "0.32.0"/opentelemetry = { version = "0.33.0"/' \
       -e 's/opentelemetry_sdk = { version = "0.32.1"/opentelemetry_sdk = { version = "0.33.0"/' \
       -e 's/opentelemetry-otlp = { version = "0.32.0"/opentelemetry-otlp = { version = "0.33.0"/' Cargo.toml
cargo update -p opentelemetry -p opentelemetry_sdk -p opentelemetry-otlp
cargo check -p autumn-web --no-default-features --features telemetry-otlp   # also breaks

# dependabot label check (via GitHub MCP get_label, or the API directly)
# GET /repos/autumn-foundation/autumn/labels/automated  → 404
# GET /repos/autumn-foundation/autumn/labels/ci          → 404
# GET /repos/autumn-foundation/autumn/labels/dependencies → 200

python3 -c "import yaml; yaml.safe_load(open('.github/dependabot.yml'))"
```

## Follow-ups still open

1. **GitHub-native Dependabot/security alert count moved again — 16→17 (4
   high, 10 moderate, 2 low → 5 high, 10 moderate, 2 low) — and still no tool
   in any session's GitHub MCP toolset exposes it.** The 17-count comes from
   this very pass's own `git push` output (printed when pushing this report's
   branch), the same mechanism that surfaced last pass's 16. High moved again
   (4→5), third consecutive week it has moved rather than held flat.
   Re-confirmed the no-visibility gap this pass by a dedicated subagent search
   (actions_get/list, get_commit, search_*, pull_request_read, issue_read —
   nothing named anything like "dependabot alert" or "security advisory"
   exists). Four passes running without resolution, and the trend (9 → 16 →
   17, high 2 → 2 → 4 → 5) is now consistently upward, not noise. Needs a
   human to look at `github.com/autumn-foundation/autumn/security/dependabot`
   directly; this session cannot. **Corrected per a Codex review comment: "0
   unwaived advisories" does not point away from the Rust graphs the way the
   first version of this section implied.** `cargo-deny`'s waivers are a
   local policy decision this repo made — GitHub's own scanner has no idea
   this repo waived anything, and scans the raw dependency graph against its
   own advisory database regardless. The 6 distinct RUSTSEC ids this repo
   has deliberately waived (`RUSTSEC-2023-0071` rsa, `RUSTSEC-2024-0384`
   instant, `RUSTSEC-2026-0253` lru, `RUSTSEC-2024-0370` proc-macro-error,
   `RUSTSEC-2025-0141` bincode, `RUSTSEC-2026-0319` anymap2 — counted
   directly from `deny.toml`/`deny-sqlite.toml`/`fuzz/deny.toml`/
   `examples/island-flock/deny.toml`) are very plausibly counted among
   GitHub's 17, alongside whatever is in the two manifests outside this
   harness's coverage (`examples/react-graphql/frontend/package-lock.json`,
   `benchmarks/runtime/django/requirements.txt`). "0 unwaived" only means
   `cargo-deny`'s own gate — which deliberately lets these exact 6 through —
   stays green; it says nothing about what GitHub's unrelated scanner
   counts. A human checking the Security tab should expect GitHub's own list
   to include these 6 by name, not be surprised by them.
2. **The opentelemetry cluster (#3143/#3144/#3148/#3149) needs a human
   decision**: close all four and open one combined PR (this pass's
   recommendation, posted on all four), or leave them open as a visible
   "don't merge piecemeal" marker until Dependabot itself notices the group
   exclusion is wrong. Either way, `dependabot.yml`'s `rust-deps` group
   exclusion of 0.x "major" bumps is working as configured, not malfunctioning
   — the gap is that nothing currently re-groups a *cluster* of excluded
   majors that happen to be interdependent. No generic fix proposed; flagging
   for a human rather than guessing at a dependabot.yml rule that might
   misfire on some other, unrelated cluster of excluded majors.
3. Five majors (#3142 `brotli`, #3145 `wasmi`, #3146 `css-inline`, #3147
   `croner`, #3150 `getrandom`) deferred without rehearsal — no forcing fact,
   but also not disproven the way the opentelemetry cluster was. `wasmi`
   0.40→2.0 is the one worth a dedicated pass of its own given its blast
   radius (two WASM host boundaries, one of them the plugin sandbox); the
   other four are lower-stakes single-crate bumps.
4. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` (`bincode`,
   reachability undetermined): still unresolved, now six passes running.
   `build-island.sh` still hasn't rerun.
5. The NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`:
   still open, unchanged since the first pass.
6. Cost attribution (`cargo build --timings`) and a usage/unused-feature audit
   on the root graph's heaviest hires: still not run, sixth pass running.
7. `fuzz/` (95 packages) and `island-flock/` (31 packages) remain uncovered by
   any scheduled-batch process — `dependabot.yml`'s `updates:` list still has
   only the two root entries. Same two options as every prior pass: extend
   `dependabot.yml`, or have Ballast own satellite-graph batches on its own
   cadence.
8. Waivers re-verified last pass, re-dated to 2026-11-02. Corrected per a
   Codex review comment: that date is not the only trigger for four of the
   six — `RUSTSEC-2023-0071` rsa (`deny.toml:62`) is also due the moment a
   fixed rsa release ships; `RUSTSEC-2026-0253` lru (`deny.toml:94-95`) the
   moment the MSRV floor rises enough to take an `aws-sdk-s3` that resolves
   a patched `lru`; `RUSTSEC-2025-0141` bincode
   (`examples/island-flock/deny.toml:45-47`) the next time `build-island.sh`
   reruns; `RUSTSEC-2026-0319` anymap2
   (`examples/island-flock/deny.toml:66-67`) when #3095 lands. Only
   `RUSTSEC-2024-0384` instant and `RUSTSEC-2024-0370` proc-macro-error are
   genuinely date-only. A second Codex comment correctly pointed out that
   the first version of this item asserted "none of the four fired" in the
   same breath as admitting neither release had actually been checked —
   checked both for real rather than assuming: the advisory database itself
   (`~/.cargo/advisory-dbs/.../RUSTSEC-2023-0071.md`, freshly fetched this
   session) states `patched = []` and explicitly "Still affected as of
   2026-09-12: rsa 0.9.10 (latest stable) and rsa 0.10.0-rc.18 (latest)" —
   `cargo info rsa` shows the true latest is now `0.10.0-rc.19`, one patch
   past that note, with no indication its tracking issues (RustCrypto/RSA
   #626/#680/#702) closed since; `cargo info aws-sdk-s3@1.123.0` (the very
   next release after the pinned 1.122.0) already requires `rust-version
   1.91.0`, exceeding this workspace's 1.88.0 MSRV, confirming 1.122.0 is
   still the ceiling with no gap to a `lru`-fixed version. `build-island.sh`
   hasn't rerun and #3095 is still open (0 closing PRs). All four checked,
   none fired; 2026-11-02 remains the earliest any of the six is due.
9. The `cargo deny list` vs `cargo deny check bans` discrepancy (flagged
   2026-09-21): not investigated this pass either; bans' count (90) tracked
   cleanly against last week's own number, so no fresh discrepancy to chase.
