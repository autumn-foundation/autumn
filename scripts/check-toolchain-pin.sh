#!/usr/bin/env bash
# Verify that CI's Rust toolchain is pinned, and pinned consistently.
#
# Why this exists: CI used to float on `dtolnay/rust-toolchain@stable`, so a Rust
# release could turn every open PR red with no change in the repo (the 1.98 and
# 1.99 clippy rollovers, #2252 and #3082, and the trybuild goldens, which match
# the compiler's diagnostic text exactly). The toolchain is now pinned, and a
# separate scheduled workflow (`toolchain-drift.yml`) is the only thing allowed
# to float on `stable`, so the next release shows up there first.
#
# Sources checked:
#   - .github/RUST_TOOLCHAIN                  (canonical pin, `MAJOR.MINOR.PATCH`)
#   - Cargo.toml [workspace.package].rust-version   (MSRV, may also be pinned)
#   - .github/workflows/*.yml and *.yaml  (Actions accepts both)
#
# Rules for every `dtolnay/rust-toolchain@<ref>` in a workflow:
#   - `<ref>` is the canonical pin, the MSRV, `nightly`, or `master` (the
#     matrix form, whose `toolchain:` input is checked below);
#   - `<ref>` is `stable` only in toolchain-drift.yml;
#   - the one documented exception: publish-gate.yml pins the toolchain
#     cargo-semver-checks needs (see the comment there).
# A matrix job that keeps `stable` as its *label* (so the check name, e.g.
# `Compile-and-serve gates (stable)`, does not change) must map it with
# `matrix.toolchain == 'stable' && '<pin>' || matrix.toolchain`, and `<pin>`
# must be the canonical pin. A bare `toolchain: ${{ matrix.toolchain }}` or
# `toolchain: stable` would float again, so both are rejected.
#
# Called from the `msrv` job in ci.yml. Runs locally with:
#
#     ./scripts/check-toolchain-pin.sh
#     ./scripts/check-toolchain-pin.sh --self-test
#     ./scripts/check-toolchain-pin.sh --bump 1.100.0   # move the pin everywhere

set -euo pipefail
# An unmatched glob (no .yaml files, say) must expand to nothing, not to itself.
shopt -s nullglob

root="$(cd "$(dirname "$0")/.." && pwd)"

# The one place that may float on `stable`.
DRIFT_WORKFLOW="toolchain-drift.yml"
# file:ref pairs that are deliberately pinned to something else.
EXCEPTIONS=("publish-gate.yml:1.94.1")

die() {
  echo "error: $*" >&2
  exit 1
}

