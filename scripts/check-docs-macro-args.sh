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
# are additive, not a fallback chain: `static_get` has both, and taking only
# the first lost `params`. A `Parse` root is the implementing TYPE, never the
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
    # straight to `route::route_macro` (and `edge::edge_macro`), so it accepts
    # the whole route grammar. Reading only its own file left it with `crate`
    # alone and reported the valid `#[oauth2_callback("/cb", timeout_ms = …)]`
    # as drift.
    "oauth2_callback": (
        "oauth2_callback.rs",
        "route.rs",
        "parse.rs",
        "edge.rs",
    ),
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
    "static_get": ("static_route.rs", "parse.rs"),
    "step_up": "step_up.rs",
    "task": "one_off_task.rs",
    "throttle": "throttle.rs",
    "ws": ("ws.rs", "parse.rs"),
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


def _balanced(src, open_idx, opener="{", closer="}"):
    """Text between `open_idx`'s delimiter and its match."""
    i, depth = open_idx + 1, 1
    while i < len(src) and depth:
        if src[i] == opener:
            depth += 1
        elif src[i] == closer:
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
            strip_test_mods((MACRO_SRC / f).read_text(encoding="utf-8", errors="replace"))
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
        # Additive, not a fallback chain: a macro can have both. `static_get`
        # parses the shared route grammar in `parse_route_attr(attr)` and its
        # own extras in `impl Parse for StaticGetAttrs`, so taking only the
        # first lost `params` and reported five correct pages as drift. When
        # the parsed-into type is named explicitly, its siblings are some other
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
WAIVER = re.compile(r"macro-arg-allow:\s*([a-z_0-9]+)\.([a-z_0-9]+)")
# A keyword argument, but never a `==` comparison.
KEYWORD_ARG = re.compile(r"\b([a-z_][a-z_0-9]*)\s*=(?!=)")


BARE_FLAG = re.compile(r"^[a-z_][a-z_0-9]*$")


def top_level_keys(args):
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
        if ch in "\"'":
            i = skip_literal(args, i)
            if depth == 0:
                buf.append("\x00")  # a literal was here
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
    for segment in "".join(buf).split(","):
        segment = segment.strip()
        if not segment:
            continue
        if "=" in segment.replace("==", ""):
            head = segment.split("=", 1)[0].strip()
            if BARE_FLAG.match(head):
                names.append((head, False))
        elif BARE_FLAG.match(segment):
            names.append((segment, True))
    return names


# A call site may qualify the macro: `#[autumn_web::repository(...)]` and
# `#[autumn_macros::model(...)]` are documented, idiomatic forms and appear in
# shipped rustdoc (`autumn/src/aggregate.rs`, `autumn/src/classify/mod.rs`).
# Requiring the bare name would leave every qualified invocation ungated.
# `cfg_attr(<pred>, <attr>)` applies `<attr>` when the predicate holds, so a
# conditionally-applied Autumn macro is a real invocation the compiler will
# reject on a typo. Requiring the name to sit immediately after `#[` skipped
# every one of them.
MACRO_OPEN = re.compile(
    r"#\[(?:cfg_attr\s*\([^,]+,\s*)?(?:autumn_web::|autumn_macros::|autumn::)?("
    + "|".join(sorted(OWNERS))
    + r")\("
)


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
    out = []
    for match in MACRO_OPEN.finditer(text):
        i, depth = match.end(), 1
        while i < len(text) and depth:
            ch = text[i]
            if ch == "/":
                nxt = skip_comment(text, i)
                if nxt is not None:
                    i = nxt
                    continue
            if ch in "\"'":
                i = skip_literal(text, i)
                continue
            if ch == "r" and text[i + 1 : i + 2] in ('"', "#"):
                nxt = skip_raw_literal(text, i)
                if nxt is not None:
                    i = nxt
                    continue
            if ch in "([":
                depth += 1
            elif ch in ")]":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        if depth == 0:
            out.append((match.group(1), text[match.end() : i], match.start()))
    return out


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
        for key, is_flag in top_level_keys(args):
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
    for lineno, line in enumerate(lines, 1):
        # A fenced example inside a block quote is still an example a reader
        # copies. `docs/guide/mcp.md` and `docs/guide/openapi.md` both carry
        # one, and without stripping the CommonMark `>` prefix the scanner
        # never entered the fence at all.
        stripped = BLOCKQUOTE.sub("", line).lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            if inside:
                collected.append(current)
                current, inside = [], False
            else:
                lang = stripped[3:].strip().lower()
                inside = lang.startswith("rust")
                if inside:
                    fences += 1
            continue
        if inside:
            current.append((lineno, BLOCKQUOTE.sub("", line)))
    collected.append(current)
    return _judge_collected(rel, collected, accepted, judgeable, waived), fences


def scan_rustdoc(path, accepted, judgeable, _calls=None):
    """Same, over ```-fenced Rust inside `//!` and `///` doc comments."""
    rel = path.relative_to(ROOT)
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    waived = collect_waivers(lines)
    inside, fences, collected, current = False, 0, [], []
    for lineno, line in enumerate(lines, 1):
        doc = re.match(r"^\s*//[!/]\s?(.*)$", line)
        if not doc:
            # A non-doc line ends any fence: an unterminated fence must not
            # swallow the rest of the file.
            if inside:
                collected.append(current)
            current, inside = [], False
            continue
        body = doc.group(1).strip()
        if body.startswith("```"):
            if inside:
                collected.append(current)
                current, inside = [], False
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
        if inside:
            current.append((lineno, body))
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
    # …while a macro that genuinely has both an arg parser and a Parse impl
    # keeps both. `static_get` parses the shared route grammar in one and its
    # own extras in the other.
    check("static_get keeps params (Parse impl)", "params" in accepted["static_get"], True)
    check("static_get keeps api_version (arg parser)", "api_version" in accepted["static_get"], True)

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
