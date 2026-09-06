#!/usr/bin/env bash
# Verify that no workspace manifest enables autumn-web's `sqlite` feature
# through a dependency edge (issue #1905 / #2539 §3).
#
# WHAT THE INVARIANT IS
#   `sqlite` is a BACKEND-FLIP feature: it swaps `db::RuntimeConnection` from
#   `AsyncPgConnection` to `SyncConnectionWrapper<SqliteConnection>`. Cargo
#   feature unification is global, so ONE edge — `autumn-web = { …, features =
#   ["sqlite"] }` in any crate or dev-dependency, or a feature that forwards
#   `autumn-web/sqlite` — flips the connection type for EVERY consumer in the
#   graph and breaks the Postgres default build. The feature is meant to be
#   turned on only by an end application or an explicit `--features sqlite`
#   invocation.
#
# WHY A SCRIPT
#   Until now the invariant was prose (autumn/Cargo.toml, autumn/src/db.rs), a
#   `sqlite`-excluding feature list in ci.yml, and review. Nothing read the
#   manifests. A dev-dependency added tomorrow would surface only if some
#   Postgres-assuming crate happened to fail to compile in a lane that runs —
#   and `scripts/pre-push-check.sh` skips the sqlite lane entirely.
#
# WHAT IT CHECKS  (every `Cargo.toml` in the tree, `target/` aside)
#   1. No dependency, dev-dependency or build-dependency edge on `autumn-web`
#      or `autumn-cli` lists `sqlite` in its `features`. Covers the inline form
#      (`autumn-web = { features = ["sqlite"] }`) and the section form
#      (`[dev-dependencies.autumn-web]` + `features = [...]`).
#   2. No `[features]` entry forwards `autumn-web/sqlite` / `autumn-cli/sqlite`
#      unless the entry is ITSELF named `sqlite`. That single exception is
#      autumn-cli's own opt-in backend (`sqlite = ["autumn-web/sqlite", …]`),
#      which is selected the same explicit way autumn-web's is.
#   3. No `default` feature list enables `sqlite`, bare or forwarded.
#
# It is a manifest gate, not a build: no toolchain, ~1 second, self-testing.
#
# Usage:
#   ./scripts/check-sqlite-unification.sh              # self-test, then check
#   ./scripts/check-sqlite-unification.sh --self-test  # self-test only
#   ./scripts/check-sqlite-unification.sh --check-only  # check only

set -euo pipefail

die() {
  echo "ERROR: $*" >&2
  exit 1
}

# Crates whose `sqlite` feature is the backend flip. An edge that enables
# `sqlite` on either one flips the whole graph.
FLIP_CRATES='autumn-web|autumn-cli'

# ---------------------------------------------------------------------------
# The checker. Prints one line per violation on stdout; exits 0 either way so
# callers decide. `$1` is the manifest to scan.
# ---------------------------------------------------------------------------
scan_manifest() {
  local manifest="$1"
  awk -v flip="$FLIP_CRATES" '
    # Join a logical entry that spans lines: keep appending until every
    # bracket and brace opened on the line has closed. A `features` array
    # written one element per line is the common shape, and a line-at-a-time
    # scan would miss it entirely.
    function balanced(s,   i, c, depth) {
      depth = 0
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (c == "[" || c == "{") depth++
        else if (c == "]" || c == "}") depth--
      }
      return depth <= 0
    }
    function strip_comment(s,   i, c, inq, out) {
      inq = 0; out = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (c == "\"") inq = !inq
        if (c == "#" && !inq) break
        out = out c
      }
      return out
    }
    function report(msg) { printf "%s:%d: %s\n", FILENAME, entry_line, msg }

    {
      line = strip_comment($0)
      if (pending != "") {
        pending = pending " " line
        if (!balanced(pending)) next
        entry = pending; pending = ""
      } else {
        gsub(/^[ \t]+|[ \t]+$/, "", line)
        if (line == "") next
        # A table header ends any entry and selects the context.
        if (line ~ /^\[/) {
          section = line
          next
        }
        if (!balanced(line)) { pending = line; entry_line = FNR; next }
        entry = line; entry_line = FNR
      }

      # ── Context ───────────────────────────────────────────────────────
      # Dependency tables, including target-specific and section forms.
      is_dep_table  = (section ~ /(^\[|\.)(dependencies|dev-dependencies|build-dependencies)\]$/)
      # `[dependencies.autumn-web]` — the crate is in the header, not the key.
      dep_section_crate = ""
      if (match(section, /(^\[|\.)(dependencies|dev-dependencies|build-dependencies)\.[A-Za-z0-9_-]+\]$/)) {
        dep_section_crate = section
        sub(/\]$/, "", dep_section_crate)
        sub(/.*\./, "", dep_section_crate)
      }
      is_features_table = (section == "[features]")

      # ── 1. A dependency edge that enables the flip ────────────────────
      # Named by key, or renamed with an explicit `package = "autumn-web"`.
      if (is_dep_table && entry ~ /"sqlite"/ \
          && (entry ~ ("^(" flip ")[ \t]*=") || entry ~ ("package[ \t]*=[ \t]*\"(" flip ")\""))) {
        report("dependency edge enables the `sqlite` backend flip")
        next
      }
      if (dep_section_crate != "" && dep_section_crate ~ ("^(" flip ")$") \
          && entry ~ /^features[ \t]*=/ && entry ~ /"sqlite"/) {
        report("dependency edge enables the `sqlite` backend flip")
        next
      }

      # ── 2 & 3. A feature that forwards the flip ───────────────────────
      if (is_features_table) {
        key = entry
        sub(/[ \t]*=.*$/, "", key)
        forwards = (entry ~ ("\"(" flip ")/sqlite\""))
        # A bare "sqlite" element only means the flip inside the autumn-web
        # manifest, where `sqlite` is the feature being defined.
        bare = (FILENAME ~ /(^|\/)autumn\/Cargo\.toml$/ && entry ~ /"sqlite"/)
        if (key == "default" && (forwards || bare)) {
          report("`default` enables the `sqlite` backend flip")
        } else if (forwards && key != "sqlite") {
          report("feature `" key "` forwards the `sqlite` backend flip")
        }
      }
    }
  ' "$manifest"
}

