#!/usr/bin/env bash
# Build-and-boot gate for the generated `autumn release init` image (issue #978).
#
# `autumn release init` scaffolds a production Dockerfile and docs/guide/deployment.md
# makes a falsifiable promise: "from a fresh `autumn new` project to a
# production-shaped container running... under 10 minutes", with `/health` wired
# as the container HEALTHCHECK. The existing tests only string-assert Dockerfile
# *contents*; nothing ever runs `docker build` on the generated image and boots
# it. This harness closes that loop: it scaffolds a fresh project, runs
# `autumn release init --force`, builds the generated image, runs the documented
# one-shot `autumn migrate` job against a throwaway Postgres, boots the web
# container, and asserts GET /health and /actuator/health both return 200 within
# a bounded startup window. It also covers the `--target docker-compose` path,
# bringing the stack up and tearing it down cleanly.
#
# The `https` target extends the same loop to direct in-process TLS (issue
# #1603): the app terminates HTTPS itself from `[server.tls]`, with no reverse
# proxy in the picture, and the gate proves the image boots that way and answers
# an HTTPS health check with a real (test) certificate.
#
# Usage:
#   scripts/check-release-image-boot.sh [default|docker-compose|https]
#
# Environment:
#   AUTUMN_BIN             Path to a prebuilt `autumn` binary. When unset, the
#                         script builds `autumn-cli` from the current checkout so
#                         the gate verifies the scaffold produced by *this* tree.
#   PG_HOST / PG_PORT     Postgres host/port for the bare `default` target's
#                         one-shot migrate + boot (default: localhost / 5432).
#                         In CI this is a service container mapped to localhost.
#   PG_USER / PG_PASSWORD Postgres credentials (default: autumn / autumn).
#   STARTUP_BUDGET_SECS   Health-probe deadline after boot (default: 30) — the
#                         documented "≤ 30s after boot" window.
#   IMAGE_SIZE_BUDGET_MB  Runtime image size budget for the secondary guard
#                         (default: 150). Reported informationally; exceeding it
#                         warns rather than fails (optional per the spec).
#   HEALTHY_BUDGET_SECS   Deadline for the container's own Docker HEALTHCHECK to
#                         report `healthy` in the `https` target (default: 120).
#                         The generated HEALTHCHECK has a 10s start period and a
#                         30s interval, so this needs headroom over the probe
#                         budget above.
set -euo pipefail

TARGET="${1:-default}"
STARTUP_BUDGET_SECS="${STARTUP_BUDGET_SECS:-30}"
IMAGE_SIZE_BUDGET_MB="${IMAGE_SIZE_BUDGET_MB:-150}"
HEALTHY_BUDGET_SECS="${HEALTHY_BUDGET_SECS:-120}"
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-autumn}"
PG_PASSWORD="${PG_PASSWORD:-autumn}"

PROJECT_NAME="releasecheck"
# Per-target names: the `default` and `https` targets both build an image and
# boot a container, so sharing one tag/name would let either target's `docker
# rm -f`/rebuild clobber the other's on a shared (self-hosted) daemon.
IMAGE_TAG="autumn-release-image-boot-${TARGET}:ci"
CONTAINER_NAME="autumn-release-image-boot-${TARGET}"

# ── repo + workspace setup ──────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

