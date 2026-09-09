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
# AT A `.route(…)` MOUNT, across non-test workspace Rust source — 23 suffixes.
# That helper is the one path builder every actuator mount goes through, and its
# own doc comment says why: "so paths match byte-for-byte". Reading the literals
# it is mounted with is therefore the list the router serves, with no snapshot to
# regenerate — a renamed endpoint lands in the same commit as the rename.
#
# TWO KINDS OF `actuator_route_path` CALL ARE DELIBERATELY NOT READ, and the
# distinction is the whole reason this gate can be trusted. Besides the mounts,
# `actuator.rs` calls the builder from two INVENTORIES —
# `actuator_endpoint_paths` (the GET path list the startup barrier seeds its
# allow-list from) and `actuator_mutating_routes` (the non-GET pairs the route
# listing classifies) — and `alerts.rs` calls it to build a `where_to_look`
# pointer. Those are second copies of a list, not evidence that a URL answers.
# The three agree today; an earlier draft of this gate read their union anyway,
# and that union is wrong in both directions and in exactly the way this gate
# exists to prevent. An endpoint dropped from the router but left in an
# inventory would go on blessing documentation for a path nothing serves, and an
# inventory-only entry would be accepted with no handler behind it. A drift gate
# must not be able to inherit the drift it is checking for.
#
# `#[cfg(test)] mod …` bodies are stripped for the same reason, one step
# further: a path that exists only in an assertion
# (`/actuator/loggers/{bogus}`, `/actuatorsomething`, both in `actuator.rs`'s
# tests) must never confer existence on a documented one — least of all a string
# written to prove a path is WRONG.
#
# RESOLUTION, and why it is deliberately permissive in three places:
#   - A mounted `{param}` segment matches any documented segment, so
#     `/actuator/loggers/root` and `/actuator/loggers/my_app` both resolve
#     against the mounted `/actuator/loggers/{name}`. A reader writing a
#     concrete value is doing the right thing.
#   - A documented path that is a PREFIX of a mounted one resolves WHEN THE PAGE
#     IS NAMING IT rather than requesting it: `/actuator/webhooks` names the
#     family whose members are `/webhooks/dlq` and `/webhooks/replay`, and the
#     bare `/actuator` is the prefix itself. Naming a family is not a claim that
#     the family root answers — and naming is what the corpus does with every
#     one of these: ~20 `/actuator/*` family mentions, `access_log_exclude =
#     ["/health", "/actuator", …]`, "Actuator prefix: `/actuator`",
#     `Disallow: /actuator/`.
#     On a line that hands the reader a REQUEST — a `curl`, or an HTTP method
#     followed by a path — the prefix rule is withdrawn, because there a prefix
#     is a URL someone sends and `curl …/actuator/webhooks` is a 404. Nothing in
#     the corpus does this today; the rule exists so nothing can start. The `*`
#     and `{param}` rules still stand in a request, since `/actuator/*` in a
#     command is still naming a family.
#   - A `*` segment matches anything, so `/actuator/*` and
#     `/actuator/webhooks/*` — the spellings the docs use for "all of these" —
#     resolve. The framework writes the same glob itself
#     (`actuator_route_glob`).
#   Each of the three makes the gate report FEWER paths. None of them can
#   rescue a name that is simply not there, which is the defect class this
#   exists for.
#
# CORPUS SCOPE: every markdown surface a reader can end up holding. That is the
# set `check-docs-cli.sh` and `check-docs-config.sh` define — `docs/guide/`,
# `docs/migrations/`, `skills/`, `agents/`, the root `README.md` /
# `EXAMPLES.md` / `CONTRIBUTING.md` / `STABILITY.md`, `docs/plugins.md` — plus
# three surfaces that a `docs/`-shaped view of a corpus misses, each of which
# reaches readers by a route other than someone opening a page:
#
#   - every `*.md.tmpl`. `autumn-cli/src/new.rs` `include_str!`s
#     `templates/README.md.tmpl` and WRITES it as every scaffolded application's
#     `README.md`, where it documents `/health` and `/actuator/health`. A stale
#     URL there ships into every new project. `check-docs-config.sh` and
#     `check-docs-symbols.sh` glob it for the same reason; `check-docs-cli.sh`,
#     which this file's corpus function was copied from, does not — so the
#     siblings were never unanimous and this header once wrongly claimed to
#     match them all.
#   - all of `examples/`, not just each `examples/*/README.md`. The wiki example
#     COMPILES `examples/wiki/content/*.md` in and SERVES them at `/docs/…`
#     (`check-docs-toml.sh` notes the same), so four actuator paths there are
#     shown by a running app rather than read from a repo.
#   - `.claude/skills/`, a SECOND skill tree rather than a copy of `skills/`.
#     `check-docs-orphans.sh` seeds both as reader entry surfaces because the
#     agent machinery loads each by name, and `run-autumn` lives only here. Its
#     SKILL.md drives a real server with `curl`, which makes its actuator paths
#     the most literally copy-and-run text in the tree — and it is where the
#     `/actuator/routes` 404 had already been DISCOVERED and written down,
#     while two guide pages went on telling readers to use it.
#
# Deliberately excluded for the same reasons as those gates:
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
# does not exist here. "Spring calls it `/actuator/scheduledtasks`" is the whole
# point of a migration table, and "`/actuator/routes` does not exist (404)" is
# the whole point of a troubleshooting row. Waive it with a marker directly
# below the passage that names it:
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
# above it — the passage it was written for, anchored at the line the marker
# OPENS on so a reason long enough to wrap is scoped by where it was written.
# The same path spelled wrong further down the page is still reported.
#
# HTML comments are blanked before paths are extracted, on the same
# visible-vs-invisible line `check-docs-orphans.sh` draws: a comment renders as
# nothing, so it is not a path a reader can see or paste. That rule is also what
# lets a waiver name the path it exempts without the gate re-reporting its own
# marker one line down.
#
# Unlike `check-docs-cli.sh`, a waiver here DOES apply inside a fenced block:
# the fence is how a comparison table's sibling page shows a foreign framework's
# transcript, and a URL in a fence is not a command the reader can be tricked
# into running blind — they will see the 404 the instant they paste it, which is
# the same signal the gate gives.
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

