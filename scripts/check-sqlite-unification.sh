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
#      Quoted keys are normalized before the rules run: `"autumn-web" = { … }`,
#      `[dependencies."autumn-web"]`, and `"autumn-web".features = [ … ]` are
#      all spellings cargo accepts (issue #2569).
#   2. No `[features]` entry forwards `autumn-web/sqlite` / `autumn-cli/sqlite`
#      unless the entry is ITSELF named `sqlite` AND the manifest belongs to one
#      of those two crates. That single exception is autumn-cli's own opt-in
#      backend (`sqlite = ["autumn-web/sqlite", …]`), selected the same explicit
#      way autumn-web's is; the same line in any other crate is an edge.
#   3. No `default` feature list enables `sqlite`, bare or forwarded.
#   4. Rules 2 and 3 follow chains of local features: `default = ["embedded"]`
#      with `embedded = ["sqlite"]` is the same flip. The report shows the
#      chain (issue #2571).
#   5. A dependency spelled as a dotted key at the root or under `[target]`
#      (`dependencies.autumn-web = { … }`) is an edge too.
#   6. A unicode escape in a basic string of a dependency or feature entry,
#      or of a table header, fails closed: the rules cannot read it.
#
# The lexer folds TOML multi-line strings to one-line strings that keep their
# text, so a bracket in one does not move the scan out of step, and
# `features = ["""sqlite"""]` still reads as "sqlite" (issue #2571). It also
# accepts the whitespace TOML allows in headers and dotted keys.
#
# It is a manifest gate, not a build: ~2 seconds, self-testing. Two layers:
#   - The SCAN reads every manifest with the awk lexer below. It needs no
#     toolchain and also covers crates outside the workspace.
#   - The RESOLVER reads `cargo metadata` for the workspace: the features
#     cargo resolves today, and each member's declared edges and feature
#     chains, optional or target-only ones too. Cargo parses the TOML, so no
#     spelling can slip past it. It needs cargo and jq; without them it is
#     skipped, unless SQLITE_GATE_REQUIRE_RESOLVE=1 (CI sets it).
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
      ml = ""
      ESCAPED_HEADER = sprintf("%c", 1) "escaped-header"
      # A dependency or feature name, as cargo accepts it once its quotes are
      # gone: any run without a dot, blank, `=`, quote, slash, `?` or bracket.
      NAME = "[^]. \t=\"/?[]+"
      DEPS = "(dependencies|dev-dependencies|build-dependencies)"
      defines_flip_sqlite = 0
    }

    # ── TOML lexing ──────────────────────────────────────────────────────
    #
    # All three helpers are STRING-AWARE and know both quote styles. A `#`
    # inside a string is not a comment; a `[` inside one does not open an
    # array. Getting either wrong desynchronizes the section tracker for the
    # rest of the file, which fails OPEN.
    # A backslash escapes the next character inside a BASIC string ("…") and
    # is literal inside a literal string (SQ…SQ). Reading `\"` as the end of a
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
    # Whether `s` holds a unicode escape. Only a basic string has escapes:
    # TOML reads a backslash in a literal string as a backslash.
    function has_unicode_escape(s,   i, c, q) {
      q = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (q == "\"" && c == "\\") {
          if (substr(s, i + 1, 1) ~ /[uU]/) return 1
          i++
          continue
        }
        if (q != "") { if (c == q) q = ""; continue }
        if (c == "\"" || c == SQ) q = c
      }
      return 0
    }
    # TOML literal strings are as valid as basic ones, so match against a copy
    # with the quotes normalized rather than writing every pattern twice.
    function normalize_quotes(s) { gsub(SQ, "\"", s); return s }

    # A TOML key may be quoted — `"autumn-web" = { … }`,
    # `"autumn-web".features = [ … ]`, `[dependencies."autumn-web"]` — and
    # cargo accepts every spelling. The rules anchor on unquoted keys, so
    # hand them one spelling: strip the key-quoting from the entry ahead of
    # the first `=`. Only the key is touched — quotes inside the value stay
    # significant to the value patterns (`"sqlite"`, `package = "autumn-web"`,
    # the `"dep/sqlite"` forwarding paths).
    function unquote_key(entry,   i, key, tail) {
      i = assign_index(entry)
      if (i == 0) return entry
      key = substr(entry, 1, i - 1)
      tail = substr(entry, i)
      gsub(SQ, "", key)
      gsub(/"/, "", key)
      # A quoted key can hold `=`: target."cfg(target_os = linux)".
      # Drop it, so the rules split the entry at the real assignment.
      gsub(/=/, "", key)
      gsub(/[ \t]*\.[ \t]*/, ".", key)
      return key tail
    }
    # The index of the assignment `=`: the first one outside quotes.
    function assign_index(s,   i, c, q) {
      q = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (q == "\"" && c == "\\") { i++; continue }
        if (q != "") { if (c == q) q = ""; continue }
        if (c == "\"" || c == SQ) { q = c; continue }
        if (c == "=") return i
      }
      return 0
    }

    # TOML multi-line strings (three double or three single quotes) can span
    # lines and hold any bracket or quote. Fold each one to a one-line basic
    # string that keeps its text, so a value such as `"""sqlite"""` still
    # reads as "sqlite". `ml` holds the open delimiter across lines, "" when
    # none is open; `ml_buf` holds the text read so far.
    function fold_multiline(s,   i, c, q, out, d, j) {
      out = ""; q = ""; i = 1
      if (ml != "") {
        j = ml_close(s, 1)
        if (j == 0) { ml_buf = ml_buf "\n" s; return "" }
        ml_buf = ml_buf "\n" substr(s, 1, j - 4)
        out = "\"" ml_text() "\""
        ml = ""
        i = j
      }
      for (; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (q == "\"" && c == "\\") { out = out c substr(s, i + 1, 1); i++; continue }
        if (q != "") { if (c == q) q = ""; out = out c; continue }
        if (c == "#") { out = out substr(s, i); break }
        d = substr(s, i, 3)
        if (d == "\"\"\"" || d == SQ SQ SQ) {
          ml = d
          j = ml_close(s, i + 3)
          if (j == 0) { ml_buf = substr(s, i + 3); return out }
          ml_buf = substr(s, i + 3, j - 3 - (i + 3))
          out = out "\"" ml_text() "\""
          ml = ""
          i = j - 1
          continue
        }
        if (c == "\"" || c == SQ) q = c
        out = out c
      }
      return out
    }
    # The text of the open multi-line string, made safe for a one-line basic
    # string. TOML drops a newline right after the opening delimiter, and in
    # a basic string a backslash at line end joins the next line. Quotes,
    # backslashes, comment marks and brackets go, so the text cannot change
    # the structure around it.
    function ml_text(   t) {
      t = ml_buf
      sub(/^\n/, "", t)
      if (ml == "\"\"\"") {
        gsub(/\\\n[ \t\n]*/, "", t)
        # Keep a real unicode escape, so the fail-closed rule still sees it:
        # drop escaped backslashes, then park each remaining `\u` / `\U`.
        gsub(/\\\\/, "", t)
        gsub(/\\[uU]/, "\001u", t)
      }
      gsub(/["\\#{}\[\]]/, "", t)
      gsub("\001", "\\", t)
      gsub(SQ, "", t)
      gsub(/\n/, " ", t)
      return t
    }
    # Index just past the delimiter that closes `ml`, from `start`; 0 when
    # this line does not close it. A run of up to five quotes closes with its
    # last three: the first one or two are content.
    function ml_close(s, start,   i, r, qc) {
      qc = substr(ml, 1, 1)
      for (i = start; i <= length(s); i++) {
        if (qc == "\"" && substr(s, i, 1) == "\\") { i++; continue }
        if (substr(s, i, 3) == ml) {
          r = 3
          while (r < 5 && substr(s, i + r, 1) == qc) r++
          return i + r
        }
      }
      return 0
    }

    # ── Entry assembly ───────────────────────────────────────────────────
    #
    # Joins a logical entry that spans lines — a `features` array written one
    # element per line is the common shape, and a line-at-a-time scan would
    # miss it entirely. Returns "" while an entry is still open.
    function feed(line,   entry) {
      sub(/\r$/, "", line)              # a CRLF checkout must not blind the gate
      line = fold_multiline(line)
      line = strip_comment(line)
      if (ml != "") {
        # A multi-line string is still open: hold the entry until it closes.
        if (pending == "") {
          gsub(/^[ \t]+|[ \t]+$/, "", line)
          pending = line; entry_line = FNR
        } else pending = pending " " line
        return ""
      }
      if (pending != "") {
        pending = pending " " line
        if (!balanced(pending)) return ""
        entry = pending; pending = ""
        return unquote_key(entry)
      }
      gsub(/^[ \t]+|[ \t]+$/, "", line)
      if (line == "") return ""
      if (line ~ /^\[/) {
        # A unicode escape in a header can spell a dependency name, and the
        # rules cannot read it. Hand pass 2 a marker to report.
        if (has_unicode_escape(line)) { entry_line = FNR; section = line; return ESCAPED_HEADER }
        # A header ends any entry. It carries no string values, so every
        # quote in it is key-quoting (`[dependencies."autumn-web"]`,
        # `[target."cfg(unix)".dependencies]`); strip them so the section
        # matchers work off one spelling.
        section = line
        gsub(SQ, "", section)
        gsub(/"/, "", section)
        # TOML allows whitespace around the dots and inside the brackets.
        gsub(/[ \t]*\.[ \t]*/, ".", section)
        gsub(/^\[[ \t]+/, "[", section)
        gsub(/[ \t]+\]$/, "]", section)
        return ""
      }
      if (!balanced(line)) { pending = line; entry_line = FNR; return "" }
      entry_line = FNR
      return unquote_key(line)
    }

    # The dependency name in a loose dotted key, or "". Only the paths cargo
    # reads: `dependencies.X` and `target.T.dependencies.X` at the root,
    # `T.dependencies.X` under `[target]`, `dependencies.X` under
    # `[target.T]`. Not `package.metadata.dependencies.X`.
    function loose_dep_name(key,   k) {
      k = key
      if (section == "") {
        if (k ~ ("^target\\.[^.]+\\." DEPS "\\.")) sub(/^target\.[^.]+\./, "", k)
      } else if (section == "[target]") {
        if (k !~ ("^[^.]+\\." DEPS "\\.")) return ""
        sub(/^[^.]+\./, "", k)
      } else if (section !~ /^\[target\.[^]]+\]$/) return ""
      if (k !~ ("^" DEPS "\\.")) return ""
      sub(("^" DEPS "\\."), "", k)
      sub(/\..*$/, "", k)
      return k
    }
    function is_dep_table() {
      return section ~ /(^\[|\.)(dependencies|dev-dependencies|build-dependencies)\]$/
    }
    # `[dependencies.autumn-web]` — the crate is in the header, not the key.
    function dep_section_crate(   name) {
      if (!match(section, "(^\\[|\\.)" DEPS "\\." NAME "\\]$"))
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
      while (match(tail, "\"" NAME "\\??/sqlite\"")) {
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

    # Find each local feature that enables the flip, directly or through a
    # chain of other local features (`default = ["embedded"]`,
    # `embedded = ["sqlite"]`). `via[f]` is the next hop; "" for a direct one.
    function resolve_reach(   k, n, p, parts, changed) {
      for (k in feat_entry)
        if (forwards_flip(feat_entry[k])) { reach[k] = 1; via[k] = "" }
      if (defines_flip_sqlite && !("sqlite" in reach)) { reach["sqlite"] = 1; via["sqlite"] = "" }
      changed = 1
      while (changed) {
        changed = 0
        for (k in feat_refs) {
          if (k in reach) continue
          n = split(feat_refs[k], parts, " ")
          for (p = 1; p <= n; p++) {
            if (parts[p] in reach) { reach[k] = 1; via[k] = parts[p]; changed = 1; break }
          }
        }
      }
    }
    # The hops from feature `k` to the flip, for the report.
    function chain(k,   s) {
      s = k
      while (via[k] != "") { k = via[k]; s = s " -> " k }
      return s
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
      # Record each feature, by any name cargo accepts, and the LOCAL features
      # it enables. Pass 2 resolves the chains, after every alias is known.
      if (section == "[features]" && norm ~ /^[^= \t][^=]*=/) {
        fkey = norm
        sub(/[ \t]*=.*$/, "", fkey)
        feat_entry[fkey] = norm
        refs = norm
        sub(/^[^=]*=/, "", refs)
        feat_refs[fkey] = ""
        while (match(refs, /"[^"]*"/)) {
          ref = substr(refs, RSTART + 1, RLENGTH - 2)
          refs = substr(refs, RSTART + RLENGTH)
          if (ref !~ /[\/:]/) feat_refs[fkey] = feat_refs[fkey] " " ref
        }
      }

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
      if (is_dep_table() && norm ~ ("^" NAME "\\.package[ \t]*=")) {
        name = norm
        sub(/\.package.*$/, "", name)
        dotted_package[name] = quoted_value(norm)
        alias_of[name] = dotted_package[name]
      }
      # A rename spelled as loose dotted keys, at the root or under
      # `[target]`: `dependencies.web.package = "autumn-web"`.
      key = norm
      sub(/[ \t]*=.*$/, "", key)
      name = loose_dep_name(key)
      if (name != "" && key ~ ("(^|\\.)" DEPS "\\." NAME "\\.package$")) alias_of[name] = quoted_value(norm)
      # The inline form, whose alias a FEATURE path then names:
      #   web = { package = "autumn-web", optional = true }
      #   embedded = ["dep:web", "web/sqlite"]
      if (is_dep_table() && norm ~ ("^" NAME "[ \t]*=") && norm ~ /package[ \t]*=[ \t]*"/) {
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
      pending = ""; section = ""; ml = ""
      if ("sqlite" in feat_entry && forwards_flip(feat_entry["sqlite"]))
        defines_flip_sqlite = 1
      # autumn-web owns the flip, so a bare "sqlite" in ITS default list is the
      # flip itself, with nothing to forward to.
      if (pkg ~ ("^(" flip ")$")) defines_flip_sqlite = 1
      resolve_reach()
    }
    {
      entry = feed($0)
      if (entry == "") next
      if (entry == ESCAPED_HEADER) {
        report("a unicode escape in a table header cannot be checked; write it plainly")
        next
      }
      norm = normalize_quotes(entry)
      mentions_sqlite = (norm ~ /"sqlite"/)
      forwards = forwards_flip(norm)
      loose = (section == "" || section ~ /^\[target(\.[^]]*)?\]$/)

      # ── 0. Spellings the rules below do not read: fail closed ─────────
      # A TOML unicode escape can spell any name, so the rules cannot read
      # it.
      if ((is_dep_table() || dep_section_crate() != "" || section == "[features]" || loose) \
          && has_unicode_escape(entry)) {
        report("a unicode escape in a dependency or feature entry cannot be checked; write it plainly")
        next
      }
      # A dependency or feature table spelled as a dotted key, at the root or
      # under `[target]`: `dependencies.autumn-web = { … }`.
      if (loose) {
        key = norm
        sub(/[ \t]*=.*$/, "", key)
        dep = loose_dep_name(key)
        if (dep != "" && mentions_sqlite \
            && (dep ~ ("^(" flip ")$") \
                || (dep in alias_of && alias_of[dep] ~ ("^(" flip ")$")) \
                || norm ~ ("package[ \t]*=[ \t]*\"(" flip ")\""))) {
          report("dependency edge enables the `sqlite` backend flip")
          next
        }
        if (section == "" && key ~ /^features(\.|$)/ && forwards) {
          report("a dotted `features` key forwards the `sqlite` backend flip")
          next
        }
      }

      # ── 1. A dependency edge that enables the flip ────────────────────
      if (is_dep_table() && mentions_sqlite) {
        # Inline: by key, or renamed with `package` in the same entry.
        if (norm ~ ("^(" flip ")[ \t]*=") \
            || norm ~ ("package[ \t]*=[ \t]*\"(" flip ")\"")) {
          report("dependency edge enables the `sqlite` backend flip")
          next
        }
        # Dotted: `autumn-web.features`, or an alias pass 1 resolved.
        if (norm ~ ("^" NAME "\\.features[ \t]*=")) {
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
        if (key == "default" && (key in reach)) {
          report("`default` enables the `sqlite` backend flip (" chain(key) ")")
        } else if (forwards && key != "sqlite") {
          report("feature `" key "` forwards the `sqlite` backend flip")
        } else if ((key in reach) && key != "sqlite") {
          report("feature `" key "` reaches the `sqlite` backend flip (" chain(key) ")")
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

# The authoritative check: ask cargo. Prints one line per violation in what
# cargo resolves or in what a workspace member declares; returns 2 if cargo or
# jq cannot run. The scan above reads TOML by hand and can miss a spelling that
# cargo accepts. The resolver cannot. It covers workspace members only, so
# the scan stays for the crates outside the workspace.
resolve_check() {
  local root="$1" meta
  command -v cargo >/dev/null 2>&1 && command -v jq >/dev/null 2>&1 || return 2
  meta="$(cd "$root" && cargo metadata --format-version 1 2>/dev/null)" || return 2
  jq -r --arg flip "$FLIP_CRATES" '
    def flip: test("^(" + $flip + ")$");
    # The local features that reach the seed set, the seed set included.
    def closure($f):
      def grow: . as $r
        | ([$f | to_entries[] | select(any(.value[]; . as $v | any($r[]; . == $v))) | .key]
           + $r | unique);
      until(grow == .; grow);
    (.packages | map({key: .id, value: .}) | from_entries) as $pkg
    # 1. What cargo resolves today.
    | ( .resolve.nodes[]
        | select(($pkg[.id].name | flip) and (.features | index("sqlite")))
        | "\($pkg[.id].name) resolves with the `sqlite` feature on" ),
    # 2. What each workspace member declares, also on an optional or
    #    target-only edge that is off today. Cargo has decoded every spelling.
      ( .workspace_members[] | $pkg[.] as $p
        | ( $p.dependencies[]
            | select((.name | flip) and (.features | index("sqlite")))
            | "\($p.name): its dependency on \(.name) enables `sqlite`" ),
          ( [$p.dependencies[] | select(.name | flip) | (.rename // .name)] as $aliases
            | ($p.features // {}) as $f
            | [$f | to_entries[]
                | select(any(.value[]; . as $v
                    | any($aliases[]; $v == (. + "/sqlite") or $v == (. + "?/sqlite"))))
                | .key] as $direct
            | (if ($p.name | flip) then $direct + ["sqlite"] else $direct end | unique) as $seed
            | ( $seed | closure($f) | .[] | select(. != "sqlite")
                | "\($p.name): feature `\(.)` enables `sqlite`" ),
              ( select(($direct | index("sqlite")) and (($p.name | flip) | not))
                | "\($p.name): feature `sqlite` forwards the flip from a crate that does not own it" ) ) )
  ' <<<"$meta" || return 2
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

  echo "==> asking cargo what the workspace resolves and declares"
  local findings rstatus=0
  findings="$(resolve_check "$root")" || rstatus=$?
  if (( rstatus != 0 )); then
    [[ "${SQLITE_GATE_REQUIRE_RESOLVE-}" == 1 ]] &&
      die "the resolver check could not run (cargo metadata or jq failed)"
    echo "note: cargo or jq not available; resolver check skipped"
  elif [[ -n "$findings" ]]; then
    printf '%s\n' "$findings"
    die "cargo metadata shows an edge to the \`sqlite\` backend flip. Find it
with: cargo tree -e features -i autumn-web"
  else
    echo "OK: cargo metadata shows no edge to the \`sqlite\` backend flip."
  fi
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

  # TOML allows a dependency key to be quoted, and cargo accepts it — every
  # dependency-edge rule anchors on an unquoted key, so these sailed through.
  make_case quoted_key <<'EOF'
[dependencies]
"autumn-web" = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "quoted dependency key" quoted_key

  make_case quoted_section <<'EOF'
[dependencies."autumn-web"]
features = ["sqlite"]
EOF
  check_fail "quoted section-form dependency table" quoted_section

  make_case quoted_dotted <<'EOF'
[dependencies]
"autumn-web".features = ["sqlite"]
EOF
  check_fail "quoted dotted-key dependency form" quoted_dotted

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

  # A TOML multi-line string can hold an unmatched bracket. The lexer must
  # carry string state across lines, or the edge below is swallowed (#2571).
  make_case multiline_basic <<'EOF'
[package]
name = "consumer"
description = """
an unmatched [ bracket
"""

[dependencies]
autumn-web = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "a multi-line basic string does not desync the scan" multiline_basic

  make_case multiline_literal <<'EOF'
[package]
name = "consumer"
description = '''
an unmatched { brace and a "quote
'''

[dependencies]
autumn-web = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "a multi-line literal string does not desync the scan" multiline_literal

  # A multi-line string that LOOKS like an edge is text, not an edge.
  make_case multiline_text <<'EOF'
[package]
name = "consumer"
description = """
[dependencies]
autumn-web = { version = "0.7", features = ["sqlite"] }
"""

[dependencies]
autumn-web = { version = "0.7", features = ["db"] }
EOF
  check_pass "an edge spelled inside a multi-line string is not an edge" multiline_text

  # A multi-line string can BE the value. Folding must keep its text.
  make_case multiline_value <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = ["""sqlite"""] }
EOF
  check_fail "a multi-line string value is read" multiline_value

  make_case multiline_value_span <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = [
"""
sqlite""",
] }
EOF
  check_fail "a multi-line string value that spans lines is read" multiline_value_span

  make_case multiline_package <<'EOF'
[dependencies.web]
package = '''autumn-web'''
features = ["sqlite"]
EOF
  check_fail "a multi-line literal package name is read" multiline_package

  # Fail closed on spellings the rules do not decode or anchor on.
  make_case unicode_escape <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = ["\u0073qlite"] }
EOF
  check_fail "a unicode escape in a dependency entry fails closed" unicode_escape

  make_case multiline_unicode <<'EOF'
[dependencies]
autumn-web = { version = "0.7", features = ["""\u0073qlite"""] }
EOF
  check_fail "a unicode escape in a multi-line basic string fails closed" multiline_unicode

  # TOML does not process escapes in a literal string, so this is a path.
  make_case literal_backslash <<'EOF'
[dependencies]
autumn-web = { path = 'C:\users\foo\autumn', features = ["db"] }
EOF
  check_pass "a backslash-u in a literal string is not an escape" literal_backslash

  make_case basic_escaped_backslash <<'EOF'
[dependencies]
autumn-web = { path = "C:\\users\\foo", features = ["db"] }
EOF
  check_pass "an escaped backslash before u is not an escape" basic_escaped_backslash

  make_case header_unicode <<'EOF'
[dependencies."autumn\u002dweb"]
features = ["sqlite"]
EOF
  check_fail "a unicode escape in a table header fails closed" header_unicode

  make_case spaced_header <<'EOF'
[ dependencies ]
autumn-web . features = ["sqlite"]
EOF
  check_fail "whitespace inside a header and a dotted key" spaced_header

  make_case root_dotted <<'EOF'
[package]
name = "consumer"
EOF
  # A root-level dotted key must come before any header.
  printf 'dependencies.autumn-web = { version = "0.7", features = ["sqlite"] }\n[package]\nname = "consumer"\n' \
    >"$tmp/root_dotted/Cargo.toml"
  check_fail "a dependency spelled as a root-level dotted key" root_dotted

  make_case target_dotted <<'EOF'
[target]
"cfg(unix)".dependencies.autumn-web = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "a target dependency spelled as a dotted key" target_dotted

  make_case root_dotted_renamed <<'EOF'
dependencies.web.package = "autumn-web"
dependencies.web.features = ["sqlite"]

[package]
name = "consumer"
EOF
  check_fail "a renamed dependency spelled as root-level dotted keys" root_dotted_renamed

  make_case root_dotted_unrelated <<'EOF'
dependencies.diesel = { version = "2", features = ["sqlite"] }
dependencies.store.package = "some-store"
dependencies.store.features = ["sqlite"]

[package]
name = "consumer"
EOF
  check_pass "an unrelated crate spelled as a dotted key is not an edge" root_dotted_unrelated

  make_case loose_unicode_alias <<'EOF'
dependencies."wébb".package = "autumn-web"
dependencies."wébb".features = ["sqlite"]

[package]
name = "consumer"
EOF
  check_fail "a quoted non-ASCII alias in loose dotted keys" loose_unicode_alias

  make_case section_unicode_alias <<'EOF'
[dependencies."wébb"]
package = "autumn-web"
features = ["sqlite"]
EOF
  check_fail "a quoted non-ASCII alias in a section header" section_unicode_alias

  make_case root_metadata <<'EOF'
package.metadata.dependencies.autumn-web = { features = ["sqlite"] }
package.metadata.features = { default = ["autumn-web/sqlite"] }
EOF
  check_pass "dotted package metadata is not a dependency or a feature" root_metadata

  make_case target_root_dotted <<'EOF'
target."cfg(unix)".dependencies.autumn-web = { version = "0.7", features = ["sqlite"] }
EOF
  check_fail "a target dependency spelled as a root-level dotted key" target_root_dotted

  make_case target_cfg_eq <<'EOF'
target.'cfg(target_os = "linux")'.dependencies.autumn-web = { path = "../autumn", optional = true, features = ["sqlite"] }
EOF
  check_fail "a dotted target key whose cfg holds an equals sign" target_cfg_eq

  make_case root_features <<'EOF'
features = { default = ["autumn-web/sqlite"] }

[package]
name = "consumer"
EOF
  check_fail "a features table spelled inline at the root" root_features

  # A chain of local features reaches the flip in two hops (#2571).
  make_case default_chain <<'EOF'
[package]
name = "autumn-cli"

[features]
default = ["embedded"]
embedded = ["sqlite"]
sqlite = ["autumn-web/sqlite"]
EOF
  check_fail "default reaching the flip through a local feature chain" default_chain

  make_case feature_chain <<'EOF'
[package]
name = "autumn-cli"

[features]
everything = ["embedded", "tls"]
embedded = ["sqlite"]
sqlite = ["autumn-web/sqlite"]
EOF
  check_fail "a feature reaching the flip through a local feature chain" feature_chain

  make_case chain_unicode <<'EOF'
[package]
name = "autumn-cli"

[features]
default = ["émbedded"]
"émbedded" = ["sqlite"]
sqlite = ["autumn-web/sqlite"]
EOF
  check_fail "a chain through a non-ASCII feature name" chain_unicode

  make_case chain_unrelated <<'EOF'
[package]
name = "some-store"

[features]
default = ["embedded"]
embedded = ["sqlite"]
sqlite = ["rusqlite"]
EOF
  check_pass "a chain to an unrelated sqlite feature is not the flip" chain_unrelated

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

  # The resolver layer, on a tiny path-only workspace. It needs cargo and jq;
  # without them the cases are skipped, unless the caller requires them.
  if command -v cargo >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    make_resolve_case() {
      local dir="$tmp/$1" edge="$2"
      mkdir -p "$dir/web/src" "$dir/consumer/src"
      : >"$dir/web/src/lib.rs"
      : >"$dir/consumer/src/lib.rs"
      printf '[workspace]\nmembers = ["web", "consumer"]\nresolver = "2"\n' >"$dir/Cargo.toml"
      printf '[package]\nname = "autumn-web"\nversion = "0.0.0"\nedition = "2021"\n\n[features]\nsqlite = []\n' \
        >"$dir/web/Cargo.toml"
      printf '[package]\nname = "consumer"\nversion = "0.0.0"\nedition = "2021"\n\n[dependencies]\n%s\n' \
        "$edge" >"$dir/consumer/Cargo.toml"
    }
    # A spelling the scan does not read; cargo does.
    make_resolve_case resolve_fail '"autumn\u002dweb" = { path = "../web", features = ["sqlite"] }'
    total+=1
    if [[ -n "$(resolve_check "$tmp/resolve_fail" 2>/dev/null)" ]]; then
      pass+=1
    else
      echo "  FAIL: resolver layer — a resolved \`sqlite\` feature not caught"
    fi
    # An optional edge that is off by default: nothing RESOLVES `sqlite`, but
    # the declared edge enables it once the dependency is on.
    make_resolve_case resolve_optional '[target."cfg(unix)".dependencies]
autumn-web = { path = "../web", optional = true, features = ["sqlite"] }'
    total+=1
    if [[ -n "$(resolve_check "$tmp/resolve_optional" 2>/dev/null)" ]]; then
      pass+=1
    else
      echo "  FAIL: resolver layer — an optional declared edge not caught"
    fi
    make_resolve_case resolve_chain 'web = { package = "autumn-web", path = "../web", optional = true }

[features]
extra = ["embedded"]
embedded = ["web?/sqlite"]'
    total+=1
    if [[ -n "$(resolve_check "$tmp/resolve_chain" 2>/dev/null)" ]]; then
      pass+=1
    else
      echo "  FAIL: resolver layer — a feature chain to the flip not caught"
    fi
    make_resolve_case resolve_pass 'autumn-web = { path = "../web" }'
    total+=1
    local out rstatus=0
    out="$(resolve_check "$tmp/resolve_pass" 2>/dev/null)" || rstatus=$?
    if [[ -z "$out" && "$rstatus" == 0 ]]; then
      pass+=1
    else
      echo "  FAIL: resolver layer — a clean workspace rejected (status $rstatus)"
    fi
  elif [[ "${SQLITE_GATE_REQUIRE_RESOLVE-}" == 1 ]]; then
    die "the resolver layer needs cargo and jq, and SQLITE_GATE_REQUIRE_RESOLVE=1"
  else
    echo "  note: cargo or jq not found; resolver self-test skipped"
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
