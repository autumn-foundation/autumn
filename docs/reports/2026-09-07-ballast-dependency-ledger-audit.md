# ⚓ Ballast: first dependency ledger audit, 2026-09-07

First Ballast pass in this repo (no prior run found: no `docs/reports/*ballast*`,
no `Ballast`-grep hit in `git log --all`, no `chore(deps)`-style commit ever
merged here). The dependency harness itself — `deny.toml`, `deny-sqlite.toml`,
`scripts/check-advisories.sh`, the `autumn doctor` `dependencies` check from
#1633 — already exists and is unusually mature, so this pass is the ledger
audit the harness enables, not a harness build. It closes with a **ledger
report**, not a PR that touches the manifest: nothing found clears the impact
floor.

## 🎯 Class

Ledger report. No single target dependency — this is the full-ledger sweep
Ballast's process document calls for before reacting to anything.

## 📈 Evidence

**Harness used** (installed `cargo-deny 0.20.2`, the exact version CI pins via
`taiki-e/install-action`, fetched from its GitHub release since this sandbox
had neither `cargo-deny` nor `cargo-audit` preinstalled):

```
$ cargo fetch --locked
$ ./scripts/check-advisories.sh          # the CI gate, run locally
$ cargo deny check licenses sources
$ cargo deny --config deny-sqlite.toml check licenses sources
$ cargo deny check bans                  # multiple-versions, not CI-gated
$ cargo deny list --format json          # license/crate census
$ cargo update --dry-run --workspace --verbose
```

**Reachability-joined advisories — 0 unwaived, 3 waived, 0 reachable.**
`./scripts/check-advisories.sh` passes clean on all three audited graphs
(workspace, SQLite backend, day-one scaffold). The three existing
`[advisories] ignore` entries in `deny.toml` were independently re-checked
against current upstream state rather than taken on faith:

| Advisory | Verdict (deny.toml's own) | Re-checked today |
| --- | --- | --- |
| RUSTSEC-2023-0071 (rsa Marvin Attack) | unreachable: RSA-JWT path only via `jsonwebtoken`, no network-reachable timing oracle | `rsa` max stable on crates.io is still **0.9.10** — no fixed release exists yet |
| RUSTSEC-2024-0384 (`instant` unmaintained) | build-time only, via `managed-pg-bundled`'s embedded-Postgres stack | `instant` max stable is still **0.1.13** (unmaintained since 2024-05), no update |
| RUSTSEC-2026-0253 (`lru::pop()` unsound) | workspace's own `lru` already fixed at 0.18.2; only the 0.16.4 copy pulled by MSRV-pinned `aws-sdk-s3 1.122` is exposed, and its one use (`S3ExpressIdentityCache`, `String` keys, `get_or_insert_mut` only) hits neither unsound precondition | `lru` max stable is now **0.18.4** (workspace copy could float higher, but that's cosmetic — the ignored copy is the transitive 0.16.4 one); `aws-sdk-s3` max stable is now **1.145.0**, still incompatible with this workspace's `rust-version = 1.88.0` floor per the existing pin comment in `autumn-storage-s3/Cargo.toml` / `autumn-media-plugin/Cargo.toml` (`>=1.122, <1.123` — "the last release compatible with this workspace's 1.88.0 MSRV") |

All three review-by dates are **2026-10-01**, 24 days out from this run —
not yet due, and nothing upstream has changed that would move any of them
early.

**Stale comment, not a ledger issue**: `deny.toml`'s advisories header claims
`yanked = "warn"` currently flags "chacha20 0.10.1, transitive." That's no
longer true — `Cargo.lock` now carries `chacha20 0.10.2`, and
`cargo deny check advisories` emits no yanked warning in this run. Leaving
this for whoever next edits that section; a comment fix alone isn't a ledger
change worth its own PR.

**Licenses — clean, one non-obvious pass worth recording.** `cargo deny check
licenses` (workspace and SQLite graphs) and `cargo deny list` agree: 18 raw
SPDX license identifiers appear somewhere in the tree, but only 17 are on
`deny.toml`'s explicit allow-list. The 18th, `LGPL-2.1-or-later`, belongs to
`r-efi` (2 duplicate versions, 5.3.0 and 6.0.0 — a `wasi`-target-only crate),
whose actual license expression is `MIT OR Apache-2.0 OR LGPL-2.1-or-later`.
`cargo deny check` evaluates the SPDX expression and is satisfied by the
`MIT`/`Apache-2.0` branch already on the allow-list — correct cargo-deny
behavior, confirmed by reading `r-efi`'s own license expression, not a policy
gap. Recorded so a future pass doesn't re-discover this as a false alarm.

**Sources — clean.** All crates resolve from `crates.io`; no git sources, no
unknown registries (`cargo deny check sources`, both graphs). Direct manifest
scan (`grep -rn 'version = "\*"'` and `git = "` across every `Cargo.toml` in
the workspace) found zero wildcard version specifiers and zero git
dependencies anywhere — this is worth stating explicitly because
`[bans] wildcards = "allow"` in `deny.toml` is a permissive *setting*, and
without checking the manifests directly that reads like an open gap rather
than the "there's nothing here to gate" it actually is.