# `.route(&actuator_route_path(<prefix expr>, "<suffix>"), …)` — a MOUNT, not a
# mention. The prefix argument is spelled several ways across the file
# (`prefix`, `&config.actuator.prefix`, a `crate::actuator::`-qualified form
# split across lines), so it is matched loosely and only the suffix literal is
# captured. A call whose suffix is not a literal (`alerts.rs` passes
# `condition.actuator_suffix()`) contributes nothing, which is correct: every
# suffix that method can return is also written as a literal at its mount site.
#
# WHY THE `.route(` PREFIX IS PART OF THE PATTERN, and not an incidental
# tightening. `actuator.rs` calls this same builder from three places: the
# mounts in `actuator_router_with_prefix`, and two INVENTORIES —
# `actuator_endpoint_paths` (the GET path list the startup barrier seeds its
# allow-list from) and `actuator_mutating_routes` (the non-GET pairs the route
# listing classifies). The three agree today, and an earlier draft of this gate
# unioned all of them. That union is wrong in both directions and in exactly the
# way this gate exists to prevent: an endpoint dropped from the router but left
# in an inventory would keep blessing documentation for a path nothing serves,
# and an inventory-only entry would be accepted with no handler behind it. A
# drift gate must not be able to inherit the drift it is checking for, which is
# the same reason `#[cfg(test)]` bodies are stripped below. So the truth set is
# what is MOUNTED, and the inventories are read as what they are: second copies
# of a list, with no authority over whether a URL answers.
ROUTE_CALL = re.compile(
    r"\.route\(\s*&\s*(?:[A-Za-z0-9_]+::)*actuator_route_path\(\s*"
    r"[^,;()]*(?:\([^()]*\))?[^,;()]*,\s*\"([^\"]*)\"")

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
        if ".route(" not in src or "actuator_route_path(" not in src:
            continue
        suffixes.update(ROUTE_CALL.findall(strip_test_mods(src)))
    # A truth set that silently empties is worse than no gate: every documented
    # path would resolve against nothing and the corpus would report clean
    # forever. Fail loudly instead, on the assumption the router was refactored
    # out from under this pattern.
    if not suffixes:
        sys.exit(
            "FAIL: no `.route(&actuator_route_path(…, \"…\"))` mounts found. The "
            "actuator router or its path builder was refactored; this gate has "
            "no truth set to read and would pass everything. Fix ROUTE_CALL in "
            "scripts/check-docs-routes.sh."
        )
    return sorted(PREFIX + s for s in suffixes)


