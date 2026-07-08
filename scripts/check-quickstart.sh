#!/usr/bin/env bash
# README-quickstart gate against the PUBLISHED crates (issue #1586).
#
# Every other CI job in this repo builds against the local workspace: the root
# Cargo.toml `[patch.crates-io]` override redirects `autumn-web` to the
# in-tree crate, so no existing test ever experiences what a brand-new user
# experiences — `cargo install autumn-cli` from crates.io generating a project
# that depends on the published `autumn-web`. This script runs the README
# quickstart verbatim, phase by phase, against crates.io.
#
# The commands are mirrored from README.md ("Quickstart") and, for the
# scaffold migrate/serve phases, docs/guide/generators.md ("Five commands to a
# working CRUD app" — the doc the README's scaffold line points at). If a
# phase here disagrees with those docs, the docs win: fix the docs (or this
# script to match them) — never paper over a broken documented step in CI.
#
# Usage:
#   scripts/check-quickstart.sh <phase>
#
# Phases, in README order (each is one CI step, so a failure is attributed to
# a named quickstart step in the Actions UI without log spelunking):
#   install          `cargo install autumn-cli --version <V>` where <V> is the
#                    exact version the README quickstart pins (or
#                    $QUICKSTART_CLI_VERSION when set — release-candidate
#                    gating). Wipes/re-creates the state dir and records the
#                    funnel start time.
#   new              `autumn new my-app`
#   setup            `autumn setup`
#   build            `cargo build`, then asserts `autumn-web` resolved from
#                    the crates.io registry (no path/patch leakage).
#   serve            `autumn dev` + poll `GET /` until 200. Records the
#                    tracked funnel number: elapsed seconds from the start of
#                    `cargo install` to the first 200 response.
#   scaffold         `autumn generate scaffold Post title:String body:Text published:bool`
#   scaffold-build   `cargo build`
#   scaffold-migrate `autumn migrate` (requires $DATABASE_URL and the diesel
#                    CLI on PATH; see docs/guide/generators.md)
#   scaffold-serve   `autumn dev` + poll `GET /posts` until 200.
#
# Environment:
#   QUICKSTART_CLI_VERSION        Override the autumn-cli version to install
#                                 (used to gate a release candidate that was
#                                 just published to crates.io). Default: the
#                                 version pinned in the README quickstart.
#   QUICKSTART_STATE_DIR          Working directory for the generated app and
#                                 phase state. Must be OUTSIDE this checkout so
#                                 the workspace [patch.crates-io] override
#                                 cannot leak into the generated project.
#                                 Default: ${RUNNER_TEMP:-/tmp}/autumn-quickstart-gate
#   QUICKSTART_PORT               Port the generated app serves on (default
#                                 3000, the README's documented port).
#   QUICKSTART_SERVE_TIMEOUT_SECS Deadline for the first response after
#                                 `autumn dev` starts (default 180).
#   DATABASE_URL                  Postgres URL for the scaffold migrate/serve
#                                 phases (CI provides a postgres:16 service
#                                 container).
set -Eeuo pipefail

phase="${1:?usage: scripts/check-quickstart.sh <install|new|setup|build|serve|scaffold|scaffold-build|scaffold-migrate|scaffold-serve>}"

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
state="${QUICKSTART_STATE_DIR:-${RUNNER_TEMP:-/tmp}/autumn-quickstart-gate}"
app_dir="$state/my-app"
port="${QUICKSTART_PORT:-3000}"
base_url="http://127.0.0.1:${port}"
serve_timeout="${QUICKSTART_SERVE_TIMEOUT_SECS:-180}"

# cargo install writes to ~/.cargo/bin; make sure we find the result even in
# minimal shells.
export PATH="$HOME/.cargo/bin:$PATH"

phase_started_at=$(date +%s)

step_summary() {
  # Append a line to the GitHub job summary; silently a no-op locally.
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    echo "$*" >>"$GITHUB_STEP_SUMMARY"
  fi
}

phase_elapsed() { echo "$(($(date +%s) - phase_started_at))"; }

fail() {
  # Single funnel-step attribution point: names the quickstart step that broke
  # both in the log (::error:: surfaces it in the Actions UI and annotations)
  # and in the job summary.
  echo "::error::Quickstart gate: step '${phase}' failed — $*" >&2
  step_summary "| \`${phase}\` | :x: failed | $(phase_elapsed)s | $* |"
  exit 1
}

on_unexpected_err() {
  local cmd="$BASH_COMMAND"
  trap - ERR
  fail "command failed: ${cmd}"
}
trap on_unexpected_err ERR