check_tree() {
  local dir="$1"
  cd "$dir"

  [[ -f .github/RUST_TOOLCHAIN ]] || die ".github/RUST_TOOLCHAIN is missing"
  local pin
  pin="$(tr -d '[:space:]' < .github/RUST_TOOLCHAIN)"
  [[ "$pin" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || die ".github/RUST_TOOLCHAIN must hold a full MAJOR.MINOR.PATCH version, got '$pin'"

  local msrv
  msrv="$(
    awk '
      /^\[workspace\.package\]/ { in_block = 1; next }
      /^\[/ && in_block       { in_block = 0 }
      in_block && /^rust-version/ {
        match($0, /"[^"]+"/)
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
      }
    ' Cargo.toml
  )"
  [[ -n "$msrv" ]] || die "could not find [workspace.package].rust-version in Cargo.toml"

  local failures=0
  local wf name line ref lineno exc ok
  for wf in .github/workflows/*.yml .github/workflows/*.yaml; do
    name="$(basename "$wf")"
    lineno=0
    while IFS= read -r line; do
      lineno=$((lineno + 1))
      # Comments may mention any ref (and do, to explain the history).
      [[ "$line" =~ ^[[:space:]]*# ]] && continue

      if [[ "$line" =~ dtolnay/rust-toolchain@([A-Za-z0-9._-]+) ]]; then
        ref="${BASH_REMATCH[1]}"
        ok=0
        case "$ref" in
          "$pin" | "$msrv" | nightly | master) ok=1 ;;
          stable) [[ "$name" == "$DRIFT_WORKFLOW" ]] && ok=1 ;;
        esac
        for exc in "${EXCEPTIONS[@]}"; do
          [[ "$name:$ref" == "$exc" ]] && ok=1
        done
        if ((ok == 0)); then
          echo "error: $wf:$lineno uses dtolnay/rust-toolchain@$ref; expected the pin ($pin)," \
            "the MSRV ($msrv), nightly, or master" >&2
          failures=$((failures + 1))
        fi
      fi

      if [[ "$line" =~ toolchain:[[:space:]]*\$\{\{[[:space:]]*matrix\.toolchain[[:space:]]*\}\} ]]; then
        echo "error: $wf:$lineno passes a bare matrix.toolchain, so a 'stable' leg floats;" \
          "map it: matrix.toolchain == 'stable' && '$pin' || matrix.toolchain" >&2
        failures=$((failures + 1))
      fi

      if [[ "$line" =~ matrix\.toolchain[[:space:]]*==[[:space:]]*\'stable\'[[:space:]]*\&\&[[:space:]]*\'([^\']+)\' ]]; then
        if [[ "${BASH_REMATCH[1]}" != "$pin" ]]; then
          echo "error: $wf:$lineno maps stable to ${BASH_REMATCH[1]}, but the pin is $pin" >&2
          failures=$((failures + 1))
        fi
      fi

      if [[ "$name" != "$DRIFT_WORKFLOW" && "$line" =~ ^[[:space:]]*toolchain:[[:space:]]*stable[[:space:]]*$ ]]; then
        echo "error: $wf:$lineno floats on 'toolchain: stable'; only $DRIFT_WORKFLOW may" >&2
        failures=$((failures + 1))
      fi
    done < "$wf"
  done

  if ((failures > 0)); then
    die "$failures toolchain pin problem(s); the pin lives in .github/RUST_TOOLCHAIN (see CONTRIBUTING.md, 'Bumping the toolchain')"
  fi
  echo "Toolchain pin OK: $pin (MSRV $msrv)"
}

# --- bump: move the pin and every reference to it, then re-check. ---
#
# All-or-nothing: the whole bump is staged in a scratch copy, the real checker
# runs on that copy, and only a copy that passes is written back. So a pin the
# checker recognises but the rewrite does not (an odd line shape, say) fails the
# bump with the working tree untouched, instead of leaving it half-moved.
bump_tree() {
  local dir="$1" new="$2"
  [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--bump needs MAJOR.MINOR.PATCH, got '$new'"
  cd "$dir"
  [[ -f .github/RUST_TOOLCHAIN ]] || die ".github/RUST_TOOLCHAIN is missing"
  local old old_re wf rel stage changed=()
  old="$(tr -d '[:space:]' < .github/RUST_TOOLCHAIN)"
  [[ "$old" != "$new" ]] || die "the pin is already $new"
  old_re="${old//./\\.}"

  stage="$(mktemp -d "${TMPDIR:-/tmp}/toolchain-pin-stage.XXXXXX")"
  # Expanded now: `stage` is local, and the trap fires when the script exits.
  trap "rm -rf '$stage'" EXIT
  mkdir -p "$stage/.github"
  cp -R .github/workflows "$stage/.github/workflows"
  cp Cargo.toml "$stage/Cargo.toml"
  printf '%s\n' "$new" > "$stage/.github/RUST_TOOLCHAIN"

  for wf in "$stage"/.github/workflows/*.yml "$stage"/.github/workflows/*.yaml; do
    rel="${wf#"$stage"/}"
    # Rewrite via a temp file, not `sed -i`: BSD/macOS sed takes the word after
    # -i as a backup suffix, so `sed -i -E` would swallow -E and match nothing.
    # Only the two constructs the checker recognises, never prose in a comment;
    # a trailing `# comment` on a pin line is kept. The delimiter is `|` because
    # the comment pattern contains a literal `#`.
    sed -E \
      -e "s|^([[:space:]]*-?[[:space:]]*uses:[[:space:]]*dtolnay/rust-toolchain@)${old_re}([[:space:]]+#.*)?[[:space:]]*\$|\1${new}\2|" \
      -e "s|(matrix\\.toolchain == 'stable' && ')${old_re}(')|\1${new}\2|" \
      "$wf" > "$wf.new"
    mv "$wf.new" "$wf"
    cmp -s "$rel" "$wf" || changed+=("$rel")
  done
  if ((${#changed[@]} == 0)); then
    rm -rf "$stage"
    die "no workflow references the pin $old; nothing to bump"
  fi
  if ! ( check_tree "$stage" > /dev/null ); then
    rm -rf "$stage"
    die "bumping to $new would leave the tree inconsistent (see above); nothing was changed"
  fi

  for rel in "${changed[@]}"; do
    cat "$stage/$rel" > "$rel"
  done
  printf '%s\n' "$new" > .github/RUST_TOOLCHAIN
  rm -rf "$stage"
  echo "Moved the pin $old -> $new in ${#changed[@]} workflow file(s)"
  check_tree "$dir"
}

# --- self-test: build tiny trees and check the gate fails where it should. ---
self_test() {
  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/toolchain-pin-test.XXXXXX")"
  # Expanded now: `tmp` is local, and the trap fires after this function returns.
  trap "rm -rf '$tmp'" EXIT

  mk() { # mk <dir> <pin> <workflow body> [file]
    local d="$1" pin="$2" body="$3" file="${4:-ci.yml}"
    mkdir -p "$d/.github/workflows"
    printf '%s\n' "$pin" > "$d/.github/RUST_TOOLCHAIN"
    printf '[workspace.package]\nrust-version = "1.88.0"\n' > "$d/Cargo.toml"
    printf '%s\n' "$body" > "$d/.github/workflows/$file"
  }
  expect_ok() {
    ( "$0" --check "$1" > /dev/null 2>&1 ) || die "self-test: '$2' should pass"
  }
  expect_fail() {
    if ( "$0" --check "$1" > /dev/null 2>&1 ); then die "self-test: '$2' should fail"; fi
  }

  mk "$tmp/ok" 1.99.0 '- uses: dtolnay/rust-toolchain@1.99.0
- uses: dtolnay/rust-toolchain@1.88.0
- uses: dtolnay/rust-toolchain@nightly
# dtolnay/rust-toolchain@stable is only mentioned here
- uses: dtolnay/rust-toolchain@master
  with:
    toolchain: ${{ matrix.toolchain == '"'stable'"' && '"'1.99.0'"' || matrix.toolchain }}'
  expect_ok "$tmp/ok" "pinned, MSRV, nightly, comment, mapped matrix"

  mk "$tmp/floating" 1.99.0 '- uses: dtolnay/rust-toolchain@stable'
  expect_fail "$tmp/floating" "@stable outside the drift workflow"

  mk "$tmp/drift" 1.99.0 '- uses: dtolnay/rust-toolchain@stable' "$DRIFT_WORKFLOW"
  expect_ok "$tmp/drift" "@stable inside the drift workflow"

  mk "$tmp/stale" 1.99.0 '- uses: dtolnay/rust-toolchain@1.98.0'
  expect_fail "$tmp/stale" "a pin that disagrees with RUST_TOOLCHAIN"

  mk "$tmp/bare" 1.99.0 '- uses: dtolnay/rust-toolchain@master
  with:
    toolchain: ${{ matrix.toolchain }}'
  expect_fail "$tmp/bare" "an unmapped matrix.toolchain"

  mk "$tmp/badmap" 1.99.0 '    toolchain: ${{ matrix.toolchain == '"'stable'"' && '"'1.98.0'"' || matrix.toolchain }}'
  expect_fail "$tmp/badmap" "a stable mapping that disagrees with the pin"

  mk "$tmp/literal" 1.99.0 '    toolchain: stable'
  expect_fail "$tmp/literal" "a literal 'toolchain: stable'"

  mk "$tmp/exception" 1.99.0 '- uses: dtolnay/rust-toolchain@1.94.1' publish-gate.yml
  expect_ok "$tmp/exception" "the documented semver-checks exception"

  mk "$tmp/short" 1.99 '- uses: dtolnay/rust-toolchain@1.99'
  expect_fail "$tmp/short" "a pin that is not MAJOR.MINOR.PATCH"


  # --bump moves the pin, the uses: lines and the stable mapping together, and
  # leaves the MSRV, nightly and the documented exception alone.
  mk "$tmp/bump" 1.99.0 '- uses: dtolnay/rust-toolchain@1.99.0
- uses: dtolnay/rust-toolchain@1.88.0
- uses: dtolnay/rust-toolchain@nightly
- uses: dtolnay/rust-toolchain@master
  with:
    toolchain: ${{ matrix.toolchain == '"'stable'"' && '"'1.99.0'"' || matrix.toolchain }}'
  ( "$0" --bump "$tmp/bump" 1.100.0 > /dev/null 2>&1 ) || die "self-test: --bump should succeed"
  [[ "$(tr -d '[:space:]' < "$tmp/bump/.github/RUST_TOOLCHAIN")" == "1.100.0" ]] \
    || die "self-test: --bump did not rewrite RUST_TOOLCHAIN"
  grep -q 'rust-toolchain@1.100.0$' "$tmp/bump/.github/workflows/ci.yml" \
    || die "self-test: --bump did not rewrite the uses: pin"
  grep -q "&& '1.100.0' ||" "$tmp/bump/.github/workflows/ci.yml" \
    || die "self-test: --bump did not rewrite the stable mapping"
  grep -q 'rust-toolchain@1.88.0$' "$tmp/bump/.github/workflows/ci.yml" \
    || die "self-test: --bump touched the MSRV"
  expect_ok "$tmp/bump" "a tree after --bump"
  if ( "$0" --bump "$tmp/bump" 1.100.0 > /dev/null 2>&1 ); then
    die "self-test: --bump to the current pin should fail"
  fi

  # A bump that finds nothing to rewrite must not move RUST_TOOLCHAIN on its own.
  mk "$tmp/orphan" 1.99.0 '- uses: dtolnay/rust-toolchain@nightly'
  if ( "$0" --bump "$tmp/orphan" 1.100.0 > /dev/null 2>&1 ); then
    die "self-test: --bump with no references should fail"
  fi
  [[ "$(tr -d '[:space:]' < "$tmp/orphan/.github/RUST_TOOLCHAIN")" == "1.99.0" ]] \
    || die "self-test: a failed --bump moved RUST_TOOLCHAIN"


  # GitHub Actions accepts .yaml as well as .yml; both are checked and bumped.
  mk "$tmp/yaml-floating" 1.99.0 '- uses: dtolnay/rust-toolchain@stable' ci.yaml
  expect_fail "$tmp/yaml-floating" "@stable in a .yaml workflow"

  mk "$tmp/yaml-bump" 1.99.0 '- uses: dtolnay/rust-toolchain@1.99.0' extra.yaml
  ( "$0" --bump "$tmp/yaml-bump" 1.100.0 > /dev/null 2>&1 ) || die "self-test: --bump should handle .yaml"
  grep -q 'rust-toolchain@1.100.0$' "$tmp/yaml-bump/.github/workflows/extra.yaml" \
    || die "self-test: --bump skipped a .yaml workflow"


  # A pin line with an inline comment is rewritten and the comment kept.
  mk "$tmp/comment" 1.99.0 '- uses: dtolnay/rust-toolchain@1.99.0 # reason
- uses: dtolnay/rust-toolchain@1.99.0'
  ( "$0" --bump "$tmp/comment" 1.100.0 > /dev/null 2>&1 ) || die "self-test: --bump should handle a trailing comment"
  grep -q 'rust-toolchain@1.100.0 # reason$' "$tmp/comment/.github/workflows/ci.yml" \
    || die "self-test: --bump dropped or skipped a pin line with a trailing comment"

  # A reference the checker recognises but the rewrite cannot reach must fail
  # the bump AND leave every file byte-identical (the all-or-nothing promise).
  mk "$tmp/unreachable" 1.99.0 '- uses: dtolnay/rust-toolchain@1.99.0
- run: echo prefix dtolnay/rust-toolchain@1.99.0 suffix'
  cp -R "$tmp/unreachable" "$tmp/unreachable.before"
  if ( "$0" --bump "$tmp/unreachable" 1.100.0 > /dev/null 2>&1 ); then
    die "self-test: --bump should fail when a reference cannot be rewritten"
  fi
  diff -r "$tmp/unreachable.before" "$tmp/unreachable" > /dev/null \
    || die "self-test: a failed --bump changed the tree"

  echo "check-toolchain-pin self-test OK"
}

case "${1:-}" in
  --self-test) self_test ;;
  --check) check_tree "${2:?--check needs a directory}" ;;
  --bump)
    if [[ $# -eq 3 ]]; then bump_tree "$2" "$3"; else bump_tree "$root" "${2:?--bump needs a version}"; fi ;;
  "") check_tree "$root" ;;
  *) die "usage: $0 [--self-test | --bump VERSION]" ;;
esac
