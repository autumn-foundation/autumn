#!/usr/bin/env bash
# Real-VPS deploy validation (issue #1948, AC-7 follow-up).
#
# Drives the REAL `autumn deploy` lifecycle against a REAL, freshly-provisioned
# cloud VM over ssh/scp, mirroring the container e2e harness
# (`autumn-cli/tests/deploy_e2e.rs`) but covering the four things a container
# cannot:
#
#   1. real kernel reboot survival (`systemctl reboot`, not `is-enabled`),
#   2. bare-Ubuntu host prep from a STOCK image (no pre-baked deploy tooling),
#   3. the "<15 min / <=3 command" onboarding wall-clock success metric, and
#   4. real pam_systemd / XDG_RUNTIME_DIR control-socket fidelity (pam_systemd is
#      NOT disabled here — the ssh session gets XDG_RUNTIME_DIR=/run/user/0 just
#      like a real host).
#
# This is a shell script rather than a parameterization of the Rust harness on
# purpose: `deploy_e2e.rs` is structurally bound to BUILDING and LAUNCHING its
# own testcontainers fixture image (build_fixture_image / GenericImage / the
# per-run ssh-agent), so retargeting it at an arbitrary externally-provisioned
# host would be a far more invasive change than reproducing its curl/ssh
# assertions here. The product binary under test (`$AUTUMN_BIN deploy ...`) is
# the single source of truth for the deploy behaviour either way.
#
# Required environment (all provided by the workflow; NO credential is ever
# hardcoded here):
#   AUTUMN_BIN            absolute path to the built `autumn` CLI on the runner
#   TARGET_HOST          public IP / hostname of the provisioned VM
#   SSH_USER             ssh user (root)
#   SSH_KEY              path to the ephemeral private key
#   SSH_PORT             ssh port (22)
#   PUBLIC_PORT          public HTTP port kamal-proxy binds on the VM (80)
#   APP_NAME             deploy app name (rvpsapp)
#   SIGNING_SECRET       AUTUMN_SECURITY__SIGNING_SECRET (64 hex chars)
#   KAMAL_PROXY_VERSION  kamal-proxy image tag to install on the host bootstrap
#   ONBOARD_BUDGET_SECS  wall-clock budget for onboarding->first-serve (900 = 15m)
set -euo pipefail

# ── Inputs / defaults ────────────────────────────────────────────────────────
: "${AUTUMN_BIN:?AUTUMN_BIN (path to built autumn CLI) is required}"
: "${TARGET_HOST:?TARGET_HOST (VM IP) is required}"
SSH_USER="${SSH_USER:-root}"
: "${SSH_KEY:?SSH_KEY (private key path) is required}"
SSH_PORT="${SSH_PORT:-22}"
PUBLIC_PORT="${PUBLIC_PORT:-80}"
APP_NAME="${APP_NAME:-rvpsapp}"
: "${SIGNING_SECRET:?SIGNING_SECRET is required}"
KAMAL_PROXY_VERSION="${KAMAL_PROXY_VERSION:-latest}"
ONBOARD_BUDGET_SECS="${ONBOARD_BUDGET_SECS:-900}"

BASE_URL="http://${TARGET_HOST}:${PUBLIC_PORT}"
WORK="$(mktemp -d)"
PROJECT="${WORK}/project"
mkdir -p "${PROJECT}"

log()  { printf '\n[real-vps] %s\n' "$*"; }
fail() { printf '\n[real-vps][FAIL] %s\n' "$*" >&2; exit 1; }

SSH_OPTS=(
  -i "${SSH_KEY}"
  -p "${SSH_PORT}"
  -o BatchMode=yes
  -o StrictHostKeyChecking=accept-new
  -o "UserKnownHostsFile=${WORK}/known_hosts"
  -o ConnectTimeout=10
)
rssh() { ssh "${SSH_OPTS[@]}" "${SSH_USER}@${TARGET_HOST}" "$@"; }

# ── HTTP helpers (mirror the harness's wait_for_http_ok / Prober) ────────────
http_body() { curl -fsS --max-time 5 "${BASE_URL}$1" 2>/dev/null; }

