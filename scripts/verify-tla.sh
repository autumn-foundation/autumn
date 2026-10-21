#!/usr/bin/env bash
# Run TLC on the TLA+ specs in verification/tla/.
#
# Usage:
#   TLA2TOOLS_JAR=/path/to/tla2tools.jar scripts/verify-tla.sh
#   TLA2TOOLS_JAR=... scripts/verify-tla.sh FleetSlotOwnership   # one spec
#
# verification/tla/checks.txt lists the checks. Each line names a spec, a
# variant (the correct protocol or a seeded bug), and properties that must
# hold, must be violated, or must be reached. A new check needs no change
# here. CI runs this script from .github/workflows/tla.yml.
#
# The script runs every check. It exits 1 if any check fails.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

jar="${TLA2TOOLS_JAR:-}"
if [[ -z "${jar}" || ! -f "${jar}" ]]; then
  echo "error: set TLA2TOOLS_JAR to the path of tla2tools.jar" >&2
  echo "  (https://github.com/tlaplus/tlaplus/releases/tag/v1.7.4)" >&2
  exit 1
fi

only="${1:-}"
specs_dir="verification/tla"
checks="${specs_dir}/checks.txt"
# The same limit as the Stateright models: reduce the bounds above it.
max_states="${MAX_STATES:-2000000}"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# Run TLC on <spec> under <variant>, with <invariant> defined in a wrapper
# module as <definition>. Prints TLC's output into ${work}/out.
run_tlc() {
  local spec="$1" variant="$2" definition="$3"
  local dir="${work}/run"
  rm -rf "${dir}"
  mkdir -p "${dir}"
  cp "${specs_dir}/${spec}.tla" "${dir}/"
  {
    echo "---- MODULE MC ----"
    echo "EXTENDS ${spec}"
    echo "MC_Check == ${definition}"
    echo "===="
  } > "${dir}/MC.tla"
  {
    echo "INIT Init"
    echo "NEXT Next"
    echo "CONSTANT Variant = \"${variant}\""
    echo "INVARIANT MC_Check"
  } > "${dir}/MC.cfg"
  # -deadlock: a bounded model stops when its clock and counters run out.
  (cd "${dir}" && java -XX:+UseParallelGC -cp "${jar}" tlc2.TLC \
    -deadlock -workers auto -cleanup -metadir "${dir}/states" \
    -config MC.cfg MC.tla) > "${work}/out" 2>&1 || true
}

violated() { grep -q "Invariant MC_Check is violated" "${work}/out"; }
completed() { grep -q "Model checking completed. No error has been found." "${work}/out"; }
# The final count. A progress line prints the same phrase with commas.
states() { grep -oE '[0-9,]+ distinct states found' "${work}/out" | tail -n 1 | cut -d' ' -f1 | tr -d ,; }

failed=0
fail() {
  echo "FAIL: $*" >&2
  sed 's/^/    /' "${work}/out" | grep -v JAVA_TOOL_OPTIONS | tail -n 40 >&2
  failed=1
}

ran=0
while read -r expect spec variant properties; do
  [[ -z "${expect}" || "${expect}" == \#* ]] && continue
  [[ -n "${only}" && "${spec}" != "${only}" ]] && continue
  if [[ ! -f "${specs_dir}/${spec}.tla" ]]; then
    echo "FAIL: ${checks} names ${spec}, but ${specs_dir}/${spec}.tla does not exist" >&2
    failed=1
    continue
  fi
  read -r -a props <<< "${properties}"
  if (( ${#props[@]} == 0 )); then
    echo "FAIL: ${checks}: '${expect} ${spec} ${variant}' names no property" >&2
    failed=1
    continue
  fi
  case "${expect}" in
    holds)
      conj="$(printf ' /\\ %s' "${props[@]}")"
      echo "==> ${spec} ${variant}: holds ${props[*]}"
      run_tlc "${spec}" "${variant}" "${conj# /\\ }"
      if ! completed; then
        fail "${spec} ${variant}: ${props[*]} do not hold"
      else
        n="$(states)"
        echo "    ${n} distinct states"
        if (( n >= max_states )); then
          fail "${spec} ${variant}: ${n} states; reduce the bounds"
        fi
      fi
      ran=$((ran + 1))
      ;;
    violates)
      for prop in "${props[@]}"; do
        echo "==> ${spec} ${variant}: violates ${prop}"
        run_tlc "${spec}" "${variant}" "${prop}"
        if ! violated; then
          fail "${spec} ${variant}: the seeded bug gives no counterexample for ${prop}"
        fi
        ran=$((ran + 1))
      done
      ;;
    reaches)
      for prop in "${props[@]}"; do
        echo "==> ${spec} ${variant}: reaches ${prop}"
        run_tlc "${spec}" "${variant}" "~(${prop})"
        if ! violated; then
          fail "${spec} ${variant}: ${prop} is never reached; the check is vacuous"
        fi
        ran=$((ran + 1))
      done
      ;;
    *)
      echo "FAIL: ${checks}: unknown expectation '${expect}'" >&2
      failed=1
      ;;
  esac
done < "${checks}"

if (( ran == 0 )); then
  echo "error: no TLC check ran${only:+ for ${only}}" >&2
  exit 1
fi
echo "${ran} TLC checks run"
exit "${failed}"
