#!/usr/bin/env bash
# Verify that the determinism seam gate (issue #1797) is still armed and that
# nothing has quietly defanged it.
#
# THE DIVISION OF LABOUR (same as the #1611 panic gate)
#   clippy does the DETECTION. `clippy.toml`'s `disallowed-methods` array bans
#   `Instant::now` / `Utc::now` / `SystemTime::now` / `Uuid::new_v4`, and each
#   gated module re-denies the lint for its production code path via
#   `#![cfg_attr(not(test), deny(clippy::disallowed_methods))]`. That runs in the
#   same `lint` CI job.
#
#   THIS SCRIPT keeps clippy able to see. It guards the manifest, the header
#   shape, the config, and the absence of any escape hatch that would let an
#   off-seam time read ship while `cargo clippy -- -D warnings` stays green.
#   It never greps for the banned calls themselves: clippy resolves `use ... as`
#   aliases and macro expansions that a grep cannot, and — decisively — clippy
#   does NOT see string literals, whereas a grep would flag the ~30 templated
#   `Utc::now()` occurrences inside `autumn-cli`'s code generators and the
#   `include_dir!`-embedded starter apps, which are generated-app text, not code.
#
# POLARITY (deliberately inverted from the panic gate)
#   `clippy::disallowed_methods` is warn-by-default and `clippy.toml` is
#   workspace-global with no level control, so populating the array arms the lint
#   everywhere at once. The workspace therefore GRANDFATHERS it
#   (`[workspace.lints.clippy] disallowed_methods = "allow"`, plus a package-level
#   `[lints.clippy]` table in each crate that does not opt into the workspace
#   table), and a module opts IN by carrying the gate header. The manifest below
#   is the enforced subset; MODULE_COUNT_FLOOR makes it a ratchet.
#
# WHAT IT CHECKS
#   1.  Every manifest module exists and carries the `autumn-determinism-gate:`
#       marker.
#   2.  The marker is IMMEDIATELY followed (blank/comment lines aside) by the
#       gate header, and that header is structurally exact: after stripping
#       comments and whitespace it must read `#![cfg_attr(not(test),deny(` and
#       list every lint in REQUIRED_GATE_LINTS, with no `allow(`/`expect(`/
#       `warn(` of its own. A widened predicate such as `all(not(test), any())`
#       — whose deny never compiles — therefore fails.
#   3.  Tree-wide inner-suppression scan: no inner attribute (`#![allow(…)]`,
#       `#![expect(…)]`, or the `cfg_attr(…, allow(…))` form) anywhere under the
#       scan roots may re-permit a gate lint or a blanket group. Attributes
#       inside a `#[cfg(test)]` scope are exempt.
#   4.  Per-site allow hygiene: an OUTER `#[allow(clippy::disallowed_methods…)]`
#       in a gated module must carry a NON-EMPTY `reason = "…"`.
#   5.  Reverse manifest: every marker-carrying `*.rs` under the scan roots must
#       be listed in the manifest.
#   6.  Module-count floor: the manifest may not shrink below
#       MODULE_COUNT_FLOOR.
#   7.  Config completeness: `clippy.toml` still lists every path in
#       REQUIRED_CONFIG_PATHS, each with a non-empty `reason`, and still pins
#       `msrv`. Emptying the array would silently kill the gate while every
#       header stayed in place.
#   8.  No crate-local `clippy.toml`: clippy resolves its config from the nearest
#       ancestor of CARGO_MANIFEST_DIR and stops there, so a crate-local file
#       SHADOWS the root one entirely — disarming the ban and dropping the MSRV
#       pin with no diagnostic.
#   9.  The workspace grandfather line is present in the root `Cargo.toml`.
#       Without it the array arms every member and CI fails on hundreds of
#       pre-existing, out-of-scope sites — so someone would "fix" it by emptying
#       the array.
#   10. Feature reachability: a gated module behind a non-default Cargo feature
#       must have that feature enabled by an ENFORCING ci.yml clippy lane (one
#       that is not commented out and carries both `-p autumn-web` and
#       `-D warnings`), otherwise its deny block is never compiled.
#
# NOT covered: a banned call reached only through a `macro_rules!` expansion
# (clippy suppresses lints inside macro expansions), and the generated code
# `#[repository]` emits into a *downstream* crate — that expansion carries its
# own `#[allow]` with a reason and is named as a known-open gap in
# CONTRIBUTING.md "Determinism seam gate".
#
# Called from the `lint` job in ci.yml. Run locally with:
#
#     ./scripts/check-determinism-gate.sh              # self-test, then the real check
#     ./scripts/check-determinism-gate.sh --self-test  # synthetic fixtures only
#     ./scripts/check-determinism-gate.sh --check-only # real tree only
#
# The default invocation runs the self-test FIRST so a refactor that silently
# defangs this script fails here rather than years later on a real regression.
# No toolchain, no cargo, no network — the whole thing is under two seconds.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