ok() {
  echo "ok: quickstart step '${phase}' passed ($(phase_elapsed)s)"
  step_summary "| \`${phase}\` | :white_check_mark: ok | $(phase_elapsed)s | $* |"
}

require_state() {
  [[ -d "$state" ]] || fail "state dir '$state' missing — run the 'install' phase first (phases run in order)"
}

require_app() {
  require_state
  [[ -d "$app_dir" ]] || fail "generated app '$app_dir' missing — run the 'new' phase first"
}

assert_outside_checkout() {
  # The [patch.crates-io] override only applies to builds rooted in this
  # workspace; keeping the generated app outside the checkout guarantees it
  # resolves autumn-web from crates.io like a real user.
  case "$state/" in
    "$repo_root"/*) fail "QUICKSTART_STATE_DIR ($state) is inside the checkout — the workspace [patch.crates-io] override would leak into the generated project" ;;
  esac
}

readme_cli_version() {
  sed -n 's/^cargo install autumn-cli --version \([0-9][0-9A-Za-z.+-]*\).*$/\1/p' "$repo_root/README.md" | head -n 1
}

remove_prebuilt_binary() {
  # README users go straight from `autumn setup` to `autumn dev` — the
  # quickstart has no discrete build step. Our build phases run `cargo build`
  # anyway (for step attribution and the lockfile source assertion), which
  # leaves a target/debug/my-app binary on disk — and `autumn dev`
  # (autumn-cli/src/dev.rs) only LOGS an initial-build failure before
  # starting whatever binary find_binary locates, so a stale prebuilt binary
  # would mask a regression in the published CLI's dev build path. Delete it
  # so `autumn dev` must build and start the app itself, exactly as it must
  # for a new user. (Dependency artifacts stay cached; only the app binary
  # is removed, so funnel timing is barely affected.)
  rm -f "${CARGO_TARGET_DIR:-$app_dir/target}/debug/my-app"
}


# ── Server lifecycle ─────────────────────────────────────────────────────────

kill_tree() {
  # Depth-first signal delivery to a whole process tree ($1 = root pid,
  # $2 = signal, default TERM).
  local pid="$1" sig="${2:-TERM}" child
  for child in $(pgrep -P "$pid" 2>/dev/null || true); do
    kill_tree "$child" "$sig"
  done
  kill -s "$sig" "$pid" 2>/dev/null || true
}

start_server() {
  # `autumn dev` is the README quickstart's run command (and how the
  # generators guide ends the scaffold path), so the gate starts the app
  # exactly that way — a regression in the published CLI's dev
  # startup/watch path would otherwise slip past the gate while every new
  # user hits it. Verified to run headless (no tty needed); the watcher
  # spawns the app server as a child process, and env vars set here pass
  # through to it. stop_server/kill_tree tears down the whole
  # watcher → server tree.
  #
  # $2 selects the runtime configuration the README path actually exercises:
  #   no-db    The quickstart up to `GET /` configures no database, so the
  #            server must run with NO DB env at all — strip DATABASE_URL /
  #            AUTUMN_DATABASE__URL even if the CI job env carries them,
  #            otherwise a regression that only affects the no-DB runtime
  #            could slip through while the gate exercises a DB-configured
  #            app.
  #   with-db  The scaffolded /posts route needs Postgres. The generated
  #            autumn.toml ships with [database] commented out, and the
  #            runtime configures no pool from a bare DATABASE_URL on a
  #            fresh app; injecting the URL through the framework's
  #            documented `AUTUMN_*` config-override env var stands in for
  #            the "uncomment [database] in autumn.toml" step — it does not
  #            change any README command.
  local log="$1" db_mode="$2"
  if curl -fsS -o /dev/null --max-time 2 "$base_url/" 2>/dev/null; then
    fail "port ${port} is already serving before the app started — port collision on the runner"
  fi
  (
    cd "$app_dir" || exit 1
    if [[ "$db_mode" == "with-db" ]]; then
      export AUTUMN_DATABASE__URL="$DATABASE_URL"
      exec autumn dev
    else
      exec env -u DATABASE_URL -u AUTUMN_DATABASE__URL autumn dev
    fi
  ) >"$log" 2>&1 &
  server_pid=$!
  echo "$server_pid" >"$state/server.pid"
}

stop_server() {
  local pid
  pid="$(cat "$state/server.pid" 2>/dev/null || true)"
  [[ -n "$pid" ]] || return 0
  kill_tree "$pid" TERM
  local i
  for i in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.5
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill_tree "$pid" KILL
  fi
  rm -f "$state/server.pid"
}

wait_for_200() {
  local url="$1" log="$2" deadline code
  deadline=$(($(date +%s) + serve_timeout))
  # The contract is literally "a 200 from the route": test the status code —
  # `curl -f` alone would also accept a 3xx redirect as success.
  while :; do
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$url" 2>/dev/null || true)"
    if [[ "$code" == "200" ]]; then
      break
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
      echo "---- server log tail ($log) ----" >&2
      tail -n 60 "$log" >&2 || true
      fail "the app process exited before answering ${url} — see the server log tail above"
    fi
    if (($(date +%s) >= deadline)); then
      echo "---- server log tail ($log) ----" >&2
      tail -n 60 "$log" >&2 || true
      stop_server
      fail "no 200 from ${url} within ${serve_timeout}s of 'autumn dev' (last status: ${code:-none}) — see the server log tail above"
    fi
    sleep 1
  done
}

# ── Phases ───────────────────────────────────────────────────────────────────

phase_install() {
  # Job-summary preamble FIRST, before anything in this phase can fail, so
  # every failure row lands under the table header. An honest note on
  # semantics: a push-triggered run validates the README against what is on
  # crates.io — not the pushed code.
  step_summary "## Quickstart Gate — README vs published crates"
  step_summary ""
  step_summary "Installs the README-pinned \`autumn-cli\` from crates.io (or the \`cli-version\` dispatch input) and runs the README quickstart verbatim."
  step_summary "This validates the published crates against the README at this commit — it does **not** exercise the pushed code (the workspace \`[patch.crates-io]\` override means no other CI job sees what a new user installs)."
  step_summary ""
  step_summary "| Phase | Result | Duration | Notes |"
  step_summary "|---|---|---|---|"

  assert_outside_checkout
  local version="${QUICKSTART_CLI_VERSION:-}"
  if [[ -z "$version" ]]; then
    version="$(readme_cli_version)"
    [[ -n "$version" ]] || fail "could not parse 'cargo install autumn-cli --version X.Y.Z' out of README.md — the quickstart changed shape; update scripts/check-quickstart.sh to match it"
  fi

  # Fresh state per gate run.
  rm -rf "$state"
  mkdir -p "$state"

  # Funnel start: the tracked install→first-200 number begins at the moment a
  # new user types `cargo install`.
  date +%s >"$state/t_install_start"
  echo "$version" >"$state/cli_version"

  echo "installing autumn-cli ${version} from crates.io (README quickstart step 1)"
  if ! cargo install autumn-cli --version "$version"; then
    fail "'cargo install autumn-cli --version ${version}' failed — the README-pinned CLI version does not install from crates.io"
  fi
  command -v autumn >/dev/null || fail "cargo install succeeded but 'autumn' is not on PATH"
  ok "autumn-cli ${version}"
}

phase_new() {
  require_state
  assert_outside_checkout
  rm -rf "$app_dir"
  (cd "$state" && autumn new my-app) || fail "'autumn new my-app' failed"
  [[ -f "$app_dir/Cargo.toml" ]] || fail "'autumn new my-app' exited 0 but produced no $app_dir/Cargo.toml"

  # Published-crate assertions on the generated manifest: a real user gets no
  # path deps and no patch section. The grep covers the inline
  # `autumn-web = { ..., path = ... }` form (anchored so commented-out lines
  # can't match, and `[^#]` so a trailing comment can't). The awk covers the
  # two-line `[dependencies.autumn-web]` + `path = ...` table form,
  # section-scoped (reset at the next `[` header) so a `path =` in a
  # neighboring section can't false-positive. Note the awk hit flag: `exit 0`
  # in a body rule still runs END, whose exit status would override it.
  # These are an early, readable tripwire — the authoritative backstop is
  # the post-build lockfile source assertion in the 'build' phase.
  if grep -q '^\[patch' "$app_dir/Cargo.toml"; then
    fail "generated Cargo.toml contains a [patch] section — the project would not build against the published autumn-web"
  fi
  if grep -q -E '^[[:space:]]*autumn-web[^=]*=[^#]*path[[:space:]]*=' "$app_dir/Cargo.toml" \
    || awk '/^\[(dev-|build-)?dependencies\.autumn-web\]/{found=1; next} /^\[/{found=0} found && /^[[:space:]]*path[[:space:]]*=/{hit=1; exit} END{exit !hit}' "$app_dir/Cargo.toml"; then
    fail "generated Cargo.toml references autumn-web by path — the project would not build against the published autumn-web"
  fi
  ok
}

phase_setup() {
  require_app
  (cd "$app_dir" && autumn setup) || fail "'autumn setup' failed (Tailwind download/toolchain setup)"
  ok
}

phase_build() {
  require_app
  (cd "$app_dir" && cargo build) || fail "'cargo build' of the freshly generated app failed against the published autumn-web"

  # The whole point of this gate: prove the app compiled against the crates.io
  # release, not a leaked local path.
  local lock="$app_dir/Cargo.lock"
  [[ -f "$lock" ]] || fail "no Cargo.lock produced by the build"
  local source_line
  # Reset `found` at every [[package]] boundary: a path-dependency autumn-web
  # has NO `source` line at all, so without the reset awk would print the
  # NEXT package's registry source and false-pass in exactly the leak
  # scenario this assertion exists to catch.
  source_line="$(awk '/^\[\[package\]\]/{found=0} /^name = "autumn-web"$/{found=1} found && /^source = /{print; exit}' "$lock")"
  if [[ "$source_line" != *"registry+https://github.com/rust-lang/crates.io-index"* ]]; then
    fail "autumn-web did not resolve from the crates.io registry (lockfile source: '${source_line:-none — path dependency}') — local workspace leaked into the quickstart"
  fi
  ok "autumn-web from crates.io: yes"
}

phase_serve() {
  require_app
  local log="$state/server-serve.log"
  # The README path up to `GET /` has no database configured — serve in
  # no-db mode so this phase exercises the same runtime a README user gets.
  remove_prebuilt_binary
  start_server "$log" no-db
  wait_for_200 "$base_url/" "$log"
  local t200 t_install elapsed
  t200=$(date +%s)
  # Stop the server before any fail() below can exit — otherwise the
  # autumn dev tree would be leaked holding port ${port}.
  stop_server
  t_install="$(cat "$state/t_install_start" 2>/dev/null || true)"
  [[ -n "$t_install" ]] || fail "missing funnel start time — the 'install' phase did not run"
  elapsed=$((t200 - t_install))
  echo "$elapsed" >"$state/install_to_first_200_secs"
  echo "::notice::Quickstart funnel: install → first 200 from GET / took ${elapsed}s"
  step_summary "| \`${phase}\` | :white_check_mark: ok | $(phase_elapsed)s | GET / returned 200 |"
  step_summary ""
  step_summary "**Funnel: \`cargo install\` → first 200 from \`GET /\` took \`${elapsed}s\`** (tracked number, target < 300s)."
  step_summary ""
  step_summary "| Phase | Result | Duration | Notes |"
  step_summary "|---|---|---|---|"
  echo "ok: quickstart step 'serve' passed ($(phase_elapsed)s); install→first-200 = ${elapsed}s"
}

phase_scaffold() {
  require_app
  # Exactly the README's scaffold line.
  (cd "$app_dir" && autumn generate scaffold Post title:String body:Text published:bool) \
    || fail "'autumn generate scaffold Post title:String body:Text published:bool' failed"
  ok
}

phase_scaffold_build() {
  require_app
  (cd "$app_dir" && cargo build) || fail "'cargo build' of the scaffolded app failed against the published autumn-web"
  ok
}

phase_scaffold_migrate() {
  require_app
  [[ -n "${DATABASE_URL:-}" ]] || fail "DATABASE_URL is not set — the scaffold migrate/serve phases need a Postgres (CI provides a postgres:16 service container)"
  command -v diesel >/dev/null || fail "the diesel CLI is not on PATH — 'autumn migrate' shells out to it (cargo install diesel_cli --no-default-features --features postgres)"
  (cd "$app_dir" && autumn migrate) || fail "'autumn migrate' failed (docs/guide/generators.md scaffold path)"
  ok
}

phase_scaffold_serve() {
  require_app
  [[ -n "${DATABASE_URL:-}" ]] || fail "DATABASE_URL is not set — the scaffolded /posts route needs a Postgres"
  local log="$state/server-scaffold-serve.log"
  remove_prebuilt_binary
  start_server "$log" with-db
  # docs/guide/generators.md: "Visit http://localhost:3000/posts to see the
  # generated index page."
  wait_for_200 "$base_url/posts" "$log"
  stop_server
  ok "GET /posts returned 200"
}

case "$phase" in
  install)          phase_install ;;
  new)              phase_new ;;
  setup)            phase_setup ;;
  build)            phase_build ;;
  serve)            phase_serve ;;
  scaffold)         phase_scaffold ;;
  scaffold-build)   phase_scaffold_build ;;
  scaffold-migrate) phase_scaffold_migrate ;;
  scaffold-serve)   phase_scaffold_serve ;;
  *) fail "unknown phase '${phase}'" ;;
esac
