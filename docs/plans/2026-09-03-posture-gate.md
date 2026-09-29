# Plan — Gate PRs on security-posture diffs and sign the posture manifest (#1624)

Planning record for issue [#1624](https://github.com/autumn-foundation/autumn/issues/1624).
Written before implementation; kept as the design rationale for the shipped slice.

Consumes [#1604](https://github.com/autumn-foundation/autumn/issues/1604) /
[#1627](https://github.com/autumn-foundation/autumn/issues/1627) (`autumn routes
audit` — the stable-ordered security posture manifest) and composes with
[#1615](https://github.com/autumn-foundation/autumn/issues/1615) (keyless
Sigstore attestation via `actions/attest-build-provenance` +
`gh attestation verify`). It adds no second signing story.

---

## 1. Brainstorming — every option considered

**Where does the posture *diff* live?**

| # | Option | Verdict |
|---|---|---|
| B1 | A GitHub Action written in YAML + `jq` over the manifest | Rejected — the classification rules (is *adding* a role a widening or a narrowing?) are the entire product. Untestable in YAML, and impossible to unit-test the falsifiability cases the issue demands. |
| B2 | A separate npm/TypeScript action published from this repo | Rejected — a second toolchain, a second release train, and app CI would depend on a marketplace action instead of the `autumn` binary it already installs. |
| B3 | **`autumn routes posture diff` in `autumn-cli`** | **Chosen.** Pure Rust, unit-testable, and the same prebuilt binary the scaffolded CI already installs for `autumn a11y verify` / `autumn routes audit`. The YAML shrinks to "fetch two files, run one command, post its markdown". |

**What are the two sides of the diff?**

| # | Option | Verdict |
|---|---|---|
| B4 | Build the app twice in CI (base ref + head ref) | Rejected — two full `cargo build`s blows the "posture diff visible in under 2 minutes" metric on any non-trivial app, and doubles the flake surface. |
| B5 | Diff the *committed* manifest against itself across refs (no build) | Rejected alone — a PR that widens the surface and simply doesn't regenerate the manifest shows an empty diff. The gate would be trivially bypassable. |
| B6 | **Head = freshly built manifest (the `routes audit` CI step already builds it); base = the committed manifest as of the base branch (`git show`)** | **Chosen.** One build, which the app's CI already pays for. The committed manifest is the "state file" (Terraform-plan analogue from the issue's gap analysis): `git diff --exit-code` after regeneration proves the committed copy is honest, and `git show origin/<base>:<manifest>` is the previous accepted posture — a file read, not a build. |

**What is the acknowledgment marker?**

| # | Option | Verdict |
|---|---|---|
| B7 | A GitHub review approval from a CODEOWNER | Rejected on its own — approvals are dismissed (or not) by branch-protection settings the framework does not control, and an approval says "I approve the code", not "I approve *this* widening". |
| B8 | A label (`posture-ack`) | Rejected — a label is a boolean. It survives *any* subsequent push, so re-widening after acknowledgment would sail through: the exact failure the AC calls out. |
| B9 | A file committed to the repo (`posture-ack.toml`) | Rejected as the primary marker — the AC wants the acknowledgment *on the PR*, and a file in the diff is acknowledged by the same push that widens. |
| B10 | **A PR comment carrying the widening-set digest: `/ack-posture <digest>`** | **Chosen.** Content-bound: the digest is computed over the *set of widening findings*, so pushing cosmetic or unrelated commits keeps the acknowledgment valid, and any new or changed widening produces a new digest that no existing comment matches — it re-blocks, by construction. It is human, explicit, timestamped, attributed, and visible exactly where the change happened. |

**Who is allowed to acknowledge?**

- B11 Encode an authorization policy in the CLI — rejected: "org-wide policy engines (who may acknowledge what)" is explicitly out of scope, and the CLI has no GitHub identity.
- B12 **The workflow harvests acknowledgment lines only from comments whose `author_association` is `OWNER`, `MEMBER`, or `COLLABORATOR`, and passes them to the CLI via `--ack-file`.** Chosen: authorization is a GitHub fact, resolved by the thing that has GitHub's identity model; the CLI stays a pure function of its inputs and is documented as trusting them.

**How does the shipped manifest become verifiable at deploy time?**

- B13 A new signing key + `cosign sign-blob` — rejected: the issue forbids a second signing story.
- B14 **`actions/attest-build-provenance` over the committed manifest file in the scaffolded deploy workflows, verified with `gh attestation verify`; plus `autumn routes posture verify`, which recomputes the manifest's posture digest, compares it to the digest CI acknowledged, and (unless waived) shells out to `gh attestation verify` so one command covers both halves.** Chosen — reuses #1615 end to end.

**What digest?** Two, deliberately distinct:

- **posture digest** — SHA-256 over a canonical projection of the manifest's
  security-relevant content (route → classification/roles/scopes/policy, CSRF
  enforcement, emitted headers, authorization bindings). Excludes `location`
  and handler `name`, so a cosmetic refactor does not change it. This is what
  `posture verify --expect-digest` compares and what the diff report prints.
- **acknowledgment digest** — SHA-256 over the canonical serialization of the
  *widening findings only*, truncated to 16 hex chars for the comment phrase.
  Small, stable under unrelated churn, and changes the moment the widening set
  changes.

---

## 2. Reverse brainstorming — how could this ship and still be worthless?

| # | Failure mode | Mitigation in this slice |
|---|---|---|
| R1 | The gate is noisy on ordinary PRs, so teams mute it within a week. | Only *widening* findings block. Narrowing and neutral findings annotate. A PR with an empty finding set prints nothing and the workflow posts no comment at all (and deletes/updates its previous one). Header **value** changes (e.g. a CSP tweak) are reported neutral, never widening: proving a CSP got weaker is not decidable, and a false block is worse than a missed annotation here — the route/gate dimensions are where the provable signal is. |
| R2 | The diff fires on cosmetic refactors (renamed handler, moved file), destroying trust. | Routes are keyed on `(path, method)`; `name`, `location`, `source` and `module` are excluded from both the comparison and the digest. A dedicated falsifiability test asserts a cosmetic-refactor manifest produces **zero** findings. |
| R3 | Acknowledgment is a rubber stamp that survives re-widening. | The acknowledgment is bound to the widening-set digest. New widening ⇒ new digest ⇒ no matching comment ⇒ blocked again. Test: `ack` for digest A does not unblock widening set B. |
| R4 | A PR widens the surface but doesn't regenerate the committed manifest, so the diff is empty. | The head side is always freshly built by `autumn routes audit`; the workflow then fails on drift between the freshly built manifest and the committed copy, naming the command to fix it. |
| R5 | The gate blocks a PR the tool got wrong and the team cannot ship without a framework patch. | The acknowledgment *is* the escape hatch: any widening finding — right or wrong — is unblockable by one comment from an authorized reviewer, recorded on the PR. There is no "disable" switch that hides the diff. |
| R6 | The gate breaks every repo that turns it on, because there is no baseline. | No base manifest ⇒ `--allow-missing-base` ⇒ bootstrap mode: report the current posture, block nothing, exit 0. The workflow passes that flag. |
| R7 | The signed manifest proves nothing because it is signed *after* being regenerated by the deploy job. | The attested subject is the **committed** manifest — the same bytes reviewers saw and acknowledged on the PR — and `posture verify --expect-digest` ties it to the digest recorded on that PR. |
| R8 | Verification "passes" while the signature was never checked. | `posture verify` runs `gh attestation verify` by default and fails when `gh` is absent; skipping it requires the explicit `--skip-signature` flag, which prints a loud warning and is documented as offline/air-gapped use only. |
| R9 | The manifest schema evolves (#1604 is at v3) and the differ silently mis-reads a v4 document. | The differ deserializes tolerantly (unknown fields ignored, missing dimensions default to empty) but **refuses** a `schema_version` newer than it knows, with an actionable "upgrade your CLI" error, and refuses one older than v3. |
| R10 | The gate exists but autumn's own apps don't run it, so it rots. | `scripts/check-posture-gate.sh` runs the whole pipeline over a committed example-app baseline in the publish gate, and the seeded red/green fixtures run on every `cargo test`. |
| R11 | Comment-scraped acknowledgments can be forged by quoting someone else's comment. | Acknowledgment lines are parsed only at the start of a line, quoted (`>`) lines are ignored, and the workflow only harvests comments from `OWNER`/`MEMBER`/`COLLABORATOR` authors. |

---

## 3. Six hats

**⚪ White (facts).** #1604 already emits a stable-ordered, deterministic
manifest with four dimensions, and its `routes` dimension carries exactly the
fields a posture diff needs (`classification`, `roles`, `scopes`, `policy`).
Role semantics are OR (`#[secured("a","b")]` = *either* role → adding a role
widens); scope semantics are AND (`__check_secured_scopes` requires *all* →
removing a scope widens). The scaffolded app CI already installs the prebuilt
`autumn` CLI and runs `autumn routes audit`. `autumn upgrade --apply` already
reconciles framework-owned files, `.github/workflows/ci.yml` among them. #1615
already attests artifacts keylessly and documents `gh attestation verify`.

**🔴 Red (instinct).** The scary part is not the diff — it is being *wrong* in
either direction. A false block teaches the team to reach for the escape hatch
reflexively; a missed widening is an incident. The gut call: be conservative
where the manifest is provable (routes, gates, authorization bindings), and be
loudly non-blocking where it is merely declared (header values). Also: nobody
reads a 200-row table — a global CSRF flip must collapse into one line, not one
line per route.

**⚫ Black (risks).** Comment scraping is attacker-adjacent input; parse it
strictly. Truncating the ack digest to 16 hex chars invites collision worry —
64 bits over an attacker-chosen widening set is far past the threshold that
matters here, but the full digest is printed too. `git show` of the base
manifest fails on a fresh fork or shallow checkout — must degrade to bootstrap
mode, not to a hard error. Adding a scaffold file changes `autumn new`'s file
set and `autumn upgrade`'s reconciliation surface: existing tests will (and
should) fail until updated. `continue-on-error` on the attestation steps must
match #1615's existing posture so a Sigstore hiccup never blocks a deploy.

**🟡 Yellow (upside).** The whole product is three commands and one workflow
file. Every rule is a pure function over two JSON documents, so the
falsifiability suite the AC demands is cheap and fast. Because the head
manifest is the one `routes audit` already builds, the marginal CI cost is a
file read and a diff — well under the 2-minute budget. And the committed
manifest doubles as the release artifact, so signing is "attest this file",
not "rebuild the world".

**🟢 Green (creativity).** The committed-manifest-as-state-file framing is what
makes this the "terraform plan for your auth surface". Two extras fall out
almost free: the report embeds an HTML marker so the workflow *updates* its
comment instead of appending (silence stays silence), and the report prints the
exact copy-pasteable acknowledgment phrase, so the reviewer never has to
compute a digest.

**🔵 Blue (process).** Ship in TDD order: (1) red tests for the pure diff/ack/
digest/render layer plus committed red→green fixtures; (2) green implementation
and clap wiring; (3) workflow template, scaffold + `autumn upgrade` adoption,
deploy-time attestation, example-app gate; (4) refactor, docs, changelog; then
multi-angle review. Everything blocking lives in Rust; YAML stays declarative.

---

## 4. Shipped surface

```
autumn routes posture diff   --base B.json --head H.json [--format markdown|text|json]
                             [--output PATH] [--ack TOKEN]... [--ack-file PATH]
                             [--allow-missing-base]
autumn routes posture digest --manifest M.json [--format text|json]
autumn routes posture verify --manifest M.json --expect-digest D
                             [--repo OWNER/REPO] [--skip-signature]
```

Exit codes: `0` clean or acknowledged · `1` blocked (unacknowledged widening,
or failed verification) · `2` usage/IO error.

Files:

- `autumn-cli/src/posture/{mod,model,diff,ack,render,verify}.rs` — the engine.
- `autumn-cli/src/templates/.github/workflows/posture-gate.yml.tmpl` — scaffolded
  by `autumn new`, adopted by `autumn upgrade --apply`.
- `autumn-cli/tests/fixtures/posture/*.json` + `tests/integration/posture_gate.rs`
  — the seeded red/green falsifiability suite.
- `scripts/check-posture-gate.sh` + a publish-gate job — autumn's own example app.
- `docs/guide/posture-gate.md` — usage, escape hatch, deploy verification.

---

## 5. Classification rules (the product)

| Change | Severity |
|---|---|
| Route added, classification `public` or `unclassified` | **widening** |
| Route added, classification `gated`/`framework` | neutral |
| Route removed | narrowing |
| `gated`/`framework` → `public`/`unclassified` | **widening** |
| `public`/`unclassified` → `gated` | narrowing |
| Roles emptied (`#[secured("admin")]` → `#[secured]`) | **widening** |
| Role added (roles are OR) | **widening** |
| Role removed, ≥1 remaining | narrowing |
| Scope removed (scopes are AND) | **widening** |
| Scope added | narrowing |
| `policy` true → false | **widening** |
| `policy` false → true | narrowing |
| `#[authorize]` binding removed from a route | **widening** |
| `#[authorize]` binding added | narrowing |
| CSRF enforcement lost on a route (incl. via a new exempt prefix) | **widening** |
| CSRF disabled globally | **widening** (one collapsed finding) |
| CSRF enforcement gained | narrowing |
| Security header no longer emitted | **widening** |
| Security header value changed | neutral |
| Security header newly emitted | narrowing |
| Handler renamed / moved / re-moduled, nothing else | *no finding* |
