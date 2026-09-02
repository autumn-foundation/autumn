#!/usr/bin/env bash
# Keep Autumn's declared plugin API surface (issue #1601) honest.
#
# THE DIVISION OF LABOUR
#   The COMPILER does the load-bearing detection. `autumn-plugin-reference` is a
#   real `Plugin` implementation that calls every surface declared
#   `SurfaceTier::Stable` in `autumn/src/plugin_contract.rs`, so removing,
#   renaming, or re-signaturing one is a build failure in the `plugin-contract`
#   CI job. Nothing in this script can (or tries to) replace that.
#
#   THIS SCRIPT guards everything the compiler cannot see: that the registry and
#   the docs table say the same thing, that no stable entry exists without a
#   compiled call site, that the ratchet has not been loosened, and that a
#   change to the declared surface updates the migration guide's *Plugin
#   authors* section.
#
# WHAT IT CHECKS
#   1.  The registry parses and every entry has a name, a tier, and a note.
#   2.  Registry names are unique and sorted (the docs table and the diff both
#       read better for it, and the Rust suite asserts the same thing).
#   3.  Docs parity: `docs/plugins.md`'s "The declared surface" table lists
#       exactly the registry's entries, each at the registry's tier.
#   4.  Reference-plugin coverage: every STABLE entry has a
#       `// surface: <name>` marker in `autumn-plugin-reference/src/lib.rs`, and
#       no marker names something the registry does not declare stable.
#   5.  Ratchet over the SET of stable names, not their count
#       (scripts/plugin-surface-baseline.txt). A count-only ratchet is defeated
#       by a swap — demote one entry to experimental, add another, total
#       unchanged — and the reference plugin stays green because the call site
#       simply moves. Every baselined name must still exist AND still be
#       stable.
#   6.  `docs/migrations/TEMPLATE.md` still carries the `## Plugin authors`
#       section, so it cannot be dropped from future guides.
#   7.  Migration-guide coverage: when the diff against the base ref touches the
#       declared surface, the SAME diff must touch `docs/migrations/next.md`,
#       and that file's `## Plugin authors` section must carry real content —
#       not template placeholders. Requiring the file to be touched matters
#       because `next.md` is a ROLLING draft: without it, one PR's section
#       satisfies every later PR until release renames the file.
#
#       This check fails CLOSED. "I could not compute the diff" (a shallow
#       clone with no merge base) is reported as a failure, not folded into
#       "nothing changed" — the whole point is that a surface change cannot
#       pass silently.
#   8.  `autumn-plugin-reference` is a workspace member — otherwise CI never
#       builds the thing check 4 is reasoning about.
#
# WHAT IT DOES NOT CATCH
#   A signature change to a stable surface that the reference plugin's single
#   call site still compiles against — narrowing `impl Into<String>` to
#   `&'static str`, say, where the reference passes a literal. The compiler is
#   the detector for that class and it can be fooled by an unrepresentative
#   call site; check 7's trigger set is widened to the plugin-facing modules to
#   raise the odds a reviewer is prompted, but this is a residual risk, not a
#   proof.
#
# WHY NOT JUST THE RUST TESTS
#   Checks 1, 2 and 4 are also asserted in Rust (`autumn`'s `plugin_contract`
#   suite and `autumn-plugin-reference`'s own tests). They are repeated here
#   because this script needs no toolchain and runs in seconds, so a contributor
#   gets the answer before a ten-minute compile — and because checks 3, 5, 6 and
#   7 have no Rust home at all.
#
# USAGE
#     ./scripts/check-plugin-surface.sh              # self-test, then the real check
#     ./scripts/check-plugin-surface.sh --self-test  # synthetic fixtures only
#     ./scripts/check-plugin-surface.sh --check-only # this repository only
#
# The default invocation runs the self-test FIRST, so a refactor that silently
# defangs a check fails here rather than passing quietly forever.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

REGISTRY_FILE="autumn/src/plugin_contract.rs"
DOCS_FILE="docs/plugins.md"
REFERENCE_FILE="autumn-plugin-reference/src/lib.rs"
TEMPLATE_FILE="docs/migrations/TEMPLATE.md"
NEXT_GUIDE="docs/migrations/next.md"
WORKSPACE_MANIFEST="Cargo.toml"
REFERENCE_CRATE="autumn-plugin-reference"
BASELINE_FILE="scripts/plugin-surface-baseline.txt"

