#!/usr/bin/env bash
# Shared EXAMPLES.md catalog-marker parsing.
#
# Sourced by scripts/check-examples.sh (the catalog-drift gate) and
# scripts/check-examples-e2e.sh (the fan-out e2e gate) so the marker-format
# regex lives in exactly one place. Each marker line has the form:
#
#   <!-- catalog:example name=<dir> tier=<tier> -->
#
# This file only defines functions — sourcing it has no side effects, unlike
# check-examples.sh itself (which runs its full gate top-to-bottom on load).

# Extract cataloged example names for one tier.
# Usage: catalog_names_by_tier <catalog-file> <tier>
catalog_names_by_tier() {
  local catalog="$1"
  local tier="$2"
  grep -E "<!-- catalog:example name=[^ ]+ tier=${tier}" "$catalog" \
    | grep -oE 'name=[^ >]+' \
    | sed 's/name=//' \
    || true
}

# Extract every cataloged example name regardless of tier.
# Usage: all_catalog_names <catalog-file>
all_catalog_names() {
  local catalog="$1"
  grep -E "<!-- catalog:example name=" "$catalog" \
    | grep -oE 'name=[^ >]+' \
    | sed 's/name=//' \
    || true
}