wait_for_serving() { # <path> <expected-substr> <timeout-secs>
  local path="$1" want="$2" timeout="$3" deadline
  deadline=$(( $(date +%s) + timeout ))
  local body=""
  while [ "$(date +%s)" -lt "${deadline}" ]; do
    if body="$(http_body "${path}")" && printf '%s' "${body}" | grep -q "${want}"; then
      printf '%s' "${body}"
      return 0
    fi
    sleep 1
  done
  fail "timed out waiting for '${want}' on ${BASE_URL}${path} (last body: ${body:-<none>})"
}

wait_for_ssh() { # <timeout-secs>
  local timeout="$1" deadline
  deadline=$(( $(date +%s) + timeout ))
  while [ "$(date +%s)" -lt "${deadline}" ]; do
    if rssh true 2>/dev/null; then return 0; fi
    sleep 3
  done
  fail "timed out waiting for ssh on ${TARGET_HOST}:${SSH_PORT}"
}

# Background zero-downtime prober: hammer / with fresh connections, count
# failures. Writes "total failures" to the given file on stop.
start_prober() { # <outfile>
  local out="$1"
  # Background the prober as this function's LAST statement, and DO NOT echo the
  # PID / use command substitution to launch it. A shell function runs in the
  # current (main) shell — it is not forked — so `( ... ) &` here backgrounds a
  # job that is a genuine child of the main shell, and the caller reads its PID
  # from `$!` in the main shell right after the call. This matters because
  # stop_prober's `wait "$pid"` only works on a child of the calling shell: if we
  # instead launched via `prober_pid="$(start_prober ...)"`, the `&` job would run
  # inside the command-substitution subshell and be reparented to init the moment
  # `$(...)` returned, so the captured PID would not be our child — `wait` would
  # error ("not a child of this shell") and stop_prober would delete prober.stop
  # before the prober ever saw it or wrote ${out}, failing every real-VPS run.
  # The `>/dev/null 2>&1` stays so the subshell can never block on / spew to the
  # log; the result is reported via the `${out}` file, not stdout.
  (
    total=0; fail=0
    while [ ! -f "${WORK}/prober.stop" ]; do
      total=$((total+1))
      curl -fsS --max-time 3 -o /dev/null "${BASE_URL}/" 2>/dev/null || fail=$((fail+1))
      sleep 0.1
    done
    echo "${total} ${fail}" > "${out}"
  ) >/dev/null 2>&1 &
}
stop_prober() { touch "${WORK}/prober.stop"; wait "$1" 2>/dev/null || true; rm -f "${WORK}/prober.stop"; }

# ── Build the tiny static test app (mirrors fixtures/deploy/app_template.rs) ──
# A genuine HTTP server: binds AUTUMN_SERVER__HOST:PORT (what the slot unit
# sets), answers /ready 200 and / with its version marker; AUTUMN_MIGRATE=1 is a
# no-op exit 0. Compiled fully static so it runs on the stock VM regardless of
# its glibc.
compile_app() { # <version> <out>
  local version="$1" out="$2" src="${WORK}/app_${version}.rs"
  cat > "${src}" <<RUST
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
const VERSION: &str = "${version}";
fn main() {
    if std::env::var("AUTUMN_MIGRATE").as_deref() == Ok("1") { println!("migrate no-op {VERSION}"); return; }
    let host = std::env::var("AUTUMN_SERVER__HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("AUTUMN_SERVER__PORT").unwrap_or_else(|_| "8080".to_string());
    let listener = TcpListener::bind(format!("{host}:{port}")).expect("bind");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let (status, body) = if path == "/ready" { ("200 OK", "ready".to_string()) }
                else { ("200 OK", format!("${APP_NAME} {VERSION}\n")) };
            let resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = s.write_all(resp.as_bytes());
        });
    }
}
RUST
  rustc -O -C target-feature=+crt-static "${src}" -o "${out}"
}

stage_release() { # <bin>
  mkdir -p "${PROJECT}/target/release"
  cp "$1" "${PROJECT}/target/release/${APP_NAME}"
}

