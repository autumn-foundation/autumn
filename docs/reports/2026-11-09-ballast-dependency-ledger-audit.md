# ⚓ Ballast: dependency ledger audit, 2026-11-09

Eighth Ballast pass (prior pass: 2026-10-26 — a two-week gap, not a skipped
week; no Ballast run is recorded for 2026-11-02). This pass's headline: two
RustSec waivers' `review-by` dates had quietly passed (2023-0071 on
2026-11-02, 2026-0387 on 2026-11-08) while nobody was watching, and when the
trigger was actually pulled it turned up real news — `rsa` shipped its first
non-prerelease fixed release (0.10.0 final) since the advisory opened in
2023. Chasing that down produced a materially better reachability verdict
(verify-only, not "exposure is limited") and a rehearsal showing *why* the
fix still can't be taken. Separately, the Dependabot queue triaged dead over
the last two weeks almost entirely on its own: a human-authored PR (#3471,
merged 2026-10-27) rolled up 8 of the 9 PRs this ledger was tracking,
resolving three of last pass's open follow-ups without any Ballast action.

## 🎯 Class

Policy: two stale advisory-waiver `review-by` dates repaired with corrected
reachability evidence (RUSTSEC-2023-0071, RUSTSEC-2026-0387) across
`deny.toml`, `deny-sqlite.toml`, `autumn-cli/src/templates/deny.toml.tmpl`,
`fuzz/deny.toml`, and `docs/guide/supply-chain.md`. Secondary: Dependabot
queue triage (no Ballast merges this pass — the queue resolved itself via a
human PR; see Evidence) and CI-health re-check.

## 📈 Evidence

**Trigger check.** `grep -n review-by` across all four advisory policies
showed two dates already in the past as of 2026-11-09:

| Advisory | Old review-by | Days overdue |
| --- | --- | --- |
| RUSTSEC-2023-0071 (`rsa`, Marvin Attack) | 2026-11-02 | 7 |
| RUSTSEC-2026-0387 (`crypto-bigint`) | 2026-11-08 | 1 |

Per Ballast's own rule, a stale trigger firing means revisit now, not defer
again.

**RUSTSEC-2023-0071 — the trigger found real news.** The old waiver text
said "no fixed rsa release exists." Querying the sparse registry index
directly (`https://index.crates.io/3/r/rsa`) showed `rsa` 0.10.0 — the
**final**, non-prerelease release — now published (the `0.10.0-rc.19`
prerelease, already present in this tree via `webauthn-rs` → `crypto-glue`,
is not the same thing cargo-deny's `patched = [">= 0.10.0"]` range was
written against). That is a forcing fact the old waiver text no longer
describes correctly, so it needed more than a date bump.

A forked investigation (kept out of this report's main thread; full findings
recorded in the commit) read the actual code and ran a rehearsal:

- **Every RSA use in this codebase is public-key *verify* only.**
  `autumn/src/inbound_mail.rs`'s `verify_rsa_signature` (SNS signing-cert
  check, `inbound-ses` feature, opt-in) calls
  `RsaPublicKey::from_public_key_der` + `.verify(...)` — no private key.
  `autumn/src/auth.rs`'s JWKS verification path (`jwt` feature, default-on)
  only restricts which algorithms a JWKS *public* key may verify; a
  crate-wide grep for `EncodingKey::from_rsa*` or `jsonwebtoken::encode`
  with an RSA key found zero hits outside one test fixture using
  `from_secret`. This app never holds or uses an RSA *private* key.
- The Marvin Attack is a private-key timing sidechannel — it has no secret
  to leak from a verify-only consumer, independent of which `rsa` version is
  in the graph. This is a **stronger and more precise reachability verdict**
  than the old text's "real exposure is limited" — it's unreachable, not
  merely low-practicality.
