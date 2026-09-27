# Fixture image for the AC-7 deploy end-to-end test
# (`autumn-cli/tests/deploy_e2e.rs`).
#
# A privileged systemd container that stands in for a freshly-provisioned VPS: it
# boots real systemd (PID 1), runs a real sshd the `autumn deploy` child connects
# to over the mapped port, and ships the `kamal-proxy` binary the deploy path
# supervises on the public port. The deploy itself installs the app's systemd
# units, runs migrations via `systemd-run`, health-gates `/ready` with `curl`, and
# flips traffic with `kamal-proxy` — nothing here is stubbed.
#
# Two files are dropped into the build context by the harness at test time rather
# than committed to the repo:
#   * `authorized_keys` — the PUBLIC half of a throwaway keypair the harness
#     generates per run (the private half stays on the host for the ssh client).
#   * `kamal-proxy`      — extracted from the `basecamp/kamal-proxy` Docker Hub
#     image via `docker create` + `docker cp` (GitHub release download is blocked
#     in this environment; the image ships the same static binary).
FROM jrei/systemd-ubuntu:22.04

# openssh-server: the real sshd the deploy connects to.
# curl:          the readiness gate (`curl -fsS .../ready`) runs on the host.
# apt over HTTP works in-container even where HTTPS egress is blocked.
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server curl \
    && rm -rf /var/lib/apt/lists/* \
    # Pre-generate the sshd host keys so the first connection after boot succeeds
    # without waiting on lazy key generation.
    && ssh-keygen -A

# Bake in the throwaway public key so root can log in over ssh key auth only.
RUN mkdir -p /root/.ssh && chmod 700 /root/.ssh
COPY authorized_keys /root/.ssh/authorized_keys
RUN chmod 600 /root/.ssh/authorized_keys

# The reverse proxy the deploy path supervises on the public port.
COPY kamal-proxy /usr/local/bin/kamal-proxy
RUN chmod 755 /usr/local/bin/kamal-proxy

# kamal-proxy control-socket path fidelity between the supervised
# `kamal-proxy run` systemd service and the `kamal-proxy deploy` commands the
# deploy issues over ssh. kamal-proxy places its control socket at
# `$XDG_RUNTIME_DIR/kamal-proxy.sock`, falling back to `/tmp/kamal-proxy.sock`
# when unset. The systemd service has no `XDG_RUNTIME_DIR` (-> `/tmp`), but an ssh
# login session gets `XDG_RUNTIME_DIR=/run/user/0` from pam_systemd (-> a
# different path), so a NAIVE deploy would fail to reach the running proxy with
# "connect: no such file or directory". (In a normal Kamal deployment kamal-proxy
# runs as a container and the CLI shares its namespace, so this only bites the
# systemd-over-ssh shape exercised by this fixture.)
#
# `ENABLE_PAM_SYSTEMD` (build arg, issue #1948 item 4) selects which shape the
# fixture reproduces:
#   * `0` (default) — DISABLE pam_systemd for logins, so ssh sessions inherit no
#     `XDG_RUNTIME_DIR` and both sides fall back to `/tmp/kamal-proxy.sock`. This
#     is the historical workaround the container harness used to sidestep the
#     mismatch.
#   * `1` — LEAVE pam_systemd ENABLED, so ssh sessions get
#     `XDG_RUNTIME_DIR=/run/user/0` exactly like a real host. This is the honest
#     regression shape: it proves `autumn deploy` reaches the control socket
#     WITHOUT the fixture papering over the mismatch (the product now pins the
#     kamal-proxy CLI to the service's socket by unsetting `XDG_RUNTIME_DIR` on
#     the `kamal-proxy deploy` invocations — see `KamalProxyController`).
ARG ENABLE_PAM_SYSTEMD=0
RUN if [ "$ENABLE_PAM_SYSTEMD" != "1" ]; then \
        sed -i 's/^session[[:space:]].*pam_systemd\.so/# &/' \
            /etc/pam.d/common-session /etc/pam.d/common-session-noninteractive || true; \
    else \
        echo "pam_systemd left ENABLED (real-host XDG_RUNTIME_DIR fidelity, issue #1948 item 4)"; \
    fi

# Start sshd on boot (systemd is PID 1 in this image).
RUN systemctl enable ssh

EXPOSE 22
