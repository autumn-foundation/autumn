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
#      or `autumn-cli` enables `sqlite`. Covers the inline form
#      (`autumn-web = { features = ["sqlite"] }`), the section form
#      (`[dev-dependencies.autumn-web]` + `features = [...]`), the dotted-key
#      form (`autumn-web.features = [...]`) and a renamed dependency in any of
#      its three spellings (`web = { package = "autumn-web", … }`,
#      `[dependencies.web]` + `package = "autumn-web"`, `web.package = "…"`) —
#      a rename splits the crate name away from the `features` list, so the
#      manifest is read twice and the aliases resolved before the rules run.
#   2. No `[features]` entry forwards `autumn-web/sqlite` / `autumn-cli/sqlite`
#      unless the entry is ITSELF named `sqlite` AND the manifest belongs to one
#      of those two crates. That single exception is autumn-cli's own opt-in
#      backend (`sqlite = ["autumn-web/sqlite", …]`), selected the same explicit
#      way autumn-web's is; the same line in any other crate is an edge.
#   3. No `default` feature list enables `sqlite`, bare or forwarded.
#
# It is a manifest gate, not a build: no toolchain, ~1 second, self-testing.
#
# Deliberately scans EVERY `Cargo.toml` under the root, including crates the
# root workspace excludes (fuzz targets, benchmark harnesses, `src-tauri`).
# Those cannot unify with the main graph, so the rule does not strictly apply
# there — but a manifest moving in or out of the workspace is a one-line edit,
# and a gate that followed `members` would silently stop covering a crate on
# that edit. Erring toward scanning costs a false positive nobody has hit;
# erring the other way costs the invariant.
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
# The checker. Prints one line per violation on stdout for the single manifest
# in `$1`. Scans IN-PROCESS — no `xargs`, no re-exec of `$0`: a gate whose
# scanner can fail to launch while the caller still reports "OK" is worse than
# no gate.
#
# The file is read TWICE (awk's `NR == FNR` idiom). Pass 1 answers two
# questions the per-line rules need up front — which crate this manifest
# belongs to, and whether it defines a `sqlite` feature that forwards the flip
# — because `default = ["sqlite"]` means the flip only in a manifest that does.
# ---------------------------------------------------------------------------
scan_manifest() {
  local manifest="$1"
  awk -v flip="$FLIP_CRATES" '
    BEGIN {
      SQ = sprintf("%c", 39)   # a literal single quote, unwritable inline here
      pkg = ""
      defines_flip_sqlite = 0
    }

    # ── TOML lexing ──────────────────────────────────────────────────────
    #
    # All three helpers are STRING-AWARE and know both quote styles. A `#`
    # inside a string is not a comment; a `[` inside one does not open an
    # array. Getting either wrong desynchronizes the section tracker for the
    # rest of the file, which fails OPEN.
    # A backslash escapes the next character inside a BASIC string ("…") and
    # is literal inside a literal string ('…'). Reading `\"` as the end of a
    # string desynchronizes everything after it: a `[` in ordinary package
    # metadata then reads as structural, the entry assembler swallows the
    # following dependency, and the scan fails OPEN.
    function strip_comment(s,   i, c, q, out) {
      q = ""; out = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (q == "\"" && c == "\\" && i < length(s)) {
          out = out c substr(s, i + 1, 1)
          i++
          continue
        }
        if (q != "") { if (c == q) q = "" }
        else if (c == "\"" || c == SQ) q = c
        else if (c == "#") break
        out = out c
      }
      return out
    }
    function balanced(s,   i, c, q, depth) {
      depth = 0; q = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (q == "\"" && c == "\\") { i++; continue }
        if (q != "") { if (c == q) q = ""; continue }
        if (c == "\"" || c == SQ) { q = c; continue }
        if (c == "[" || c == "{") depth++
        else if (c == "]" || c == "}") depth--
      }
      return depth <= 0
    }
    # TOML literal strings are as valid as basic ones, so match against a copy
    # with the quotes normalized rather than writing every pattern twice.
    function normalize_quotes(s) { gsub(SQ, "\"", s); return s }

    # ── Entry assembly ───────────────────────────────────────────────────
    #
    # Joins a logical entry that spans lines — a `features` array written one
    # element per line is the common shape, and a line-at-a-time scan would
    # miss it entirely. Returns "" while an entry is still open.
    function feed(line,   entry) {
      sub(/\r$/, "", line)              # a CRLF checkout must not blind the gate
      line = strip_comment(line)
      if (pending != "") {
        pending = pending " " line
        if (!balanced(pending)) return ""
        entry = pending; pending = ""
        return entry
      }
      gsub(/^[ \t]+|[ \t]+$/, "", line)
      if (line == "") return ""
      if (line ~ /^\[/) { section = line; return "" }   # a header ends any entry
      if (!balanced(line)) { pending = line; entry_line = FNR; return "" }
      entry_line = FNR
      return line
    }

    function is_dep_table() {
      return section ~ /(^\[|\.)(dependencies|dev-dependencies|build-dependencies)\]$/
    }
    # `[dependencies.autumn-web]` — the crate is in the header, not the key.
    function dep_section_crate(   name) {
      if (!match(section, /(^\[|\.)(dependencies|dev-dependencies|build-dependencies)\.[A-Za-z0-9_-]+\]$/))
        return ""
      name = section
      sub(/\]$/, "", name)
      sub(/.*\./, "", name)
      return name
    }
    function report(msg) { printf "%s:%d: %s\n", FILENAME, entry_line, msg }
    # Whether a `[features]` entry forwards the flip, under the real crate name
    # or under a rename. Cargo writes the DEPENDENCY ALIAS in a feature path
    # (`web/sqlite` for `web = { package = "autumn-web" }`), so matching the
    # real names alone leaves a rename free to enable the flip.
    function forwards_flip(e,   tail, name) {
      tail = e
      # `dep?/feature` is the WEAK forwarding syntax cargo accepts — "enable
      # the feature only if something else enabled the dependency". A `default`
      # that pairs it with `dep:autumn-web` enables both, so the `?` spelling
      # flips the backend exactly like the plain one.
      while (match(tail, /"[A-Za-z0-9_-]+\??\/sqlite"/)) {
        name = substr(tail, RSTART + 1, RLENGTH - 2)
        sub(/\??\/sqlite$/, "", name)
        if (name ~ ("^(" flip ")$")) return 1
        if (name in alias_of && alias_of[name] ~ ("^(" flip ")$")) return 1
        tail = substr(tail, RSTART + RLENGTH)
      }
      return 0
    }
    # The value of a `key = "value"` entry.
    function quoted_value(entry,   value) {
      value = entry
      sub(/^[^=]*=[ \t]*"/, "", value)
      sub(/".*$/, "", value)
      return value
    }

    # ── Pass 1: whose manifest is this, and what does it define? ─────────
    NR == FNR {
      entry = feed($0)
      if (entry == "") next
      norm = normalize_quotes(entry)
      if (section == "[package]" && norm ~ /^name[ \t]*=/) {
        pkg = norm
        sub(/^name[ \t]*=[ \t]*"/, "", pkg)
        sub(/".*$/, "", pkg)
      }
      if (section == "[features]" && norm ~ /^sqlite[ \t]*=/ && forwards_flip(norm))
        defines_flip_sqlite = 1

      # A RENAMED dependency names its real crate in a `package` key that can
      # sit anywhere in the entry, so the rules cannot see it one line at a
      # time. Both spellings are collected here and resolved in pass 2:
      #
      #   [dependencies.web]        |  [dependencies]
      #   package = "autumn-web"    |  web.package = "autumn-web"
      #   features = ["sqlite"]     |  web.features = ["sqlite"]
      #
      # Cargo accepts both and both enable the flip.
      if (dep_section_crate() != "" && norm ~ /^package[ \t]*=/) {
        section_package[section] = quoted_value(norm)
        alias_of[dep_section_crate()] = section_package[section]
      }
      if (is_dep_table() && norm ~ /^[A-Za-z0-9_-]+\.package[ \t]*=/) {
        name = norm
        sub(/\.package.*$/, "", name)
        dotted_package[name] = quoted_value(norm)
        alias_of[name] = dotted_package[name]
      }
      # The inline form, whose alias a FEATURE path then names:
      #   web = { package = "autumn-web", optional = true }
      #   embedded = ["dep:web", "web/sqlite"]
      if (is_dep_table() && norm ~ /^[A-Za-z0-9_-]+[ \t]*=/ && norm ~ /package[ \t]*=[ \t]*"/) {
        name = norm
        sub(/[ \t]*=.*$/, "", name)
        value = norm
        sub(/^.*package[ \t]*=[ \t]*"/, "", value)
        sub(/".*$/, "", value)
        alias_of[name] = value
      }
      next
    }

    # ── Pass 2: the rules ────────────────────────────────────────────────
    FNR == 1 {
      pending = ""; section = ""
      # autumn-web owns the flip, so a bare "sqlite" in ITS default list is the
      # flip itself, with nothing to forward to.
      if (pkg ~ ("^(" flip ")$")) defines_flip_sqlite = 1
    }
    {
      entry = feed($0)
      if (entry == "") next
      norm = normalize_quotes(entry)
      mentions_sqlite = (norm ~ /"sqlite"/)
      forwards = forwards_flip(norm)

      # ── 1. A dependency edge that enables the flip ────────────────────
      if (is_dep_table() && mentions_sqlite) {
        # Inline: by key, or renamed with `package` in the same entry.
        if (norm ~ ("^(" flip ")[ \t]*=") \
            || norm ~ ("package[ \t]*=[ \t]*\"(" flip ")\"")) {
          report("dependency edge enables the `sqlite` backend flip")
          next
        }
        # Dotted: `autumn-web.features`, or an alias pass 1 resolved.
        if (norm ~ /^[A-Za-z0-9_-]+\.features[ \t]*=/) {
          alias = norm
          sub(/\.features.*$/, "", alias)
          if (alias ~ ("^(" flip ")$") \
              || (alias in dotted_package && dotted_package[alias] ~ ("^(" flip ")$"))) {
            report("dependency edge enables the `sqlite` backend flip")
            next
          }
        }
      }
      # Section form: the crate is the last header segment, unless a
      # `package` key inside the section renamed it.
      crate = dep_section_crate()
      if (crate != "" && (section in section_package)) crate = section_package[section]
      if (crate ~ ("^(" flip ")$") && norm ~ /^features[ \t]*=/ && mentions_sqlite) {
        report("dependency edge enables the `sqlite` backend flip")
        next
      }

      # ── 2 & 3. A feature that forwards or defaults into the flip ──────
      if (section == "[features]") {
        key = norm
        sub(/[ \t]*=.*$/, "", key)
        if (key == "default" && (forwards || (mentions_sqlite && defines_flip_sqlite))) {
          report("`default` enables the `sqlite` backend flip")
        } else if (forwards && key != "sqlite") {
          report("feature `" key "` forwards the `sqlite` backend flip")
        } else if (forwards && !(pkg ~ ("^(" flip ")$"))) {
          # A same-named `sqlite` feature is the sanctioned opt-in ONLY in the
          # two crates that own the flip. Anywhere else it is an edge wearing
          # the exception as a name.
          report("feature `sqlite` forwards the backend flip from a crate that does not own it")
        }
      }
    }
  ' "$manifest" "$manifest"
}

# Scan every manifest under `$1`. Prints violations; returns 1 if any, 2 if the
# scan could not run (no manifests found, or a scanner failure). Reporting OK
# because nothing ran is the failure mode this guards.
gate_check() {
  local root="$1"
  local findings="" manifest out
  local -i count=0

  while IFS= read -r manifest; do
    count+=1
    if ! out="$(scan_manifest "$manifest")"; then
      echo "scanner failed on $manifest" >&2
      return 2
    fi
    if [[ -n "$out" ]]; then
      findings+="$out"$'\n'
    fi
  done < <(find "$root" -name Cargo.toml -not -path '*/target/*' | sort)

  if (( count == 0 )); then
    echo "no Cargo.toml found under $root" >&2
    return 2
  fi
  if [[ -n "$findings" ]]; then
    printf '%s' "$findings"
    return 1
  fi
  return 0
}

run_real_check() {
  local root status=0
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
  # Resolved, not assumed: a symlinked or relocated script that scanned the
  # wrong tree would find no manifests and — before the count check in
  # `gate_check` — report OK.
  [[ -f "$root/autumn/Cargo.toml" ]] ||
    die "expected the repository root at $root, but $root/autumn/Cargo.toml is missing"

  echo "==> scanning workspace manifests for a \`sqlite\` backend-flip edge"
  gate_check "$root" || status=$?
  case "$status" in
    0) echo "OK: no manifest enables the \`sqlite\` feature through a dependency edge." ;;
    2) die "the manifest scan could not run — see above. A gate that cannot
  scan must not report OK." ;;
    *) die "a manifest enables the \`sqlite\` backend flip.

\`sqlite\` swaps db::RuntimeConnection for the WHOLE dependency graph, so a
single edge breaks every Postgres consumer. Build the SQLite lane with an
explicit invocation instead:

    cargo build -p autumn-web --features sqlite
    cargo build -p autumn-cli --no-default-features --features sqlite

See the \`sqlite = [...]\` comment in autumn/Cargo.toml." ;;
  esac
}

# ---------------------------------------------------------------------------
# Self-test: prove the checker still catches what it claims to.
# ---------------------------------------------------------------------------
self_test() {
  local tmp
  local -i pass=0 total=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  # Each case is a whole crate directory, so `check_pass` cannot be satisfied
  # by an empty scan: `gate_check` returns 2 when it finds no manifest.
  make_case() {
    local dir="$tmp/$1"
    mkdir -p "$dir"
    cat >"$dir/Cargo.toml"
  }

  check_fail() {
    local name="$1" dir="$2" status=0
    total+=1
    gate_check "$tmp/$dir" >/dev/null 2>&1 || status=$?
    if (( status == 1 )); then
      pass+=1
    else
      echo "  FAIL: $name — violation not caught (status $status)"
    fi
  }

  check_pass() {
    local name="$1" dir="$2" status=0
    total+=1
    gate_check "$tmp/$dir" >/dev/null 2>&1 || status=$?
    if (( status == 0 )); then
      pass+=1
    else
      echo "  FAIL: $name — legitimate manifest rejected (status $status)"
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

  make_case renamed <<'EOF'
[dependencies]
web = { package = "autumn-web", version = "0.7", features = ["sqlite"] }
EOF
  check_fail "renamed dependency edge" renamed

  make_case dotted <<'EOF'
[dependencies]
autumn-web.workspace = true
autumn-web.features = ["sqlite"]
EOF
  check_fail "dotted-key dependency form" dotted

  # A rename splits the crate name away from the `features` list, so neither
  # line names the flip on its own. Both spellings, and `features` written
  # BEFORE the `package` key that resolves it.
  make_case renamed_section <<'EOF'
[dependencies.web]
package = "autumn-web"
features = ["sqlite"]
EOF
  check_fail "renamed dependency in table form" renamed_section

  make_case renamed_section_reordered <<'EOF'
[dependencies.web]
features = ["sqlite"]
package = "autumn-web"
EOF
  check_fail "renamed dependency in table form, package last" renamed_section_reordered

  make_case renamed_dotted <<'EOF'
[dependencies]
web.package = "autumn-web"
web.features = ["sqlite"]
EOF
  check_fail "renamed dependency in dotted form" renamed_dotted

  make_case renamed_unrelated <<'EOF'
[dependencies.store]
package = "some-store"
features = ["sqlite"]
EOF
  check_pass "a rename of an unrelated crate is not an edge" renamed_unrelated

  make_case single_quoted <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = ['sqlite'] }
EOF
  check_fail "single-quoted feature name" single_quoted

  mkdir -p "$tmp/crlf"
  printf '[dev-dependencies]\r\nautumn-web = { path = "../autumn", features = ["sqlite"] }\r\n' \
    >"$tmp/crlf/Cargo.toml"
  check_fail "CRLF line endings" crlf

  make_case desync <<'EOF'
[package]
name = "example"
description = "an [experimental framework"

[dependencies]
autumn-web = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "an unbalanced bracket inside a string does not desync the scan" desync

  make_case forward <<'EOF'
[package]
name = "example"

[features]
embedded = ["autumn-web/sqlite"]
EOF
  check_fail "feature forwarding the flip under another name" forward

  # Cargo writes the dependency ALIAS in a feature path, not the real crate
  # name, so a rename hides the flip from a match on the real names alone.
  make_case forward_weak <<'EOF'
[package]
name = "consumer"

[dependencies]
autumn-web = { version = "0.7", optional = true }

[features]
default = ["dep:autumn-web", "autumn-web?/sqlite"]
EOF
  check_fail "weak dependency-feature forwarding" forward_weak

  make_case forward_renamed <<'EOF'
[package]
name = "consumer"

[dependencies]
web = { package = "autumn-web", version = "0.7", optional = true }

[features]
embedded = ["dep:web", "web/sqlite"]
EOF
  check_fail "feature forwarding the flip through a renamed dependency" forward_renamed

  # An escaped quote inside package metadata must not end the string: reading
  # it as the end lets a later `[` count as structural, and the entry
  # assembler then swallows the dependency below it.
  mkdir -p "$tmp/escaped"
  cat >"$tmp/escaped/Cargo.toml" <<'EOF'
[package]
name = "consumer"
description = "contains \" and [ bracket"

[dependencies]
autumn-web = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "an escaped quote does not desync the scan" escaped

  make_case same_name_elsewhere <<'EOF'
[package]
name = "example-app"

[features]
sqlite = ["autumn-web/sqlite"]
EOF
  check_fail "the same-named exception does not travel to other crates" same_name_elsewhere

  make_case default_forward <<'EOF'
[package]
name = "autumn-cli"

[features]
default = ["autumn-web/sqlite"]
sqlite = ["autumn-web/sqlite"]
EOF
  check_fail "default forwarding the flip" default_forward

  make_case default_bare <<'EOF'
[package]
name = "autumn-cli"

[features]
default = ["tls", "sqlite"]
sqlite = ["autumn-web/sqlite", "diesel_migrations/sqlite"]
EOF
  check_fail "default enabling the crate's own flip feature" default_bare

  make_case commented <<'EOF'
[dependencies]
# autumn-web = { version = "0.7", features = ["sqlite"] }
autumn-web = { version = "0.7", features = ["db"] }
EOF
  check_pass "a commented-out edge is not an edge" commented

  make_case hash_in_string <<'EOF'
[package]
name = "example"
description = "tracks issue #1905"

[dependencies]
autumn-web = { version = "0.7", features = ["db"] }
EOF
  check_pass "a # inside a string is not a comment" hash_in_string

  mkdir -p "$tmp/escaped_clean"
  cat >"$tmp/escaped_clean/Cargo.toml" <<'EOF'
[package]
name = "example"
description = "contains \" and [ bracket"

[dependencies]
autumn-web = { version = "0.7", features = ["db"] }
EOF
  check_pass "escape handling does not invent a violation" escaped_clean

  make_case forward_renamed_unrelated <<'EOF'
[package]
name = "consumer"

[dependencies]
store = { package = "some-store", version = "1" }

[features]
embedded = ["store/sqlite"]
EOF
  check_pass "a renamed unrelated crate's sqlite feature is not the flip" forward_renamed_unrelated

  make_case same_name <<'EOF'
[package]
name = "autumn-cli"

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

  make_case default_without_flip <<'EOF'
[package]
name = "some-store"

[features]
default = ["sqlite"]
sqlite = ["rusqlite"]
EOF
  check_pass "an unrelated crate's own sqlite feature in default" default_without_flip

  # The scan must refuse to report OK when it scanned nothing.
  total+=1
  mkdir -p "$tmp/empty"
  local status=0
  gate_check "$tmp/empty" >/dev/null 2>&1 || status=$?
  if (( status == 2 )); then
    pass+=1
  else
    echo "  FAIL: an empty tree must not report OK (status $status)"
  fi

  echo "self-test: $pass/$total passed"
  (( pass == total )) || die "sqlite-unification self-test failed — the checker
  is not catching what it claims to. Fix the checker before trusting a green
  gate."
  trap - EXIT
  rm -rf "$tmp"
}

# ---------------------------------------------------------------------------

case "${1-}" in
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