# ── Corpus ───────────────────────────────────────────────────────────────────

# The same reader-facing set `check-docs-cli.sh` and `check-docs-config.sh`
# define; see the CORPUS SCOPE note in this file's header for why they agree.
INCLUDE_DIRS = ("docs/guide/", "docs/migrations/", "skills/", "agents/",
                # `.claude/skills/` is a SECOND skill tree, not a copy of
                # `skills/`: `check-docs-orphans.sh` seeds both as reader entry
                # surfaces because the agent machinery loads each by name, and
                # `run-autumn` lives only here. Its SKILL.md drives a real
                # server with `curl`, so its actuator paths are the most
                # literally copy-and-run text in the tree.
                ".claude/skills/",
                # `examples/wiki/content/` is EMBEDDED and SERVED: the wiki
                # example compiles these pages in and renders them at
                # `/docs/...`, as `check-docs-toml.sh` notes. A stale URL here
                # is not a page a reader might open — it is a page the running
                # example shows them.
                "examples/")
INCLUDE_FILES = ("README.md", "EXAMPLES.md", "CONTRIBUTING.md", "STABILITY.md",
                 "docs/plugins.md")


def in_scope(path):
    return path.startswith(INCLUDE_DIRS) or path in INCLUDE_FILES


def corpus(root):
    """The reader-facing pages, including the one written into a new project.

    `*.md.tmpl` is in the glob for the reason `check-docs-config.sh` and
    `check-docs-symbols.sh` put it in theirs: `autumn-cli/src/new.rs`
    `include_str!`s `templates/README.md.tmpl` and writes it as every scaffolded
    application's `README.md`, where it documents `/health` and
    `/actuator/health`. It is not a guide page, but it is a page a reader holds,
    and it reaches more of them than most guide pages do. A template excluded
    from the corpus is a page with no owner — and worse here than elsewhere,
    because a stale URL in it ships into every new project rather than sitting
    on one page someone might notice.
    """
    # NUL-delimited so a path containing whitespace is not split into fragments,
    # and so git does not quote unusual paths.
    out = subprocess.run(["git", "ls-files", "-z", "*.md", "*.md.tmpl"],
                         cwd=root, capture_output=True, text=True,
                         check=True).stdout
    return [f for f in out.split("\0")
            if f and (in_scope(f) or f.endswith(".md.tmpl"))]


# ── Extraction ───────────────────────────────────────────────────────────────

# A path starts at the literal `/actuator` and runs while segments look like a
# URL. `.` is allowed INSIDE a segment (`/actuator/loggers/my.app`) but a
# trailing run of sentence punctuation is stripped afterwards, so
# "…probe `/actuator/health`." does not become `/actuator/health.` and get
# reported as a missing endpoint.
#
# The lookahead after the prefix is load-bearing, and its absence was a silent
# hole rather than a cosmetic one. Without it `/actuatorhealth` matches just
# `/actuator`, which then RESOLVES against the bare-prefix rule — so the gate
# would green-light a URL that 404s, by truncating it down to one that does not.
# A gate that answers a question the reader did not ask is worse than one that
# declines to answer.
#
# A TRAILING SLASH IS KEPT, not stripped, because axum distinguishes the
# mounted `/actuator/health` from `/actuator/health/` and mounts no normalizing
# layer — so a documented trailing slash is a 404 the old pattern silently
# truncated away. `resolves()` reads it as "the subtree below this", which is
# what the two shapes the corpus actually writes both mean: `Disallow:
# /actuator/` in a robots.txt sample, and `/actuator/…` in prose. Under a leaf
# that has nothing below it, the same reading rejects `/actuator/health/`.
DOC_PATH = re.compile(
    r"/actuator(?![A-Za-z0-9_-])(?:/[A-Za-z0-9_*{}-]+(?:\.[A-Za-z0-9_-]+)*)*/?")