die() {
  echo "error: $*" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Canonical configuration
# ---------------------------------------------------------------------------

# Modules whose PRODUCTION code path must read time and mint identifiers through
# the injected seams, as `<path>:<cargo feature>`. `default` means the module is
# compiled by a plain `cargo clippy --workspace`. Keep in sync with
# CONTRIBUTING.md "Determinism seam gate".
GATED_MODULES=(
  autumn/src/time.rs:default
  autumn/src/entropy.rs:default
  autumn/src/state.rs:default
  autumn/src/scheduler.rs:default
  autumn/src/app.rs:default
  autumn/src/db.rs:default
  autumn/src/job.rs:default
  # The durable SQLite job backend (#1907): a submodule of the gated job.rs,
  # listed on its own because its enforcing clippy lane is the sqlite one.
  autumn/src/job/sqlite.rs:sqlite
  # The ratified W3 identifier sites. They carry no off-seam production call at
  # all — every id they mint already comes from the injected `Entropy` — so
  # gating them costs nothing and is what makes the `Uuid::new_v4` clause of the
  # ban enforce something rather than sit unreachable.
  autumn/src/session.rs:default
  autumn/src/middleware/request_id.rs:default
  # The authored fault lane (#1680). Every fault timestamp and every window
  # decision is read from the app's INJECTED clock — that is the whole point of
  # `FaultPlan::only_between`, and a single `Utc::now()` here would make a
  # replayed scenario's outcome record differ run to run.
  autumn/src/sim/fault.rs:default
  # The embedded cluster control plane (#1762). Every node id comes from the
  # injected `Entropy`, every elapsed measurement and the boot incarnation come
  # from the injected `ClockSource` — the deterministic two-node suite in
  # `src/cluster/tests.rs` is only reproducible because of that, so the gate is
  # load-bearing here rather than decorative.
  autumn/src/cluster/mod.rs:default
  autumn/src/cluster/counter.rs:default
  autumn/src/cluster/membership.rs:default
  autumn/src/cluster/wire.rs:default
  autumn/src/cluster/transport.rs:default
  autumn/src/cluster/node.rs:default
  # In-place upgrades (#1674). The handoff measures the successor's readiness
  # window on the injected `ClockSource`, so an upgrade's timing is as
  # reproducible under a virtual clock as the drain it replaces.
  autumn/src/upgrade.rs:default
  # Direct HTTPS termination (#1603). `tls.rs::now_unix` is the module's one
  # deliberate real-wall-time read — certificate validity is a fact about the
  # real world, not about the injected clock — and the gate is what keeps a
  # later off-seam read added beside it (in the bind path or the `CertReloader`
  # poll loop) from shipping unlinted. NOTE: `autumn/src/acme/renewal.rs` has
  # its own ungated `default_now_unix`; gating it needs an `--features acme`
  # enforcing clippy lane, which does not exist yet.
  autumn/src/tls.rs:tls
)

# The manifest is a ratchet: it may grow, never shrink.
MODULE_COUNT_FLOOR=19

# Every lint the gate header must deny.
REQUIRED_GATE_LINTS=(
  clippy::disallowed_methods
)

# Every method path `clippy.toml` must ban.
REQUIRED_CONFIG_PATHS=(
  "std::time::Instant::now"
  "chrono::Utc::now"
  "std::time::SystemTime::now"
  "uuid::Uuid::new_v4"
)

MARKER="autumn-determinism-gate:"
SCAN_DIRS=("autumn/src")
CLIPPY_TOML="clippy.toml"
ROOT_MANIFEST="Cargo.toml"
CI_WORKFLOW=".github/workflows/ci.yml"

# A `reason = "…"` with at least one character. The `+` is load-bearing:
# `reason = ""` must fail.
NONEMPTY_REASON_RE='reason[[:space:]]*=[[:space:]]*"[^"]+"'

# Blanket groups whose re-permitting would sweep the gate lints along with them.
BLANKET_GROUPS='clippy::(restriction|all|pedantic|nursery)'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Rewrite a Rust source file so that every check below reads CODE, not text.
#
# Line and (NESTING-AWARE) block comments are blanked to spaces; inside string,
# char, and raw-string literals every bracket — ( ) [ ] { } — is replaced with a
# placeholder byte. Line structure and byte offsets are preserved exactly, so
# line numbers stay meaningful.
#
# This is what closes four otherwise-fatal holes, each verified against the real
# tree before it was fixed:
#   * a gate header (or the whole marker + header) hidden inside `/* … */` was
#     extracted as if it were live code — so the gate could be commented out
#     while the script still reported OK;
#   * perl's non-greedy `/\*.*?\*/` does not nest, but Rust block comments DO,
#     so `deny(/* /* */ clippy::disallowed_methods */ clippy::correctness)`
#     flattened to text that *contained* the required lint while rustc saw a
#     header denying something else entirely;
#   * a `{` or `}` inside a string literal desynchronised the `#[cfg(test)]`
#     brace counter, and a `(` inside a `reason = "…"` string desynchronised the
#     attribute joiner;
#   * a lint named only in a comment counted as denied.
#
# Brackets are replaced rather than the whole string because the per-site check
# still needs to see `reason = "…"` and prove it is non-empty.
# NOTE ON APOSTROPHES: the perl program below is a single-quoted shell string,
# so a literal apostrophe anywhere inside it -- including in a perl comment --
# terminates that string. Keep perl comments apostrophe-free; explanations that
# need one live out here.
#
# Lifetime vs char literal: a tick is a LIFETIME (as in a reference with a named
# lifetime) unless the next byte opens an escape or the byte after that closes
# the literal. Getting this wrong is catastrophic -- a lifetime would swallow
# every byte up to the next tick in the file.
sanitize_rust() {
  perl -0777 -e '
    my $src = do { local $/; <STDIN> };
    my @c = split //, $src;
    my $n = scalar @c;
    my ($i, $out) = (0, "");
    my $blank = sub { my $ch = shift; return $ch eq "\n" ? "\n" : " " };
    my $mask  = sub { my $ch = shift; return $ch =~ /[\(\)\[\]\{\}]/ ? "\x01" : $ch };
    while ($i < $n) {
      my $ch = $c[$i];
      my $nx = $i + 1 < $n ? $c[$i + 1] : "";
      # line comment
      if ($ch eq "/" && $nx eq "/") {
        while ($i < $n && $c[$i] ne "\n") { $out .= $blank->($c[$i]); $i++ }
        next;
      }
      # block comment (nesting)
      if ($ch eq "/" && $nx eq "*") {
        my $depth = 0;
        while ($i < $n) {
          if ($c[$i] eq "/" && $i + 1 < $n && $c[$i + 1] eq "*") {
            $depth++; $out .= $blank->($c[$i]); $out .= $blank->($c[$i + 1]); $i += 2; next;
          }
          if ($c[$i] eq "*" && $i + 1 < $n && $c[$i + 1] eq "/") {
            $depth--; $out .= $blank->($c[$i]); $out .= $blank->($c[$i + 1]); $i += 2;
            last if $depth == 0;
            next;
          }
          $out .= $blank->($c[$i]); $i++;
        }
        next;
      }
      # raw string: r"…" / r#"…"# / br#"…"#. Guarded on the preceding byte so a
      # bare `r` inside an identifier (`for`, `iter`) cannot start one.
      my $prev = $i > 0 ? $c[$i - 1] : "";
      if (($ch eq "r" || ($ch eq "b" && $nx eq "r")) && $prev !~ /[A-Za-z0-9_]/) {
        my $j = $i + ($ch eq "b" ? 2 : 1);
        my $hashes = 0;
        while ($j < $n && $c[$j] eq "#") { $hashes++; $j++ }
        if ($j < $n && $c[$j] eq "\"") {
          # copy the prefix verbatim, then mask until the closing delimiter
          $out .= join("", @c[$i .. $j]);
          $i = $j + 1;
          my $close = "\"" . ("#" x $hashes);
          while ($i < $n) {
            if (join("", @c[$i .. ($i + length($close) - 1 > $n - 1 ? $n - 1 : $i + length($close) - 1)]) eq $close) {
              $out .= $close; $i += length($close); last;
            }
            $out .= $mask->($c[$i]); $i++;
          }
          next;
        }
      }
      # LIFETIME vs char-literal disambiguation -- see the shell comment above.
      if ($ch eq "'"'"'") {
        my $n2 = $i + 2 < $n ? $c[$i + 2] : "";
        unless ($nx eq "\\" || $n2 eq "'"'"'") {
          $out .= $ch; $i++; next;
        }
      }
      # string / char literal
      if ($ch eq "\"" || $ch eq "'"'"'") {
        my $q = $ch;
        $out .= $q; $i++;
        while ($i < $n) {
          if ($c[$i] eq "\\" && $i + 1 < $n) { $out .= "  "; $i += 2; next }
          if ($c[$i] eq $q) { $out .= $q; $i++; last }
          $out .= $mask->($c[$i]); $i++;
        }
        next;
      }
      $out .= $ch; $i++;
    }
    print $out;
  '
}

# Collapse whitespace out of the (already comment-free) header text so its
# STRUCTURE is validated rather than its rustfmt formatting.
strip_space() {
  tr -d '[:space:]'
}

# Print the module's gate header: the `#![cfg_attr(…)]` attribute that follows
# the marker line, read from the SANITIZED source so an attribute that only
# exists inside a comment is invisible here (as it is to rustc).
extract_gate_header() {
  local sanitized="$1" marker_line="$2"
  awk -v from="$marker_line" '
    NR < from { next }
    !started {
      if ($0 ~ /^[[:space:]]*#!\[cfg_attr\(/) { started = 1 }
      else if ($0 ~ /^[[:space:]]*$/) { next }
      else { exit }
    }
    started { print; depth += gsub(/\(/, "("); depth -= gsub(/\)/, ")"); if (depth <= 0) exit }
  ' "$sanitized"
}

# The line number of the gate marker, requiring it to be a LIVE line comment:
# the marker text must be present in the original, and the same line must be
# blank in the sanitized view (i.e. it really was a comment, not code, and not
# text inside a block comment that also swallowed the header).
marker_line_number() {
  local file="$1" sanitized="$2"
  awk -v marker="$MARKER" '
    NR == FNR { if (index($0, marker)) cand[NR] = 1; next }
    (FNR in cand) && $0 ~ /^[[:space:]]*$/ { print FNR; exit }
  ' "$file" "$sanitized"
}

# Join every attribute (`#[…]` or `#![…]`) in a sanitized file into one logical
# line, emitting `<start-line><TAB><kind><TAB><joined text>`. Bracket characters
# inside literals are already masked, so the depth counter cannot desynchronise,
# and a multi-line attribute — the house style, since every allow carries a
# `reason = "…"` — is tested as a single unit.
join_attributes() {
  awk '
    {
      line[NR] = $0
    }
    END {
      for (i = 1; i <= NR; i++) {
        s = line[i]
        # find each attribute start on this line
        pos = 1
        while (1) {
          hi = index(substr(s, pos), "#!["); ho = index(substr(s, pos), "#[")
          if (hi == 0 && ho == 0) break
          if (hi > 0 && (ho == 0 || hi <= ho)) { at = pos + hi - 1; kind = "inner"; skip = 3 }
          else { at = pos + ho - 1; kind = "outer"; skip = 2 }
          buf = substr(s, at)
          depth = gsub(/\[/, "[", buf) - gsub(/\]/, "]", buf)
          j = i
          while (depth > 0 && j < NR) {
            j++
            buf = buf " " line[j]
            depth += gsub(/\[/, "[", line[j]) - gsub(/\]/, "]", line[j])
          }
          gsub(/\t/, " ", buf)
          print i "\t" kind "\t" buf
          pos = at + skip
          if (pos > length(s)) break
        }
      }
    }
  ' "$1"
}

# Emit every line index inside a `#[cfg(test)] mod …` block — the ONLY form the
# exemption advertises. Requiring the `mod` keyword is load-bearing: the old
# version latched on `#[cfg(test)]` applied to a *struct field*
# (`autumn/src/scheduler.rs` has one) and then exempted the next 69 lines of
# production code. Braces inside literals are already masked by `sanitize_rust`,
# so the depth counter is trustworthy; an unbalanced block is a hard error
# rather than a silent exemption running to EOF.
cfg_test_mod_line_set() {
  awk '
    /^[[:space:]]*#\[cfg\(test\)\]/ && !intest { pending = NR; next }
    pending && /^[[:space:]]*$/ { next }
    pending {
      if ($0 ~ /(^|[[:space:]])mod[[:space:]]+[A-Za-z_]/) {
        intest = 1
        for (k = pending; k < NR; k++) print k
        print NR
        depth = gsub(/\{/, "{") - gsub(/\}/, "}")
        if (depth <= 0 && index($0, "{") > 0) intest = 0
        pending = 0
        next
      }
      pending = 0
    }
    intest {
      print NR
      depth += gsub(/\{/, "{") - gsub(/\}/, "}")
      if (depth <= 0) intest = 0
    }
    END { if (intest) { print "UNBALANCED" } }
  ' "$1"
}

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

check_manifest_headers() {
  local tree="$1"
  local entry file feature header flat lint san mline
  for entry in "${GATED_MODULES[@]}"; do
    file="${entry%%:*}"
    feature="${entry##*:}"
    [[ -n "$feature" ]] || die "manifest entry '$entry' is missing its :<feature> suffix"
    [[ -f "$tree/$file" ]] || die "gated module '$file' does not exist"

    grep -qF "$MARKER" "$tree/$file" \
      || die "gated module '$file' is missing the '$MARKER' marker"

    san="$(mktemp)"
    sanitize_rust < "$tree/$file" > "$san"

    mline="$(marker_line_number "$tree/$file" "$san")"
    [[ -n "$mline" ]] \
      || { rm -f "$san"; die "gated module '$file' carries the '$MARKER' text but not as a live line comment (it is inside a block comment or a string literal), so the header under it is not live code either"; }

    header="$(extract_gate_header "$san" "$((mline + 1))")"
    [[ -n "$header" ]] \
      || { rm -f "$san"; die "gated module '$file' has the marker but no live '#![cfg_attr(' gate header immediately after it"; }
    rm -f "$san"

    flat="$(printf '%s' "$header" | strip_space)"
    [[ "$flat" == '#![cfg_attr(not(test),deny('* ]] \
      || die "gated module '$file' gate header must open exactly '#![cfg_attr(not(test), deny(' (got: ${flat:0:60}…)"

    case "$flat" in
      *'allow('*|*'expect('*|*'warn('*)
        die "gated module '$file' gate header must not carry its own allow/expect/warn" ;;
    esac

    for lint in "${REQUIRED_GATE_LINTS[@]}"; do
      case "$flat" in
        *"$lint"*) ;;
        *) die "gated module '$file' gate header does not deny '$lint'" ;;
      esac
    done
  done
}

