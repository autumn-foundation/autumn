#!/usr/bin/env bash
# Macro-argument drift gate: every keyword argument the reader-facing docs put
# inside an Autumn attribute macro must name a key that macro parses.
#
# WHY THIS EXISTS: the corpus already gates six of the seven things a reader
# copies off a page.
# `scripts/check-docs-links.sh` gates its *links* (a 404 on GitHub),
# `scripts/check-docs-cli.sh` its *commands* (`unrecognized subcommand`),
# `scripts/check-docs-config.sh` the `AUTUMN_*` variables they SET (a silent
# no-op), `scripts/check-docs-toml.sh` the `autumn.toml` keys they WRITE
# (dropped silently), `scripts/check-docs-symbols.sh` the `autumn_web::…` paths
# they IMPORT (E0432 against their own file), and
# `scripts/check-docs-routes.sh` the `/actuator/…` URLs they REQUEST (a 404
# that reads like a feature they failed to enable). `check-docs-orphans.sh`
# asserts the page can be reached at all.
#
# Nothing gated the surface the guide spends most of its Rust on: the
# *arguments* to the attribute macros. `#[secured]`, `#[job]`, `#[scheduled]`,
# `#[cached]`, `#[throttle]`, `#[model]`, `#[repository]` and their siblings
# each carry a bespoke keyword grammar, and a page can name a key that macro
# has never parsed. The reader pastes the annotation onto their own handler and
# the build stops on their file, with a message about a grammar they were
# copying in good faith from the page that taught it to them.
#
# The baseline run found five occurrences of one spelling:
# `#[secured(policy = "…")]`, across `docs/guide/downloads.md` (twice),
# `skills/autumn-web/references/examples.md`, and — the sharp half — the
# rustdoc module headers of `autumn/src/download.rs` and `autumn/src/range.rs`,
# which ship to docs.rs as the landing pages for `Download` and ranged
# responses. `#[secured]` has never had a `policy` key: its grammar is bare
# role literals and/or `scopes = ["…"]`, and a `policy` key lands on the
# catch-all arm of `parse_secured_args` and fails the build. The corpus already
# spelled the working form twice (`#[secured(scopes = ["reports:read"])]`, in
# `docs/guide/openapi.md` and `docs/guide/authentication.md`), so the page that
# would have rescued the reader existed — under a different key AND a different
# separator (`reports.read` vs `reports:read`), which is exactly the pair a
# reader cannot guess their way across.
#
# Why it survived every existing gate: both rustdoc fences are ```ignore, so
# rustdoc never compiles them, and the markdown fences are not compiled by
# anything at all. The spelling looked plausible enough to be copied forward
# from one file into four.
#
# ── Truth set ────────────────────────────────────────────────────────────────
#
# The accepted keys are read out of each macro's own source in
# `autumn-macros/src/`, never from a snapshot: a renamed key lands in the same
# commit as the rename, so this gate cannot go stale behind the crate it
# checks. A macro's argument grammar is expressed in exactly one of a handful
# of shapes, and the extractor reads all of them:
#
#   meta.path.is_ident("ttl")            `#[cached(ttl = …)]`
#   key != "grant" / ident == "table"    `#[agent_operable(grant = …)]`
#   key.as_deref() != Some("max_age")    `#[step_up(max_age = …)]`
#   "sum" => / "count" | "sum"           match-arm grammars
#
# The union is deliberately permissive. A gate that reports a key the macro
# does accept is worse than one that misses a key it doesn't: the first teaches
# readers to distrust the gate and gets waived away wholesale, the second only
# fails to catch what nothing was catching before. Every narrowing below is
# there because the permissive read produced a false positive on this corpus:
#
#   - **A macro whose grammar the extractor cannot read is skipped, not
#     failed.** If a source yields zero keys, this gate cannot judge its
#     arguments and says so under `--list` rather than reporting every key its
#     pages use. All 20 macros are judgeable today; the guard exists so that a
#     macro rewritten into a shape the extractor does not know goes quiet
#     instead of going loud against correct documentation.
#   - **Only fenced Rust is read.** `docs/guide/agent-authority.md` discusses a
#     `#[repository(.., grant = X)]` key in prose as an explicitly-named
#     follow-up that does not exist yet. That is a correct sentence about a
#     missing feature, and reporting it would be reporting the docs for being
#     accurate. Prose names keys; fences hand them over to be pasted, and only
#     the second is a thing a reader copies.
#   - **`==` is not a keyword argument.** `#[cfg(feature = "db")]`-style keys
#     are matched by `key =` but a comparison inside a macro argument is not,
#     hence the `=(?!=)` lookahead.
#
# ── Corpus ───────────────────────────────────────────────────────────────────
#
# Two halves, because this defect class lived in both:
#
#   1. The tracked markdown a reader browses — the guide, README, EXAMPLES,
#      CONTRIBUTING, and the `skills/` references the agent machinery loads by
#      name.
#   2. The rustdoc of every publishable crate, which ships to docs.rs. Two of
#      the five baseline defects were here, and they are the ones a reader is
#      most likely to trust: docs.rs is where you land when you look up the
#      type, and an ```ignore fence looks exactly like a compiled one.
#
# The archive trees are excluded for the same reason the sibling gates exclude
# them (`check-docs-toml.sh` states it at length): `docs/plans/`, `docs/adr/`,
# `docs/design/`, `docs/stories/`, `docs/reports/`, `docs/releases/`,
# `docs/migrations/`, `docs/schemas/`, `docs/perf/` and the dated brainstorming
# notes record what was proposed or shipped at a point in time. A migration
# guide's `# 0.3.x` block is *supposed* to show the spelling that no longer
# works; gating it would force the record to lie.
#
# Test trees are excluded from the rustdoc half: `autumn/tests/compile-fail/`
# exists to hold code that must not compile.
#
# ── Waivers ──────────────────────────────────────────────────────────────────
#
# A passage that must name a key this gate rejects — another framework's
# spelling shown for comparison, or a key whose macro landed after this ran —
# waives it beside the passage, with the reason:
#
#     <!-- macro-arg-allow: secured.policy — Spring's name; Autumn spells it
#          scopes = ["…"] -->
#
# In Rust source the same marker goes in a line comment. The waiver names
# `<macro>.<key>` so a waiver for one macro's key cannot silently bless
# another's.
#
# Run locally with:
#
#   scripts/check-docs-macro-args.sh             # the gate
#   scripts/check-docs-macro-args.sh --list      # what the gate read
#   scripts/check-docs-macro-args.sh --self-test # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# Kept in Python for the same reason as its sibling gates: fence tracking
# across two comment syntaxes and per-macro key extraction are both work that
# bash renders unreadable, and python3 is already a dependency of
# scripts/check-docs-cli.sh, scripts/check-docs-config.sh,
# scripts/check-docs-toml.sh and scripts/check-docs-routes.sh.
run_py() {
  python3 - "$@" <<'PYEOF'
import collections
import pathlib
import re
import sys

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])