# Sentence punctuation stripped from the end of a match. `/` is deliberately
# ABSENT (it is significant, per above) and so are the three characters a survey
# of the corpus found genuinely terminating a path, each of which the match must
# stop at rather than swallow:
#   `?`  a query string — `GET /actuator/logfile?level=warn` in logging-pii.md.
#        The PATH ends at the `?`; the query is not part of it.
#   `>`  a markdown autolink — `<http://localhost:3000/actuator/health>` in
#        tutorial/01-project-setup.md.
#   `:`  a `::`-separated logger target — `/actuator/loggers/my_app::orders` in
#        skills/autumn-web/SKILL.md, a legal `{name}` value.
# None of the three is a defect, so none is reported; they are listed here
# because "reject every unrecognized suffix" would report all three.
TRAILING = ".,;:!?)\"'`]}>"

# `/actuatorhealth` — the separator dropped. Reported on its own, because the
# lookahead above only stops the gate from mis-reading it; something still has
# to say it is wrong, and a missing slash is the most plausible way a documented
# actuator URL 404s.
#
# A HYPHEN IS NOT A MISSING SEPARATOR, which is why `_` and alphanumerics are
# matched here but `-` is not. `/actuator-dashboard` is a distinct route a reader
# may legitimately mount, not a typo of an actuator endpoint — the framework's
# own router test asserts exactly that, mounting `/actuator-dashboard` to prove
# the actuator prefix must not swallow it. Reporting it would be this gate
# claiming a path was meant to be one of ours.
MISSING_SEPARATOR = re.compile(r"/actuator[A-Za-z0-9_][A-Za-z0-9_./{}-]*")

# A line that hands the reader a REQUEST rather than a name: a `curl`
# invocation, or an HTTP method immediately followed by a path. Only these
# withdraw the prefix rule (see `resolves`), because only on these does a prefix
# become a URL someone actually sends.
#
# The method form requires the path to FOLLOW the method, which is what
# separates `GET /actuator/webhooks` (a request) from
# `macro-transparency.md`'s route-table column `/actuator/*  GET  -> actuator`
# (a listing). Deliberately narrow: everything else in the corpus that writes a
# prefix — `access_log_exclude = ["/health", "/actuator", …]`, "Actuator prefix:
# `/actuator`", `Disallow: /actuator/`, and the ~20 `/actuator/*` family
# mentions — is a NAME, and reporting those would be the gate calling correct
# lines defects.
REQUEST_LINE = re.compile(
    r"\bcurl\b|\b(?:GET|POST|PUT|PATCH|DELETE|HEAD)\s+/")

# `<!-- route-surface-allow: /actuator/x — reason -->`. The reason is required:
# a waiver without one outlives the sentence it was written for. Matched over
# the whole page rather than line by line, because a waiver long enough to
# explain itself wraps — the corpus's first one takes three lines.
WAIVER = re.compile(
    r"<!--\s*route-surface-allow:\s*(/actuator[^\s—:]*)\s*(?:—|:)\s*(\S[^>]*?)-->",
    re.S)

# Any HTML comment, blanked before paths are extracted.
#
# A comment renders as nothing, so it is not a path a reader can see, read or
# paste — the same visible-vs-invisible line `check-docs-orphans.sh` draws.
# Without this the gate reads its own waivers: a marker naming the path it
# exempts re-reports that path one line down, and the waiver can never win.
HTML_COMMENT = re.compile(r"<!--.*?-->", re.S)


def blank_comments(text):
    """Replace HTML comment bodies with spaces, preserving every line number."""
    return HTML_COMMENT.sub(
        lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)


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
    for match in WAIVER.finditer(text):
        path = match.group(1)
        # Anchored at the line the marker OPENS on, so a wrapped waiver is
        # scoped by where it was written rather than by where it happens to end.
        lineno = text.count("\n", 0, match.start()) + 1
        here = block_of[lineno - 1]
        scope = {here, here - 1}
        hits = covered.setdefault(path, set())
        hits.update(n for n, b in enumerate(block_of, 1) if b in scope)
    return covered


