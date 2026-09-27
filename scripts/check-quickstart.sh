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
#   scaffold-migrate the documented scaffold prerequisites + migrate
#                    (docs/guide/generators.md): install the Diesel CLI if
#                    missing, uncomment [database] in autumn.toml (using
#                    $DATABASE_URL as the value), `autumn db create`, then
#                    `autumn migrate` with DB env stripped.
#   scaffold-serve   `autumn dev` + poll `GET /posts` until 200 (DB config
#                    comes from autumn.toml, not env).
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
#   DATABASE_URL                  Postgres URL the scaffold-migrate phase
#                                 writes into autumn.toml's [database]
#                                 section (the documented config step; CI
#                                 provides a postgres:16 service container).
#                                 Never passed to the app or migrate as an
#                                 env var — the documented commands run with
#                                 DB env stripped.
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

# ── Pre-release detection (crates.io sparse index) ───────────────────────────
#
# During a release window the README is bumped to a new version BEFORE that
# version appears on crates.io. In that gap both `cargo install autumn-cli
# --version <new>` (install phase) and the crates.io registry-provenance
# assertion (build phase) fail through no real fault. `crate_version_published`
# lets those phases detect the window and fall back to a local source install
# ("PRE-RELEASE MODE") instead of going structurally red. When the version IS
# published (the normal case — 0.6.0 is live today) this is dormant and the
# published crates.io path runs unchanged.

sparse_index_path() {
  # crates.io sparse-index path layout (see the Cargo book, "Index files"):
  #   1 char  -> 1/{name}
  #   2 chars -> 2/{name}
  #   3 chars -> 3/{first}/{name}
  #   4+      -> {first-two}/{second-two}/{name}
  # Crate names are case-insensitive on the index; the path is lowercased.
  local name="${1,,}" n=${#1}
  case "$n" in
    1) printf '1/%s' "$name" ;;
    2) printf '2/%s' "$name" ;;
    3) printf '3/%s/%s' "${name:0:1}" "$name" ;;
    *) printf '%s/%s/%s' "${name:0:2}" "${name:2:2}" "$name" ;;
  esac
}