check_inner_suppressions() {
  local tree="$1"
  local dir file san exempt line n kind text lint hit
  for dir in "${SCAN_DIRS[@]}"; do
    [[ -d "$tree/$dir" ]] || continue
    while IFS= read -r file; do
      san="$(mktemp)"
      sanitize_rust < "$file" > "$san"
      exempt=" $(cfg_test_mod_line_set "$san" | tr '\n' ' ') "
      [[ "$exempt" == *" UNBALANCED "* ]] \
        && { rm -f "$san"; die "${file#"$tree/"}: a #[cfg(test)] mod block never closes; refusing to guess its extent (an unbalanced block would silently exempt the rest of the file)"; }
      while IFS=$'\t' read -r n kind text; do
        [[ "$kind" == "inner" ]] || continue
        [[ "$text" =~ \#!\[[[:space:]]*(allow|expect)[[:space:]]*\( ]] \
          || [[ "$text" =~ cfg_attr.*[[:space:]]*(allow|expect)[[:space:]]*\( ]] \
          || continue
        [[ "$exempt" == *" $n "* ]] && continue
        hit=""
        for lint in "${REQUIRED_GATE_LINTS[@]}"; do
          [[ "$text" == *"$lint"* ]] && hit="$lint"
        done
        if [[ -z "$hit" ]] && [[ "$text" =~ $BLANKET_GROUPS ]]; then
          hit="a blanket clippy group"
        fi
        if [[ -n "$hit" ]]; then
          rm -f "$san"
          die "${file#"$tree/"}:$n re-permits $hit with a module-wide inner attribute; use a narrowest-scope #[allow(…, reason = \"…\")] instead"
        fi
      done < <(join_attributes "$san")
      rm -f "$san"
    done < <(find "$tree/$dir" -name '*.rs' -type f | sort)
  done
}

check_per_site_allow_reasons() {
  local tree="$1"
  local entry file san n kind text
  for entry in "${GATED_MODULES[@]}"; do
    file="${entry%%:*}"
    san="$(mktemp)"
    sanitize_rust < "$tree/$file" > "$san"
    while IFS=$'\t' read -r n kind text; do
      [[ "$kind" == "outer" ]] || continue
      [[ "$text" == *"clippy::disallowed_methods"* ]] || continue
      [[ "$text" =~ $NONEMPTY_REASON_RE ]] \
        || { rm -f "$san"; die "$file:$n has #[allow(clippy::disallowed_methods)] without a non-empty reason = \"…\""; }
    done < <(join_attributes "$san")
    rm -f "$san"
  done
}

check_reverse_manifest() {
  local tree="$1"
  local dir file rel listed entry
  for dir in "${SCAN_DIRS[@]}"; do
    [[ -d "$tree/$dir" ]] || continue
    while IFS= read -r file; do
      grep -qF "$MARKER" "$file" || continue
      rel="${file#"$tree/"}"
      listed=""
      for entry in "${GATED_MODULES[@]}"; do
        [[ "${entry%%:*}" == "$rel" ]] && listed=1
      done
      [[ -n "$listed" ]] \
        || die "'$rel' carries the '$MARKER' marker but is not in GATED_MODULES; add it (the manifest may grow, never shrink)"
    done < <(find "$tree/$dir" -name '*.rs' -type f | sort)
  done
}

check_module_floor() {
  local count="${#GATED_MODULES[@]}"
  [[ "$count" -ge "$MODULE_COUNT_FLOOR" ]] \
    || die "GATED_MODULES has $count entries, below the floor of $MODULE_COUNT_FLOOR; the gate may grow, never shrink"
}

check_clippy_config() {
  local tree="$1"
  local conf="$tree/$CLIPPY_TOML"
  [[ -f "$conf" ]] || die "$CLIPPY_TOML is missing; the whole ban lives in its disallowed-methods array"
  grep -Eq '^[[:space:]]*msrv[[:space:]]*=' "$conf" \
    || die "$CLIPPY_TOML no longer pins 'msrv'"
  grep -q 'disallowed-methods' "$conf" \
    || die "$CLIPPY_TOML no longer declares 'disallowed-methods'; the gate headers would deny a lint that bans nothing"
  local path line
  for path in "${REQUIRED_CONFIG_PATHS[@]}"; do
    line="$(grep -F "\"$path\"" "$conf" || true)"
    [[ -n "$line" ]] \
      || die "$CLIPPY_TOML no longer bans '$path'"
    [[ "$line" =~ $NONEMPTY_REASON_RE ]] \
      || die "$CLIPPY_TOML entry for '$path' has no non-empty reason = \"…\" (the reason IS the error message a contributor sees)"
  done
}

check_no_crate_local_clippy_toml() {
  local tree="$1"
  local found
  found="$(find "$tree" \( -name clippy.toml -o -name .clippy.toml \) \
             -not -path "$tree/target/*" -not -path '*/.git/*' | sort)"
  [[ "$found" == "$tree/$CLIPPY_TOML" ]] \
    || die "a crate-local clippy.toml exists and would SHADOW the root one, silently disarming the determinism ban and dropping the MSRV pin. Found:
$found"
}

check_workspace_grandfather() {
  local tree="$1"
  grep -Eq '^[[:space:]]*disallowed_methods[[:space:]]*=[[:space:]]*"allow"' "$tree/$ROOT_MANIFEST" \
    || die "root $ROOT_MANIFEST is missing '[workspace.lints.clippy] disallowed_methods = \"allow\"'. Without it clippy.toml's array arms the lint in every workspace member, turning hundreds of out-of-scope sites into CI failures — and the pressure to 'fix' that is to empty the array, which silently removes the gate."
}

check_feature_reachability() {
  local tree="$1"
  local wf="$tree/$CI_WORKFLOW"
  [[ -f "$wf" ]] || die "$CI_WORKFLOW is missing"
  # Feature lists from ENFORCING clippy lanes only: uncommented `cargo clippy`
  # runs that target autumn-web and deny warnings.
  local enabled
  enabled="$(grep -E '^[[:space:]]*run:[[:space:]]*cargo clippy' "$wf" \
             | grep -F -- '-p autumn-web' | grep -F -- '-D warnings' \
             | grep -oE '\-\-features "[^"]*"' | tr -d '"' | sed 's/--features //' | tr ',' '\n' | tr -d ' ' | sort -u || true)"
  local entry feature
  for entry in "${GATED_MODULES[@]}"; do
    feature="${entry##*:}"
    [[ "$feature" == "default" ]] && continue
    grep -qx "$feature" <<<"$enabled" \
      || die "gated module '${entry%%:*}' is behind feature '$feature', which no enforcing ci.yml clippy lane enables — its deny block would never be compiled"
  done
}

run_all_checks() {
  local tree="$1"
  check_module_floor
  check_manifest_headers "$tree"
  check_inner_suppressions "$tree"
  check_per_site_allow_reasons "$tree"
  check_reverse_manifest "$tree"
  check_clippy_config "$tree"
  check_no_crate_local_clippy_toml "$tree"
  check_workspace_grandfather "$tree"
  check_feature_reachability "$tree"
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

GOOD_HEADER='// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework'"'"'s injected seams.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
'

make_fixture() {
  local dir="$1"
  mkdir -p "$dir/autumn/src" "$dir/.github/workflows"
  printf '%s' "$GOOD_HEADER" > "$dir/autumn/src/gated.rs"
  cat > "$dir/clippy.toml" <<'EOF'
msrv = "1.88.0"
disallowed-methods = [
  { path = "std::time::Instant::now", reason = "use the injected clock" },
  { path = "chrono::Utc::now", reason = "use the injected clock" },
  { path = "std::time::SystemTime::now", reason = "use the injected clock" },
  { path = "uuid::Uuid::new_v4", reason = "use the injected entropy source" },
]
EOF
  cat > "$dir/Cargo.toml" <<'EOF'
[workspace.lints.clippy]
disallowed_methods = "allow"
EOF
  cat > "$dir/.github/workflows/ci.yml" <<'EOF'
      - name: Clippy
        run: cargo clippy -p autumn-web --features "ws,tls" --all-targets -- -D warnings
EOF
}

# Run the full check set against a fixture tree, expecting failure with a
# message matching `$2`.
expect_fail() {
  local dir="$1" pattern="$2" label="$3"
  local out
  if out="$( (run_all_checks "$dir") 2>&1 )"; then
    die "self-test '$label' PASSED but should have failed"
  fi
  grep -qE "$pattern" <<<"$out" \
    || die "self-test '$label' failed for the wrong reason: $out"
}

expect_pass() {
  local dir="$1" label="$2"
  local out
  out="$( (run_all_checks "$dir") 2>&1 )" \
    || die "self-test '$label' should have passed but failed: $out"
}

# Second half of the self-test scenario list. Split out purely for readability:
# the first half lives inline in the runner below, next to the fixture helpers.
self_test_tail() {
  local tmp="$1"

  make_fixture "$tmp/multiline"
  { printf '%s' "$GOOD_HEADER"
    printf '#[allow(\n    clippy::disallowed_methods,\n    reason = "this IS the seam"\n)]\nfn f() {}\n'; } \
    > "$tmp/multiline/autumn/src/gated.rs"
  expect_pass "$tmp/multiline" "multi-line per-site allow with a reason"

  # Unlisted marker-carrying module (reverse manifest).
  make_fixture "$tmp/unlisted"
  printf '%s' "$GOOD_HEADER" > "$tmp/unlisted/autumn/src/other.rs"
  expect_fail "$tmp/unlisted" "is not in GATED_MODULES" "unlisted gated module"

  # clippy.toml emptied of a required path.
  make_fixture "$tmp/noconfig"
  sed -i '/chrono::Utc::now/d' "$tmp/noconfig/clippy.toml"
  expect_fail "$tmp/noconfig" "no longer bans" "config missing a required path"

  # clippy.toml entry stripped of its reason.
  make_fixture "$tmp/noconfreason"
  sed -i 's/{ path = "chrono::Utc::now".*/{ path = "chrono::Utc::now" },/' "$tmp/noconfreason/clippy.toml"
  expect_fail "$tmp/noconfreason" "no non-empty reason" "config entry without a reason"

  # msrv pin dropped.
  make_fixture "$tmp/nomsrv"
  sed -i '/^msrv/d' "$tmp/nomsrv/clippy.toml"
  expect_fail "$tmp/nomsrv" "no longer pins 'msrv'" "msrv pin dropped"

  # Crate-local clippy.toml shadowing the root.
  make_fixture "$tmp/shadow"
  mkdir -p "$tmp/shadow/autumn"
  printf 'msrv = "1.88.0"\n' > "$tmp/shadow/autumn/clippy.toml"
  expect_fail "$tmp/shadow" "would SHADOW the root one" "crate-local clippy.toml"

  # Workspace grandfather line removed.
  make_fixture "$tmp/nograndfather"
  printf '[workspace]\n' > "$tmp/nograndfather/Cargo.toml"
  expect_fail "$tmp/nograndfather" "disallowed_methods = .allow." "workspace grandfather removed"

  # A gated module behind a feature no enforcing lane enables.
  make_fixture "$tmp/unreachable"
  GATED_MODULES=(autumn/src/gated.rs:nosuchfeature)
  expect_fail "$tmp/unreachable" "no enforcing ci.yml clippy lane enables" "feature unreachable by any lint lane"
  GATED_MODULES=(autumn/src/gated.rs:default)

  # A feature enabled only by a NON-enforcing lane (no -D warnings) does not count.
  make_fixture "$tmp/nonenforcing"
  cat > "$tmp/nonenforcing/.github/workflows/ci.yml" <<'EOF'
      - name: Clippy (no denial)
        run: cargo clippy -p autumn-web --features "acme" --all-targets
EOF
  GATED_MODULES=(autumn/src/gated.rs:acme)
  expect_fail "$tmp/nonenforcing" "no enforcing ci.yml clippy lane enables" "non-enforcing lane does not count"
  GATED_MODULES=(autumn/src/gated.rs:default)

  # A feature enabled by an enforcing lane DOES count.
  make_fixture "$tmp/reachable"
  GATED_MODULES=(autumn/src/gated.rs:tls)
  expect_pass "$tmp/reachable" "feature reachable via an enforcing lane"
  GATED_MODULES=(autumn/src/gated.rs:default)

  # Floor enforcement.
  make_fixture "$tmp/floor"
  GATED_MODULES=()
  expect_fail "$tmp/floor" "below the floor" "manifest shrunk below the floor"
}

mode="${1:---all}"
case "$mode" in
  --self-test|--all) ;;
  --check-only) ;;
  *) die "unknown mode '$mode' (expected --self-test, --check-only, or --all)" ;;
