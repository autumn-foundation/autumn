#!/usr/bin/env bash
# Driver for the Autumn framework: brings up a real Autumn app and drives it.
#
# Agent tooling, not product code. Paths are relative to the workspace root.
#
#   ./.claude/skills/run-autumn/driver.sh all           # todo-app, full DB stack
#   ./.claude/skills/run-autumn/driver.sh all --example hello   # no DB, ~40s
#   ./.claude/skills/run-autumn/driver.sh smoke         # re-run assertions only
#   ./.claude/skills/run-autumn/driver.sh shot /todos   # screenshot one path
#   ./.claude/skills/run-autumn/driver.sh down          # stop app + drop container
#
# Ports are deliberately NOT 3000/5432 — those collide with whatever else the
# dev box is running. Override with APP_PORT / PG_PORT.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT" || exit 1

EXAMPLE="${EXAMPLE:-todo-app}"
APP_PORT="${APP_PORT:-3100}"
PG_PORT="${PG_PORT:-55432}"
PG_NAME="${PG_NAME:-autumn-run-pg}"
OUT="${OUT:-$ROOT/target/autumn-run}"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"

# Parse trailing flags.
ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --example) EXAMPLE="$2"; shift 2 ;;
    --port)    APP_PORT="$2"; shift 2 ;;
    *)         ARGS+=("$1"); shift ;;
  esac
done
set -- "${ARGS[@]+"${ARGS[@]}"}"

CMD="${1:-all}"; shift 2>/dev/null || true

DB_URL="postgres://autumn:autumn@localhost:${PG_PORT}/todos"
BASE="http://127.0.0.1:${APP_PORT}"
PIDFILE="$OUT/$EXAMPLE.pid"
LOG="$OUT/$EXAMPLE.log"
mkdir -p "$OUT"

# Examples that need Postgres. `hello` and friends run with no DB at all.
needs_db() { [ -f "$ROOT/examples/$EXAMPLE/docker-compose.yml" ]; }

say()  { printf '\033[1;33m==>\033[0m %s\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m   %s\n' "$*"; }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILED=$((FAILED+1)); }
FAILED=0

# ── db ────────────────────────────────────────────────────────────────────────

cmd_db() {
  needs_db || { say "example '$EXAMPLE' needs no database — skipping"; return 0; }
  if [ -n "$(docker ps -q -f name="^${PG_NAME}$")" ]; then
    say "postgres container '$PG_NAME' already running on :$PG_PORT"
  else
    docker rm -f "$PG_NAME" >/dev/null 2>&1
    say "starting postgres:16 as '$PG_NAME' on :$PG_PORT"
    docker run -d --name "$PG_NAME" \
      -e POSTGRES_DB=todos -e POSTGRES_USER=autumn -e POSTGRES_PASSWORD=autumn \
      -p "${PG_PORT}:5432" postgres:16 >/dev/null || return 1
  fi
  for _ in $(seq 1 60); do
    docker exec "$PG_NAME" pg_isready -U autumn -d todos >/dev/null 2>&1 && { ok "postgres ready"; return 0; }
    sleep 1
  done
  bad "postgres never became ready"; return 1
}

# The app auto-migrates on dev startup under a *collision-substituted* version,
# so a database that `autumn migrate` already touched makes the app re-run
# 00000000000000_create_todos and die on `relation "todos" already exists`.
# Always hand the app a virgin database.
cmd_reset() {
  needs_db || return 0
  say "recreating database 'todos' (app owns its own migrations)"
  docker exec "$PG_NAME" psql -U autumn -d postgres \
    -c 'DROP DATABASE IF EXISTS todos WITH (FORCE);' \
    -c 'CREATE DATABASE todos OWNER autumn;' >/dev/null 2>&1 \
    && ok "database reset" || bad "database reset failed"
}

# ── build ─────────────────────────────────────────────────────────────────────