- **Rehearsed bumping autumn-web's own direct `rsa` dependency** (the
  `inbound-ses` feature's `rsa = { version = "0.9", ... }`, independent of
  jsonwebtoken's transitive copy) from `"0.9"` to `"0.10"`. It doesn't
  resolve: `webauthn-rs` 0.6's `crypto-glue` pins `rsa = "^0.10.0-rc.18"`
  and requests `rsa`'s `sha2` feature; final `rsa` 0.10.0 made `sha2` a
  mandatory (non-optional) dependency rather than a feature, so Cargo can't
  unify `crypto-glue`'s requirement with the final release at all —
  *"package `crypto-glue` depends on `rsa` with feature `sha2` but `rsa`
  does not have that feature."* This is an upstream incompatibility between
  `crypto-glue`/`webauthn-rs` and the brand-new final release, not anything
  in our own code. `jsonwebtoken` 11.1.0 (confirmed latest via the sparse
  index) also still pins `rsa` `^0.9` and hasn't moved.
- Working tree confirmed clean after the rehearsal (reverted, nothing
  committed from it).

This is a **documented pin / negative result** per the hard gate's
acceptable outcomes: a real upgrade was rehearsed, it failed for a clear,
named upstream reason, and the waiver now carries the corrected verdict and
an accurate revisit trigger (the upstream incompatibility clearing) instead
of a stale one ("no fixed release exists," now false).

**RUSTSEC-2026-0387 — reconfirmed unchanged, nothing new to act on.**
Queried the sparse index for `jsonwebtoken` (latest: 11.1.0, unchanged),
`crypto-bigint` (latest: 0.7.11, irrelevant — the vulnerable copy is pinned
at 0.5.5 by `elliptic-curve` 0.13, a different major line), `p256`/`p384`
(latest: 0.14.0, matching what `webauthn-rs`'s `crypto-glue` path already
uses — the AWS SigV4 and WebAuthn paths are already on the patched RustCrypto
line via `crypto-bigint` 0.7.10/0.7.11). `jsonwebtoken` remains the sole
holdout on the old `p256`/`p384` 0.13 / `elliptic-curve` 0.13 /
`crypto-bigint` 0.5.5 stack; it hasn't released a version that moves. No
forcing fact changed; the waiver's reasoning was already correct, just its
date had lapsed.

