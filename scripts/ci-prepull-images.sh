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
# Exits non-zero only if an image still cannot be pulled after every retry, so a
# registry that is genuinely down fails here, loudly, not in a random test.

set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <image> [<image>...]" >&2
  exit 2
fi

attempts="${PREPULL_ATTEMPTS:-4}"

for image in "$@"; do
  attempt=1
  until docker pull "$image"; do
    if [ "$attempt" -ge "$attempts" ]; then
      echo "error: could not pull $image after $attempts attempts" >&2
      exit 1
    fi
    delay=$((attempt * 5))
    echo "pull of $image failed (attempt $attempt/$attempts); retrying in ${delay}s" >&2
    sleep "$delay"
    attempt=$((attempt + 1))
  done
done