# ── Truth set ────────────────────────────────────────────────────────────────

MACRO_SRC = ROOT / "autumn-macros" / "src"

# The source file that owns each attribute macro's argument grammar. Keyed by
# the macro name as it is written at a call site.
OWNERS = {
    "agent_operable": "agent_authority.rs",
    "api_doc": "api_doc.rs",
    "authorize": "authorize.rs",
    "cached": "cached.rs",
    "edge": "edge.rs",
    "event": "event.rs",
    "feature_flag": "feature_flag.rs",
    "inbound_mail": "inbound_mail.rs",
    "job": "job.rs",
    "lifecycle": "lifecycle.rs",
    "listener": "listener.rs",
    "mailer": "mailer.rs",
    "model": "model.rs",
    "query_budget": "query_budget.rs",
    "repository": "repository.rs",
    "scheduled": "scheduled.rs",
    "secured": "secured.rs",
    "service": "service.rs",
    "step_up": "step_up.rs",
    "throttle": "throttle.rs",
}

# Every shape a macro source uses to name an argument key it accepts. See the
# header for why the union is deliberately permissive.
KEY_PATTERNS = (
    r'is_ident\("([a-z_0-9]+)"\)',
    r'[!=]=\s*"([a-z_0-9]+)"',
    r'Some\("([a-z_0-9]+)"\)',
    r'"([a-z_0-9]+)"\s*=>',
    r'"([a-z_0-9]+)"\s*\|',
)