**Dependabot queue — resolved itself via a human PR, not a Ballast merge.**
`search/issues` for `is:pr is:open author:app/dependabot` returned exactly
one Rust/Actions result (`#3620`, `actions/setup-node` 4→7); every one of the
9 PRs tracked in the 2026-10-26 report (`css-inline`, the `opentelemetry`
trio, `getrandom`, `wasmi`, `croner`, `brotli`, `tracing-opentelemetry`) is
now closed. Timeline + comments trace this to **#3471**
("dependency refresh for 0.9.0," merged 2026-10-27 by a human maintainer,
cross-referenced from each closed PR's "superseded" comment): it rolled up 8
of the 9 bumps in one rehearsed PR with ported code for the API breaks
(`getrandom` 0.4, `css-inline` 0.23.1 — one version past the tracked bump,
latest at merge time), and **explicitly did not take `wasmi` 0.40→2.0**,
with a concrete, named reason: `wasmi` 2.0.0 reads a bad instance pointer in
`Caller::get_export` on Windows when built at a different opt-level than its
caller (this workspace's dev profile sets `opt-level = 3` for `wasmi`),
reproduced standalone outside the workspace and confirmed as real UB, not a
test artifact. This closes three of last pass's open follow-ups (css-inline
reachability, croner, and wasmi's own disposition) without any action here —
recorded so a future pass doesn't re-research work already done.

**The one remaining open PR, `#3620` (`actions/setup-node` 4→7, GitHub
Actions, not a Rust crate), is blocked by an unrelated pre-existing test
failure, not by the bump.** `Test (Docker)`'s job log shows exactly one
failure: `integration::repository_dependent_ordering::
cascaded_child_commit_hook_runs_once_per_child_on_delete_many`, an
`assert_eq!` mismatch inside `autumn/tests/integration/
repository_dependent_ordering.rs:887` — a repository-cascade-hook ordering
test with no connection to Node.js tooling. **Last pass's CI-health finding
(every "Test suite" shard reported `skipped` under Dependabot's restricted
token) did not reproduce this pass**: every `Test (${{ matrix.os }},
partition)` shard on this PR's run completed `success`, including all
Windows/macOS/Linux partitions and the `sim` shards. That specific failure
mode looks fixed or was circumstantial; the aggregator is red for an
ordinary (if unrelated) test failure this time, which is a more trustworthy
signal than last pass had. Not re-diagnosing the ordering-test failure
itself here — it's app-level test logic, outside a dependency-hygiene pass's
scope, and it blocks only one low-stakes CI-tooling bump.

**License and source checks**: clean, both graphs (`cargo deny check
licenses sources` on root and `deny-sqlite.toml`).

**Duplicate crate versions** (`cargo deny check bans`, warn-level): **67**
on root (down from 91 last pass), **59** on `deny-sqlite.toml`. This drop
came from two weeks of ordinary feature work touching `Cargo.lock`
(including #3471's own `cargo update`), not from anything this pass did —
recorded as the new baseline, not claimed as a Ballast result.

**GitHub-native Dependabot alert count**: still not checked this pass — the
REST alerts endpoint (`dependabot/alerts`) returned "Write access to this
GitHub API path is not permitted through this proxy" (HTTP 403) even on a
GET, so this tooling gap (fifth pass now) persists; no number to report.

## 💡 Mechanism / forcing fact

Two independent triggers fired on their own schedule: a `review-by` date
passing is this project's own contract to re-run the check, not a courtesy
reminder, and it fired for both entries this pass. For RUSTSEC-2023-0071 the
underlying fact actually changed (a final fixed release shipped for the
first time in three years) — exactly the kind of fact a lapsed pin's trigger
exists to catch before someone else trips over it. The correct action
turned out not to be "upgrade" (upstream blocks it) but "replace a
now-inaccurate reason with a correct, stronger one, and point the trigger at
the real blocker (the crypto-glue/webauthn-rs sha2-feature conflict) instead
of the now-false one ('no fixed release')." For RUSTSEC-2026-0387 the fact
underneath was unchanged, so the only forcing fact was the date itself.

## 🔧 Change

- `deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`,
  `autumn-cli/src/templates/deny.toml.tmpl`: rewrote the RUSTSEC-2023-0071
  waiver's comment and `reason` string with the verify-only reachability
  verdict (file:line evidence), the rehearsal's finding (rsa 0.10.0 final
  exists but is blocked from both consumers by named, specific reasons), and
  a corrected revisit trigger; `review-by` moved from the lapsed 2026-11-02
  to 2026-12-05. Refreshed the RUSTSEC-2026-0387 waiver's `reconfirmed`
  date and moved its `review-by` from the lapsed 2026-11-08 to 2026-11-23.
- `docs/guide/supply-chain.md`: updated the worked waiver example (the exact
  `reason` string it quotes) and the surrounding paragraph to match —
  "no fix an app can take exists for either" was no longer accurate for
  `rsa` once a final release shipped upstream, even though the practical
  effect (an app still can't take it) is unchanged.
- No dependency version changed; no `Cargo.lock` diff; no changelog fragment
  (nothing user-visible changed — the waivers' presence and effect are
  identical, only their recorded reasoning and review date moved).

## 📊 Measurement

| Check | 2026-10-26 | 2026-11-09 (before) | 2026-11-09 (after) |
| --- | --- | --- | --- |
| `cargo deny --offline check advisories` (root) | ok, zero warnings | ok, zero warnings | **ok, zero warnings** |
| `cargo deny --offline --config deny-sqlite.toml check advisories` | ok, zero warnings | ok, zero warnings | **ok, zero warnings** |
| RUSTSEC-2023-0071 `review-by` | 2026-11-02 (not yet due) | 2026-11-02 (7 days overdue) | **2026-12-05** |
| RUSTSEC-2026-0387 `review-by` | 2026-11-08 (not yet due) | 2026-11-08 (1 day overdue) | **2026-11-23** |
| Root/SQLite licenses + sources | clean | clean | clean |
| Duplicate crate versions, root (warn-level, `cargo deny check bans`) | 91 | 67 (unrelated feature-work drift) | 67 (unchanged — no version touched this pass) |
| Duplicate crate versions, sqlite graph | — | 59 | 59 |
| Open Dependabot PRs (Rust/Actions) | 9 | 1 (`#3620`, GH Action only) | 1 (unchanged — blocked by unrelated Docker test failure, not re-diagnosed this pass) |
| Unwaived advisories, all graphs | 0 | 0 | 0 |

Rehearsal (RUSTSEC-2023-0071 bump, reverted): `cargo build -p autumn-web
--no-default-features --features inbound-ses` after `rsa = { version =
"0.10", ... }` — fails at dependency resolution (`crypto-glue` / `rsa`
`sha2`-feature conflict), before any compile step; working tree confirmed
clean afterward.

Verification of the actual landed change: `cargo test -p autumn-cli --test
cli_tests -- integration::dependency_audit` (33 passed), `cargo test -p
autumn-cli --bin autumn -- new::tests::the_advisory_policy_is_the_same_for_every_flavor
new::tests::the_advisory_policy_is_generated_but_not_framework_owned` (2
passed), `./scripts/check-advisories.sh --self-test` (OK, all 4 policies).

## 🔬 Reproduce

```
cargo install --locked cargo-deny@0.20.2
cargo fetch --locked
cargo deny fetch db
cargo deny --offline check advisories
cargo deny --offline --config deny-sqlite.toml check advisories
cargo deny --offline check licenses sources
cargo deny --offline --config deny-sqlite.toml check licenses sources
cargo deny check bans
cargo deny --offline --config deny-sqlite.toml check bans
./scripts/check-advisories.sh --self-test

cargo test -p autumn-cli --test cli_tests -- integration::dependency_audit
cargo test -p autumn-cli --bin autumn -- \
  new::tests::the_advisory_policy_is_the_same_for_every_flavor \
  new::tests::the_advisory_policy_is_generated_but_not_framework_owned

# Upstream version checks (sparse index, no registry auth needed)
curl -sS https://index.crates.io/3/r/rsa | tail -1              # latest rsa
curl -sS https://index.crates.io/js/on/jsonwebtoken | tail -1   # latest jsonwebtoken
curl -sS https://index.crates.io/cr/yp/crypto-bigint | tail -1  # latest crypto-bigint
curl -sS https://index.crates.io/p2/56/p256 | tail -1           # latest p256

# Dependabot queue (via GitHub MCP / gh api)
gh api -X GET search/issues -f q='repo:autumn-foundation/autumn is:pr is:open author:app/dependabot'
gh api repos/autumn-foundation/autumn/pulls/3620
gh api "repos/autumn-foundation/autumn/commits/<sha>/check-runs?per_page=100"
```

## Follow-ups still open

1. **Cost attribution and usage/unused-feature audit on the root graph's
   heaviest hires**: still not run — **ninth pass now** flagging this as
   the oldest open item on the ledger (first flagged 2026-09-07). This
   deserves to be a dedicated pass's primary focus rather than continuing
   to lose its budget to queue triage and waiver upkeep each week.
2. **`fuzz/` and `examples/island-flock/` scheduled-batch ownership**: still
   unaddressed, unchanged across eight passes now.
3. **GitHub-native Dependabot alert count/breakdown**: still not readable
   from this session's tooling (`dependabot/alerts` returns 403 through the
   proxy even on GET) — fifth pass flagging the gap, still no number to
   report or compare against `cargo-deny`'s RustSec-backed count.
4. **The Docker-test failure blocking `#3620`**
   (`cascaded_child_commit_hook_runs_once_per_child_on_delete_many`,
   `repository_dependent_ordering.rs:887`) is unrelated to any dependency
   and outside this pass's scope to fix — flagged for whichever lane owns
   app-level test health, not re-diagnosed further here. It is the only
   thing blocking a trivial, zero-risk GitHub Actions bump.
5. **RUSTSEC-2023-0071's real unblock condition is now precise, not open-
   ended**: `webauthn-rs`/`crypto-glue` need to move their `rsa` pin past
   `^0.10.0-rc.18` to accept final `0.10.0` (or drop the now-mandatory
   `sha2` feature requirement), or `jsonwebtoken` needs to move off `rsa`
   `^0.9`. Worth checking `webauthn-rs`'s and `jsonwebtoken`'s own issue
   trackers next pass rather than re-deriving this from the sparse index
   again — a mechanical check, not a re-investigation.