write_config() {
  cat > "${PROJECT}/autumn.toml" <<TOML
[server]
port = ${PUBLIC_PORT}

[deploy]
host = "${TARGET_HOST}"
user = "${SSH_USER}"
ssh_port = ${SSH_PORT}
app_name = "${APP_NAME}"
readiness_timeout_secs = 60
TOML
}

autumn_deploy() { # <args...>
  ( cd "${PROJECT}" \
      && HOME="${WORK}/home" \
         AUTUMN_SECURITY__SIGNING_SECRET="${SIGNING_SECRET}" \
         "${AUTUMN_BIN}" deploy "$@" )
}

# The deploy child's keyless ssh discovers the key from a private ssh-agent
# (exactly as the container harness does), so no product code path needs -i.
mkdir -p "${WORK}/home/.ssh"
cp "${SSH_KEY}" "${WORK}/home/.ssh/id_ed25519"
chmod 600 "${WORK}/home/.ssh/id_ed25519"
eval "$(ssh-agent -s)"
trap 'ssh-agent -k >/dev/null 2>&1 || true' EXIT
ssh-add "${WORK}/home/.ssh/id_ed25519"

# ── Host bootstrap on the STOCK Ubuntu image (item 2) ────────────────────────
# `autumn deploy` does the app-specific host prep itself; the ONE documented
# precondition (see docs/guide/deployment.md "Limitations") is that the
# kamal-proxy binary is present at /usr/local/bin/kamal-proxy. We install curl +
# drop the proxy binary here, from a stock image — nothing deploy-specific is
# pre-baked. (This mirrors "install ssh/curl, drop kamal-proxy" from the issue.)
log "host bootstrap on stock Ubuntu (install curl + kamal-proxy ${KAMAL_PROXY_VERSION})"
wait_for_ssh 300
rssh 'set -e; export DEBIAN_FRONTEND=noninteractive; apt-get update -qq;
      apt-get install -y -qq curl docker.io >/dev/null;
      cid=$(docker create basecamp/kamal-proxy:'"${KAMAL_PROXY_VERSION}"');
      docker cp "$cid:/usr/local/bin/kamal-proxy" /usr/local/bin/kamal-proxy;
      docker rm -f "$cid" >/dev/null;
      chmod 755 /usr/local/bin/kamal-proxy;
      /usr/local/bin/kamal-proxy version || true' \
  || fail "host bootstrap failed"
# pam_systemd is deliberately LEFT ENABLED (unlike the container fixture) so the
# ssh sessions the deploy opens get XDG_RUNTIME_DIR=/run/user/0 — the real-host
# control-socket shape (item 4).

# ── Compile v1 / v2, write config ────────────────────────────────────────────
log "compiling test app binaries (v1, v2)"
compile_app v1 "${WORK}/app_v1"
compile_app v2 "${WORK}/app_v2"
write_config

# ── 1. First deploy (v1) + onboarding wall-clock metric (item 3) ─────────────
# The onboarding "commands to first serve" the metric counts are the
# autumn-specific ones a new user runs; host bootstrap above is the documented
# precondition. We TIME from the first deploy command to first public serve.
log "onboarding: first deploy (v1) — timing to first public serve"
onboard_start=$(date +%s)
stage_release "${WORK}/app_v1"
autumn_deploy up || fail "first 'deploy up' failed"
body="$(wait_for_serving / "${APP_NAME} v1" 60)"
onboard_end=$(date +%s)
onboard_secs=$(( onboard_end - onboard_start ))
log "first deploy serves: ${body}"
log "ONBOARDING METRIC: first serve in ${onboard_secs}s (budget ${ONBOARD_BUDGET_SECS}s), onboarding commands = 1 (\`autumn deploy up\`)"
[ "${onboard_secs}" -le "${ONBOARD_BUDGET_SECS}" ] \
  || fail "onboarding exceeded ${ONBOARD_BUDGET_SECS}s (took ${onboard_secs}s)"

