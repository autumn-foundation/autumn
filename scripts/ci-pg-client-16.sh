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
# 16 or absent. The runner image installs client 18 from the PGDG repository
# and then removes that repository, and Ubuntu 26.04 has no client 16. So when
# apt cannot find the package, the PGDG helper from postgresql-common adds the
# repository first.
#
# If the install fails, a GitHub-hosted runner fails the step. A self-hosted
# runner (no passwordless sudo) gets a warning and keeps the client on PATH.
# PG_CLIENT_16_BIN overrides the client 16 directory.
set -euo pipefail

bin=${PG_CLIENT_16_BIN:-/usr/lib/postgresql/16/bin}

major() {
  "$1" --version 2>/dev/null | grep -oE '[0-9]+' | head -n 1
}

install_client_16() {
  local helper=/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh
  sudo apt-get update || return 1
  if ! apt-cache show postgresql-client-16 >/dev/null 2>&1; then
    [ -x "$helper" ] || return 1
    # Adds the PGDG source for this release and runs apt-get update.
    sudo "$helper" -y || return 1
  fi
  sudo apt-get install -y postgresql-client-16
}

if [ ! -x "$bin/pg_restore" ]; then
  current=$(major pg_restore || true)
  if [ -z "$current" ] || [ "$current" -gt 16 ]; then
    if ! install_client_16; then
      message="could not install postgresql-client-16; the tests use pg_restore ${current:-(none)}"
      if [ "${RUNNER_ENVIRONMENT:-}" = "github-hosted" ]; then
        echo "::error::$message"
        exit 1
      fi
      echo "::warning::$message"
    fi
  fi
fi

if [ -x "$bin/pg_restore" ]; then
  echo "$bin" >> "${GITHUB_PATH:-/dev/null}"
  export PATH="$bin:$PATH"
fi

pg_dump --version
pg_restore --version
