#!/usr/bin/env bash
# Pull container images once, with retries, before any testcontainer test runs.
#
#     ./scripts/ci-prepull-images.sh postgres:11-alpine postgres:16-alpine
#
# Every testcontainer DB test starts its own container, and they all start at
# the same moment. On a runner that does not have the image cached they all pull
# it in parallel, and one of those pulls can die mid-stream
# (`PullImage ... "bytes remaining on stream"`), which fails an unrelated test
# before its body runs. Pulling once, serially, with a few retries, leaves every
# test a cached image to start from.
#
# Docker Hub limits unauthenticated pulls per runner IP, and a shared runner can
# arrive with the limit already spent (`toomanyrequests`). Waiting does not help
# within a job, so a rate-limited or failed image is pulled from a Docker Hub
# mirror instead and tagged with its Docker Hub name: testcontainers then finds
# it in the local cache and never asks Docker Hub.
#
# Exits non-zero only if an image still cannot be pulled after every retry and
# every mirror, so a registry that is genuinely down fails here, loudly, not in
# a random test.

set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <image> [<image>...]" >&2
  exit 2
fi

attempts="${PREPULL_ATTEMPTS:-4}"
mirrors="${PREPULL_MIRRORS:-mirror.gcr.io public.ecr.aws/docker}"

# Pull from Docker Hub with retries. Returns 1 at once on a rate limit.
pull_hub() {
  local image="$1" attempt=1 out
  until out="$(docker pull "$image" 2>&1)"; do
    echo "$out" >&2
    if grep -q toomanyrequests <<<"$out"; then
      echo "Docker Hub rate-limited the pull of $image" >&2
      return 1
    fi
    if [ "$attempt" -ge "$attempts" ]; then
      return 1
    fi
    delay=$((attempt * 5))
    echo "pull of $image failed (attempt $attempt/$attempts); retrying in ${delay}s" >&2
    sleep "$delay"
    attempt=$((attempt + 1))
  done
  echo "$out"
}

# Pull `image` from a mirror and tag it with its Docker Hub name.
pull_mirror() {
  local image="$1" path="$1" mirror
  # An official image (`postgres:16-alpine`) lives under `library/`.
  [[ "$image" == */* ]] || path="library/$image"
  for mirror in $mirrors; do
    if docker pull "$mirror/$path"; then
      docker tag "$mirror/$path" "$image"
      return 0
    fi
    echo "pull of $image from $mirror failed" >&2
  done
  return 1
}

for image in "$@"; do
  if ! pull_hub "$image" && ! pull_mirror "$image"; then
    echo "error: could not pull $image from Docker Hub or any mirror" >&2
    exit 1
  fi
done
