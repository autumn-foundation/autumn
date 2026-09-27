#!/usr/bin/env bash
# Autumn self-hosted GitHub Actions runner bootstrap (Hetzner Cloud user-data).
# Runs once at first boot as root. Installs Docker + the CI toolchain deps that
# ci.yml expects on a Linux runner, plus the Actions runner, then installs a
# systemd template unit driving N EPHEMERAL runner slots (one job per
# registration). NO secrets are baked in here: the registration PAT is
# delivered out-of-band over SSH after boot (see
# provision-self-hosted-runner.yml), so it never lands in cloud user-data or the
# instance metadata service. GH_OWNER / GH_REPO / RUNNER_COUNT are prepended as
# exports by the provisioning workflow before this is used as user-data.
set -euo pipefail

RUNNER_USER="runner"
RUNNER_HOME="/opt/autumn-runner"
RUNNER_COUNT="${RUNNER_COUNT:-6}"
GH_OWNER="${GH_OWNER:-madmax983}"
GH_REPO="${GH_REPO:-autumn}"
RUNNER_VERSION="${RUNNER_VERSION:-}"   # empty => resolve the latest release at boot
# Workflow-controlled runner-name prefix so the provisioner can verify THIS run's
# runners by name (see provision-self-hosted-runner.yml). Empty => run-ephemeral.sh
# falls back to hetzner-$(hostname).
RUNNER_NAME_PREFIX="${RUNNER_NAME_PREFIX:-}"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl jq git tar gzip build-essential \
  pkg-config libssl-dev postgresql-client openssh-client iptables iptables-persistent

# --- Docker (testcontainers used by the ci.yml Docker-dependent test sweep) ---
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
# shellcheck disable=SC1091
. /etc/os-release
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${VERSION_CODENAME} stable" \
  > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker

# --- system-wide Rust toolchain -------------------------------------------
# ci.yml's Test job sets Rust up with dtolnay/rust-toolchain, which installs
# cargo/rustc into the RUNNER user's ~/.cargo/bin and only exports that dir via
# $GITHUB_PATH for workflow *steps*. Subprocesses SPAWNED BY THE TESTS — e.g.
# autumn-cli's `Command::new("cargo").args(["metadata", ...])` (src/dev.rs) and
# `Command::new(rustc)` (src/new.rs) — resolve the binary via a plain PATH
# lookup and do not reliably inherit that step-only PATH, and a test that
# env_clear()s or overrides $HOME loses the $HOME/.cargo resolution entirely, so
# those spawns fail with `Os { code: 2, kind: NotFound }`. Install the toolchain
# system-wide in an absolute, HOME-independent location so every process (and
# every child it spawns) can resolve cargo/rustc/rustup regardless of $HOME or a
# reset environment.
export RUSTUP_HOME=/opt/rust/rustup
export CARGO_HOME=/opt/rust/cargo
curl -fsSL https://sh.rustup.rs \
  | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
      --component rustfmt --component clippy
# Record a system default toolchain under the absolute RUSTUP_HOME so a rustup
# proxy invoked with a cleared/reset environment still resolves a toolchain.
RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo /opt/rust/cargo/bin/rustup default stable
# Belt-and-suspenders: symlink the proxies onto the exec search path. The
# load-bearing target is /usr/bin: when a process spawns a command by name with
# PATH unset/cleared (Command::env_clear()), glibc's execvp falls back to the
# confstr _CS_PATH default of `/bin:/usr/bin` (verify: `getconf PATH`), which
# does NOT include /usr/local/bin — so a link there alone would still NotFound
# an env_clear'd `cargo`/`rustc` spawn. Linking into /usr/bin puts the proxies
# on that default path (`env -i /usr/bin/cargo --version` succeeds). We also
# link into /usr/local/bin, which is earlier in a normal PATH. /usr/bin/{cargo,
# rustc,rustup} are free on this image (Rust is installed via rustup at /opt/rust,
# not apt), so `ln -sf` clobbers no distro package.
for b in cargo rustc rustup; do
  ln -sf /opt/rust/cargo/bin/"${b}" /usr/local/bin/"${b}"
  ln -sf /opt/rust/cargo/bin/"${b}" /usr/bin/"${b}"
done
# Login shells (and anything sourcing /etc/profile) also get the toolchain BIN
# dir on PATH. Deliberately do NOT export RUSTUP_HOME/CARGO_HOME here: pointing
# them at the read-only /opt/rust would break job-time toolchain/tool installs
# (dtolnay/rust-toolchain, `cargo install cargo-fuzz`) for login-shell jobs too.
# Left unset, they default to the runner user's writable ~/.rustup / ~/.cargo.
cat > /etc/profile.d/rust.sh <<'PROFILE'
export PATH="/opt/rust/cargo/bin:${PATH}"
PROFILE
chmod 0644 /etc/profile.d/rust.sh
# Make the whole toolchain world-readable/traversable (done LAST so every file
# rustup wrote is covered) so the unprivileged `runner` user — and the CI test
# subprocesses running as it — can execute it.
chmod -R a+rX /opt/rust

# --- unprivileged runner user (needs sudo: ci.yml runs sudo apt-get / sudo rm) ---
if ! id -u "${RUNNER_USER}" >/dev/null 2>&1; then
  useradd -m -s /bin/bash "${RUNNER_USER}"
fi
usermod -aG docker "${RUNNER_USER}"
echo "${RUNNER_USER} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/90-autumn-runner
chmod 0440 /etc/sudoers.d/90-autumn-runner