# Files whose change means "the plugin surface may have moved". The registry and
# the reference plugin are the definitive two; the other two are the modules
# whose signatures the registry points at, included so a `Plugin` or
# `plugin_conformance` edit prompts a Plugin-authors note too.
SURFACE_PATHS=(
  "autumn/src/plugin_contract.rs"
  "autumn/src/plugin.rs"
  "autumn/src/plugin_conformance.rs"
  "autumn-plugin-reference/src/lib.rs"
)

# The heading the migration guide uses for plugin-facing changes.
PLUGIN_SECTION='## Plugin authors'

failures=0

fail() {
  printf '\033[0;31m✗\033[0m %s\n' "$*" >&2
  failures=$((failures + 1))
}

pass() {
  printf '\033[0;32m✓\033[0m %s\n' "$*"
}

die() {
  printf '\033[0;31mfatal:\033[0m %s\n' "$*" >&2
  exit 2
}

# ── Extraction ─────────────────────────────────────────────────────────────

# Print `name<TAB>tier` for every entry in PLUGIN_SURFACES, in source order.
#
# The registry is a `const` array of struct literals with `name:`, `tier:` and
# `note:` fields in that order. Parsing is line-oriented and deliberately
# strict: a shape it cannot read is a hard error, not a silently empty list —
# an empty list would make every downstream check vacuously pass.
extract_registry() {
  local file="$1"
  awk '
    /^pub const PLUGIN_SURFACES/ { inarr = 1; next }
    inarr && /^\];/              { inarr = 0 }
    !inarr                       { next }
    /name: "/ {
      line = $0
      sub(/^.*name: "/, "", line)
      sub(/".*$/, "", line)
      name = line
      have_name = 1
      have_tier = 0
      have_note = 0
      next
    }
    /tier: SurfaceTier::/ {
      line = $0
      sub(/^.*tier: SurfaceTier::/, "", line)
      sub(/[^A-Za-z].*$/, "", line)
      tier = tolower(line)
      have_tier = 1
      next
    }
    /note: "/ {
      line = $0
      sub(/^.*note: "/, "", line)
      # `line` is now `<content>",` (or `<content>"` if the trailing comma was
      # dropped). Strip the closing quote and anything after it; what remains is
      # the note, and an empty remainder is an entry with nothing to tell an
      # author.
      sub(/"[^"]*$/, "", line)
      have_note = (length(line) > 0)
      if (have_name && have_tier) {
        printf "%s\t%s\t%s\n", name, tier, (have_note ? "noted" : "empty")
        have_name = 0
      }
      next
    }
  ' "$file"
}

