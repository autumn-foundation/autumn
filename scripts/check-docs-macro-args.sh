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
#   key != "grant"                       `#[agent_operable(grant = …)]`
#   key.as_deref() != Some("max_age")    `#[step_up(max_age = …)]`
#   match key.as_str() { "resource" =>   `#[authorize(resource = …)]`
#
# The last two are read only where the thing being tested is a *key*. A
# comparison needs a positively key-ish left side rather than merely a
# not-value-ish one: `window != "pending"` in `job.rs` tests what follows
# `unique_window =`, and `basis == "deleted_at"` in `repository.rs` tests a
# retention basis. Neither comparand reads as a value, so defaulting the
# unknown case to "key" admitted both and let `#[job(pending = true)]` pass. The same shape
# spells out values elsewhere — `repository.rs` matches `"destroy"`,
# `"delete_all"`, `"nullify"` and `"restrict"` as the spellings accepted after
# `on_delete =`, inside `parse_repo_args` itself — and reading those arms let
# `#[repository(Post, delete_all = true)]` pass, while reading none of them
# lost `authorize`'s two real keys. What the scrutinee was built from settles
# it: `key.get_ident()` dispatches keys, `nested.value()?` dispatches values.
#
# One key belongs to every macro and appears in no owner file:
# `crate_path::extract_crate_override` strips `crate = "…"` before any parser
# runs, and all 33 entry points call it. Omitting it made the supported
# `#[get("/x", crate = "autumn_web_05")]` report as drift.
#
# **Where** those patterns are read matters as much as what they match. Run
# over a whole source file they are far too generous: `model.rs` is ~10k lines
# of codegen, and reading all of it accepted `username`, `mouse` and `goose` as
# `#[model(...)]` keys — so `#[model(username = "x")]` passed a corpus run
# clean. `parse_attr_args`, the function that actually parses that attribute,
# takes `table` and `managed`.
#
# So extraction is scoped to the macro's own argument parser, found
# structurally rather than by name: the function whose signature takes the raw
# `attr: TokenStream` but NOT `item: TokenStream`. That is the dedicated arg
# parser; the one taking both is the macro entry point, which reaches the
# entire implementation. Alongside it, the `syn::Parse` impl for the type the
# macro parses its arguments into — `api_doc` and `agent_operable` parse that
# way and have no attr-taking function at all, so without it `api_doc` had no
# readable grammar across 53 guide examples and `agent_operable` fell back to
# its whole macro entry and accepted `cfg`, `fn` and `jobs` as keys. The two
# are additive rather than a fallback chain. No macro in the corpus currently
# has both — `static_get`, the case that motivated the union, turned out not
# to use the shared route parser at all — so the union is inert today and is
# kept for its failure direction: a dropped root narrows the accepted set and
# reports correct pages as drift, while a superfluous one only widens it and
# costs a miss. A `Parse` root is the implementing TYPE, never the
# bare method name — `code_blocks` groups same-named methods together, so
# rooting at `parse` dragged `EffectSpec::parse` in beside
# `OperableAttr::parse`. Only when neither exists does the macro entry serve.
#
# From the root the reader follows calls transitively into other functions and
# `impl` blocks, so a grammar split across helpers (`job.rs`'s
# `parse_basic_arg` / `parse_uniqueness_arg` / `parse_concurrency_arg`) is read
# whole.
#
# Three things bound that walk, each because ignoring it produced a wrong
# answer on this corpus:
#
#   - **A block parsing a nested group's grammar is not read.**
#     `Attribute::parse_nested_meta` parses an attribute's own arguments;
#     `ParseNestedMeta::parse_nested_meta` descends into a group, and the
#     receiver says which. Reading the descents promoted `action` (inside
#     `dependent(…)`) and `after`/`basis` (inside `retention(…)`) to top-level
#     `#[repository]` keys and let `#[repository(Post, action = true)]` pass.
#     Nesting *depth* could not tell them apart: `parse_repo_args` enters its
#     own top level through `syn::meta::parser`, not through a
#     `parse_nested_meta` call, so every such call in it is already a descent.
#
#   - **A callee is followed only if it receives the attribute** in some syn
#     form (`ParseNestedMeta`, `Meta`, `TokenStream`, `Attribute`, …). One
#     taking a bare `&str` is parsing a *value* already handed to it, and its
#     literals are values: `DependentAction::parse(action: &str)` matches
#     `delete_all`, `destroy`, `nullify` and `restrict`, which are spellings
#     accepted *after* `dependent =`, never keys of `#[model(...)]`. Following
#     it let `#[model(delete_all = true)]` pass.
#   - **A macro's grammar may span files.** The route verbs dispatch from
#     `route.rs` but parse their keys in the shared `parse.rs`, so a
#     single-file read reported `#[get(api_version = "v1")]` — three correct
#     pages of `docs/guide/api-versioning.md` — as drift. `OWNERS` therefore
#     takes a tuple where one file is not the whole story.
#
# Symmetrically, only the attribute's **own** keys are judged: a nested group
# carries its own grammar, so `#[get("/about", seo(title = …, og_type = …))]`
# names `title` and `og_type` as keys of `seo(...)`. Judging them against
# `#[get]` reported three correct SEO pages as drift. Nested grammars are not
# checked at all, which is the safe direction.
#
# The union is still deliberately permissive within that scope. A gate that
# reports a key the macro does accept is worse than one that misses a key it
# doesn't: the first teaches readers to distrust the gate and gets waived away
# wholesale, the second only fails to catch what nothing was catching before.
# Every narrowing below is there because the permissive read produced a false
# positive on this corpus:
#
#   - **A macro whose grammar the extractor cannot read is skipped, not
#     failed.** If the scope yields zero keys, this gate cannot judge that
#     macro's arguments and says so under `--list` rather than reporting every
#     key its pages use. Three are skipped today — `mailer_preview`, `service`
#     and `sim_test` — and the other 30 are judged. A macro whose parser was
#     *found* but yields no keys is a different case: it is a marker like
#     `#[public]`, `crate` is its whole grammar, and it is judged on that
#     alone. Knowing there is nothing is not the same as knowing nothing —
#     though it can also be a *forwarder*: `oauth2_callback` hands its
#     arguments to `route::route_macro`, so reading only its own file left it
#     with `crate` alone and reported a valid `timeout_ms` as drift. A
#     self-test checks the registry covers every macro entry a macro forwards
#     `attr` to, rather than trusting the marker inference. The self-test
#     holds a floor under that count so a refactor cannot quietly empty the
#     truth set, the failure mode where a gate keeps passing because it
#     stopped looking, and a second case fails if `lib.rs` exports a macro
#     `OWNERS` does not name: an unregistered macro is not a permissive read
#     but no read at all.
#   - **Only fenced Rust is read.** `docs/guide/agent-authority.md` discusses a
#     `#[repository(.., grant = X)]` key in prose as an explicitly-named
#     follow-up that does not exist yet. That is a correct sentence about a
#     missing feature, and reporting it would be reporting the docs for being
#     accurate. Prose names keys; fences hand them over to be pasted, and only
#     the second is a thing a reader copies.
#   - **`==` is not a keyword argument.** `#[cfg(feature = "db")]`-style keys
#     are matched by `key =` but a comparison inside a macro argument is not,
#     hence the `=(?!=)` lookahead.
#   - **A bare flag is an argument; a positional is not.** `#[job(unique)]` and
#     `#[model(managed)]` take no value, and a typo in one fails the build
#     exactly like a mistyped key — reading only `key =` left `#[job(uniqe)]`
#     passing clean. But the type in `#[repository(Post, …)]`, the role literal
#     in `#[secured("admin")]` and the value in `resource = Post` are
#     positional, so literals and nested groups are collapsed to a placeholder
#     before the split and a segment carrying `=` yields only its left side.
#   - **A delimiter inside a literal or a comment is data, not structure.** The
#     depth scan skips `"…"`, `'…'`, `r#"…"#`, `// …` and (nesting) `/* … */`
#     before counting, so neither `#[secured("admin)", policy = "x")]` nor
#     `#[secured(/* ) */ policy = "x")]` ends at that `)`. Any argument
#     carrying a route pattern, regex or glob has the same shape.
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
# "Beside the passage" is enforced, not merely advised: a marker covers the
# fence it is inside, or else the next one to open within `WAIVER_REACH` lines,
# and is then spent. Applying that window independently to every fence let one
# marker waive several — two consecutive one-line fences both sit within six
# lines of a marker written for the first. Collapsing every marker in a file to one `(macro, key)` set
# would mean a single legitimate waiver near the top of a long guide silently
# accepting every later use of that key on the page, including an unrelated
# typo — a waiver that reads as local but behaves as a file-wide opt-out.
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
#
# Every `#[proc_macro_attribute]` `autumn-macros` exports is registered, and
# `registry_covers_every_exported_macro` in the self-test reads that list out of
# `lib.rs` and fails if one is missing. An unregistered macro is not a
# permissive read, it is no read at all: its pages go completely ungated while
# the gate still reports a clean run. The route verbs and `#[task]` were absent
# from the first draft, which left documented keys like
# `#[get(…, api_version = …)]` and `#[task(name = …)]` unchecked.
OWNERS = {
    "agent_operable": "agent_authority.rs",
    "api_doc": "api_doc.rs",
    "authorize": "authorize.rs",
    "cached": "cached.rs",
    "delete": ("route.rs", "parse.rs"),
    "edge": "edge.rs",
    "event": "event.rs",
    "feature_flag": "feature_flag.rs",
    "get": ("route.rs", "parse.rs"),
    "inbound_mail": "inbound_mail.rs",
    "job": "job.rs",
    "lifecycle": "lifecycle.rs",
    "listener": "listener.rs",
    "mailer": "mailer.rs",
    "mailer_preview": "mailer_preview.rs",
    "main": "main_macro.rs",
    "model": "model.rs",
    # A forwarder, not a marker: `oauth2_callback_macro` hands its arguments
    # straight to `route::route_macro`, so it accepts the whole route grammar.
    # Reading only its own file left it with `crate` alone and reported the
    # valid `#[oauth2_callback("/cb", timeout_ms = …)]` as drift. Its other
    # call, `edge::reject_if_edge`, takes the ITEM and looks for a separate
    # `#[edge]` attribute, so `edge.rs` is not part of this grammar —
    # registering it made `edge`, `needs` and `kv` look like OAuth arguments.
    "oauth2_callback": ("oauth2_callback.rs", "route.rs", "parse.rs"),
    "patch": ("route.rs", "parse.rs"),
    "post": ("route.rs", "parse.rs"),
    "public": "public.rs",
    "put": ("route.rs", "parse.rs"),
    "query_budget": "query_budget.rs",
    "repository": "repository.rs",
    "scheduled": "scheduled.rs",
    "secured": "secured.rs",
    "service": "service.rs",
    "sim_test": "sim_test.rs",
    # NOT a route alias either: `StaticGetAttrs::parse` accepts `params`,
    # `revalidate` and `seo` and nothing else, and `static_get_macro` never
    # hands `attr` to `route::route_macro` — it only borrows
    # `crate::parse::SeoAttrArgs` for the nested `seo(...)` group. Registering
    # the shared route parser made `timeout_ms`, `api_version` and `name` look
    # valid here. (In round nine I kept `parse.rs` believing `params` came from
    # it; `params` is in `static_route.rs`, so that reasoning was wrong.)
    "static_get": "static_route.rs",
    "step_up": "step_up.rs",
    "task": "one_off_task.rs",
    "throttle": "throttle.rs",
    # NOT a route alias despite the shape: `ws_macro` calls
    # `parse::parse_route_path(attr)`, which takes a single path literal, so it
    # has no keyword grammar of its own. Registering the shared route parser
    # made `api_version`, `timeout_ms` and the rest look valid here.
    #
    # Known residue: reading `ws.rs` alone still yields `seo`, from
    # `reject_seo_argument`'s `ident == "seo"` — the one key `#[ws]` goes out
    # of its way to REJECT. The extractor cannot tell an acceptance test from a
    # rejection test, so `#[ws(.., seo(...))]` is missed. That is the safe
    # direction (a miss, not a false report) and it is one key on one macro;
    # the alternative, registering `parse.rs` to reach the path-only parser,
    # drags the whole route grammar back in with it.
    "ws": "ws.rs",
}

# Every shape a macro source uses to name an argument key it accepts. See the
# header for why the union is deliberately permissive.
KEY_PATTERNS = (
    r'is_ident\("([a-z_0-9]+)"\)',
    r'Some\("([a-z_0-9]+)"\)',
)

# `x == "…"` names a key only when `x` is one. The same shape compares values:
# `window != "pending"` in `job.rs` tests what follows `unique_window =`, and
# `value == "fetch"` / `basis == "deleted_at"` in `repository.rs` do the same
# for `validate_on_update` and the retention basis. Reading them let
# `#[job(pending = true)]` and `#[repository(Post, deleted_at = true)]` pass.
#
# Unlike the match-arm reader this requires a positively key-ish left side
# rather than merely a not-value-ish one: `window` and `basis` are neither, and
# defaulting an unknown comparand to "key" is what admitted them.
COMPARISON = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)\s*[!=]=\s*\"([a-z_0-9]+)\"")


def comparison_keys(text):
    return {
        value
        for lhs, value in COMPARISON.findall(text)
        if KEYISH_SCRUTINEE.match(lhs)
    }

# Some grammars dispatch keys through a `match` rather than `is_ident`, so the
# arms carry real keys — `authorize.rs` does exactly that for `resource` and
# `from`. But so do *value* grammars: `repository.rs` matches `"destroy"`,
# `"delete_all"`, `"nullify"` and `"restrict"` as the spellings accepted after
# `on_delete =`, and those arms sit in `parse_repo_args` itself, so no
# callee filter can reach them. Reading every arm made
# `#[repository(Post, delete_all = true)]` pass; reading none of them lost
# `authorize`'s two real keys.
#
# The scrutinee separates them, and it is not a naming convention but what the
# expression was built from:
#
#   match key.as_str() { "resource" => …   ← key.get_ident(), a KEY dispatch
#   match value.to_string().as_str() { …   ← nested.value()?, a VALUE dispatch
#
# So match arms are collected only from blocks whose scrutinee reads as a key
# and not as a value.
MATCH_BLOCK = re.compile(r"\bmatch\s+([^{\n]{0,120}?)\s*\{")
# Loose on the trailing edge, and symmetrically so: the scrutinee is as often
# `key_str.as_str()` as `key.as_str()`, and as often `value_str` as `value`.
# An exact-word match missed `api_doc`'s `match key_str.as_str()` and left its
# four real keys out of the truth set.
KEYISH_SCRUTINEE = re.compile(r"\b(key|path|ident|name)\w*")
VALUEISH_SCRUTINEE = re.compile(r"\b(value|val|action|kind|lit)\w*")
MATCH_ARM = re.compile(r'"([a-z_0-9]+)"\s*(?:\||=>)')


def match_arm_keys(text):
    """Keys from `match` arms, but only where the scrutinee is a key."""
    keys = set()
    for block in MATCH_BLOCK.finditer(text):
        scrutinee = block.group(1)
        if VALUEISH_SCRUTINEE.search(scrutinee):
            continue
        if not KEYISH_SCRUTINEE.search(scrutinee):
            continue
        body, _ = _balanced(text, block.end() - 1)
        keys |= set(MATCH_ARM.findall(body))
    return keys


# Accepted by every attribute macro, and by none of their own parsers:
# `crate_path::extract_crate_override` strips `crate = "…"` off the token
# stream before the macro's parser ever sees it, and all 33 entry points in
# `lib.rs` call it. Reading only the owner files therefore made
# `#[get("/x", crate = "autumn_web_05")]` — a supported form, documented for
# renamed dependencies — report as drift.
UNIVERSAL_KEYS = frozenset({"crate"})

