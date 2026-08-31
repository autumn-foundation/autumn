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

```bash
TAG=v0.7.0
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
autumn-foundation/autumn   https://slsa.dev/provenance/v1  .github/workflows/cli-release.yml@refs/tags/v0.7.0
```

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

The attestation is bound to the artifact's **digest**, so any modification —
a re-uploaded asset, an extra byte, a repacked tarball with identical contents
— produces a different digest and finds no attestation. Re-uploading a genuine
asset under a different name still verifies (the bytes are unchanged), which is
correct: the claim is about the bytes, not the filename.

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
docker cp "$(docker create my-app:latest)":/usr/local/bin/my-app ./my-app
cd /tmp && autumn sbom --binary ./my-app | jq -r \
  '.components[] | "\(.name) \(.version)"' | head
```

If the binary was not built through `cargo-auditable`, the command says so and
names the fix rather than reporting an empty list.

### 2.3 Cross-check the two

The sidecar SBOM and the embedded list are produced by different mechanisms
from the same build. They should agree on every runtime crate:

```bash
autumn sbom --binary ./my-app \
  | jq -r '.components[] | select((.properties // []) | any(.name == "cargo:dependency-kind") | not)
           | "\(.name)@\(.version)"' | sort > from-binary.txt

jq -r '.components[] | "\(.name)@\(.version)"' image-sbom.json | sort > from-image.txt

comm -3 from-binary.txt from-image.txt
```

The sidecar SBOM is generated from `cargo metadata` and is therefore *broader*:
it includes dev-dependencies and every optional feature's crates, which are
resolved but not linked. The embedded list is what actually went into the
binary. Entries appearing only in `from-image.txt` are expected; an entry
appearing only in `from-binary.txt` is not, and is worth investigating.

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
  --predicate-type https://spdx.dev/Document
```

Attestations are bound to the image **digest**, never its tag: a tag can be
re-pointed at different bytes later, a digest cannot. Verifying `:v1.2.3` by
tag would tell you about whatever that tag points at *today*.

Deploying with a plain `docker build` (the `fly`, `docker-compose`, and default
targets) still gets you the in-image SBOM and the auditable binary — the
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
released, generates the SBOM, then *regenerates it and compares
component-by-component*, and requires the root component's version to equal
both `[workspace.package].version` and the pushed tag. A stale, substituted, or
hand-edited SBOM fails, and the failure names the components that drifted:

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

**Downloads during an image build are checksum-verified.** The generated
Dockerfile obtains the Tailwind binary through `autumn setup`, which verifies
its SHA-256 against the `sha256sums.txt` published with that Tailwind release
and refuses to install on a mismatch. There is no unverified `curl` of an
executable anywhere in the generated build; an integration test enforces that.

---

## Command reference

| Command | Answers |
|---|---|
| `autumn sbom` | What does this source tree resolve to? (CycloneDX, to stdout) |
| `autumn sbom --output FILE` | …written to a file. |
| `autumn sbom --verify FILE` | Does `FILE` still describe this source tree? |
| `autumn sbom --expect-version V` | …and does it describe version `V`? |
| `autumn sbom --locked` | …with a `Cargo.lock` that matches the manifests. |
| `autumn sbom --binary FILE` | What is compiled into this binary? (no source tree) |
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
