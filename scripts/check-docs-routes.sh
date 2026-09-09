#!/usr/bin/env bash
# Actuator-path drift gate: every `/actuator/…` URL the reader-facing docs tell
# someone to REQUEST must name an endpoint the framework mounts.
#
# WHY THIS EXISTS: the corpus already gates five of the six things a reader
# copies off a page.
# `scripts/check-docs-links.sh` gates its *links* (a 404 on GitHub),
# `scripts/check-docs-cli.sh` its *commands* (`unrecognized subcommand`),
# `scripts/check-docs-config.sh` the `AUTUMN_*` variables they SET (a silent
# no-op), `scripts/check-docs-toml.sh` the `autumn.toml` keys they WRITE
# (dropped silently), `scripts/check-docs-symbols.sh` the `autumn_web::…` paths
# they IMPORT (E0432 against their own file), and
# `scripts/check-docs-orphans.sh` asserts the page can be reached at all.
# Nothing gated the sixth: the URL they CURL.
#
# The reader-facing corpus names 236 `/actuator/…` paths across 195 pages, and
# a renamed or never-shipped endpoint leaves behind a line that looks exactly
# like a working one. The baseline run found four:
#
#   docs/guide/generators.md:1119          `/actuator/routes`
#   docs/guide/tutorial/07-htmx.md:71      `/actuator/routes`
#   docs/guide/coming-from-other-frameworks.md:254  `/actuator/scheduledtasks`
#                                                   (twice, once per column)
#
# WHY THIS SURFACE IS WORSE THAN A BAD LINK, NOT BETTER. A 404 from a docs link
# arrives while the reader is still reading, on a page they can back out of. A
# 404 from `/actuator/…` arrives against a RUNNING APP, and the actuator is the
# operator surface: `/actuator/health` is what a load balancer probes,
# `/actuator/jobs` and `/actuator/tasks` are what someone opens at 3am to find
# out why a scheduled backup stopped. `curl` answers `404 Not Found` with no
# hint of the right name, and the reader's next move is to doubt their own
# deployment — the actuator is behind `sensitive = true`, so a 404 reads exactly
# like an endpoint they failed to enable. `coming-from-other-frameworks.md` is
# the sharpest case: its Spring→Autumn actuator table exists for the sole
# purpose of telling a migrating reader what the endpoint is CALLED here, and
# the `scheduledtasks` row told them the name was unchanged. It is not:
# Autumn serves the same payload (`{"scheduled_tasks": …}`) at
# `/actuator/tasks`. A reader who trusts the table gets a 404 from the one
# page whose job was to prevent it, and — because the real endpoint has a
# different name — searching for `scheduledtasks` cannot rescue them either.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   Every `/actuator/…` path in the reader-facing corpus resolves against the
#   set of paths the framework actually mounts.
#
# TRUTH SET: the string literals passed to `actuator::actuator_route_path()`
# across non-test workspace Rust source — 23 suffixes. That helper is the ONE
# path builder the actuator uses: the mount list (`actuator_router_with_prefix`),
# the enumeration the startup barrier reads (`actuator_endpoint_paths`), the
# non-`GET` mounts (`actuator_mutating_routes`) and the alert pointers
# (`alerts::actuator_where_to_look`) all go through it, and its own doc comment
# says why — "so paths match byte-for-byte". Reading the literals it is called
# with is therefore the same list the router mounts, with no snapshot to
# regenerate: a renamed endpoint lands in the same commit as the rename.
# `#[cfg(test)] mod …` bodies are stripped first, so a path that exists only in
# an assertion (`/actuator/loggers/{bogus}`, `/actuatorsomething`) never
# confers existence on a documented one.
#
# RESOLUTION, and why it is deliberately permissive in three places:
#   - A mounted `{param}` segment matches any documented segment, so
#     `/actuator/loggers/root` and `/actuator/loggers/my_app` both resolve
#     against the mounted `/actuator/loggers/{name}`. A reader writing a
#     concrete value is doing the right thing.
#   - A documented path that is a PREFIX of a mounted one resolves:
#     `/actuator/webhooks` names the family whose members are
#     `/webhooks/dlq` and `/webhooks/replay`, and the bare `/actuator` is the
#     prefix itself. Naming a family is not a claim that the family root
#     answers.
#   - A `*` segment matches anything, so `/actuator/*` and
#     `/actuator/webhooks/*` — the spellings the docs use for "all of these" —
#     resolve. The framework writes the same glob itself
#     (`actuator_route_glob`).
#   Each of the three makes the gate report FEWER paths. None of them can
#   rescue a name that is simply not there, which is the defect class this
#   exists for.
#
# CORPUS SCOPE: identical to `check-docs-cli.sh` and `check-docs-config.sh` —
# `docs/guide/`, `docs/migrations/`, `skills/`, `agents/`, the root
# `README.md` / `EXAMPLES.md` / `CONTRIBUTING.md` / `STABILITY.md`,
# `docs/plugins.md`, and each `examples/*/README.md`. The three definitions of
# "reader-facing" are kept identical on purpose: a page covered by one gate and
# not the next is how a page ends up with no owner. Deliberately excluded for
# the same reasons as those gates:
#   - `CHANGELOG.md` and `docs/releases/` — a historical record. An endpoint
#     that existed at 0.5.0 must stay written as it was.
#   - `docs/plans/`, `docs/stories/`, `docs/adr/`, `docs/reports/`,
#     `docs/design/` — planning artifacts whose job includes naming endpoints
#     that do not exist yet. `docs/plans/2026-03-26-actuator-design.md` names
#     `/actuator/threaddump` and `/actuator/metrics/prometheus` precisely
#     because it is arguing about what to build; gating a proposal is a tax on
#     writing one.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Application routes (`/posts`, `/login`, `/checkout/review`). Those belong
#     to the reader's own app or to an example, and the framework has no
#     opinion about them — there is nothing to resolve against.
#   - `/_autumn/…` (the dev inspector, mail preview, job-status and unsubscribe
#     mounts). Those paths are built from constants in four modules and two of
#     them are composed at runtime from a CONFIGURABLE root
#     (`format!("{path}/requests/{{id}}")` over `dev.inspector_path`), so there
#     is no single authoritative literal set to diff against and a gate over
#     them would be guessing. They are also a browser surface rather than a
#     `curl` one. Left out on purpose rather than approximated.
#   - A non-default actuator prefix. `[actuator] prefix = "/internal"` renames
#     the whole surface, and a page demonstrating that legitimately writes
#     `/internal/health`. Only the literal `/actuator` prefix is read, which is
#     what every corpus page writes today.
#   - Whether the endpoint is EXPOSED. Most of the surface needs
#     `[actuator] sensitive = true`, and a page may correctly name an endpoint a
#     default prod profile hides. This gate answers one question — does the
#     framework mount this path at all — and `docs/guide/operator-alerts.md`
#     already carries the `sensitive` caveat where it matters.
#
# WAIVERS: a reader-facing page sometimes has to name an actuator path that
# does not exist here — "Spring calls it `/actuator/scheduledtasks`" is the
# whole point of a migration table. Waive it with a marker directly below the
# passage that names it:
#
#     <!-- route-surface-allow: /actuator/scheduledtasks — Spring Boot's name,
#          shown for comparison; Autumn serves it at /actuator/tasks -->
#
# The marker sits in the page beside the claim, so it is deleted by the same
# commit that deletes the sentence. A central allowlist would outlive it and
# silently re-admit the defect. Every waiver must carry a reason after the
# path, separated by `—` or `:`.
#
# A waiver covers only its own blank-line-separated block and the one directly
# above it — the passage it was written for. The same path spelled wrong
# further down the page is still reported. Unlike `check-docs-cli.sh`, a waiver
# here DOES apply inside a fenced block: the fence is how a comparison table's
# sibling page shows a foreign framework's transcript, and a URL in a fence is
# not a command the reader can be tricked into running blind — they will see
# the 404 the instant they paste it, which is the same signal the gate gives.
#
# USAGE:
#   scripts/check-docs-routes.sh              # gate the corpus
#   scripts/check-docs-routes.sh --list       # print the mounted path surface
#   scripts/check-docs-routes.sh --self-test  # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# Kept in Python for the same reason as its sibling gates: brace-balanced
# `#[cfg(test)]` stripping and segment-wise path matching are both work that
# bash renders unreadable, and python3 is already a dependency of
# scripts/check-docs-cli.sh, scripts/check-docs-config.sh and
# scripts/check-docs-toml.sh.
run_py() {
  python3 - "$@" <<'PYEOF'
import os
import pathlib
import re
import subprocess
import sys

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])

