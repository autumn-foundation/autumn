#!/usr/bin/env bash
# Verify that every publishable crate carries the required crates.io metadata
# fields, a rust-version declaration, and version-pinned intra-workspace
# dependency edges that match the current workspace version.
#
# Required fields per crate:
#   description, homepage, repository, readme, license, keywords, categories,
#   rust-version (direct pin or workspace inheritance)
#
# Version pin check:
#   Any dependency on another Autumn crate must pin the exact workspace version
#   so crates.io consumers get a coherent set (e.g. autumn-admin-plugin must
#   declare autumn-web = { version = "X.Y.Z" } where X.Y.Z matches [workspace.package].version).
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-crate-metadata.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() {
  echo "error: $*" >&2
  exit 1
}

warn() {
  echo "warn:  $*" >&2
}

ok() {
  echo "ok:    $*"
}

# Publishable crates: (package-name, manifest-path)
declare -a CRATES=(
  "autumn-web:autumn/Cargo.toml"
  "autumn-macros:autumn-macros/Cargo.toml"
  "autumn-cli:autumn-cli/Cargo.toml"
  "autumn-admin-plugin:autumn-admin-plugin/Cargo.toml"
  "autumn-storage-s3:autumn-storage-s3/Cargo.toml"
  "autumn-cache-redis:autumn-cache-redis/Cargo.toml"
  "autumn-search:autumn-search/Cargo.toml"
)

# Required fields that must appear in [package] (inline, quoted, array, or
# dotted-key workspace inheritance: field.workspace = true).
REQUIRED_FIELDS=(description homepage repository readme license keywords categories rust-version)

failures=0

# Returns non-empty if the field is present in [package] in any recognised form:
#   field = "value"
#   field = [...]
#   field.workspace = true
field_present_in_package() {
  local manifest="$1"
  local key="$2"
  awk -v key="$key" '
    /^\[package\]/               { in_pkg = 1; next }
    /^\[/ && in_pkg              { in_pkg = 0 }
    in_pkg && $0 ~ "^[[:space:]]*" key "([.[:space:]]|$)" {
      print $0
      exit
    }
  ' "$manifest"
}

for entry in "${CRATES[@]}"; do
  name="${entry%%:*}"
  manifest="${entry##*:}"

  echo ""
  echo "==> $name ($manifest)"

  if [[ ! -f "$manifest" ]]; then
    warn "$manifest not found — skipping"
    continue
  fi

  crate_ok=true

  for field in "${REQUIRED_FIELDS[@]}"; do
    val="$(field_present_in_package "$manifest" "$field")"
    if [[ -z "$val" ]]; then
      warn "  missing field: $field"
      crate_ok=false
      failures=$((failures + 1))
    else
      ok "  $field present"
    fi
  done

  # readme file must exist on disk if declared inline (not workspace-inherited).
  readme_line="$(field_present_in_package "$manifest" "readme")"
  if echo "$readme_line" | grep -qv '\.workspace'; then
    # Extract the path value between quotes
    readme_path="$(echo "$readme_line" | sed 's/.*=\s*"\(.*\)".*/\1/')"
    if [[ -n "$readme_path" && "$readme_path" != "$readme_line" ]]; then
      crate_dir="$(dirname "$manifest")"
      if [[ "$readme_path" == /* ]]; then
        readme_abs="$readme_path"
      else
        readme_abs="$crate_dir/$readme_path"
      fi
      if [[ ! -f "$readme_abs" ]]; then
        warn "  readme file not found on disk: $readme_abs"
        crate_ok=false
        failures=$((failures + 1))
      else
        ok "  readme file exists: $readme_abs"
      fi
    fi
  fi

  $crate_ok && echo "  PASS" || echo "  FAIL"
done

# ---------------------------------------------------------------------------
# Inter-crate version pin alignment
# Every publishable crate that depends on another Autumn crate must pin the
# exact workspace version so crates.io consumers always get a coherent set.
# ---------------------------------------------------------------------------
echo ""
echo "==> Checking inter-crate Autumn dependency version pins"

workspace_version="$(
  awk '
    /^\[workspace\.package\]/ { in_block = 1; next }
    /^\[/ && in_block         { in_block = 0 }
    in_block && /^version/    {
      match($0, /"[^"]+"/)
      print substr($0, RSTART + 1, RLENGTH - 2)
      exit
    }
  ' Cargo.toml
)"
[[ -n "$workspace_version" ]] || die "could not parse workspace version from Cargo.toml"
echo "workspace version = $workspace_version"

# Derived from CRATES to avoid manual duplication.
AUTUMN_CRATE_NAMES=("${CRATES[@]%%:*}")

for entry in "${CRATES[@]}"; do
  name="${entry%%:*}"
  manifest="${entry##*:}"
  [[ -f "$manifest" ]] || continue

  for dep in "${AUTUMN_CRATE_NAMES[@]}"; do
    [[ "$dep" == "$name" ]] && continue  # skip self-reference

    # Look for the dep in [dependencies] / [dev-dependencies] / [build-dependencies].
    # We only care about non-path-only declarations (i.e. ones that need a version
    # for crates.io consumers).
    dep_line="$(grep -E "^[[:space:]]*${dep}[[:space:]]*=" "$manifest" 2>/dev/null || true)"
    [[ -z "$dep_line" ]] && continue  # crate doesn't depend on this Autumn crate

    # Extract the version string if present.
    pinned_ver="$(echo "$dep_line" | grep -oE 'version[[:space:]]*=[[:space:]]*"[^"]+"' | grep -oE '"[^"]+"' | tr -d '"' || true)"
    if [[ -z "$pinned_ver" ]]; then
      warn "  $name: $dep dependency has no version pin (add version = \"$workspace_version\")"
      failures=$((failures + 1))
    elif [[ "$pinned_ver" != "$workspace_version" ]]; then
      warn "  $name: $dep version pin is \"$pinned_ver\" but workspace version is \"$workspace_version\""
      failures=$((failures + 1))
    else
      ok "  $name: $dep = \"$pinned_ver\" matches workspace version"
    fi
  done
done

echo ""
if [[ "$failures" -gt 0 ]]; then
  die "$failures issue(s) found across publishable crates. Fix them before publishing."
fi

echo "Crate metadata OK — all publishable crates have required fields and coherent version pins."
