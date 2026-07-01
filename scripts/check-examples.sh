#!/usr/bin/env bash
# Example catalog drift gate.
#
# Checks that the examples catalog (EXAMPLES.md) is coherent with:
#   1. Every directory under examples/ is cataloged.
#   2. Every workspace examples/* member is cataloged as a supported example.
#   3. Every example in the README.md Examples table appears in the catalog.
#   4. Each supported example has a README.md with the required quickstart sections.
#
# The catalog format uses machine-readable HTML comment markers:
#
#   <!-- catalog:example name=<dir> tier=supported -->
#   <!-- catalog:example name=<dir> tier=excluded -->
#   <!-- catalog:example name=<dir> tier=experimental -->
#
# Output is grouped by support tier so contributors can immediately see which
# failures block release and which are intentionally manual.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-examples.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

CATALOG="EXAMPLES.md"
EXAMPLES_DIR="examples"
WORKSPACE_MANIFEST="Cargo.toml"
README="README.md"

# Required sections in every supported example README (case-insensitive grep).
REQUIRED_README_SECTIONS=(
  "## Prerequisites"
  "## Quick"
)

die() {
  echo "error: $*" >&2
  exit 1
}

ok() {
  echo "ok:    $*"
}

warn() {
  echo "warn:  $*" >&2
}

fail() {
  echo "FAIL:  $*" >&2
  failures=$((failures + 1))
}

failures=0

# ---------------------------------------------------------------------------
# 0. Catalog must exist
# ---------------------------------------------------------------------------
echo "==> Checking catalog file exists: $CATALOG"
if [[ ! -f "$CATALOG" ]]; then
  fail "catalog file '$CATALOG' not found — create it to pass this gate"
  echo ""
  echo "Example drift gate: $failures failure(s) found."
  die "$failures failure(s) — see output above. Create EXAMPLES.md to begin."
fi
ok "catalog file found"
echo ""

# ---------------------------------------------------------------------------
# Helper: extract cataloged example names by tier from EXAMPLES.md — shared
# with scripts/check-examples-e2e.sh via scripts/lib/catalog.sh so the
# marker-format regex lives in exactly one place.
# ---------------------------------------------------------------------------
source "$root/scripts/lib/catalog.sh"

# ---------------------------------------------------------------------------
# 1. Every examples/ directory must be cataloged
# ---------------------------------------------------------------------------
echo "==> Checking every examples/ directory is cataloged"