**Duplicate versions — pervasive, already a documented, deliberate
non-gate.** `cargo deny check bans` (not CI-gated, per `deny.toml`'s own
note) finds **73 crate names** with duplicate versions in the audited graph —
dominated by the RustCrypto 0.9/0.10-era split (aes, digest, ecdsa,
elliptic-curve, hmac, p256, sec1, signature, spki, …) and the
`windows-sys`/`windows-targets` per-target-triple shim family. This matches
`deny.toml`'s existing rationale verbatim ("pervasive and cosmetic... RustCrypto
old/new, windows-sys target shims"). No single collapse here clears a ≥3-node
subtree without also picking a side in an upstream RustCrypto major-version
split across half a dozen unrelated dependency chains — that's a forcing-fact
question for a specific `Upgrade`-class PR later, not something a ledger sweep
should force today.

**Graph facts** (deny.toml's own audited feature graph, via
`cargo deny list --format json`):

- 743 unique crate@version nodes, 665 unique crate names
- 131 direct (non-workspace) dependencies declared across the 28 workspace
  member crates (whole-workspace `cargo metadata` resolve; a slightly wider
  scope than `deny.toml`'s curated feature list, cross-checked here rather
  than taken as identical)
- 73 crate names carrying duplicate versions (above)

**Scheduled-batch check.** `cargo update --dry-run --workspace --verbose`
locks **0 packages** — every dependency already sits at the newest version
its current manifest constraint permits. 121 dependencies show as "behind
latest" in the verbose diff, but every one sampled is blocked by either an
upstream pre-release pin outside our control (e.g. `aead 0.6.0-rc.10`, held
by `rsa`'s own manifest, not ours) or this workspace's `rust-version = 1.88.0`
floor (e.g. `aes 0.9.3 requires Rust 1.89`). A scheduled-batch PR opened today
would carry an **empty lockfile diff** — there is nothing to batch yet.

**Pain ledger.** No prior Ballast run and no prior dependency-only commit
exist anywhere in this repo's history (`git log --all --grep=Ballast -i`,
`git log --all --grep='chore(deps)'` both empty). The only pain-ledger record
today is `deny.toml`'s own inline comments, which already cite their forcing
history per ignored advisory.

## 💡 Mechanism / forcing fact

None found. Every one of Ballast's floor conditions was checked and none
holds today: 0 reachable advisories to close (all 3 known ones are
unreachable/no-fix and independently re-confirmed as such), no ≥3-node
duplicate subtree collapsible without picking a side in an unrelated
RustCrypto-major fight, no unused dependency or feature identified in this
pass's scope, no measured ≥10% build-time or size win (cost attribution
wasn't run this pass — see Follow-ups), no scheduled batch available (0
packages movable), no supply-chain fact to fix (no wildcards, no git refs,
sources/licenses both clean). Per the hard gate, that means: no PR, because
manufacturing one now would be "churn with a lockfile diff" against
Ballast's own banned-changes list, not hygiene.

## 🔧 Change

None. This report is the deliverable.

## 📊 Measurement

Baseline recorded for the next pass to diff against:

| Metric | Value |
| --- | --- |
| Workspace members | 28 |
| Direct (non-workspace) deps | 131 |
| Unique crate@version nodes (deny.toml graph) | 743 |
| Unique crate names (deny.toml graph) | 665 |
| Crate names with duplicate versions | 73 |
| Advisories matched to graph | 3 (0 reachable, 3 unreachable/no-fix, 0 undetermined) |
| License identifiers in tree / on allow-list | 18 / 17 explicit (+1 satisfied via OR-expression) |
| Wildcard version specifiers | 0 |
| Unpinned git dependencies | 0 |
| Packages `cargo update` would move today | 0 |

Not measured this pass (scope call, not an oversight — recorded so the next
Ballast run doesn't have to rediscover the gap):

- Build-time / binary-size cost attribution per dependency
- Usage audit (which surface of each heavy dependency is actually called)
- Unused-default-feature audit

## 🔬 Reproduce

```
cargo fetch --locked
./scripts/check-advisories.sh                       # CI's own gate
cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources
cargo deny check bans                                # duplicate-version census
cargo deny list --format json                        # license census
cargo update --dry-run --workspace --verbose         # scheduled-batch check
grep -rn 'version = "\*"' --include=Cargo.toml .     # wildcard scan
grep -rn 'git = "' --include=Cargo.toml .            # unpinned git-ref scan
```

## Follow-ups for the next pass

1. Revisit all three `deny.toml` ignores on/after **2026-10-01** (their
   shared review-by date) — re-check `rsa`, `instant`, and
   `aws-sdk-s3`/`lru` upstream status then.
2. `deny.toml`'s advisories header comment names a stale yanked version
   (`chacha20 0.10.1`); the tree is on `0.10.2` now. Worth a one-line fix
   whenever that section is next touched.
3. Run cost attribution (`cargo build --timings`) and a usage audit on the
   heaviest hires once a baseline harness for those exists — this pass only
   covered the advisory/license/source/duplicate/pin sweep.