PREFIX = "/actuator"

# ── Truth set: the paths the actuator actually mounts ────────────────────────

# `actuator_route_path(<prefix expr>, "<suffix>")`. The prefix argument is
# spelled four different ways across the call sites (`prefix`,
# `&config.actuator.prefix`, `&cfg.actuator.prefix`, and a `crate::actuator::`
# qualified form split across lines), so it is matched loosely and only the
# suffix literal is captured. A call whose suffix is NOT a literal
# (`alerts.rs` passes `condition.actuator_suffix()`) contributes nothing here,
# which is correct: every suffix that method can return is also written as a
# literal at its mount site.
ROUTE_CALL = re.compile(
    r"actuator_route_path\(\s*[^,;()]*(?:\([^()]*\))?[^,;()]*,\s*\"([^\"]*)\"")

# `#[cfg(test)] mod <name> { … }`, removed before the scan above runs.
#
# Without this the truth set inherits every path an assertion ever typed —
# `/actuator/loggers/{bogus}` and a deliberately-unmatched `/actuatorsomething`
# are both in `actuator.rs`'s tests — and a gate whose truth set includes the
# strings written to prove a path is WRONG cannot report a wrong path.
CFG_TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+[A-Za-z0-9_]+\s*\{")


def strip_test_mods(src):
    """Drop every brace-balanced `#[cfg(test)] mod …` body from `src`."""
    out = []
    cursor = 0
    for match in CFG_TEST_MOD.finditer(src):
        if match.start() < cursor:
            continue
        out.append(src[cursor:match.start()])
        i, depth = match.end(), 1
        while i < len(src) and depth:
            if src[i] == "{":
                depth += 1
            elif src[i] == "}":
                depth -= 1
            i += 1
        cursor = i
    out.append(src[cursor:])
    return "".join(out)


def mounted_paths(root):
    """Every `/actuator/…` path the framework mounts, under the default prefix."""
    listing = subprocess.run(
        ["git", "ls-files", "-z", "*.rs"],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout
    suffixes = set()
    for rel in filter(None, listing.split("\0")):
        src = (root / rel).read_text(encoding="utf-8", errors="ignore")
        if "actuator_route_path(" not in src:
            continue
        suffixes.update(ROUTE_CALL.findall(strip_test_mods(src)))
    if not suffixes:
        sys.exit(
            "FAIL: no `actuator_route_path(…, \"…\")` call sites found. The path "
            "builder was renamed or moved; this gate has no truth set to read "
            "and would pass everything. Fix ROUTE_CALL in "
            "scripts/check-docs-routes.sh."
        )
    return sorted(PREFIX + s for s in suffixes)


# ── Corpus ───────────────────────────────────────────────────────────────────

# Kept byte-identical to `check-docs-cli.sh` and `check-docs-config.sh`; see
# the CORPUS SCOPE note in this file's header for why the three agree.
INCLUDE_DIRS = ("docs/guide/", "docs/migrations/", "skills/", "agents/")
INCLUDE_FILES = ("README.md", "EXAMPLES.md", "CONTRIBUTING.md", "STABILITY.md",
                 "docs/plugins.md")
INCLUDE_README_DIRS = ("examples/",)


def in_scope(path):
    return (path.startswith(INCLUDE_DIRS) or path in INCLUDE_FILES
            or (path.startswith(INCLUDE_README_DIRS)
                and pathlib.PurePath(path).name == "README.md"))


def corpus(root):
    # NUL-delimited so a path containing whitespace is not split into fragments,
    # and so git does not quote unusual paths.
    out = subprocess.run(["git", "ls-files", "-z", "*.md"], cwd=root,
                         capture_output=True, text=True, check=True).stdout
    return [f for f in out.split("\0") if f and in_scope(f)]


# ── Extraction ───────────────────────────────────────────────────────────────

# A path starts at the literal `/actuator` and runs while segments look like a
# URL. `.` is allowed INSIDE a segment (`/actuator/loggers/my.app`) but a
# trailing run of sentence punctuation is stripped afterwards, so
# "…probe `/actuator/health`." does not become `/actuator/health.` and get
# reported as a missing endpoint.
DOC_PATH = re.compile(
    r"/actuator(?:/[A-Za-z0-9_*{}-]+(?:\.[A-Za-z0-9_-]+)*)*")
TRAILING = ".,;:!?)\"'`]}>"

# `<!-- route-surface-allow: /actuator/x — reason -->`. The reason is required:
# a waiver without one outlives the sentence it was written for.
WAIVER = re.compile(
    r"<!--\s*route-surface-allow:\s*(/actuator[^\s—:]*)\s*(?:—|:)\s*(\S[^>]*?)-->",
    re.S)


def waived_lines(text):
    """Map a waived path to the set of line numbers its waivers cover.

    A waiver covers its own blank-line-separated block and the one directly
    above it — the passage it was written for. Anything further down the page
    is still reported, because a page-wide waiver silently re-admits the defect
    the gate exists to catch.
    """
    lines = text.splitlines()
    # Block index per line, where a run of blank lines separates blocks.
    block_of = []
    block = 0
    prev_blank = True
    for line in lines:
        blank = not line.strip()
        if blank:
            prev_blank = True
            block_of.append(block)
            continue
        if prev_blank:
            block += 1
        prev_blank = False
        block_of.append(block)

    covered = {}
    for lineno, line in enumerate(lines, 1):
        for path, _reason in WAIVER.findall(line):
            here = block_of[lineno - 1]
            scope = {here, here - 1}
            hits = covered.setdefault(path, set())
            hits.update(n for n, b in enumerate(block_of, 1) if b in scope)
    return covered


def documented(text):
    """Yield (line_no, path) for every `/actuator/…` path a page shows."""
    for lineno, line in enumerate(text.splitlines(), 1):
        for match in DOC_PATH.finditer(line):
            path = match.group(0).rstrip(TRAILING)
            if path:
                yield lineno, path


# ── Resolution ───────────────────────────────────────────────────────────────

def resolves(path, mounted):
    """Does `path` name something the framework mounts?

    Segment-wise, and permissive in the three ways the header lists: a mounted
    `{param}` matches any segment, a documented `*` matches any segment, and a
    documented path that is a strict PREFIX of a mounted one resolves.
    """
    segs = [s for s in path.strip("/").split("/") if s]
    for target in mounted:
        tsegs = [s for s in target.strip("/").split("/") if s]
        if len(segs) > len(tsegs):
            continue
        if all(t.startswith("{") or s == "*" or s == t
               for s, t in zip(segs, tsegs)):
            return True
    return False


def nearest(path, mounted):
    """The mounted path most like `path`, for the `did you mean` hint."""
    import difflib
    match = difflib.get_close_matches(path, mounted, n=1, cutoff=0.0)
    return match[0] if match else None


# ── Modes ────────────────────────────────────────────────────────────────────

def main():
    mounted = mounted_paths(ROOT)
    files = corpus(ROOT)
    checked = 0
    waived = 0
    defects = []

    for rel in files:
        text = (ROOT / rel).read_text(encoding="utf-8", errors="ignore")
        if PREFIX not in text:
            continue
        waivers = waived_lines(text)
        for lineno, path in documented(text):
            checked += 1
            if resolves(path, mounted):
                continue
            if lineno in waivers.get(path, ()):
                waived += 1
                continue
            defects.append((rel, lineno, path))

    print(f"corpus: {len(files)} reader-facing markdown files")
    print(f"surface: {len(mounted)} mounted actuator paths")
    print(f"checked: {checked} `/actuator/…` occurrences")
    for rel, lineno, path in defects:
        hint = nearest(path, mounted)
        suffix = f"  (did you mean `{hint}`?)" if hint else ""
        print(f"{rel}:{lineno}: `{path}` is not mounted{suffix}")
    print(f"\ndefects: {len(defects)}" + (f" ({waived} waived)" if waived else ""))
    return 0 if not defects else 1


def list_surface():
    for path in mounted_paths(ROOT):
        print(path)
    return 0


# ── Self-test ────────────────────────────────────────────────────────────────

def self_test():
    """Assert the matcher's behaviour on a synthetic surface.

    The gate's own corpus run is not a test: it passes trivially once the
    corpus is clean, and would keep passing if the matcher were broken open.
    These cases pin the three permissive rules and the waiver scoping.
    """
    surface = ["/actuator/health", "/actuator/loggers/{name}",
               "/actuator/tasks", "/actuator/webhooks/dlq",
               "/actuator/webhooks/replay"]
    cases = [
        ("exact mount resolves", resolves("/actuator/health", surface), True),
        ("missing endpoint is a defect",
         resolves("/actuator/routes", surface), False),
        ("a Spring name is a defect",
         resolves("/actuator/scheduledtasks", surface), False),
        ("mounted {param} matches a concrete value",
         resolves("/actuator/loggers/root", surface), True),
        ("mounted {param} matches the param spelling",
         resolves("/actuator/loggers/{name}", surface), True),
        ("prefix of a mounted family resolves",
         resolves("/actuator/webhooks", surface), True),
        ("bare prefix resolves", resolves("/actuator", surface), True),
        ("documented glob resolves",
         resolves("/actuator/webhooks/*", surface), True),
        ("a longer path than any mount is a defect",
         resolves("/actuator/health/deep", surface), False),
        ("a near-miss on a real name is still a defect",
         resolves("/actuator/task", surface), False),
    ]

    text = "prose `/actuator/health`.\nand `/actuator/tasks`, plus (/actuator/info)\n"
    found = sorted(p for _, p in documented(text))
    cases.append((
        "trailing sentence punctuation is stripped",
        found,
        ["/actuator/health", "/actuator/info", "/actuator/tasks"],
    ))

    fence = "| `/actuator/gone` |\n<!-- route-surface-allow: /actuator/gone — reason -->\n"
    cases.append((
        "a waiver covers the block above it",
        1 in waived_lines(fence).get("/actuator/gone", set()),
        True,
    ))
    far = ("`/actuator/gone`\n\nfiller\n\nfiller\n\n"
           "<!-- route-surface-allow: /actuator/gone — reason -->\n")
    cases.append((
        "a waiver does not reach a distant block",
        1 in waived_lines(far).get("/actuator/gone", set()),
        False,
    ))
    cases.append((
        "a waiver without a reason does not parse",
        waived_lines("<!-- route-surface-allow: /actuator/gone -->\n"),
        {},
    ))

    # The truth set must be non-empty on the real tree, or the gate is a no-op
    # that reports `defects: 0` forever.
    cases.append((
        "the real surface is non-empty",
        len(mounted_paths(ROOT)) > 10,
        True,
    ))

    passed = failed = 0
    for label, got, want in cases:
        if got == want:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: got {got!r}, want {want!r}")
    print(f"self-test: {passed}/{passed + failed} passed")
    return 0 if failed == 0 else 1


sys.exit({"--self-test": self_test, "--list": list_surface}.get(MODE, main)())
PYEOF
}

case "${1-}" in
  --self-test)
    run_py --self-test "$root"
    ;;
  --list)
    run_py --list "$root"
    ;;
  "")
    echo "Checking actuator paths across the reader-facing docs..."
    if run_py --check "$root"; then
      echo "Actuator path gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the docs tell a reader to request actuator endpoints that are not
mounted (above).

The reader gets `404 Not Found` from a running app with no hint of the right
name — and because most of the actuator is behind `[actuator] sensitive =
true`, a 404 reads like an endpoint they failed to enable rather than one that
was never there.

Fix each one where it lives:
  - renamed endpoint  -> use the current path (the `did you mean` hint is the
                         closest mounted path)
  - never existed     -> drop the claim, or name the endpoint that does the job
  - another framework's name, shown for comparison -> waive it beside the
                         passage, with the Autumn path in the reason:

      <!-- route-surface-allow: /actuator/scheduledtasks — Spring Boot's name;
           Autumn serves it at /actuator/tasks -->

  - genuinely new endpoint -> land the mount first; this gate reads the
                         `actuator_route_path(…)` call sites, so it needs no
                         snapshot update

Inspect what the gate read:  scripts/check-docs-routes.sh --list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--list|--self-test]" >&2
    exit 2
    ;;
esac