def strip_rust_comments(src):
    """Blank every comment, preserving length so offsets still line up.

    A doc comment is prose, and prose does not balance braces. `route.rs:734`
    documents its generated code as `/// pub fn __autumn_path_handler(…) {`,
    which the function scanner read as a real definition whose body then ran
    21,000 characters into `parse.rs` — far enough to pick up
    `is_ident("intercept")` and make `intercept`, the name of a *separate*
    attribute, an accepted `#[get]` key.

    Literals are skipped first: a `//` inside a string is not a comment.
    """
    out, i = [], 0
    while i < len(src):
        ch = src[i]
        if ch == "/" and src[i + 1 : i + 2] in ("/", "*"):
            end = skip_comment(src, i)
            if end is not None:
                # Keep newlines so line numbers and `#[cfg(test)] mod` shapes
                # elsewhere are unaffected.
                out.append("".join("\n" if c == "\n" else " " for c in src[i:end]))
                i = end
                continue
        if ch == "r" and src[i + 1 : i + 2] in ('"', "#"):
            nxt = skip_raw_literal(src, i)
            if nxt is not None:
                out.append(src[i:nxt])
                i = nxt
                continue
        if ch == '"':
            nxt = skip_literal(src, i)
            out.append(src[i:nxt])
            i = nxt
            continue
        if ch == "'":
            nxt = _skip_rust_char(src, i)
            if nxt is not None:
                out.append(src[i:nxt])
                i = nxt
                continue
        out.append(ch)
        i += 1
    return "".join(out)


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


FN_OPEN = re.compile(r"\bfn\s+([a-z_0-9]+)\s*(?:<[^>]*>)?\s*\(")
IMPL_OPEN = re.compile(r"\bimpl\b[^{;]*?\bfor\s+([A-Za-z][A-Za-z0-9_]*)\s*\{")
# Any impl block, inherent or trait — used only to find method spans.
IMPL_BLOCK = re.compile(r"\bimpl\b[^{;()]{0,120}?\{")
IDENT = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\b")
ATTR_PARAM = re.compile(r"\battr\s*:\s*(?:proc_macro2::)?TokenStream")
ITEM_PARAM = re.compile(r"\bitem\s*:\s*(?:proc_macro2::)?TokenStream")

# A function that parses attribute *arguments* receives the attribute in some
# syn form. One that receives only a `&str` is parsing a *value* already handed
# to it, and its string literals are values, not keys — `DependentAction::parse
# (action: &str)` in `model.rs` matches `delete_all`, `destroy`, `nullify` and
# `restrict`, which are spellings accepted *after* `dependent =`, never keys of
# `#[model(...)]` itself. Following it made `#[model(delete_all = true)]` pass.
# `job.rs`'s three helpers take `ParseNestedMeta` and must stay reachable, so
# the test is on the parameter types rather than on the function name.
PARSE_STREAM_PARAM = re.compile(r":\s*(?:syn::)?(?:parse::)?ParseStream\b")
# An `impl Parse` body, recognised by the method it must define.
PARSE_IMPL_BODY = re.compile(
    r"\bfn\s+parse\s*\(\s*[a-z_0-9]+\s*:\s*(?:syn::)?(?:parse::)?ParseStream"
)
# The type a macro parses its ARGUMENTS into. Both forms are anchored on
# `attr`: without that anchor `parse_args::<EffectSpec>` matched too, making
# `#[agent_effect]`'s grammar a second root for `#[agent_operable]` and handing
# it `cross_tenant`, `writes` and `jobs`.
PARSED_INTO = re.compile(
    r"(?:parse2::<\s*([A-Za-z][A-Za-z0-9_]*)\s*>\s*\(\s*attr\b"
    r"|\battr\s*\.\s*parse_args::<\s*([A-Za-z][A-Za-z0-9_]*)\s*>)"
)
ARG_PARAM = re.compile(
    r":\s*&?\s*(?:mut\s+)?(?:syn::)?(?:meta::)?"
    r"(ParseNestedMeta|Meta|MetaList|TokenStream|Attribute|ParseStream|ExprLit|Expr|Lit)\b"
)


def _skip_rust_char(src, i):
    """Index just past a Rust char literal at `i`, or None if it is a lifetime.

    `'` is ambiguous in Rust: `'{'` is a literal whose brace must not be
    counted, while `'a` in `&'a str` is a lifetime and consumes nothing. A
    literal closes on the next `'`, allowing one escape.
    """
    if src[i + 1 : i + 2] == "\\":
        end = src.find("'", i + 2)
        return end + 1 if end != -1 else None
    if src[i + 2 : i + 3] == "'":
        return i + 3
    return None