esac

if [[ "$mode" != "--check-only" ]]; then
  echo "determinism gate: self-test"
  (
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    GATED_MODULES=(autumn/src/gated.rs:default)
    MODULE_COUNT_FLOOR=1

    make_fixture "$tmp/ok";           expect_pass "$tmp/ok" "baseline"

    make_fixture "$tmp/missing";      rm "$tmp/missing/autumn/src/gated.rs"
    expect_fail "$tmp/missing" "does not exist" "missing module"

    make_fixture "$tmp/nohdr"
    printf '// %s marker only\nfn f() {}\n' "$MARKER" > "$tmp/nohdr/autumn/src/gated.rs"
    expect_fail "$tmp/nohdr" "no live '#!\[cfg_attr\(' gate header" "marker without header"

    make_fixture "$tmp/widened"
    printf '// %s\n#![cfg_attr(all(not(test), any()), deny(clippy::disallowed_methods))]\n' "$MARKER" \
      > "$tmp/widened/autumn/src/gated.rs"
    expect_fail "$tmp/widened" "must open exactly" "widened cfg predicate"

    make_fixture "$tmp/comment"
    printf '// %s\n#![cfg_attr(any(), /* not(test) */ deny(clippy::disallowed_methods))]\n' "$MARKER" \
      > "$tmp/comment/autumn/src/gated.rs"
    expect_fail "$tmp/comment" "must open exactly" "predicate hidden in a comment"

    make_fixture "$tmp/wronglint"
    printf '// %s\n#![cfg_attr(not(test), deny(clippy::unwrap_used))]\n' "$MARKER" \
      > "$tmp/wronglint/autumn/src/gated.rs"
    expect_fail "$tmp/wronglint" "does not deny" "header missing the required lint"

    make_fixture "$tmp/innerallow"
    { printf '%s' "$GOOD_HEADER"; printf '#![allow(clippy::disallowed_methods)]\n'; } \
      > "$tmp/innerallow/autumn/src/gated.rs"
    expect_fail "$tmp/innerallow" "re-permits" "module-wide inner allow"

    make_fixture "$tmp/blanket"
    { printf '%s' "$GOOD_HEADER"; printf '#![allow(clippy::all)]\n'; } \
      > "$tmp/blanket/autumn/src/gated.rs"
    expect_fail "$tmp/blanket" "re-permits" "blanket group inner allow"

    make_fixture "$tmp/testallow"
    { printf '%s' "$GOOD_HEADER"
      printf '#[cfg(test)]\nmod tests {\n    #![allow(clippy::disallowed_methods)]\n}\n'; } \
      > "$tmp/testallow/autumn/src/gated.rs"
    expect_pass "$tmp/testallow" "cfg(test) inner allow is exempt"

    make_fixture "$tmp/noreason"
    { printf '%s' "$GOOD_HEADER"; printf '#[allow(clippy::disallowed_methods)]\nfn f() {}\n'; } \
      > "$tmp/noreason/autumn/src/gated.rs"
    expect_fail "$tmp/noreason" "without a non-empty reason" "per-site allow without a reason"

    make_fixture "$tmp/emptyreason"
    { printf '%s' "$GOOD_HEADER"; printf '#[allow(clippy::disallowed_methods, reason = "")]\nfn f() {}\n'; } \
      > "$tmp/emptyreason/autumn/src/gated.rs"
    expect_fail "$tmp/emptyreason" "without a non-empty reason" "per-site allow with an empty reason"

    # ── Attacks the first version of this script did NOT survive ────────────
    # Each of these was verified to pass the old gate AND `cargo clippy
    # --all-targets -- -D warnings` while leaving an off-seam call live.

    # The whole marker+header block-commented out (the plausible-by-accident one:
    # someone comments it to unblock a local build and forgets to restore it).
    make_fixture "$tmp/blockcomment"
    { printf '/*\n'; printf '%s' "$GOOD_HEADER"; printf '*/\nfn f() {}\n'; } \
      > "$tmp/blockcomment/autumn/src/gated.rs"
    expect_fail "$tmp/blockcomment" "no live '#!\[cfg_attr\(' gate header" "marker + header inside a block comment"

    # NESTED block comment smuggling the required lint into the flattened text
    # while rustc sees a header denying something else. Perl's non-greedy
    # `/\*.*?\*/` does not nest; Rust's block comments do.
    make_fixture "$tmp/nested"
    { printf '// %s\n' "$MARKER"
      printf '#![cfg_attr(not(test), deny(/* /* */ clippy::disallowed_methods */ clippy::correctness /* */))]\n'; } \
      > "$tmp/nested/autumn/src/gated.rs"
    expect_fail "$tmp/nested" "does not deny" "nested block comment hiding the required lint"

    # MULTI-LINE inner allow. The house style puts `reason = "…"` on its own
    # line, so this is the shape a real spoof would take.
    make_fixture "$tmp/multiallow"
    { printf '%s' "$GOOD_HEADER"
      printf 'pub mod inner {\n    #![allow(\n        clippy::disallowed_methods,\n        reason = "legacy"\n    )]\n}\n'; } \
      > "$tmp/multiallow/autumn/src/gated.rs"
    expect_fail "$tmp/multiallow" "re-permits" "multi-line inner allow"

    # Same, with `expect` instead of `allow` (previously untested entirely).
    make_fixture "$tmp/multiexpect"
    { printf '%s' "$GOOD_HEADER"
      printf 'pub mod inner {\n    #![expect(\n        clippy::disallowed_methods,\n        reason = "legacy"\n    )]\n}\n'; } \
      > "$tmp/multiexpect/autumn/src/gated.rs"
    expect_fail "$tmp/multiexpect" "re-permits" "multi-line inner expect"

    # A space before the paren: `#![allow (…)]`. Rust tokenizes it fine; the old
    # literal `*'allow('*` glob did not.
    make_fixture "$tmp/spacedallow"
    { printf '%s' "$GOOD_HEADER"; printf '#![allow (clippy::disallowed_methods)]\n'; } \
      > "$tmp/spacedallow/autumn/src/gated.rs"
    expect_fail "$tmp/spacedallow" "re-permits" "inner allow with a space before the paren"

    # `#[cfg(test)]` on a struct FIELD must not exempt the code that follows.
    # `autumn/src/scheduler.rs` has exactly this shape, and it used to exempt the
    # next 69 lines of production code from the anti-spoof scan.
    make_fixture "$tmp/cfgfield"
    { printf '%s' "$GOOD_HEADER"
      printf 'struct S {\n    a: u8,\n    #[cfg(test)]\n    b: u8,\n}\n'
      printf 'impl S {\n    #![allow(clippy::disallowed_methods)]\n}\n'; } \
      > "$tmp/cfgfield/autumn/src/gated.rs"
    expect_fail "$tmp/cfgfield" "re-permits" "#[cfg(test)] on a struct field must not exempt what follows"

    # A brace inside a string literal inside `#[cfg(test)] mod tests` must not
    # extend the exemption past the end of the module.
    make_fixture "$tmp/bracestring"
    { printf '%s' "$GOOD_HEADER"
      printf '#[cfg(test)]\nmod tests {\n    fn t() { let _ = "{"; }\n}\n'
      printf '#![allow(clippy::disallowed_methods)]\n'; } \
      > "$tmp/bracestring/autumn/src/gated.rs"
    expect_fail "$tmp/bracestring" "re-permits" "brace inside a string cannot extend the cfg(test) exemption"

    # A paren inside a `reason` string must not desynchronise the attribute
    # joiner and hide the NEXT attribute from the hygiene check.
    make_fixture "$tmp/parenstring"
    { printf '%s' "$GOOD_HEADER"
      printf '#[allow(clippy::disallowed_methods, reason = "see foo(")]\nfn a() {}\n'
      printf '#[allow(clippy::disallowed_methods)]\nfn b() {}\n'; } \
      > "$tmp/parenstring/autumn/src/gated.rs"
    expect_fail "$tmp/parenstring" "without a non-empty reason" "paren inside a reason string cannot hide a later allow"

    # A lifetime must not be mistaken for a char literal (which would swallow
    # the rest of the file and blind every check after it).
    make_fixture "$tmp/lifetime"
    { printf '%s' "$GOOD_HEADER"
      printf 'pub struct W<%sa> { s: &%sa str }\n' "'" "'"
      printf '#![allow(clippy::disallowed_methods)]\n'; } \
      > "$tmp/lifetime/autumn/src/gated.rs"
    expect_fail "$tmp/lifetime" "re-permits" "a lifetime must not blind the scanner"

    # A legitimate `#[cfg(test)] mod tests` still exempts its own contents even
    # when it contains braces in strings and lifetimes.
    make_fixture "$tmp/realtests"
    { printf '%s' "$GOOD_HEADER"
      printf '#[cfg(test)]\nmod tests {\n    #![allow(clippy::disallowed_methods)]\n    fn t() { let _ = "}}{{"; }\n}\n'; } \
      > "$tmp/realtests/autumn/src/gated.rs"
    expect_pass "$tmp/realtests" "a real cfg(test) mod is still exempt"

    self_test_tail "$tmp"

    echo "determinism gate: self-test OK"
  )
fi

if [[ "$mode" != "--self-test" ]]; then
  run_all_checks "$root"
  echo "determinism gate: ${#GATED_MODULES[@]} modules gated (floor $MODULE_COUNT_FLOOR); config, headers, and escape hatches OK"
fi