# Boot-survival persistence intent (same as the container harness asserts).
rssh "systemctl is-enabled ${APP_NAME}-blue.service" | grep -q enabled \
  || fail "blue slot unit is not enabled for boot survival"

# ── 2. REAL reboot survival (item 1) ─────────────────────────────────────────
log "rebooting the VM (real kernel power-cycle)"
rssh 'systemctl reboot' || true   # connection drops as the box goes down
sleep 20
wait_for_ssh 300
log "VM back up — asserting app AND kamal-proxy serve after reboot"
body="$(wait_for_serving / "${APP_NAME} v1" 120)"
rssh 'systemctl is-active kamal-proxy.service' | grep -q '^active' \
  || fail "kamal-proxy.service is not active after reboot"
rssh "systemctl is-active ${APP_NAME}-blue.service" | grep -q '^active' \
  || fail "${APP_NAME}-blue.service is not active after reboot"
log "reboot survival OK — serves ${body}, kamal-proxy + app units active"

# ── 3. Zero-downtime redeploy (v2) under load (AC-2) ─────────────────────────
log "zero-downtime redeploy to v2 under a background prober"
sleep 2   # distinct per-second release id
stage_release "${WORK}/app_v2"
# Launch in the main shell (no command substitution) so the prober is a child of
# THIS shell and `$!` is its PID — required for stop_prober's `wait` to join it.
# Capture `$!` immediately; nothing else may background before this read.
start_prober "${WORK}/prober.out"
prober_pid=$!
autumn_deploy up || { stop_prober "${prober_pid}"; fail "redeploy 'deploy up' failed"; }
stop_prober "${prober_pid}"
read -r total failures < "${WORK}/prober.out"
log "prober: ${failures}/${total} requests failed during cutover"
[ "${failures}" -eq 0 ] || fail "zero-downtime redeploy dropped ${failures}/${total} requests"
[ "${total}" -gt 0 ] || fail "prober made no requests"
body="$(wait_for_serving / "${APP_NAME} v2" 60)"
log "redeploy OK — now serves ${body}"

# ── 4. On-demand rollback (v2 -> v1) ─────────────────────────────────────────
log "on-demand rollback to previous release (v1)"
autumn_deploy rollback || fail "'deploy rollback' failed"
body="$(wait_for_serving / "${APP_NAME} v1" 60)"
log "rollback OK — restored ${body}"

# ── 5. kamal-proxy control-socket fidelity under real pam_systemd (item 4) ───
# The redeploy + rollback above ALREADY exercised `kamal-proxy deploy` over ssh
# sessions carrying XDG_RUNTIME_DIR=/run/user/0 (pam_systemd is enabled) — a
# green run is itself the proof the control socket was reached despite the
# service/ssh XDG_RUNTIME_DIR mismatch. Assert the shape deterministically:
#   (a) an ssh session really does get XDG_RUNTIME_DIR=/run/user/0 (real-host
#       shape, NOT the disabled-pam container workaround), and
#   (b) the supervised service's control socket lives at /tmp/kamal-proxy.sock
#       (the XDG-unset fallback the product pins the CLI to).
# Together with the successful flips above, that proves the CLI reached the
# service's socket across the XDG boundary.
log "verifying kamal-proxy control socket under real pam_systemd"
xdg="$(rssh 'printf %s "${XDG_RUNTIME_DIR:-<unset>}"')"
log "ssh session XDG_RUNTIME_DIR=${xdg}"
[ "${xdg}" = "/run/user/0" ] \
  || fail "expected pam_systemd XDG_RUNTIME_DIR=/run/user/0 in ssh session, got '${xdg}' (real-host shape not reproduced)"
rssh 'test -S /tmp/kamal-proxy.sock' \
  || fail "kamal-proxy control socket not found at /tmp/kamal-proxy.sock (service/CLI socket mismatch?)"
log "control-socket fidelity OK — service socket at /tmp/kamal-proxy.sock reached by the CLI despite ssh XDG_RUNTIME_DIR=${xdg}"

log "ALL real-VPS deploy AC-7 assertions passed"