crate_version_published() {
  # Exit 0 iff <crate> <version> is published on crates.io, 1 if it is
  # definitely not (the crate exists but lacks that version, or the crate has
  # never been published at all → sparse-index 404). On a TRANSPORT/network
  # error we cannot tell, and both silent outcomes are dangerous: silently
  # source-installing could mask a genuinely broken published crate, while
  # silently treating it as published would reintroduce the red we are fixing.
  # So an indeterminate result is a LOUD fail() — the caller only ever branches
  # on a definitive yes/no.
  local crate="$1" version="$2" path url resp curl_rc http_code body
  path="$(sparse_index_path "$crate")"
  url="https://index.crates.io/${path}"
  # No `curl -f`: a 404 must be READABLE (published-crate-without-version vs
  # never-published), so we read the HTTP status from --write-out instead of
  # letting -f collapse every non-2xx into the same nonzero exit. --retry rides
  # out transient blips (not 404s). curl's own nonzero exit therefore means a
  # genuine transport failure, which we distinguish from a 404 below.
  if resp="$(curl -sS --max-time 20 --retry 3 --retry-delay 2 --retry-connrefused \
      --write-out $'\n%{http_code}' "$url" 2>/dev/null)"; then
    curl_rc=0
  else
    curl_rc=$?
  fi
  http_code="${resp##*$'\n'}"
  body="${resp%$'\n'*}"
  if [[ "$http_code" == "404" ]]; then
    # Definitively not published (the crate name has no index entry at all).
    return 1
  fi
  if [[ $curl_rc -ne 0 || "$http_code" != "200" ]]; then
    # Couldn't tell (network error, proxy, 5xx, rate-limit, empty status) →
    # fail loudly rather than guessing either way.
    fail "could not determine whether ${crate} ${version} is published: crates.io sparse index ${url} returned curl exit ${curl_rc}, HTTP '${http_code:-none}'"
  fi
  # The sparse-index body is newline-delimited JSON, one object per version.
  # `jq -e 'select(.vers==$v)'` streams every line and exits 0 iff some line's
  # .vers equals the requested version.
  if printf '%s\n' "$body" | jq -e --arg v "$version" 'select(.vers==$v)' >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

# Every database env override the published runtime/CLI honors must be
# stripped from the documented commands, or a var the CI job happens to
# carry could satisfy DB resolution and mask a broken documented
# no-env/TOML path. Sources (published 0.5.0): autumn-web config.rs applies
# AUTUMN_DATABASE__URL / AUTUMN_DATABASE__PRIMARY_URL /
# AUTUMN_DATABASE__REPLICA_URL as database config overrides, and
# autumn-cli's migrate resolution order is AUTUMN_DATABASE__PRIMARY_URL >
# AUTUMN_DATABASE__URL > DATABASE_URL. Add future URL-bearing vars here
# once — every `env` call site expands this list.
db_env_strip=(
  -u DATABASE_URL
  -u AUTUMN_DATABASE__URL
  -u AUTUMN_DATABASE__PRIMARY_URL
  -u AUTUMN_DATABASE__REPLICA_URL
)

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

configure_database() {
  # The DOCUMENTED way to give the app a database: the generated autumn.toml
  # ships a commented-out [database] section headed "Uncomment to configure
  # database:", and README §Database Topologies says to set database.url.
  # Perform exactly that user edit here. The documented commands themselves
  # (`autumn migrate`, `autumn dev`) then run with every supported DB env
  # override stripped (see db_env_strip), so the gate proves the file-based
  # path a clean-shell user follows actually works — env vars the CI job
  # happens to carry can never mask a broken documented step.
  local toml="$app_dir/autumn.toml"
  [[ -f "$toml" ]] || fail "generated app has no autumn.toml to configure the database in"
  if ! grep -q '^\[database\]' "$toml"; then
    # [[:space:]]*$ tolerates the CRLF line endings the generated
    # autumn.toml ships with (observed with the published autumn-cli 0.5.0);
    # a bare $ would never match after the trailing \r.
    sed -i \
      -e 's|^# \[database\][[:space:]]*$|[database]|' \
      -e "s|^# url = \"postgres://[^\"]*\"[[:space:]]*\$|url = \"${DATABASE_URL}\"|" \
      "$toml"
  fi
  grep -q '^\[database\]' "$toml" \
    || fail "could not uncomment the [database] section in the generated autumn.toml — the template changed shape; update scripts/check-quickstart.sh to match it"
  grep -q "^url = \"${DATABASE_URL}\"" "$toml" \
    || fail "could not set database.url in the generated autumn.toml — the template changed shape; update scripts/check-quickstart.sh to match it"
}

create_database() {
  # The documented database-creation step. autumn-cli gained a first-party
  # `autumn db create` subcommand before the 0.7.0 release cut, and
  # docs/guide/generators.md's "Five commands to a working CRUD app" now
  # teaches that command (matching docs/guide/getting-started.md, which
  # already did) instead of shelling out to the separately-installed
  # `createdb` (postgresql-client) binary. Run it exactly as documented, in a
  # clean shell so the database URL resolves from autumn.toml alone — the
  # same realism `autumn migrate` gets below. The CI service container
  # deliberately does NOT pre-create this database, so skipping this step
  # fails the migrate phase — exactly as it fails for a fresh user.
  (cd "$app_dir" && env "${db_env_strip[@]}" autumn db create) \
    || fail "'autumn db create' failed with database.url configured in autumn.toml (docs/guide/generators.md scaffold path)"
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
  # spawns the app server as a child process. stop_server/kill_tree tears
  # down the whole watcher → server tree.
  #
  # Every supported DB env override (db_env_strip) is ALWAYS stripped from
  # the server's environment: database configuration must come from the
  # documented user action — the [database] section in autumn.toml, written
  # by configure_database in the scaffold-migrate phase — never from env
  # vars the CI job happens to carry. That way the base serve phase
  # exercises the no-DB runtime a README user gets ([database] is still
  # commented out at that point), and the scaffold serve phase exercises
  # the documented file-based configuration in a clean shell.
  local log="$1"
  if curl -fsS -o /dev/null --max-time 2 "$base_url/" 2>/dev/null; then
    fail "port ${port} is already serving before the app started — port collision on the runner"
  fi
  (
    cd "$app_dir" || exit 1
    exec env "${db_env_strip[@]}" autumn dev
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
  step_summary "Toolchain: \`$(rustc --version 2>/dev/null | head -n 1 || echo unknown)\` — the \`stable\` leg is the canonical funnel metric; the MSRV leg guards the README's documented \`Rust 1.88.0+\` floor."
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

  # PRE-RELEASE MODE: if the README-pinned version is not yet on crates.io (a
  # release window between bumping the README and the crate publishing), install
  # from the local source tree instead of going red. A dispatch override
  # (QUICKSTART_CLI_VERSION) that IS published takes the normal path; if it names
  # an unpublished candidate it too falls back to the honest local build. This
  # publication check is the ONLY gate — when the version is published (0.6.0
  # today) the published crates.io path below is byte-for-byte unchanged.
  if ! crate_version_published autumn-cli "$version"; then
    # Persist a marker BESIDE the shared state so the later (separate-process)
    # build phase resolves the generated app against the in-tree crates too —
    # in a real pre-release window autumn-web at this version is also unpublished,
    # so the registry-provenance assertion would otherwise just move the red from
    # install → build.
    echo "$version" >"$state/.quickstart-prerelease"
    echo "::warning::PRE-RELEASE MODE: autumn-cli ${version} is not yet published on crates.io — installing autumn-cli from the local source tree (${repo_root}/autumn-cli) instead of crates.io"
    echo "installing autumn-cli ${version} from local source tree (PRE-RELEASE MODE — not yet on crates.io)"
    if ! cargo install --path "$repo_root/autumn-cli" --locked; then
      fail "'cargo install --path ${repo_root}/autumn-cli --locked' (PRE-RELEASE MODE source install) failed"
    fi
    command -v autumn >/dev/null || fail "cargo install succeeded but 'autumn' is not on PATH"
    ok "PRE-RELEASE MODE — autumn-cli ${version} not on crates.io; installed autumn-cli from local source tree"
    return
  fi

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

inject_prerelease_patch() {
  # PRE-RELEASE MODE: point the generated app's autumn-web dependency at the
  # in-tree source so it builds without needing crates.io for any autumn crate.
  # We patch ONLY autumn-web — mirroring the root Cargo.toml's
  # `[patch.crates-io] autumn-web = { path = "autumn" }`. autumn-web's sole
  # in-tree dependency, autumn-macros, is referenced in autumn-web's OWN manifest
  # by `path = "../autumn-macros"`, so patching autumn-web transitively resolves
  # autumn-macros from source too (a `path` dep is always resolved by path,
  # regardless of the patch). Adding a redundant `[patch] autumn-macros` here
  # would be an UNUSED patch (cargo would warn), because nothing in the graph
  # requests autumn-macros from crates.io. This matches how the whole workspace
  # builds today with only the autumn-web patch. The generated app depends on no
  # other autumn crate (autumn-schema-core / the plugins / storage / cache crates
  # are not in its graph).
  local toml="$app_dir/Cargo.toml"
  local marker="# >>> autumn quickstart PRE-RELEASE MODE patch (auto-injected) >>>"
  local endmarker="# <<< autumn quickstart PRE-RELEASE MODE patch <<<"
  # Idempotent: a phase rerun must not append a second [patch] table (which would
  # be a TOML duplicate-key error). phase_new already asserts the generated
  # manifest ships no [patch] section, so appending our own table is safe.
  if grep -qF "$marker" "$toml"; then
    echo "PRE-RELEASE MODE: [patch.crates-io] already present in ${toml} (idempotent rerun)"
    return 0
  fi
  {
    printf '\n%s\n' "$marker"
    printf '# PRE-RELEASE MODE: the README-pinned autumn-web is not yet on crates.io,\n'
    printf '# so redirect it (and, via autumn-web'"'"'s own path dep, autumn-macros) to\n'
    printf '# the in-tree workspace source, mirroring the root Cargo.toml.\n'
    printf '[patch.crates-io]\n'
    printf 'autumn-web = { path = "%s/autumn" }\n' "$repo_root"
    printf '%s\n' "$endmarker"
  } >>"$toml"
  echo "PRE-RELEASE MODE: injected [patch.crates-io] autumn-web -> ${repo_root}/autumn into ${toml}"
}

phase_build() {
  require_app

  # PRE-RELEASE MODE (marker written by phase_install when the README-pinned
  # version is not yet on crates.io): the same version's autumn-web is also
  # unpublished, so build the generated app against the in-tree source and relax
  # the crates.io registry-provenance assertion to a path-source check. Without a
  # marker this is the unchanged published-crates path.
  if [[ -f "$state/.quickstart-prerelease" ]]; then
    local prerelease_version
    prerelease_version="$(cat "$state/.quickstart-prerelease")"
    inject_prerelease_patch
    (cd "$app_dir" && cargo build) || fail "PRE-RELEASE MODE: 'cargo build' of the generated app failed against the in-tree autumn-web (${repo_root}/autumn)"

    local lock="$app_dir/Cargo.lock"
    [[ -f "$lock" ]] || fail "no Cargo.lock produced by the build"
    grep -q '^name = "autumn-web"$' "$lock" || fail "autumn-web missing from the generated Cargo.lock"
    local source_line
    source_line="$(awk '/^\[\[package\]\]/{found=0} /^name = "autumn-web"$/{found=1} found && /^source = /{print; exit}' "$lock")"
    # A cargo PATH dependency has NO `source =` line in Cargo.lock (empty) — or an
    # explicit `path+file://` source; a crates.io leak would carry the registry
    # source. In PRE-RELEASE MODE we REQUIRE the path source and reject the
    # registry (the inverse of the published-path assertion).
    if [[ "$source_line" == *"registry+https://github.com/rust-lang/crates.io-index"* ]]; then
      fail "PRE-RELEASE MODE: autumn-web resolved from the crates.io registry ('${source_line}') despite the [patch.crates-io] path redirect — the generated app did not build against the in-tree source"
    fi
    echo "::warning::PRE-RELEASE MODE: autumn-web ${prerelease_version} is not on crates.io — the generated app was patched to the in-tree source (${repo_root}/autumn) and the crates.io registry-provenance check was relaxed to a path-source check"
    ok "PRE-RELEASE MODE — autumn-web ${prerelease_version} built from in-tree source (${repo_root}/autumn); registry-provenance check relaxed to path-source (lockfile source: '${source_line:-none — path dependency}')"
    return
  fi

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
  # The README path up to `GET /` has no database configured — this phase
  # must exercise the no-DB runtime a README user gets. Guard the phase
  # order: the [database] section only becomes active in scaffold-migrate.
  if grep -q '^\[database\]' "$app_dir/autumn.toml" 2>/dev/null; then
    fail "autumn.toml already has an active [database] section — the base serve phase must exercise the no-DB runtime (phases run out of order?)"
  fi
  remove_prebuilt_binary
  start_server "$log"
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
  [[ -n "${DATABASE_URL:-}" ]] || fail "DATABASE_URL is not set — it supplies the Postgres URL written into autumn.toml's [database] section (CI provides a postgres:16 service container)"
  # The documented one-time prerequisite (docs/guide/generators.md):
  # `autumn migrate` delegates to the Diesel CLI. Install it with exactly
  # the documented command when missing — CI must not pre-install it, or a
  # fresh user's missing-prerequisite failure would be masked here. Sits
  # outside the funnel number, which ends at the base serve phase.
  if ! command -v diesel >/dev/null; then
    cargo install diesel_cli --no-default-features --features postgres \
      || fail "'cargo install diesel_cli --no-default-features --features postgres' failed — the documented Diesel CLI prerequisite for 'autumn migrate' does not install"
  fi
  configure_database
  create_database
  # Run the documented command in a clean shell: DB config must resolve from
  # autumn.toml alone (autumn-cli's resolve_database_url exits if no
  # TOML/env value is present — that failure must surface here, not be
  # masked by CI env vars a real user doesn't have).
  (cd "$app_dir" && env "${db_env_strip[@]}" autumn migrate) \
    || fail "'autumn migrate' failed with database.url configured in autumn.toml (docs/guide/generators.md scaffold path)"
  ok "diesel CLI present, database created, database.url configured in autumn.toml"
}

phase_scaffold_serve() {
  require_app
  grep -q '^\[database\]' "$app_dir/autumn.toml" 2>/dev/null \
    || fail "autumn.toml has no active [database] section — run the scaffold-migrate phase first"
  local log="$state/server-scaffold-serve.log"
  remove_prebuilt_binary
  start_server "$log"
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