cmd_build() {
  # Tailwind is fetched once into target/autumn/ by `autumn setup`. Without it
  # the example's build.rs prints a warning and emits NO css — the page renders
  # unstyled and a screenshot looks broken.
  if [ ! -x "$ROOT/target/autumn/tailwindcss" ]; then
    say "fetching Tailwind CLI (autumn setup, ~60s)"
    cargo run -q -p autumn-cli -- setup || return 1
  fi
  # build.rs only reruns on src/ or input.css changes, so a binary built before
  # `autumn setup` stays CSS-less until we poke the input.
  [ -f "$ROOT/examples/$EXAMPLE/static/css/input.css" ] && \
    touch "$ROOT/examples/$EXAMPLE/static/css/input.css"
  say "cargo build -p $EXAMPLE"
  cargo build -p "$EXAMPLE" --bins || return 1
  ok "built target/debug/$EXAMPLE"
}

# ── run ───────────────────────────────────────────────────────────────────────

cmd_stop() {
  [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null && say "stopped pid $(cat "$PIDFILE")"
  rm -f "$PIDFILE"
  pkill -f "target/debug/$EXAMPLE" 2>/dev/null
  return 0
}

cmd_start() {
  cmd_stop
  say "launching $EXAMPLE on :$APP_PORT"
  # Run from the example dir: autumn.toml is discovered relative to CWD.
  # AUTUMN_SERVER__PORT / AUTUMN_DATABASE__URL are the env override layer and
  # win over autumn.toml, which is how we dodge the 3000/5432 collisions.
  (
    cd "$ROOT/examples/$EXAMPLE" || exit 1
    if needs_db; then
      AUTUMN_SERVER__PORT="$APP_PORT" AUTUMN_DATABASE__URL="$DB_URL" \
        "$ROOT/target/debug/$EXAMPLE" > "$LOG" 2>&1 &
    else
      AUTUMN_SERVER__PORT="$APP_PORT" "$ROOT/target/debug/$EXAMPLE" > "$LOG" 2>&1 &
    fi
    echo $! > "$PIDFILE"
  )
  for _ in $(seq 1 80); do
    curl -sf "$BASE/health" >/dev/null 2>&1 && { ok "healthy: $(curl -s "$BASE/health")"; return 0; }
    sleep 0.5
  done
  bad "app never became healthy — last 30 log lines:"; tail -30 "$LOG"; return 1
}

cmd_seed() {
  needs_db || return 0
  say "seeding"
  AUTUMN_DATABASE__URL="$DB_URL" cargo run -q -p autumn-cli -- seed --package "$EXAMPLE" 2>&1 | tail -2
}

# ── smoke ─────────────────────────────────────────────────────────────────────

expect() { # expect <label> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected '$2', got '$3')"; fi
}
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

cmd_smoke() {
  say "smoking $EXAMPLE at $BASE"
  expect "GET /health"        200 "$(code "$BASE/health")"
  expect "GET / (following redirects)" 200 "$(code -L "$BASE/")"
  expect "GET /actuator/info" 200 "$(code "$BASE/actuator/info")"

  if ! needs_db; then
    expect "GET /hello/Mark body" "Hello, Mark!" "$(curl -s "$BASE/hello/Mark")"
    return 0
  fi

  expect "GET /todos"          200 "$(code "$BASE/todos")"
  expect "GET /api/todos 401 without a token" 401 "$(code "$BASE/api/todos")"

  local tok before after id
  tok=$(curl -s -X POST "$BASE/api/tokens" -H 'content-type: application/json' \
        -d '{"principal_id":"run-autumn-driver"}')
  [ -n "$tok" ] && ok "issued bearer token (${tok:0:12}…)" || bad "token issuance returned empty"

  expect "GET /api/todos 200 with token" 200 \
    "$(code "$BASE/api/todos" -H "Authorization: Bearer $tok")"

  before=$(curl -s "$BASE/todos/summary" -H 'Accept: application/json' | jq -r .total)
  id=$(curl -s -X POST "$BASE/api/todos" -H "Authorization: Bearer $tok" \
       -H 'content-type: application/json' \
       -d '{"title":"created by run-autumn driver"}' | jq -r .id)
  after=$(curl -s "$BASE/todos/summary" -H 'Accept: application/json' | jq -r .total)
  [ "$after" = "$((before + 1))" ] && ok "POST /api/todos created id=$id (${before} -> ${after})" \
    || bad "create did not change summary total (${before} -> ${after})"

  # htmx: toggle returns an <li> fragment, not a redirect.
  curl -s -X POST "$BASE/todos/$id/toggle" -H 'HX-Request: true' | grep -q "id=\"todo-$id\"" \
    && ok "POST /todos/$id/toggle returned an htmx <li> fragment" \
    || bad "toggle fragment missing id=\"todo-$id\""

  # Content negotiation: one handler, two representations.
  curl -s "$BASE/todos/summary" -H 'Accept: application/json' | jq -e .total >/dev/null \
    && ok "GET /todos/summary negotiates JSON" || bad "summary JSON negotiation"
  curl -s "$BASE/todos/summary" -H 'Accept: text/html' | grep -qi '<' \
    && ok "GET /todos/summary negotiates HTML" || bad "summary HTML negotiation"

  # Tracked background job -> polled progress -> download payload.
  local frag job
  frag=$(curl -s -X POST "$BASE/todos/export" -H 'HX-Request: true' \
         | sed -n 's/.*hx-get="\([^"]*\)".*/\1/p')
  if [ -n "$frag" ]; then
    for _ in $(seq 1 15); do
      job=$(curl -s "$BASE$frag")
      echo "$job" | grep -q succeeded && break
      sleep 1
    done
    echo "$job" | grep -q '"status":"succeeded"' \
      && ok "CSV export job reached succeeded via $frag" \
      || bad "export job never succeeded: $(echo "$job" | head -c 120)"
  else
    bad "POST /todos/export returned no polling fragment"
  fi

  # MCP: the same #[api_doc(mcp)] routes projected as agent tools.
  curl -s -X POST "$BASE/mcp" -H "Authorization: Bearer $tok" \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
    | jq -e '.result.tools | length > 0' >/dev/null \
    && ok "POST /mcp tools/list projected $(curl -s -X POST "$BASE/mcp" -H "Authorization: Bearer $tok" -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq -r '[.result.tools[].name]|join(", ")')" \
    || bad "MCP tools/list returned no tools"

  expect "GET /_autumn/inspect (dev inspector)" 200 "$(code "$BASE/_autumn/inspect")"
  return 0
}

# ── screenshot ────────────────────────────────────────────────────────────────

cmd_shot() {
  local path="${1:-/todos}" name
  name=$(echo "${EXAMPLE}${path}" | tr -c 'a-zA-Z0-9' '-' | sed 's/--*/-/g;s/-$//')
  local png="$OUT/$name.png"
  rm -f "$png"
  [ -x "$CHROME" ] || { bad "no Chrome at $CHROME — set CHROME=..."; return 1; }
  say "screenshotting $BASE$path"
  # Headless Chrome writes the png and then does NOT exit here, so background
  # it, wait for the file to stop growing, and kill it.
  "$CHROME" --headless --disable-gpu --hide-scrollbars --no-first-run \
    --user-data-dir="$OUT/chrome-profile" --virtual-time-budget=5000 \
    --window-size=1280,900 --screenshot="$png" "$BASE$path" >/dev/null 2>&1 &
  local cpid=$!
  for _ in $(seq 1 40); do [ -s "$png" ] && break; sleep 0.5; done
  sleep 1; kill $cpid 2>/dev/null; wait $cpid 2>/dev/null
  if [ -s "$png" ]; then ok "wrote $png ($(wc -c < "$png" | tr -d ' ') bytes)"; echo "$png"
  else bad "no screenshot produced"; return 1; fi
}

cmd_down() { cmd_stop; docker rm -f "$PG_NAME" >/dev/null 2>&1 && say "removed $PG_NAME"; return 0; }

cmd_all() {
  cmd_db    || return 1
  cmd_reset
  cmd_build || return 1
  cmd_start || return 1
  cmd_seed
  cmd_smoke
  needs_db && cmd_shot /todos || cmd_shot /
  echo
  if [ "$FAILED" -eq 0 ]; then say "ALL CHECKS PASSED — app still running at $BASE (driver.sh down to stop)"
  else say "$FAILED CHECK(S) FAILED — log: $LOG"; return 1; fi
}

case "$CMD" in
  db|reset|build|start|stop|seed|smoke|shot|down|all) "cmd_$CMD" "$@" ;;
  *) sed -n '2,14p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
exit $(( FAILED > 0 ? 1 : 0 ))