def documented(text):
    """Yield (line_no, path, requested) for every `/actuator/…` path a page shows.

    Missing-separator spellings are yielded too, so they are reported rather
    than skipped; nothing mounts them, so resolution rejects them on its own.

    `requested` marks a path the page hands someone to REQUEST rather than to
    read, which is the one distinction the prefix rule needs and the only one
    this extractor can honestly draw: a `curl` line, or an HTTP method followed
    by a path. `check-docs-cli.sh` separates its two populations the same way.
    """
    for lineno, line in enumerate(blank_comments(text).splitlines(), 1):
        requested = bool(REQUEST_LINE.search(line))
        for pattern in (DOC_PATH, MISSING_SEPARATOR):
            for match in pattern.finditer(line):
                path = match.group(0).rstrip(TRAILING)
                if path:
                    yield lineno, path, requested


# ── Resolution ───────────────────────────────────────────────────────────────

def resolves(path, mounted, requested=False):
    """Does `path` name something the framework mounts?

    Segment-wise, and permissive in the three ways the header lists: a mounted
    `{param}` matches any segment, a documented `*` matches any segment, and a
    documented path that is a strict PREFIX of a mounted one resolves.

    A TRAILING SLASH means "the subtree below this", so it requires something to
    actually be mounted below — which is the difference between `/actuator/`
    (a family, and how robots.txt and prose write it) and `/actuator/health/`
    (a leaf with a stray slash, which axum answers with a 404).

    `requested=True` withdraws the prefix rule, and only that rule. A path a
    page hands someone to REQUEST has to be a whole path: `curl …/actuator` and
    `GET /actuator/webhooks` both 404, however sound `/actuator/webhooks` is as
    the NAME of a family two lines up in prose. The `*` and `{param}` rules
    stand, since a page writing `/actuator/*` in a command is still naming a
    family rather than promising that literal string answers.
    """
    subtree = path.endswith("/")
    segs = [s for s in path.strip("/").split("/") if s]
    for target in mounted:
        tsegs = [s for s in target.strip("/").split("/") if s]
        if len(segs) > len(tsegs):
            continue
        # Nothing is mounted below a path that IS a mounted path, so a trailing
        # slash on one names a route the router does not have.
        if subtree and len(segs) == len(tsegs):
            continue
        # A request must name the whole path, not a prefix of one — unless the
        # shortfall is spelled `*`, which is family notation in any position.
        if requested and len(segs) < len(tsegs) and "*" not in segs:
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
        for lineno, path, requested in documented(text):
            checked += 1
            if resolves(path, mounted, requested):
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
    found = sorted(p for _, p, _r in documented(text))
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
    wrapped = ("| `/actuator/gone` |\n\n"
               "<!-- route-surface-allow: /actuator/gone — a reason long\n"
               "     enough to wrap onto a second line -->\n")
    cases.append((
        "a wrapped waiver covers the block above it",
        1 in waived_lines(wrapped).get("/actuator/gone", set()),
        True,
    ))
    cases.append((
        "a waiver's own text is not read as a documented path",
        [(n, q) for n, q, _r in documented(wrapped)],
        [(1, "/actuator/gone")],
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

    # A mount confers existence; a second copy of the list does not. Both halves
    # are pinned, because a pattern loose enough to admit the inventory and one
    # tight enough to miss a real mount fail in opposite directions and only one
    # of them is visible from a green corpus run.
    mount_src = '.route(\n    &actuator_route_path(prefix, "/served"),\n    get(h),\n)'
    inventory_src = 'paths.push(actuator_route_path(prefix, "/inventory_only"));'
    alert_src = 'crate::actuator::actuator_route_path(prefix, "/pointer_only")'
    cases.append((
        "a `.route(…)` mount is read",
        ROUTE_CALL.findall(mount_src), ["/served"],
    ))
    cases.append((
        "an inventory entry is not read",
        ROUTE_CALL.findall(inventory_src), [],
    ))
    cases.append((
        "an alert `where_to_look` pointer is not read",
        ROUTE_CALL.findall(alert_src), [],
    ))
    cases.append((
        "a mount inside a `#[cfg(test)] mod` is not read",
        ROUTE_CALL.findall(strip_test_mods(
            "#[cfg(test)]\nmod tests {\n" + mount_src + "\n}\n")),
        [],
    ))

    # The prefix boundary. Truncating `/actuatorhealth` down to `/actuator`
    # would have it RESOLVE against the bare-prefix rule, so the gate would
    # green-light a URL that 404s. Both the extraction and the verdict are
    # pinned, since the bug was only visible in their combination.
    cases.append((
        "a missing separator is reported, not truncated to the prefix",
        [(n, q) for n, q, _r in documented("see /actuatorhealth for status\n")],
        [(1, "/actuatorhealth")],
    ))
    cases.append((
        "a missing separator does not resolve",
        resolves("/actuatorhealth", surface), False,
    ))
    cases.append((
        "a hyphenated sibling route is out of scope, not a typo",
        [(n, q) for n, q, _r in documented("mounted at /actuator-dashboard by the app\n")],
        [],
    ))
    cases.append((
        "the bare prefix still resolves",
        [(n, q) for n, q, _r in documented("everything under /actuator is gated\n")],
        [(1, "/actuator")],
    ))

    # A trailing slash is significant — axum mounts no normalizing layer — so it
    # is read as "the subtree below this" rather than dropped. That is what the
    # two shapes the corpus writes both mean, and it rejects the one that 404s.
    cases.append((
        "a trailing slash on the prefix names the family",
        resolves("/actuator/", surface), True,
    ))
    cases.append((
        "a trailing slash on a mounted family resolves",
        resolves("/actuator/webhooks/", surface), True,
    ))
    cases.append((
        "a trailing slash on a leaf is a defect",
        resolves("/actuator/health/", surface), False,
    ))
    cases.append((
        "a trailing slash is kept, not stripped",
        [(n, q) for n, q, _r in documented("Disallow: /actuator/\n")],
        [(1, "/actuator/")],
    ))

    # Three suffixes a corpus survey found genuinely terminating a path. Each
    # must stop the match without being reported, because "reject every
    # unrecognized suffix" would report all three and every one is correct.
    cases.append((
        "a query string is not part of the path",
        [(n, q) for n, q, _r in documented("GET /actuator/logfile?level=warn\n")],
        [(1, "/actuator/logfile")],
    ))
    cases.append((
        "a markdown autolink terminates the path",
        [(n, q) for n, q, _r in documented("<http://localhost:3000/actuator/health>\n")],
        [(1, "/actuator/health")],
    ))
    cases.append((
        "a `::` logger target resolves as a {name} value",
        resolves(next(q for _, q, _r in documented(
            "curl -X PUT .../actuator/loggers/my_app::orders\n")), surface),
        True,
    ))

    # A prefix is a NAME in prose and a 404 in a request, and the extractor can
    # tell those apart. Both directions are pinned: withdrawing the prefix rule
    # too widely would report the ~20 `/actuator/*` family mentions and the
    # `access_log_exclude = [… "/actuator" …]` config values, every one correct.
    cases.append((
        "a prefix resolves when a page NAMES it",
        resolves("/actuator/webhooks", surface, requested=False), True,
    ))
    cases.append((
        "a prefix does not resolve when a page REQUESTS it",
        resolves("/actuator/webhooks", surface, requested=True), False,
    ))
    cases.append((
        "the bare prefix does not resolve in a request either",
        resolves("/actuator", surface, requested=True), False,
    ))
    cases.append((
        "`*` is family notation in a request too",
        resolves("/actuator/*", surface, requested=True), True,
    ))
    cases.append((
        "a whole mounted path resolves in a request",
        resolves("/actuator/health", surface, requested=True), True,
    ))
    for line, want in (
        ("curl http://localhost:3000/actuator/webhooks", True),
        ("GET /actuator/logfile?level=warn", True),
        # A route-table column, not a request: the method FOLLOWS the path.
        ("/actuator/*  GET      -> actuator", False),
        ('access_log_exclude = ["/health", "/actuator", "/static"]', False),
        ("Everything under `/actuator/webhooks` is sensitive.", False),
    ):
        got = [r for _, _, r in documented(line + "\n")]
        cases.append((f"request-position detection: {line[:40]!r}",
                      got and got[0], want if got else None))

    # Reader-facing surfaces a `*.md`-under-`docs/` view of the corpus misses.
    # The scaffolded README is a `.md.tmpl`; `.claude/skills/` is a second skill
    # tree the agent machinery loads by name; the wiki example compiles its
    # content pages in and serves them at `/docs/...`.
    for path in ("autumn-cli/src/templates/README.md.tmpl",
                 ".claude/skills/run-autumn/SKILL.md",
                 "examples/wiki/content/configuration.md"):
        cases.append((f"`{path}` is in the corpus", path in corpus(ROOT), True))

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