# --- harden: deny the runner user access to the cloud metadata service so a job
#     can never read Hetzner instance metadata / injected data. ---
iptables -I OUTPUT -m owner ! --uid-owner 0 -d 169.254.169.254 -j REJECT || true
# Docker bridge networking forwards CONTAINER traffic through the FORWARD chain,
# not OUTPUT — so a testcontainer in a CI job would otherwise bypass the rule
# above and reach the metadata IP. Block forwarded traffic to it as well.
iptables -I FORWARD -d 169.254.169.254 -j REJECT || true
mkdir -p /etc/iptables
iptables-save > /etc/iptables/rules.v4 || true

# --- runner tarball ---
if [ -z "${RUNNER_VERSION}" ]; then
  RUNNER_VERSION="$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name)"
  RUNNER_VERSION="${RUNNER_VERSION#v}"
fi
mkdir -p "${RUNNER_HOME}"
curl -fsSL -o /tmp/actions-runner.tar.gz \
  "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
for i in $(seq 1 "${RUNNER_COUNT}"); do
  slot="${RUNNER_HOME}/slot-${i}"
  mkdir -p "${slot}"
  tar -xzf /tmp/actions-runner.tar.gz -C "${slot}"
done
# installdependencies.sh installs host-wide apt packages, so once is enough for
# all slots.
"${RUNNER_HOME}/slot-1/bin/installdependencies.sh" || true
chown -R "${RUNNER_USER}:${RUNNER_USER}" "${RUNNER_HOME}"

# --- config dir; the PAT lands here over SSH (root-only) after boot ---
mkdir -p /etc/autumn-runner
cat > /etc/autumn-runner/env <<EOF
GH_OWNER=${GH_OWNER}
GH_REPO=${GH_REPO}
RUNNER_HOME=${RUNNER_HOME}
RUNNER_NAME_PREFIX=${RUNNER_NAME_PREFIX}
EOF
chmod 0644 /etc/autumn-runner/env

# --- per-cycle ephemeral wrapper (runs as root to read the PAT, drops to the
#     runner user for config/run; actions/runner refuses to run as root) ---
cat > "${RUNNER_HOME}/run-ephemeral.sh" <<'WRAP'
#!/usr/bin/env bash
set -euo pipefail
slot="$1"
# shellcheck disable=SC1091
. /etc/autumn-runner/env
cd "${RUNNER_HOME}/slot-${slot}"
pat="$(cat /etc/autumn-runner/pat)"
reg_token="$(curl -fsSL -X POST \
  -H "Authorization: Bearer ${pat}" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/repos/${GH_OWNER}/${GH_REPO}/actions/runners/registration-token" \
  | jq -r .token)"
# An --ephemeral runner deregisters server-side after its single job, but a
# prior run (or a crash) can leave this slot's LOCAL config in place, which
# makes config.sh refuse with "already configured" (--replace only reconciles
# the REMOTE same-named runner). Clear the local config so every restart
# re-registers cleanly and the slot keeps taking jobs.
sudo -u runner rm -f .runner .credentials .credentials_rsaparams 2>/dev/null || true
sudo -u runner ./config.sh --unattended --replace \
  --url "https://github.com/${GH_OWNER}/${GH_REPO}" \
  --token "${reg_token}" \
  --name "${RUNNER_NAME_PREFIX:-hetzner-$(hostname)}-${slot}" \
  --labels "self-hosted,hetzner,linux,x64" \
  --ephemeral
# The runner worker (Runner.Worker) is started via `sudo -u runner ./run.sh`,
# and sudo's default env_reset resets PATH to secure_path — so the unit's
# Environment=PATH never reaches the worker or the cargo/rustc it spawns. Set
# PATH explicitly with `env` at the sudo boundary so run.sh, Runner.Worker, and
# every spawned `cargo`/`rustc` subprocess resolve the system toolchain binaries
# via /opt/rust/cargo/bin + the /usr/local/bin symlinks.
#
# Deliberately do NOT set RUSTUP_HOME/CARGO_HOME here: /opt/rust is root-owned
# and read-only (a+rX), so pointing them there would make job-time installs
# (dtolnay/rust-toolchain@nightly, `cargo install cargo-fuzz`) fail with
# permission errors. Left unset, they default to the runner user's WRITABLE
# ~/.rustup / ~/.cargo, where the job's own toolchain step installs; the rustup
# proxy (found on PATH) then resolves that per-user toolchain. This mirrors
# GitHub-hosted runners: system-discoverable binaries + a per-user writable home.
exec sudo -u runner env \
  PATH="/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
  ./run.sh
WRAP
chmod 0755 "${RUNNER_HOME}/run-ephemeral.sh"

# --- systemd template unit (installed disabled; the provisioner enables slots
#     once the PAT is in place) ---
cat > /etc/systemd/system/autumn-runner@.service <<'UNIT'
[Unit]
Description=Autumn ephemeral GitHub Actions runner (slot %i)
After=network-online.target docker.service
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
# Put the system-wide Rust toolchain BIN dir (installed by bootstrap.sh at
# /opt/rust) on the service PATH so the runner service, the Runner.Worker it
# forks, `cargo`, AND every subprocess the tests spawn (autumn-cli's
# `cargo metadata` / `rustc`) resolve cargo/rustc/rustup without depending on
# $GITHUB_PATH or the runner user's $HOME (fixes the `Os NotFound` spawn
# failures on the self-hosted runner). Deliberately do NOT set
# RUSTUP_HOME/CARGO_HOME: /opt/rust is read-only, so pointing them there would
# break job-time installs (dtolnay/rust-toolchain, cargo-fuzz). Left unset they
# default to the runner user's writable ~/.rustup / ~/.cargo.
Environment=PATH=/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/opt/autumn-runner/run-ephemeral.sh %i
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
