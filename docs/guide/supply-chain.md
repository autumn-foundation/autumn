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
> from*. Whether any of it is known-vulnerable is a separate question — see
> `cargo audit` and issue #1600. Runtime build-info reporting lives on
> `/actuator/info`.

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
it covers the whole resolved graph for the default feature set, including
dev-dependencies, which are resolved but never linked into the release binary.
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
| `gh attestation verify F --repo O/R` | Did `O/R`'s CI build exactly these bytes? |

---

## See also

- [Deployment](deployment.md) — the production image and deploy targets.
- [Release checklist](../release-checklist.md) — the gates a tag must pass.
- [Health indicators](health-indicators.md) — and `/actuator/info`, what a
  *running* app reports about its own build.

[sigstore]: https://www.sigstore.dev/
[gh]: https://cli.github.com/
[auditable]: https://github.com/rust-secure-code/cargo-auditable
