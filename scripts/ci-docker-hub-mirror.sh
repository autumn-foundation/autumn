#!/usr/bin/env bash
# Send the runner's Docker Hub pulls through a mirror.
#
#     ./scripts/ci-docker-hub-mirror.sh
#
# Docker Hub limits unauthenticated pulls per IP, and a shared GitHub runner
# often arrives with that limit spent (`toomanyrequests`, or a timeout on
# auth.docker.io). Everything that pulls through the daemon then fails before
# any test runs: a `FROM rust:...` in a generated Dockerfile, a `docker compose`
# service, a testcontainer. Adding a `registry-mirrors` entry makes the daemon
# try the mirror first for every `docker.io` image, and Docker Hub only when
# the mirror does not have it.
#
# `registry-mirrors` is reloadable, so this sends SIGHUP rather than restarting
# the daemon: a job's `services:` containers keep running.
#
# Best effort: a runner where the daemon cannot be reconfigured keeps pulling
# from Docker Hub directly, with a warning.

set -euo pipefail

mirror="${DOCKER_HUB_MIRROR:-https://mirror.gcr.io}"
config=/etc/docker/daemon.json

if [ "$(uname -s)" != Linux ] || ! command -v jq >/dev/null || ! sudo -n true 2>/dev/null; then
  echo "::warning::cannot configure the Docker daemon here; pulls go to Docker Hub"
  exit 0
fi

current='{}'
if sudo test -s "$config"; then
  current="$(sudo cat "$config")"
fi
updated="$(jq --arg m "$mirror" \
  '.["registry-mirrors"] = ((.["registry-mirrors"] // []) + [$m] | unique)' <<<"$current")"
sudo mkdir -p "$(dirname "$config")"
printf '%s\n' "$updated" | sudo tee "$config" >/dev/null

if ! sudo systemctl reload docker 2>/dev/null; then
  sudo pkill -HUP -x dockerd || true
fi

for _ in $(seq 1 30); do
  if docker info --format '{{json .RegistryConfig.Mirrors}}' 2>/dev/null | grep -qF "${mirror%/}"; then
    echo "Docker Hub mirror: $mirror"
    exit 0
  fi
  sleep 1
done
echo "::warning::the Docker daemon did not pick up $mirror; pulls go to Docker Hub"