mapfile -t example_dirs < <(find "$EXAMPLES_DIR" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
mapfile -t catalog_all < <(all_catalog_names "$CATALOG" | sort)

for dir in "${example_dirs[@]}"; do
  if printf '%s\n' "${catalog_all[@]}" | grep -qx "$dir"; then
    ok "  examples/$dir is cataloged"
  else
    fail "  examples/$dir has no catalog entry in $CATALOG"
  fi
done
echo ""

# ---------------------------------------------------------------------------
# 2. Every workspace examples/* member must be cataloged as supported
# ---------------------------------------------------------------------------
echo "==> Checking workspace examples/* members are cataloged as supported"

mapfile -t workspace_examples < <(
  # Extract only paths from the [workspace] members array, not from exclude or
  # other sections. awk tracks section and key boundaries so that an
  # "examples/foo" entry under `exclude` is not misread as a workspace member.
  awk '
    /^\[workspace\]/                    { in_ws = 1; in_members = 0; next }
    /^\[/                               { in_ws = 0; in_members = 0 }
    in_ws && /^members/                 { in_members = 1 }
    in_ws && /^[a-z]/ && !/^members/   { in_members = 0 }
    in_members && /examples\//          { print }
  ' "$WORKSPACE_MANIFEST" \
    | grep -oE 'examples/[a-zA-Z0-9_-]+' \
    | sed 's|examples/||' \
    | sort
)
mapfile -t catalog_supported < <(catalog_names_by_tier "$CATALOG" "supported" | sort)

for member in "${workspace_examples[@]}"; do
  if printf '%s\n' "${catalog_supported[@]}" | grep -qx "$member"; then
    ok "  workspace member examples/$member is cataloged as supported"
  else
    fail "  workspace member examples/$member is NOT cataloged as supported in $CATALOG"
  fi
done

# Inverse: every supported catalog entry must be a workspace member so it
# participates in normal compilation and test validation.
for ex in "${catalog_supported[@]}"; do
  if printf '%s\n' "${workspace_examples[@]}" | grep -qx "$ex"; then
    ok "  supported catalog entry '$ex' is a workspace member"
  else
    fail "  supported catalog entry '$ex' is NOT in Cargo.toml workspace members (add it or change its catalog tier)"
  fi
done
echo ""

# ---------------------------------------------------------------------------
# 3. Every README.md Examples table entry must appear in the catalog
# ---------------------------------------------------------------------------
echo "==> Checking README.md Examples table entries appear in catalog"

# Match the URL portion of markdown links: both [`examples/foo`](...) and [examples/foo](...).
# Extracting from the URL (not the link text) handles both link styles unambiguously.
mapfile -t readme_examples < <(
  grep -oE '\(examples/[a-zA-Z0-9_-]+\)' "$README" \
    | tr -d '()' \
    | sed 's|examples/||' \
    | sort -u
)

for ex in "${readme_examples[@]}"; do
  if printf '%s\n' "${catalog_all[@]}" | grep -qx "$ex"; then
    ok "  README example '$ex' found in catalog"
  else
    fail "  README example '$ex' is NOT in the catalog"
  fi
done

# Inverse: every supported catalog entry must also have a README table row.
for ex in "${catalog_supported[@]}"; do
  if printf '%s\n' "${readme_examples[@]}" | grep -qx "$ex"; then
    ok "  supported catalog entry '$ex' is listed in README table"
  else
    fail "  supported catalog entry '$ex' is NOT in the README Examples table (add a row)"
  fi
done
echo ""

# ---------------------------------------------------------------------------
# 3b. Every examples/* path referenced in docs/guide/ must appear in the catalog
# ---------------------------------------------------------------------------
echo "==> Checking docs/guide/ example references appear in catalog"

DOCS_GUIDE="docs/guide"
if [[ -d "$DOCS_GUIDE" ]]; then
  mapfile -t docs_examples < <(
    grep -rhoE 'examples/[a-zA-Z0-9_-]+' "$DOCS_GUIDE" \
      | sed 's|examples/||' \
      | sort -u
  )
  for ex in "${docs_examples[@]}"; do
    # Skip references that are just path fragments matching no real directory.
    [[ -d "$EXAMPLES_DIR/$ex" ]] || continue
    if printf '%s\n' "${catalog_all[@]}" | grep -qx "$ex"; then
      ok "  docs/guide/ reference '$ex' found in catalog"
    else
      fail "  docs/guide/ reference 'examples/$ex' is NOT in the catalog"
    fi
  done
else
  warn "docs/guide/ directory not found — skipping guide reference check"
fi
echo ""

# ---------------------------------------------------------------------------
# 4. Every supported example must have a README.md with required sections
# ---------------------------------------------------------------------------
echo "==> Checking supported examples have README.md with required sections"

for ex in "${catalog_supported[@]}"; do
  readme="$EXAMPLES_DIR/$ex/README.md"
  if [[ ! -f "$readme" ]]; then
    fail "  examples/$ex has no README.md"
    continue
  fi

  for section in "${REQUIRED_README_SECTIONS[@]}"; do
    if grep -qi "$section" "$readme"; then
      ok "  examples/$ex: section '$section' present"
    else
      fail "  examples/$ex: README.md missing section '$section'"
    fi
  done
done
echo ""

# ---------------------------------------------------------------------------
# 5. Summary grouped by tier
#
# Failures in SUPPORTED examples block release.
# EXPERIMENTAL and EXCLUDED examples are informational only; they do not
# participate in normal workspace validation.
# ---------------------------------------------------------------------------
echo "==> Catalog summary by support tier"

mapfile -t _supported_summary  < <(catalog_names_by_tier "$CATALOG" "supported"    | sort)
mapfile -t _exp_summary        < <(catalog_names_by_tier "$CATALOG" "experimental" | sort)
mapfile -t _excl_summary       < <(catalog_names_by_tier "$CATALOG" "excluded"     | sort)

_count_supported=${#_supported_summary[@]}
echo ""
echo "  SUPPORTED  (failures here block release):"
for ex in "${_supported_summary[@]}"; do
  echo "    examples/$ex"
done
echo "  ($( [[ $_count_supported -eq 0 ]] && echo "none" || echo "$_count_supported example(s)" ))"

echo ""
echo "  EXPERIMENTAL  (informational; not release-blocking):"
if [[ ${#_exp_summary[@]} -eq 0 ]]; then
  echo "    (none)"
else
  for ex in "${_exp_summary[@]}"; do
    echo "    examples/$ex"
  done
fi

echo ""
echo "  EXCLUDED  (intentionally out of adoption path):"
if [[ ${#_excl_summary[@]} -eq 0 ]]; then
  echo "    (none)"
else
  for ex in "${_excl_summary[@]}"; do
    echo "    examples/$ex"
  done
fi

echo ""
if [[ "$failures" -gt 0 ]]; then
  echo "Tip: run 'cat EXAMPLES.md' to see the catalog and 'cat scripts/check-examples.sh'" \
       "for the marker format." >&2
  die "$failures failure(s) found — fix catalog, README.md table, or workspace membership before publishing."
fi

echo "Example catalog drift gate: all checks passed."