# Scan every manifest under `$1`. Prints violations; returns 1 if any.
gate_check() {
  local root="$1"
  local findings
  findings="$(
    find "$root" -name Cargo.toml -not -path '*/target/*' -print0 |
      sort -z |
      xargs -0 -I{} "$0" --scan-one {}
  )"
  if [[ -n "$findings" ]]; then
    printf '%s\n' "$findings"
    return 1
  fi
  return 0
}

run_real_check() {
  local root
  root="$(cd "$(dirname "$0")/.." && pwd)"
  echo "==> scanning workspace manifests for a \`sqlite\` backend-flip edge"
  if gate_check "$root"; then
    echo "OK: no manifest enables the \`sqlite\` feature through a dependency edge."
  else
    die "a manifest enables the \`sqlite\` backend flip.

\`sqlite\` swaps db::RuntimeConnection for the WHOLE dependency graph, so a
single edge breaks every Postgres consumer. Build the SQLite lane with an
explicit invocation instead:

    cargo build -p autumn-web --features sqlite
    cargo build -p autumn-cli --no-default-features --features sqlite

See the \`sqlite = [...]\` comment in autumn/Cargo.toml."
  fi
}

# ---------------------------------------------------------------------------
# Self-test: prove the checker still catches what it claims to.
# ---------------------------------------------------------------------------
self_test() {
  local tmp pass=0 total=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  make_case() {
    local dir="$tmp/$1"
    mkdir -p "$dir"
    cat >"$dir/Cargo.toml"
  }

  check_fail() {
    local name="$1" dir="$2"
    total=$((total + 1))
    if gate_check "$tmp/$dir" >/dev/null 2>&1; then
      echo "  FAIL: $name — violation not caught"
    else
      pass=$((pass + 1))
    fi
  }

  check_pass() {
    local name="$1" dir="$2"
    total=$((total + 1))
    if gate_check "$tmp/$dir" >/dev/null 2>&1; then
      pass=$((pass + 1))
    else
      echo "  FAIL: $name — legitimate manifest rejected"
    fi
  }

  make_case inline <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = ["db", "sqlite"] }
EOF
  check_fail "inline dependency edge" inline

  make_case dev <<'EOF'
[dev-dependencies]
autumn-web = { path = "../autumn", features = ["sqlite"] }
EOF
  check_fail "dev-dependency edge" dev

  make_case multiline <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = [
    "db",
    "sqlite",
] }
EOF
  check_fail "multi-line features array" multiline

  make_case section <<'EOF'
[dev-dependencies.autumn-web]
path = "../autumn"
features = ["sqlite"]
EOF
  check_fail "section-form dependency table" section

  make_case target_dep <<'EOF'
[target.'cfg(unix)'.dependencies]
autumn-cli = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "target-specific dependency edge" target_dep

  make_case forward <<'EOF'
[features]
embedded = ["autumn-web/sqlite"]
EOF
  check_fail "feature forwarding the flip under another name" forward

  make_case default_forward <<'EOF'
[features]
default = ["autumn-web/sqlite"]
sqlite = ["autumn-web/sqlite"]
EOF
  check_fail "default enabling the flip" default_forward

  make_case renamed <<'EOF'
[dependencies]
web = { package = "autumn-web", version = "0.7", features = ["sqlite"] }
EOF
  check_fail "renamed dependency edge" renamed

  make_case commented <<'EOF'
[dependencies]
# autumn-web = { version = "0.7", features = ["sqlite"] }
autumn-web = { version = "0.7", features = ["db"] }
EOF
  check_pass "a commented-out edge is not an edge" commented

  make_case same_name <<'EOF'
[features]
default = ["postgres"]
sqlite = ["autumn-web/sqlite", "diesel_migrations/sqlite"]
EOF
  check_pass "autumn-cli's own opt-in sqlite feature" same_name

  make_case unrelated <<'EOF'
[dependencies]
diesel = { version = "2", features = ["sqlite", "postgres"] }
autumn-web = { version = "0.7", features = ["db", "mail"] }
EOF
  check_pass "another crate's sqlite feature is unrelated" unrelated

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]] || die "sqlite-unification self-test failed — the
  checker is not catching what it claims to. Fix the checker before trusting a
  green gate."
  trap - EXIT
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------

case "${1-}" in
  --scan-one)
    # Internal: scan a single manifest (used by `gate_check`'s xargs).
    scan_manifest "$2"
    ;;
  --self-test)
    self_test
    ;;
  --check-only)
    run_real_check
    ;;
  "")
    self_test
    echo
    run_real_check
    ;;
  *)
    die "unknown argument '$1' (expected --self-test, --check-only, or none)"
    ;;
esac
