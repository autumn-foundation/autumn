# Verify What You're Running

Every Autumn release artifact — and every production image the scaffold builds
— ships with two things an auditor can check without trusting anyone's word:

- a **CycloneDX SBOM**: the exact list of crates compiled into the artifact.
- a **build-provenance attestation**: a keyless [Sigstore][sigstore] signature
  binding that artifact's digest to the repository, commit, and CI run that
  produced it.

Nothing here needs third-party supply-chain tooling, a signing key, or a
subscription. This page is meant to be followed verbatim; every command is
runnable as written.

> **Scope.** This is about *what is inside* an artifact and *where it came
> from*. Whether any of it is **known-vulnerable** is the advisory gate, which
> your scaffolded app runs on every push — jump to
> [Part 3a](#part-3a--the-advisory-gate-known-vulnerable-dependencies). Runtime
> build-info reporting lives on `/actuator/info`.

---

## What you need

| Tool | Why | Install |
|---|---|---|
| [`gh`][gh] ≥ 2.49 | `gh attestation verify` | `brew install gh` / [other platforms][gh] |
| `autumn` | reading SBOMs out of binaries | `cargo install autumn-cli` |
| `docker` | only for the container image sections | — |

`gh attestation verify` needs to be authenticated (`gh auth login`) so it can
read the attestation from GitHub's transparency log.

---

## Part 1 — verifying an Autumn release

Every asset attached to an Autumn GitHub Release carries a provenance
attestation: the CLI archives, their `.sha256` files, and the release SBOM.

### 1.1 Download an asset

SBOM assets and attestations exist from the first release that ships this
feature onward, so take the tag from the release list rather than hardcoding
one:

```bash
TAG=$(gh release list --repo autumn-foundation/autumn --limit 1 \
        --json tagName --jq '.[0].tagName')
gh release download "$TAG" \
  --repo autumn-foundation/autumn \
  --pattern 'autumn-x86_64-unknown-linux-musl.tar.gz*'
```

### 1.2 Verify its provenance — the one command

```bash
gh attestation verify autumn-x86_64-unknown-linux-musl.tar.gz \
  --repo autumn-foundation/autumn
```

A genuine asset prints a success line naming the workflow that built it:

```
Loaded digest sha256:… for file://autumn-x86_64-unknown-linux-musl.tar.gz
Loaded 1 attestation from GitHub API
✓ Verification succeeded!

sha256:… was attested by:
REPO                       PREDICATE_TYPE                  WORKFLOW
autumn-foundation/autumn   https://slsa.dev/provenance/v1  .github/workflows/release.yml@refs/heads/trunk
```

> **What the source ref means.** The SBOM asset is attested by the Publish
> Gate, which the tag push triggers directly, so its provenance records the tag
> and the tagged commit. The CLI archives are attested by `cli-release.yml`,
> which runs as a child of the `workflow_run`-triggered release workflow — and
> GitHub always runs those in default-branch context, so their recorded ref and
> commit name the default branch even though the build checked the tag out.
> For those assets the subject digest, the repository and the CI run are exact;
> the ref is not. Verify with `--repo`, and do not add `--source-ref
> refs/tags/...` for an archive, which would fail against a genuine asset.

`--repo autumn-foundation/autumn` is **not optional**. Without it, an
attestation from *any* repository would satisfy the check — including one an
attacker published for their own build of their own bytes.

### 1.3 Prove the check is real: tamper with the file

Verification is only worth running if you have seen it fail. Change a single
byte and try again:

```bash
cp autumn-x86_64-unknown-linux-musl.tar.gz tampered.tar.gz
printf '\x00' >> tampered.tar.gz

gh attestation verify tampered.tar.gz --repo autumn-foundation/autumn
# ✗ Verification failed: no attestations found for subject sha256:…
```

The attestation is bound to the artifact's **digest**, so any modification — an
extra byte, a repacked tarball with identical contents, a substituted binary —
produces a different digest and finds no attestation.

What this does *not* catch, by design: re-uploading a **genuine, unmodified**
asset somewhere else. The claim is about the bytes and who built them, not about
the filename or which release they hang off. If you need to pin an asset to the
workflow that produced it as well, add:

```bash
gh attestation verify autumn-x86_64-unknown-linux-musl.tar.gz \
  --repo autumn-foundation/autumn \
  --signer-workflow autumn-foundation/autumn/.github/workflows/cli-release.yml
```

### 1.4 Read the release SBOM

Every tagged release attaches `autumn-<tag>.cdx.json`, a CycloneDX 1.5 document
listing every crate in the released workspace:

```bash
gh release download "$TAG" --repo autumn-foundation/autumn \
  --pattern "autumn-${TAG}.cdx.json"

gh attestation verify "autumn-${TAG}.cdx.json" --repo autumn-foundation/autumn

# Which version of `serde` did this release resolve?
jq -r '.components[] | select(.name == "serde") | "\(.name) \(.version)"' \
  "autumn-${TAG}.cdx.json"
```

The SBOM is **deterministic**: it carries no random serial number and no build
timestamp, and its components are sorted. Regenerating it from the tagged
source produces byte-identical output — which is exactly what the release gate
checks (below), so a hand-edited SBOM cannot be published.

---

## Part 2 — verifying a scaffolded app

`autumn release init` generates a Dockerfile that makes all of this the
default. There is nothing to enable.

### 2.1 What's in the image

The image carries its SBOM at a fixed path, advertised as a label so a scanner
can find it knowing nothing about Autumn:

```bash
docker inspect --format '{{ index .Config.Labels "io.autumn.sbom.path" }}' my-app:latest
# /usr/share/autumn/sbom.cdx.json

docker run --rm --entrypoint cat my-app:latest /usr/share/autumn/sbom.cdx.json > image-sbom.json
jq '.components | length' image-sbom.json
```

### 2.2 What's in the *binary* — no source tree, no lockfile

The SBOM file above sits *beside* the binary; a file beside a binary can be
swapped. The binary itself independently carries the same list, because the
generated Dockerfile compiles it through [`cargo-auditable`][auditable]
(`RUSTC_WORKSPACE_WRAPPER=cargo-auditable`), which embeds the resolved
dependency graph into a `.dep-v0` section of the executable.

Read it back with the `autumn` CLI the image already ships:

```bash
docker run --rm --entrypoint autumn my-app:latest \
  sbom --binary /usr/local/bin/my-app
```

This works on any copy of the binary, anywhere — no source tree, no
`Cargo.lock`, no network:

```bash
docker cp "$(docker create my-app:latest)":/usr/local/bin/my-app /tmp/my-app
autumn sbom --binary /tmp/my-app | jq -r '.components[] | "\(.name) \(.version)"' | head
```

If the binary was not built through `cargo-auditable`, the command says so and
names the fix rather than reporting an empty list.

A macOS **universal** binary is refused rather than guessed at: its
architecture slices can carry genuinely different dependency lists (a
`cfg(target_arch)` crate is in one and not another), so describing the whole
file by one of them would omit crates that are really in it. Split it first:

```bash
lipo -thin arm64 my-app -output my-app.arm64
autumn sbom --binary my-app.arm64
```

(Slices that agree are not a disagreement — a universal binary whose slices
carry identical lists is read normally.)

> **If the `autumn` inside the image is older than `sbom`.** The image installs
> the CLI at the version its Dockerfile pins, so an image scaffolded before this
> feature shipped carries a CLI without the subcommand. The binary is still
> auditable — read it with a newer `autumn` on the host (as in the second
> snippet above), or with `cargo audit bin`, which reads the same `.dep-v0`
> section. Regenerating the release files (`autumn release init --force`) with a
> current CLI updates the pin.

### 2.3 Cross-check the two

The sidecar SBOM and the embedded list are produced by different mechanisms
from the same build. They should agree on every runtime crate:

```bash
autumn sbom --binary /tmp/my-app \
  | jq -r '.components[] | select((.properties // []) | any(.name == "cargo:dependency-kind") | not)
           | "\(.name)@\(.version)"' | sort > from-binary.txt

jq -r '.components[] | "\(.name)@\(.version)"' image-sbom.json | sort > from-image.txt

comm -3 from-binary.txt from-image.txt
```

Components that have no crates.io identity — a path dependency, a workspace
member, a `[patch]`ed git checkout — deliberately carry **no** `purl`, and a
`bom-ref` that says what they are (`path:my-app@1.0.0`). A bare
`pkg:cargo/<name>@<version>` asserts a registry package, and emitting one for
an unpublished crate points a scanner at somebody else's project. Git and
alternate-registry dependencies get a `?vcs_url=` / `?repository_url=`
qualifier naming where they actually came from.

The sidecar SBOM is generated from `cargo metadata` and is therefore *broader*:
it covers the whole resolved graph for the image's own target triple and
feature set, including dev-dependencies, which are resolved but never linked
into the release binary.

(The image's SBOM is narrowed with `--filter-platform` to the target that built
it — without that, it would list every platform's target-specific dependencies,
the whole `windows-*` family included, none of which can be in a Linux image.
Autumn's **own** release SBOM is deliberately *not* filtered: that is a source
release consumed on every platform, so narrowing it to whichever runner built
it would understate what was published.)
(`--all-features` widens it further, to crates no single build can contain — it
is available deliberately, and deliberately not the default.) The embedded list
is what actually went into the binary. Entries appearing only in
`from-image.txt` are expected; an entry appearing only in `from-binary.txt` is
not, and is worth investigating.

### 2.4 Verify the image's provenance

The scaffolded deploy workflows (`--target aws-ecs`, `gcp-cloud-run`,
`azure-container-apps`) attest every image they push, plus the SBOM baked into
it. Verify by digest:

```bash
IMAGE=registry.example.com/my-app
DIGEST=$(docker buildx imagetools inspect "$IMAGE:v1.2.3" --format '{{ .Manifest.Digest }}')

gh attestation verify "oci://$IMAGE@$DIGEST" --repo my-org/my-app

# And the SBOM attestation for the same image:
gh attestation verify "oci://$IMAGE@$DIGEST" \
  --repo my-org/my-app \
  --predicate-type https://cyclonedx.org/bom
```

(The predicate type follows the SBOM's format. `actions/attest-sbom` maps
CycloneDX to `https://cyclonedx.org/bom`; an SPDX document would instead be
`https://spdx.dev/Document/v2.3`.)

Attestations are bound to the image **digest**, never its tag: a tag can be
re-pointed at different bytes later, a digest cannot. Verifying `:v1.2.3` by
tag would tell you about whatever that tag points at *today*.

Deploying with a plain `docker build` — the `fly`, `docker-compose`,
`aws-app-runner` and default targets, none of which scaffold a CI workflow —
still gets you the in-image SBOM and the auditable binary — the
attestation step needs CI, because keyless signing needs a CI identity to sign.
To add it, copy the `Resolve the pushed image digest` /
`Attest build provenance` / `Attest the image SBOM` steps out of one of the
generated deploy workflows into your own, along with the `id-token: write` and
`attestations: write` permissions.

---

## Part 3 — the gates behind all this

You do not have to take the framework's word for any of it either.

**The SBOM matches the tagged source.** `scripts/check-sbom.sh` runs in the
Publish Gate on every tag. It builds the generator from the checkout being
released, generates the SBOM, then regenerates it and compares
component-by-component — which proves the generator is deterministic, the
property everything else here rests on — and requires the root component's
version to equal both `[workspace.package].version` and the pushed tag.

The same `--verify` then runs a **second** time in `prepare-release`, against
the artifact after it has travelled through the artifact store. That is the run
that can catch a substituted or truncated document, and it is the file that run
checks which becomes the release asset. Either way the failure names the
components that drifted:

```console
$ autumn sbom --verify sbom.cdx.json
Error: SBOM does not match the source tree:
  unexpected component: backdoor@6.6.6
  missing component: serde@1.0.228
```

Run the same gate yourself against any checkout:

```bash
./scripts/check-sbom.sh                     # against the working tree
RELEASE_TAG=v0.7.0 ./scripts/check-sbom.sh  # also enforce tag agreement
```

**Downloads during an image build are checksum-verified.** Every artifact the
generated build pulls in, and exactly what each check buys you:

| Download | Integrity |
|---|---|
| `cargo install` of `cargo-chef`, `autumn-cli`, `diesel_cli`, `cargo-auditable` | crates.io checksums, each `--locked` and pinned to an exact `--version` |
| `apt-get install …` | apt repository signatures |
| Tailwind CLI, via `autumn setup` | SHA-256 against the `sha256sums.txt` published with that Tailwind release; refuses to install on a mismatch |
| Base images (`rust:…`, `debian:bookworm-slim`) | tag-pinned, **not** digest-pinned |

There is no unverified `curl` of an executable anywhere in the generated build,
and an integration test enforces that. Two honest caveats: the Tailwind check is
trust-on-first-use against the publisher — it detects a corrupted or truncated
download, not a compromised upstream release that re-published both the binary
and its checksum file — and the base images are pinned by tag, so pin them by
digest yourself if your threat model requires it.

---

## Part 3a — the advisory gate: known-vulnerable dependencies

An SBOM tells you *what* you are running. The advisory gate tells you whether
any of it is known to be vulnerable — and, unlike a dashboard of alerts, it
blocks the build.

### What it checks

Every app `autumn new` generates ships two halves of one gate:

| File | Role |
|---|---|
| `.github/workflows/ci.yml` | Runs `cargo deny check advisories` on every push and pull request. |
| `deny.toml` | The policy it reads: what counts as a failure, and which advisories you have explicitly accepted. |

The check resolves your **whole** dependency graph — transitive crates
included, not just what your `Cargo.toml` names — and matches it against the
[RustSec advisory database][rustsec]. A crate with a known advisory that
`deny.toml` does not waive fails the job. Vulnerabilities always fail;
`unmaintained` and `unsound` advisories fail too (narrow those in `deny.toml`
if you would rather triage them separately), and a yanked crate warns.

Run exactly what CI runs, locally:

```bash
cargo install --locked cargo-deny   # once
cargo deny check advisories
```

### Reading a failure

```console
error[vulnerability]: Marvin Attack: potential key recovery through timing sidechannels
    ┌─ /home/you/my-app/Cargo.lock:322:1
    │
322 │ rsa 0.9.10 registry+https://github.com/rust-lang/crates.io-index
    │ ────────────────────────────────────────────────────────────────
    │
    ├ ID: RUSTSEC-2023-0071
    ├ Advisory: https://rustsec.org/advisories/RUSTSEC-2023-0071
    ├ Solution: No safe upgrade is available!
    ├ rsa v0.9.10
      └── jsonwebtoken v10.1.0
          └── autumn-web v0.7.0
              └── my-app v0.1.0

advisories FAILED
```

Four things to read, in order:

1. **The id** (`RUSTSEC-2023-0071`) — the advisory's permanent name; the
   `Advisory:` URL has the full write-up and the real-world exploitability.
2. **The crate and version** (`rsa 0.9.10`) — what is actually vulnerable.
3. **`Solution:`** — the fixed version, or "No safe upgrade is available!"
   when upstream has not shipped one.
4. **The dependency path**, read bottom-up — `my-app` → `autumn-web` →
   `jsonwebtoken` → `rsa`. The crate to bump is rarely the vulnerable one; it
   is the highest entry in that chain you control.

Fix it before you waive it: `cargo update -p <crate>` when a patched version is
already compatible, otherwise bump the direct dependency that pulls it in. In an
Autumn app that dependency is usually `autumn-web` itself — most of the tree
arrives through it — so check whether a newer autumn-web release resolves a
fixed version, and if none does yet, [open an
issue](https://github.com/autumn-foundation/autumn/issues) and waive it locally
with a review-by date in the meantime. (The example above is the one advisory
you will not see fail: it ships pre-waived. Every other finding reads the same
way.)

### Waiving an advisory

When there is no fix — or the vulnerable code path is unreachable from your app
— acknowledge the advisory in `deny.toml` instead of weakening the gate:

```toml
[advisories]
ignore = [
    # Why this is acceptable *here*, and when you will look again.
    { id = "RUSTSEC-2023-0071", reason = "no fixed rsa release exists; reaches this app only through jsonwebtoken's RSA-family JWT path; review-by 2026-10-01" },
]
```

A waiver lets exactly one id through. Every other advisory — including a new
one in the same crate — still fails, and the gate itself stays on. The id and
the rationale are committed to your repository, so the decision is reviewable
in a pull request rather than living in someone's memory.

Your generated `deny.toml` ships with a waiver already in it:
`RUSTSEC-2023-0071`. `rsa` reaches every Autumn app through `jsonwebtoken`,
which `autumn-web` depends on unconditionally, and no patched `rsa` release
exists. An app generated with `--bundled-pg` gets a second one
(`RUSTSEC-2024-0384`, `instant`, unmaintained, reachable only through the
embedded-Postgres build stack) — and only that flavor gets it, so cargo-deny's
"this waiver is unused" warning stays available to tell you when one of *your*
waivers has gone stale.

Those waivers are why a freshly scaffolded app's CI is green on day one instead
of red on the first push. Autumn's own CI re-audits autumn-web's dependency tree
against that exact policy on every run, so the shipped waiver set cannot quietly
stop covering what the scaffold ships.

Never disable the gate to get green. `continue-on-error: true`, `|| true`, and
deleting the step all turn a security control into a decoration. Autumn's own
test suite fails if the *generated* workflow does any of them, and in your
project an edited `ci.yml` comes back as a conflict on every
[`autumn upgrade`](upgrading.md#scaffold-files) — the cost of the workaround
keeps being paid. Fix the advisory, or waive it.

### When the advisory database is unreachable

The gate **fails closed**. Fetching the RustSec database is the one part that
needs the network, so the generated workflow does it in its own step: three
attempts, backing off 10s then 20s between them. If the database is still
unreachable the job fails with an explicit message rather than hanging or
silently skipping the audit — a dependency tree nobody could verify is never
reported as clean. The audit itself then runs `--offline` against the database
just fetched, so a failure in *that* step is always a real advisory and never a
network blip.

Two related failures are called out rather than mistaken for an outage:
cargo-deny loads `deny.toml` *before* it touches the network, so a malformed
policy is reported as the configuration error it is; and a missing `deny.toml`
stops the step with a message naming the file, instead of auditing your app
under cargo-deny's built-in default policy, which waives nothing.

### The framework's own gate

Autumn holds itself to the same rule. `scripts/check-advisories.sh` runs in
both PR CI and the **Publish Gate**, so a release cannot be tagged while an
unwaived advisory sits in the tree being published:

```bash
./scripts/check-advisories.sh              # workspace, sqlite graph, scaffold graph
./scripts/check-advisories.sh --self-test  # prove the gate still rejects a CVE
```

The third graph is the interesting one: it audits `autumn-web`'s dependency
tree against the `deny.toml` that `autumn new` writes, with every feature any
scaffold flavor can enable turned on — so "your day-one CI is green" is a
checked property of every release rather than a hope. Being precise about what
that covers: it is the autumn-web half of your tree, audited generously (a
superset of what your app compiles from autumn-web), resolved against Autumn's
own lockfile. Your app's own direct dependencies, and the exact versions your
lockfile resolves, are what *your* CI audits — which is why the gate ships with
your app rather than only living here.

`--self-test` is the negative proof, and it runs in Autumn's CI on every pull
request: it audits a throwaway crate carrying a deliberately injected
known-vulnerable dependency (`time 0.1.45`, RUSTSEC-2020-0071) and requires
both policies — Autumn's own and the one shipped into your app — to reject it,
then to accept it once, and only once, that id is waived. A gate nobody has
watched fail is indistinguishable from a gate that no longer runs.

---

## Command reference

| Command | Answers |
|---|---|
| `autumn sbom` | What does this source tree resolve to? (CycloneDX, to stdout) |
| `autumn sbom --output FILE` | …written to a file. |
| `autumn sbom --verify FILE` | Does `FILE` still describe this source tree? |
| `autumn sbom --expect-version V` | …and does it describe version `V`? |
| `autumn sbom --locked` | …with a `Cargo.lock` that matches the manifests. |
| `autumn sbom --all-features` | …with every optional feature on (broader than any single build). |
| `autumn sbom --binary FILE` | What is compiled into this binary? (no source tree) |
| `autumn sbom --features F` | …resolving the features the build used. |
| `autumn sbom --filter-platform T` | …restricted to one target triple. |
| `cargo deny check advisories` | Is anything in this tree known-vulnerable? (reads `deny.toml`) |
| `./scripts/check-advisories.sh` | …for Autumn's own graphs, and for the app scaffold's. |
| `./scripts/check-advisories.sh --self-test` | Can that gate still reject a known CVE? |
| `gh attestation verify F --repo O/R` | Did `O/R`'s CI build exactly these bytes? |

---

## See also

- [Deployment](deployment.md) — the production image and deploy targets.
- [Release checklist](../release-checklist.md) — the gates a tag must pass.
- [Health indicators](health-indicators.md) — and `/actuator/info`, what a
  *running* app reports about its own build.

[rustsec]: https://rustsec.org/
[sigstore]: https://www.sigstore.dev/
[gh]: https://cli.github.com/
[auditable]: https://github.com/rust-secure-code/cargo-auditable
