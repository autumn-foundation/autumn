#!/usr/bin/env bash
#
# prepare_agent_workspace.sh — assemble a builder-visible workspace for a trial.
#
# The habit-tracker harness lives inside the framework repos it evaluates, so a
# raw checkout hands the building agent the *executable* hidden suites
# (hidden/, phase2/archive_hidden_test.py) that decide 60% of the score. Running
# a trial from a workspace prepared by this script — instead of the raw
# checkout — walls those evaluator-only materials off: only the builder-visible
# brief, spec, and visible suites are copied.
#
# Usage:
#   prepare_agent_workspace.sh <framework> <dest-dir>
#     <framework>  one of: autumn django rails springboot phoenix loco
#     <dest-dir>   directory to populate (created if missing)
#
# Copies ONLY builder-visible materials:
#   SPEC.md, prompts/_core.md, prompts/<framework>.md, acceptance/,
#   phase2/archive_test.py
# and NEVER copies hidden/, phase2/archive_hidden_test.py, scripts/score.py,
# rubric.md, metrics-schema.json, or runs/.
set -euo pipefail

VALID_FRAMEWORKS="autumn django rails springboot phoenix loco"

usage() {
    echo "Usage: prepare_agent_workspace.sh <framework> <dest-dir>" >&2
    echo "  <framework>  one of: ${VALID_FRAMEWORKS}" >&2
    echo "  <dest-dir>   directory to populate (created if missing)" >&2
}

framework="${1:-}"
dest="${2:-}"

if [[ -z "${framework}" ]]; then
    echo "error: missing <framework> argument" >&2
    usage
    exit 1
fi

valid=0
for f in ${VALID_FRAMEWORKS}; do
    if [[ "${framework}" == "${f}" ]]; then
        valid=1
        break
    fi
done
if [[ "${valid}" -ne 1 ]]; then
    echo "error: unknown framework '${framework}' (valid: ${VALID_FRAMEWORKS})" >&2
    exit 1
fi

if [[ -z "${dest}" ]]; then
    echo "error: missing <dest-dir> argument" >&2
    usage
    exit 1
fi

# Resolve the harness root (this script lives in <harness>/scripts/).
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
harness_root="$(cd "${script_dir}/.." && pwd)"

framework_prompt="${harness_root}/prompts/${framework}.md"
if [[ ! -f "${framework_prompt}" ]]; then
    echo "error: no prompt found for framework '${framework}' at ${framework_prompt}" >&2
    exit 1
fi

mkdir -p "${dest}/prompts" "${dest}/phase2"

# Builder-visible materials only.
cp "${harness_root}/SPEC.md" "${dest}/SPEC.md"
cp "${harness_root}/prompts/_core.md" "${dest}/prompts/_core.md"
cp "${framework_prompt}" "${dest}/prompts/${framework}.md"
cp -R "${harness_root}/acceptance" "${dest}/acceptance"
cp "${harness_root}/phase2/archive_test.py" "${dest}/phase2/archive_test.py"

echo "Prepared builder-visible workspace for '${framework}' at: ${dest}"
echo "Copied:"
echo "  SPEC.md"
echo "  prompts/_core.md"
echo "  prompts/${framework}.md"
echo "  acceptance/  (visible acceptance suite)"
echo "  phase2/archive_test.py  (visible phase-2 suite)"
echo
echo "NOTE: hidden suites are evaluator-only and were deliberately NOT copied:"
echo "  hidden/, phase2/archive_hidden_test.py, scripts/score.py,"
echo "  rubric.md, metrics-schema.json, runs/"
echo "Never expose those to the building agent."