log()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[fail]\033[0m %s\n' "$*" >&2; }

# Resolve the `autumn` binary. Build from the current checkout when AUTUMN_BIN is
# unset so the gate exercises the scaffold emitted by the code under test.
if [[ -n "${AUTUMN_BIN:-}" ]]; then
  AUTUMN="${AUTUMN_BIN}"
else
  log "Building autumn-cli from the current checkout"
  cargo build -p autumn-cli --bin autumn --manifest-path "${REPO_ROOT}/Cargo.toml"
  AUTUMN="${REPO_ROOT}/target/debug/autumn"
fi
log "Using autumn binary: ${AUTUMN}"
"${AUTUMN}" --version || true

WORKDIR="$(mktemp -d)"
PROJECT_DIR="${WORKDIR}/${PROJECT_NAME}"

# Probe response captured by the most recent failed health check, surfaced in
# the failure summary so the breakage is diagnosable from the CI log alone.
LAST_PROBE_RESPONSE=""

# Extra curl options every `probe_until_healthy` call passes through. The
# `https` target sets this to `--cacert <test CA>`; every other target leaves it
# empty, so their probes are byte-for-byte the plain-HTTP ones they were.
PROBE_CURL_OPTS=()

# Directory holding the `https` target's test certificate (set by
# generate_test_certificate; lives under WORKDIR, so cleanup removes it).
TLS_DIR=""

DB_URL="postgres://${PG_USER}:${PG_PASSWORD}@${PG_HOST}:${PG_PORT}/${PROJECT_NAME}_prod"
SIGNING_SECRET="$(openssl rand -hex 32 2>/dev/null || echo "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")"

# ── cleanup ─────────────────────────────────────────────────────────────────
cleanup() {
  set +e
  if [[ "${TARGET}" == "docker-compose" && -d "${PROJECT_DIR}" ]]; then
    log "Tearing down docker-compose stack"
    ( cd "${PROJECT_DIR}" && docker compose down -v --remove-orphans >/dev/null 2>&1 )
  fi
  # Only remove the named container in the targets that start it (`default`
  # and `https`); the docker-compose target never starts it, and removing it
  # unconditionally would kill a sibling job's container on a shared runner.
  if [[ "${TARGET}" == "default" || "${TARGET}" == "https" ]]; then
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  fi
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT

# ── scaffold ────────────────────────────────────────────────────────────────
log "Scaffolding a fresh project with \`autumn new\`"
( cd "${WORKDIR}" && "${AUTUMN}" new "${PROJECT_NAME}" )

# ── vendor the in-tree autumn-web into the build context ─────────────────────
# The scaffold depends on `autumn-web` from crates.io. On trunk-dev the templates
# use framework APIs (e.g. `Flash::render`, `FLASH_CSS_PATH`) that are ahead of
# the last published autumn-web release, so a real `docker build` against
# crates.io can't compile the generated project until the next release ships.
#
# Mirror what the Generator Conformance gates already do for plain `cargo` builds
# (see autumn-cli/tests/generate.rs::patch_generated_cargo_toml): vendor the
# in-tree autumn-web — and its proc-macro path dependency autumn-macros — into
# the Docker build context and `[patch.crates-io]` the scaffold at it. The gate
# then exercises *this* tree's framework through the real Docker plumbing
# (cargo-chef, libpq, the runtime image) instead of a stale published crate.
vendor_in_tree_autumn_web() {
  log "Vendoring in-tree autumn-web into the build context"
  local vendor_dir="${PROJECT_DIR}/vendor"
  mkdir -p "${vendor_dir}"

  # autumn (= autumn-web) and its path-dep autumn-macros are the only crates the
  # scaffold's framework dependency needs. autumn-cli is vendored so it can be
  # installed from source (see inject_local_autumn_binary), and autumn-schema-core
  # is vendored because autumn-cli now carries a `path = "../autumn-schema-core"`
  # dependency on it — without the crate on disk `cargo install --path
  # ./vendor/autumn-cli` fails to read `vendor/autumn-schema-core/Cargo.toml`.
  # autumn-edge is vendored for the same manifest-resolution reason: autumn-web's
  # `edge` feature declares `autumn-edge = { path = "../autumn-edge", optional =
  # true }`, and cargo reads every path dependency's manifest even when the
  # feature is off. The shared target/ lives at the workspace root, so these
  # crate dirs are source-only and cheap to copy.
  cp -R "${REPO_ROOT}/autumn" "${vendor_dir}/autumn"
  cp -R "${REPO_ROOT}/autumn-macros" "${vendor_dir}/autumn-macros"
  cp -R "${REPO_ROOT}/autumn-cli" "${vendor_dir}/autumn-cli"
  cp -R "${REPO_ROOT}/autumn-schema-core" "${vendor_dir}/autumn-schema-core"
  cp -R "${REPO_ROOT}/autumn-edge" "${vendor_dir}/autumn-edge"
  # Drop any stray build artifacts so the context stays small and deterministic.
  rm -rf "${vendor_dir}/autumn/target" "${vendor_dir}/autumn-macros/target" \
         "${vendor_dir}/autumn-cli/target" "${vendor_dir}/autumn-schema-core/target" \
         "${vendor_dir}/autumn-edge/target"
  # Copy the monorepo Cargo.lock into vendor/ so `cargo install --locked` inside
  # Docker uses the same pinned dependency versions as the main workspace (e.g.
  # time=0.3.47, which is compatible with cookie-0.18.1; free resolution picks
  # time=0.3.52 which broke the time::parse() API).
  cp "${REPO_ROOT}/Cargo.lock" "${vendor_dir}/Cargo.lock"

  # A workspace root so the vendored crates' `*.workspace = true` keys,
  # `[workspace.dependencies]`, and `[workspace.lints]` resolve exactly as in the
  # real tree. Derived from the real root manifest (members trimmed to the five
  # vendored crates) so it stays in sync automatically.
  # autumn-cli is included so that `cargo install --path ./vendor/autumn-cli` (used
  # by inject_local_autumn_binary) resolves workspace dependencies correctly and
  # compiles inside Docker against the builder's glibc — avoiding the glibc version
  # mismatch that arises when copying a runner-built binary into the container.
  # autumn-schema-core is a member because autumn-cli path-depends on it and it
  # inherits `*.workspace = true` keys that must resolve against this root.
  sed 's|^members = \[.*\]|members = ["autumn", "autumn-macros", "autumn-cli", "autumn-schema-core", "autumn-edge"]|' \
    "${REPO_ROOT}/Cargo.toml" > "${vendor_dir}/Cargo.toml"

  # The scaffold's own Cargo.toml declares an (empty) `[workspace]`, which makes
  # `${PROJECT_DIR}` a workspace root covering everything beneath it — including
  # the vendored crates under `vendor/`. Cargo would then try to resolve their
  # `*.workspace = true` inheritance against the scaffold root (which has no
  # `[workspace.package]`) and fail. Exclude `vendor/` so the vendored crates
  # resolve against their own trimmed root (`vendor/Cargo.toml`) instead.
  sed -i 's|^\[workspace\]$|[workspace]\nexclude = ["vendor"]|' \
    "${PROJECT_DIR}/Cargo.toml"

  # Point the scaffold's `autumn-web` crates.io dependency at the vendored source.
  cat >> "${PROJECT_DIR}/Cargo.toml" <<'TOML'

# CI-only: build the generated image against the in-tree autumn-web rather than
# the last published crate (injected by scripts/check-release-image-boot.sh).
[patch.crates-io]
autumn-web = { path = "vendor/autumn" }
TOML
}

# The generated Dockerfile uses cargo-chef: the builder stage copies only
# `recipe.json` and runs `cargo chef cook` to pre-build dependencies before the
# real `COPY . .`. cargo-chef reconstructs skeleton manifests for the analyzed
# *workspace*, but our vendored autumn-web lives in its own (excluded) workspace
# under `vendor/`, so chef does not skeletonize it — `cargo chef cook` then needs
# the real `vendor/` source on disk and fails with "failed to read
# /app/vendor/autumn/Cargo.toml". Stage `vendor/` from the planner (which did
# `COPY . .`) before the cook step so the path-patched dependency resolves. This
# post-processing is CI-only and matches the CI-only vendoring above; the
# generated artifact a user gets is unchanged.
stage_vendor_before_chef_cook() {
  sed -i \
    's|^COPY --from=planner /app/recipe.json recipe.json$|COPY --from=planner /app/vendor vendor\nCOPY --from=planner /app/recipe.json recipe.json|' \
    "${PROJECT_DIR}/Dockerfile"
}

# Patch the generated Dockerfile to install autumn-cli from the vendored in-tree
# source rather than from crates.io. This avoids the glibc version mismatch that
# arises when a runner-built binary is copied into the Docker builder container
# (the runner may link against a newer glibc than the Debian Bookworm base image),
# and ensures sub-commands like `autumn build --embed` that post-date the last
# published release are available inside the build.  The generated Dockerfile is
# unchanged from what users receive.
inject_local_autumn_binary() {
  log "Patching Dockerfile to install autumn-cli from in-tree vendor source"
  sed -i \
    's|^RUN cargo install --locked autumn-cli.*$|RUN cargo install --locked --path ./vendor/autumn-cli|' \
    "${PROJECT_DIR}/Dockerfile"
}

vendor_in_tree_autumn_web

# ── health probe helper ─────────────────────────────────────────────────────
# Polls each URL until it returns HTTP 200 or the per-URL budget elapses.
# Each URL gets a fresh budget window so a slow-starting first endpoint cannot
# starve subsequent ones. A single curl invocation per tick captures body and
# status atomically (no TOCTOU). The body file is truncated before each curl
# so a failed request (no HTTP response written) never exposes stale bytes.
#
# Usage: probe_until_healthy <budget_secs> <url> [<url> ...]
#   budget_secs  — seconds each URL is given to reach 200
#   url(s)       — one or more endpoints; all must return 200
probe_until_healthy() {
  local budget_secs="$1"
  shift
  local urls=("$@")
  local probe_body_file
  probe_body_file="$(mktemp "${WORKDIR}/probe_body.XXXXXX")"

  for url in "${urls[@]}"; do
    local code=""
    local body=""
    LAST_PROBE_RESPONSE=""
    local url_deadline=$(( SECONDS + budget_secs ))
    while (( SECONDS < url_deadline )); do
      : > "${probe_body_file}"
      # `PROBE_CURL_OPTS` is expanded in the `${a[@]+"${a[@]}"}` form so an
      # empty array is safe under `set -u` on bash < 4.4.
      code="$(curl -o "${probe_body_file}" -s -m 5 -w '%{http_code}' \
        ${PROBE_CURL_OPTS[@]+"${PROBE_CURL_OPTS[@]}"} "${url}" 2>/dev/null || echo 000)"
      body="$(cat "${probe_body_file}" 2>/dev/null || true)"
      if [[ "${code}" == "200" ]]; then
        log "HEALTHY: ${url} -> 200 (${body})"
        break
      fi
      LAST_PROBE_RESPONSE="${code} ${body}"
      sleep 1
    done
    if [[ "${code}" != "200" ]]; then
      rm -f "${probe_body_file}"
      fail "${url} did not return 200 within ${budget_secs}s (last code: ${code:-none}, body: ${body:-<empty>})"
      return 1
    fi
  done

  rm -f "${probe_body_file}"
  return 0
}

# Report the runtime image size against the secondary budget (informational).
report_image_size() {
  local image="$1"
  local bytes mb
  bytes="$(docker image inspect "${image}" --format '{{.Size}}' 2>/dev/null || echo 0)"
  mb=$(( bytes / 1024 / 1024 ))
  log "Runtime image size: ${mb} MB (budget: ${IMAGE_SIZE_BUDGET_MB} MB)"
  if (( mb > IMAGE_SIZE_BUDGET_MB )); then
    warn "image size ${mb} MB exceeds the ${IMAGE_SIZE_BUDGET_MB} MB budget — investigate runtime image bloat"
  fi
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    printf '* Runtime image size: **%s MB** (budget %s MB)\n' "${mb}" "${IMAGE_SIZE_BUDGET_MB}" >> "${GITHUB_STEP_SUMMARY}"
  fi
}

# ── default (bare release init) target ──────────────────────────────────────
run_default_target() {
  log "release init (bare/default target)"
  ( cd "${PROJECT_DIR}" && "${AUTUMN}" release init --force )
  stage_vendor_before_chef_cook
  inject_local_autumn_binary

  log "docker build the generated image"
  if ! ( cd "${PROJECT_DIR}" && docker build -t "${IMAGE_TAG}" . 2>&1 | tee "${WORKDIR}/build.log" ); then
    fail "docker build failed — see build log above"
    exit 1
  fi

  report_image_size "${IMAGE_TAG}"

  # AC: exercise the documented one-shot migrate path against the primary
  # *before* the web container is marked ready (deployment.md Step 4).
  log "one-shot migrate against the primary (\`autumn migrate\`)"
  if ! docker run --rm --network host \
        -e AUTUMN_DATABASE__PRIMARY_URL="${DB_URL}" \
        "${IMAGE_TAG}" autumn migrate 2>&1 | tee "${WORKDIR}/migrate.log"; then
    fail "one-shot \`autumn migrate\` failed — the rollout must stop here"
    exit 1
  fi

  log "boot the web container"
  # Remove any leftover container with the same name before starting a new one.
  docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  # --network host lets the container reach the Postgres service on localhost
  # and bind :3000 on the runner. Minimal AUTUMN_* env: the primary URL, the
  # required production signing secret, and a trusted-host allowlist so the
  # prod profile binds and /health is reachable.
  docker run -d --name "${CONTAINER_NAME}" --network host \
    -e AUTUMN_DATABASE__PRIMARY_URL="${DB_URL}" \
    -e AUTUMN_SECURITY__SIGNING_SECRET="${SIGNING_SECRET}" \
    -e AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS="*" \
    "${IMAGE_TAG}"

  if ! probe_until_healthy "${STARTUP_BUDGET_SECS}" \
       "http://localhost:3000/health" \
       "http://localhost:3000/actuator/health"; then
    fail "container did not reach a healthy state — boot logs follow"
    docker logs "${CONTAINER_NAME}" || true
    printf '\n--- failing probe response ---\n%s\n' "${LAST_PROBE_RESPONSE}" >&2
    exit 1
  fi

  log "default target: image builds and boots, /health + /actuator/health = 200"
}

# ── docker-compose target ───────────────────────────────────────────────────
run_compose_target() {
  log "release init --target docker-compose"
  ( cd "${PROJECT_DIR}" && "${AUTUMN}" release init --force --target docker-compose )
  stage_vendor_before_chef_cook
  inject_local_autumn_binary

  # The generated compose app runs in the prod profile, which requires a
  # non-empty trusted-host allowlist to bind. Inject it (and a signing secret)
  # via a smoke-only override file so the *generated* compose file stays
  # untouched — the artifact under test is unchanged.
  cat > "${PROJECT_DIR}/docker-compose.override.yml" <<'YAML'
services:
  app:
    environment:
      AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS: "*"
YAML

  export AUTUMN_SECURITY__SIGNING_SECRET="${SIGNING_SECRET}"

  log "docker compose up --build (app + one-shot migrate + Postgres)"
  if ! ( cd "${PROJECT_DIR}" && docker compose up --build -d 2>&1 | tee "${WORKDIR}/compose-build.log" ); then
    fail "docker compose up failed — see build log above"
    ( cd "${PROJECT_DIR}" && docker compose logs ) || true
    exit 1
  fi

  if ! probe_until_healthy "${STARTUP_BUDGET_SECS}" \
       "http://localhost:3000/health" \
       "http://localhost:3000/actuator/health"; then
    fail "compose stack did not reach a healthy state — compose logs follow"
    ( cd "${PROJECT_DIR}" && docker compose logs ) || true
    printf '\n--- failing probe response ---\n%s\n' "${LAST_PROBE_RESPONSE}" >&2
    exit 1
  fi

  log "compose target: stack builds, migrates, and serves /health + /actuator/health = 200"
  # Teardown is handled by the EXIT trap (docker compose down -v).
}

# ── direct-TLS (HTTPS) target ───────────────────────────────────────────────
#
# Issue #1603 AC6: an app can terminate HTTPS itself ([server.tls]) with no
# reverse proxy, so the release image has to be able to boot that way and pass
# an HTTPS health check. This target mirrors `run_default_target` — same
# scaffold, same image, same one-shot migrate — with three differences, each of
# which is a documented step in docs/guide/tls.md ("Serving HTTPS from the
# release image"):
#
#   1. the app's `[features] default` turns on `tls` (the image's builder runs a
#      bare `cargo build --release`, so the feature has to be a default);
#   2. a self-signed test certificate is mounted read-only into the container
#      and pointed at with the `AUTUMN_SERVER__TLS__*` env vars;
#   3. the probes speak HTTPS and validate against that certificate, and
#      `AUTUMN_HEALTHCHECK_URL` re-points the image's own HEALTHCHECK at
#      `https://`.
#
# It then asserts the negative too: plain HTTP on the same port must NOT answer,
# or the "HTTPS" claim would pass on an app that quietly served cleartext.

# Turn on the `tls` feature by default in the generated app, so the Dockerfile's
# `cargo build --release` links the TLS stack.
enable_tls_feature() {
  log "Enabling the \`tls\` feature in the generated app"
  local manifest="${PROJECT_DIR}/Cargo.toml"
  if ! grep -q '^default = \["flash"\]$' "${manifest}"; then
    fail "scaffold Cargo.toml no longer has \`default = [\"flash\"]\`; update this gate to match"
    exit 1
  fi
  # Two edits: add `tls` to the default set, and define the forwarding feature.
  sed -i 's/^default = \["flash"\]$/default = ["flash", "tls"]\ntls = ["autumn-web\/tls"]/' "${manifest}"
  grep -q '^tls = \["autumn-web/tls"\]$' "${manifest}" \
    || { fail "failed to enable the tls feature in ${manifest}"; exit 1; }
}

# Self-signed `CN=localhost` certificate (SAN localhost + 127.0.0.1) for the
# boot check — the stand-in for the operator's real certbot/corporate cert.
# World-readable because the runtime image runs as the unprivileged `autumn`
# user (uid 10001), which does not share this runner's uid; a real deployment
# keeps the key 0600 and owned by the app user.
generate_test_certificate() {
  log "Generating a self-signed test certificate"
  TLS_DIR="${WORKDIR}/tls"
  mkdir -p "${TLS_DIR}"
  local openssl_err
  openssl_err="$(mktemp "${WORKDIR}/openssl.XXXXXX")"
  if ! openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes \
    -keyout "${TLS_DIR}/key.pem" -out "${TLS_DIR}/cert.pem" \
    -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>"${openssl_err}"; then
    fail "openssl could not generate the test certificate: $(cat "${openssl_err}")"
    exit 1
  fi
  # The container's unprivileged `autumn` user (uid 10001) does not share this
  # runner's uid, so the bind-mount root has to be traversable and both PEMs
  # readable by it. A real deployment keeps the key 0600 and owned by the app
  # user instead.
  chmod 0755 "${TLS_DIR}"
  chmod 0644 "${TLS_DIR}/cert.pem" "${TLS_DIR}/key.pem"
}

# Poll `docker inspect` until the container's OWN HEALTHCHECK reports healthy.
# This is what proves the generated HEALTHCHECK works against an HTTPS listener
# (a probe hardcoded to `http://` would leave the container `unhealthy` forever,
# and in compose `depends_on: condition: service_healthy` would never release).
wait_until_container_healthy() {
  local budget_secs="$1"
  local status=""
  local deadline=$(( SECONDS + budget_secs ))
  while (( SECONDS < deadline )); do
    status="$(docker inspect -f '{{.State.Health.Status}}' "${CONTAINER_NAME}" 2>/dev/null || echo unknown)"
    if [[ "${status}" == "healthy" ]]; then
      log "HEALTHY: the container's own HEALTHCHECK reports healthy over HTTPS"
      return 0
    fi
    # An image with no HEALTHCHECK at all makes `docker inspect` print
    # `<no value>` and exit 0, so name that case instead of burning the whole
    # budget on a probe that can never report anything.
    if [[ "${status}" == "<no value>" ]]; then
      fail "the generated image has no HEALTHCHECK directive — the probe this target verifies is gone"
      return 1
    fi
    if [[ "${status}" == "unhealthy" ]]; then
      break
    fi
    sleep 2
  done
  fail "the container HEALTHCHECK never reported healthy within ${budget_secs}s (last status: ${status:-none})"
  docker inspect -f '{{json .State.Health}}' "${CONTAINER_NAME}" 2>/dev/null || true
  docker logs "${CONTAINER_NAME}" || true
  return 1
}

run_https_target() {
  log "release init + direct TLS ([server.tls], issue #1603)"
  ( cd "${PROJECT_DIR}" && "${AUTUMN}" release init --force )
  enable_tls_feature
  generate_test_certificate
  stage_vendor_before_chef_cook
  inject_local_autumn_binary

  log "docker build the generated image (with the tls feature on)"
  if ! ( cd "${PROJECT_DIR}" && docker build -t "${IMAGE_TAG}" . 2>&1 | tee "${WORKDIR}/build.log" ); then
    fail "docker build failed — see build log above"
    exit 1
  fi

  report_image_size "${IMAGE_TAG}"

  log "one-shot migrate against the primary (\`autumn migrate\`)"
  if ! docker run --rm --network host \
        -e AUTUMN_DATABASE__PRIMARY_URL="${DB_URL}" \
        "${IMAGE_TAG}" autumn migrate 2>&1 | tee "${WORKDIR}/migrate.log"; then
    fail "one-shot \`autumn migrate\` failed — the rollout must stop here"
    exit 1
  fi

  log "boot the web container with [server.tls] configured"
  docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  # TLS comes entirely from the environment (no autumn.toml edit): the runtime
  # materializes `[server.tls]` from AUTUMN_SERVER__TLS__*, which is the shape a
  # container deployment uses. AUTUMN_HEALTHCHECK_URL re-points the image's own
  # HEALTHCHECK at https://, and AUTUMN_HEALTHCHECK_INSECURE lets that loopback
  # probe skip verification — the test certificate is issued to `localhost`, but
  # the container has no CA for it. The gate's OWN probes below still validate
  # it, with --cacert.
  docker run -d --name "${CONTAINER_NAME}" --network host \
    -v "${TLS_DIR}:/etc/autumn/tls:ro" \
    -e AUTUMN_DATABASE__PRIMARY_URL="${DB_URL}" \
    -e AUTUMN_SECURITY__SIGNING_SECRET="${SIGNING_SECRET}" \
    -e AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS="*" \
    -e AUTUMN_SERVER__TLS__CERT_PATH=/etc/autumn/tls/cert.pem \
    -e AUTUMN_SERVER__TLS__KEY_PATH=/etc/autumn/tls/key.pem \
    -e AUTUMN_HEALTHCHECK_URL=https://localhost:3000/health \
    -e AUTUMN_HEALTHCHECK_INSECURE=1 \
    "${IMAGE_TAG}"

  # Validate the certificate rather than skipping verification: the point of
  # direct TLS is that a real certificate is served, so `--cacert` (not `-k`).
  # `local` here shadows the global for the rest of this call (bash is
  # dynamically scoped), so every probe below inherits it and nothing outside
  # this function can be left holding a stale `--cacert`.
  local -a PROBE_CURL_OPTS=(--cacert "${TLS_DIR}/cert.pem")
  if ! probe_until_healthy "${STARTUP_BUDGET_SECS}" \
       "https://localhost:3000/health" \
       "https://localhost:3000/actuator/health"; then
    fail "the HTTPS container did not reach a healthy state — boot logs follow"
    docker logs "${CONTAINER_NAME}" || true
    printf '\n--- failing probe response ---\n%s\n' "${LAST_PROBE_RESPONSE}" >&2
    exit 1
  fi

  # The negative: cleartext HTTP on the same port must not answer, or the gate
  # would pass on an app that never terminated TLS at all.
  log "asserting plain HTTP is NOT served on the TLS port"
  # No `|| echo 000`: on a refused/aborted request curl prints `000` AND exits
  # non-zero, so appending a fallback would concatenate the two into `000000`
  # (and a real `200` on a request curl also errored on would read `200000`,
  # sliding past the equality check this assertion depends on).
  local plain_code=""
  plain_code="$(curl -o /dev/null -s -m 5 -w '%{http_code}' http://localhost:3000/health 2>/dev/null)" \
    || plain_code="${plain_code:-000}"
  if [[ "${plain_code}" == "200" ]]; then
    fail "http://localhost:3000/health returned 200 — the app is serving cleartext on the TLS port"
    exit 1
  fi
  log "OK: plain HTTP on :3000 does not answer (code: ${plain_code})"

  wait_until_container_healthy "${HEALTHY_BUDGET_SECS}" || exit 1

  log "https target: image builds and boots over TLS, HTTPS /health + /actuator/health = 200"
}

case "${TARGET}" in
  default)        run_default_target ;;
  docker-compose) run_compose_target ;;
  https)          run_https_target ;;
  *)
    fail "unknown target '${TARGET}'; expected 'default', 'docker-compose', or 'https'"
    exit 2
    ;;
esac

log "release-image-boot gate passed for target '${TARGET}'"
