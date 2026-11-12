#!/usr/bin/env bash
# Put the PostgreSQL 16 client tools first on PATH for the later CI steps.
#
#     ./scripts/ci-pg-client-16.sh
#
# The Docker tests start `postgres:11-alpine` and `postgres:16-alpine` servers
# and restore into them with the host `pg_restore`. `pg_restore` 17 and later
# sends `SET transaction_timeout = 0`, which a server older than 17 refuses, so
# the restore exits 1. Ubuntu 26.04 runners ship client 18. Client 16 is the
# newest that restores into every server the tests start.
#
# apt runs only when client 16 is missing and the client on PATH is newer than
# 16 or absent. If apt fails (a self-hosted runner without passwordless sudo,
# or a mirror outage), the step warns and the tests use the client on PATH.
# PG_CLIENT_16_BIN overrides the client 16 directory.
set -euo pipefail

bin=${PG_CLIENT_16_BIN:-/usr/lib/postgresql/16/bin}

major() {
  "$1" --version 2>/dev/null | grep -oE '[0-9]+' | head -n 1
}

if [ ! -x "$bin/pg_restore" ]; then
  current=$(major pg_restore || true)
  if [ -z "$current" ] || [ "$current" -gt 16 ]; then
    if ! { sudo apt-get update && sudo apt-get install -y postgresql-client-16; }; then
      echo "::warning::could not install postgresql-client-16; the tests use pg_restore ${current:-(none)}"
    fi
  fi
fi

if [ -x "$bin/pg_restore" ]; then
  echo "$bin" >> "${GITHUB_PATH:-/dev/null}"
  export PATH="$bin:$PATH"
fi

pg_dump --version
pg_restore --version