# Print `name<TAB>tier` for every row of the docs table under
# "### The declared surface". Rows look like:
#   | `AppBuilder::nest` | stable | Mount a raw axum router … |
extract_docs_table() {
  local file="$1"
  awk '
    /^### The declared surface/ { intable = 1; next }
    intable && /^#/             { intable = 0 }
    !intable                    { next }
    /^\| `/ {
      line = $0
      sub(/^\| `/, "", line)
      name = line
      sub(/`.*$/, "", name)
      rest = line
      sub(/^[^|]*\| */, "", rest)
      tier = rest
      sub(/ *\|.*$/, "", tier)
      printf "%s\t%s\n", name, tolower(tier)
    }
  ' "$file"
}

# Print every `// surface: <name>` marker in the reference plugin.
# Exactly one space after the colon, matching the Rust side's
# `strip_prefix("// surface: ")`. A looser pattern here would let the two gates
# disagree about what counts as a marker.
extract_markers() {
  local file="$1"
  sed -n 's|^[[:space:]]*// surface: \(.*[^ ]\) *$|\1|p' "$file"
}

# ── Checks ─────────────────────────────────────────────────────────────────

check_registry() {
  local root="$1"
  local registry
  registry="$(extract_registry "$root/$REGISTRY_FILE")"

  if [[ -z "$registry" ]]; then
    fail "could not read any entry from PLUGIN_SURFACES in $REGISTRY_FILE — the parser and the registry have diverged"
    return
  fi

  local empty_notes
  empty_notes="$(awk -F'\t' '$3 == "empty" { print $1 }' <<<"$registry")"
  if [[ -n "$empty_notes" ]]; then
    fail "surface(s) with an empty note (the note is what a plugin author reads): $(tr '\n' ' ' <<<"$empty_notes")"
  fi

  local names sorted
  names="$(cut -f1 <<<"$registry")"
  sorted="$(LC_ALL=C sort <<<"$names")"
  if [[ "$(LC_ALL=C sort -u <<<"$names" | wc -l)" -ne "$(wc -l <<<"$names")" ]]; then
    fail "duplicate name in PLUGIN_SURFACES"
  fi
  if [[ "$names" != "$sorted" ]]; then
    fail "PLUGIN_SURFACES is not sorted by name; keep it sorted so the docs table and the diff stay readable"
  fi

  local stable_count
  stable_count="$(awk -F'\t' '$2 == "stable"' <<<"$registry" | wc -l | tr -d ' ')"

  if [[ "$failures" -eq 0 ]]; then
    pass "registry: $(wc -l <<<"$registry" | tr -d ' ') surfaces, $stable_count stable, sorted and noted"
  fi
}

# Ratchet over the SET of stable names, not their count. See the header.
check_stable_baseline() {
  local root="$1"

  if [[ ! -f "$root/$BASELINE_FILE" ]]; then
    fail "$BASELINE_FILE is missing; the stable-surface ratchet is disarmed"
    return
  fi

  local stable baseline lost added baseline_sorted
  stable="$(extract_registry "$root/$REGISTRY_FILE" | awk -F'\t' '$2 == "stable" { print $1 }' | LC_ALL=C sort)"
  baseline="$(grep -v '^[[:space:]]*#' "$root/$BASELINE_FILE" | grep -v '^[[:space:]]*$' || true)"

  if [[ -z "$baseline" ]]; then
    fail "$BASELINE_FILE has no entries; emptying it would disarm the ratchet silently"
    return
  fi

  baseline_sorted="$(LC_ALL=C sort -u <<<"$baseline")"
  if [[ "$baseline" != "$baseline_sorted" ]]; then
    fail "$BASELINE_FILE must be sorted and free of duplicates"
    return
  fi

  # Present in the baseline but no longer STABLE in the registry — either
  # removed outright, or demoted to experimental (the swap a count-only ratchet
  # never sees).
  lost="$(comm -23 <(echo "$baseline_sorted") <(echo "$stable"))"
  if [[ -n "$lost" ]]; then
    fail "these surfaces were declared STABLE and no longer are: $(tr '\n' ' ' <<<"$lost")"
    fail "  removing or demoting a stable plugin surface is a break: say so under '$PLUGIN_SECTION' in $NEXT_GUIDE, then drop the line(s) from $BASELINE_FILE in the same change"
    return
  fi

  added="$(comm -13 <(echo "$baseline_sorted") <(echo "$stable"))"
  if [[ -n "$added" ]]; then
    fail "these surfaces are declared STABLE but are not in $BASELINE_FILE: $(tr '\n' ' ' <<<"$added")"
    fail "  add them (keeping the file sorted) so the ratchet covers them from now on"
    return
  fi

  pass "ratchet: all $(wc -l <<<"$baseline_sorted" | tr -d ' ') baselined stable surfaces are still stable"
}

check_docs_parity() {
  local root="$1"
  local registry docs
  registry="$(extract_registry "$root/$REGISTRY_FILE" | cut -f1,2 | LC_ALL=C sort)"
  docs="$(extract_docs_table "$root/$DOCS_FILE" | LC_ALL=C sort)"

  if [[ -z "$docs" ]]; then
    fail "$DOCS_FILE has no '### The declared surface' table — the declared tiers are unreadable to a plugin author"
    return
  fi

  local diff_out
  if ! diff_out="$(diff <(echo "$registry") <(echo "$docs") 2>&1)"; then
    fail "$DOCS_FILE's surface table has drifted from PLUGIN_SURFACES ('<' = registry only, '>' = docs only):"
    sed 's/^/    /' <<<"$diff_out" >&2
    return
  fi
  pass "docs parity: $DOCS_FILE matches the registry"
}

check_reference_coverage() {
  local root="$1"
  local stable markers missing extra
  stable="$(extract_registry "$root/$REGISTRY_FILE" | awk -F'\t' '$2 == "stable" { print $1 }' | LC_ALL=C sort)"
  markers="$(extract_markers "$root/$REFERENCE_FILE" | LC_ALL=C sort -u)"

  if [[ -z "$markers" ]]; then
    fail "$REFERENCE_FILE has no '// surface:' markers — the reference plugin is no longer proving anything"
    return
  fi

  missing="$(comm -23 <(echo "$stable") <(echo "$markers"))"
  if [[ -n "$missing" ]]; then
    fail "declared STABLE with no call site in the reference plugin: $(tr '\n' ' ' <<<"$missing")"
    fail "  add a call under a '// surface: <name>' marker in $REFERENCE_FILE, or drop the entry from the registry"
  fi

  extra="$(comm -13 <(echo "$stable") <(echo "$markers"))"
  if [[ -n "$extra" ]]; then
    fail "marked as plugin surface in the reference plugin but not declared STABLE: $(tr '\n' ' ' <<<"$extra")"
  fi

  if [[ -z "$missing" && -z "$extra" ]]; then
    pass "reference coverage: every stable surface is exercised by $REFERENCE_CRATE"
  fi
}

check_workspace_membership() {
  local root="$1"
  if ! grep -q "\"$REFERENCE_CRATE\"" "$root/$WORKSPACE_MANIFEST"; then
    fail "$REFERENCE_CRATE is not a workspace member in $WORKSPACE_MANIFEST — CI would never build the surface gate"
    return
  fi
  pass "workspace: $REFERENCE_CRATE is a member"
}

check_template_section() {
  local root="$1"
  if ! grep -qF "$PLUGIN_SECTION" "$root/$TEMPLATE_FILE"; then
    fail "$TEMPLATE_FILE lost its '$PLUGIN_SECTION' section; every future guide would ship without one"
    return
  fi
  pass "template: $TEMPLATE_FILE carries '$PLUGIN_SECTION'"
}

# Does a file carry `## Plugin authors` with real content under it?
#
# "Real content" is a non-blank line under the heading, before the next `## `
# heading, that is none of:
#   - an HTML comment, single- OR multi-line (`<!-- TODO -->` is not content,
#     and neither is a comment spread across three lines);
#   - a line whose only substance is a `{placeholder}` — the template ships
#     those, and pasting one back is not saying what changed;
#   - a line the TEMPLATE itself carries verbatim. This is the load-bearing
#     one: the template's section opens with two paragraphs of guidance, so
#     without it a straight copy-paste of the template satisfies the gate while
#     telling a plugin author nothing.
#
# The heading is matched EXACTLY, so `### Plugin authors` or
# `## Plugin authors (breaking)` is reported by the caller as a *missing*
# section rather than an empty one, which is the accurate diagnostic.
#
# $1 = the guide to check, $2 = the template to compare against.
guide_section_filled() {
  local file="$1" template="$2"
  awk -v heading="$PLUGIN_SECTION" '
    function strip(line,   out) {
      out = line
      if (in_comment) {
        if (out ~ /-->/) { sub(/^.*-->/, "", out); in_comment = 0 } else { return "" }
      }
      gsub(/<!--[^>]*-->/, "", out)
      if (out ~ /<!--/) { sub(/<!--.*$/, "", out); in_comment = 1 }
      return out
    }
    # Reduce a line to the substance a reader would take from it: drop the
    # bullet marker, the bold label, every {placeholder}, and punctuation.
    function substance(line,   out) {
      out = line
      sub(/^[[:space:]]*[-*][[:space:]]*/, "", out)
      sub(/^\*\*[^*]*\*\*/, "", out)
      gsub(/\{[^}]*\}/, "", out)
      gsub(/[[:space:]]|[-*`:.,()\/]/, "", out)
      return out
    }
    function key(line,   out) {
      out = line
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", out)
      return out
    }
    FNR == NR {
      if ($0 == heading) { t_inside = 1; next }
      if (t_inside && /^## /) { t_inside = 0 }
      if (t_inside && length(key($0)) > 0) { template_line[key($0)] = 1 }
      next
    }
    $0 == heading    { inside = 1; next }
    inside && /^## / { inside = 0 }
    !inside          { next }
    {
      line = strip($0)
      if (length(substance(line)) == 0) next
      if (key(line) in template_line) next
      found = 1
    }
    END { exit(found ? 0 : 1) }
  ' "$template" "$file"
}

# Which of SURFACE_PATHS (and NEXT_GUIDE) this branch touched, one per line.
#
# Measured from the MERGE BASE with the base ref against the WORKING TREE, so it
# sees committed and uncommitted changes alike — a contributor gets the answer
# before they push, and CI (where everything is committed) gets the same one.
# Untracked files count too: a brand-new reference plugin is exactly the kind of
# surface change this is looking for, and `git diff` never mentions it.
#
# Exit 2 means "could not compute" (no merge base — a shallow clone). The caller
# treats that as a FAILURE, never as "nothing changed": the two must not share
# an exit path, or a surface change passes silently on precisely the CI
# configuration this gate runs in.
changed_paths() {
  local root="$1" base="$2" merge_base pattern
  merge_base="$(git -C "$root" merge-base "$base" HEAD 2>/dev/null)" || return 2
  [[ -n "$merge_base" ]] || return 2
  pattern="$(printf '%s\n' "${SURFACE_PATHS[@]}" "$NEXT_GUIDE" | paste -sd'|' -)"
  {
    git -C "$root" diff --name-only "$merge_base" 2>/dev/null
    git -C "$root" ls-files --others --exclude-standard 2>/dev/null
  } | grep -E "^($pattern)$" | LC_ALL=C sort -u
}

check_migration_guide() {
  local root="$1"
  local base="${BASE_REF:-origin/trunk-dev}"

  if ! git -C "$root" rev-parse --verify --quiet "$base" >/dev/null 2>&1; then
    printf '\033[0;33m\xe2\x88\x92\033[0m migration guide: base ref %s is unavailable; skipping the surface-change check\n' "$base"
    return
  fi

  local touched rc path surface_touched=""
  touched="$(changed_paths "$root" "$base")" && rc=0 || rc=$?
  if [[ "$rc" -eq 2 ]]; then
    fail "cannot compute a diff against $base (no merge base — a shallow clone?), so a plugin-surface change cannot be ruled out"
    fail "  in CI, check out with 'fetch-depth: 0'; locally, run 'git fetch --unshallow origin'"
    return
  fi

  for path in "${SURFACE_PATHS[@]}"; do
    if grep -qxF "$path" <<<"$touched"; then
      surface_touched="yes"
      break
    fi
  done

  if [[ -z "$surface_touched" ]]; then
    pass "migration guide: this change does not touch the declared plugin surface"
    return
  fi

  # `next.md` is a ROLLING draft that survives across PRs, so "it contains a
  # Plugin authors section" is satisfied forever by whichever PR wrote one
  # first. Requiring THIS change to touch the file is what makes the gate bite
  # on every subsequent surface change.
  if ! grep -qxF "$NEXT_GUIDE" <<<"$touched"; then
    fail "this change touches the declared plugin surface but does not touch $NEXT_GUIDE"
    fail "  add what it means for plugin authors under '$PLUGIN_SECTION' (copy the section from $TEMPLATE_FILE)"
    return
  fi

  if [[ ! -f "$root/$NEXT_GUIDE" ]]; then
    fail "$NEXT_GUIDE is missing, and this change touches the declared plugin surface"
    return
  fi

  if ! grep -qxF "$PLUGIN_SECTION" "$root/$NEXT_GUIDE"; then
    fail "this change touches the declared plugin surface, so $NEXT_GUIDE needs a '$PLUGIN_SECTION' section, spelled exactly (copy it from $TEMPLATE_FILE)"
    return
  fi

  if ! guide_section_filled "$root/$NEXT_GUIDE" "$root/$TEMPLATE_FILE"; then
    fail "$NEXT_GUIDE's '$PLUGIN_SECTION' section says nothing this change did not inherit from the template — only blanks, comments, placeholders, or the template's own words. Say what changed for plugin authors and what to do about it."
    return
  fi

  pass "migration guide: the plugin surface changed and $NEXT_GUIDE says so"
}

run_checks() {
  local root="$1"
  check_registry "$root"
  check_stable_baseline "$root"
  check_docs_parity "$root"
  check_reference_coverage "$root"
  check_workspace_membership "$root"
  check_template_section "$root"
  check_migration_guide "$root"
}

# ── Self-test ──────────────────────────────────────────────────────────────
#
# Each scenario builds a synthetic tree with ONE defect and asserts the script
# rejects it for the right reason, plus a clean tree it must accept.

SELF_TEST_DIR=""

cleanup() {
  [[ -n "$SELF_TEST_DIR" && -d "$SELF_TEST_DIR" ]] && rm -rf "$SELF_TEST_DIR"
  return 0
}
trap cleanup EXIT

make_fixture() {
  local dir="$1"
  mkdir -p "$dir/autumn/src" "$dir/docs/migrations" "$dir/autumn-plugin-reference/src" "$dir/scripts"

  cat >"$dir/$REGISTRY_FILE" <<'EOF'
pub const PLUGIN_SURFACES: &[PluginSurface] = &[
    PluginSurface {
        name: "AppBuilder::nest",
        tier: SurfaceTier::Stable,
        note: "Mount a raw axum router under a prefix.",
    },
    PluginSurface {
        name: "AppBuilder::with_edge_kv",
        tier: SurfaceTier::Experimental,
        note: "Edge-capsule KV binding; may change in any release.",
    },
    PluginSurface {
        name: "Plugin::build",
        tier: SurfaceTier::Stable,
        note: "Apply the plugin's wiring to the builder.",
    },
];
EOF

  cat >"$dir/$DOCS_FILE" <<'EOF'
# Autumn Plugins

### The declared surface

| API | Tier | Notes |
|-----|------|-------|
| `AppBuilder::nest` | stable | Mount a raw axum router under a prefix. |
| `AppBuilder::with_edge_kv` | experimental | Edge-capsule KV binding; may change in any release. |
| `Plugin::build` | stable | Apply the plugin's wiring to the builder. |

## Something else
EOF

  cat >"$dir/$REFERENCE_FILE" <<'EOF'
impl Plugin for ReferencePlugin {
    // surface: Plugin::build
    fn build(self, app: AppBuilder) -> AppBuilder {
        // surface: AppBuilder::nest
        app.nest("/x", Router::new())
    }
}
EOF

  printf 'AppBuilder::nest\nPlugin::build\n' >"$dir/$BASELINE_FILE"
  printf '## Plugin authors\n\nNothing changed for plugin authors.\n' >"$dir/$TEMPLATE_FILE"
  printf '## Plugin authors\n\n- **Stable surface changed:** none.\n' >"$dir/$NEXT_GUIDE"
  printf 'members = ["autumn", "autumn-plugin-reference"]\n' >"$dir/$WORKSPACE_MANIFEST"
}

# Run the checks against a fixture, with the ratchet relaxed to the fixture's
# size and the migration-guide check neutralised (fixtures are not git repos).
self_test_run() {
  local dir="$1"
  ( BASE_REF="__no_such_ref__" failures=0
    check_registry "$dir"
    check_stable_baseline "$dir"
    check_docs_parity "$dir"
    check_reference_coverage "$dir"
    check_workspace_membership "$dir"
    check_template_section "$dir"
    [[ "$failures" -eq 0 ]]
  ) >/dev/null 2>&1
}

expect_fail() {
  local label="$1" dir="$2"
  if self_test_run "$dir"; then
    die "self-test '$label' PASSED but should have failed"
  fi
}

expect_pass() {
  local label="$1" dir="$2"
  if ! self_test_run "$dir"; then
    printf '\033[0;31mself-test %s failed; the checks reported:\033[0m\n' "$label" >&2
    ( BASE_REF="__no_such_ref__" failures=0
      check_registry "$dir"; check_stable_baseline "$dir"; check_docs_parity "$dir"
      check_reference_coverage "$dir"; check_workspace_membership "$dir"
      check_template_section "$dir" ) >&2 || true
    die "self-test '$label' should have passed but failed"
  fi
}

self_test() {
  SELF_TEST_DIR="$(mktemp -d)"
  local base="$SELF_TEST_DIR/clean"
  make_fixture "$base"
  expect_pass "clean fixture" "$base"

  local d

  d="$SELF_TEST_DIR/docs-drift"; cp -r "$base" "$d"
  sed -i.bak 's/| `Plugin::build` | stable |/| `Plugin::build` | experimental |/' "$d/$DOCS_FILE"
  expect_fail "docs table declares a different tier than the registry" "$d"

  d="$SELF_TEST_DIR/docs-missing-row"; cp -r "$base" "$d"
  sed -i.bak '/`Plugin::build`/d' "$d/$DOCS_FILE"
  expect_fail "docs table missing a registry entry" "$d"

  d="$SELF_TEST_DIR/docs-extra-row"; cp -r "$base" "$d"
  awk '/^\| `Plugin::build`/ { print; print "| `AppBuilder::ghost` | stable | Not in the registry. |"; next } { print }' \
    "$base/$DOCS_FILE" >"$d/$DOCS_FILE"
  expect_fail "docs table row the registry does not declare" "$d"

  d="$SELF_TEST_DIR/uncovered"; cp -r "$base" "$d"
  sed -i.bak '/\/\/ surface: AppBuilder::nest/d' "$d/$REFERENCE_FILE"
  expect_fail "stable surface with no reference-plugin call site" "$d"

  d="$SELF_TEST_DIR/extra-marker"; cp -r "$base" "$d"
  printf '    // surface: AppBuilder::ghost\n' >>"$d/$REFERENCE_FILE"
  expect_fail "reference-plugin marker the registry does not declare" "$d"

  d="$SELF_TEST_DIR/no-markers"; cp -r "$base" "$d"
  sed -i.bak '/\/\/ surface:/d' "$d/$REFERENCE_FILE"
  expect_fail "reference plugin has no markers at all" "$d"

  d="$SELF_TEST_DIR/empty-note"; cp -r "$base" "$d"
  sed -i.bak 's|note: "Apply the plugin.s wiring to the builder.",|note: "",|' "$d/$REGISTRY_FILE"
  expect_fail "registry entry with an empty note" "$d"

  d="$SELF_TEST_DIR/unsorted"; cp -r "$base" "$d"
  cat >"$d/$REGISTRY_FILE" <<'EOF'
pub const PLUGIN_SURFACES: &[PluginSurface] = &[
    PluginSurface {
        name: "Plugin::build",
        tier: SurfaceTier::Stable,
        note: "Apply the plugin's wiring to the builder.",
    },
    PluginSurface {
        name: "AppBuilder::nest",
        tier: SurfaceTier::Stable,
        note: "Mount a raw axum router under a prefix.",
    },
    PluginSurface {
        name: "AppBuilder::with_edge_kv",
        tier: SurfaceTier::Experimental,
        note: "Edge-capsule KV binding; may change in any release.",
    },
];
EOF
  expect_fail "registry out of sorted order" "$d"

  d="$SELF_TEST_DIR/duplicate"; cp -r "$base" "$d"
  python3 - "$d/$REGISTRY_FILE" <<'PY' 2>/dev/null || perl -0pi -e 's/\];/    PluginSurface {\n        name: "Plugin::build",\n        tier: SurfaceTier::Stable,\n        note: "Duplicate.",\n    },\n];/' "$d/$REGISTRY_FILE"
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("];", '    PluginSurface {\n        name: "Plugin::build",\n        tier: SurfaceTier::Stable,\n        note: "Duplicate.",\n    },\n];')
open(p, "w").write(s)
PY
  expect_fail "duplicate registry name" "$d"

  d="$SELF_TEST_DIR/unparseable"; cp -r "$base" "$d"
  printf 'pub const SOMETHING_ELSE: u8 = 0;\n' >"$d/$REGISTRY_FILE"
  expect_fail "registry the parser cannot read" "$d"

  # The swap a count-only ratchet never sees: demote one baselined stable
  # surface to experimental and add another, leaving the total unchanged. Every
  # other check stays green — the docs table matches, and the reference plugin
  # still compiles because the call site simply moved.
  d="$SELF_TEST_DIR/tier-demotion"; cp -r "$base" "$d"
  cat >"$d/$REGISTRY_FILE" <<'FIXTURE'
pub const PLUGIN_SURFACES: &[PluginSurface] = &[
    PluginSurface {
        name: "AppBuilder::merge",
        tier: SurfaceTier::Stable,
        note: "Merge a raw axum router at the root.",
    },
    PluginSurface {
        name: "AppBuilder::nest",
        tier: SurfaceTier::Experimental,
        note: "Demoted; may change in any release.",
    },
    PluginSurface {
        name: "AppBuilder::with_edge_kv",
        tier: SurfaceTier::Experimental,
        note: "Edge-capsule KV binding; may change in any release.",
    },
    PluginSurface {
        name: "Plugin::build",
        tier: SurfaceTier::Stable,
        note: "Apply the plugin's wiring to the builder.",
    },
];
FIXTURE
  cat >"$d/$DOCS_FILE" <<'FIXTURE'
# Autumn Plugins

### The declared surface

| API | Tier | Notes |
|-----|------|-------|
| `AppBuilder::merge` | stable | Merge a raw axum router at the root. |
| `AppBuilder::nest` | experimental | Demoted; may change in any release. |
| `AppBuilder::with_edge_kv` | experimental | Edge-capsule KV binding; may change in any release. |
| `Plugin::build` | stable | Apply the plugin's wiring to the builder. |

## Something else
FIXTURE
  cat >"$d/$REFERENCE_FILE" <<'FIXTURE'
impl Plugin for ReferencePlugin {
    // surface: Plugin::build
    fn build(self, app: AppBuilder) -> AppBuilder {
        // surface: AppBuilder::merge
        app.merge(Router::new())
    }
}
FIXTURE
  expect_fail "a baselined stable surface demoted to experimental" "$d"

  d="$SELF_TEST_DIR/baseline-removed"; cp -r "$base" "$d"
  rm -f "$d/$BASELINE_FILE"
  expect_fail "the stable-surface baseline file deleted" "$d"

  d="$SELF_TEST_DIR/baseline-emptied"; cp -r "$base" "$d"
  printf '# only comments\n' >"$d/$BASELINE_FILE"
  expect_fail "the stable-surface baseline emptied" "$d"

  d="$SELF_TEST_DIR/baseline-stale"; cp -r "$base" "$d"
  printf 'AppBuilder::nest\n' >"$d/$BASELINE_FILE"
  expect_fail "a stable surface missing from the baseline" "$d"

  d="$SELF_TEST_DIR/baseline-unsorted"; cp -r "$base" "$d"
  printf 'Plugin::build\nAppBuilder::nest\n' >"$d/$BASELINE_FILE"
  expect_fail "an unsorted baseline" "$d"

  d="$SELF_TEST_DIR/not-a-member"; cp -r "$base" "$d"
  printf 'members = ["autumn"]\n' >"$d/$WORKSPACE_MANIFEST"
  expect_fail "reference crate dropped from the workspace" "$d"

  d="$SELF_TEST_DIR/template-lost-section"; cp -r "$base" "$d"
  printf '## Compiler error cheat sheet\n' >"$d/$TEMPLATE_FILE"
  expect_fail "migration template lost its Plugin authors section" "$d"

  d="$SELF_TEST_DIR/no-table"; cp -r "$base" "$d"
  printf '# Autumn Plugins\n\nNo table here.\n' >"$d/$DOCS_FILE"
  expect_fail "docs page has no declared-surface table" "$d"

  # `guide_section_filled` is exercised directly: it is the only part of the
  # migration-guide check that does not need a git history.
  local guide="$SELF_TEST_DIR/guide.md"
  local template="$SELF_TEST_DIR/template.md"
  cat >"$template" <<'FIXTURE'
## Plugin authors

Everything here is addressed to someone maintaining a plugin crate.

- **Stable surface changed:** {the API, what changed, and the replacement}
- **New stable surface:** {additive APIs — or "none"}

## Next
FIXTURE

  printf '## Plugin authors\n\n<!-- TODO -->\n\n## Next\n' >"$guide"
  guide_section_filled "$guide" "$template" && die "self-test 'HTML-comment-only section' PASSED but should have failed"

  printf '## Plugin authors\n\n<!--\nTODO\nlater\n-->\n\n## Next\n' >"$guide"
  guide_section_filled "$guide" "$template" && die "self-test 'multi-line-comment-only section' PASSED but should have failed"

  printf '## Plugin authors\n\n- **Stable surface changed:** {the API, what happened}\n\n## Next\n' >"$guide"
  guide_section_filled "$guide" "$template" && die "self-test 'placeholder-only bullet' PASSED but should have failed"

  # The one that matters: the template pasted back verbatim, prose and all.
  cp "$template" "$guide"
  guide_section_filled "$guide" "$template" && die "self-test 'template pasted verbatim' PASSED but should have failed"

  printf '## Something\n\ntext\n' >"$guide"
  guide_section_filled "$guide" "$template" && die "self-test 'absent Plugin authors section' PASSED but should have failed"

  printf '## Plugin authors\n\n- **Stable surface changed:** none.\n\n## Next\n' >"$guide"
  guide_section_filled "$guide" "$template" || die "self-test 'filled Plugin authors section' should have passed"

  printf '## Plugin authors\n\nEverything here is addressed to someone maintaining a plugin crate.\n\n- **Stable surface changed:** `AppBuilder::nest` was removed.\n\n## Next\n' >"$guide"
  guide_section_filled "$guide" "$template" || die "self-test 'real content beside template prose' should have passed"

  printf '\033[0;32m\xe2\x9c\x93\033[0m self-test: every scenario behaved as expected\n'
}

# ── Entry point ────────────────────────────────────────────────────────────

mode="${1:---all}"
case "$mode" in
  --self-test|--check-only|--all) ;;
  *) die "unknown mode '$mode' (expected --self-test, --check-only, or --all)" ;;
esac

if [[ "$mode" != "--check-only" ]]; then
  self_test
fi

if [[ "$mode" != "--self-test" ]]; then
  echo "==> Checking the declared plugin API surface (issue #1601)"
  run_checks "$REPO_ROOT"
  if [[ "$failures" -gt 0 ]]; then
    printf '\n\033[0;31m%s failure(s).\033[0m See docs/plugins.md "The plugin API contract".\n' "$failures" >&2
    exit 1
  fi
  printf '\n\033[0;32mThe declared plugin surface is consistent.\033[0m\n'
fi