def _balanced(src, open_idx, opener="{", closer="}"):
    """Text between `open_idx`'s delimiter and its match, in real Rust.

    Delimiters inside strings, char literals and comments are data. Counting
    them naively is not a rounding error: `positional_format_string` in
    `route.rs` matches on `'{'` and `'}'`, so its body ran 69,000 characters
    past its own closing brace and swallowed the rest of the file. Every
    identifier in that tail then looked reachable from it, which is how
    `intercept` — the name of a *separate* attribute, parsed by
    `extract_interceptors` — became an accepted `#[get]` key.
    """
    i, depth = open_idx + 1, 1
    while i < len(src) and depth:
        ch = src[i]
        if ch == "/":
            nxt = skip_comment(src, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == "r" and src[i + 1 : i + 2] in ('"', "#"):
            nxt = skip_raw_literal(src, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == '"':
            i = skip_literal(src, i)
            continue
        if ch == "'":
            nxt = _skip_rust_char(src, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == opener:
            depth += 1
        elif ch == closer:
            depth -= 1
        i += 1
    return src[open_idx + 1 : i - 1], i


# `Attribute::parse_nested_meta` parses an attribute's OWN arguments;
# `ParseNestedMeta::parse_nested_meta` descends into a nested group. The
# receiver is which of the two this is, and the two call sites read
# `attr.parse_nested_meta(…)` and `meta.parse_nested_meta(…)` accordingly.
NESTED_META_DESCENT = re.compile(r"\b(?!attr\b|input\b)([a-z_0-9]+)\.parse_nested_meta\s*\(")


def strip_nested_group_parsers(text):
    """Drop the bodies of blocks that parse a *nested group's* grammar.

    `#[repository(…, dependent(fk = …, action = …), retention(after = …,
    basis = …))]` parses its own keys through `syn::meta::parser(|meta| …)` and
    each group's keys through `meta.parse_nested_meta(|nested| …)`. Reading the
    inner blocks promoted `action`, `after` and `basis` to top-level keys and
    let `#[repository(Post, action = true)]` pass.

    Depth alone could not tell them apart: `parse_repo_args` enters its top
    level through `syn::meta::parser`, not through a `parse_nested_meta` call,
    so every such call in it is already a descent. The receiver is what
    distinguishes them.

    This is the truth-set mirror of `top_level_keys`, which already declines to
    judge a nested group's keys on the documentation side. Both sides now stop
    at the same boundary.
    """
    out, cursor = [], 0
    for match in NESTED_META_DESCENT.finditer(text):
        if match.start() < cursor:
            continue
        _, end = _balanced(text, match.end() - 1, "(", ")")
        out.append(text[cursor : match.end()])
        out.append(")")
        cursor = end
    out.append(text[cursor:])
    return "".join(out)


def code_blocks(src):
    """`name -> [(body, signature)]` for every free fn and trait `impl`.

    A method defined inside an `impl` is deliberately NOT registered under its
    bare name. Doing so grouped every same-named method in the file together,
    and since traversal follows any identifier that names a block, reaching the
    word `parse` inside `OperableAttr::parse` pulled in `EffectSpec::parse`
    too — handing `#[agent_operable]` the `#[agent_effect]` keys
    (`cross_tenant`, `writes`, `jobs`). Trait impls are reachable by their
    implementing type instead, which is unambiguous.
    """
    impl_spans = []
    for match in IMPL_BLOCK.finditer(src):
        brace = src.find("{", match.start())
        if brace == -1:
            continue
        _, end = _balanced(src, brace)
        impl_spans.append((brace, end))

    def inside_impl(pos):
        return any(start < pos < end for start, end in impl_spans)

    out = collections.defaultdict(list)
    for match in FN_OPEN.finditer(src):
        if inside_impl(match.start()):
            continue
        params, after = _balanced(src, match.end() - 1, "(", ")")
        brace = src.find("{", after)
        semi = src.find(";", after)
        if brace == -1 or (semi != -1 and semi < brace):
            continue  # a trait method declaration, not a definition
        body, _ = _balanced(src, brace)
        out[match.group(1)].append((body, params))
    for match in IMPL_OPEN.finditer(src):
        brace = src.index("{", match.start())
        body, _ = _balanced(src, brace)
        out[match.group(1)].append((body, ""))
    return out


def accepted_keys():
    """Read each macro's accepted argument keys out of its own arg parser.

    Scoped rather than file-wide — see the header. The parser is identified
    structurally: it takes the raw `attr: TokenStream` and, unlike the macro
    entry point, not `item: TokenStream`. Calls are then followed transitively
    within the file so a grammar split across helpers is read whole.
    """
    out = {}
    for macro, owned in OWNERS.items():
        # A macro's grammar may span files: the route verbs dispatch into
        # `route.rs` but parse their keys in the shared `parse.rs`, so reading
        # only the first left `#[get(api_version = …)]` reported as drift.
        filenames = (owned,) if isinstance(owned, str) else owned
        sources = [
            strip_test_mods(
                strip_rust_comments(
                    (MACRO_SRC / f).read_text(encoding="utf-8", errors="replace")
                )
            )
            for f in filenames
            if (MACRO_SRC / f).exists()
        ]
        if not sources:
            out[macro] = set()
            continue
        src = "\n".join(sources)
        blocks = code_blocks(src)
        takes_attr, arg_parsers = [], []
        for name, defs in blocks.items():
            for _, params in defs:
                if not ATTR_PARAM.search(params):
                    continue
                takes_attr.append(name)
                if not ITEM_PARAM.search(params):
                    arg_parsers.append(name)
        # A `syn::Parse` impl is rooted at the implementing TYPE, never at the
        # bare method name: `code_blocks` groups every same-named method
        # together, so rooting at `parse` pulled `EffectSpec::parse` in beside
        # `OperableAttr::parse` and handed `#[agent_operable]` the
        # `#[agent_effect]` keys (`cross_tenant`, `writes`, `jobs`).
        parse_impls = [
            name
            for name, defs in blocks.items()
            for body, _ in defs
            if PARSE_IMPL_BODY.search(body)
        ]
        # When the macro names the type it parses into — `syn::parse2::<T>(attr)`
        # or `attr.parse_args::<T>()` — that is the one, and its siblings are
        # some other attribute's grammar.
        named = [
            t
            for pair in PARSED_INTO.findall(src)
            for t in pair
            if t and t in parse_impls
        ]
        # Prefer the dedicated `fn(attr: TokenStream)` parser. Failing that, the
        # `syn::Parse` impl for the argument type — `api_doc` and
        # `agent_operable` parse that way and have no attr-taking function at
        # all, so without it `api_doc` had no readable grammar (53 guide
        # examples, wholly ungated) and `agent_operable` fell back to its whole
        # macro entry and accepted `cfg`, `fn` and `jobs`. Only if neither
        # exists does the macro entry serve as the root.
        # Additive, not a fallback chain, so that a macro splitting its grammar
        # across both forms keeps all of it. Nothing in the corpus does today
        # (see the header note on `static_get`); the union is kept because
        # dropping a root narrows the accepted set and turns correct pages into
        # reported drift, while an extra root only costs a miss. When the
        # parsed-into type is named explicitly, its siblings are some other
        # attribute's grammar and are left out.
        roots = list(dict.fromkeys(arg_parsers + (named or parse_impls)))
        roots = roots or takes_attr
        seen, queue, scoped = set(), list(roots), []
        while queue:
            name = queue.pop()
            if name in seen:
                continue
            seen.add(name)
            for body, params in blocks.get(name, []):
                # A root is read whatever it takes; a callee is read only if it
                # receives the attribute in some syn form. One taking a bare
                # `&str` is parsing a value, and its literals are values.
                if name not in roots and params and not ARG_PARAM.search(params):
                    continue
                scoped.append(body)
                for ident in set(IDENT.findall(body)):
                    if ident in blocks and ident not in seen:
                        queue.append(ident)
        text = strip_nested_group_parsers("\n".join(scoped))
        keys = set()
        for pattern in KEY_PATTERNS:
            keys |= set(re.findall(pattern, text))
        keys |= match_arm_keys(text)
        keys |= comparison_keys(text)
        # A macro whose parser was FOUND but yields no keys of its own is a
        # marker like `#[public]`: `crate` is then its entire grammar, and it
        # is judgeable on that alone — otherwise `#[public(crtae = "renamed")]`
        # passed. A macro whose parser could not be found at all is a different
        # case and stays skipped: knowing nothing is not the same as knowing
        # there is nothing.
        out[macro] = (keys | UNIVERSAL_KEYS) if roots else set()
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

BLOCKQUOTE = re.compile(r"^[ \t]*(?:>[ \t]?)+")
# A code span, so that markup markers quoted inside one are read as text.
INLINE_CODE = re.compile(r"(`+)(?:(?!\1).)*\1")
WAIVER = re.compile(r"macro-arg-allow:\s*([a-z_0-9]+)\.([A-Za-z_0-9]+)")
# A keyword argument, but never a `==` comparison.
KEYWORD_ARG = re.compile(r"\b([a-z_][a-z_0-9]*)\s*=(?!=)")


# The name to the LEFT of an `=` is a key whatever its case, and no Autumn
# macro has an upper-case one, so `#[secured(Scopes = ["a:b"])]` is a defect
# the lower-case pattern above let through. That pattern cannot simply be
# widened: it also decides whether a bare identifier is a flag, and there
# `#[repository(Post, api, mcp)]`'s leading TYPE is exactly what the
# lower-case restriction excludes. Widening it reports `Post` on a form the
# corpus documents throughout — trading a miss for a false positive on valid
# pages. Only the key position is case-blind; a bare identifier still has to
# look like a flag to be judged as one.
# Rust identifiers are Unicode (XID), not ASCII, so `polícy` is a real key a
# reader can write and the macro really does reject. Python's `\w` is
# Unicode-aware, and `[^\W\d]` is "a word character that is not a digit" — an
# identifier start.
KEY_IDENT = re.compile(r"^[^\W\d]\w*$", re.UNICODE)


def _is_bare_flag(name):
    """Whether `name` reads as a bare flag rather than a positional type.

    The key position is case-blind, but this one cannot be: it is what keeps
    `#[repository(Post, api, mcp)]`'s leading TYPE from being reported. The
    test is on the first character rather than an ASCII range, so it holds for
    a non-ASCII identifier too.
    """
    return bool(KEY_IDENT.match(name)) and not name[0].isupper()


def _plain_ident(name):
    """A raw identifier reduced to the identifier it spells.

    `r#policy` IS `policy` — the prefix only escapes the name from Rust's
    keyword list — so `#[secured(r#policy = "x")]` is the same rejected key.
    I found this gap myself several rounds ago while auditing the lexer and
    declined it as a safe miss, on the grounds that no macro has a
    keyword-colliding key. That was the wrong question: what matters is what a
    reader can write, not what the parsers happen to name.
    """
    return name[2:] if name.startswith("r#") else name

# Macros whose FIRST argument is positional even when written as a bare
# identifier. `parse_authorize_args` takes the leading bare path as the action
# verb (`#[authorize(update, resource = Post)]`), so reading it as a flag
# reported a supported form as drift.
#
# This is an explicit list rather than a structural test, and deliberately so:
# `#[repository(Post, api, mcp)]` also leads with a positional, but there `api`
# and `mcp` ARE flags, and nothing in the parsers distinguishes "first bare
# path is a value" from "bare paths are flags" without reading intent. A short
# list that a self-test pins is honest; a heuristic that guesses would be the
# kind of over-reach that has cost this gate false positives already.
POSITIONAL_FIRST_IDENT = frozenset({"authorize"})


def top_level_keys(args, macro=None):
    """The argument names at the attribute's own nesting level.

    Two shapes count, because the macro rejects a typo in either:

        #[job(queue = "mail")]   a keyword argument
        #[job(unique)]           a bare flag

    Matching only `key =` left `#[job(uniqe)]` and `#[model(managd)]` passing
    clean even though both fail the build.

    What does *not* count is a positional argument — the type in
    `#[repository(Post, …)]`, the role literal in `#[secured("admin")]`, the
    value in `resource = Post`. Literals and nested groups are collapsed to a
    placeholder first, so a segment that held one is never mistaken for a bare
    flag, and a segment carrying `=` yields only its left side.

    A nested group carries its own grammar: `#[get("/about", seo(title = …,
    og_type = …))]` names `title` and `og_type` as keys of `seo(...)`, not of
    `#[get]`. Judging them against the outer macro reported three correct SEO
    pages as drift. Nested grammars are not checked at all, which is the safe
    direction — a miss, not a false alarm.
    """
    buf, depth, i = [], 0, 0
    while i < len(args):
        ch = args[i]
        if ch == "/":
            nxt = skip_comment(args, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == '"':
            i = skip_literal(args, i)
            if depth == 0:
                buf.append("\x00")  # a literal was here
            continue
        if ch == "'":
            # `'a` is a lifetime and consumes nothing; only `'x'` is a literal.
            # Reading `#[listener(Event<'static>, …)]`'s lifetime as an unclosed
            # literal ran to the end of the argument list, so every key after it
            # went unread.
            nxt = _skip_rust_char(args, i)
            if nxt is None:
                i += 1
                continue
            i = nxt
            if depth == 0:
                buf.append("\x00")
            continue
        if ch == "r" and args[i + 1 : i + 2] in ('"', "#"):
            nxt = skip_raw_literal(args, i)
            if nxt is not None:
                i = nxt
                if depth == 0:
                    buf.append("\x00")
                continue
        if ch in "([{":
            depth += 1
            # The group's *interior* is its own grammar, but the identifier
            # introducing it is an argument of this attribute and is left in
            # place to be judged: `#[get("/", soe(title = "T"))]` misspells
            # `seo`, and replacing the whole group — name included — with a
            # placeholder reported zero defects. Same for `dependent`,
            # `retention` and `invalidates`.
        elif ch in ")]}":
            depth -= 1
        elif depth == 0:
            buf.append(ch)
        i += 1

    names = []
    first = True
    for segment in "".join(buf).split(","):
        segment = segment.strip()
        if not segment:
            continue
        if "=" in segment.replace("==", ""):
            head = segment.split("=", 1)[0].strip()
            # A keyword argument does NOT close the positional slot. The slot
            # is consumed by the positional argument itself, not by position in
            # the list: `parse_authorize_args` takes the first bare `Meta::Path`
            # whenever `args.action` is still unset, so
            # `#[authorize(resource = Post, update)]` is as valid as
            # `#[authorize(update, resource = Post)]`. Clearing the flag here
            # reported the second form's action verb as an unknown flag. This
            # also subsumes the narrower `crate = "…"` carve-out that stood
            # here: a universal key was only the most obvious case of a keyword
            # argument that cannot occupy a positional slot, not a special one.
            #
            # Known miss, left deliberately: a leading string literal fills the
            # action slot too (`parse_with_leading_literal`), so a bare path
            # after one — `#[authorize("update", draft)]` — is an error this
            # reads as the action verb. Closing it means giving a positional
            # literal the power to close the slot, and a stray literal anywhere
            # else already makes the attribute unparseable for a reason this
            # gate could not name. A miss is the safe direction; a confidently
            # wrong message is not.
            head = _plain_ident(head)
            if KEY_IDENT.match(head):
                names.append((head, False))
        elif _is_bare_flag(_plain_ident(segment)):
            leading, first = first, False
            if leading and macro in POSITIONAL_FIRST_IDENT:
                continue  # the action verb, not a flag
            names.append((_plain_ident(segment), True))
    return names


# A call site may qualify the macro: `#[autumn_web::repository(...)]` and
# `#[autumn_macros::model(...)]` are documented, idiomatic forms and appear in
# shipped rustdoc (`autumn/src/aggregate.rs`, `autumn/src/classify/mod.rs`).
# Requiring the bare name would leave every qualified invocation ungated.
# Rust permits whitespace between an attribute path and its delimiter, so
# `#[secured (policy = "x")]` is a valid invocation the macro still rejects.
# A comment is a token separator like whitespace, so Rust accepts one anywhere
# whitespace goes: `#[/* a */ secured /* b */ (policy = "x")]` is a real
# invocation. The two gaps inside an attribute head — after `#[` and before the
# delimiter — are therefore walked by `_trivia_end` rather than matched by the
# regex, because a regex separator means `\s*` and `\s*` does not know what a
# comment is. Writing one that did would also have to give up nested block
# comments, which `skip_comment` already handles.
# `#` and `[` are separate tokens, so trivia goes between them too:
# `# /* rationale */ [secured(policy = "x")]` compiles and applies the
# attribute. Verified with rustc rather than assumed. There are three gaps in
# an attribute head, not two, and all three are walked the same way.
ATTR_SIGIL = re.compile(r"#")
# A leading `::` is a valid path root, so `#[::autumn_web::secured(…)]` is the
# same macro as `#[autumn_web::secured(…)]`. It is allowed ONLY in front of a
# known crate prefix, and the crate prefix only immediately in front of a macro
# name. Both restrictions are load-bearing rather than tidiness:
#
#   `#[::secured(…)]`                                    — a crate NAMED
#       `secured`, not this macro.
#   `#[::autumn_web::reexports::axum::routing::get(…)]`  — axum's router
#       macro, re-exported. It is spelled `get`, takes a path literal, and
#       has none of `#[get]`'s keyword grammar. It appears in this repo.
#
# Widening the path to "any segments ending in a known name" would report the
# second as `#[get]` drift, which is a false positive on valid Rust.
# `::` is a token like any other, so trivia goes around it too:
# `#[autumn_web /* rationale */ :: secured(…)]` resolves to the same macro. A
# contiguous regex cannot express that, so the path is walked segment by
# segment with the same trivia walker as the rest of the attribute head.
_CRATE_NAMES = frozenset({"autumn_web", "autumn_macros", "autumn"})
# A path segment may be a raw identifier too. `#[r#secured(…)]` resolves to
# the same attribute, and the previous commit normalized raw identifiers in the
# KEY position while leaving the PATH position alone — the one-half-of-a-pair
# failure, inside the very commit whose message claimed both halves were done.
_SEGMENT = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*")


def _macro_path_at(text, i):
    """`(name, start, end)` for a macro path at `i` past trivia, else None."""
    start = _trivia_end(text, i)
    j, rooted = start, False
    if text[j : j + 2] == "::":
        j, rooted = _trivia_end(text, j + 2), True
    first = _SEGMENT.match(text, j)
    if first is None:
        return None
    after = _trivia_end(text, first.end())
    if text[after : after + 2] == "::":
        # A qualified path: the first segment must be a known crate and the
        # NEXT must be the macro. Anything deeper is a different macro that
        # merely ends in a familiar name — `autumn_web::reexports::axum::
        # routing::get` is axum's router macro, which takes a path literal and
        # has none of `#[get]`'s keyword grammar.
        if _plain_ident(first.group(0)) not in _CRATE_NAMES:
            return None
        k = _trivia_end(text, after + 2)
        name = _SEGMENT.match(text, k)
        if name is None or _plain_ident(name.group(0)) not in OWNERS:
            return None
        return _plain_ident(name.group(0)), start, name.end()
    # A bare name. Rooted, it is a crate NAMED like the macro (`::secured`),
    # not the macro.
    if rooted or _plain_ident(first.group(0)) not in OWNERS:
        return None
    return _plain_ident(first.group(0)), start, first.end()


def _cfg_attr_path_at(text, i):
    """`("cfg_attr", start, end)` when `cfg_attr` sits at `i`, else None."""
    start = _trivia_end(text, i)
    m = _SEGMENT.match(text, start)
    if m is None or m.group(0) != "cfg_attr":
        return None
    return "cfg_attr", start, m.end()


def _trivia_end(text, i):
    """Index past the whitespace and comments starting at `i`."""
    while i < len(text):
        if text[i].isspace():
            i += 1
            continue
        nxt = skip_comment(text, i)
        if nxt is None:
            break
        i = nxt
    return i


def _delimited_at(text, i, path_at):
    """A macro path at `i` and its delimiter. -> (name, start, open, body_start)."""
    found = path_at(text, i)
    if found is None:
        return None
    name, start, end = found
    j = _trivia_end(text, end)
    if j >= len(text) or text[j] not in "([{":
        return None
    return name, start, text[j], j + 1



# `cfg_attr(<pred>, <attr>, …)` applies each `<attr>` when the predicate holds,
# so a conditionally-applied Autumn macro is a real invocation the compiler
# will reject on a typo. Its body is scanned recursively rather than matched
# with a prefix pattern: a predicate can itself carry commas and parentheses
# (`all(feature = "a", feature = "b")`), and more than one attribute can
# follow it (`cfg_attr(feature = "a", inline, secured(…))`), so a `[^,]+`
# prefix stopped at the predicate's first comma and saw no later payload.
# A `cfg_attr` payload is the same path grammar as an attribute head, so it
# uses the same walker rather than a second pattern that could drift from it.


def skip_literal(text, i):
    """Index just past the `"…"` or `'…'` literal opening at `i`.

    A delimiter inside a literal is data, not structure: `#[secured("admin)",
    policy = "x")]` closed the attribute on the `)` inside the role string and
    never reached `policy`. Any argument carrying a route pattern, regex or
    glob has the same shape.
    """
    quote, i = text[i], i + 1
    while i < len(text):
        if text[i] == "\\":
            i += 2
            continue
        if text[i] == quote:
            return i + 1
        i += 1
    return i


def skip_comment(text, i):
    """Index just past a `//` or `/* */` comment at `i`, or None if not one.

    Rust allows a comment inside an attribute's argument list, and a `)` in
    one is not structure: `#[secured(/* ) */ policy = "x")]` ended the scan
    before `policy`. Block comments nest in Rust, so the scan counts them.
    """
    pair = text[i : i + 2]
    if pair == "//":
        end = text.find("\n", i)
        return len(text) if end == -1 else end
    if pair != "/*":
        return None
    depth, j = 1, i + 2
    while j < len(text) and depth:
        if text[j : j + 2] == "/*":
            depth += 1
            j += 2
            continue
        if text[j : j + 2] == "*/":
            depth -= 1
            j += 2
            continue
        j += 1
    return j


def skip_raw_literal(text, i):
    """Index just past a `r"…"` / `r#"…"#` literal at `i`, or None if not one."""
    j = i + 1
    hashes = 0
    while j < len(text) and text[j] == "#":
        hashes += 1
        j += 1
    if j >= len(text) or text[j] != '"':
        return None
    close = '"' + "#" * hashes
    end = text.find(close, j + 1)
    return len(text) if end == -1 else end + len(close)


def _masked_spans(text):
    """Spans of `text` that are string/char literals or comments."""
    spans, i = [], 0
    while i < len(text):
        ch = text[i]
        if ch == "/":
            end = skip_comment(text, i)
            if end is not None:
                spans.append((i, end))
                i = end
                continue
        if ch == "r" and text[i + 1 : i + 2] in ('"', "#"):
            end = skip_raw_literal(text, i)
            if end is not None:
                spans.append((i, end))
                i = end
                continue
        if ch == '"':
            end = skip_literal(text, i)
            spans.append((i, end))
            i = end
            continue
        if ch == "'":
            end = _skip_rust_char(text, i)
            if end is not None:
                spans.append((i, end))
                i = end
                continue
            # a lifetime — consumes nothing
        i += 1
    return spans


CLOSER_OF = {"(": ")", "[": "]", "{": "}"}


def _close_of(text, start, opener="("):
    """Index of the delimiter closing the group opened just before `start`.

    A proc-macro attribute accepts any delimited token tree, so
    `#[secured { policy = "x" }]` and `#[secured [policy = "x"]]` are real
    invocations the macro still rejects; recognising only `(` left both
    entirely ungated.
    """
    closer = CLOSER_OF[opener]
    i, depth = start, 1
    while i < len(text) and depth:
        ch = text[i]
        if ch == "/":
            nxt = skip_comment(text, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == '"':
            i = skip_literal(text, i)
            continue
        if ch == "'":
            # A lifetime is not a literal: `Event<'static>` must consume
            # nothing, or the scan runs to EOF and the attribute is never
            # closed. `_skip_rust_char` already drew this distinction for the
            # source balancer; the documentation scanners needed it too.
            nxt = _skip_rust_char(text, i)
            if nxt is not None:
                i = nxt
            else:
                i += 1
            continue
        if ch == "r" and text[i + 1 : i + 2] in ('"', "#"):
            nxt = skip_raw_literal(text, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == opener:
            depth += 1
        elif ch == closer:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return None


def find_macro_calls(text):
    """Yield `(macro, args, offset)` for each `#[macro(…)]` call in `text`.

    Depth-aware rather than regular, because the arguments are not
    bracket-free: `#[secured(scopes = ["a:b"])]` and
    `#[lifecycle(transitions = [...])]` both carry a nested array, and a
    `[^\\]]*` body stops dead at the first `]`. That made every array-valued
    form invisible to this gate — including `scopes`, the one working spelling
    the baseline defect had to be corrected *to*. Caught by renaming `scopes`
    in `secured.rs` and watching the gate stay silent when it should have
    reported every page still saying `scopes`.

    `text` is a whole fence, not a line, so an attribute spread over several
    lines — the house style for `#[repository(...)]` and `#[lifecycle(...)]`
    once they carry more than one key — is matched like any other.
    """
    masked = _masked_spans(text)

    def in_literal(pos):
        return any(start <= pos < end for start, end in masked)

    out = []
    for attr in ATTR_SIGIL.finditer(text):
        if in_literal(attr.start()):
            # Inside a string or comment this is a *value*, not an
            # invocation: `let shown = r##"#[job(pending = true)]"##;`
            # never applies the attribute, and reporting it would fail the
            # gate on a snippet that quotes a spelling on purpose.
            continue
        bracket = _trivia_end(text, attr.end())
        if bracket >= len(text) or text[bracket] != "[":
            continue
        found = _delimited_at(text, bracket + 1, _macro_path_at)
        if found is not None:
            name, _, opener, body_start = found
            end = _close_of(text, body_start, opener)
            if end is None:
                continue
            out.append((name, text[body_start:end], attr.start()))
            continue
        found = _delimited_at(text, bracket + 1, _cfg_attr_path_at)
        if found is None or found[2] != "(":
            continue
        body_start = found[3]
        end = _close_of(text, body_start, "(")
        if end is None:
            continue
        # A `cfg_attr` body applies each of its attributes, so each is a
        # real invocation — but only the attributes are. Searching the
        # whole body for macro openers also found them inside an
        # attribute's *value*: `doc = stringify!(secured(policy = "x"))`
        # passes `secured(…)` to a macro as tokens and applies nothing, and
        # reporting it forced a waiver onto valid Rust. The body is now
        # read as what the grammar says it is — a predicate followed by
        # attribute meta items.
        out.extend(_cfg_attr_payloads(text[body_start:end], body_start))
    return out


def _cfg_attr_payloads(body, base):
    """Attributes a `cfg_attr` body applies, as (macro, args, offset)."""
    out = []
    items = _split_top_level(body)
    # The first item is the predicate (`all(feature = "a", …)`), never an
    # applied attribute. Everything after it is one attribute each.
    for start, end in items[1:]:
        item = body[start:end]
        # An item may lead with a comment — `cfg_attr(feature = "a",
        # /* apply auth */ secured(…))`. `_split_top_level` already steps over
        # comments when balancing commas, but it leaves them in the item, so an
        # anchored match had to know about them too.
        found = _delimited_at(item, 0, _macro_path_at)
        if found is None:
            # Either a bare marker attribute (`inline`), or a name-value one
            # (`doc = "…"`). Neither carries an Autumn argument list. A macro
            # name appearing later inside such an item is part of a value.
            nested = _delimited_at(item, 0, _cfg_attr_path_at)
            if nested is not None and nested[2] == "(":
                inner_start = nested[3]
                inner_end = _close_of(item, inner_start, "(")
                if inner_end is not None:
                    out.extend(
                        _cfg_attr_payloads(
                            item[inner_start:inner_end],
                            base + start + inner_start,
                        )
                    )
            continue
        name, name_start, opener, inner_start = found
        inner_end = _close_of(item, inner_start, opener)
        if inner_end is None:
            continue
        out.append(
            (
                name,
                item[inner_start:inner_end],
                base + start + name_start,
            )
        )
    return out


def _split_top_level(text):
    """Spans of `text`'s comma-separated items, ignoring nested and quoted ones."""
    spans, depth, start, i = [], 0, 0, 0
    while i < len(text):
        ch = text[i]
        if ch == "/":
            nxt = skip_comment(text, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == "r" and text[i + 1 : i + 2] in ('"', "#"):
            nxt = skip_raw_literal(text, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == '"':
            i = skip_literal(text, i)
            continue
        if ch == "'":
            nxt = _skip_rust_char(text, i)
            i = nxt if nxt is not None else i + 1
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == "," and depth == 0:
            spans.append((start, i))
            start = i + 1
        i += 1
    spans.append((start, len(text)))
    return [(s, e) for s, e in spans if text[s:e].strip()]


def is_archived(rel):
    s = str(rel)
    return s.startswith(ARCHIVE_PREFIXES) or s in ARCHIVE_FILES


def markdown_files():
    # `*.md.tmpl` too: `new.rs` writes `templates/README.md.tmpl` as every
    # scaffolded application's README, so a fence there reaches a reader by a
    # route other than opening a page. Three sibling gates already define the
    # corpus that way — `check-docs-config.sh`, `-routes.sh` and `-symbols.sh`
    # each carry the same note — and this one did not, which made it the odd
    # gate out on a surface the others consider reader-facing.
    out = []
    paths = sorted(set(ROOT.rglob("*.md")) | set(ROOT.rglob("*.md.tmpl")))
    for path in paths:
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


def judge_fences(rel, fence_lines, accepted, judgeable, waived):
    """Judge one fence's keyword arguments.

    `fence_lines` is the fence body as `(lineno, text)` pairs. They are joined
    and scanned as one string rather than line by line: an attribute spread
    over several lines is a single call, and matching per line skipped every
    one of them — including the multiline `#[repository(...)]` and
    `#[lifecycle(...)]` blocks in shipped rustdoc. Offsets map back to the
    original line so a report still points at the attribute.
    """
    if not fence_lines:
        return []
    text = "\n".join(t for _, t in fence_lines)
    starts, pos = [], 0
    for lineno, chunk in fence_lines:
        starts.append((pos, lineno))
        pos += len(chunk) + 1

    def line_of(offset):
        found = fence_lines[0][0]
        for start, lineno in starts:
            if start <= offset:
                found = lineno
            else:
                break
        return found

    out = []
    for macro, args, offset in find_macro_calls(text):
        if macro not in judgeable:
            continue
        for key, is_flag in top_level_keys(args, macro):
            if key in accepted[macro] or (macro, key) in waived:
                continue
            out.append((macro, key, f"{rel}:{line_of(offset)}", is_flag))
    return out


def collect_waivers(lines):
    """`(macro, key) -> [waiver line numbers]`.

    Positions are kept, not flattened to a set. A waiver is documented as
    sitting *beside the passage*, and a file-global one does not behave that
    way: one legitimate `secured.policy` waiver near the top of a long guide
    would silently accept every later `#[secured(policy = …)]` on the page,
    including an unrelated typo. `waiver_covers` below turns a position into
    the single fence it introduces.
    """
    waived = collections.defaultdict(list)
    for lineno, line in enumerate(lines, 1):
        for macro, key in WAIVER.findall(line):
            waived[(macro, key)].append(lineno)
    return waived


def assign_waivers(waived, fences):
    """Bind each waiver marker to exactly ONE fence: the one it introduces.

    Applying a proximity window independently to every fence let a single
    marker waive several: two consecutive one-line fences both sit inside the
    six-line reach of a marker written for the first, so both invalid examples
    passed. A marker waives the fence it is inside, otherwise the *next* fence
    to open, and then it is spent.

    `fences` is a list of `(start, end)` line pairs in document order. Returns
    a list of `set[(macro, key)]`, one per fence.
    """
    per_fence = [set() for _ in fences]
    for (macro, key), linenos in waived.items():
        for lineno in linenos:
            for index, (start, end) in enumerate(fences):
                inside = start <= lineno <= end
                introduces = start - WAIVER_REACH <= lineno < start
                if inside or introduces:
                    per_fence[index].add((macro, key))
                    break  # spent on this fence, and no other
    return per_fence


# How far above a fence a waiver may sit and still be "beside" it: enough for a
# marker plus the blank line and a wrapped comment, not enough to reach the
# previous passage.
WAIVER_REACH = 6


# CommonMark allows a fence to be indented up to three spaces. At four it is
# an indented code block instead, and its contents are literal text — so a page
# DISPLAYING a fenced example, indented, has no fence in it at all. Stripping
# indentation without limit turned such a display into a live fence and failed
# the gate on a document containing no Rust.
FENCE_INDENT_MAX = 3


def _indent_width(line):
    """Columns of leading whitespace, a tab advancing to the next 4-column stop.

    CommonMark measures indentation in columns, not characters, so a single
    leading tab is already past the fence allowance. Counting characters made
    `\t> ```rust` — an indented display of a quoted fence — read as a live
    fence once the quote prefix was stripped.
    """
    width = 0
    for ch in line:
        if ch == " ":
            width += 1
        elif ch == "\t":
            width += 4 - (width % 4)
        else:
            break
    return width


def _fence_body(line, base=0, in_fence=False):
    """A line with its block-quote prefix removed, unless it is over-indented.

    The quote marker is itself subject to the indentation rule: at four spaces
    `    > ```rust` is an indented code block DISPLAYING a quoted fence, not a
    quote containing one. Stripping the prefix first also stripped the
    indentation that says so, and the display was judged as live Rust.

    A list marker on the same line is removed as well: `- ```rust` opens a
    fence in the item it introduces. The previous round recorded the content
    column such a marker establishes but still handed the marker itself to the
    fence test, which then saw `-` where the delimiter should be.
    """
    # Only when no fence is open. Inside one, a line beginning `- ` is CODE —
    # stripping the marker there turned `- ``` ` inside a raw string into a
    # closing delimiter and truncated the block. The list tracking already
    # refused to run inside a fence for the same reason; this half did not.
    marker = None if in_fence else LIST_MARKER.match(line)
    if marker is not None:
        col = _list_content_column(line, marker)
        # Only the padding actually consumed is dropped; any beyond it stays as
        # indentation, which is what makes an over-padded marker introduce an
        # indented code block rather than a fence.
        used = col - _indent_width(marker.group(1)) - len(marker.group(2))
        line = " " * col + line[len(marker.group(1)) + len(marker.group(2)) + used :]
    if _indent_width(line) - base > FENCE_INDENT_MAX:
        return line
    return BLOCKQUOTE.sub("", line)


# A list item establishes a content column, and CommonMark measures a fence
# from THERE, not from the left margin: under `10. Step:` the content column is
# four, so a four-space-indented fence is a fence. An absolute limit read that
# as an indented code block and went silent. The corpus already nests 222 fence
# lines in lists at columns two and three; a list numbered past nine is the
# shape that pushes one to four.
LIST_MARKER = re.compile(r"^([ \t]*)([-*+]|\d{1,9}[.)])([ \t]+)")


def _list_content_column(line, marker=None):
    """The content column a list marker on `line` opens, or None.

    `marker` lets a caller that has already matched pass its match in. The
    corpus is 1.4M lines and this pattern was being run three times over each
    of them, which measurably slowed the whole gate.
    """
    m = marker if marker is not None else LIST_MARKER.match(line)
    if m is None:
        return None
    # CommonMark counts only 1-4 spaces after the marker as padding. At five or
    # more, one space is padding and the REST is indentation, so `-     ```rust`
    # opens an indented code block displaying the delimiter rather than a fence.
    # Adding the whole run made that display live Rust and failed the gate on it.
    pad = _indent_width(m.group(3))
    return _indent_width(m.group(1)) + len(m.group(2)) + (1 if pad >= 5 else pad)


def _run_at(text, i):
    """Index past the backtick run starting at `i`."""
    j = i
    while j < len(text) and text[j] == "`":
        j += 1
    return j


def _blank_code_spans(text):
    """`text` with the contents of matched code spans replaced by spaces.

    A span opens on a backtick run and closes on a run of the SAME length. An
    unmatched run is ordinary text — the first version opened a span on any
    run and carried it forward, so a stray backtick in prose masked a real
    `<!--` and the hidden fence under it was scanned and reported.

    Newlines survive, so a caller can split the result back into lines. Spans
    cannot cross a blank line, so callers resolve one block at a time.
    """
    out, i, n = list(text), 0, len(text)
    while i < n:
        if text[i] != "`":
            i += 1
            continue
        j = _run_at(text, i)
        run, k, closed = j - i, j, None
        while k < n:
            if text[k] != "`":
                k += 1
                continue
            m = _run_at(text, k)
            if m - k == run:
                closed = m
                break
            k = m
        if closed is None:
            i = j  # an unmatched run: ordinary text
            continue
        for p in range(i, closed):
            if out[p] != "\n":
                out[p] = " "
        i = closed
    return "".join(out)


def _code_span_masked(lines):
    """`lines` with matched code-span contents blanked, block by block."""
    out, block = list(lines), []

    def flush():
        if not block:
            return
        masked = _blank_code_spans("\n".join(lines[i] for i in block))
        for i, seg in zip(block, masked.split("\n")):
            out[i] = seg
        block.clear()

    for i, line in enumerate(lines):
        if line.strip():
            block.append(i)
        else:
            flush()
    flush()
    return out


def _html_comment_step(text, inside, fence_open, base, markup=None):
    """`(skip this line, still inside)` for HTML-comment tracking.

    A fence inside an HTML comment is not rendered, so a reader can neither see
    nor copy it — in a page or in a doc comment, where `rustdoc --test`
    likewise reports no tests for one. This lived in the markdown scanner
    alone, the seventh time a rule was taught to one half of the pair, so it is
    a shared step rather than a second copy.

    Three things it must not do, each learned the hard way: it does not track
    inside a fence, where `<!--` is code; it ignores a marker inside an inline
    code span, which is text ABOUT a marker; and it ignores an over-indented
    one, which is an indented code block displaying it. The last two matter
    because an opener with no closer swallows everything after it.
    """
    if inside:
        return True, "-->" not in text
    if markup is None:
        markup = text
    displayed = _indent_width(text) - base > FENCE_INDENT_MAX
    if (
        not fence_open
        and not displayed
        and "<!--" in markup
        and "-->" not in markup.split("<!--", 1)[1]
    ):
        return True, True
    return False, False


def _fence_lang(suffix):
    """The info string's first token, lower-cased.

    An info string is arbitrary text, so `rust` has to match as a whole token
    rather than a prefix: ```rustic is not Rust, and judging it as Rust failed
    the gate on a fence that is not even claiming to be an Autumn example. The
    corpus writes `rust`, `rust,ignore` and `rust,no_run`, all of which this
    reads as `rust`.
    """
    return re.split(r"[,\s]", suffix.strip().lower())[0]


def _fence_at(body, base=0):
    """`(char, length, suffix)` when `body` is a fence line, else None.

    `body` has had any block-quote prefix removed but keeps its indentation,
    which is what decides whether it is a fence at all. `base` is the content
    column of any enclosing list item; the allowance is measured from there.
    """
    if _indent_width(body) - base > FENCE_INDENT_MAX:
        return None
    stripped = body.lstrip(" \t")
    for char in ("`", "~"):
        if stripped.startswith(char * 3):
            length = len(stripped) - len(stripped.lstrip(char))
            suffix = stripped[length:]
            # A backtick fence's info string may not contain a backtick, so
            # ```rust `example` is not a fence at all. Returning one made the
            # prose under it read as Rust. (A tilde fence has no such rule.)
            if char == "`" and "`" in suffix:
                return None
            return char, length, suffix
    return None


def _judge_collected(rel, collected, accepted, judgeable, waived):
    """Judge every collected fence, each with only the waivers bound to it."""
    spans = [(block[0][0], block[-1][0]) for block in collected if block]
    bound = assign_waivers(waived, spans)
    found, index = [], 0
    for block in collected:
        if not block:
            continue
        found.extend(
            judge_fences(rel, block, accepted, judgeable, bound[index])
        )
        index += 1
    return found


def scan_markdown(path, accepted, judgeable, _calls=None):
    """Yield (macro, key, line, is_flag) for arguments inside fenced Rust."""
    rel = path.relative_to(ROOT)
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    waived = collect_waivers(lines)
    inside, fences, collected, current = False, 0, [], []
    open_char, open_len, list_col, fence_quoted = None, 0, 0, False
    in_html_comment = False
    # Code spans are resolved for the whole file up front rather than carried
    # line by line: only a run with a matching close is a span, and that cannot
    # be known until the close is found.
    masked = _code_span_masked(lines)
    for lineno, line in enumerate(lines, 1):
        # A fence inside an HTML comment is not rendered, so a reader cannot
        # see or copy it — and the gate was failing CI on a block someone had
        # deliberately commented OUT. Only tracked outside a fence, where
        # `<!--` is markup rather than code. Waivers are unaffected: they are
        # collected from the raw lines, not from this scan.
        skip, in_html_comment = _html_comment_step(
            line, in_html_comment, open_char is not None, list_col, masked[lineno - 1]
        )
        if skip:
            continue
        if open_char is None:
            # Track the innermost list item's content column, but only outside
            # a fence — inside one, a line beginning `- ` is code, not a list.
            # The column is given up at the left margin, where a paragraph ends
            # the list. Conservative on purpose: guessing a LARGER allowance
            # than the document really has is how an indented display becomes a
            # live fence again, which is the false positive this rule exists to
            # avoid.
            col = _list_content_column(line)
            if col is not None:
                list_col = col
            elif line.strip() and _indent_width(line) == 0:
                list_col = 0
        # A fenced example inside a block quote is still an example a reader
        # copies. `docs/guide/mcp.md` and `docs/guide/openapi.md` both carry
        # one, and without stripping the CommonMark `>` prefix the scanner
        # never entered the fence at all.
        # A fence opened inside a block quote belongs to that quote, so a line
        # that is not part of the quote ends BOTH. Removing the prefix without
        # tracking membership left the fence open across the break and judged
        # the prose after it as Rust. `rustdoc --test` shows the truth plainly:
        # the quoted fences collect as two EMPTY doctests, so the unprefixed
        # line between them was never code.
        if open_char is not None and fence_quoted and not BLOCKQUOTE.match(line):
            if inside:
                collected.append(current)
                current = []
            inside, open_char, open_len = False, None, 0
        body = _fence_body(line, list_col, open_char is not None)
        fence = _fence_at(body, list_col)
        handled = False
        if fence:
            char, length, suffix = fence
            if open_char is None:
                # Every fence is tracked, not only Rust ones. A ````text block
                # displaying a literal ```rust example is documentation ABOUT
                # a fence; treating the inner delimiter as live structure made
                # the displayed attribute fail the gate.
                open_char, open_len = char, length
                fence_quoted = BLOCKQUOTE.match(line) is not None
                inside = _fence_lang(suffix) == "rust"
                if inside:
                    fences += 1
                handled = True
            # CommonMark: a fence closes only on the same character, at least
            # as long as the opener, AND followed by nothing but whitespace.
            # Removing a fixed three characters read ````rust as the language
            # "`rust"; closing on any same-length run truncated a fence at a
            # ```not-a-close line inside a raw string, so every attribute after
            # it went unread.
            elif char == open_char and length >= open_len and not suffix.strip():
                if inside:
                    collected.append(current)
                    current = []
                inside = False
                open_char, open_len = None, 0
                handled = True
        if handled:
            continue
        # Not a fence line, or a fence-shaped line that does not close this
        # fence — either way it is content when one is open.
        if inside:
            current.append((lineno, body))
    collected.append(current)
    return _judge_collected(rel, collected, accepted, judgeable, waived), fences


DOC_ATTR = re.compile(r"^\s*#!?\[\s*doc\s*=\s*")
# `#[cfg_attr(doc, doc = "…")]` is documentation too: rustdoc builds with
# `cfg(doc)` on, so it renders and TESTS the fence inside. The predicate is not
# inspected — whatever it is, if the attribute can supply docs under some
# configuration, a reader can reach that page.
CFG_ATTR_DOC = re.compile(r"^\s*#!?\[\s*cfg_attr\s*\(")
DOC_ITEM = re.compile(r"^\s*doc\s*=\s*")


def _doc_attr_text(line):
    """The markdown in a `#[doc = "…"]` attribute, or None.

    rustdoc renders and TESTS these exactly as it does `///`, so a fence in one
    reaches docs.rs like any other. Only the literal forms are read — a
    `#[doc = include_str!(…)]` names a file this scanner would have to resolve,
    and that file is markdown the corpus walk already covers on its own.
    """
    m = DOC_ATTR.match(line)
    if m is None:
        return None
    rest = line[m.end() :]
    if rest[:1] == "r":
        opened = re.match(r'r(#*)"', rest)
        if opened is None:
            return None
        closer = '"' + "#" * len(opened.group(1))
        end = rest.find(closer, opened.end())
        # A raw literal that does not close on this line is a doc attribute
        # spanning several source lines. Reading a partial one would invent
        # markdown that is not there, so it is left as a documented miss —
        # this scanner is line-based, and a wrong answer is worse than none.
        if end == -1:
            return None
        return rest[opened.end() : end]
    if rest[:1] != '"':
        return None
    end = skip_literal(rest, 0)
    return _decode_rust_string(rest[1 : end - 1])


def _doc_attr_at(lines, idx):
    """`(markdown, lines consumed)` for a doc attribute at `lines[idx]`.

    A doc attribute may span source lines — the raw form
    `#[doc = r#"…"#]` is the natural way to write a multi-line one, and an
    ordinary string literal may carry newlines too. Reading only the first line
    left the rest to be scanned as Rust source, so such an attribute went
    unread. Lines are joined until the literal closes.
    """
    conditional = DOC_ATTR.match(lines[idx]) is None
    if conditional and CFG_ATTR_DOC.match(lines[idx]) is None:
        return None, 1
    joined = lines[idx]
    for end in range(idx, min(idx + _DOC_ATTR_MAX_LINES, len(lines))):
        if end > idx:
            joined += "\n" + lines[end]
        text = (
            _cfg_attr_doc_text(joined) if conditional else _doc_attr_text(joined)
        )
        if text is not None:
            return text, end - idx + 1
    return None, 1


def _cfg_attr_doc_text(text):
    """The markdown a `#[cfg_attr(…, doc = "…")]` supplies, or None.

    One attribute may carry several `doc =` items; they are joined in order,
    exactly as rustdoc concatenates them.
    """
    m = CFG_ATTR_DOC.match(text)
    if m is None:
        return None
    end = _close_of(text, m.end(), "(")
    if end is None:
        return None
    docs = []
    for start, stop in _split_top_level(text[m.end() : end]):
        item = text[m.end() :][start:stop]
        if DOC_ITEM.match(item) is None:
            continue
        got = _doc_attr_text("#[" + item.strip())
        if got is None:
            return None
        docs.append(got)
    return "\n".join(docs) if docs else None


# An unterminated literal must not make the reader walk the whole file for
# every `#[doc` it sees; past this it is left unread, which is a miss.
_DOC_ATTR_MAX_LINES = 400

_SIMPLE_ESCAPES = {
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "\\": "\\",
    "0": "\0",
    "'": "'",
    '"': '"',
}


def _decode_rust_string(raw):
    """A string literal's body with escapes decoded, or None if it is not valid.

    Every escape Rust accepts is decoded, and anything else is not valid Rust,
    so the answer is None rather than a guess. The first version of this passed
    unknown escapes through with the backslash dropped, which silently turned
    `\\x60` into the three characters `x60` — inventing text instead of reading
    it, and hiding a fence written that way. Same failure as the half-read raw
    literal: a wrong answer is worse than none.
    """
    out, i = [], 0
    while i < len(raw):
        if raw[i] != "\\":
            out.append(raw[i])
            i += 1
            continue
        nxt = raw[i + 1 : i + 2]
        # A backslash before a newline is a line continuation: the newline and
        # the indentation after it are removed. Rejecting it as unknown threw
        # away the whole attribute, which is how a multi-line doc attribute
        # written in the ordinary (non-raw) form went unread.
        # `nxt in "\r\n"` would be True for the empty string — a lone backslash
        # at the end of an unterminated literal — and consuming it made the
        # line-joiner stop one line early on the very form this decodes.
        if nxt in ("\r", "\n"):
            i += 2
            while i < len(raw) and raw[i] in " \t\r\n":
                i += 1
            continue
        if nxt in _SIMPLE_ESCAPES:
            out.append(_SIMPLE_ESCAPES[nxt])
            i += 2
            continue
        if nxt == "x":
            digits = raw[i + 2 : i + 4]
            if len(digits) != 2:
                return None
            try:
                out.append(chr(int(digits, 16)))
            except ValueError:
                return None
            i += 4
            continue
        if nxt == "u" and raw[i + 2 : i + 3] == "{":
            close = raw.find("}", i + 3)
            if close == -1:
                return None
            try:
                out.append(chr(int(raw[i + 3 : close], 16)))
            except ValueError:
                return None
            i = close + 1
            continue
        return None
    return "".join(out)


def _after_attribute(line):
    """Whatever follows a leading `#[…]` on `line`, or the line if it has none."""
    open_at = line.find("[")
    if open_at == -1:
        return ""
    # `_close_of` returns the index OF the closing delimiter, since its callers
    # slice the body with it — so the tail starts one past that.
    end = _close_of(line, open_at + 1, "[")
    return "" if end is None else line[end + 1 :]


def _bracket_delta(text):
    """Net bracket depth `text` adds, skipping literals and comments."""
    depth, i = 0, 0
    while i < len(text):
        ch = text[i]
        if ch == "/":
            nxt = skip_comment(text, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == "r" and text[i + 1 : i + 2] in ('"', "#"):
            nxt = skip_raw_literal(text, i)
            if nxt is not None:
                i = nxt
                continue
        if ch == '"':
            i = skip_literal(text, i)
            continue
        if ch == "'":
            nxt = _skip_rust_char(text, i)
            i = nxt if nxt is not None else i + 1
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        i += 1
    return depth


def _block_doc_split(text, depth):
    """`(doc text on this line, depth after it)` inside a block doc comment.

    Rust block comments NEST, so a `/* … */` written as prose inside a `/** …
    */` does not end the doc. Ending at the first `*/` dropped everything after
    such an aside. `skip_comment` has counted nested comments since the source
    balancer was written; this is the same rule, line by line.
    """
    out, i = [], 0
    while i < len(text):
        if text[i : i + 2] == "/*":
            depth += 1
            out.append(text[i : i + 2])
            i += 2
            continue
        if text[i : i + 2] == "*/":
            depth -= 1
            if depth == 0:
                return "".join(out), 0
            out.append(text[i : i + 2])
            i += 2
            continue
        out.append(text[i])
        i += 1
    return "".join(out), depth


def scan_rustdoc(path, accepted, judgeable, _calls=None):
    """Same, over ```-fenced Rust inside `//!` and `///` doc comments."""
    rel = path.relative_to(ROOT)
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    waived = collect_waivers(lines)
    inside, fences, collected, current = False, 0, [], []
    open_char, open_len, list_col, fence_quoted = None, 0, 0, False
    in_block_doc, attr_depth, comment_depth = 0, 0, 0
    in_html_comment = False
    # A `#[doc = "…"]` attribute is expanded into the line-doc form it is
    # equivalent to, so every rule below — fences, block quotes, indentation,
    # list columns — applies to it without a second implementation. The source
    # line number is kept, so a defect still points at the attribute.
    stream, idx = [], 0
    while idx < len(lines):
        text, used = _doc_attr_at(lines, idx)
        if text is None:
            stream.append((idx + 1, lines[idx]))
            idx += 1
            continue
        for md in text.split("\n"):
            stream.append((idx + 1, "/// " + md))
        idx += used
    # Code spans are resolved over the stream for the same reason as in the
    # page scanner. Last commit wired this into the page half and left the doc
    # half reading raw text — inside the very change whose message said the
    # step was shared. The step was; its INPUT was not.
    stream_masked = _code_span_masked([text for _, text in stream])
    for pos, (lineno, line) in enumerate(stream):
        # `/** … */` and `/*! … */` are doc comments too, and `rustdoc --test`
        # collects and runs their fences exactly as it does `///`'s. Only the
        # line forms were recognised, so a whole legitimate doc form was
        # ungated. Entered only outside a fence, where `/**` is markup rather
        # than code.
        if in_block_doc:
            text, in_block_doc = _block_doc_split(line, in_block_doc)
            doc_text = re.sub(r"^\s*\*\s?", "", text)
        # An open fence does NOT disqualify a block doc comment. The guard that
        # said so was added to stop `/**` inside a fence being read as markup,
        # but a bare `/**` beginning a source line cannot be fence content: the
        # fence's content arrives through `///` prefixes, and an item between
        # two doc lines is E0753, so nothing else can sit there. `rustdoc
        # --test` includes the middle line of `/// ```` / `/** let x = 41; */`
        # / `/// assert_eq!(x + 1, 42);` and the doctest passes, which is how
        # this was settled.
        # `/***` is an ordinary block comment, not a doc comment — `rustdoc
        # --test` reports zero tests for a fence inside one, where `/**`
        # reports one. Classifying it as doc failed the gate on a commented-out
        # or displayed example. `/**/` is ordinary for the same reason.
        elif re.match(r"^\s*/\*(?:\*(?!\*)|!)", line) and not re.match(
            r"^\s*/\*\*/", line
        ):
            body_after = re.sub(r"^\s*/\*[*!]\s?", "", line)
            doc_text, in_block_doc = _block_doc_split(body_after, 1)
        else:
            doc_text = None
        if doc_text is not None:
            doc = re.match(r"(.*)", doc_text, re.S)
        else:
            # `////` is an ordinary comment, not a doc comment — rustdoc drops
            # it while concatenating the `///` lines around it. Matching it as
            # doc content injected a line into the fenced code that the
            # doctest never sees, which failed the gate on documentation
            # rustdoc itself compiles clean. Verified by putting invalid Rust
            # on a `////` line and watching `rustdoc --test` still pass.
            doc = re.match(r"^\s*(?://!|///(?!/))\s?(.*)$", line)
        if not doc:
            # A blank line or an ordinary comment between two doc lines does
            # not break the doc comment: rustdoc concatenates the attributes
            # either way, and `rustdoc --test` runs a fence that spans the gap
            # as one block. Resetting here meant the closing delimiter after
            # such a gap was read as a fresh opener and the code between went
            # unread. Verified against rustdoc rather than assumed, including
            # the negative — an item between two doc lines is an error
            # (E0753), so any other non-doc line still ends the fence, and an
            # unterminated one still cannot swallow the rest of the file.
            #
            # An outer attribute is the same case and for the same reason: it
            # is not an item, so `/// ```rust`, `#[allow(dead_code)]`,
            # `/// …` all still attach to the one item below, and
            # `rustdoc --test` collects and runs the fence across it.
            # An attribute can span lines, and only its first one starts with
            # `#`. Its continuation lines were reaching the reset below and
            # ending the fence — the previous commit handled the one-line form
            # and stopped there.
            bare = line.strip()
            if attr_depth > 0:
                attr_depth = max(0, attr_depth + _bracket_delta(line))
                continue
            # An ordinary block comment is a gap like a line comment or a blank
            # line: rustdoc concatenates the doc attributes on either side of
            # it. Only the line forms were exempt, so `/* note */` between two
            # `///` lines ended the fence. It may span lines, so the depth is
            # carried — the same nesting rule as the block doc form.
            if comment_depth > 0:
                _, comment_depth = _block_doc_split(line, comment_depth)
                continue
            if bare.startswith("/*"):
                _, comment_depth = _block_doc_split(line[line.index("/*") + 2 :], 1)
                continue
            if not bare or bare.startswith("//"):
                continue
            if bare.startswith("#"):
                depth = _bracket_delta(line)
                if depth > 0:
                    attr_depth = depth
                    continue
                # Balanced on this line. If an ITEM shares the line —
                # `#[allow(dead_code)] pub fn a() {}` — the doc comment ends
                # here after all: the fence belongs to that item, and later
                # `///` lines document the next one. Treating the whole line as
                # an attribute-only gap merged two items' documentation.
                tail = _after_attribute(line)
                if not tail.strip() or tail.strip().startswith("//"):
                    continue
            if inside:
                collected.append(current)
            current, inside = [], False
            open_char, open_len = None, 0
            continue
        # A block quote inside a doc comment is a fence like any other, and the
        # markdown half has stripped the CommonMark `>` prefix since the guide
        # pages that carry one were found. This half did not, so `/// > ```rust`
        # never opened a fence and everything in it went unread. Third time a
        # rule taught to one scanner had to be taught to the other; they now
        # share every one of them.
        # Indentation is kept, not stripped: the same CommonMark rules decide a
        # fence here as in markdown, and both the four-space limit and the
        # closing-suffix check need it.
        # A doc comment is markdown, so a list in one establishes a content
        # column exactly as it does in a page. Same tracking, same scanner.
        # A doc comment is markdown, so an HTML comment hides a fence here too —
        # `rustdoc --test` reports no tests for one. Same step as the page
        # scanner rather than a second copy of it.
        skip, in_html_comment = _html_comment_step(
            doc.group(1),
            in_html_comment,
            open_char is not None,
            list_col,
            stream_masked[pos],
        )
        if skip:
            continue
        if open_char is None:
            col = _list_content_column(doc.group(1))
            if col is not None:
                list_col = col
            elif doc.group(1).strip() and _indent_width(doc.group(1)) == 0:
                list_col = 0
        # Same rule as the page scanner: an unquoted line ends a quoted fence.
        if (
            open_char is not None
            and fence_quoted
            and not BLOCKQUOTE.match(doc.group(1))
        ):
            if inside:
                collected.append(current)
                current = []
            inside, open_char, open_len = False, None, 0
        body = _fence_body(doc.group(1), list_col, open_char is not None)
        fence = _fence_at(body, list_col)
        handled = False
        if fence:
            char, length, suffix = fence
            if open_char is None:
                # rustdoc fences default to Rust, and the attribute-bearing
                # ones are usually `ignore` / `no_run` / `compile_fail`. A
                # `text` fence is still tracked so a Rust fence displayed
                # inside it is not read as live structure. The token has to
                # match whole here too, or ```rustic reads as Rust.
                token = _fence_lang(suffix)
                open_char, open_len = char, length
                fence_quoted = BLOCKQUOTE.match(doc.group(1)) is not None
                # Enumerated against `rustdoc --test` rather than recalled, with
                # a bogus attribute as the control: `standalone_crate` and
                # `ignore-<reason>` both collect a doctest, `custom` does not.
                # The `-reason` form was a miss I noted several rounds ago and
                # left, on the same "nobody writes that" reasoning that was
                # wrong about raw identifiers.
                inside = token == "" or re.fullmatch(
                    r"rust|ignore(?:-[\w-]+)?|no_run|compile_fail|should_panic"
                    r"|edition\d+|standalone_crate",
                    token,
                ) is not None
                if inside:
                    fences += 1
                handled = True
            elif char == open_char and length >= open_len and not suffix.strip():
                if inside:
                    collected.append(current)
                    current = []
                inside = False
                open_char, open_len = None, 0
                handled = True
        if handled:
            continue
        if inside:
            current.append((lineno, body.strip()))
    collected.append(current)
    return _judge_collected(rel, collected, accepted, judgeable, waived), fences


def run_scan():
    accepted = accepted_keys()
    judgeable = {m for m, keys in accepted.items() if keys}
    defects = []
    md_files = markdown_files()
    rs_files = rustdoc_files()
    md_fences = rs_fences = 0
    for path in md_files:
        found, fences = scan_markdown(path, accepted, judgeable)
        defects.extend(found)
        md_fences += fences
    for path in rs_files:
        found, fences = scan_rustdoc(path, accepted, judgeable)
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
    for macro, key, loc, is_flag in defects:
        grouped[(macro, key, is_flag)].append(loc)
    for (macro, key, is_flag), locs in sorted(grouped.items()):
        known = ", ".join(sorted(stats["accepted"][macro])) or "(none)"
        shown = key if is_flag else f"{key} = …"
        print(f"\n  #[{macro}({shown})] — {macro} has no `{key}` key", file=sys.stderr)
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
    passed = failed = 0

    def check(name, got, want):
        nonlocal passed, failed
        if got == want:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL {name}: got {got!r}, want {want!r}", file=sys.stderr)

    import tempfile

    def scan_text_full(text, suffix):
        with tempfile.NamedTemporaryFile(
            "w", suffix=suffix, dir=ROOT, delete=False, encoding="utf-8"
        ) as fh:
            fh.write(text)
            tmp = pathlib.Path(fh.name)
        try:
            scanner = scan_markdown if suffix == ".md" else scan_rustdoc
            found, _ = scanner(tmp, accepted, judgeable)
            return found
        finally:
            tmp.unlink()

    def scan_text(text, suffix):
        return [(m, k) for m, k, _, _ in scan_text_full(text, suffix)]

    def scan_text_lines(text, suffix):
        return [loc.rsplit(":", 1)[1] for _, _, loc, _ in scan_text_full(text, suffix)]

    # The truth set is read from the macro sources, not a snapshot.
    check("secured accepts scopes", "scopes" in accepted["secured"], True)
    check("secured rejects policy", "policy" in accepted["secured"], False)
    check("agent_operable accepts grant", "grant" in accepted["agent_operable"], True)
    check("step_up accepts max_age", "max_age" in accepted["step_up"], True)
    check("cached accepts ttl", "ttl" in accepted["cached"], True)
    check("model accepts table", "table" in accepted["model"], True)
    # Not every macro is judgeable, and that is the safe direction: a scope
    # that yields no keys means this gate cannot read that grammar, so it says
    # nothing rather than reporting every key the macro's pages use. The floor
    # guards against a refactor quietly emptying the truth set wholesale — the
    # failure mode where a gate keeps passing because it stopped looking.
    check("most macros are judgeable", len(judgeable) >= 28, True)
    check(
        "skipped macros are named",
        sorted(set(OWNERS) - judgeable),
        ["mailer_preview", "service", "sim_test"],
    )
    # The route verbs parse their keys in `parse.rs`, not the file they
    # dispatch from. Reading only one file reported `api_version` as drift.
    check("get accepts api_version (cross-file grammar)", "api_version" in accepted["get"], True)
    check("static_get accepts params", "params" in accepted["static_get"], True)
    # A nested group's keys belong to the group, not the outer macro.
    check(
        "markdown: nested group keys are not judged against the outer macro",
        scan_text(
            '```rust\n#[get("/about", seo(title = "T", og_type = "website"))]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: a bad top-level key beside a nested group is still caught",
        scan_text(
            '```rust\n#[get("/a", seo(title = "T"), bogus = 1)]\n```\n', ".md"
        ),
        [("get", "bogus")],
    )

    # `crate = "…"` is stripped by `crate_path::extract_crate_override` before
    # any macro's own parser runs, so it appears in no owner file while being
    # valid on all 33.
    check("crate is universal", "crate" in accepted["get"], True)
    check(
        "markdown: crate override is not drift",
        scan_text('```rust\n#[get("/x", crate = "autumn_web_05")]\n```\n', ".md"),
        [],
    )

    # Match arms carry real keys in a key dispatch and values in a value
    # dispatch; the scrutinee is what separates them.
    check("authorize accepts resource (key match arm)", "resource" in accepted["authorize"], True)
    check("authorize accepts from (key match arm)", "from" in accepted["authorize"], True)
    check(
        "repository rejects a value match arm",
        "delete_all" in accepted["repository"],
        False,
    )
    check(
        "markdown: a root-parser value arm is not an accepted key",
        scan_text('```rust\n#[repository(Post, delete_all = true)]\n```\n', ".md"),
        [("repository", "delete_all")],
    )

    # Bare flags are arguments too, and a typo in one fails the build just the
    # same. Positional arguments are not.
    check(
        "markdown: a misspelled bare flag is caught",
        scan_text("```rust\n#[job(uniqe)]\n```\n", ".md"),
        [("job", "uniqe")],
    )
    check(
        "markdown: a correct bare flag passes",
        scan_text('```rust\n#[job(unique, queue = "mail")]\n```\n', ".md"),
        [],
    )
    check(
        "markdown: bare flags alongside a positional type pass",
        scan_text("```rust\n#[repository(Post, api, mcp, soft_delete)]\n```\n", ".md"),
        [],
    )
    check(
        "markdown: a positional literal is not read as a flag",
        scan_text('```rust\n#[secured("admin")]\n```\n', ".md"),
        [],
    )
    check(
        "markdown: a value after = is not read as a flag",
        scan_text(
            "```rust\n#[authorize(\"update\", resource = Post, from = post)]\n```\n", ".md"
        ),
        [],
    )
    # `crate = "…"` is stripped by `extract_crate_override` before the macro's
    # own parser sees the tokens, so it is legal in any position and does not
    # occupy the positional slot. Counting it as the first argument promoted
    # the action verb to a bare flag and reported this correct form as drift.
    check(
        "markdown: a crate override before a positional action passes",
        scan_text(
            '```rust\n#[authorize(crate = "autumn_web_05", update, resource = Post)]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: a bad flag after a crate override is still caught",
        scan_text(
            '```rust\n#[job(crate = "autumn_web_05", uniqe)]\n```\n',
            ".md",
        ),
        [("job", "uniqe")],
    )
    # The positional slot is closed by the positional argument, never by
    # position in the list. `parse_authorize_args` takes the first bare
    # `Meta::Path` whenever `args.action` is unset, so a keyword argument may
    # precede the action verb.
    check(
        "markdown: the authorize action may follow a keyword argument",
        scan_text("```rust\n#[authorize(resource = Post, update)]\n```\n", ".md"),
        [],
    )
    check(
        "markdown: a bad key alongside a trailing action is still caught",
        scan_text(
            "```rust\n#[authorize(resource = Post, update, bogus = 1)]\n```\n", ".md"
        ),
        [("authorize", "bogus")],
    )
    check(
        "markdown: only the first bare identifier takes the action slot",
        scan_text("```rust\n#[authorize(resource = Post, update, draft)]\n```\n", ".md"),
        [("authorize", "draft")],
    )

    # A macro parsing through a `syn::Parse` impl has no `fn(attr: TokenStream)`
    # at all. Rooting at the whole macro entry instead made `agent_operable`
    # accept `cfg`/`fn`/`jobs`; finding nothing left `api_doc` unjudged across
    # 53 guide examples.
    check("api_doc is judgeable", "api_doc" in judgeable, True)
    check("api_doc accepts summary", "summary" in accepted["api_doc"], True)
    check("api_doc accepts operation_id", "operation_id" in accepted["api_doc"], True)
    check("agent_operable accepts grant", "grant" in accepted["agent_operable"], True)
    check("agent_operable rejects an implementation term", "cfg" in accepted["agent_operable"], False)
    check(
        "markdown: an api_doc typo is caught",
        scan_text('```rust\n#[api_doc(summry = "typo")]\n```\n', ".md"),
        [("api_doc", "summry")],
    )
    check(
        "markdown: real api_doc keys pass",
        scan_text(
            '```rust\n#[api_doc(summary = "S", description = "D", status = 200)]\n```\n',
            ".md",
        ),
        [],
    )

    # A nested group's keys belong to the group on the truth-set side too.
    check("repository rejects a nested-group key", "action" in accepted["repository"], False)
    check("repository still accepts a real key", "table" in accepted["repository"], True)
    check(
        "markdown: a nested-only key is not a top-level key",
        scan_text("```rust\n#[repository(Post, action = true)]\n```\n", ".md"),
        [("repository", "action")],
    )
    check(
        "markdown: the nested group itself still passes",
        scan_text(
            '```rust\n#[repository(Post, dependent(Comment, fk = "c", on_delete = destroy))]\n```\n',
            ".md",
        ),
        [],
    )

    # A delimiter inside a comment is not structure either.
    check(
        "markdown: paren inside a block comment does not end the attribute",
        scan_text('```rust\n#[secured(/* ) */ policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: paren inside a line comment does not end the attribute",
        scan_text(
            '```rust\n#[secured(\n    // )\n    policy = "x",\n)]\n```\n', ".md"
        ),
        [("secured", "policy")],
    )

    # A `syn::Parse` root is the implementing TYPE. Rooting at the bare method
    # name grouped every `parse` in the file together, so `OperableAttr::parse`
    # dragged in `EffectSpec::parse` and `#[agent_operable]` inherited the
    # `#[agent_effect]` keys.
    check("agent_operable accepts only its own key", sorted(accepted["agent_operable"]), ["crate", "grant"])
    check(
        "markdown: a sibling attribute's key is not accepted",
        scan_text("```rust\n#[agent_operable(cross_tenant)]\n```\n", ".md"),
        [("agent_operable", "cross_tenant")],
    )
    # `static_get` is NOT a route alias. It reads like one — same `#[get]`
    # shape, same file neighbourhood — and I registered the shared route
    # parser for it on that resemblance. `StaticGetAttrs::parse` takes
    # `params`, `revalidate` and `seo`, its `other =>` arm rejects the rest,
    # and `static_get_macro` never hands `attr` to `route::route_macro`. The
    # shared parser blessed `api_version`, `timeout_ms` and `name` here, so a
    # doc page writing one of those would have passed the gate.
    check(
        "static_get accepts exactly its own grammar",
        sorted(accepted["static_get"]),
        ["crate", "params", "revalidate", "seo"],
    )
    check(
        "markdown: a route-only key on static_get is caught",
        scan_text('```rust\n#[static_get("/", api_version = "v1")]\n```\n', ".md"),
        [("static_get", "api_version")],
    )

    # A marker macro's whole grammar is the universal key, and it is judgeable
    # on that alone.
    check("public is judgeable", "public" in judgeable, True)
    check(
        "markdown: a typo'd crate override on a marker macro is caught",
        scan_text('```rust\n#[public(crtae = "renamed")]\n```\n', ".md"),
        [("public", "crtae")],
    )
    check(
        "markdown: a bare marker macro passes",
        scan_text("```rust\n#[public]\nfn f() {}\n```\n", ".md"),
        [],
    )

    # The identifier introducing a nested group is an argument of this
    # attribute, even though the group's interior is not.
    check(
        "markdown: a misspelled group name is caught",
        scan_text('```rust\n#[get("/", soe(title = "T"))]\n```\n', ".md"),
        [("get", "soe")],
    )
    check(
        "markdown: a correct group name passes",
        scan_text('```rust\n#[get("/", seo(title = "T"))]\n```\n', ".md"),
        [],
    )
    check(
        "markdown: repository group names pass",
        scan_text(
            '```rust\n#[repository(Post, dependent(Comment, fk = "c"), retention(after = "30d"))]\n```\n',
            ".md",
        ),
        [],
    )

    # A fenced example inside a block quote is still an example.
    check(
        "markdown: fence inside a block quote is scanned",
        scan_text('> ```rust\n> #[secured(policy = "x")]\n> ```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: nested block quote is scanned",
        scan_text('> > ```rust\n> > #[secured(policy = "x")]\n> > ```\n', ".md"),
        [("secured", "policy")],
    )
    # …and the same in rustdoc, which learned this rule three findings after
    # the markdown half did.
    check(
        "rustdoc: fence inside a block quote is scanned",
        scan_text('//! > ```rust\n//! > #[secured(policy = "x")]\n//! > ```\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: nested block quote is scanned",
        scan_text(
            '/// > > ```ignore\n/// > > #[secured(policy = "x")]\n/// > > ```\n', ".rs"
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a quoted non-Rust fence is still not judged",
        scan_text('//! > ```text\n//! > #[secured(policy = "x")]\n//! > ```\n', ".rs"),
        [],
    )
    # CommonMark: a fence may be indented up to three spaces. At four it is an
    # indented code block, so a page DISPLAYING a fenced example has no fence.
    check(
        "markdown: a four-space indented display is not a fence",
        scan_text('    ```rust\n    #[secured(policy = "x")]\n    ```\n', ".md"),
        [],
    )
    check(
        "markdown: a three-space indented fence is still scanned",
        scan_text('   ```rust\n   #[secured(policy = "x")]\n   ```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a four-space indented display is not a fence",
        scan_text(
            '//!     ```rust\n//!     #[secured(policy = "x")]\n//!     ```\n', ".rs"
        ),
        [],
    )
    # The quote marker is itself subject to the indentation rule, so the
    # prefix cannot be stripped before the rule is applied.
    check(
        "markdown: an indented display of a quoted fence is not a fence",
        scan_text(
            '    > ```rust\n    > #[secured(policy = "x")]\n    > ```\n', ".md"
        ),
        [],
    )
    check(
        "markdown: a quoted fence within the allowance is still scanned",
        scan_text('  > ```rust\n  > #[secured(policy = "x")]\n  > ```\n', ".md"),
        [("secured", "policy")],
    )
    # Indentation is measured in columns, so one tab is already past the
    # allowance. Counting characters let a tab-indented display through.
    check(
        "markdown: a tab-indented display is not a fence",
        scan_text('\t```rust\n\t#[secured(policy = "x")]\n\t```\n', ".md"),
        [],
    )
    check(
        "markdown: a tab before a quote marker is still over-indented",
        scan_text('\t> ```rust\n\t> #[secured(policy = "x")]\n\t> ```\n', ".md"),
        [],
    )
    check(
        "markdown: tab-indented content inside a real fence is still scanned",
        scan_text('```rust\n\t#[secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    # A list item establishes a content column and the allowance is measured
    # from there, so a list numbered past nine can carry a four-column fence.
    check(
        "markdown: a fence under a two-digit list item is a fence",
        scan_text('10. Step:\n\n    ```rust\n    #[secured(policy = "x")]\n    ```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a fence under a bullet is a fence",
        scan_text('- Step:\n\n  ```rust\n  #[secured(policy = "x")]\n  ```\n', ".md"),
        [("secured", "policy")],
    )
    # …and the guard that keeps the earlier false positive fixed: the column is
    # given up when a paragraph ends the list, so a later indented display does
    # not inherit the allowance.
    check(
        "markdown: an indented display after the list ends is not a fence",
        scan_text(
            '1. Step:\n\n   ```rust\n   #[secured(policy = "x")]\n   ```\n\n'
            'A paragraph ends the list.\n\n'
            '    ```rust\n    #[secured(policy = "y")]\n    ```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # rustdoc: an outer attribute is not an item, so it does not break the doc
    # comment. Verified with `rustdoc --test`.
    check(
        "rustdoc: a fence survives an outer attribute",
        scan_text(
            '/// ```\n#[allow(dead_code)]\n/// #[secured(policy = "x")]\n/// ```\n'
            'pub fn f() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # A fence inside an HTML comment is not rendered, so a reader can neither
    # see nor copy it.
    check(
        "markdown: a fence inside an HTML comment is not scanned",
        scan_text('<!--\n```rust\n#[secured(policy = "x")]\n```\n-->\n', ".md"),
        [],
    )
    check(
        "markdown: a fence after a one-line HTML comment is still scanned",
        scan_text(
            '<!-- note -->\n\n```rust\n#[secured(policy = "x")]\n```\n', ".md"
        ),
        [("secured", "policy")],
    )
    # `r#policy` IS `policy`; the prefix only escapes the keyword list.
    check(
        "markdown: a raw identifier key is caught",
        scan_text('```rust\n#[secured(r#policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a raw identifier naming a real flag passes",
        scan_text('```rust\n#[job(r#unique, queue = "mail")]\n```\n', ".md"),
        [],
    )
    # `/** … */` and `/*! … */` are doc comments too, and rustdoc tests them.
    check(
        "rustdoc: a block doc comment is scanned",
        scan_text('/** doc\n```\n#[secured(policy = "x")]\n```\n*/\npub fn f() {}\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an inner block doc comment with star decoration is scanned",
        scan_text('/*!\n * ```\n * #[secured(policy = "x")]\n * ```\n */\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an ordinary block comment is not a doc comment",
        scan_text('/* note\n```\n#[secured(policy = "x")]\n```\n*/\npub fn g() {}\n', ".rs"),
        [],
    )
    check(
        "rustdoc: an empty block comment is not a doc comment",
        scan_text('/**/\n```\n#[secured(policy = "x")]\n```\n', ".rs"),
        [],
    )
    # Rust block comments nest, so a `/* … */` aside written as prose inside a
    # `/** … */` does not end the doc.
    check(
        "rustdoc: a nested comment does not end a block doc",
        scan_text(
            '/** doc with /* aside */ prose\n```\n#[secured(policy = "x")]\n```\n*/\n'
            'pub fn f() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # A raw identifier in the PATH is the same macro, exactly as in the key.
    check(
        "markdown: a raw identifier macro path is caught",
        scan_text('```rust\n#[r#secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a raw identifier crate prefix is caught",
        scan_text('```rust\n#[r#autumn_web::r#secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    # `<!--` inside an inline code span is text about a marker, not a marker.
    check(
        "markdown: a quoted comment opener does not open a comment",
        scan_text(
            'The opener `<!--` starts one.\n\n```rust\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # `////` is an ordinary comment. rustdoc drops it while concatenating the
    # `///` lines around it, so its contents are not part of the doctest.
    check(
        "rustdoc: a four-slash comment is not doc content",
        scan_text(
            '//! ```\n//// #[secured(policy = "x")]\n//! let x = 1;\n//! ```\n', ".rs"
        ),
        [],
    )
    check(
        "rustdoc: three slashes are still doc content",
        scan_text('//! ```\n/// #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )
    # An attribute can span lines, and only its first starts with `#`.
    check(
        "rustdoc: a fence survives a multi-line outer attribute",
        scan_text(
            '/// ```\n#[cfg(\n    feature = "x"\n)]\n/// #[secured(policy = "x")]\n'
            '/// ```\npub fn g() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # `#[doc = "…"]` is rendered and TESTED by rustdoc exactly as `///` is.
    check(
        "rustdoc: a doc attribute is scanned",
        scan_text(
            '#[doc = "```\\n#[secured(policy = \\"x\\")]\\n```"]\npub fn f() {}\n', ".rs"
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a raw doc attribute is scanned",
        scan_text(
            '#[doc = r"```"]\n#[doc = r#"#[secured(policy = "x")]"#]\n'
            '#[doc = r"```"]\npub fn h() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an unclosed raw doc attribute invents nothing",
        scan_text('#[doc = r"```\nunclosed\n"]\npub fn i() {}\n', ".rs"),
        [],
    )
    # Every escape Rust accepts is decoded. Passing an unknown one through with
    # the backslash dropped turned `\x60` into `x60` and hid the fence.
    check(
        "rustdoc: a hex-escaped doc attribute fence is scanned",
        scan_text(
            '#[doc = "\\x60\\x60\\x60\\n#[secured(policy = \\"x\\")]\\n\\x60\\x60\\x60"]\n'
            'pub fn f() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a unicode-escaped doc attribute fence is scanned",
        scan_text(
            '#[doc = "\\u{60}\\u{60}\\u{60}\\n#[secured(policy = \\"x\\")]\\n'
            '\\u{60}\\u{60}\\u{60}"]\npub fn f() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an invalid escape invents nothing",
        scan_text('#[doc = "bad \\q escape"]\npub fn h() {}\n', ".rs"),
        [],
    )
    # Doc forms mix freely on one item: an open fence does not disqualify a
    # block doc comment, and an ordinary block comment is a gap like any other.
    check(
        "rustdoc: a block doc comment continues an open fence",
        scan_text('/// ```\n/** #[secured(policy = "x")] */\n/// ```\npub fn f() {}\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a fence survives an ordinary block comment",
        scan_text(
            '/// ```\n/* note */\n/// #[secured(policy = "x")]\n/// ```\n'
            'pub fn g() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a fence survives a multi-line ordinary block comment",
        scan_text(
            '/// ```\n/* multi\n   line */\n/// #[secured(policy = "x")]\n/// ```\n'
            'pub fn h() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # A fence may begin on the marker's own line.
    check(
        "markdown: a fence on the list marker's line is scanned",
        scan_text('- ```rust\n  #[secured(policy = "x")]\n  ```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: an ordered marker's line too",
        scan_text('1. ```rust\n   #[secured(policy = "x")]\n   ```\n', ".md"),
        [("secured", "policy")],
    )
    # An over-indented `<!--` is a DISPLAY of the marker, so it opens nothing —
    # otherwise an unclosed one swallows every later fence.
    check(
        "markdown: an indented comment marker does not swallow later fences",
        scan_text(
            'shown\n\n    <!-- opener with no closer\n\n```rust\n'
            '#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # `/***` is an ordinary block comment: rustdoc reports zero tests for a
    # fence inside one, where `/**` reports one.
    check(
        "rustdoc: a triple-star comment is not a doc comment",
        scan_text('/***\n```\n#[secured(policy = "x")]\n```\n*/\npub fn f() {}\n', ".rs"),
        [],
    )
    check(
        "rustdoc: a double-star comment still is",
        scan_text('/**\n```\n#[secured(policy = "x")]\n```\n*/\npub fn f() {}\n', ".rs"),
        [("secured", "policy")],
    )
    # A doc attribute may span source lines; the raw form is how a multi-line
    # one is normally written.
    check(
        "rustdoc: a multi-line raw doc attribute is scanned",
        scan_text(
            '#[doc = r#"\n```ignore\n#[secured(policy = "x")]\n```\n"#]\n'
            'pub fn g() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # An HTML comment hides a fence in a doc comment exactly as it does in a
    # page — `rustdoc --test` reports no tests for one.
    check(
        "rustdoc: a fence inside an HTML comment is not scanned",
        scan_text(
            '//! <!--\n//! ```\n//! #[secured(policy = "x")]\n//! ```\n//! -->\n', ".rs"
        ),
        [],
    )
    check(
        "rustdoc: a fence after a one-line HTML comment is still scanned",
        scan_text(
            '//! <!-- hidden -->\n//! ```\n//! #[secured(policy = "x")]\n//! ```\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # A backslash before a newline is a line continuation, not an unknown
    # escape: the newline and the indentation after it are removed.
    check(
        "rustdoc: an escaped-newline doc attribute is scanned",
        scan_text(
            '#[doc = "```\\n\\\n#[secured(policy = \\"x\\")]\\n\\\n```"]\n'
            'pub fn f() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # Only 1-4 spaces after a list marker are padding. At five, one is padding
    # and the rest is indentation, so the fence is a display.
    check(
        "markdown: an over-padded list marker introduces a display",
        scan_text('-     ```rust\n      #[secured(policy = "x")]\n      ```\n', ".md"),
        [],
    )
    check(
        "markdown: four spaces of padding is still a fence",
        scan_text('-    ```rust\n     #[secured(policy = "x")]\n     ```\n', ".md"),
        [("secured", "policy")],
    )
    # A code span may cross a newline, so the marker inside one is still text.
    check(
        "markdown: a multi-line code span does not open a comment",
        scan_text(
            'A marker `start\n<!--\nend` then\n\n```rust\n'
            '#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # Enumerated against `rustdoc --test`: these collect a doctest, `custom`
    # does not.
    check(
        "rustdoc: a standalone_crate fence is Rust",
        scan_text('//! ```standalone_crate\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an ignore fence with a reason is Rust",
        scan_text('//! ```ignore-wasm\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an unknown fence attribute is not Rust",
        scan_text('//! ```custom\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [],
    )
    # An unmatched backtick is ordinary text, so it must not mask a real
    # comment opener and expose the hidden fence beneath it.
    check(
        "markdown: a stray backtick does not mask a comment opener",
        scan_text(
            'Prose with a stray ` here\n<!--\n```rust\n'
            '#[secured(policy = "x")]\n```\n-->\n',
            ".md",
        ),
        [],
    )
    # A backtick fence's info string may not contain a backtick.
    check(
        "markdown: backticks in the info string mean it is not a fence",
        scan_text('```rust `example`\n#[secured(policy = "x")]\n```\n', ".md"),
        [],
    )
    # Inside a fence a line beginning `- ` is code, not a list item.
    check(
        "markdown: a list-shaped line inside a fence is content",
        scan_text(
            '```rust\nlet s = r#"\n- ```\n"#;\n#[secured(policy = "x")]\n```\n', ".md"
        ),
        [("secured", "policy")],
    )
    # An attribute sharing its line with an ITEM ends the doc comment.
    check(
        "rustdoc: an item sharing the attribute's line ends the fence",
        scan_text(
            '/// ```\n#[allow(dead_code)] pub fn a() {}\n'
            '/// #[secured(policy = "x")]\npub fn b() {}\n',
            ".rs",
        ),
        [],
    )
    # A fence opened inside a block quote ends where the quote does.
    check(
        "rustdoc: an unquoted line ends a quoted fence",
        scan_text(
            '/// > ```\n/// #[secured(policy = "x")]\n/// > ```\npub fn f() {}\n', ".rs"
        ),
        [],
    )
    check(
        "rustdoc: a fully quoted fence is still scanned",
        scan_text(
            '/// > ```\n/// > #[secured(policy = "x")]\n/// > ```\npub fn f() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: an unquoted line ends a quoted fence",
        scan_text('> ```rust\n#[secured(policy = "x")]\n> ```\n', ".md"),
        [],
    )
    # The code-span mask reaches the doc stream, not only the page scanner.
    check(
        "rustdoc: a quoted comment opener does not open a comment",
        scan_text(
            '//! The marker `<!--` starts one.\n//! ```\n'
            '//! #[secured(policy = "x")]\n//! ```\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # Rust identifiers are Unicode, so a non-ASCII key is still a key.
    check(
        "markdown: a non-ASCII key is caught",
        scan_text('```rust\n#[secured(polícy = "x")]\n```\n', ".md"),
        [("secured", "polícy")],
    )
    check(
        "markdown: a positional type is still not a flag",
        scan_text("```rust\n#[repository(Post, api, mcp, soft_delete)]\n```\n", ".md"),
        [],
    )
    # `#[cfg_attr(doc, doc = "…")]` supplies documentation rustdoc renders and
    # tests, so a fence in one is reader-facing.
    check(
        "rustdoc: a conditional doc attribute is scanned",
        scan_text(
            '#[cfg_attr(doc, doc = "```\\n#[secured(policy = \\"x\\")]\\n```")]\n'
            'pub fn g() {}\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    # An info string is arbitrary text, so `rust` matches as a whole token.
    check(
        "markdown: a language merely starting with rust is not Rust",
        scan_text('```rustic\n#[secured(policy = "x")]\n```\n', ".md"),
        [],
    )
    check(
        "markdown: rust,ignore is Rust",
        scan_text('```rust,ignore\n#[secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: rust,no_run is Rust",
        scan_text('```rust,no_run\n#[secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a language merely starting with rust is not Rust",
        scan_text('//! ```rustic\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [],
    )
    check(
        "rustdoc: a bare ignore fence is still Rust",
        scan_text('//! ```ignore\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )
    # A closing fence may be followed only by whitespace. Closing on any
    # same-length run truncated the block at a `​```not-a-close` line inside a
    # raw string, so every attribute after it went unread.
    check(
        "markdown: a same-length run with a suffix does not close",
        scan_text(
            '```rust\nlet s = r#"\n```not-a-close\n"#;\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: a closing fence may carry trailing whitespace",
        scan_text('```rust\n#[secured(policy = "x")]\n```   \n', ".md"),
        [("secured", "policy")],
    )
    # rustdoc concatenates doc attributes across a blank line or an ordinary
    # comment, and runs a fence spanning the gap as one block. Any other
    # non-doc line still ends the fence.
    check(
        "rustdoc: a fence survives a blank source line",
        scan_text('//! ```\n\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a fence survives an ordinary comment line",
        scan_text(
            '//! ```\n// note\n//! #[secured(policy = "x")]\n//! ```\n', ".rs"
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: an item still ends an unterminated fence",
        scan_text('//! ```\npub fn f() {}\n#[secured(policy = "x")]\n', ".rs"),
        [],
    )
    # `#` and `[` are separate tokens, so trivia goes between them too.
    check(
        "markdown: trivia between the attribute sigil tokens",
        scan_text('```rust\n# /* why */ [secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a lone hash is not an attribute",
        scan_text('```rust\nlet n = 1; // # secured(policy = "x")\n```\n', ".md"),
        [],
    )
    # A leading `::` is a valid path root, but only in front of a known crate
    # prefix — and the prefix only immediately in front of a macro name. The
    # three negatives below are why: each is valid Rust that is NOT this macro.
    check(
        "markdown: a root-qualified path is the same macro",
        scan_text('```rust\n#[::autumn_web::secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a crate merely NAMED like a macro is not it",
        scan_text('```rust\n#[::secured(policy = "x")]\n```\n', ".md"),
        [],
    )
    check(
        "markdown: axum's re-exported get is not autumn's get",
        scan_text(
            '```rust\n#[::autumn_web::reexports::axum::routing::get("/x")]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: an unknown crate path is not assumed to be autumn's",
        scan_text('```rust\n#[some_other::secured(policy = "x")]\n```\n', ".md"),
        [],
    )
    check(
        "markdown: a root-qualified cfg_attr payload is reached",
        scan_text(
            '```rust\n#[cfg_attr(feature = "a", ::autumn_web::secured(policy = "x"))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # `::` is a token, so trivia goes around it as well. The negatives below
    # matter more than the positives: the same trivia must not turn a deeper
    # path into a match.
    check(
        "markdown: a comment around the path separator",
        scan_text(
            '```rust\n#[autumn_web /* why */ :: secured(policy = "x")]\n```\n', ".md"
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: whitespace around the path separator",
        scan_text('```rust\n#[ :: autumn_web :: secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a path separator across a line break",
        scan_text(
            '```rust\n#[autumn_web\n    :: secured(policy = "x")]\n```\n', ".md"
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: trivia does not make a deeper path match",
        scan_text(
            '```rust\n#[autumn_web /* x */ :: reexports :: axum :: routing :: get("/x")]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: trivia does not make a rooted bare path match",
        scan_text('```rust\n#[ :: secured(policy = "x")]\n```\n', ".md"),
        [],
    )
    # A markdown TEMPLATE is documentation a reader will hold: `new.rs` writes
    # `templates/README.md.tmpl` as every scaffolded application's README.
    with tempfile.NamedTemporaryFile(
        "w", suffix=".md.tmpl", dir=ROOT, delete=False, encoding="utf-8"
    ) as fh:
        fh.write("# generated\n")
        tmpl = pathlib.Path(fh.name)
    try:
        check("corpus: a markdown template is in the corpus", tmpl in set(markdown_files()), True)
    finally:
        tmpl.unlink()

    # `x == "…"` names a key only when `x` is one. `window != "pending"` and
    # `basis == "deleted_at"` compare values.
    check("job rejects a compared value", "pending" in accepted["job"], False)
    check("repository rejects a compared value", "deleted_at" in accepted["repository"], False)
    check("agent_operable keeps its compared key", "grant" in accepted["agent_operable"], True)
    check(
        "markdown: a compared value is not an accepted key",
        scan_text("```rust\n#[job(pending = true)]\n```\n", ".md"),
        [("job", "pending")],
    )
    check(
        "markdown: real job keys still pass",
        scan_text('```rust\n#[job(queue = "mail", max_attempts = 3)]\n```\n', ".md"),
        [],
    )

    # A waiver is spent on the one fence it introduces.
    check(
        "markdown: waiver does not carry to the next fence",
        scan_text(
            "<!-- macro-arg-allow: secured.policy -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n'
            '```rust\n#[secured(policy = "typo")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: waiver still covers its own fence",
        scan_text(
            "<!-- macro-arg-allow: secured.policy -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [],
    )
    # A macro that forwards its arguments to another macro's entry accepts
    # that macro's grammar, so its owners must include the target's files.
    # `oauth2_callback` forwards to `route::route_macro` and `edge::edge_macro`;
    # reading only its own file left it with `crate` alone and reported the
    # valid `#[oauth2_callback("/cb", timeout_ms = …)]` as drift. This is the
    # hazard behind treating a keyless macro as a marker: it may be a
    # forwarder instead, so the registry is checked rather than assumed.
    # Forwarding means the ARGUMENTS are passed on, so the call must carry
    # `attr`. A bare mention of another macro's entry — guard detection,
    # a doc comment — is not forwarding.
    forward_call = re.compile(
        r"\b([a-z_0-9]+)::([a-z_0-9]+)_macro\s*\([^)]*\battr\b"
    )
    for macro, owned in OWNERS.items():
        names = (owned,) if isinstance(owned, str) else owned
        have = set(names)
        text = "\n".join(
            (MACRO_SRC / f).read_text(encoding="utf-8", errors="replace")
            for f in names
            if (MACRO_SRC / f).exists()
        )
        for module, _ in set(forward_call.findall(text)):
            target = f"{module}.rs"
            if (MACRO_SRC / target).exists() and target not in have:
                check(f"{macro} covers forwarded {target}", target, "(registered)")
    check("oauth2_callback accepts route keys", "timeout_ms" in accepted["oauth2_callback"], True)
    check(
        "markdown: a forwarded route key is not drift",
        scan_text(
            '```rust\n#[oauth2_callback("/cb", timeout_ms = 1000)]\n```\n', ".md"
        ),
        [],
    )

    # A conditionally-applied macro is a real invocation.
    check(
        "markdown: cfg_attr payload is inspected",
        scan_text(
            '```rust\n#[cfg_attr(feature = "auth", secured(policy = "x"))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: a correct cfg_attr payload passes",
        scan_text(
            '```rust\n#[cfg_attr(feature = "auth", secured(scopes = ["a:b"]))]\n```\n',
            ".md",
        ),
        [],
    )
    # A predicate can carry its own commas and parens, and more than one
    # attribute can follow it.
    check(
        "markdown: cfg_attr with a compound predicate",
        scan_text(
            '```rust\n#[cfg_attr(all(feature = "a", feature = "b"), secured(policy = "x"))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: cfg_attr with a later payload",
        scan_text(
            '```rust\n#[cfg_attr(feature = "a", inline, secured(policy = "y"))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: compound predicate with a correct payload passes",
        scan_text(
            '```rust\n#[cfg_attr(all(feature = "a", feature = "b"), secured(scopes = ["a:b"]))]\n```\n',
            ".md",
        ),
        [],
    )

    # Body extraction must not count delimiters inside Rust literals or read
    # code out of doc comments.
    check("get rejects a sibling attribute's name", "intercept" in accepted["get"], False)
    check("get keeps its own keys", "api_version" in accepted["get"], True)
    check(
        "markdown: a sibling attribute name is not a route key",
        scan_text('```rust\n#[get("/", intercept = MyLayer)]\n```\n', ".md"),
        [("get", "intercept")],
    )
    check("oauth2_callback excludes the edge grammar", "needs" in accepted["oauth2_callback"], False)
    check("oauth2_callback keeps route keys", "timeout_ms" in accepted["oauth2_callback"], True)

    # A fence closes only on the same character, at least as long as the opener.
    check(
        "markdown: four-backtick fence is scanned",
        scan_text('````rust\n#[secured(policy = "x")]\n````\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a shorter run does not close a longer fence",
        scan_text(
            '````rust\n```\n#[secured(policy = "x")]\n```\n````\n', ".md"
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: tilde fence is scanned",
        scan_text('~~~rust\n#[secured(policy = "x")]\n~~~\n', ".md"),
        [("secured", "policy")],
    )

    # `#[ws]` is not a route alias: it takes a path literal and explicitly
    # rejects the route macros' `seo(...)`.
    check("ws rejects route keys", "timeout_ms" in accepted["ws"], False)
    check(
        "markdown: a route key on ws is caught",
        scan_text('```rust\n#[ws("/live", timeout_ms = 1000)]\n```\n', ".md"),
        [("ws", "timeout_ms")],
    )
    check(
        "markdown: a bare ws path passes",
        scan_text('```rust\n#[ws("/live")]\n```\n', ".md"),
        [],
    )
    # …while the real route aliases keep theirs.
    check("get keeps timeout_ms", "timeout_ms" in accepted["get"], True)
    check("static_get keeps params", "params" in accepted["static_get"], True)

    # An attribute quoted inside a literal or a comment is a value, not an
    # invocation — reporting it would fail the gate on a snippet that shows a
    # spelling on purpose.
    check(
        "markdown: attribute inside a raw string is not an invocation",
        scan_text(
            '```rust\nlet shown = r##"#[job(pending = true)]"##;\n```\n', ".md"
        ),
        [],
    )
    check(
        "markdown: attribute inside a string is not an invocation",
        scan_text(
            '```rust\nlet s = "#[secured(policy = \\"x\\")]";\n```\n', ".md"
        ),
        [],
    )
    check(
        "markdown: attribute inside a comment is not an invocation",
        scan_text("```rust\n// #[model(bogus = 1)]\n```\n", ".md"),
        [],
    )
    check(
        "markdown: a real attribute beside a quoted one is still caught",
        scan_text(
            '```rust\nlet s = "#[job(pending = true)]";\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # …and the same masking inside a `cfg_attr` body, which is a separate scan.
    check(
        "markdown: attribute inside a cfg_attr doc string is not an invocation",
        scan_text(
            '```rust\n#[cfg_attr(feature = "docs", doc = "#[secured(policy = 1)]")]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: a real cfg_attr payload beside a quoted one is caught",
        scan_text(
            '```rust\n#[cfg_attr(feature = "d", doc = "#[job(pending = 1)]", secured(policy = "x"))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )

    # Rust allows whitespace between an attribute path and its delimiter.
    check(
        "markdown: whitespace before the delimiter is still an invocation",
        scan_text('```rust\n#[secured (policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: whitespace inside a cfg_attr payload too",
        scan_text(
            '```rust\n#[cfg_attr(feature = "a", secured (policy = "x"))]\n```\n', ".md"
        ),
        [("secured", "policy")],
    )
    # A `cfg_attr` body is a predicate followed by attribute meta items, and
    # only the items are applied. An attribute's VALUE can carry a macro name
    # without invoking anything: `stringify!` takes `secured(…)` as tokens.
    check(
        "markdown: a macro name inside a cfg_attr value is not an invocation",
        scan_text(
            '```rust\n#[cfg_attr(all(), doc = stringify!(secured(policy = "x")))]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: a predicate naming a macro is not an invocation",
        scan_text(
            '```rust\n#[cfg_attr(all(feature = "a"), inline)]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: a nested cfg_attr payload is still reached",
        scan_text(
            '```rust\n#[cfg_attr(a, cfg_attr(b, secured(policy = "x")))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    # A comment is a token separator like whitespace, so it can sit in either
    # gap of an attribute head, and in front of a `cfg_attr` payload.
    check(
        "markdown: a block comment before the delimiter",
        scan_text('```rust\n#[secured /* why */ (policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a block comment before the macro name",
        scan_text('```rust\n#[/* why */ secured(policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a line comment before the delimiter",
        scan_text('```rust\n#[secured // why\n    (policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a nested block comment before the delimiter",
        scan_text('```rust\n#[secured /* a /* b */ c */ (policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: a comment before a cfg_attr payload",
        scan_text(
            '```rust\n#[cfg_attr(feature = "a", /* apply */ secured(policy = "x"))]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "markdown: a comment does not invent an invocation",
        scan_text('```rust\n#[derive(Debug)] /* secured(policy = "x") */\n```\n', ".md"),
        [],
    )
    # The name left of an `=` is a key whatever its case, and no macro has an
    # upper-case one.
    check(
        "markdown: an upper-case keyword key is caught",
        scan_text('```rust\n#[secured(Scopes = ["a:b"])]\n```\n', ".md"),
        [("secured", "Scopes")],
    )
    check(
        "markdown: a positional type is still not read as a flag",
        scan_text("```rust\n#[repository(Post, api, mcp, soft_delete)]\n```\n", ".md"),
        [],
    )
    check(
        "markdown: an associated type binding is not read as a key",
        scan_text("```rust\n#[listener(Iterator<Item = u32>, durable)]\n```\n", ".md"),
        [],
    )
    check(
        "rustdoc: whitespace before the delimiter",
        scan_text('//! ```ignore\n//! #[secured (policy = "x")]\n//! ```\n', ".rs"),
        [("secured", "policy")],
    )

    # The fence-delimiter rule applies to the rustdoc scanner as well.
    check(
        "rustdoc: four-backtick fence is scanned",
        scan_text('//! ````ignore\n//! #[secured(policy = "x")]\n//! ````\n', ".rs"),
        [("secured", "policy")],
    )
    check(
        "rustdoc: a shorter run does not close a longer fence",
        scan_text(
            '//! ````ignore\n//! ```\n//! #[secured(policy = "x")]\n//! ```\n//! ````\n',
            ".rs",
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: tilde fence is scanned",
        scan_text('//! ~~~ignore\n//! #[secured(policy = "x")]\n//! ~~~\n', ".rs"),
        [("secured", "policy")],
    )

    # A Rust fence DISPLAYED inside a non-Rust fence is documentation about a
    # fence, not one. Every fence is tracked; only Rust ones are judged.
    check(
        "markdown: rust fence displayed inside a text fence is not scanned",
        scan_text(
            '````text\n```rust\n#[secured(policy = "x")]\n```\n````\n', ".md"
        ),
        [],
    )
    check(
        "markdown: a real fence after a text fence is still scanned",
        scan_text(
            '```text\n#[secured(policy = "shown")]\n```\n'
            '```rust\n#[secured(policy = "real")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
    )
    check(
        "rustdoc: rust fence displayed inside a text fence is not scanned",
        scan_text(
            '//! ````text\n//! ```rust\n//! #[secured(policy = "x")]\n//! ```\n//! ````\n',
            ".rs",
        ),
        [],
    )

    # A proc-macro attribute accepts any delimited token tree.
    check(
        "markdown: brace-delimited attribute is inspected",
        scan_text('```rust\n#[secured { policy = "x" }]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: bracket-delimited attribute is inspected",
        scan_text('```rust\n#[secured [policy = "x"]]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: brace-delimited with a valid key passes",
        scan_text('```rust\n#[secured { scopes = ["a:b"] }]\n```\n', ".md"),
        [],
    )

    # `#[authorize]` takes its action verb positionally, bare or quoted.
    check(
        "markdown: authorize's positional action is not a flag",
        scan_text("```rust\n#[authorize(update, resource = Post)]\n```\n", ".md"),
        [],
    )
    check(
        "markdown: authorize's other keys are still judged",
        scan_text("```rust\n#[authorize(update, bogus = 1)]\n```\n", ".md"),
        [("authorize", "bogus")],
    )
    check(
        "markdown: the quoted action form still passes",
        scan_text(
            '```rust\n#[authorize("update", resource = Post)]\n```\n', ".md"
        ),
        [],
    )
    # …and the exemption is confined to that macro: elsewhere a leading bare
    # identifier is a flag and stays judged.
    check(
        "markdown: a leading flag on another macro is still judged",
        scan_text("```rust\n#[job(uniqe)]\n```\n", ".md"),
        [("job", "uniqe")],
    )
    check(
        "markdown: repository's leading positional and flags both work",
        scan_text("```rust\n#[repository(Post, api, mcp)]\n```\n", ".md"),
        [],
    )

    check(
        "markdown: two waivers cover two fences",
        scan_text(
            "<!-- macro-arg-allow: secured.policy -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n'
            "<!-- macro-arg-allow: secured.policy -->\n"
            '```rust\n#[secured(policy = "y")]\n```\n',
            ".md",
        ),
        [],
    )

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

    # Extraction is scoped to the arg parser, not the whole file. `model.rs`
    # mentions `username` in ~10k lines of codegen; `parse_attr_args` does not
    # accept it, so `#[model(username = …)]` is a defect.
    check("model accepts managed", "managed" in accepted["model"], True)
    check("model rejects username", "username" in accepted["model"], False)
    check(
        "markdown: a codegen word is not an accepted key",
        scan_text('```rust\n#[model(username = "x")]\n```\n', ".md"),
        [("model", "username")],
    )
    # A grammar split across helper parsers is still read whole.
    check("job accepts unique_by (helper parser)", "unique_by" in accepted["job"], True)
    check(
        "job accepts concurrency_key (helper parser)",
        "concurrency_key" in accepted["job"],
        True,
    )

    # A qualified invocation is the same call. Both forms ship in rustdoc.
    check(
        "markdown: qualified path is inspected",
        scan_text('```rust\n#[autumn_web::model(bogus = "x")]\n```\n', ".md"),
        [("model", "bogus")],
    )
    check(
        "rustdoc: qualified path is inspected",
        scan_text('//! ```ignore\n//! #[autumn_macros::model(bogus = 1)]\n//! ```\n', ".rs"),
        [("model", "bogus")],
    )

    # A multiline attribute is one call, not a set of unparseable lines.
    check(
        "markdown: multiline attribute is scanned",
        scan_text(
            '```rust\n#[model(\n    table = "posts",\n    bogus = 1,\n)]\n```\n', ".md"
        ),
        [("model", "bogus")],
    )
    check(
        "rustdoc: multiline attribute is scanned",
        scan_text(
            '//! ```ignore\n//! #[model(\n//!     table = "posts",\n'
            "//!     bogus = 1,\n//! )]\n//! ```\n",
            ".rs",
        ),
        [("model", "bogus")],
    )
    # A multiline call reports the line the attribute opens on.
    check(
        "markdown: multiline defect reports the opening line",
        scan_text_lines("pad\n```rust\n#[model(\n    bogus = 1,\n)]\n```\n", ".md"),
        ["3"],
    )

    # Every exported attribute macro is registered. An unregistered one is not
    # a permissive read but no read at all: its pages go ungated while the gate
    # still reports a clean run.
    lib = (MACRO_SRC / "lib.rs").read_text(encoding="utf-8", errors="replace")
    exported = set()
    for block in lib.split("#[proc_macro_attribute]")[1:]:
        found = re.search(r"pub fn ([a-z_0-9]+)\s*\(", block)
        if found:
            exported.add(found.group(1))
    check("registry covers every exported macro", sorted(exported - set(OWNERS)), [])
    check("route verbs are registered", "get" in OWNERS and "post" in OWNERS, True)

    # A value spelling reachable from the parser is not a key. `delete_all`,
    # `destroy`, `nullify` and `restrict` are accepted *after* `dependent =`,
    # never as `#[model(...)]` keys.
    check("model rejects a dependent-action value", "delete_all" in accepted["model"], False)
    check(
        "markdown: a parser value is not an accepted key",
        scan_text('```rust\n#[model(delete_all = true)]\n```\n', ".md"),
        [("model", "delete_all")],
    )
    # …while a grammar genuinely split across helper parsers stays reachable.
    check("job still accepts unique_by", "unique_by" in accepted["job"], True)

    # A delimiter inside a literal is data, not structure.
    check(
        "markdown: paren inside a string does not end the attribute",
        scan_text('```rust\n#[secured("admin)", policy = "x")]\n```\n', ".md"),
        [("secured", "policy")],
    )
    check(
        "markdown: bracket inside a string does not end the attribute",
        scan_text('```rust\n#[secured("a]b", bogus = 1)]\n```\n', ".md"),
        [("secured", "bogus")],
    )
    check(
        "markdown: raw string is skipped whole",
        scan_text('```rust\n#[secured(r#"a)b"#, bogus = 1)]\n```\n', ".md"),
        [("secured", "bogus")],
    )
    check(
        "markdown: escaped quote does not end the literal",
        scan_text('```rust\n#[secured("a\\")x", bogus = 1)]\n```\n', ".md"),
        [("secured", "bogus")],
    )
    # A `'` opens a char literal or a lifetime, and only the first has a
    # closing quote. Treating `'static` as a literal ran the scan to the end of
    # the passage looking for one, so the attribute never closed and every key
    # after the lifetime went unread.
    check(
        "markdown: a lifetime does not swallow the rest of the attribute",
        scan_text("```rust\n#[listener(Event<'static>, bogus = 1)]\n```\n", ".md"),
        [("listener", "bogus")],
    )
    check(
        "markdown: a char literal is still skipped whole",
        scan_text("```rust\n#[throttle(key = ')', bogus = 1)]\n```\n", ".md"),
        [("throttle", "bogus")],
    )

    # A waiver reaches the passage it introduces, and no further.
    check(
        "markdown: waiver covers the fence it introduces",
        scan_text(
            "<!-- macro-arg-allow: secured.policy — another framework's name -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n',
            ".md",
        ),
        [],
    )
    check(
        "markdown: waiver does not reach a distant later passage",
        scan_text(
            "<!-- macro-arg-allow: secured.policy -->\n"
            '```rust\n#[secured(policy = "x")]\n```\n'
            + "\nfiller\n" * 12
            + '```rust\n#[secured(policy = "typo")]\n```\n',
            ".md",
        ),
        [("secured", "policy")],
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