CFG_TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+[A-Za-z0-9_]+\s*\{")


def strip_test_mods(src):
    """Drop every brace-balanced `#[cfg(test)] mod …` body from `src`.

    Not for soundness — an extra key only makes this gate more permissive, and
    a miss is the safe direction. It is for the remediation hint: the failure
    message prints the accepted keys, and a test fixture matching one of the
    patterns above (`f.sig.ident == "h"` in `secured.rs`) would put `h` in
    front of a reader as though `#[secured(h = …)]` were a supported form. A
    gate whose advice looks like noise gets waived away wholesale.
    """
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


def accepted_keys():
    """Read each macro's accepted argument keys out of its own source."""
    out = {}
    for macro, filename in OWNERS.items():
        path = MACRO_SRC / filename
        if not path.exists():
            out[macro] = set()
            continue
        text = strip_test_mods(
            path.read_text(encoding="utf-8", errors="replace")
        )
        keys = set()
        for pattern in KEY_PATTERNS:
            keys |= set(re.findall(pattern, text))
        out[macro] = keys
    return out


# ── Corpus ───────────────────────────────────────────────────────────────────

# Records of what was proposed or shipped at a point in time, not instructions
# a reader follows today. See the header for why gating these is worse than not.
ARCHIVE_PREFIXES = (
    "docs/plans/",
    "docs/adr/",
    "docs/design/",
    "docs/stories/",
    "docs/reports/",
    "docs/releases/",
    "docs/migrations/",
    "docs/schemas/",
    "docs/perf/",
    "docs/ci-health/",
    "benchmarks/",
    "bmad/",
    "agents/",
)
ARCHIVE_FILES = (
    "CHANGELOG.md",
    "RELEASE_NOTES.md",
    "docs/architecture-autumn-2026-03-20.md",
    "docs/autumn-workflow-architecture.md",
    "docs/brainstorming-hybrid-rendering-2026-03-26.md",
    "docs/brainstorming-technical-challenges-2026-03-20.md",
    "docs/prd-autumn-2026-03-20.md",
    "docs/product-brief-autumn-2026-03-20.md",
    "docs/research-competitive-technical-2026-03-20.md",
    "docs/sprint-plan-autumn-2026-03-20.md",
    "docs/echo-dx-audit.md",
    "dx_audit_report.md",
    "eris_advisories.md",
)

# Publishable crates whose rustdoc ships to docs.rs.
RUSTDOC_CRATES = (
    "autumn",
    "autumn-cli",
    "autumn-macros",
    "autumn-edge",
    "autumn-search",
    "autumn-storage-s3",
    "autumn-cache-redis",
    "autumn-schema-core",
    "autumn-admin-plugin",
    "autumn-media-plugin",
)

WAIVER = re.compile(r"macro-arg-allow:\s*([a-z_0-9]+)\.([a-z_0-9]+)")
# A keyword argument, but never a `==` comparison.
KEYWORD_ARG = re.compile(r"\b([a-z_][a-z_0-9]*)\s*=(?!=)")


MACRO_OPEN = re.compile(r"#\[(" + "|".join(sorted(OWNERS)) + r")\(")


class MacroCalls:
    """Find `#[macro(…)]` calls and hand back their argument text.

    Depth-aware rather than regular, because the arguments are not
    bracket-free: `#[secured(scopes = ["a:b"])]` and
    `#[lifecycle(transitions = [...])]` both carry a nested array, and a
    `[^\\]]*` body stops dead at the first `]`. That made every
    array-valued form invisible to this gate — including `scopes`, the one
    working spelling the baseline defect had to be corrected *to*. Caught by
    renaming `scopes` in `secured.rs` and watching the gate stay silent when
    it should have reported every page still saying `scopes`.
    """

    @staticmethod
    def findall(text):
        out = []
        for match in MACRO_OPEN.finditer(text):
            i = match.end()
            depth = 1
            while i < len(text) and depth:
                ch = text[i]
                if ch in "([":
                    depth += 1
                elif ch in ")]":
                    depth -= 1
                    if depth == 0:
                        break
                i += 1
            if depth == 0:
                out.append((match.group(1), text[match.end():i]))
        return out


def macro_call_re():
    return MacroCalls


def is_archived(rel):
    s = str(rel)
    return s.startswith(ARCHIVE_PREFIXES) or s in ARCHIVE_FILES


def markdown_files():
    out = []
    for path in sorted(ROOT.rglob("*.md")):
        rel = path.relative_to(ROOT)
        parts = rel.parts
        if "target" in parts or ".git" in parts or "node_modules" in parts:
            continue
        if is_archived(rel):
            continue
        out.append(path)
    return out


def rustdoc_files():
    out = []
    for crate in RUSTDOC_CRATES:
        src = ROOT / crate / "src"
        if src.exists():
            out.extend(sorted(src.rglob("*.rs")))
    return out


def scan_markdown(path, accepted, judgeable, calls):
    """Yield (macro, key, line) for keyword args inside fenced Rust."""
    rel = path.relative_to(ROOT)
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    waived = set()
    for line in lines:
        for macro, key in WAIVER.findall(line):
            waived.add((macro, key))
    inside = False
    fences = 0
    found = []
    for lineno, line in enumerate(lines, 1):
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            if inside:
                inside = False
            else:
                lang = stripped[3:].strip().lower()
                inside = lang.startswith("rust")
                if inside:
                    fences += 1
            continue
        if not inside:
            continue
        for macro, args in calls.findall(line):
            if macro not in judgeable:
                continue
            for key in KEYWORD_ARG.findall(args):
                if key in accepted[macro] or (macro, key) in waived:
                    continue
                found.append((macro, key, f"{rel}:{lineno}"))
    return found, fences


def scan_rustdoc(path, accepted, judgeable, calls):
    """Same, over ```-fenced Rust inside `//!` and `///` doc comments."""
    rel = path.relative_to(ROOT)
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    waived = set()
    for line in lines:
        for macro, key in WAIVER.findall(line):
            waived.add((macro, key))
    inside = False
    fences = 0
    found = []
    for lineno, line in enumerate(lines, 1):
        doc = re.match(r"^\s*//[!/]\s?(.*)$", line)
        if not doc:
            # A non-doc line ends any fence: an unterminated fence must not
            # swallow the rest of the file.
            inside = False
            continue
        body = doc.group(1).strip()
        if body.startswith("```"):
            if inside:
                inside = False
            else:
                # rustdoc fences default to Rust, and the attribute-bearing
                # ones are usually `ignore` / `no_run` / `compile_fail`.
                lang = body[3:].strip().lower()
                inside = lang == "" or re.match(
                    r"^(rust|ignore|no_run|compile_fail|should_panic|edition\d+)",
                    lang,
                ) is not None
                if inside:
                    fences += 1
            continue
        if not inside:
            continue
        for macro, args in calls.findall(body):
            if macro not in judgeable:
                continue
            for key in KEYWORD_ARG.findall(args):
                if key in accepted[macro] or (macro, key) in waived:
                    continue
                found.append((macro, key, f"{rel}:{lineno}"))
    return found, fences


def run_scan():
    accepted = accepted_keys()
    judgeable = {m for m, keys in accepted.items() if keys}
    calls = macro_call_re()
    defects = []
    md_files = markdown_files()
    rs_files = rustdoc_files()
    md_fences = rs_fences = 0
    for path in md_files:
        found, fences = scan_markdown(path, accepted, judgeable, calls)
        defects.extend(found)
        md_fences += fences
    for path in rs_files:
        found, fences = scan_rustdoc(path, accepted, judgeable, calls)
        defects.extend(found)
        rs_fences += fences
    stats = {
        "md_files": len(md_files),
        "rs_files": len(rs_files),
        "md_fences": md_fences,
        "rs_fences": rs_fences,
        "accepted": accepted,
        "judgeable": judgeable,
    }
    return defects, stats


def main():
    defects, stats = run_scan()
    print(
        f"corpus: {stats['md_files']} markdown files "
        f"({stats['md_fences']} rust fences), "
        f"{stats['rs_files']} rustdoc sources ({stats['rs_fences']} fences)"
    )
    print(
        f"macros: {len(stats['judgeable'])}/{len(OWNERS)} with a readable "
        f"argument grammar"
    )
    print(f"defects: {len(defects)}")
    if not defects:
        return 0
    grouped = collections.defaultdict(list)
    for macro, key, loc in defects:
        grouped[(macro, key)].append(loc)
    for (macro, key), locs in sorted(grouped.items()):
        known = ", ".join(sorted(stats["accepted"][macro])) or "(none)"
        print(f"\n  #[{macro}({key} = …)] — {macro} has no `{key}` key", file=sys.stderr)
        print(f"      accepts: {known}", file=sys.stderr)
        for loc in locs:
            print(f"      {loc}", file=sys.stderr)
    return 1


def list_surface():
    accepted = accepted_keys()
    for macro in sorted(OWNERS):
        keys = sorted(accepted[macro])
        label = ", ".join(keys) if keys else "(grammar not readable — SKIPPED)"
        print(f"{macro:16} {label}")
    return 0


# ── Self-test ────────────────────────────────────────────────────────────────


def self_test():
    accepted = accepted_keys()
    judgeable = {m for m, keys in accepted.items() if keys}
    calls = macro_call_re()
    passed = failed = 0

    def check(name, got, want):
        nonlocal passed, failed
        if got == want:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL {name}: got {got!r}, want {want!r}", file=sys.stderr)

    import tempfile

    def scan_text(text, suffix):
        with tempfile.NamedTemporaryFile(
            "w", suffix=suffix, dir=ROOT, delete=False, encoding="utf-8"
        ) as fh:
            fh.write(text)
            tmp = pathlib.Path(fh.name)
        try:
            scanner = scan_markdown if suffix == ".md" else scan_rustdoc
            found, _ = scanner(tmp, accepted, judgeable, calls)
            return [(m, k) for m, k, _ in found]
        finally:
            tmp.unlink()

    # The truth set is read from the macro sources, not a snapshot.
    check("secured accepts scopes", "scopes" in accepted["secured"], True)
    check("secured rejects policy", "policy" in accepted["secured"], False)
    check("agent_operable accepts grant", "grant" in accepted["agent_operable"], True)
    check("step_up accepts max_age", "max_age" in accepted["step_up"], True)
    check("cached accepts ttl", "ttl" in accepted["cached"], True)
    check("model accepts table", "table" in accepted["model"], True)
    check("every macro is judgeable", len(judgeable), len(OWNERS))

    # A bad key inside a fence is a defect.
    check(
        "markdown: bad key in rust fence",
        scan_text('```rust\n#[secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    # The same key in prose is not: prose names keys, fences hand them over.
    check(
        "markdown: bad key in prose is ignored",
        scan_text('A `#[secured(policy = "x")]` key is the follow-up.\n', ".md"),
        [],
    )
    # Nor in a non-Rust fence.
    check(
        "markdown: bad key in non-rust fence is ignored",
        scan_text('```toml\n#[secured(policy = "x")]\n```\n', ".md"),
        [],
    )
    # A good key is not a defect.
    check(
        "markdown: good key passes",
        scan_text('```rust\n#[secured(scopes = ["a:b"])]\n```\n', ".md"),
        [],
    )
    # An array-valued argument must be *seen*, not skipped. A `[^\]]*` body
    # stops at the first `]` and silently drops every such form — which is how
    # a rename of `scopes` could land with the whole corpus still saying
    # `scopes` and this gate reporting a clean run.
    check(
        "markdown: array-valued arg is scanned, not skipped",
        scan_text('```rust\n#[secured(bogus = ["a:b"])]\n```\n', ".md"),
        [("secured", "bogus")],
    )
    check(
        "markdown: bad key after an array arg is seen",
        scan_text('```rust\n#[secured(scopes = ["a"], bogus = 1)]\n```\n', ".md"),
        [("secured", "bogus")],
    )
    check(
        "rustdoc: array-valued arg is scanned, not skipped",
        scan_text('//! ```ignore\n//! #[secured(bogus = ["a:b"])]\n//! ```\n', ".rs"),
        [("secured", "bogus")],
    )
    # `==` is a comparison, not a keyword argument.
    check(
        "markdown: == is not a keyword arg",
        scan_text("```rust\n#[cached(ttl = \"60s\")]\n```\n", ".md"),
        [],
    )
    # A waiver beside the passage suppresses exactly its own macro.key.
    check(
        "markdown: waiver suppresses its own key",
        scan_text(
            "<!-- macro-arg-allow: secured.policy — another framework's name -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: waiver does not bless another macro",
        scan_text(
            "<!-- macro-arg-allow: job.policy -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )

    # rustdoc half: an `ignore` fence is exactly where the baseline defects hid.
    check(
        "rustdoc: bad key in ignore fence",
        scan_text('//! ```ignore\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: bad key in bare fence",
        scan_text('/// ```\n/// #[secured(policy = "x")]\n/// ```\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: good key passes",
        scan_text('//! ```ignore\n//! #[secured(scopes = ["a:b"])]\n//! ```\n', ".rs"),
        [],
    )
    # Real code outside a doc comment is not documentation.
    check(
        "rustdoc: non-doc code is ignored",
        scan_text('#[secured(policy = "x")]\nfn f() {}\n', ".rs"),
        [],
    )
    # A non-doc line closes an unterminated fence rather than swallowing on.
    check(
        "rustdoc: unterminated fence does not run away",
        scan_text('//! ```ignore\nfn f() {}\n#[secured(policy = "x")]\n', ".rs"),
        [],
    )
    # A text fence in rustdoc is prose, not code to paste.
    check(
        "rustdoc: text fence is ignored",
        scan_text('//! ```text\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [],
    )

    print(f"self-test: {passed}/{passed + failed} passed")
    return 1 if failed else 0


sys.exit(
    {"--self-test": self_test, "--list": list_surface}.get(MODE, main)()
)
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
    echo "Checking Autumn macro arguments across the reader-facing docs..."
    if run_py --check "$root"; then
      echo "Macro argument gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the docs hand a reader an Autumn attribute macro with a keyword argument
that macro does not parse (above).

The reader pastes the annotation onto their own handler and the build stops on
their file, quoting a grammar they were copying in good faith from the page
that taught it to them. Nothing compiles these fences — rustdoc's ```ignore
blocks and markdown fences alike — so the spelling can be copied forward from
one page into four before anybody types it.

Fix each one where it lives:
  - wrong key       -> use the key the macro parses (the `accepts:` line lists
                       them, read out of the macro's own source)
  - key not shipped -> land the macro change first; this gate reads
                       `autumn-macros/src/`, so a new key needs no snapshot
                       update
  - another framework's name, shown for comparison -> waive it beside the
                       passage, with the Autumn spelling in the reason:

      <!-- macro-arg-allow: secured.policy — Spring's name; Autumn spells it
           scopes = ["…"] -->

Inspect what the gate read:  scripts/check-docs-macro-args.sh --list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--list|--self-test]" >&2
    exit 2
    ;;
esac
