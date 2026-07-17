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

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl jq git tar gzip build-essential \
  pkg-config libssl-dev postgresql-client openssh-client iptables

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
  "${slot}/bin/installdependencies.sh" || true
done
chown -R "${RUNNER_USER}:${RUNNER_USER}" "${RUNNER_HOME}"

# --- config dir; the PAT lands here over SSH (root-only) after boot ---
mkdir -p /etc/autumn-runner
cat > /etc/autumn-runner/env <<EOF
GH_OWNER=${GH_OWNER}
GH_REPO=${GH_REPO}
RUNNER_HOME=${RUNNER_HOME}
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
sudo -u runner ./config.sh --unattended --replace \
  --url "https://github.com/${GH_OWNER}/${GH_REPO}" \
  --token "${reg_token}" \
  --name "hetzner-$(hostname)-${slot}" \
  --labels "self-hosted,hetzner,linux,x64" \
  --ephemeral
exec sudo -u runner ./run.sh
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
ExecStart=/opt/autumn-runner/run-ephemeral.sh %i
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
