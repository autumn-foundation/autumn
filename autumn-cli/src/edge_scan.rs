//! Source scan for `#[edge]` route markers and `edge_routes![]` registrations
//! (issue #1790).
//!
//! `autumn build` and `autumn doctor` both need to know, *without compiling the
//! project*, whether it has any edge-eligible routes: the build needs it to
//! decide whether to emit an edge capsule at all, and doctor needs it to run its
//! `edge_target` / `edge_routes` preflights in a project whose dependencies may
//! not even be fetched yet. Compiling to find out would defeat the purpose of a
//! preflight, so — exactly like `autumn a11y verify`, `autumn i18n check`, and
//! `autumn lifecycle check` — this is a source scanner: it parses each `.rs`
//! file under `src/` with `syn` and inspects the attributes each function
//! carries, and it tokenizes the same file to find `edge_routes![...]`
//! invocations and the handler identifiers they register.
//!
//! It records, per marked function: its name, the file and line it is declared
//! at, and whether the *same* function also carries one of the auth/guard
//! attributes (`#[secured]`, `#[authorize]`, `#[step_up]`, `#[throttle]`) that
//! the `#[edge]` macro rejects — the edge capsule has no session or auth state,
//! so that combination is a compile error, and catching it here turns a failed
//! build into a doctor line.
//!
//! **Recognition limits.** Like its sibling scanners this is a best-effort
//! textual pass, not a name resolver:
//!
//! - `#[edge]` is recognized under a bare `#[edge]` / `#[edge(needs(kv))]` path
//!   and under any qualified path whose **last** segment is `edge` (so
//!   `#[autumn_web::edge]` and `#[macros::edge]` are recognized). An `use ...
//!   ::edge as e;` rename is **not** resolved, so `#[e]` is invisible to the
//!   scan.
//! - Guard attributes are matched the same way (last path segment), on the same
//!   function only. A guard applied by an enclosing module or a layer is not
//!   visible here — that is the macro's and the router's job, not this scan's.
//! - `edge_routes![...]` is recognized as the identifier `edge_routes` followed
//!   by `!` and a delimited group, anywhere in the file (item position, inside a
//!   function body, nested in another macro's group). A re-exported alias of the
//!   macro under a different name is not recognized. Each comma-separated entry
//!   keeps its full path text, so `edge_routes![handlers::greet]` registers
//!   `handlers::greet`. A qualifier that is a `use`-introduced alias
//!   (`use crate::handlers as h;` then `edge_routes![h::show]`) is matched
//!   literally as written (`h::show`), the same "not a name resolver"
//!   limit as the `#[edge]`-rename case above — this scan does not track
//!   `use` at all, so an alias reports as an extra false-positive
//!   "unregistered" warning rather than resolving to the real path (Codex
//!   review on #2739, round 18, P2). A `pub use crate::handlers::*;`
//!   glob re-export — `pub mod api { pub use crate::handlers::*; }`, say,
//!   making `handlers::show` also reachable as `api::show` — is the same
//!   limit in a different shape: `edge_routes![my_app::api::show]` compares
//!   `api` against `show`'s own recorded module path (`handlers`), not
//!   against every path a re-export might also make it reachable through,
//!   so it reports the same false-positive "unregistered" warning rather
//!   than resolving the re-export (Codex review on #2739, round 22, P2 —
//!   investigated, not applied: tracking arbitrary `pub use` re-exports
//!   (glob, selective, renamed, chained through several modules) is a
//!   name-resolution feature, not a textual scan, and any partial version
//!   risks the false-positive-credit direction this whole module exists to
//!   avoid for a pattern the "not a name resolver" limit already covers in
//!   spirit).
//!   A leading `self` or `super` in an entry is resolved
//!   against the *inline* module the invocation itself is written in
//!   (`self::show` inside `mod users { ... }` resolves to `users::show`)
//!   before matching; an out-of-line invocation site (inside a separate
//!   `mod x;` file) resolves `self`/`super` against that file's own
//!   location instead, the same way a function's module path is derived.
//! - A registration is matched against a function by name, plus the chain of
//!   modules the scan saw the function declared under: *inline* (`mod users {
//!   #[edge] fn show() {} }` gives `show` the path `users`), or derived from
//!   the declaring file's own location for an *out-of-line* `mod users;`
//!   (`src/users.rs` also gives `users`, following Rust's directory-mirrors-
//!   modules convention) — except under `src/bin/`, where each `<name>.rs`
//!   or `<name>/main.rs` is its own separate Cargo crate root, not a
//!   submodule of the app's library crate. A bare entry (`greet`, no `::`)
//!   matches a function with that name in any module — the old, lenient
//!   rule. A qualified entry (`users::show`, or `crate::users::show` /
//!   `my_crate::users::show` with the `crate::` / crate-name prefix stripped
//!   first) matches only a function whose own module path is exactly
//!   `users` — including a crate-root function (an empty module path),
//!   which only a `crate` or crate-name qualifier (or none) can match. A
//!   *relative* qualified entry (`v1::show` written inside `mod api { mod
//!   v1 { ... } } }`, meaning `api::v1::show` in real Rust) is not resolved
//!   — genuinely ambiguous without full symbol resolution — so it reports
//!   as unregistered, the documented safe direction. A bare or `crate`- (not
//!   crate-name-) qualified entry additionally never crosses a `src/bin/`
//!   crate boundary: it must be written in the same crate as the function it
//!   names, since real Rust never resolves a bare name or `crate::` into a
//!   different crate either — a `use` import that brings a library item into
//!   a capsule bin's scope by a bare name is invisible to this scan, so that
//!   pattern also reports as unregistered.
//! - Only free functions are scanned, including those declared in inline
//!   modules (`mod routes { ... }`) or out-of-line ones (`mod routes;`
//!   backed by `src/routes.rs` or `src/routes/mod.rs`). A function generated
//!   by another macro, or declared in a file the scan does not reach, is
//!   invisible. The file-derived module path is a location heuristic, not
//!   name resolution: it does not follow a `#[path = "..."]` attribute that
//!   points a module at a differently-named file, and nested out-of-line
//!   modules under an inline one are not stitched together.
//! - `#[cfg(...)]` is evaluated, but only a safe, narrow slice of it. The scan
//!   reads the crate's own `Cargo.toml`, follows `[features] default = [...]`
//!   plus any features the caller explicitly requested (`autumn build
//!   --features x` — see [`resolve_edge_scan_with_extra_file`]) through the
//!   feature graph in `[features]`, and builds the set of feature names this
//!   build turns on. It then checks each `#[cfg(...)]` on the same function
//!   as `#[edge]` (real Rust requires every one of them to hold, so several
//!   such attributes combine the same way), AND every *inline* `mod x { ... }`
//!   enclosing it — real Rust strips the whole module along with everything
//!   in it, so this scan does too, rather than only ever looking at a
//!   function's own attributes. An *out-of-line* `mod x;`'s own `#[cfg(...)]`
//!   is not seen this way: that module's file is scanned independently (see
//!   above), with no link back to the declaration that named it, so a
//!   handler under a disabled out-of-line module can still wrongly stay in
//!   the scan — the one shape of this problem left unsolved, since fixing it
//!   needs a real cross-file module tree this scanner does not build. A
//!   predicate built only from `feature = "x"` leaves combined with
//!   `not(...)`, `all(...)`, and `any(...)` is evaluated against that set,
//!   and the function (or module) is excluded only when such a predicate is
//!   definitely false. Any other predicate —
//!   `target_os`, `debug_assertions`, a feature implied by an optional
//!   dependency, or one this scan cannot parse — is treated as true, so the
//!   function stays in the scan. `autumn build` has no `--no-default-features`
//!   flag, so that case does not arise; workspace feature unification (a
//!   sibling crate elsewhere in the build turning on a feature this scan does
//!   not know about) is not considered.
//! - A file that does not parse is skipped silently (it will fail the build with
//!   a far better message than this scanner could produce).
//!
//! Every consequence of a miss is conservative: a missed marker means the edge
//! capsule step is skipped (the native build is unaffected), and a missed
//! registration means at most a warning that names the function.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// Attribute names that cannot coexist with `#[edge]`: the edge capsule has no
/// session, no auth state, and no rate-limit store, and `#[intercept]` layers
/// are origin-only tower middleware that never run in the capsule, so the
/// `#[edge]` macro rejects each of these on the same handler.
pub const EDGE_GUARD_ATTRS: &[&str] = &["secured", "authorize", "step_up", "throttle", "intercept"];

/// One `#[edge]`-marked handler function found in the project's sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeFn {
    /// The function's name — also the identifier `edge_routes![]` registers it by.
    pub name: String,
    /// Path of the declaring file, relative to the scan root (`src/routes/home.rs`).
    pub file: String,
    /// 1-based line of the function's declaration.
    pub line: usize,
    /// Guard attributes carried by the same function, in source order
    /// (a subset of [`EDGE_GUARD_ATTRS`]).
    pub guards: Vec<String>,
    /// Names of the modules the function is declared under, outermost first
    /// (`mod a { mod b { ... } }` gives `["a", "b"]`; an out-of-line `mod a;`
    /// backed by `src/a.rs` gives the same, derived from the file's own
    /// location). Empty for a genuinely top-level function. Relative to
    /// [`Self::crate_root`], not necessarily to `src/` — see there.
    pub module_path: Vec<String>,
    /// Which Cargo crate this function's own module path is relative to:
    /// `""` for the app's library crate (anything under `src/` outside
    /// `src/bin/`), or `"bin:<name>"` for a `[[bin]]` target's own separate
    /// crate. Two functions can share an identical `module_path` (both
    /// crate roots, say) while genuinely belonging to different crates —
    /// the scan's own matching logic treats this as part of the match key
    /// precisely so a registration written in one crate can never silently
    /// credit a same-named function root in another (Codex review on #2739,
    /// round 7, P2).
    pub crate_root: String,
}

impl EdgeFn {
    /// `name @ file:line`, the form used in build warnings and doctor details.
    #[must_use]
    pub fn location(&self) -> String {
        format!("{} @ {}:{}", self.name, self.file, self.line)
    }
}

/// Result of scanning a project's sources for edge routes.
#[derive(Debug, Clone, Default)]
pub struct EdgeScan {
    /// Every `#[edge]`-marked function, in scan order (files sorted by path).
    pub functions: Vec<EdgeFn>,
    /// `(crate_root, path text)` for each `edge_routes![...]` entry: `crate_root`
    /// is the crate the invocation itself was written in (see
    /// [`EdgeFn::crate_root`]) and the text is the entry as written, with a
    /// leading `self`/`super` resolved against the invocation's own module.
    /// Nothing outside this module reads it directly, only through
    /// [`EdgeScan::unregistered`] and [`EdgeScan::registered_fns`].
    pub registered: BTreeSet<(String, String)>,
    /// Number of `edge_routes![...]` invocations seen (0 means the app never
    /// registers its marked handlers, so the capsule would serve nothing).
    pub registrations: usize,
    /// Number of `.rs` files read.
    pub files_scanned: usize,
    /// The scanned crate's own Rust library-crate identifier (hyphens
    /// converted to underscores, and a `[lib] name` override honored),
    /// when its manifest could be read and parsed. Lets a registration
    /// written as `my_crate::show` (Rust's own rule: a crate's name is also
    /// a valid path root to its own items, same as `crate::show`) match a
    /// function that would otherwise look like a reference to an unrelated
    /// `my_crate` module.
    pub crate_name: Option<String>,
}

impl EdgeScan {
    /// `true` when the project has no `#[edge]`-marked function at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// Names of every marked function, in scan order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.functions.iter().map(|f| f.name.as_str()).collect()
    }

    /// Marked functions that no `edge_routes![...]` invocation registers — they
    /// compile, but never reach the capsule's router.
    #[must_use]
    pub fn unregistered(&self) -> Vec<&EdgeFn> {
        self.functions
            .iter()
            .filter(|f| !is_registered(f, &self.registered, self.crate_name.as_deref()))
            .collect()
    }

    /// Marked functions that an `edge_routes![...]` invocation registers — the
    /// routes the capsule actually serves.
    #[must_use]
    pub fn registered_fns(&self) -> Vec<&EdgeFn> {
        self.functions
            .iter()
            .filter(|f| is_registered(f, &self.registered, self.crate_name.as_deref()))
            .collect()
    }

    /// Marked functions that also carry an auth/guard attribute — a combination
    /// the `#[edge]` macro rejects at compile time.
    #[must_use]
    pub fn guarded(&self) -> Vec<&EdgeFn> {
        self.functions
            .iter()
            .filter(|f| !f.guards.is_empty())
            .collect()
    }
}

/// `true` when some `edge_routes![...]` entry in `registered` names `f`.
///
/// An entry's last `::` segment must equal `f.name`. A bare entry (no `::`)
/// matches `f` regardless of module OR crate — the scanner's long-standing
/// lenient rule for unqualified names. A same-crate-only reading might look
/// more precise (a plain name in real Rust only ever resolves within the
/// crate it is written in, with no qualifier), but this scanner never tracks
/// `use` imports (see the module doc's "Recognition limits"), so a bare name
/// can equally well be an item a `use other_crate::x::*;` pulled in from
/// elsewhere — restricting the match to the entry's own recorded crate turned
/// that ordinary, valid pattern into a false "unregistered" scan, which
/// `run_edge_capsule_build` then treats as a hard, build-blocking error, not
/// a soft warning, when it is the invocation's only route (Codex review on
/// #2739, round 10, P2, partially reverting round 7's tightening). A
/// *qualified* entry is different: `crate::`/crate-name-qualified and plain
/// module-qualified entries below still require the same-crate match, since
/// their qualifier's own crate membership is unambiguous from how it is
/// written — crossing it really would be wrong, not merely unresolved.
/// A qualified entry matches only when its qualifier equals `f.module_path`
/// exactly, crate-root prefix stripped (see below). `f.module_path` is
/// always the scan's real answer, never a placeholder for "unknown":
/// [`crate_context_from_file`] derives it from the declaring file's own
/// location, so an empty path means the function is genuinely declared at
/// its crate's root (`src/lib.rs`, or `src/bin/<name>.rs`'s own separate
/// crate), not that the scan lost track of it. A qualified entry therefore
/// does not match a crate-root function unless the qualifier is empty too
/// (after the `crate` prefix is stripped) — the old rule of treating an
/// empty path as "match any qualifier" would let an unrelated same-named
/// function elsewhere silently satisfy a registration meant for the
/// crate-root one.
///
/// Exact equality, not a suffix match, is deliberate: with sibling modules
/// `mod a { mod x { #[edge] fn f() {} } }` and `mod b { mod x { #[edge] fn
/// f() {} } }`, a suffix match on `x::f` from anywhere would satisfy both,
/// silently muting the real warning for whichever one was not actually
/// registered. A relative reference such as `edge_routes![v1::greet]`,
/// written from inside an ancestor of the real `greet`'s module, is the
/// same trade-off in a different shape and is left unresolved by
/// [`registration_candidates`] for the same reason — see its own doc for
/// why trying it as a second candidate was reverted.
///
/// A leading `crate` segment in the qualifier is stripped first, and — since
/// `crate` never reaches outside the crate it is written in, unlike a bare
/// name — requires `f.crate_root` to equal the entry's own recorded crate.
/// A leading segment
/// equal to `crate_name` (the scanned crate's own Rust library-crate
/// identifier, when known — see [`rust_crate_name_from_manifest`]) is
/// stripped the same way, but targets the library crate specifically
/// (`f.crate_root` must be empty) regardless of which crate the entry
/// itself was written in — Rust's own rule that a crate's name is a valid
/// path root to its items from anywhere, including a `[[bin]]` target
/// referencing the library crate it links against. Without requiring
/// `f.crate_root` equality (or emptiness) at all, `edge_routes![crate::show]`
/// written inside `src/bin/edge-capsule.rs` and a same-named `show` at
/// `src/lib.rs`'s own root — two different crates, but the same empty
/// `module_path` — would be indistinguishable, silently crediting whichever
/// one this function happened to see first (Codex review on #2739, round 7,
/// P2).
///
/// The crate-name reading is applied unconditionally whenever the leading
/// segment matches, without checking whether the invocation's own crate
/// ALSO happens to declare a local module of that same name — real Rust
/// would resolve an unqualified `my_app::show` to such a local module
/// first (ordinary name shadowing takes priority over the extern-crate
/// reading), only reaching the library crate via the explicitly-anchored
/// `::my_app::show`. A project that names a local module identically to
/// its own package is the same "not a name resolver" limit as an
/// unresolved `use`-alias (see the module doc's "Recognition limits"): this
/// scanner doesn't track what else is in scope at the invocation site, so
/// it cannot tell the two readings apart, and deliberately does not guess
/// — guessing risks exactly the false-positive-credit direction this
/// function's own crate-root/crate-name checks exist to avoid, for a
/// pattern rare enough (and confusing enough as real code) that resolving
/// it isn't worth that risk (Codex review on #2739, round 20, P2).
///
/// The crate-name comparison strips a leading `r#` from the leading segment
/// before comparing it to `crate_name`. A package name that collides with a
/// Rust keyword (`name = "type"`) compiles to a crate identifier that is
/// only legal in path position spelled as a raw identifier
/// (`edge_routes![r#type::show]`); `proc_macro2::Ident::to_string()` keeps
/// that `r#` prefix verbatim (verified directly — it is not a hypothetical),
/// but `crate_name` itself comes from the manifest's own package-name text
/// via `rust_crate_name_from_manifest`, which never carries one. Comparing
/// the two spellings unnormalized always missed the library crate through
/// this branch (Codex review on #2739, round 22, P2).
///
/// The final module-path equality strips a leading `r#` from EVERY
/// remaining segment on both sides, for the same reason: a conventional
/// `mod r#type;` loads `src/type.rs` (verified directly — Rust's own
/// file-system convention strips the escape), so [`crate_context_from_file`]
/// derives module path `["type"]` for that file, while a registration
/// written as `edge_routes![my_app::r#type::show]` tokenizes its qualifier
/// with the `r#` intact. An INLINE `mod r#type { ... }`, by contrast, is
/// tracked via [`scan_items`]'s own `item_mod.ident.to_string()`, which
/// keeps the `r#` in `f.module_path` too — so which side (or both, or
/// neither) actually carries the prefix depends on how that module was
/// discovered, and stripping it from both makes the comparison correct
/// either way, since `r#foo` and `foo` never name different identifiers —
/// the escape is a purely syntactic affordance for using a keyword as a
/// name, not part of the name itself (Codex review on #2739, round 22, P2).
fn is_registered(
    f: &EdgeFn,
    registered: &BTreeSet<(String, String)>,
    crate_name: Option<&str>,
) -> bool {
    registered.iter().any(|(written_in, entry)| {
        let (qualifier, name) = entry.rsplit_once("::").unwrap_or(("", entry.as_str()));
        if name != f.name {
            return false;
        }
        if qualifier.is_empty() {
            return true;
        }
        let mut qualifier_segments = qualifier.split("::");
        match qualifier_segments.clone().next() {
            Some("crate") => {
                if *written_in != f.crate_root {
                    return false;
                }
                qualifier_segments.next();
            }
            Some(seg) if crate_name.is_some_and(|name| name == strip_raw_prefix(seg)) => {
                if !f.crate_root.is_empty() {
                    return false;
                }
                qualifier_segments.next();
            }
            _ => {
                if *written_in != f.crate_root {
                    return false;
                }
            }
        }
        qualifier_segments.map(strip_raw_prefix).eq(f
            .module_path
            .iter()
            .map(|segment| strip_raw_prefix(segment)))
    })
}

/// Strip a leading `r#` from a Rust path segment. `r#foo` and `foo` never
/// name different identifiers — the escape is a purely syntactic affordance
/// for using a keyword as a name, not part of the name itself — but
/// `syn`/`proc_macro2::Ident::to_string()` keeps it verbatim (verified
/// directly), while a file-system-derived module path
/// ([`crate_context_from_file`], [`module_path_from_segments`]) never
/// carries one (the file itself is never actually named with a literal
/// `r#` prefix). [`is_registered`] uses this to compare the two spellings
/// as equal (Codex review on #2739, round 22, P2).
fn strip_raw_prefix(segment: &str) -> &str {
    segment.strip_prefix("r#").unwrap_or(segment)
}

/// Scan a set of in-memory `(file, source)` pairs, given the set of feature
/// names enabled for this build — see [`enabled_features_from_manifest`] —
/// used to evaluate `#[cfg(...)]` on `#[edge]`-marked functions.
///
/// `#[cfg(test)]`: nothing outside a test build calls this. Production code
/// (`resolve_edge_scan_with_extra_file`) scans files straight from disk via
/// [`scan_source`]/[`scan_source_with_context`] instead of collecting them
/// into an in-memory list first, so it can special-case one file (a custom
/// `[lib] path`) without this helper's uniform per-file treatment getting in
/// the way (Codex review on #2739, round 20, P2).
#[cfg(test)]
fn scan_sources_with_features(
    sources: &[(&str, &str)],
    default_features: &BTreeSet<String>,
) -> EdgeScan {
    let mut scan = EdgeScan::default();
    for (file, src) in sources {
        scan_source(file, src, None, default_features, &mut scan);
        scan.files_scanned += 1;
    }
    scan
}

/// Scan a set of in-memory `(file, source)` pairs, treating every feature as
/// off (so a `#[cfg(feature = "...")]`-gated `#[edge]` function is excluded
/// unless a `not(...)` around it makes that resolve to true). Most unit tests
/// in this module — and in `build.rs`'s and `doctor.rs`'s own test modules —
/// drive this directly with inline source strings; a test that exercises
/// `#[cfg(...)]` evaluation against a specific feature set uses
/// [`scan_sources_with_features`] instead.
///
/// `#[cfg(test)]`: nothing outside a test build calls this — production code
/// goes through [`resolve_edge_scan_with_extra_file`], which needs the real
/// feature set.
#[cfg(test)]
#[must_use]
pub fn scan_sources(sources: &[(&str, &str)]) -> EdgeScan {
    scan_sources_with_features(sources, &BTreeSet::new())
}

/// [`resolve_edge_scan_with_extra_file`] with no explicitly-requested
/// features and no extra file. Every real caller now passes its own
/// requested-features list (`build.rs`'s `--features x`, `doctor.rs`'s
/// always-empty `&[]`) and its own resolved capsule bin, so this convenience
/// wrapper is test-only — like [`scan_sources`], nothing outside a test
/// build calls it.
#[cfg(test)]
#[must_use]
fn resolve_edge_scan(project_root: &Path) -> EdgeScan {
    resolve_edge_scan_with_extra_file(project_root, &[], None)
}

/// The ordinary `src/` walk's own file list, filtered against what the
/// custom `[lib] path`/capsule `[[bin]] path` trees already scanned:
/// `claimed` (from a custom `[lib] path`) is always excluded — that tree
/// IS the one-and-only library, so there is no separate identity left to
/// credit. `capsule_claimed` is more nuanced: a file the CAPSULE tree
/// touched is not always capsule-exclusive, since real Rust independently
/// compiles the same physical file twice when the capsule reaches it via
/// an EXPLICIT `#[path]` alias to an otherwise-ordinary library file
/// (`#[path = "../handlers.rs"] mod local_handlers;` in a capsule root,
/// where `src/handlers.rs` is ALSO the library's own `mod handlers;`
/// target) — verified directly via a real build. Unconditionally excluding
/// it, as this scan previously did, dropped the library's own separate
/// registration match entirely.
///
/// The exception in `capsule_aliased` is scoped to files reached via an
/// explicit `#[path]` alias specifically, never the tree's own root or a
/// CONVENTIONALLY resolved submodule: those two stay unconditionally
/// excluded, preserving round 22's fix, where a capsule's own
/// conventionally-reached `src/bin/` submodule must NOT also be scanned
/// under the ordinary walk's `src/bin/` heuristic's own (there, phantom)
/// identity for that same file (Codex review on #2739, round 44, P2).
fn ordinary_walk_sources(
    files: &[PathBuf],
    project_root: &Path,
    claimed: &BTreeSet<PathBuf>,
    capsule_claimed: &BTreeSet<PathBuf>,
    capsule_aliased: &BTreeSet<PathBuf>,
) -> Vec<(String, String)> {
    files
        .iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if claimed.contains(path.as_path()) {
                return None;
            }
            if capsule_claimed.contains(path.as_path()) && !capsule_aliased.contains(path.as_path())
            {
                return None;
            }
            let src = std::fs::read_to_string(path).ok()?;
            Some((rel, src))
        })
        .collect()
}

/// Scans each of `sources` under its own identity — an entry in
/// `path_overrides`, when this file has one, otherwise
/// [`crate_context_from_file`]'s ordinary directory-guessed identity.
///
/// A capsule-aliased file (`capsule_aliased`, from
/// [`ordinary_walk_sources`]'s own doc) is scanned here for its SEPARATE,
/// genuinely-independent library identity specifically, bypassing
/// `path_overrides` even when it has an entry for this exact file:
/// `path_overrides` was built by walking EVERY file under `src/`,
/// including the capsule's own tree, so it ALSO independently recorded
/// this exact file's override under the capsule's OWN identity (the same
/// one `scan_bin_crate_tree` already credited it under, via `scan`'s
/// caller). Consulting it here would credit that same wrong
/// (capsule-relative) identity a second time instead of the ordinary
/// walk's own natural guess (Codex review on #2739, round 44, P2).
fn scan_ordinary_sources(
    sources: &[(String, String)],
    project_root: &Path,
    table: Option<&toml::Table>,
    path_overrides: &BTreeMap<String, Vec<(String, Vec<String>)>>,
    capsule_aliased: &BTreeSet<PathBuf>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) {
    let capsule_aliased_rel: BTreeSet<String> = capsule_aliased
        .iter()
        .map(|path| {
            path.strip_prefix(project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    for (rel, src) in sources {
        let path_override = if capsule_aliased_rel.contains(rel.as_str()) {
            None
        } else {
            path_overrides.get(rel.as_str())
        };
        if let Some(identities) = path_override {
            for (crate_root, module_path) in identities {
                scan_source_with_context(
                    rel,
                    src,
                    crate_root,
                    module_path.clone(),
                    default_features,
                    scan,
                );
            }
        } else {
            scan_source(rel, src, table, default_features, scan);
        }
    }
}

/// Shared implementation behind [`resolve_edge_scan_with_extra_file`] (its
/// own doc has the full picture — feature resolution, the `src/` walk, the
/// custom-path cases): read the manifest once, resolve
/// BOTH a custom `[lib] path` and a custom edge-capsule `[[bin]] path` (when
/// either is given/present) to their own crate-tree scan, then fill in the
/// rest from the ordinary `src/` walk. Sharing this is what makes the two
/// custom-path features compose — a project can use a custom library path
/// and a custom capsule path at once, and each gets scanned with its own
/// crate identity regardless of the other (Codex review on #2739, round 21,
/// P2, after `resolve_edge_scan_with_extra_file` grew its own separate copy
/// of this logic in round 13 that a later round's `[lib] path` fix never
/// reached).
fn resolve_edge_scan_impl(
    project_root: &Path,
    requested_features: &[&str],
    capsule_bin_file: Option<&Path>,
) -> EdgeScan {
    // A missing or unparseable Cargo.toml yields no default features, which is
    // the safe direction: every `#[cfg(feature = "...")]` then stays
    // unresolved, and the function it gates stays in the scan.
    let manifest = std::fs::read_to_string(project_root.join("Cargo.toml")).ok();
    let table = manifest
        .as_deref()
        .and_then(|manifest| toml::from_str::<toml::Table>(manifest).ok());
    let resolver_v1 = resolver_v1_is_in_effect(project_root, table.as_ref());
    let default_features = manifest
        .as_deref()
        .map(|manifest| {
            enabled_features_from_manifest_for_resolver(manifest, requested_features, resolver_v1)
        })
        .unwrap_or_default();
    let crate_name = table.as_ref().and_then(rust_crate_name_from_manifest);
    let custom_lib_path = table.as_ref().and_then(custom_lib_path_from_manifest);

    let mut scan = EdgeScan {
        crate_name,
        ..EdgeScan::default()
    };
    let mut claimed: BTreeSet<PathBuf> = BTreeSet::new();
    let mut capsule_claimed: BTreeSet<PathBuf> = BTreeSet::new();

    // A custom `[lib] path` (e.g. `src/app.rs`, `src/custom/app.rs`, or even
    // `lib/app.rs` outside `src/` entirely) IS this crate's library root, the
    // same way a custom `[[bin]] path` is its own bin's root (see
    // `resolve_edge_scan_with_extra_file`'s doc). Always scanned via
    // `scan_bin_crate_tree`, regardless of whether it lives inside `src/`:
    // an in-`src/` root's own out-of-line submodules (`mod routes;` next to
    // it) still need real Rust module-path resolution relative to the
    // root's own directory, not `crate_context_from_file`'s
    // directory-mirrors-module heuristic, which would invent an extra
    // leading segment from the root's enclosing directory name (`custom`)
    // that is not a real module at all (Codex review on #2739, round 22,
    // P2 — round 21's own fix only corrected the root file's own identity,
    // never its submodules').
    if let Some(lib_path) = &custom_lib_path {
        let lib_file = project_root.join(lib_path);
        if lib_file.is_file() {
            let (touched, _aliased) = scan_bin_crate_tree(
                &lib_file,
                project_root,
                "",
                Vec::new(),
                &default_features,
                &mut scan,
            );
            claimed.extend(touched);
        }
    }

    // The capsule's own tree — always scanned this way, even for one of
    // Cargo's own two `src/bin/` auto-discovery shapes
    // (`src/bin/<name>.rs`, `src/bin/<name>/main.rs`): those get the right
    // ROOT identity from `crate_context_from_file`'s own `src/bin/`
    // handling, but that heuristic cannot tell a real second flat bin file
    // apart from an out-of-line submodule the first one pulls in via `mod
    // routes;` from the very same `src/bin/` directory — both look
    // identical on disk. With `autobins = false` and no separate `[[bin]]`
    // entry for it, such a file is not a crate root at all, yet the
    // ordinary walk credited it with a phantom `bin:routes` identity
    // regardless, so a valid `edge_routes![crate::routes::show]` could never
    // match it (Codex review on #2739, round 22, P2 — round 21's file-shape
    // guard only ever bypassed this for a NON-conventional root, never
    // fixing this conventional-root case since the ordinary walk was
    // assumed sufficient for it).
    let mut capsule_aliased: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(file) = capsule_bin_file {
        let (touched, aliased) = scan_bin_crate_tree(
            file,
            project_root,
            "bin:edge-capsule",
            Vec::new(),
            &default_features,
            &mut scan,
        );
        capsule_claimed.extend(touched);
        capsule_aliased.extend(aliased);
    }

    let mut files = Vec::new();
    collect_rs_files(&project_root.join("src"), &mut files);

    // A custom `[lib] path` REPLACES the conventional `src/lib.rs` as this
    // crate's library root — Cargo compiles only the custom file, never
    // `src/lib.rs`, even when the latter still exists on disk with its own
    // `#[edge]` handlers — verified directly via a real build: a
    // `compile_error!` placed in an unrelated `src/lib.rs` does not fail a
    // build whose `[lib] path` points elsewhere. Left in `files`, that dead
    // `src/lib.rs` was scanned by the ordinary walk below as if it were an
    // active source, so a stale handler in it could be reported as a real,
    // uncompiled-but-unregistered route and wrongly fail preflight. Excluded
    // here whenever the manifest's custom path resolves (after normalizing
    // any `.`/`..` components the same way `scan_bin_crate_tree`'s own root
    // does) to a different file than the conventional one — the ordinary
    // `claimed` exclusion below only ever covers files the custom tree's own
    // `mod` declarations actually reach, never this now-inactive file
    // itself (Codex review on #2739, round 46, P2).
    if let Some(lib_path) = &custom_lib_path {
        let conventional_lib_file = project_root.join("src/lib.rs");
        let custom_lib_file = lexically_normalize_path(&project_root.join(lib_path));
        if custom_lib_file != conventional_lib_file {
            files.retain(|path| path != &conventional_lib_file);
        }
    }
    // Sorted so warnings, doctor details, and the build's route list are stable
    // across platforms and filesystem orderings.
    files.sort();

    // A `#[path = "..."]` override on an out-of-line `mod name;` redirects
    // which FILE backs the module without changing the module's own real
    // name/path — real Rust still resolves `name::show` (never a name
    // derived from the file), so this scan's ordinary directory-walk guess
    // (`crate_context_from_file`, which assumes file path mirrors module
    // path) needs to be corrected for exactly these files (Codex review on
    // #2739, round 31, P2).
    let path_overrides =
        path_attribute_module_paths(project_root, table.as_ref(), &default_features);

    // Read first, scan second, so the filesystem half and the pure half stay
    // separable: `scan_sources` is the same entry point the unit tests drive
    // with inline sources. An unreadable file is skipped, like the sibling
    // scanners do.
    let sources: Vec<(String, String)> = ordinary_walk_sources(
        &files,
        project_root,
        &claimed,
        &capsule_claimed,
        &capsule_aliased,
    );

    let sources_rel: BTreeSet<&str> = sources.iter().map(|(rel, _)| rel.as_str()).collect();

    scan_ordinary_sources(
        &sources,
        project_root,
        table.as_ref(),
        &path_overrides,
        &capsule_aliased,
        &default_features,
        &mut scan,
    );
    scan.files_scanned += sources.len();

    // A `#[path]` override can redirect to a file OUTSIDE `src/` entirely
    // (`#[path = "../routes.rs"] mod handlers;` in `src/lib.rs` compiles a
    // file the ordinary, `src/`-only walk above never reaches) — verified
    // directly via a real build. `path_attribute_module_paths` already
    // recorded its identity; scan it, and any of ITS OWN further out-of-line
    // children (`pub mod child;` inside that same outside file resolves
    // beside it, never back under `src/`), by reusing `scan_bin_crate_tree`'s
    // BFS with the already-recorded identity as its starting module path,
    // once per recorded identity the same way an in-`src/` override target
    // already is above, since it was never in `files`/`sources` to begin
    // with (Codex review on #2739, round 40, P1; descendant traversal added
    // round 45, P1).
    for (target_rel, identities) in &path_overrides {
        if sources_rel.contains(target_rel.as_str()) {
            continue;
        }
        let target_path = project_root.join(target_rel);
        if claimed.contains(&target_path) {
            continue;
        }
        for (crate_root, module_path) in identities {
            let (touched, _aliased) = scan_bin_crate_tree(
                &target_path,
                project_root,
                crate_root,
                module_path.clone(),
                &default_features,
                &mut scan,
            );
            claimed.extend(touched);
        }
    }
    scan
}

/// The library target's own `[lib] path` override, when the manifest sets
/// one — the file Cargo actually compiles as this crate's library root
/// instead of the conventional `src/lib.rs`.
#[must_use]
fn custom_lib_path_from_manifest(table: &toml::Table) -> Option<String> {
    table
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

/// Walk `project_root/src` and scan every `.rs` file below it, plus
/// `extra_file` — the edge-capsule bin's own resolved root file, scanned as
/// its own `[[bin]]` crate tree (root file plus every out-of-line submodule
/// it transitively declares) rather than as part of the library. Paths are
/// recorded relative to `project_root` (`src/routes/home.rs`). A missing
/// `src/` yields an empty scan — a project without sources simply has no
/// edge routes.
///
/// `requested_features` are feature names asked for on the command line
/// (`autumn build --features x`, forwarded here from `build.rs`), on top of
/// the manifest's own `[features] default = [...]`. Without them, a route
/// gated on a non-default feature the caller explicitly requested would look
/// cfg'd-out to this scan even though the real build the caller is about to
/// run turns it on — exactly the dangerous direction (a route that will
/// really be served, silently missing from the scan) this module's `#[cfg]`
/// evaluation is designed to never risk.
///
/// A project may declare its edge-capsule bin at a custom `[[bin]] path`
/// (`path = "cmd/edge.rs"`, or even `path = "src/edge.rs"` — inside `src/`
/// but outside the conventional `src/bin/`). The library's `src/` walk
/// cannot give such a file the right identity on its own: it would either
/// never reach it at all (outside `src/`) or credit it to a fictitious
/// library module named after the file (inside `src/`, since only the
/// `src/bin/` prefix specifically is recognized as a `[[bin]]` crate root).
/// Either way, a `edge_routes![...]` call or a bare `crate::`-qualified
/// registration written there could look invisible or wrongly scoped to
/// `autumn doctor`, even though the real build serves the routes fine
/// (Codex review on #2739, round 7 and round 13, P2). `extra_file` is a
/// no-op only when `None` — even Cargo's own two `src/bin/` auto-discovery
/// shapes (`src/bin/<name>.rs`, `src/bin/<name>/main.rs`) still need this
/// treatment for their own out-of-line submodules: the ordinary `src/` walk
/// gives the ROOT the right identity there, but cannot tell a real second
/// flat bin file under `src/bin/` apart from a submodule the first one pulls
/// in via `mod routes;` from that same directory (Codex review on #2739,
/// round 22, P2). A custom `[lib] path` is handled the same way regardless
/// of `extra_file` — see [`resolve_edge_scan_impl`]'s own doc.
#[must_use]
pub fn resolve_edge_scan_with_extra_file(
    project_root: &Path,
    requested_features: &[&str],
    extra_file: Option<&Path>,
) -> EdgeScan {
    resolve_edge_scan_impl(project_root, requested_features, extra_file)
}

/// The scanned crate's own `[package] name`, when the manifest table parses
/// far enough to say. Used only by
/// [`enabled_features_from_manifest_for_resolver`], which matches Cargo's
/// own `-p <package>` / `--features pkg/feat` CLI syntax —
/// that syntax names the *package*, hyphens and all, never the Rust crate
/// identifier `path::item` syntax uses (see [`rust_crate_name_from_manifest`]
/// for that one).
#[must_use]
fn package_name_from_manifest(table: &toml::Table) -> Option<String> {
    table
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

/// Whether `target_key` — the string inside `[target.<target_key>.dependencies]`
/// — can apply to the edge capsule's own build target,
/// [`crate::build::EDGE_TARGET`] (`wasm32-wasip1`): the one target this
/// scanner's target-specific dependency check can reliably reason about.
///
/// Verified directly against `cargo rustc -- --print cfg`: Cargo enables a
/// target-specific optional dependency's implicit feature only for a target
/// whose predicate actually holds — treating every target table as
/// unconditionally active (this scanner's prior behavior, round 8) let a
/// target that can never build the edge capsule (`cfg(windows)`, say) still
/// count as declaring an "optional dependency," crediting a
/// `#[cfg(feature = "...")]` route that Cargo never really compiles for
/// wasm32-wasip1 — a phantom route that can spuriously demand a capsule/WASI
/// target or reject `--embed` (Codex review on #2739, round 13, P2).
///
/// Only a literal `wasm32-wasip1` triple, or a `cfg(...)` predicate built
/// from a leaf naming one of `WASM32_WASIP1_CFG_VALUES`' known `(key,
/// value)` pairs — checking *membership*, since a key like
/// `target_has_atomic` legitimately has several simultaneous real values —
/// or a `target_feature = "..."` leaf (always false: unlike every other key,
/// Cargo's own dependency-table evaluator does not resolve `target_feature`
/// against a target's real values at all), combined with `not`/`all`/`any`,
/// is resolved; anything else (an OS-family shorthand like `windows`/`unix`,
/// a key this table does not list, or a predicate this scan cannot parse) is
/// treated as NOT applying. That is the opposite
/// default from function-level `#[cfg(...)]` evaluation ([`eval_cfg_attr`],
/// which stays conservative by assuming *true*): crediting an inapplicable
/// target here produces a phantom route, not a merely missed one, so
/// "cannot resolve" must mean "does not match."
#[must_use]
fn target_key_matches_edge_capsule(target_key: &str) -> bool {
    if target_key == crate::build::EDGE_TARGET {
        return true;
    }
    let Some(inner) = target_key
        .strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    syn::parse_str::<TargetCfgPredicate>(inner).is_ok_and(|pred| pred.eval())
}

/// A `cfg(...)` predicate this scan can resolve against the fixed
/// wasm32-wasip1 edge-capsule target: built only from `target_arch = "wasm32"`
/// leaves, combined with `not`, `all`, and `any` — see
/// [`target_key_matches_edge_capsule`].
enum TargetCfgPredicate {
    /// A `key = "value"` leaf this scan recognizes, already compared against
    /// wasm32-wasip1's own known value for that key.
    Leaf(bool),
    Not(Box<Self>),
    All(Vec<Self>),
    Any(Vec<Self>),
}

/// wasm32-wasip1's own value(s) for every `key = "value"` shaped `cfg(...)`
/// key this scan resolves — every such pair `rustc --print cfg --target
/// wasm32-wasip1` prints, `debug_assertions` (a bare flag with no value,
/// handled as its own always-true special case in
/// [`TargetCfgPredicate`]'s `Parse` impl instead — see round 25 there)
/// aside — verified directly against that complete output (Codex review on
/// #2739, rounds 14 through 17, each
/// extending the previous round's grammar after a real predicate it did not
/// recognize was found to evaluate as "does not match" when it actually
/// does — the exact dangerous-direction mistake this evaluator exists to
/// avoid: a route Cargo really compiles for the capsule was scanned out).
/// `panic = "abort"` (round 17) is not a target-architecture property like
/// the rest, but it is still a real, always-true cfg for this target —
/// wasm32-wasip1 always builds with `panic = "abort"` — so it belongs here
/// on the same footing. A `cfg(...)` key can list MULTIPLE
/// simultaneous values for one target — `target_has_atomic` does here — and
/// a real predicate checks *membership* in that set: `cfg(target_has_atomic
/// = "32")` is satisfied because "32" is one of five values this target
/// reports for that key, not because it is the only one. Round 15 treated a
/// multi-valued key as entirely unresolvable instead of checking
/// membership, which is why this list holds one entry per `(key, value)`
/// PAIR, not one entry per key — a key with several real values simply
/// appears several times.
///
/// Deliberately excludes `target_feature`: unlike every key here, Cargo's
/// OWN `[target.'cfg(...)'.dependencies]` evaluator does not resolve
/// `target_feature` against a target's real values at all — it is handled
/// as its own special case in [`TargetCfgPredicate`]'s `Parse` impl instead,
/// always false regardless of value or target (verified directly: a real
/// `[target.'cfg(target_feature = "crt-static")'.dependencies]` table pulls
/// in nothing for `--target wasm32-wasip1` even though rustc reports
/// `crt-static` as a real default feature there — Codex review on #2739,
/// round 23, P1).
const WASM32_WASIP1_CFG_VALUES: &[(&str, &str)] = &[
    ("target_arch", "wasm32"),
    ("target_os", "wasi"),
    ("target_family", "wasm"),
    ("target_env", "p1"),
    ("target_pointer_width", "32"),
    ("target_vendor", "unknown"),
    ("target_endian", "little"),
    ("target_abi", ""),
    ("target_has_atomic", "8"),
    ("target_has_atomic", "16"),
    ("target_has_atomic", "32"),
    ("target_has_atomic", "64"),
    ("target_has_atomic", "ptr"),
    ("panic", "abort"),
];

impl syn::parse::Parse for TargetCfgPredicate {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        // `syn::Ident`'s own `Parse` impl rejects `true`/`false` outright
        // (verified directly: `syn::parse_str::<syn::Ident>("false")` fails
        // with "expected identifier, found keyword `false`", even though
        // `proc_macro2::Ident` itself has no such restriction) — using
        // `IdentExt::parse_any` here, which has no keyword restriction,
        // is what makes the `true`/`false` branch below reachable at all
        // (Codex review on #2739, round 22, P1 — the P1 fix below was
        // otherwise dead code).
        let ident: syn::Ident = syn::ext::IdentExt::parse_any(input)?;
        // `true`/`false` tokenize as a plain `Ident` (verified directly),
        // and are Cargo's own stable constant cfg predicates — a real
        // `[target.'cfg(true)'.dependencies]` table applies unconditionally,
        // to every target including wasm32-wasip1, verified directly against
        // a real `cargo rustc --target wasm32-wasip1` build. Without this,
        // the leading-identifier parse above still succeeds (it is a valid
        // ident), but neither the known-key nor the `not`/`all`/`any`/
        // `windows`/`unix` branches below recognize it, so it fell through
        // to the final parse error — "unresolvable", which for this
        // evaluator means "does not match" — wrongly excluding a dependency
        // Cargo actually includes (Codex review on #2739, round 22, P1).
        if ident == "true" || ident == "false" {
            return Ok(Self::Leaf(ident == "true"));
        }
        // Cargo's own dependency-table cfg evaluator does not support
        // `target_feature` at all — verified directly: a real
        // `[target.'cfg(target_feature = "crt-static")'.dependencies]` table
        // pulls in nothing for `--target wasm32-wasip1` even though rustc
        // reports `crt-static` as one of that target's own default features,
        // and the `not(...)` form of the SAME predicate pulls the dependency
        // in unconditionally instead — i.e. Cargo treats every
        // `target_feature = "..."` leaf here as simply false, never true, no
        // matter the value or the real target. This scan previously checked
        // `target_feature` leaves against `WASM32_WASIP1_CFG_VALUES` the
        // same way as every other key (matching rustc's real reported
        // values, which IS how `#[cfg(target_feature = "...")]` resolves at
        // the source-code level, just not in a `[target.'cfg(...)']` table),
        // so `cfg(not(target_feature = "crt-static"))` was scanned as
        // excluded when Cargo actually includes it — omitting an active
        // `#[cfg(feature = "...")]`-gated handler that a real release build
        // still compiles (Codex review on #2739, round 23, P1).
        if ident == "target_feature" && input.peek(syn::Token![=]) {
            input.parse::<syn::Token![=]>()?;
            let _lit: syn::LitStr = input.parse()?;
            return Ok(Self::Leaf(false));
        }
        let key_is_known = WASM32_WASIP1_CFG_VALUES.iter().any(|(key, _)| ident == key);
        if key_is_known {
            if input.peek(syn::Token![=]) {
                input.parse::<syn::Token![=]>()?;
                let lit: syn::LitStr = input.parse()?;
                let value = lit.value();
                let matches = WASM32_WASIP1_CFG_VALUES
                    .iter()
                    .any(|(key, known_value)| ident == key && value == *known_value);
                return Ok(Self::Leaf(matches));
            }
            // The BARE form of a known key (`target_has_atomic`, no
            // `= "value"`) is a different predicate from any of its valued
            // forms, and always false: verified directly (a native,
            // non-wasm target has real `target_has_atomic = "..."` values,
            // yet `#[cfg(target_has_atomic)]` bare still does not compile
            // in) — rustc never emits these keys as a bare flag, only as
            // one or more `key = "value"` pairs. Previously this fell
            // through to a parse error, which failed the WHOLE enclosing
            // predicate (not just this leaf) — `not(target_has_atomic)`,
            // always true, evaluated as unresolvable-so-false instead
            // (Codex review on #2739, round 19, P1).
            return Ok(Self::Leaf(false));
        }
        if ident == "not" {
            let content;
            syn::parenthesized!(content in input);
            let inner: Self = content.parse()?;
            return Ok(Self::Not(Box::new(inner)));
        }
        if ident == "all" || ident == "any" {
            let content;
            syn::parenthesized!(content in input);
            let list =
                syn::punctuated::Punctuated::<Self, syn::Token![,]>::parse_terminated(&content)?;
            let parts: Vec<Self> = list.into_iter().collect();
            return Ok(if ident == "all" {
                Self::All(parts)
            } else {
                Self::Any(parts)
            });
        }
        // `windows` and `unix` are rustc's own built-in aliases for
        // `target_family = "windows"` / `target_family = "unix"` — bare
        // flags, no `= "value"` to parse. wasm32-wasip1's only
        // `target_family` value is "wasm" (a target has exactly one), so
        // both are always false for it; leaving them unresolvable (as
        // opposed to explicitly false) made `not(windows)` — true for this
        // target, and a real pattern for an optional dependency meant for
        // every non-Windows target including the capsule — evaluate as
        // "does not match" instead, filtering out a route Cargo genuinely
        // compiles (Codex review on #2739, round 18, P1).
        if ident == "windows" || ident == "unix" {
            return Ok(Self::Leaf(false));
        }
        // Round 20 concluded `debug_assertions` should stay unresolvable
        // (hence false) here, reasoning that the capsule always builds
        // `--release` and `-C debug-assertions=off` makes the flag genuinely
        // absent for it. That conflated two DIFFERENT Cargo pipelines:
        // real, direct verification (`cargo build --release --target
        // wasm32-wasip1 -v` on a manifest with `[target.'cfg(debug_assertions)'.dependencies]`)
        // shows the dependency compiled into the release build regardless —
        // Cargo resolves a target-table predicate's `debug_assertions`
        // using its fixed per-target cfg database (always true, the same
        // one `rustc --print cfg`'s default dev-profile-shaped query
        // reports), not the profile's actual `-C debug-assertions=off`/`on`
        // flag, which only affects how the CRATE'S OWN source-level
        // `#[cfg(debug_assertions)]` compiles, a later and separate step.
        // Treating it as unresolvable-so-false was therefore the wrong
        // direction after all: it filtered out a dependency (and the
        // `#[cfg(feature = "...")]` route behind it) that a real release
        // build genuinely includes (Codex review on #2739, round 25, P1).
        if ident == "debug_assertions" {
            return Ok(Self::Leaf(true));
        }
        // Any OTHER bare identifier (no `= "value"` following) is a custom
        // or unrecognized cfg flag this scan has no value table for — and
        // verified directly (`cargo tree --target wasm32-wasip1` on
        // `[target.'cfg(not(my_custom_flag))'.dependencies]` includes the
        // dependency; the same manifest with the bare, un-negated
        // `cfg(my_custom_flag)` does not) that Cargo treats an unknown bare
        // flag as simply absent — false, not "unparseable." Previously this
        // fell through to a parse `Err`, which fails the WHOLE enclosing
        // predicate (not just this leaf): `not(my_custom_flag)`, really
        // true since Cargo doesn't know an absent flag as anything but
        // false, evaluated as unresolvable-so-false instead — the same
        // class of mistake round 19 already fixed for
        // `target_has_atomic`'s bare form specifically, now generalized to
        // every OTHER unknown bare flag too (Codex review on #2739, round
        // 26, P1).
        if !input.peek(syn::Token![=]) {
            return Ok(Self::Leaf(false));
        }
        // A `key = "value"` pair this scan does not recognize the key for
        // gets the identical treatment, for the identical reason — verified
        // directly the same way: `[target.'cfg(not(my_key = "x"))'.…]`
        // includes the dependency for wasm32-wasip1, the bare, un-negated
        // form of the same predicate does not. Consume the `= "value"` so
        // the rest of the enclosing `not`/`all`/`any` still parses, rather
        // than erroring here and failing the WHOLE predicate the same
        // wrong way the bare-flag case did before round 26 (Codex review
        // on #2739, round 28, P1).
        input.parse::<syn::Token![=]>()?;
        input.parse::<syn::LitStr>()?;
        Ok(Self::Leaf(false))
    }
}

impl TargetCfgPredicate {
    fn eval(&self) -> bool {
        match self {
            Self::Leaf(matches) => *matches,
            Self::Not(inner) => !inner.eval(),
            Self::All(parts) => parts.iter().all(Self::eval),
            Self::Any(parts) => parts.iter().any(Self::eval),
        }
    }
}

/// Whether Cargo's *feature resolver* is version 1 for this scan.
///
/// Verified directly, not assumed: a real `cargo rustc --target
/// wasm32-wasip1 -- --print cfg` on a package with `[features] default =
/// ["foo/x"]` and `foo` declared ONLY under `[target.'cfg(windows)'.dependencies]`
/// (`optional = true`) shows `feature="foo"` active for that build under
/// resolver v1 — Cargo's own docs confirm this is the documented
/// difference: "the version `1` resolver will unify features for a package
/// no matter where it is specified," while version 2 "avoids unifying
/// features for ... platform-specific dependencies for another platform."
/// Explicitly setting `resolver = "2"` on the same manifest reproduces the
/// `TargetCfgPredicate`-filtered behavior this scan otherwise always
/// assumed. Assuming v2 for a genuinely-v1 project is the dangerous
/// direction: a `#[cfg(feature = "foo")]` route the real build turns on
/// would look cfg'd-out here (Codex review on #2739, round 22, P1).
///
/// Cargo's own resolver-selection rule, each branch verified directly:
/// - A package's own `[package] resolver` (or, absent that, its own
///   `edition`: 2015/2018 imply `"1"`, 2021/2024 imply `"2"`) decides,
///   UNLESS it is part of a workspace.
/// - A workspace governs every member regardless of the member's own
///   `resolver`/`edition`. An explicit `[workspace] resolver` wins outright.
/// - Without one, a NON-virtual workspace (it has its own `[package]`, i.e.
///   the workspace root is itself a package) defaults from THAT root
///   package's own edition, never the member's.
/// - Without one, a VIRTUAL workspace (`[workspace]`, no `[package]`)
///   always defaults to `"1"` — REGARDLESS of any member's edition. This is
///   not a rare corner case: Cargo prints its own warning for exactly this
///   shape ("virtual workspace defaulting to `resolver = \"1\"` despite one
///   or more workspace members being on edition 2021 which implies
///   `resolver = \"2\"`"), so an ordinary virtual workspace that never
///   explicitly opted in still hits this today.
///
/// [`find_ancestor_workspace_manifest`]'s search does not verify that
/// `project_root` is actually listed in that workspace's `members` — like
/// every other heuristic in this module, resolving path globs and
/// `exclude` is out of scope for a textual scan; the (rare) cost of that is
/// treating an unrelated ancestor's `[workspace]` as this project's own.
fn resolver_v1_is_in_effect(project_root: &Path, table: Option<&toml::Table>) -> bool {
    if let Some(t) = table
        && t.contains_key("workspace")
    {
        return workspace_implies_resolver_v1(t);
    }
    if let Some(workspace_table) = find_ancestor_workspace_manifest(project_root) {
        return workspace_implies_resolver_v1(&workspace_table);
    }
    let Some(package) = table
        .and_then(|t| t.get("package"))
        .and_then(toml::Value::as_table)
    else {
        return true;
    };
    if let Some(resolver) = package.get("resolver").and_then(toml::Value::as_str) {
        return resolver == "1";
    }
    edition_implies_resolver_v1(package)
}

/// `table` is known to contain a `[workspace]` key — governs every member's
/// resolver version per the rules in [`resolver_v1_is_in_effect`]'s own doc.
fn workspace_implies_resolver_v1(table: &toml::Table) -> bool {
    let workspace = table.get("workspace").and_then(toml::Value::as_table);
    if let Some(resolver) = workspace
        .and_then(|w| w.get("resolver"))
        .and_then(toml::Value::as_str)
    {
        return resolver == "1";
    }
    let Some(root_package) = table.get("package").and_then(toml::Value::as_table) else {
        return true; // virtual workspace, no explicit resolver: always "1"
    };
    root_package_edition_implies_resolver_v1(root_package, workspace)
}

/// Cargo's own default: edition 2015/2018 (or no `edition` key at all, which
/// is 2015) implies resolver v1; 2021 and 2024 imply v2.
fn edition_implies_resolver_v1(package_table: &toml::Table) -> bool {
    !matches!(
        package_table.get("edition").and_then(toml::Value::as_str),
        Some("2021" | "2024")
    )
}

/// Same rule as [`edition_implies_resolver_v1`], but for a workspace ROOT
/// package specifically: its own `edition` can be `edition.workspace =
/// true` (a TOML table, not a string) instead of a literal edition string,
/// inheriting from `[workspace.package] edition` in that SAME manifest — a
/// real, sanctioned Cargo pattern (define the edition once under
/// `[workspace.package]`, every member including the root inherits it via
/// `edition.workspace = true`), verified directly against a real build: a
/// root package that inherits edition `"2021"` this way resolves as v2, not
/// v1. `package_table.get("edition").and_then(toml::Value::as_str)` alone
/// returns `None` for a table value, which this function's simpler sibling
/// would then treat as "no edition at all" (2015, implying v1) — silently
/// wrong for a 2021/2024-edition workspace (Codex review on #2739, round
/// 22, P2). A workspace MEMBER's own `edition.workspace = true` is not
/// resolved here: only the root's is ever read from at all (see
/// [`workspace_implies_resolver_v1`]'s doc), and a member can only use that
/// syntax when it's already part of a workspace to inherit from, which
/// means it is never reached from the "standalone package" call site that
/// uses the plain [`edition_implies_resolver_v1`] instead.
fn root_package_edition_implies_resolver_v1(
    root_package: &toml::Table,
    workspace: Option<&toml::Table>,
) -> bool {
    match root_package.get("edition") {
        Some(toml::Value::String(edition)) => !matches!(edition.as_str(), "2021" | "2024"),
        Some(toml::Value::Table(inherit))
            if inherit.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
        {
            let inherited_edition = workspace
                .and_then(|w| w.get("package"))
                .and_then(toml::Value::as_table)
                .and_then(|p| p.get("edition"))
                .and_then(toml::Value::as_str);
            !matches!(inherited_edition, Some("2021" | "2024"))
        }
        _ => true, // no edition key at all defaults to 2015, implying v1
    }
}

/// Best-effort discovery of the nearest ancestor directory whose
/// `Cargo.toml` declares a `[workspace]` table that actually GOVERNS
/// `project_root` — Cargo's own workspace root, when `project_root` is a
/// member of one. Walks upward from `project_root`'s PARENT (not
/// `project_root` itself — a self-owned `[workspace]`, when the scanned
/// package is itself the workspace root, is handled directly by
/// [`resolver_v1_is_in_effect`] without needing a filesystem walk at all).
///
/// A candidate ancestor whose `[workspace] exclude` covers `project_root`
/// does NOT govern it — verified directly against a real build: nesting a
/// package under such an ancestor and giving the two conflicting resolver
/// versions (workspace `resolver = "2"`, package `edition = "2018"`, no
/// override) shows the package's OWN edition-implied `"1"` in effect, the
/// ancestor's `"2"` entirely ignored. The walk continues past an excluded
/// ancestor to search further up, the same way Cargo's own automatic
/// workspace-root discovery does (Codex review on #2739, round 22, P1 —
/// `exclude` is a real, sanctioned nested-workspace pattern, not a
/// hypothetical).
///
/// An explicit `[workspace] members` entry covering `project_root` wins
/// over an overlapping `exclude` — verified directly: `members =
/// ["crates/app"]` alongside `exclude = ["crates"]` still governs
/// `crates/app` with the workspace's own resolver, Cargo's documented
/// "members always wins" precedence for this exact overlap shape. Checked
/// first, before `exclude`, for that reason. [`any_glob_matches`] matches the
/// common shapes real manifests use for `members`/`exclude`, not the full
/// glob grammar; beyond that, this scanner still does not confirm a
/// workspace actually lists `project_root` in `members` at all (only that
/// nothing excludes it, or something explicitly includes it) — the same
/// accepted "best-effort, not a build system" trade-off as everywhere else
/// in this module.
fn find_ancestor_workspace_manifest(project_root: &Path) -> Option<toml::Table> {
    let mut dir = project_root.parent();
    while let Some(candidate) = dir {
        if let Ok(content) = std::fs::read_to_string(candidate.join("Cargo.toml"))
            && let Ok(table) = toml::from_str::<toml::Table>(&content)
            && table.contains_key("workspace")
        {
            let workspace_globs = |key: &str| -> Vec<String> {
                table
                    .get("workspace")
                    .and_then(toml::Value::as_table)
                    .and_then(|w| w.get(key))
                    .and_then(toml::Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let governed = project_root.strip_prefix(candidate).is_ok_and(|rel| {
                let rel = rel.to_string_lossy().replace('\\', "/");
                let members = workspace_globs("members");
                let member_patterns: Vec<&str> = members.iter().map(String::as_str).collect();
                // `members` never covers a subtree beneath a matched entry —
                // each entry must itself BE a package (see `any_glob_matches`).
                if any_glob_matches(&member_patterns, &rel, false) {
                    return true;
                }
                let exclude = workspace_globs("exclude");
                let exclude_patterns: Vec<&str> = exclude.iter().map(String::as_str).collect();
                // `exclude`, unlike `members`, DOES cover a whole subtree.
                !any_glob_matches(&exclude_patterns, &rel, true)
            });
            if governed {
                return Some(table);
            }
        }
        dir = candidate.parent();
    }
    None
}

/// Whether any of `patterns` (Cargo's `[workspace] members = [...]` or
/// `exclude = [...]` globs) matches `rel` — the forward-slash path from a
/// workspace root to a candidate member. Supports the shapes real manifests
/// actually use for these fields: an exact path (`"mainpkg"`) and a single
/// `*` wildcard within one segment (`"crates/*"`) always require the SAME
/// number of segments as `rel` — not the full glob grammar dedicated crates
/// support (recursive `**`, character classes, brace expansion), which these
/// fields essentially never need in practice, and in particular `*` never
/// crosses a `/` the way a recursive glob would: verified directly (`cargo
/// metadata` on `members = ["crates/*"]` with only `crates/group/app`
/// present, no `crates/group/Cargo.toml`, errors trying to load
/// `crates/group` itself as the matched member — it never even considers
/// the deeper `crates/group/app` a candidate).
///
/// `allow_subtree_match` covers the one shape that DOES extend past the
/// pattern's own segment count, and it is real only for `exclude`, verified
/// directly: a workspace with `members = ["crates/app"]` (a path dependency
/// away from `vendor/nested/pkg`, which Cargo auto-includes as an implicit
/// member) and `exclude = ["vendor"]` — a bare, one-segment, non-wildcard
/// pattern — drops `vendor/nested/pkg` from `workspace_members` entirely.
/// `members` never gets this treatment: verified directly the other way,
/// `members = ["vendor"]` alone (no wildcard, same one-segment pattern)
/// errors trying to load `vendor/Cargo.toml` as the matched package instead
/// of reaching into `vendor/nested/pkg` — `members` entries must each name
/// an actual package root, with no auto-recursion into subtrees at all.
/// Previously this scan let `pattern_segments.len() <= rel_segments.len()`
/// govern BOTH fields uniformly (truncating the comparison to the
/// pattern's own length and ignoring rel's remaining segments), so a
/// `members = ["crates/*"]` entry wrongly matched a package two levels
/// deeper, like `crates/group/app` — treating an ancestor workspace as
/// covering a package one glob-segment further than any real Cargo build
/// would ever agree it does (Codex review on #2739, round 25, P1).
fn any_glob_matches(patterns: &[&str], rel: &str, allow_subtree_match: bool) -> bool {
    let rel_segments: Vec<&str> = rel.split('/').collect();
    patterns.iter().any(|pattern| {
        let pattern_segments: Vec<&str> = pattern.split('/').collect();
        let length_matches = if allow_subtree_match {
            pattern_segments.len() <= rel_segments.len()
        } else {
            pattern_segments.len() == rel_segments.len()
        };
        length_matches
            && pattern_segments
                .iter()
                .zip(&rel_segments)
                .all(|(pattern, segment)| segment_glob_matches(pattern, segment))
    })
}

/// A single path segment against a pattern segment with at most one `*`
/// wildcard (matching any run of characters, including none).
fn segment_glob_matches(pattern: &str, segment: &str) -> bool {
    pattern
        .split_once('*')
        .map_or(pattern == segment, |(prefix, suffix)| {
            segment.len() >= prefix.len() + suffix.len()
                && segment.starts_with(prefix)
                && segment.ends_with(suffix)
        })
}

/// Whether `<name>` is declared `optional = true` in `[dependencies]` or in
/// a `[target.'cfg(...)'.dependencies]` table — dev/build dependency tables
/// are still not consulted, matching this scanner's other best-effort
/// limits. A version-string dependency (`name = "1"`) is never optional;
/// only the expanded table form can set the flag.
///
/// `resolver_v1` (see [`resolver_v1_is_in_effect`]) decides which target
/// tables count: under Cargo's feature resolver v2, only one whose target
/// actually applies to the edge capsule's own build (see
/// [`target_key_matches_edge_capsule`]) does; under v1, every target table
/// counts regardless of its own predicate, since that resolver version
/// unifies a target-specific dependency's features into the package
/// REGARDLESS OF WHICH TARGET IS ACTUALLY BEING BUILT — verified directly
/// (see `resolver_v1_is_in_effect`'s own doc for the exact experiment).
#[must_use]
fn is_optional_dependency(table: &toml::Table, name: &str, resolver_v1: bool) -> bool {
    let declares_optional = |deps: &toml::Table| {
        deps.get(name)
            .and_then(toml::Value::as_table)
            .and_then(|dep| dep.get("optional"))
            .and_then(toml::Value::as_bool)
            == Some(true)
    };
    let in_table = |key: &str, tbl: &toml::Table| {
        tbl.get(key)
            .and_then(toml::Value::as_table)
            .is_some_and(declares_optional)
    };
    if in_table("dependencies", table) {
        return true;
    }
    table
        .get("target")
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets
                .iter()
                .filter(|(target_key, _)| {
                    resolver_v1 || target_key_matches_edge_capsule(target_key)
                })
                .map(|(_, target)| target)
                .filter_map(toml::Value::as_table)
                .any(|target| in_table("dependencies", target))
        })
}

/// Whether some feature entry, anywhere in `features_table`, explicitly
/// names `dep:<pkg>` — Cargo's own syntax to depend on an optional
/// dependency being enabled without also turning on its implicit same-named
/// local feature.
///
/// Verified directly against `cargo rustc -- --print cfg`: once a manifest
/// uses `dep:pkg` *anywhere* in `[features]`, Cargo suppresses `pkg`'s
/// implicit feature crate-wide, even for an unrelated `default = ["pkg/feat"]`
/// entry that would otherwise turn it on — the same round-9-style trap of
/// assuming a Cargo feature-graph rule holds unconditionally when it is
/// actually gated by something else in the manifest. Missing this made a
/// `#[cfg(feature = "pkg")]` route look included in the scan even though
/// such a manifest's real build compiles it out (Codex review on #2739,
/// round 13, P2).
#[must_use]
fn implicit_feature_is_suppressed(features_table: Option<&toml::Table>, pkg: &str) -> bool {
    let needle = format!("dep:{pkg}");
    features_table.is_some_and(|features| {
        features.values().any(|entries| {
            entries
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(&needle)))
        })
    })
}

/// Whether `[features]` declares an entry literally named `pkg` — a real,
/// explicit feature that merely happens to share an optional dependency's
/// name, as opposed to Cargo's own auto-generated implicit feature of the
/// same name (which only exists when no such explicit entry is present).
/// [`enabled_features_from_manifest_for_resolver`] checks this BEFORE
/// consulting [`implicit_feature_is_suppressed`]: `dep:pkg` used elsewhere
/// suppresses the auto-generated implicit feature, but has no bearing on
/// an explicit one, which `pkg/feat` syntax still turns on regardless
/// (Codex review on #2739, round 34, P1).
#[must_use]
fn has_explicit_feature_entry(features_table: Option<&toml::Table>, pkg: &str) -> bool {
    features_table.is_some_and(|features| features.contains_key(pkg))
}

/// The scanned crate's own Rust library-crate identifier — what
/// `edge_routes![this_name::item]` would actually have to spell to reach one
/// of its items, same as `crate::item`. This is not always the bare
/// `[package] name`: `[lib] name = "..."` can rename the library target
/// outright, and even without that override Cargo turns every `-` in the
/// package name into `_` for the crate identifier (a package named
/// `my-app` compiles to `extern crate my_app`, never `my-app` — that is not
/// a legal Rust identifier). [`resolve_edge_scan_with_extra_file`] uses this
/// for [`EdgeScan::crate_name`]; [`enabled_features_from_manifest_for_resolver`]
/// does not — see [`package_name_from_manifest`].
#[must_use]
fn rust_crate_name_from_manifest(table: &toml::Table) -> Option<String> {
    table
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .or_else(|| package_name_from_manifest(table).map(|name| name.replace('-', "_")))
}

/// [`enabled_features_from_manifest_for_resolver`] assuming Cargo's feature
/// resolver v2 (`resolver_v1 = false`) — the common case every test in this
/// module that isn't specifically about resolver-v1 unification exercises.
///
/// `#[cfg(test)]`: production code calls
/// [`enabled_features_from_manifest_for_resolver`] directly, since it needs
/// the real, manifest-derived resolver version (see
/// [`resolver_v1_is_in_effect`]).
#[cfg(test)]
#[must_use]
fn enabled_features_from_manifest(manifest: &str, requested: &[&str]) -> BTreeSet<String> {
    enabled_features_from_manifest_for_resolver(manifest, requested, false)
}

/// Read a crate's `Cargo.toml`, seed the feature queue with `[features]
/// default = [...]` plus `requested` (features asked for on the command
/// line), and follow both through the feature graph in `[features]` to build
/// the full set of feature names actually enabled.
///
/// Only follows features the crate declares in its own `[features]` table. A
/// dependency-feature reference's own suffix (`pkg/feat`'s `feat`, or
/// `pkg?/feat`'s) is not a feature name of this crate, so it is never queued
/// as one — `cfg(feature = "...")` never names a dependency's feature
/// anyway. Its `pkg` half is different: naming an optional dependency this
/// way *without* the `?` weak-dependency marker also turns that dependency
/// on, and Cargo auto-generates a same-named local feature for every
/// optional dependency, so `pkg/feat` (unlike `pkg?/feat`) queues `pkg`
/// itself too — skipping that would leave a `#[cfg(feature = "pkg")]`
/// route out of the scan even though Cargo really compiles it, the
/// dangerous direction this scan exists to avoid.
/// A requested name is still recorded as enabled even without a `[features]`
/// table to expand it through — an app can request a feature that exists
/// only to gate `#[cfg(feature = "...")]` code, with no `[features]` entry of
/// its own. Returns just `requested` (or nothing) on a parse failure — the
/// safe direction, since an unresolvable `feature = "..."` predicate stays
/// conservative regardless.
///
/// `requested` may use Cargo's package-qualified `--features` syntax
/// (`autumn build -p blog --features blog/extra-routes`, `pkg?/feat`). When
/// `pkg` names *this* crate (its own `[package] name`), that is exactly a
/// request for `feat`, so the qualifier is stripped — leaving it in would
/// drop the feature at the dependency-feature check below and silently miss
/// a route the real build turns on, the dangerous direction this scan exists
/// to avoid. A qualifier naming a *different* crate is left untouched (and so
/// still dropped there): a workspace build can turn on a same-named feature
/// on some other package, and stripping that qualifier too would wrongly
/// enable this crate's own `#[cfg(feature = "...")]` code that Cargo left
/// off, the opposite mistake.
///
/// `resolver_v1` (see [`resolver_v1_is_in_effect`]) is forwarded to
/// [`is_optional_dependency`] for the strong-dependency-feature check below.
#[must_use]
fn enabled_features_from_manifest_for_resolver(
    manifest: &str,
    requested: &[&str],
    resolver_v1: bool,
) -> BTreeSet<String> {
    let mut enabled = BTreeSet::new();
    let table = toml::from_str::<toml::Table>(manifest).ok();
    let features_table = table
        .as_ref()
        .and_then(|table| table.get("features"))
        .and_then(toml::Value::as_table)
        .cloned();
    let package_name = table.as_ref().and_then(package_name_from_manifest);

    // `default` is a real feature name — but, verified directly against
    // `cargo rustc -- --print cfg`, only when the manifest actually declares
    // `[features] default = [...]`, even an empty list. With no `[features]`
    // table at all, or one that simply never mentions `default`, Cargo never
    // emits `feature = "default"`, and a `#[cfg(feature = "default")]` route
    // is compiled out of an ordinary default build exactly like any other
    // never-enabled feature name — so seeding it unconditionally made THAT
    // route look included when Cargo really excludes it, a phantom route
    // that can spuriously demand a capsule/WASI target or reject `--embed`
    // (Codex review on #2739, round 12, P2, correcting round 9's overly
    // broad fix: `default` is only ever "always on" for a build that already
    // declares it, never as a feature this scan may assume into existence).
    let default_members = features_table
        .as_ref()
        .and_then(|features| features.get("default"))
        .and_then(toml::Value::as_array);
    let mut queue: Vec<String> = Vec::new();
    if let Some(members) = default_members {
        queue.push("default".to_owned());
        queue.extend(
            members
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned),
        );
    }
    queue.extend(requested.iter().map(|name| {
        name.split_once('/').map_or_else(
            || (*name).to_owned(),
            |(pkg, feat)| {
                if Some(pkg.trim_end_matches('?')) == package_name.as_deref() {
                    feat.to_owned()
                } else {
                    (*name).to_owned()
                }
            },
        )
    }));

    while let Some(name) = queue.pop() {
        if name.starts_with("dep:") {
            continue;
        }
        if let Some((pkg, _feat)) = name.split_once('/') {
            // The strong form (`pkg/feat`, no `?`) also activates `pkg`
            // itself — but only when `pkg` is declared `optional = true`:
            // Cargo auto-generates a same-named local feature for an
            // optional dependency, never for a normal one, so a normal
            // dependency sharing its name with an unrelated local feature
            // must not have that feature turned on here (Codex review on
            // #2739, round 7, P2). The weak form (`pkg?/feat`) makes no
            // such promise either way. That IMPLICIT feature is itself
            // suppressed crate-wide once any feature entry spells `dep:pkg`
            // — see `implicit_feature_is_suppressed` (round 13, P2) — but
            // an EXPLICIT `[features] pkg = [...]` entry is a different
            // feature that merely happens to share the dependency's name,
            // and `pkg/feat` still turns it on regardless of `dep:pkg`
            // appearing elsewhere: verified directly (`cargo rustc --
            // --print cfg`) that `feature="pkg"` is active for
            // `default = ["pkg/feat"]` + an explicit `pkg = []` + an
            // unrelated `other = ["dep:pkg"]`, and is NOT active with the
            // explicit `pkg = []` entry removed (the round-13 case this
            // suppression still correctly covers). Checking for an
            // explicit entry first, before consulting the suppression at
            // all, is what distinguishes the two (Codex review on #2739,
            // round 34, P1).
            if !pkg.ends_with('?')
                && table
                    .as_ref()
                    .is_some_and(|t| is_optional_dependency(t, pkg, resolver_v1))
                && (has_explicit_feature_entry(features_table.as_ref(), pkg)
                    || !implicit_feature_is_suppressed(features_table.as_ref(), pkg))
            {
                queue.push(pkg.to_owned());
            }
            continue;
        }
        if !enabled.insert(name.clone()) {
            continue; // Already expanded — skip, so a feature cycle can't loop forever.
        }
        if let Some(sub) = features_table
            .as_ref()
            .and_then(|features| features.get(&name))
            .and_then(toml::Value::as_array)
        {
            queue.extend(
                sub.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned),
            );
        }
    }
    enabled
}

/// Recursively collect `.rs` files under `dir`, skipping build output, dot-dirs,
/// and `tests/` trees, and never following symlinks (a symlinked directory cycle
/// would otherwise hang this tool).
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if name == "target" || name == "tests" || name.starts_with('.') {
                continue;
            }
            collect_rs_files(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Scan one source: `syn` for the marked functions, tokens for the registrations.
///
/// The two halves are independent on purpose — a file whose *items* fail to
/// parse can still contribute registrations, and vice versa.
fn scan_source(
    file: &str,
    src: &str,
    table: Option<&toml::Table>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) {
    let (crate_root, module_path) = crate_context_from_file(file, table);
    scan_source_with_context(file, src, &crate_root, module_path, default_features, scan);
}

/// [`scan_source`], but with the crate/module context supplied directly
/// instead of derived from `file`'s own path via [`crate_context_from_file`].
/// [`scan_bin_crate_tree`] uses this: a custom `[[bin]] path]`'s own
/// out-of-line submodule lives wherever `mod x;` resolution says it does,
/// which need not follow `src/`'s directory-mirrors-modules convention at
/// all (Codex review on #2739, round 13, P2).
fn scan_source_with_context(
    file: &str,
    src: &str,
    crate_root: &str,
    module_path: Vec<String>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) {
    if let Ok(ast) = syn::parse_file(src) {
        let mut module_path = module_path.clone();
        scan_items(
            &ast.items,
            file,
            crate_root,
            &mut module_path,
            default_features,
            scan,
        );
    }
    if let Ok(stream) = TokenStream::from_str(src) {
        let mut module_path = module_path;
        collect_registrations(
            &stream,
            crate_root,
            &mut module_path,
            default_features,
            scan,
        );
    }
}

/// Derive a function's crate identity and module-path *prefix* from the file
/// it is declared in, following Rust's directory-mirrors-modules convention:
/// `src/routes/home.rs` is module `routes::home`; `src/routes/mod.rs`,
/// `src/main.rs`, and `src/lib.rs` contribute no segment of their own (each
/// is the "index" file for its directory, or the crate root). Inline `mod x
/// { ... }` nesting (tracked separately in [`scan_items`]) appends after
/// this prefix, giving the full accumulated path.
///
/// Without the module-path half, every function reached via `mod x;` (a
/// separate file) — the common case, not the inline-module one — got an
/// empty module path. [`is_registered`] used to treat any empty path as
/// "any qualifier matches" (a fallback for a path it genuinely could not
/// resolve), which let `edge_routes![users::show]` mark an unrelated
/// `admin::show` as registered too, for the exact `mod users;` / `mod
/// admin;` layout this function exists to handle (Codex review on #2739,
/// P2). That fallback is gone now that this function makes the path
/// resolvable in the first place — an empty path means a genuine crate-root
/// function, not an unresolved one, so `is_registered` no longer treats it
/// as a wildcard either (Codex review on #2739, round 3, P2).
///
/// A non-standard layout can still make this wrong for a shape this scan
/// does not resolve — a directory-style out-of-line submodule this scan
/// cannot find at all, e.g. — the same accepted "best-effort, not a build
/// system" trade-off as every other heuristic here: a wrong prefix produces
/// an extra false-positive "unregistered" warning, never a missed one. A
/// `#[path = "..."]` override specifically is corrected BEFORE reaching
/// this function at all: [`resolve_edge_scan_impl`] resolves
/// [`path_attribute_module_paths`] first and calls
/// [`scan_source_with_context`] directly for an overridden file, bypassing
/// this directory-mirrors-modules guess entirely — the false-positive
/// direction turned out not to be safe for it, since a sole registration
/// reached only through a `#[path]`-redirected file being wrongly reported
/// unregistered makes `run_edge_capsule_build` hard-fail the whole build,
/// not merely warn (Codex review on #2739, round 31, P2).
///
/// `src/bin/<name>.rs` (or the directory form, `src/bin/<name>/main.rs`) is
/// none of the above: Cargo treats it as its OWN crate root, one `[[bin]]`
/// target entirely separate from the app's library crate, however deep its
/// own path under `src/` looks. `crate::show` inside it means that bin
/// target's own crate root, so the module path returned here is relative to
/// the bin target's root, not to `src/` itself, and the returned crate
/// identity is `"bin:<name>"` rather than the library crate's `""` (Codex
/// review on #2739, round 6, P2). The crate identity matters beyond
/// resolving `crate::` correctly: two different crates can each declare a
/// same-named function at their own root, both with an empty module path,
/// and [`is_registered`] needs the identity to tell them apart (round 7,
/// P2) — see [`EdgeFn::crate_root`].
///
/// A file outside `src/` entirely is the same story again, one level up:
/// [`resolve_edge_scan_with_extra_file`] is the only caller that ever passes
/// one in, always the edge-capsule bin's own `[[bin]] path` when that path
/// points somewhere other than the conventional `src/bin/...` (a
/// conventional path is under `src/` and so never reaches here that way —
/// see that function's own `src/`-prefix short-circuit). It is scanned as a
/// single file, never walked for submodules the way `src/bin/<name>/` is, so
/// that whole file IS the custom bin's own crate root, not a module nested
/// under the library crate's tree — treating it as the latter let a bare
/// registration written in it credit a same-named library-crate function
/// instead, and rejected a valid `crate::`-qualified registration to its own
/// handler because the handler looked like it lived under a fictitious
/// nested module (Codex review on #2739, round 8, P2).
///
/// `src/main.rs` — a package's own implicit default binary — is yet another
/// crate root distinct from `src/lib.rs`'s library: when both exist, Cargo
/// compiles them as two separate crates, and `crate::` inside `main.rs`
/// means main.rs's own binary crate, never the library's. Giving it the
/// library's own empty crate root conflated the two: a `crate::show`
/// written in `main.rs` could credit an unrelated same-named `show` at
/// `lib.rs`'s own root, and (since a crate can only ever refer to itself via
/// `crate::`, never its own external name) a crate-name-qualified
/// `my_app::show` — which real Rust can only ever mean "the library crate
/// named `my_app`," reached as an extern dependency — could equally be
/// satisfied by main.rs's own same-named function instead of the library's
/// genuine one, silently muting the warning for whichever one was not
/// really registered (Codex review on #2739, round 15, P2).
#[must_use]
fn crate_context_from_file(file: &str, table: Option<&toml::Table>) -> (String, Vec<String>) {
    let without_ext = file.strip_suffix(".rs").unwrap_or(file);
    let Some(without_src) = without_ext.strip_prefix("src/") else {
        return (format!("bin:{without_ext}"), Vec::new());
    };
    if without_src.is_empty() {
        return (String::new(), Vec::new());
    }
    if let Some(rest) = without_src.strip_prefix("bin/") {
        // `<name>` alone (the flat `src/bin/<name>.rs` form) has nothing left
        // once the bin target's own name is dropped — it IS that crate's root
        // — but only when Cargo actually compiles `<name>` as one: with
        // `[package] autobins = false` and no `[[bin]]` entry naming it,
        // Cargo gives it no crate of its own at all, so this file (whether
        // it's that bin's own root or one of ITS OWN further submodules,
        // like `src/bin/<name>/routes.rs`) is only ever reachable, if at
        // all, as an ordinary submodule of whichever crate's own `mod`
        // declaration actually references it — this per-file directory walk
        // cannot trace that (the same class of limit as a
        // `#[path]`-redirected file's own further submodules, round 31).
        // Falling through to the ordinary library-relative treatment below
        // at least avoids asserting the definitely-wrong `bin:<name>`
        // identity for it (Codex review on #2739, round 35, P2).
        let (name, bin_relative) = rest.split_once('/').unwrap_or((rest, ""));
        if src_bin_target_is_real(table, name) {
            return (
                format!("bin:{name}"),
                module_path_from_segments(bin_relative),
            );
        }
    }
    if without_src == "main" {
        // The package's own implicit default binary, `src/main.rs` — a
        // separate crate from the library's `src/lib.rs`, even though both
        // map to the same empty module path. No real `[[bin]]` can be named
        // literally "main" while a default `src/main.rs` binary also
        // exists (Cargo rejects the name collision), so reusing the
        // `"bin:<name>"` convention here is always unambiguous — but only
        // when `src/main.rs` is actually compiled as a target at all:
        // verified directly that `[package] autobins = false` suppresses
        // Cargo's automatic discovery of `src/main.rs` too, not just
        // `src/bin/*.rs` (a real build with `autobins = false` and no
        // `[[bin]]` entry produces no binary whatsoever, `src/main.rs`
        // included) — the identical mistake `src_bin_target_is_real` exists
        // to avoid, applied to the one bin name Cargo reserves for this
        // file specifically (Codex review on #2739, round 35, P2).
        if src_main_is_real(table) {
            return ("bin:main".to_owned(), Vec::new());
        }
    }
    (String::new(), module_path_from_segments(without_src))
}

/// Whether `name` (the first path segment under `src/bin/`, shared by both
/// its own root file and every one of ITS OWN further submodules — a
/// flat-file bin's root IS `name` with nothing left over, a directory-style
/// one's root is `name/main.rs` and a sibling `name/routes.rs` is still
/// part of the SAME crate) is a bin target Cargo actually compiles: either
/// automatic `src/bin/` discovery (the default, unless `[package] autobins
/// = false`), which claims `name` unconditionally regardless of which
/// shape backs it, or an explicit `[[bin]]` entry naming it — via its own
/// `path` matching either conventional shape for `name`, or, absent a
/// `path`, its own `name` field matching directly (Cargo infers the same
/// conventional shape autobins would).
///
/// With `autobins = false` and no `[[bin]]` entry for it, nothing under
/// `src/bin/<name>/` (or the flat `src/bin/<name>.rs`) is compiled as its
/// own crate at all — such a file can instead be an ordinary submodule
/// loaded by some OTHER real target's own `mod name;` (e.g. an explicit
/// `[[bin]] path = "src/bin.rs"` target whose own out-of-line submodules
/// live under `src/bin/`, following Rust's own directory-mirrors-modules
/// convention for wherever `bin.rs` itself lives) (Codex review on #2739,
/// round 35, P2).
#[must_use]
fn src_bin_target_is_real(table: Option<&toml::Table>, name: &str) -> bool {
    let autobins_disabled = table
        .and_then(|t| t.get("package"))
        .and_then(|p| p.get("autobins"))
        .and_then(toml::Value::as_bool)
        == Some(false);
    if !autobins_disabled {
        return true;
    }
    let Some(bins) = table
        .and_then(|t| t.get("bin"))
        .and_then(toml::Value::as_array)
    else {
        return false;
    };
    bins.iter().any(|bin| {
        bin.get("path").and_then(toml::Value::as_str).map_or_else(
            || bin.get("name").and_then(toml::Value::as_str) == Some(name),
            |path| {
                let normalized = path.strip_prefix("./").unwrap_or(path);
                normalized == format!("src/bin/{name}.rs")
                    || normalized == format!("src/bin/{name}/main.rs")
            },
        )
    })
}

/// Whether `src/main.rs` is actually compiled as the package's implicit
/// default binary: real unless `[package] autobins = false` (verified
/// directly — see [`src_bin_target_is_real`]) AND no explicit `[[bin]]`
/// entry's `path` names it directly (a `name`-only entry can't infer
/// `src/main.rs`, since Cargo's own name-inference for a pathless `[[bin]]`
/// entry always looks under `src/bin/`, never `src/main.rs` itself, no
/// matter what the entry is named).
#[must_use]
fn src_main_is_real(table: Option<&toml::Table>) -> bool {
    let autobins_disabled = table
        .and_then(|t| t.get("package"))
        .and_then(|p| p.get("autobins"))
        .and_then(toml::Value::as_bool)
        == Some(false);
    if !autobins_disabled {
        return true;
    }
    let Some(bins) = table
        .and_then(|t| t.get("bin"))
        .and_then(toml::Value::as_array)
    else {
        return false;
    };
    bins.iter().any(|bin| {
        bin.get("path")
            .and_then(toml::Value::as_str)
            .is_some_and(|path| path.strip_prefix("./").unwrap_or(path) == "src/main.rs")
    })
}

/// Split `path` on `/`, dropping a trailing index-file segment — the rule
/// [`crate_context_from_file`] applies both from `src/` and, relative to a
/// `[[bin]]` target's own root, from `src/bin/<name>/`.
///
/// `mod` is dropped whenever it is the last segment, at any depth: unlike
/// `main`/`lib`, `<dir>/mod.rs` is Rust's own "index file for this
/// directory" convention, valid for a module nested arbitrarily deep, not
/// just at a crate root. `main`/`lib` are dropped only when they are the
/// WHOLE path (a single segment) — they are never an index-file convention
/// for an ordinary subdirectory, only the name of a crate root itself (a
/// package's `src/main.rs` binary target, or a directory-style `[[bin]]`
/// target's own `src/bin/<name>/main.rs`). Dropping them unconditionally, at
/// any depth, wrongly stripped the segment from a genuinely nested module
/// that merely happens to share the name — `mod routes { mod main; }`
/// (`src/routes/main.rs`) is `routes::main` in real Rust, not `routes`
/// (Codex review on #2739, round 15, P2).
fn module_path_from_segments(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut segments: Vec<&str> = path.split('/').collect();
    match segments.last() {
        Some(&"mod") => {
            segments.pop();
        }
        Some(&("main" | "lib")) if segments.len() == 1 => {
            segments.pop();
        }
        _ => {}
    }
    segments.into_iter().map(str::to_owned).collect()
}

/// Walk items, collecting `#[edge]`-marked functions and descending into inline
/// modules (`mod routes { ... }`). `mod routes;` has no items here — its file is
/// visited separately by the directory walk.
///
/// `module_path` is the chain of inline module names enclosing the items
/// currently being walked. It is pushed before, and popped after, each
/// `mod routes { ... }` recursion, so it always reads as the current nesting.
///
/// An inline module carrying its own definitely-false `#[cfg(...)]` is never
/// descended into at all — real Rust strips it and everything inside it, so a
/// handler under it must not stay in the scan just because `edge_fn` only
/// ever looks at a function's own attributes (Codex review on #2739, round
/// 11, P2).
fn scan_items(
    items: &[syn::Item],
    file: &str,
    crate_root: &str,
    module_path: &mut Vec<String>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) {
    for item in items {
        match item {
            syn::Item::Fn(item_fn) => {
                if let Some(found) = edge_fn(
                    &item_fn.attrs,
                    &item_fn.sig,
                    file,
                    crate_root,
                    module_path,
                    default_features,
                ) {
                    scan.functions.push(found);
                }
            }
            syn::Item::Mod(item_mod) => {
                if let Some((_, inner)) = &item_mod.content {
                    // The module's own `#[cfg(...)]` gates everything inside
                    // it, same as a function's — `edge_fn` only ever sees a
                    // function's OWN attributes, so a handler under a
                    // definitely-disabled `#[cfg(feature = "premium")] mod
                    // routes { ... }` stayed in the scan even though real
                    // Rust strips the whole module out. That is the
                    // dangerous direction this cfg support exists to avoid:
                    // not a missed route, but a phantom one that makes an
                    // otherwise-default build look like it needs the edge
                    // capsule / WASI target, or spuriously conflicts with
                    // `--embed` (Codex review on #2739, round 11, P2 —
                    // extended to a `cfg_attr`-injected exclusion in round
                    // 33, P2).
                    let cfg_excludes = item_mod
                        .attrs
                        .iter()
                        .any(|attr| attr_cfg_excludes(attr, default_features));
                    if cfg_excludes {
                        continue;
                    }
                    module_path.push(item_mod.ident.to_string());
                    scan_items(inner, file, crate_root, module_path, default_features, scan);
                    module_path.pop();
                }
            }
            _ => {}
        }
    }
}

/// Find every out-of-line `mod name;` declaration reachable from `items`
/// (descending into inline `mod a { ... }` blocks the same way [`scan_items`]
/// does, cfg-excluded ones included), returning each as `(enclosing_path,
/// name, path_attribute_value)` — `enclosing_path` is the chain of *inline*
/// module names between `items`' own file and the declaration, empty for a
/// top-level one; `path_attribute_value` is `Some` when the declaration
/// carries a `#[path = "..."]` override.
///
/// [`scan_bin_crate_tree`] uses this to follow a custom `[[bin]] path]`'s own
/// module graph: unlike the library's `src/` walk (which reaches every file
/// under `src/` regardless of whether any `mod` declaration actually
/// references it), a bin target outside the conventional layout has no such
/// directory convention to lean on, so its submodules are invisible unless
/// this scan actually follows its `mod` declarations (Codex review on #2739,
/// round 13, P2). [`path_attribute_module_paths`] reuses this same
/// collector for the ordinary `src/` walk's own `#[path]` overrides, simply
/// filtering for the entries where `path_attribute_value` is `Some` —
/// before round 35 that was a second, near-identical collector
/// (`collect_path_attribute_mods`); [`scan_bin_crate_tree`]'s own use of
/// this function never saw a `#[path]` value at all, so a custom crate
/// tree's own `#[path]`-redirected submodule was invisible to it entirely,
/// found by neither collector (Codex review on #2739, round 35, P1).
fn out_of_line_mod_declarations(
    items: &[syn::Item],
    default_features: &BTreeSet<String>,
) -> Vec<(Vec<String>, String, Option<String>)> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    collect_out_of_line_mods(items, default_features, &mut path, &mut out);
    out
}

fn collect_out_of_line_mods(
    items: &[syn::Item],
    default_features: &BTreeSet<String>,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, String, Option<String>)>,
) {
    for item in items {
        let syn::Item::Mod(item_mod) = item else {
            continue;
        };
        let cfg_excludes = item_mod
            .attrs
            .iter()
            .any(|attr| attr_cfg_excludes(attr, default_features));
        if cfg_excludes {
            continue;
        }
        match &item_mod.content {
            Some((_, inner)) => {
                path.push(item_mod.ident.to_string());
                collect_out_of_line_mods(inner, default_features, path, out);
                path.pop();
            }
            None => out.push((
                path.clone(),
                item_mod.ident.to_string(),
                path_attribute_value(&item_mod.attrs, default_features),
            )),
        }
    }
}

/// Every out-of-line `mod name;` in the project carrying a `#[path =
/// "..."]` override, mapped from the FILE it redirects to (relative to
/// `project_root`, forward-slash-separated — the same key format the main
/// `src/` walk uses) to that module's real crate identity and module path.
///
/// `#[path]` redirects which file backs a module without renaming the
/// module itself: real Rust still resolves `name::show` for `#[path =
/// "actual.rs"] mod name;`, never a path derived from the file `actual.rs`
/// happens to be named. The main `src/` walk assumes file path mirrors
/// module path (`crate_context_from_file`) for every file it finds via
/// plain directory listing, not by following `mod` declarations, so a
/// `#[path]`-redirected file's module path came out wrong — not merely a
/// cosmetic difference: when that file's registration is the project's
/// ONLY one, `is_registered` never matches it, `scan.registered_fns()`
/// comes back empty, and `run_edge_capsule_build` hard-fails the whole
/// build ("found N `#[edge]` handler(s) but none are registered"), not just
/// the extra false-positive warning a wrong module-path guess produces in
/// every other case (Codex review on #2739, round 31, P2).
///
/// Scoped to the direct case only: a `mod name;` (top-level or nested
/// inside an INLINE `mod { ... }`) whose own `#[path]` attribute names the
/// file, resolved relative to the DECLARING file's own directory (Rust's
/// real rule) and lexically normalized. A file reached this way that
/// itself declares FURTHER out-of-line submodules of its own is a deeper,
/// genuinely ambiguous corner of Rust's `#[path]` interaction with nested
/// modules this scan does not attempt to resolve — the same accepted,
/// best-effort trade-off as every other heuristic in this module.
///
/// Maps to a `Vec` of identities, not one: Rust allows the SAME file to be
/// `#[path]`-included under multiple names (`#[path = "shared.rs"] mod a;`
/// and `#[path = "shared.rs"] mod b;` elsewhere both compile `shared.rs`,
/// once per name — verified directly via a real build). A single-value map
/// previously let the second registration silently overwrite the first, so
/// the file was scanned under only one of its two real identities and a
/// registration naming the other never matched (Codex review on #2739,
/// round 40, P2).
fn path_attribute_module_paths(
    project_root: &Path,
    table: Option<&toml::Table>,
    default_features: &BTreeSet<String>,
) -> BTreeMap<String, Vec<(String, Vec<String>)>> {
    let mut files = Vec::new();
    collect_rs_files(&project_root.join("src"), &mut files);
    files.sort();
    let mut overrides: BTreeMap<String, Vec<(String, Vec<String>)>> = BTreeMap::new();
    // Every CONVENTIONAL (non-`#[path]`) out-of-line `mod name;`
    // declaration found across the whole `src/` tree, alongside the real
    // file it resolves to. Collected in this same pass so a SECOND pass
    // (below) can find, for each `#[path]`-aliased target already in
    // `overrides`, any ALSO-conventionally-reachable identity for that
    // same physical file.
    let mut conventional_declarations: Vec<(String, String, Vec<String>)> = Vec::new();
    for declaring_file in &files {
        let Ok(src) = std::fs::read_to_string(declaring_file) else {
            continue;
        };
        let Ok(ast) = syn::parse_file(&src) else {
            continue;
        };
        let declaring_rel = declaring_file
            .strip_prefix(project_root)
            .unwrap_or(declaring_file)
            .to_string_lossy()
            .replace('\\', "/");
        let (crate_root, base_module_path) = crate_context_from_file(&declaring_rel, table);
        let found = out_of_line_mod_declarations(&ast.items, default_features);
        for (inline, name, path_value) in found {
            let mut module_path = base_module_path.clone();
            module_path.extend(inline.iter().cloned());
            module_path.push(name.clone());
            if let Some(path_value) = path_value {
                // A `#[path]` override nested inside an INLINE module resolves
                // relative to the DIRECTORY THAT MODULE PATH IMPLIES, not the
                // declaring file's own physical directory — verified directly:
                // `mod api { #[path = "actual.rs"] mod handlers; }` in
                // `src/lib.rs` compiles by finding `src/api/actual.rs`, never
                // `src/actual.rs`. Same rule an ordinary out-of-line `mod
                // handlers;` (no `#[path]`) already follows via
                // `resolve_out_of_line_module_file`'s own `dir_segments`
                // accumulation — this mirrors it by folding each inline
                // segment in as a subdirectory before joining the attribute's
                // own value (Codex review on #2739, round 33, P2).
                //
                // Fresh evidence beyond that top-level case is the SAME
                // shape declared inside an already out-of-line file: for
                // `src/foo.rs` (reached via a plain, non-aliased `mod foo;`)
                // containing `mod inline { #[path = "actual.rs"] mod
                // handlers; }`, rustc loads `src/foo/inline/actual.rs` —
                // verified directly via a real build — never `src/inline/
                // actual.rs`. The declaring FILE's own physical parent
                // directory (`src/`, from `declaring_file.parent()`) is the
                // wrong base here: `foo.rs`'s own conventional child
                // directory is `src/foo/`, exactly what `base_module_path`
                // (the file's own logical module path, already used by the
                // conventional branch below via `resolve_out_of_line_module_file`)
                // already names, so folding `base_module_path` then `inline`
                // onto `src/` directly — instead of the declaring file's own
                // parent directory — gives the right base for both the
                // top-level and the nested case alike (Codex review on
                // #2739, round 46, P2).
                let inline_dir = base_module_path
                    .iter()
                    .chain(inline.iter())
                    .fold(project_root.join("src"), |dir, segment| dir.join(segment));
                let target_file = lexically_normalize_path(&inline_dir.join(&path_value));
                let target_rel = target_file
                    .strip_prefix(project_root)
                    .unwrap_or(&target_file)
                    .to_string_lossy()
                    .replace('\\', "/");
                overrides
                    .entry(target_rel)
                    .or_default()
                    .push((crate_root.clone(), module_path));
            } else if let Some(target_file) = resolve_out_of_line_module_file(
                &project_root.join("src"),
                &{
                    let mut dir_segments = base_module_path.clone();
                    dir_segments.extend(inline);
                    dir_segments
                },
                &name,
            ) {
                let target_rel = target_file
                    .strip_prefix(project_root)
                    .unwrap_or(&target_file)
                    .to_string_lossy()
                    .replace('\\', "/");
                conventional_declarations.push((target_rel, crate_root.clone(), module_path));
            }
        }
    }
    // A file reached both conventionally (`mod handlers;`) AND via a
    // `#[path]` alias (`#[path = "handlers.rs"] mod alias;`) is compiled
    // under BOTH names by real Rust — verified directly via a real build.
    // `overrides` above only ever recorded the alias side; add the
    // conventional identity too, but ONLY for a target that already has an
    // alias recorded (this loop, not the one above, is where a plain,
    // never-aliased file's identity is decided — leaving it to the
    // ordinary walk's own `crate_context_from_file` guess, unchanged, so
    // this fix cannot touch the vast majority of files that have nothing
    // to do with `#[path]` at all) (Codex review on #2739, round 45, P2).
    for (target_rel, crate_root, module_path) in conventional_declarations {
        if let Some(identities) = overrides.get_mut(&target_rel)
            && !identities.contains(&(crate_root.clone(), module_path.clone()))
        {
            identities.push((crate_root, module_path));
        }
    }
    overrides
}

/// The string value of a plain `#[path = "..."]` meta, if `meta` is one.
fn path_name_value_str(meta: &syn::Meta) -> Option<String> {
    if !meta.path().is_ident("path") {
        return None;
    }
    let syn::Meta::NameValue(name_value) = meta else {
        return None;
    };
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(lit_str),
        ..
    }) = &name_value.value
    else {
        return None;
    };
    Some(lit_str.value())
}

/// The string value of a `#[path = "..."]` attribute, if `attrs` has one —
/// including one introduced by an active `#[cfg_attr(condition, path =
/// "...")]`, which expands to a real `#[path = "..."]` exactly like any
/// other `cfg_attr`-gated attribute once `condition` holds (verified
/// directly via a real build: with the gating feature enabled, `mod
/// handlers;` behind `#[cfg_attr(feature = "alt", path = "actual.rs")]`
/// compiles `actual.rs`, not `handlers.rs`). Before this, only a literal
/// `#[path]` was recognized, so a `cfg_attr`-conditional one fell back to
/// the conventional file (or reported unresolved entirely, in a custom
/// crate tree outside `src/`), missing the real file real Rust compiles
/// (Codex review on #2739, round 40, P1).
///
/// Only a condition that DEFINITELY resolves true applies the override —
/// the opposite conservative direction from [`attr_names_including_cfg_attr`]:
/// an unresolvable condition here falls through to no override (the same
/// "not resolved" fallback an ordinary non-`#[path]` mod already gets),
/// rather than risk following a path override that isn't really active.
fn path_attribute_value(
    attrs: &[syn::Attribute],
    default_features: &BTreeSet<String>,
) -> Option<String> {
    attrs.iter().find_map(|attr| {
        if let Some(value) = path_name_value_str(&attr.meta) {
            return Some(value);
        }
        if attr.path().is_ident("cfg_attr")
            && let syn::Meta::List(list) = &attr.meta
        {
            return path_value_from_cfg_attr_metas(list, default_features);
        }
        None
    })
}

/// The `path = "..."` meta inside an active `#[cfg_attr(condition, ...)]`'s
/// payload, recursing into a NESTED `cfg_attr` meta the same way
/// [`cfg_attr_active_marker_names`] does — `#[cfg_attr(feature = "a",
/// cfg_attr(feature = "b", path = "actual.rs"))]` must surface `actual.rs`
/// once both conditions hold, not stop at the literal name `cfg_attr`
/// (verified directly via a real build). The outer, one-level case was
/// round 40's fix; this is the same gap one level deeper, which rustc
/// itself expands just as readily (Codex review on #2739, round 41, P1).
fn path_value_from_cfg_attr_metas(
    list: &syn::MetaList,
    default_features: &BTreeSet<String>,
) -> Option<String> {
    let (condition, metas) = cfg_attr_payload(list, default_features)?;
    if condition != Some(true) {
        return None;
    }
    metas.iter().find_map(|meta| {
        if let Some(value) = path_name_value_str(meta) {
            return Some(value);
        }
        if meta.path().is_ident("cfg_attr")
            && let syn::Meta::List(inner) = meta
        {
            return path_value_from_cfg_attr_metas(inner, default_features);
        }
        None
    })
}

/// Resolve an out-of-line `mod name;` to its file, following Rust's own rule:
/// relative to `root_dir` (the crate root's own directory) plus
/// `dir_segments` (the accumulated module path so far), the file is either
/// `<dir>/name.rs` or, failing that, `<dir>/name/mod.rs`. `None` when neither
/// exists — a non-standard `#[path = "..."]` override, like every other
/// heuristic in this best-effort scanner, is not resolved (see the module
/// doc's "Recognition limits").
///
/// A raw identifier (`mod r#type;`) keeps its `r#` prefix as a Rust path
/// segment — `syn::Ident::to_string()` preserves it verbatim (verified
/// directly) — but Rust's own file-system module-resolution convention
/// strips it: `mod r#type;` resolves to `type.rs`, never `r#type.rs`
/// (verified directly against a real build). Every accumulated directory
/// segment needs the same normalization, not just the leaf `name` — a
/// nested `mod r#type { mod inner; }` accumulates `r#type` into
/// `dir_segments` too (Codex review on #2739, round 22, P2).
fn resolve_out_of_line_module_file(
    root_dir: &Path,
    dir_segments: &[String],
    name: &str,
) -> Option<PathBuf> {
    fn strip_raw(segment: &str) -> &str {
        segment.strip_prefix("r#").unwrap_or(segment)
    }
    let dir = dir_segments
        .iter()
        .fold(root_dir.to_path_buf(), |dir, segment| {
            dir.join(strip_raw(segment))
        });
    let name = strip_raw(name);
    let flat = dir.join(format!("{name}.rs"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = dir.join(name).join("mod.rs");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

/// Collapse `.` and `..` components lexically (no filesystem access, so it
/// works on a path that may not exist yet — unlike `std::fs::canonicalize`,
/// and without resolving symlinks, which lexical `..` handling never claims
/// to do correctly in their presence; that mismatch is accepted here the
/// same way every other heuristic in this best-effort scanner accepts a
/// narrower-than-Cargo's-own-semantics trade-off). A leading `..` with
/// nothing to pop against is kept as-is rather than discarded.
///
/// Only pops when `out`'s LAST component is a real, `Normal` one — not
/// merely non-empty. `PathBuf::pop()` alone can't tell "a real directory
/// name to cancel against" from "an earlier unresolved `..` that was kept
/// because it had nothing to pop against either": for `../../shared/lib.rs`,
/// the first `..` has nothing to pop and is kept as-is, but the SECOND `..`
/// would then happily pop that kept `..` off (a `PathBuf` popping its own
/// last component regardless of what kind it is), lexically cancelling two
/// components that don't actually cancel and normalizing to `shared/lib.rs`
/// — a different file entirely from the one a real `rustc`/Cargo build
/// resolves (Codex review on #2739, round 30, P2).
fn lexically_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
                if matches!(
                    out.components().next_back(),
                    Some(std::path::Component::Normal(_))
                ) =>
            {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Scan a `[[bin]]` target's own crate tree, starting at its root file:
/// `root_file` itself, plus every out-of-line submodule it (transitively)
/// declares via `mod name;`, each tagged with `crate_root` and its own
/// module path. Returns the absolute paths of every file scanned, so the
/// caller can exclude them from the library's own `src/` walk — without
/// that, a root file living inside `src/` (a custom `[[bin]] path =
/// "src/edge.rs"`, say) would be scanned twice: once here with the correct
/// bin identity, once by the library walk with an incorrect library-module
/// one (Codex review on #2739, round 13, P2).
///
/// [`resolve_edge_scan_with_extra_file`] calls this for `extra_file`
/// whenever it is `Some`, including one of Cargo's own two `src/bin/`
/// auto-discovery shapes (`src/bin/<name>.rs`, `src/bin/<name>/main.rs`):
/// the ordinary library walk would give such a root the right identity via
/// [`crate_context_from_file`]'s own `src/bin/` handling, but not its own
/// out-of-line submodules, which that heuristic cannot tell apart from an
/// unrelated second flat bin file in the same directory (Codex review on
/// #2739, round 22, P2).
///
/// `initial_module_path` seeds `root_file`'s own logical module path —
/// empty for a real crate root (a custom `[lib]`/`[[bin]] path`), but
/// non-empty when this same BFS machinery is reused to recursively follow
/// an EXTERNAL `#[path]` override target's own further out-of-line
/// children from the ordinary `src/` walk: that target's already-recorded
/// identity (from `path_attribute_module_paths`) becomes the starting
/// point instead of an empty path (Codex review on #2739, round 45, P1).
fn scan_bin_crate_tree(
    root_file: &Path,
    project_root: &Path,
    crate_root: &str,
    initial_module_path: Vec<String>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) -> (BTreeSet<PathBuf>, BTreeSet<PathBuf>) {
    // A manifest-specified path (a custom `[lib] path` or `[[bin]] path`)
    // can spell its root with a `..` component (`"src/sub/../app.rs"`).
    // Unlike a `.` component, Rust's own `Path`/`PathBuf` equality does NOT
    // lexically collapse `..` (verified directly: `Path::new("a/../b") !=
    // Path::new("b")`, and a `BTreeSet<PathBuf>` built from the former does
    // not recognize the latter as already present) — the caller's `claimed`
    // filter, built from this function's returned paths, would then fail to
    // recognize the ordinary `src/` walk's own (naturally `..`-free, since
    // it comes from a real directory listing) path to the very same file,
    // scanning it a second time under a different, wrong crate identity
    // (Codex review on #2739, round 22, P2). Normalizing here, once, up
    // front is enough: every other path this function builds is joined
    // from `root_dir` using plain segment names, which can never
    // (re-)introduce a `..` of their own.
    let root_file = lexically_normalize_path(root_file);
    let root_file = root_file.as_path();
    let root_dir = root_file.parent().unwrap_or_else(|| Path::new(""));
    // Cycle safety here mirrors rustc's own: verified directly via a real
    // build, `#[path = "b.rs"] mod b;` in `a.rs` and `#[path = "a.rs"] mod
    // a;` in `b.rs` fails with rustc's own "circular modules: a.rs -> b.rs
    // -> a.rs" — a file including ITSELF, directly or transitively, never
    // compiles at all. Two INDEPENDENT aliases of the same file (`#[path =
    // "shared.rs"] mod a;` and `#[path = "shared.rs"] mod b;`, neither a
    // descendant of the other) are not a cycle and both compile fine, each
    // with its own copy of that file's own further submodules — verified
    // directly too: `shared.rs`'s own `#[path = "child.rs"] mod child;`
    // compiles as BOTH `a::child` and `b::child`. So cycle detection here
    // tracks each branch's own ANCESTOR chain (the files from the tree
    // root down to here) rather than a single global "seen" set: a file
    // is skipped only when it already appears in ITS OWN branch's
    // ancestors, never merely because a DIFFERENT branch reached it first.
    // A single global `visited`-by-file set (round 42's fix, before this)
    // stopped a shared file's OWN children from ever being (re-)discovered
    // under its second alias, since the first alias to reach it "used up"
    // the file for children-discovery purposes entirely (Codex review on
    // #2739, round 43, P2).
    let mut scanned_identities: BTreeSet<(PathBuf, Vec<String>)> = BTreeSet::new();
    let mut aliased: BTreeSet<PathBuf> = BTreeSet::new();
    // A queued file's `resolution_base` is the directory ITS OWN top-level
    // (non-inline) out-of-line `mod name;` declarations resolve relative
    // to — kept as its own PHYSICAL value, separate from `module_path`
    // (the LOGICAL name accumulator used for registration matching),
    // because the two diverge the moment any ancestor was `#[path]`d. A
    // `#[path]`-loaded file resets this to its OWN directory for its own
    // children (verified directly via a real build: `#[path =
    // "shared.rs"] mod handlers;` where `shared.rs` has a plain `mod
    // child;` resolves `child` at `child.rs` beside `shared.rs`, not
    // `handlers/child.rs` — the logical name `handlers` plays no part).
    // Reconstructing this from `root_dir` + the full accumulated
    // `module_path` (this function's previous approach) stayed correct
    // only up to the first `#[path]` in the chain (Codex review on #2739,
    // round 43, P1).
    let mut queue: std::collections::VecDeque<(PathBuf, Vec<String>, Vec<PathBuf>, PathBuf)> =
        std::collections::VecDeque::new();
    queue.push_back((
        root_file.to_path_buf(),
        initial_module_path,
        vec![root_file.to_path_buf()],
        root_dir.to_path_buf(),
    ));

    while let Some((file, module_path, ancestors, resolution_base)) = queue.pop_front() {
        if !scanned_identities.insert((file.clone(), module_path.clone())) {
            continue; // This exact file/module-path identity is already scanned.
        }
        let Ok(src) = std::fs::read_to_string(&file) else {
            continue;
        };
        let rel = file
            .strip_prefix(project_root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        scan_source_with_context(
            &rel,
            &src,
            crate_root,
            module_path.clone(),
            default_features,
            scan,
        );
        scan.files_scanned += 1;

        let Ok(ast) = syn::parse_file(&src) else {
            continue;
        };
        let declaring_dir = file.parent().unwrap_or_else(|| Path::new(""));
        for (nested, name, path_value) in out_of_line_mod_declarations(&ast.items, default_features)
        {
            let mut dir_segments = module_path.clone();
            dir_segments.extend(nested.iter().cloned());
            // A `#[path = "..."]` override resolves relative to the
            // directory the DECLARING FILE ITSELF physically lives in, not
            // the directory its full logical module path (`dir_segments`)
            // would imply — Rust's real rule, verified directly: a custom
            // capsule root's `mod wiring;` reaching `cmd/wiring.rs`, which
            // itself has `#[path = "actual.rs"] mod handlers;`, resolves
            // that override to `cmd/actual.rs` (beside `wiring.rs`), never
            // `cmd/wiring/actual.rs`. Only `nested` — segments from an
            // INLINE `mod { ... }` block inside THIS SAME file — still
            // folds in as a subdirectory relative to `declaring_dir`, the
            // same rule the ordinary `src/` walk's own
            // `path_attribute_module_paths` already applies (Codex review on
            // #2739, round 35, P1, fixed incompletely; round 39, P2, this
            // fold's own base was still wrong for a non-root declaring
            // file). The CONVENTIONAL branch resolves relative to
            // `resolution_base` (this file's own physical anchor,
            // threaded through the queue) folded with `nested`, never
            // `root_dir` + the full logical `dir_segments` — those only
            // coincide up to the first `#[path]` in the chain.
            let (resolved, child_resolution_base, is_aliased) = path_value.map_or_else(
                || {
                    let base = nested
                        .iter()
                        .fold(resolution_base.clone(), |dir, segment| dir.join(segment));
                    let resolved = resolve_out_of_line_module_file(&base, &[], &name);
                    let child_base = base.join(&name);
                    (resolved, child_base, false)
                },
                |path_value| {
                    let dir = nested
                        .iter()
                        .fold(declaring_dir.to_path_buf(), |dir, segment| {
                            dir.join(segment)
                        });
                    let resolved = lexically_normalize_path(&dir.join(&path_value));
                    let child_base = resolved
                        .parent()
                        .map_or_else(|| dir.clone(), Path::to_path_buf);
                    (Some(resolved), child_base, true)
                },
            );
            let Some(resolved) = resolved else {
                continue;
            };
            if ancestors.contains(&resolved) {
                continue; // A real cycle — rustc itself rejects this at compile time.
            }
            if is_aliased {
                aliased.insert(resolved.clone());
            }
            let mut child_module_path = dir_segments;
            child_module_path.push(name);
            let mut child_ancestors = ancestors.clone();
            child_ancestors.push(resolved.clone());
            queue.push_back((
                resolved,
                child_module_path,
                child_ancestors,
                child_resolution_base,
            ));
        }
    }

    // Every physical file this BFS reached under any identity — the caller
    // uses this to keep the ordinary `src/` walk from ALSO scanning (and
    // double-crediting) the same file under its own conventional identity.
    // `aliased` is the subset reached via an explicit `#[path]` — see
    // `resolve_edge_scan_impl`'s own use of both for why that subset needs
    // separate tracking (Codex review on #2739, round 44, P2).
    let touched: BTreeSet<PathBuf> = scanned_identities
        .into_iter()
        .map(|(file, _)| file)
        .collect();
    (touched, aliased)
}

/// The last `::` segment of an attribute path, e.g. `edge` for
/// `#[autumn_web::edge]`. Attribute paths with generic arguments are not
/// attribute macros, so a plain segment match is enough.
fn attr_name(attr: &syn::Attribute) -> Option<String> {
    attr.path().segments.last().map(|s| s.ident.to_string())
}

/// Every attribute name visible on an item: each ordinary attribute's own
/// last path segment (what [`attr_name`] gives directly), plus — for each
/// `#[cfg_attr(condition, meta1, meta2, ...)]` whose condition does not
/// definitely resolve false — the last path segment of every meta in its
/// payload too.
///
/// `#[cfg_attr(feature = "edge-routes", edge)]` expands to exactly
/// `#[edge]` once `edge-routes` is on — real, valid Rust — but this scan's
/// marker search previously only ever looked at each attribute's own
/// top-level path, never `cfg_attr`'s payload, so a handler whose ONLY
/// `#[edge]` marker arrived this way was invisible to the scan entirely:
/// not merely excluded, but never even considered a candidate `EdgeFn` no
/// matter how the build was configured (Codex review on #2739, round 27,
/// P1). Conservative in the same direction as [`eval_cfg_attr`]: only a
/// condition that DEFINITELY resolves false drops the payload's names — an
/// unparseable condition (`target_os`, ...) still lets them through, since
/// wrongly hiding a real `#[edge]` marker is the dangerous direction here.
fn attr_names_including_cfg_attr(
    attrs: &[syn::Attribute],
    default_features: &BTreeSet<String>,
) -> Vec<String> {
    let mut names = Vec::new();
    for attr in attrs {
        let Some(name) = attr_name(attr) else {
            continue;
        };
        if name == "cfg_attr"
            && let syn::Meta::List(list) = &attr.meta
        {
            names.extend(cfg_attr_active_marker_names(list, default_features));
        } else {
            names.push(name);
        }
    }
    names
}

/// Parses a `cfg_attr(condition, meta1, meta2, ...)` meta list's tokens into
/// the condition's resolution — `None` for "unresolvable", the same
/// convention [`eval_cfg_attr`] uses for a plain `#[cfg(...)]` — and its
/// meta list (empty if the part after the condition fails to parse as one).
/// `None` overall means the tokens aren't shaped like a `cfg_attr` payload
/// at all (no top-level `,`).
///
/// Takes the raw `MetaList` rather than a whole `syn::Attribute` so this
/// also works one level down, on a `cfg_attr(...)` meta NESTED inside
/// another `cfg_attr`'s own payload (`#[cfg_attr(feature = "a",
/// cfg_attr(feature = "b", edge))]` — real, valid Rust: rustc expands
/// `cfg_attr` recursively, verified directly via a real build).
///
/// Splits the raw tokens on the first top-level `,` by hand, rather than
/// parsing `condition` and the rest through one combined `syn::parse::Parse`
/// impl, specifically so an UNPARSEABLE condition still doesn't lose access
/// to the meta list after it: `?`-propagating a parse failure from a
/// combined parser would bail out before ever reaching the metas, defeating
/// every conservative "unresolvable stays visible" rule built on top of
/// this.
fn cfg_attr_payload(
    list: &syn::MetaList,
    default_features: &BTreeSet<String>,
) -> Option<(Option<bool>, Vec<syn::Meta>)> {
    let tokens: Vec<TokenTree> = list.tokens.clone().into_iter().collect();
    let comma_index = tokens
        .iter()
        .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ','))?;
    let condition_tokens: proc_macro2::TokenStream =
        tokens[..comma_index].iter().cloned().collect();
    let condition = syn::parse2::<CfgPredicate>(condition_tokens)
        .ok()
        .map(|pred| pred.eval(default_features));
    let rest_tokens: proc_macro2::TokenStream = tokens[comma_index + 1..].iter().cloned().collect();
    let metas = syn::parse::Parser::parse2(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        rest_tokens,
    )
    .map(|metas| metas.into_iter().collect())
    .unwrap_or_default();
    Some((condition, metas))
}

/// The attribute names inside `#[cfg_attr(condition, meta1, meta2, ...)]`'s
/// payload, once `condition` is resolved against `default_features` —
/// empty when it definitely resolves false. Recurses into a NESTED
/// `cfg_attr(...)` meta the same way — `#[cfg_attr(feature = "a",
/// cfg_attr(feature = "b", edge))]` must surface `edge` once both
/// conditions hold, not the literal name `cfg_attr` (Codex review on
/// #2739, round 30, P1 — the direct, one-level `cfg_attr(cond, edge)` case
/// was round 27's fix; this is the same gap one level deeper, which rustc
/// itself expands just as readily).
fn cfg_attr_active_marker_names(
    list: &syn::MetaList,
    default_features: &BTreeSet<String>,
) -> Vec<String> {
    let Some((condition, metas)) = cfg_attr_payload(list, default_features) else {
        return Vec::new();
    };
    if condition == Some(false) {
        return Vec::new();
    }
    let mut names = Vec::new();
    for meta in &metas {
        let Some(name) = meta.path().segments.last().map(|s| s.ident.to_string()) else {
            continue;
        };
        if name == "cfg_attr" {
            if let syn::Meta::List(inner) = meta {
                names.extend(cfg_attr_active_marker_names(inner, default_features));
            }
        } else {
            names.push(name);
        }
    }
    names
}

/// Whether an active `#[cfg_attr(condition, cfg(inner_condition), ...)]`
/// injects an exclusion this scan can PROVE — `condition` DEFINITELY
/// resolves true (so the injected attribute really is present) AND the
/// injected `cfg(...)`'s own predicate DEFINITELY resolves false. Verified
/// directly: a real build of `#[cfg_attr(feature = "outer", cfg(feature =
/// "inner"))] fn show() { ... }` with only `outer` enabled reports `show`
/// as "configured out," rustc's own diagnostic naming the injected
/// `cfg(...)` as the reason. Recurses into a nested `cfg_attr(...)` meta
/// the same way [`cfg_attr_active_marker_names`] does, for the identical
/// reason.
///
/// Every condition along the way must be DEFINITE, matching this scan's
/// standing conservative default everywhere else (only a PROVEN exclusion
/// ever excludes): an unresolvable outer condition, or an unresolvable
/// injected predicate, must not force an exclusion that might not be real —
/// the opposite direction from [`cfg_attr_active_marker_names`], which stays
/// conservative by keeping a marker VISIBLE on the same kind of
/// uncertainty, since here the dangerous mistake is excluding a route that
/// still compiles in, not crediting a phantom one (Codex review on #2739,
/// round 29, P2).
fn cfg_attr_injects_a_false_cfg(list: &syn::MetaList, default_features: &BTreeSet<String>) -> bool {
    let Some((condition, metas)) = cfg_attr_payload(list, default_features) else {
        return false;
    };
    if condition != Some(true) {
        return false;
    }
    metas.iter().any(|meta| {
        let syn::Meta::List(inner) = meta else {
            return false;
        };
        if inner.path.is_ident("cfg") {
            return syn::parse2::<CfgPredicate>(inner.tokens.clone())
                .is_ok_and(|pred| !pred.eval(default_features));
        }
        inner.path.is_ident("cfg_attr") && cfg_attr_injects_a_false_cfg(inner, default_features)
    })
}

/// Build an [`EdgeFn`] when `attrs` contains an `#[edge]` marker and no
/// `#[cfg(...)]` on the same function definitely excludes it.
fn edge_fn(
    attrs: &[syn::Attribute],
    sig: &syn::Signature,
    file: &str,
    crate_root: &str,
    module_path: &[String],
    default_features: &BTreeSet<String>,
) -> Option<EdgeFn> {
    let names = attr_names_including_cfg_attr(attrs, default_features);
    if !names.iter().any(|n| n == "edge") {
        return None;
    }
    // Several `#[cfg(...)]` attributes on one function are ANDed, like real
    // Rust: any one of them resolving to definitely false excludes it — and
    // so does a `cfg(...)` injected by an ACTIVE `cfg_attr(...)` (Codex
    // review on #2739, round 29, P2).
    let cfg_excludes = attrs
        .iter()
        .any(|attr| attr_cfg_excludes(attr, default_features));
    if cfg_excludes {
        return None;
    }
    let guards: Vec<String> = names
        .into_iter()
        .filter(|n| EDGE_GUARD_ATTRS.contains(&n.as_str()))
        .collect();
    Some(EdgeFn {
        name: sig.ident.to_string(),
        file: file.to_owned(),
        line: sig.ident.span().start().line,
        guards,
        module_path: module_path.to_vec(),
        crate_root: crate_root.to_owned(),
    })
}

/// A `#[cfg(...)]` predicate this scan can fully resolve: built only from
/// `feature = "x"` leaves and the stable `true`/`false` boolean-literal
/// predicates, combined with `not`, `all`, and `any`.
enum CfgPredicate {
    Feature(String),
    Bool(bool),
    Not(Box<Self>),
    All(Vec<Self>),
    Any(Vec<Self>),
}

impl syn::parse::Parse for CfgPredicate {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        // `syn::Ident`'s own `Parse` impl rejects `true`/`false` outright
        // (verified directly — see `TargetCfgPredicate::parse`'s own doc
        // for the exact experiment); `IdentExt::parse_any` has no such
        // keyword restriction, which is what makes the `true`/`false`
        // branch below reachable at all (Codex review on #2739, round 22,
        // P2 — otherwise dead code).
        let ident: syn::Ident = syn::ext::IdentExt::parse_any(input)?;
        if ident == "feature" {
            input.parse::<syn::Token![=]>()?;
            let lit: syn::LitStr = input.parse()?;
            return Ok(Self::Feature(lit.value()));
        }
        // `true`/`false` tokenize as a plain `Ident`, not a `syn::Lit`
        // (verified directly), and are Cargo's own stable constant cfg
        // predicates — `#[cfg(false)]` unconditionally excludes an item,
        // `#[cfg(true)]` unconditionally keeps it, same as an empty `cfg()`
        // wrapped in `all`/`any`. Without recognizing them, a `#[cfg(false)]`
        // `#[edge]` handler (or a `#[cfg(false)]`-disabled wiring function's
        // `edge_routes![...]`) fell through to "unresolvable", which this
        // scan's own safe-direction default treats as "not excluded" —
        // wrongly crediting a route/registration the real build compiles out
        // entirely (Codex review on #2739, round 22, P2).
        if ident == "true" || ident == "false" {
            return Ok(Self::Bool(ident == "true"));
        }
        if ident == "not" {
            let content;
            syn::parenthesized!(content in input);
            let inner: Self = content.parse()?;
            return Ok(Self::Not(Box::new(inner)));
        }
        if ident == "all" || ident == "any" {
            let content;
            syn::parenthesized!(content in input);
            let list =
                syn::punctuated::Punctuated::<Self, syn::Token![,]>::parse_terminated(&content)?;
            let parts: Vec<Self> = list.into_iter().collect();
            return Ok(if ident == "all" {
                Self::All(parts)
            } else {
                Self::Any(parts)
            });
        }
        // Anything else — `target_os`, `debug_assertions`, a name this scan
        // does not know — is not part of the resolvable grammar.
        Err(input.error("cfg predicate not resolvable by this scan"))
    }
}

impl CfgPredicate {
    /// Evaluate against the crate's default-on feature set. Only called after
    /// a full, successful parse, so every leaf is a plain feature name.
    fn eval(&self, default_features: &BTreeSet<String>) -> bool {
        match self {
            Self::Feature(name) => default_features.contains(name),
            Self::Bool(value) => *value,
            Self::Not(inner) => !inner.eval(default_features),
            Self::All(parts) => parts.iter().all(|p| p.eval(default_features)),
            Self::Any(parts) => parts.iter().any(|p| p.eval(default_features)),
        }
    }
}

/// Try to fully resolve one `#[cfg(...)]` attribute against `default_features`.
///
/// `None` means "stay conservative": a non-`cfg` attribute, a predicate this
/// scan does not parse (`target_os`, ...), or one with trailing tokens it does
/// not understand. A caller treats `None` the same as `Some(true)` — kept in
/// the scan — so only `Some(false)` ever excludes a function.
fn eval_cfg_attr(attr: &syn::Attribute, default_features: &BTreeSet<String>) -> Option<bool> {
    if !attr.path().is_ident("cfg") {
        return None;
    }
    let syn::Meta::List(list) = &attr.meta else {
        return None;
    };
    syn::parse2::<CfgPredicate>(list.tokens.clone())
        .ok()
        .map(|pred| pred.eval(default_features))
}

/// Whether `attr`, on its own, excludes whatever it's attached to: either a
/// literal `#[cfg(...)]` that [`eval_cfg_attr`] resolves as definitely
/// false, or an active `#[cfg_attr(condition, cfg(inner), ...)]` whose
/// injected `cfg(...)` [`cfg_attr_injects_a_false_cfg`] resolves as
/// definitely false the same way. Every AST-level cfg-exclusion check in
/// this module should go through this rather than `eval_cfg_attr` alone —
/// `mod`-level exclusion checks (inline-module discovery in [`scan_items`]
/// and out-of-line discovery in [`collect_out_of_line_mods`]) previously
/// only checked the literal form, so `#[cfg_attr(feature = "outer",
/// cfg(feature = "inner"))] mod
/// premium { ... }` with only `outer` on kept scanning a module real Rust
/// strips out entirely (verified directly via a real build: the module
/// name itself becomes unresolved) — both its `#[edge]` handlers and its
/// `edge_routes![]` registrations were credited as active (Codex review on
/// #2739, round 33, P2; [`edge_fn`]'s own version of this check, added in
/// round 29, was the only one already correct).
fn attr_cfg_excludes(attr: &syn::Attribute, default_features: &BTreeSet<String>) -> bool {
    if eval_cfg_attr(attr, default_features) == Some(false) {
        return true;
    }
    let syn::Meta::List(list) = &attr.meta else {
        return false;
    };
    attr.path().is_ident("cfg_attr") && cfg_attr_injects_a_false_cfg(list, default_features)
}

/// Find every `edge_routes![...]` invocation in a token stream and record the
/// handler identifiers it registers, resolved against the module the
/// invocation itself is written in. `crate_root` is that same invocation's
/// own crate identity (see [`EdgeFn::crate_root`]), recorded alongside each
/// entry so [`is_registered`] can require it to match a function's own
/// crate before crediting a same-crate-relative reference.
///
/// Token-level rather than AST-level because the invocation is equally valid in
/// item position, inside a function body, or nested in another macro's group —
/// and because the registration list is a plain path list, so no parsing beyond
/// "last identifier of each comma-separated entry" is needed. `mod <name> {
/// ... }` nesting is tracked the same way, by direct token pattern rather than
/// `syn`, purely so a `self`/`super`-relative entry (`edge_routes![self::show]`)
/// resolves against the right module instead of comparing "self" or "super"
/// literally, which never matches a real module path (Codex review on #2739,
/// round 4, P2). An out-of-line `mod <name>;` has no group to recurse into
/// here; its own registrations are covered when that file is scanned on its
/// own, seeded from its file-derived module path instead (see
/// [`crate_context_from_file`]).
///
/// `default_features` lets an inline `mod`'s own `#[cfg(...)]` exclude its
/// registrations the same way [`scan_items`] already excludes its handlers:
/// without this, `#[cfg(feature = "premium")] mod wiring { edge_routes![show]; }`
/// still recorded `show` as registered with `premium` off, even though real
/// Rust strips the whole module (and its `edge_routes![]` call) out — a
/// capsule that genuinely serves nothing could then build and report `show`
/// as served (Codex review on #2739, round 15, P2).
fn collect_registrations(
    stream: &TokenStream,
    crate_root: &str,
    module_path: &mut Vec<String>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    let mut index = 0;
    while index < trees.len() {
        if let Some(next_index) = skip_cfg_excluded_statement(&trees, index, default_features) {
            index = next_index;
            continue;
        }
        if let TokenTree::Ident(ident) = &trees[index]
            && ident == "mod"
            && let Some(TokenTree::Ident(name)) = trees.get(index + 1)
            && let Some(TokenTree::Group(group)) = trees.get(index + 2)
            && group.delimiter() == Delimiter::Brace
        {
            // `#[cfg(feature = "premium")] pub mod wiring { ... }` has
            // `pub` (or `pub(crate)`/`pub(super)`/`pub(in path)`) between
            // the attribute and `mod`, same as `fn`/`impl` tolerate via
            // `skip_back_over_fn_modifiers` — checking immediately before
            // `mod` alone would see the visibility modifier instead of the
            // attribute and wrongly conclude nothing excludes it (Codex
            // review on #2739, round 27, P2).
            let attr_index = skip_back_over_fn_modifiers(&trees, index);
            if !preceding_cfg_excludes(&trees, attr_index, default_features) {
                module_path.push(name.to_string());
                collect_registrations(
                    &group.stream(),
                    crate_root,
                    module_path,
                    default_features,
                    scan,
                );
                module_path.pop();
            }
            index += 3;
            continue;
        }
        if let TokenTree::Ident(ident) = &trees[index]
            && ident == "impl"
            && let Some(body_index) = simple_impl_body_index(&trees, index)
        {
            // `#[cfg(feature = "premium")] impl Routes { fn wire() {
            // edge_routes![show]; } }` — the catch-all `Group` branch below
            // only recognizes a cfg attribute with NOTHING between it and
            // the group, but an `impl` block's brace is always preceded by
            // at least the implementing type (`Routes`, or `Trait for
            // Type`), so that branch never saw the attribute and
            // unconditionally recursed into a block real Rust strips
            // entirely with the feature off, crediting an inactive handler
            // as registered (Codex review on #2739, round 23, P2).
            let attr_index = skip_back_over_fn_modifiers(&trees, index);
            if let TokenTree::Group(body) = &trees[body_index]
                && !preceding_cfg_excludes(&trees, attr_index, default_features)
            {
                collect_registrations(
                    &body.stream(),
                    crate_root,
                    module_path,
                    default_features,
                    scan,
                );
            }
            index = body_index + 1;
            continue;
        }
        if let TokenTree::Ident(ident) = &trees[index]
            && ident == "fn"
            && let Some(body_index) = simple_fn_body_index(&trees, index)
        {
            // `#[cfg(feature = "premium")] fn wire() { edge_routes![show]; }`
            // — a cfg'd-out FUNCTION, not just a cfg'd-out `mod { ... }`,
            // must also exclude its own `edge_routes![]` calls. Only the
            // simple `fn name(...) [-> plain return type] { ... }` shape is
            // resolved (no generics, no `where` clause, no nested groups in
            // the return type) — anything else falls through to the
            // ordinary walk below rather than risk mis-scanning a LATER,
            // unrelated item's body as this function's own (Codex review on
            // #2739, round 16, P2).
            let attr_index = skip_back_over_fn_modifiers(&trees, index);
            if let TokenTree::Group(body) = &trees[body_index]
                && !preceding_cfg_excludes(&trees, attr_index, default_features)
            {
                collect_registrations(
                    &body.stream(),
                    crate_root,
                    module_path,
                    default_features,
                    scan,
                );
            }
            index = body_index + 1;
            continue;
        }
        match &trees[index] {
            TokenTree::Group(group) => {
                // A group with NOTHING between it and a preceding
                // `#[cfg(...)]` — a bare cfg'd block statement, e.g. — must
                // honor that cfg the same way the `mod { ... }` case above
                // does. This does not reach a function's body (there is
                // always a name and a parenthesized parameter list in
                // between, handled by the `fn` special case above instead).
                if !preceding_cfg_excludes(&trees, index, default_features) {
                    collect_registrations(
                        &group.stream(),
                        crate_root,
                        module_path,
                        default_features,
                        scan,
                    );
                }
            }
            TokenTree::Ident(ident) if ident == "edge_routes" => {
                let bang = matches!(
                    trees.get(index + 1),
                    Some(TokenTree::Punct(p)) if p.as_char() == '!'
                );
                if bang
                    && let Some(TokenTree::Group(group)) = trees.get(index + 2)
                    && group.delimiter() != Delimiter::None
                    // `#[cfg(feature = "premium")] edge_routes![show];` — a
                    // cfg attribute directly on the macro invocation
                    // STATEMENT itself (legal Rust: statement attributes
                    // apply to a macro-invocation statement same as any
                    // other), not just on an enclosing `mod {}`/`fn {}` —
                    // neither of which this specific token position sits
                    // inside. Without this check, a disabled route's
                    // registration was still credited, letting a build
                    // with zero real handlers pass preflight (Codex review
                    // on #2739, round 22, P2).
                    && !preceding_cfg_excludes(&trees, index, default_features)
                {
                    scan.registrations += 1;
                    for name in registered_idents(&group.stream()) {
                        for candidate in registration_candidates(&name, module_path) {
                            scan.registered.insert((crate_root.to_owned(), candidate));
                        }
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
}

/// Walk backward from `fn_index` (the `fn` keyword) over any of `pub`,
/// `pub(...)`, `async`, `const`, `unsafe`, and `extern`/`extern "ABI"` —
/// ordinary function modifiers — and return the index of whatever comes
/// before all of them. [`preceding_cfg_excludes`] is meant to be called at
/// that index, not at `fn_index` itself: `#[cfg(feature = "premium")] pub
/// async fn wire() { ... }` has `async`/`pub` between the attribute and
/// `fn`, and checking immediately before `fn` alone would see those
/// modifiers instead of the attribute and wrongly conclude nothing excludes
/// it (Codex review on #2739, round 18, P2).
fn skip_back_over_fn_modifiers(trees: &[TokenTree], fn_index: usize) -> usize {
    let mut i = fn_index;
    while let Some(prev) = i.checked_sub(1) {
        match &trees[prev] {
            TokenTree::Ident(ident)
                if matches!(
                    ident.to_string().as_str(),
                    "pub" | "async" | "const" | "unsafe" | "extern"
                ) =>
            {
                i = prev;
            }
            // `pub(crate)` / `pub(super)` / `pub(in path)`: the parenthesized
            // group only belongs to a modifier if `pub` precedes it.
            TokenTree::Group(group) if group.delimiter() == Delimiter::Parenthesis => {
                if prev >= 1 && matches!(&trees[prev - 1], TokenTree::Ident(id) if id == "pub") {
                    i = prev - 1;
                } else {
                    break;
                }
            }
            // `extern "C"`: the ABI string only belongs to a modifier if
            // `extern` precedes it.
            TokenTree::Literal(_) => {
                if prev >= 1 && matches!(&trees[prev - 1], TokenTree::Ident(id) if id == "extern") {
                    i = prev - 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    i
}

/// Walk forward from `i` over one or more `#[...]` attributes, returning
/// whether any is a `cfg(...)` that resolves false and the index right
/// after the attribute run (unchanged if `trees[i]` is not `#` followed by a
/// bracket group).
fn skip_leading_cfg_attrs(
    trees: &[TokenTree],
    mut i: usize,
    default_features: &BTreeSet<String>,
) -> (bool, usize) {
    let mut excluded = false;
    while let Some(TokenTree::Punct(hash)) = trees.get(i) {
        if hash.as_char() != '#' {
            break;
        }
        let Some(TokenTree::Group(bracket)) = trees.get(i + 1) else {
            break;
        };
        if bracket.delimiter() != Delimiter::Bracket {
            break;
        }
        if cfg_attribute_group_is_false(bracket, default_features) {
            excluded = true;
        }
        i += 2;
    }
    (excluded, i)
}

/// The forward mirror of [`skip_back_over_fn_modifiers`]: the index right
/// after any leading `pub`/`pub(...)`/`async`/`const`/`unsafe`/`extern
/// "ABI"` modifiers starting at `i`, in whatever combination those allow.
fn skip_forward_over_item_modifiers(trees: &[TokenTree], mut i: usize) -> usize {
    loop {
        match trees.get(i) {
            Some(TokenTree::Ident(ident))
                if matches!(
                    ident.to_string().as_str(),
                    "pub" | "async" | "const" | "unsafe" | "extern"
                ) =>
            {
                i += 1;
            }
            Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => {
                if i >= 1 && matches!(&trees[i - 1], TokenTree::Ident(id) if id == "pub") {
                    i += 1;
                } else {
                    break;
                }
            }
            Some(TokenTree::Literal(_)) => {
                if i >= 1 && matches!(&trees[i - 1], TokenTree::Ident(id) if id == "extern") {
                    i += 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    i
}

/// Whether the item starting at `i` — after any leading attributes have
/// already been skipped — is a `mod`/`fn`/`impl` (itself, or behind any
/// combination of [`skip_forward_over_item_modifiers`]'s modifiers). Those
/// three are each handled by their own dedicated logic above, which looks
/// BACKWARD from the keyword and already copes with modifiers in between;
/// the generic statement-level cfg suppression this guards must defer to
/// them rather than also claim the same statement.
fn is_specially_handled_item(trees: &[TokenTree], i: usize) -> bool {
    matches!(
        trees.get(skip_forward_over_item_modifiers(trees, i)),
        Some(TokenTree::Ident(ident)) if matches!(ident.to_string().as_str(), "mod" | "fn" | "impl")
    )
}

/// If `trees[index]` begins one or more `#[...]` attributes that resolve to
/// an excluded `cfg(...)`, and the item they attach to is not one of the
/// specially-handled `mod`/`fn`/`impl` items ([`is_specially_handled_item`],
/// which already look backward for their own attribute) nor a bare `Group`
/// (the [`collect_registrations`] catch-all already covers that case),
/// returns the index just past the whole excluded statement — up to and
/// including its own top-level `;` — so the caller can skip it without
/// recursing into any group nested inside it. Returns `None` when no such
/// statement starts here, meaning the caller should fall through to its
/// normal per-token handling instead.
///
/// `#[cfg(feature = "premium")] routes.extend(edge_routes![show]);` is
/// exactly the shape this exists for: the macro invocation sits nested
/// inside an ordinary expression/let statement, not immediately adjacent to
/// the attribute, so [`collect_registrations`]'s `Group` catch-all — which
/// only ever checked whether ITS OWN position was immediately preceded by
/// the attribute — never saw it and recursed unconditionally, crediting a
/// registration real Rust strips out along with the rest of the statement
/// (Codex review on #2739, round 24, P2). A `;` nested inside one of the
/// statement's own groups belongs to a different token stream entirely and
/// is never visited at this level, so watching for a top-level one here
/// cannot run past this statement into the next.
fn skip_cfg_excluded_statement(
    trees: &[TokenTree],
    index: usize,
    default_features: &BTreeSet<String>,
) -> Option<usize> {
    if !(matches!(&trees[index], TokenTree::Punct(p) if p.as_char() == '#')
        && matches!(trees.get(index + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket))
    {
        return None;
    }
    let (excluded, attrs_end) = skip_leading_cfg_attrs(trees, index, default_features);
    if !excluded || is_specially_handled_item(trees, attrs_end) {
        return None;
    }
    if matches!(trees.get(attrs_end), Some(TokenTree::Group(_))) {
        return Some(attrs_end + 1);
    }
    // `if`/`match`/`while`/`loop`/`for`/`unsafe`, a bare anonymous `const {
    // ... }` block, and a labeled block/loop, used as a whole statement,
    // never end in `;` — real Rust's grammar simply doesn't require or
    // allow one there (a trailing `;` after `if cond { .. }` would be an
    // EXTRA, separate empty statement, not part of it). The generic
    // scan-to-`;` fallback below cannot see that: given `#[cfg(...)] if
    // cond { edge_routes![premium] }` immediately followed by an ACTIVE
    // `edge_routes![show];`, it would keep consuming tokens right through
    // the excluded `if`'s own closing brace and into the next, unrelated,
    // still-real statement, stopping only at ITS semicolon — dropping a
    // genuine registration the real build still compiles in (Codex review
    // on #2739, round 25, P2, extended in round 28, P2 for `const { ... }`
    // and labeled blocks/loops, the identical ambiguity in a different
    // spelling).
    if let Some(block_end) = statement_without_semicolon_end(trees, attrs_end) {
        return Some(block_end + 1);
    }
    // A cfg'd-out MATCH ARM (`#[cfg(feature = "premium")] true =>
    // edge_routes![premium],`) is comma-delimited, not semicolon-delimited
    // — this same function also bounds a match's arms, since
    // `collect_registrations` recurses into a `match { ... }`'s body brace
    // the same generic way it does any other block. A top-level (ungrouped)
    // comma essentially never appears within one ordinary function-body
    // STATEMENT's own tokens — every legitimate comma-bearing construct
    // (tuples, call arguments, array/struct literals) is always inside its
    // own Paren/Bracket/Brace group, invisible at this flat level — so it
    // unambiguously marks a match arm's own boundary here. Without this,
    // the scan found no `;` anywhere in the arms' flattened token stream
    // and consumed every remaining arm, including a later, unrelated,
    // still-active one's own `edge_routes![show]` (Codex review on #2739,
    // round 42, P2).
    // An ungrouped generic-argument comma (`Result<(), ()>`) is ALSO a
    // top-level, ungrouped comma — angle brackets are plain `<`/`>` Punct
    // tokens, not their own Group delimiter — so the comma-as-terminator
    // rule above needs the same angle-bracket-depth tracking every other
    // generics-aware scan in this file already uses, or it stops
    // mid-type-argument-list instead of at the statement's real end,
    // recursing into the excluded statement's own initializer block
    // (verified directly via a real build: `#[cfg(false)] let _:
    // Result<(), ()> = { show(); Ok(()) };` drops the whole statement,
    // `show()` included). Depth is clamped at 0 rather than erroring out on
    // a negative count (as the narrower generics-only scans elsewhere do):
    // unlike those, this fallback also runs over ordinary comparison/shift
    // `>`/`<` operators with no matching partner, and clamping keeps a
    // later, real top-level comma or semicolon recognized regardless (Codex
    // review on #2739, round 44, P2).
    //
    // A semicolon always terminates regardless of depth (round 45, P2), but
    // a comma cannot: a genuine top-level comma DOES legitimately occur
    // inside a real, still-open generic/turbofish argument list
    // (`Result<(), ()>`, `foo::<A, B>()`), so comma-termination still needs
    // real depth tracking. Simply counting every `<`/`>` (as semicolons can
    // get away with) reintroduces the same eternal-lock bug in the other
    // direction: an unmatched comparison `<` with no partner anywhere ahead
    // (`#[cfg(false)] 0 => 1 < 2,`) would raise depth forever and swallow
    // every following, still-active match arm's own comma — verified
    // directly via a real build that this arm shape compiles fine and a
    // following arm remains real Rust. So `<`/`>` only affect `angle_depth`
    // here when `matched_angle_bracket_positions` confirms they belong to an
    // actual matched pair somewhere in the remaining tokens; a stray,
    // never-closed `<` (or a stray `>` with no opener, e.g. from `1 > 2`) is
    // just an ordinary pass-through token instead (Codex review on #2739,
    // round 46, P2).
    let matched_angles = matched_angle_bracket_positions(trees, attrs_end);
    let mut i = attrs_end;
    let mut angle_depth: u32 = 0;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Punct(p) if p.as_char() == '-' => {
                let is_arrow =
                    matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>');
                i += usize::from(is_arrow) + 1;
            }
            TokenTree::Punct(p) if p.as_char() == '<' && matched_angles.contains(&i) => {
                angle_depth += 1;
                i += 1;
            }
            TokenTree::Punct(p) if p.as_char() == '>' && matched_angles.contains(&i) => {
                angle_depth = angle_depth.saturating_sub(1);
                i += 1;
            }
            // A `;` always ends the statement regardless of `angle_depth`,
            // unlike a comma: a genuine top-level `;` can never actually
            // occur while a real generic argument list is still open at
            // this flat scan depth (the only place a semicolon legitimately
            // appears inside `<...>` is a const-generic block expression,
            // e.g. `Foo<{ let x = 1; x }>`, and that block's own braces make
            // it one opaque Group token here, never reached). Without this,
            // a comparison/shift operator with no matching partner (`1 <
            // 2`) left `angle_depth` permanently elevated, so the scan ran
            // straight past the statement's real semicolon into a
            // following, unrelated, still-active statement — verified
            // directly via a real build that `#[cfg(false)] let disabled =
            // 1 < 2;` compiles fine, dropping the whole statement (Codex
            // review on #2739, round 45, P2).
            TokenTree::Punct(p) if p.as_char() == ';' => {
                i += 1;
                break;
            }
            TokenTree::Punct(p) if p.as_char() == ',' => {
                i += 1;
                if angle_depth == 0 {
                    break;
                }
            }
            _ => i += 1,
        }
    }
    Some(i)
}

/// The set of token indices, within `trees[start..]`, that are one half of a
/// genuinely matched `<`/`>` pair — computed with the standard stack-based
/// bracket-matching algorithm (push each `<`, pop on `>`; a `>` with nothing
/// to pop, and any `<` left on the stack at the end, are both stray and
/// excluded), so an ordinary comparison/shift operator with no matching
/// partner anywhere ahead is never mistaken for one half of an open generic
/// argument list. `->` is skipped as the atomic two-token unit it is, the
/// same way every other generics-aware scan in this file treats it, so its
/// own `>` half is never treated as a stray closer.
///
/// A bare `=>` (a match arm's own fat arrow — the ONLY way this token can
/// appear here at all, since a NESTED match's own arms live inside its own
/// opaque body `Group`, invisible at this flat level) can NEVER legitimately
/// occur inside a real, still-open generic argument list — unlike `->`,
/// which a trait-bound return type can nest (`Foo<dyn Fn() -> u32>`), so
/// `->` is skipped as a unit without disturbing `stack`, but crossing a
/// `=>` clears every still-open entry on it outright. Without this, a
/// stray `<` opened in one excluded arm's own comparison could pair with
/// ANY later `>` reachable by simply scanning far enough forward — not just
/// a later arm's own fat arrow (round 46's first fix for that ONE specific
/// case), but an entirely unrelated real comparison several arms further
/// on (`#[cfg(false)] 0 => 1 < 2, _ => show().len() > 0,` — verified
/// directly via a real build that this compiles, with the second arm
/// remaining real, active code) — since nothing bounded the search to the
/// excluded arm's own extent. A `=>` is the one token that provably can
/// never appear inside a real generic here, so treating it as a hard reset
/// bounds the match to at most one arm's own tokens, exactly the scope this
/// function's own `start` parameter already bounds it to on the other side
/// (Codex review on #2739, round 47, P2).
fn matched_angle_bracket_positions(trees: &[TokenTree], start: usize) -> BTreeSet<usize> {
    let mut matched = BTreeSet::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut i = start;
    while i < trees.len() {
        match trees.get(i) {
            Some(TokenTree::Punct(p))
                if p.as_char() == '-'
                    && matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>') =>
            {
                i += 2;
                continue;
            }
            Some(TokenTree::Punct(p))
                if p.as_char() == '='
                    && matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>') =>
            {
                stack.clear();
                i += 2;
                continue;
            }
            Some(TokenTree::Punct(p)) if p.as_char() == '<' => stack.push(i),
            Some(TokenTree::Punct(p)) if p.as_char() == '>' => {
                if let Some(open) = stack.pop() {
                    matched.insert(open);
                    matched.insert(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    matched
}

/// The end of a block-like control-flow statement (`if`/`match`/`while`/
/// `loop`/`for`/`unsafe`) starting right after its own keyword at `i` —
/// these never need, or allow, a trailing `;` to end the statement, unlike
/// everything else [`skip_cfg_excluded_statement`] assumes does.
///
/// Scans forward treating any token — including a whole `Group` — as an
/// opaque pass-through except the FIRST Brace-delimited one, which ends the
/// primary block. That is safe because Rust's grammar forbids a bare,
/// un-parenthesized struct-literal (or any other top-level `{`) in an
/// `if`/`while`/`for` condition or a `match` scrutinee, precisely to avoid
/// this exact ambiguity with the block that follows — so the first Brace
/// group reached here is unambiguously the real body, never part of the
/// condition. An `if`'s own `else`/`else if` chain, which shares the same
/// "no `;` needed" property, is folded in recursively; the other four
/// keywords have no such chain.
fn control_flow_block_end(trees: &[TokenTree], mut i: usize) -> Option<usize> {
    let start = i;
    loop {
        match trees.get(i) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => {
                // A bare block expression (`if { true } { .. }`, real, valid
                // Rust — verified directly via a real build, warned as
                // "unnecessary braces" but not rejected) can stand as the
                // WHOLE condition or scrutinee itself, with no other tokens
                // before it: unlike a struct literal, a bare, un-parenthesized
                // `{ ... }` with no preceding path is not what Rust's
                // "no struct literal in condition" restriction forbids, so
                // it's read as the condition/scrutinee expression, and the
                // real body's own brace follows directly after it. This is
                // only possible for `if`/`while`/`match`, which always need a
                // condition/scrutinee before their body; `unsafe`/`loop` have
                // no condition at all (their body brace comes immediately
                // after the keyword, so `i == start` there IS the real
                // body), and `for`'s pattern never starts with a bare `{`.
                // Without this, the whole cfg'd-out `if { true } {
                // edge_routes![show]; }` had its condition block mistaken for
                // the real body, leaving the actual body an ordinary,
                // unexcluded sibling group that credited `show` (Codex review
                // on #2739, round 45, P2).
                if i == start
                    && matches!(trees.get(start.wrapping_sub(1)), Some(TokenTree::Ident(id)) if matches!(id.to_string().as_str(), "if" | "while" | "match"))
                {
                    i += 1;
                    continue;
                }
                // A brace-delimited macro invocation in the condition or
                // scrutinee (`match value!{} { .. }`) has its OWN brace
                // group — the macro's argument list — directly preceded by
                // `!`, immediately before the real body's own brace arrives
                // right after it. Verified directly via a real build: the
                // whole cfg'd-out `match value!{} { _ => show() }` drops
                // `show()` along with it, so crediting a registration
                // inside what this loop mistook for the body (the macro's
                // empty `{}` argument group) is wrong the same way a
                // struct-pattern brace is. A body's own opening brace is
                // never itself preceded by `!` in valid Rust grammar (that
                // only ever precedes a macro invocation's own delimiter),
                // so this check cannot misfire on a genuine body (Codex
                // review on #2739, round 44, P2).
                if i > 0
                    && matches!(trees.get(i - 1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
                {
                    i += 1;
                    continue;
                }
                // An async block used in the condition or scrutinee (`if
                // async { predicate().await }.await { .. }`, real, valid
                // Rust — verified directly via a real build) has its OWN
                // brace group immediately preceded by the `async` keyword
                // itself, before the real body's own brace arrives later.
                // A genuine body's own opening brace is never itself
                // preceded by the literal keyword `async` in valid Rust
                // grammar (there is no "async if"), so this cannot misfire
                // on a real body either (Codex review on #2739, round 45,
                // P2). An `async move { .. }` block (also real, valid Rust,
                // verified directly) inserts `move` between the keyword and
                // the brace, so the brace is preceded by `move`, not `async`
                // directly, and needs the identical two-token lookback
                // (Codex review on #2739, round 46, P2).
                if i > 0 && matches!(trees.get(i - 1), Some(TokenTree::Ident(id)) if id == "async")
                {
                    i += 1;
                    continue;
                }
                if i > 1
                    && matches!(trees.get(i - 1), Some(TokenTree::Ident(id)) if id == "move")
                    && matches!(trees.get(i - 2), Some(TokenTree::Ident(id)) if id == "async")
                {
                    i += 1;
                    continue;
                }
                // An inline `const { .. }` block used in the condition or
                // scrutinee (`if const { true } { .. }`, real, valid Rust —
                // verified directly via a real build) has its OWN brace
                // group immediately preceded by the `const` keyword itself,
                // the identical shape as the `async`/`async move` cases
                // above. A genuine body's own opening brace is never itself
                // preceded by the literal keyword `const` in valid Rust
                // grammar (there is no "const if"), so this cannot misfire
                // on a real body either (Codex review on #2739, round 46,
                // P2).
                if i > 0 && matches!(trees.get(i - 1), Some(TokenTree::Ident(id)) if id == "const")
                {
                    i += 1;
                    continue;
                }
                // An `unsafe { .. }` block used in the condition or
                // scrutinee (`if unsafe { true } { .. }`, real, valid Rust —
                // verified directly via a real build, warned as
                // "unnecessary `unsafe` block" but not rejected) is the
                // identical shape as `async`/`const` above, EXCEPT `unsafe`
                // is also one of the SIX keywords this very function is
                // itself dispatched for (`statement_without_semicolon_end`
                // calls `control_flow_block_end(trees, i + 1)` right after
                // an `unsafe` used as the WHOLE outer statement, which has
                // no condition at all — its body brace comes immediately
                // after the keyword). So `i == start` there IS the real
                // body (the same reasoning the bare-block-condition check
                // above already excludes `unsafe` for), and only `i >
                // start` — meaning some condition tokens of an ENCLOSING
                // if/while/match already came before this nested `unsafe`
                // block — signals the condition-block shape (Codex review
                // on #2739, round 47, P2).
                if i > start
                    && matches!(trees.get(i - 1), Some(TokenTree::Ident(id)) if id == "unsafe")
                {
                    i += 1;
                    continue;
                }
                // A struct/tuple-struct PATTERN can itself contain a brace
                // group — `if let Foo { x } = value() { .. }`, `for Foo { x }
                // in items { .. }` — so the first Brace reached is not always
                // the real body: unlike an expression, Rust's grammar does
                // not forbid an unparenthesized struct pattern in an
                // if-let/while-let condition or a for-loop's pattern
                // (verified directly via a real build). A pattern's own
                // brace is always followed by more of the condition — `=`
                // for an if-let/while-let, `in` for a for-loop, or `|` before
                // another alternative in an unparenthesized or-pattern
                // (`if let Foo { x } | Bar { x } = value() { .. }`, also
                // verified directly) — before the real body brace arrives;
                // the real body brace, reached in statement position, is
                // never followed by any of the three (only possibly `else`,
                // handled below), so this alone tells them apart without
                // parsing the pattern itself (Codex review on #2739, round
                // 38, P2, extended in round 39, P2 for or-patterns).
                if matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '=')
                    || matches!(trees.get(i + 1), Some(TokenTree::Ident(id)) if id == "in")
                    || matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '|')
                {
                    i += 1;
                    continue;
                }
                break;
            }
            Some(_) => i += 1,
            None => return None,
        }
    }
    if matches!(trees.get(i + 1), Some(TokenTree::Ident(id)) if id == "else") {
        return match trees.get(i + 2) {
            Some(TokenTree::Ident(id)) if id == "if" => control_flow_block_end(trees, i + 3),
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => Some(i + 2),
            _ => Some(i),
        };
    }
    Some(i)
}

/// The end of a statement or item starting at `i` that this scan's generic
/// scan-to-`;` fallback cannot correctly bound on its own, or `None` when
/// `trees[i..]` doesn't start one of these shapes at all (the caller then
/// falls back to that generic scan).
///
/// Covers `if`/`match`/`while`/`loop`/`for`/`unsafe` (via
/// [`control_flow_block_end`]); a bare, anonymous `const { ... }` block
/// (stable since Rust 1.79, verified directly via a real build that it
/// compiles as a statement with no trailing `;` needed) — distinguished
/// from an ordinary `const NAME: TYPE = ...;` ITEM by checking whether the
/// very next token is a Brace group directly, since an item's own name
/// always intervenes and a block never has one; a labeled block or loop
/// (`'label: { ... }`, `'label: loop { ... }`, `'label: while ... { ... }`,
/// `'label: for ... in ... { ... }` — verified directly that a bare labeled
/// block also compiles as a semicolon-free statement), which shares the
/// exact same property as its unlabeled form and is folded in via the same
/// two helpers (Codex review on #2739, round 28, P2); a `struct`/`enum`/
/// `union`/`trait` item (possibly behind `pub`/`pub(...)`, skipped via
/// [`skip_forward_over_item_modifiers`]) via
/// [`braced_or_semicolon_item_end`] — unlike everything else handled here,
/// such an item does NOT always skip a trailing `;` (a tuple or unit
/// struct still needs one), but it's included in this same dispatch
/// because the naive scan-to-`;` fallback gets it wrong in the OTHER
/// direction: a struct/enum/union/trait with a braced body has no `;` to
/// find, so that fallback runs straight through it into the next,
/// unrelated, still-real statement (Codex review on #2739, round 30, P2);
/// and `macro_rules! name { ... }` — the identical brace-or-semicolon
/// ambiguity in a different spelling, verified directly via a real build:
/// the brace form (`macro_rules! name { ... }`) needs no trailing `;`, but
/// the parenthesized/bracketed forms (`macro_rules! name(...);`,
/// `macro_rules! name[...];`) do, so it reuses
/// [`braced_or_semicolon_item_end`] directly (starting right after
/// `macro_rules` itself — the `!` and the macro's own name are just more
/// pass-through tokens to that scan) rather than a dedicated helper (Codex
/// review on #2739, round 32, P2).
///
/// The index right after a simple path (`name`, `a::b::c`) starting at `i`,
/// where `trees[i]` is already known to be an `Ident` — used to look past a
/// macro invocation's qualifier (`crate::configure!`, `self::configure!`)
/// before checking for the `!` and brace-delimited group that follow.
/// Returns `i + 1` unchanged when no `::` continues the path.
fn simple_path_end(trees: &[TokenTree], i: usize) -> usize {
    let mut end = i + 1;
    while matches!(trees.get(end), Some(TokenTree::Punct(p)) if p.as_char() == ':')
        && matches!(trees.get(end + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
        && matches!(trees.get(end + 2), Some(TokenTree::Ident(_)))
    {
        end += 3;
    }
    end
}

fn statement_without_semicolon_end(trees: &[TokenTree], i: usize) -> Option<usize> {
    if let Some(TokenTree::Ident(ident)) = trees.get(i) {
        match ident.to_string().as_str() {
            "if" | "match" | "while" | "loop" | "for" | "unsafe" => {
                return control_flow_block_end(trees, i + 1);
            }
            "const" => {
                return matches!(trees.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace)
                    .then_some(i + 1);
            }
            "macro_rules" => {
                return braced_or_semicolon_item_end(trees, i + 1);
            }
            _ => {}
        }
        // ANY OTHER macro invocation statement using brace delimiters
        // (`configure! { ... }`, not `configure!(...)`/`configure![...]`) —
        // possibly through a qualified path (`crate::configure! { ... }`,
        // `self::configure! { ... }`, `a::b::configure! { ... }`, all
        // verified directly via a real build) — needs no trailing `;`
        // either. `macro_rules` above is the same rule with an extra
        // "macro's own name" token between `!` and the group; an ordinary
        // invocation has nothing between them, so this checks the group
        // directly, after skipping over any leading `path::segments::`.
        // None of the specific keywords above can themselves be followed by
        // `!` or `::` (they're reserved words, never valid path segments),
        // so there's no overlap with them (Codex review on #2739, round 36,
        // P2, extended in round 41, P2 for a qualified macro path).
        let path_end = simple_path_end(trees, i);
        if matches!(trees.get(path_end), Some(TokenTree::Punct(p)) if p.as_char() == '!')
            && matches!(trees.get(path_end + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace)
        {
            return Some(path_end + 1);
        }
    }
    if matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '\'')
        && matches!(trees.get(i + 1), Some(TokenTree::Ident(_)))
        && matches!(trees.get(i + 2), Some(TokenTree::Punct(p)) if p.as_char() == ':')
    {
        return match trees.get(i + 3) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => Some(i + 3),
            Some(TokenTree::Ident(ident))
                if matches!(ident.to_string().as_str(), "loop" | "while" | "for") =>
            {
                control_flow_block_end(trees, i + 4)
            }
            _ => None,
        };
    }
    let item_start = skip_forward_over_item_modifiers(trees, i);
    if let Some(TokenTree::Ident(ident)) = trees.get(item_start)
        && matches!(
            ident.to_string().as_str(),
            "struct" | "enum" | "union" | "trait"
        )
    {
        return braced_or_semicolon_item_end(trees, item_start + 1);
    }
    // A foreign block (`extern "C" { ... }`, bare `extern { ... }`) or an
    // async block used as a statement (`async { ... }`) leaves nothing but
    // the brace itself after its modifiers are skipped — neither has a
    // name/keyword of its own between the modifiers and the body, unlike
    // `struct`/`macro_rules`/etc above, and neither ever takes a trailing
    // `;` — verified directly via a real build. `unsafe extern "C" { ... }`
    // already reaches its brace correctly through the `"unsafe"` branch at
    // the top of this function (control_flow_block_end's opaque scan skips
    // over `extern "ABI"` on the way there); this is specifically for the
    // modifier combinations that don't start with one of that branch's
    // keywords, so `extern "C" { ... }` alone still needs it (Codex review
    // on #2739, round 38, P2).
    if matches!(trees.get(item_start), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace)
    {
        return Some(item_start);
    }
    None
}

/// The end of a `struct`/`enum`/`union`/`trait` item's declaration starting
/// right after its own keyword at `i`. A struct/enum/union with a braced
/// body (`struct Foo { field: T }`), or a trait, ends at that brace with NO
/// trailing `;`; a tuple struct (`struct Foo(T);`) or unit struct (`struct
/// Foo;`) ends at a `;` instead — both shapes are handled here since the
/// caller cannot know which one it's looking at without scanning.
///
/// Tracks angle-bracket depth the same way [`simple_impl_body_index`] does
/// (a braced const-generic default in the item's own generics, or its
/// implementing type, isn't mistaken for its body) and skips a `->` arrow
/// as one atomic unit for the identical reason
/// [`simple_fn_body_index`]'s return-type loop does (a `where T: Fn() ->
/// U` clause on the item itself is scanned directly here; an associated
/// function's own arrow, inside the item's body, is never reached — the
/// body is nested inside the Brace group this loop simply returns at).
fn braced_or_semicolon_item_end(trees: &[TokenTree], mut i: usize) -> Option<usize> {
    let mut angle_depth: i32 = 0;
    loop {
        match trees.get(i) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace && angle_depth == 0 => {
                return Some(i);
            }
            Some(TokenTree::Punct(p)) if p.as_char() == ';' && angle_depth == 0 => {
                return Some(i);
            }
            Some(TokenTree::Punct(p))
                if p.as_char() == '-'
                    && matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>') =>
            {
                i += 2;
            }
            Some(TokenTree::Punct(p)) if p.as_char() == '<' => {
                angle_depth += 1;
                i += 1;
            }
            Some(TokenTree::Punct(p)) if p.as_char() == '>' => {
                angle_depth -= 1;
                if angle_depth < 0 {
                    return None;
                }
                i += 1;
            }
            Some(
                TokenTree::Ident(_)
                | TokenTree::Punct(_)
                | TokenTree::Literal(_)
                | TokenTree::Group(_),
            ) => {
                i += 1;
            }
            _ => return None,
        }
    }
}

/// For `[<generics>] <Type>` or `[<generics>] <Trait> for <Type> [where
/// ...]` starting right after the `impl` keyword at `trees[impl_index]`,
/// return the index of the body's brace group.
///
/// Scans forward token by token rather than parsing the shape explicitly —
/// real `impl` syntax never puts a brace group anywhere before its own body
/// (paths, bounds, and `where` clauses don't use one) — with one exception:
/// a braced const-generic default nested inside the generic parameter list
/// or the implementing type (`impl<const N: usize> Foo<{ N + 1 }>`), the
/// same ambiguity `simple_fn_body_index`'s return-type loop guards against.
/// Angle-bracket depth is tracked for exactly that reason: only a Brace
/// group seen at depth 0 is treated as the body.
fn simple_impl_body_index(trees: &[TokenTree], impl_index: usize) -> Option<usize> {
    let mut i = impl_index + 1;
    let mut angle_depth: i32 = 0;
    loop {
        match trees.get(i) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace && angle_depth == 0 => {
                return Some(i);
            }
            // `->` (e.g. a `Fn() -> U` bound in a `where` clause or generic
            // param) is two Punct tokens, `-` then `>` — see the identical
            // fix and rationale in `simple_fn_body_index` (Codex review on
            // #2739, round 25, P2).
            Some(TokenTree::Punct(p))
                if p.as_char() == '-'
                    && matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>') =>
            {
                i += 2;
            }
            Some(TokenTree::Punct(p)) if p.as_char() == '<' => {
                angle_depth += 1;
                i += 1;
            }
            Some(TokenTree::Punct(p)) if p.as_char() == '>' => {
                angle_depth -= 1;
                if angle_depth < 0 {
                    return None;
                }
                i += 1;
            }
            Some(
                TokenTree::Ident(_)
                | TokenTree::Punct(_)
                | TokenTree::Literal(_)
                | TokenTree::Group(_),
            ) => {
                i += 1;
            }
            _ => return None,
        }
    }
}

/// For `fn <name>(<params>) [-> <plain return type>] { <body> }` starting at
/// `trees[fn_index]` (the `fn` keyword itself), return the index of the
/// body's brace group.
///
/// Deliberately narrow: the name must be a single identifier (no generics),
/// the parameter list must be one parenthesized group, and an optional
/// return type must be built only from idents/`::`/`<`/`>`/`&`/lifetimes/
/// literals with no delimited group of its own (so `Vec<EdgeRoute>` and
/// similar plain paths resolve, but a `where` clause, a const-generic
/// default, or any other shape does not). Returning `None` for anything
/// outside this shape is the safe choice: forward-scanning for "the next
/// brace group" without it risks finding a LATER, unrelated item's body
/// instead of this function's own (e.g. a body-less `fn foo();` followed by
/// `mod bar { ... }` — the naive scan would treat `bar`'s body as `foo`'s).
fn simple_fn_body_index(trees: &[TokenTree], fn_index: usize) -> Option<usize> {
    let mut i = fn_index + 1;
    if !matches!(trees.get(i), Some(TokenTree::Ident(_))) {
        return None;
    }
    i += 1;
    // Tolerate a simple generic-parameter list (`<T, U: Bound<V>>`) between
    // the function name and its parameter list, the same way the return-type
    // case below tolerates a simple ungrouped token run (Codex review on
    // #2739, round 21, P2).
    if matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '<') {
        let mut depth: i32 = 0;
        loop {
            match trees.get(i) {
                // `->` (e.g. a `Fn() -> U` bound inside `<T: Fn() -> U>`) is
                // two Punct tokens, `-` then `>` — the `>` half is not a
                // closing angle bracket and must not decrement `depth`, or a
                // legitimate generic bound ends this loop early and leaves
                // the real closing `>` as an unexpected token the rest of
                // this function cannot parse, returning `None` for a
                // perfectly ordinary generic function (Codex review on
                // #2739, round 25, P2).
                Some(TokenTree::Punct(p))
                    if p.as_char() == '-'
                        && matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>') =>
                {
                    i += 2;
                }
                Some(TokenTree::Punct(p)) if p.as_char() == '<' => {
                    depth += 1;
                    i += 1;
                }
                Some(TokenTree::Punct(p)) if p.as_char() == '>' => {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                Some(
                    TokenTree::Ident(_)
                    | TokenTree::Punct(_)
                    | TokenTree::Literal(_)
                    | TokenTree::Group(_),
                ) => {
                    i += 1;
                }
                _ => return None,
            }
            if depth < 0 {
                return None;
            }
        }
    }
    if !matches!(trees.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
    {
        return None;
    }
    i += 1;
    if matches!(trees.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
        return Some(i);
    }
    // A return type (`-> T`), a `where` clause (`where T: Trait`), or both in
    // sequence (`-> T where T: Trait`) are all just a flat run of simple
    // tokens up to the body's own brace group — genuine Rust grammar never
    // puts a `{ ... }` group inside either shape (bounds and return types
    // don't use brace groups), so the same tolerant loop the return-type
    // case already used for a TRAILING `where` clause (it never
    // distinguished the two, just consumed tokens until a brace) now starts
    // from either keyword. Previously a function with a `where` clause but
    // NO return type (`fn routes<T>() where T: Trait { ... }`) hit neither
    // this branch's `->` check nor the body-brace check above, falling
    // through to `None` — the unprotected old fallback then scanned the
    // body of what could be a `#[cfg(...)]`-excluded function, crediting a
    // registration that does not really exist (Codex review on #2739,
    // round 22, P2).
    let starts_return_type_or_where = matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '-')
        && matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '>');
    let starts_where_only = matches!(trees.get(i), Some(TokenTree::Ident(id)) if id == "where");
    if starts_return_type_or_where || starts_where_only {
        if starts_return_type_or_where {
            i += 2;
        }
        // A braced const-generic expression in the return type (stable Rust:
        // `fn wire() -> R<{ 1 + 2 }>` compiles today) is itself a
        // Brace-delimited Group nested inside `<...>` — indistinguishable
        // from the function's own body by delimiter alone. Track angle-
        // bracket depth (angle brackets are plain `<`/`>` Punct tokens, not
        // their own Group delimiter) so only a Brace group seen OUTSIDE any
        // `<...>` ends this loop; previously the first Brace group at any
        // depth was taken as the body, which for this shape pointed at the
        // const-generic expression instead — and since the caller then
        // resumes scanning right after that wrong index, the function's
        // real body (and its `#[cfg(...)]`-gated `edge_routes![]` call) was
        // reached via the outer, cfg-blind fallback instead of being
        // correctly skipped (Codex review on #2739, round 22, P2).
        let mut angle_depth: i32 = 0;
        loop {
            match trees.get(i) {
                Some(TokenTree::Group(g))
                    if g.delimiter() == Delimiter::Brace && angle_depth == 0 =>
                {
                    return Some(i);
                }
                // `->` (a plain function arrow, e.g. inside `impl Fn() ->
                // T`) is two Punct tokens, `-` then `>` — the `>` half must
                // not be mistaken for a closing angle bracket, or a
                // legitimate return type nesting an arrow (any
                // `Fn`/`FnMut`/`FnOnce` bound) trips the `angle_depth < 0`
                // guard below and wrongly returns `None`, sending the
                // caller's `#[cfg(...)]`-excluded function through the
                // unprotected, cfg-blind fallback instead (Codex review on
                // #2739, round 25, P2).
                Some(TokenTree::Punct(p))
                    if p.as_char() == '-'
                        && matches!(trees.get(i + 1), Some(TokenTree::Punct(p2)) if p2.as_char() == '>') =>
                {
                    i += 2;
                }
                Some(TokenTree::Punct(p)) if p.as_char() == '<' => {
                    angle_depth += 1;
                    i += 1;
                }
                Some(TokenTree::Punct(p)) if p.as_char() == '>' => {
                    angle_depth -= 1;
                    if angle_depth < 0 {
                        return None;
                    }
                    i += 1;
                }
                // A top-level `;` ends a bodyless declaration (`fn
                // disabled() -> ();`, legal in a trait) — verified directly
                // via a real build. Previously this fell into the tolerant
                // catch-all below, which accepted `;` as ordinary
                // punctuation and kept scanning past it into a LATER,
                // unrelated item's own brace (e.g. a trait's own closing
                // brace, or the next method's body) and returned that as if
                // it were `disabled`'s body — so the caller resumed scanning
                // past the real end of the excluded declaration, running
                // straight through a subsequent active method's own
                // `edge_routes![]` call along with it (Codex review on
                // #2739, round 39, P2).
                // A top-level `;` ends a bodyless declaration (`fn
                // disabled() -> ();`, legal in a trait) — verified directly
                // via a real build. Previously this fell into the tolerant
                // catch-all below, which accepted `;` as ordinary
                // punctuation and kept scanning past it into a LATER,
                // unrelated item's own brace (e.g. a trait's own closing
                // brace, or the next method's body) and returned that as if
                // it were `disabled`'s body — so the caller resumed scanning
                // past the real end of the excluded declaration, running
                // straight through a subsequent active method's own
                // `edge_routes![]` call along with it (Codex review on
                // #2739, round 39, P2).
                Some(TokenTree::Punct(p)) if p.as_char() == ';' && angle_depth == 0 => {
                    return None;
                }
                // A non-brace group, or a Brace group nested inside `<...>`,
                // is legitimate return-type/where-clause syntax — `-> ()`,
                // `-> Result<(), E>`, `-> [T; N]`, `-> R<{ 1 + 2 }>`, a
                // parenthesized trait-bound group. Previously only bare
                // `Ident`/`Punct`/`Literal` tokens were tolerated, so a
                // grouped return type fell through to `None` and the
                // unprotected old fallback then scanned the body of what
                // could be a `#[cfg(...)]`-excluded function (Codex review
                // on #2739, round 22, P2).
                Some(
                    TokenTree::Ident(_)
                    | TokenTree::Punct(_)
                    | TokenTree::Literal(_)
                    | TokenTree::Group(_),
                ) => i += 1,
                _ => return None,
            }
        }
    }
    None
}

/// Whether the tokens immediately preceding `trees[item_index]` form one or
/// more attributes (`#[...]`), at least one of which is a `#[cfg(...)]` that
/// [`eval_cfg_attr`]'s own predicate grammar resolves as definitely false —
/// the token-level counterpart [`collect_registrations`] needs since it
/// walks a raw `TokenStream`, never a `syn::Attribute` list.
fn preceding_cfg_excludes(
    trees: &[TokenTree],
    item_index: usize,
    default_features: &BTreeSet<String>,
) -> bool {
    let mut i = item_index;
    while i >= 2 {
        let Some(TokenTree::Group(bracket)) = trees.get(i - 1) else {
            break;
        };
        if bracket.delimiter() != Delimiter::Bracket {
            break;
        }
        let Some(TokenTree::Punct(hash)) = trees.get(i - 2) else {
            break;
        };
        if hash.as_char() != '#' {
            break;
        }
        if cfg_attribute_group_is_false(bracket, default_features) {
            return true;
        }
        i -= 2;
    }
    false
}

/// Whether `group` — the bracketed contents of one `#[...]` attribute — is a
/// `cfg(...)` whose predicate resolves as definitely false, using the same
/// grammar and safe-direction fallback (unparseable stays *true*, i.e. not
/// excluded) as [`eval_cfg_attr`] — or an active `#[cfg_attr(condition,
/// cfg(inner), ...)]` whose injected `cfg(...)` resolves as definitely
/// false the same way [`cfg_attr_injects_a_false_cfg`] does at the
/// `syn::Attribute` level. Every other cfg-check in [`collect_registrations`]
/// (`mod` detection, the bare-`Group` catch-all, the statement-level
/// `edge_routes![]` check, [`skip_leading_cfg_attrs`]'s statement-skip
/// suppression) goes through [`preceding_cfg_excludes`] or
/// [`skip_leading_cfg_attrs`], both built on this one function — without
/// this, a `cfg_attr`-injected exclusion on an inline `mod` was invisible to
/// the TOKEN-level registration walk even after [`attr_cfg_excludes`] fixed
/// every AST-level one, so `#[cfg_attr(feature = "outer", cfg(feature =
/// "inner"))] mod premium { edge_routes![show]; }` with only `outer` on
/// still credited `show` as registered (Codex review on #2739, round 33,
/// P2).
fn cfg_attribute_group_is_false(
    group: &proc_macro2::Group,
    default_features: &BTreeSet<String>,
) -> bool {
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let (Some(TokenTree::Ident(ident)), Some(TokenTree::Group(payload))) =
        (inner.first(), inner.get(1))
    else {
        return false;
    };
    if payload.delimiter() != Delimiter::Parenthesis {
        return false;
    }
    if ident == "cfg" {
        return syn::parse2::<CfgPredicate>(payload.stream())
            .is_ok_and(|pred| !pred.eval(default_features));
    }
    if ident == "cfg_attr" {
        return syn::parse2::<syn::Meta>(group.stream())
            .ok()
            .and_then(|meta| match meta {
                syn::Meta::List(list) if list.path.is_ident("cfg_attr") => Some(list),
                _ => None,
            })
            .is_some_and(|list| cfg_attr_injects_a_false_cfg(&list, default_features));
    }
    false
}

/// Resolve one registration entry against `module_path`, the module the
/// `edge_routes![...]` invocation is written in, returning every reading
/// worth trying — `is_registered` matches if any of them names the function.
///
/// Cargo's own path rules: a leading `self` names that module itself, so it
/// is dropped in favor of `module_path`; a leading `super` (repeatable) goes
/// up one level of `module_path` per occurrence. Either way, a resolution
/// that bottoms out at the crate root is written back out with an explicit
/// `crate` prefix (`super::show` from module `admin` becomes `crate::show`,
/// not bare `show`) so [`is_registered`]'s exact-equality match — not the
/// looser bare-entry rule — is what decides it. `self`/`super` are unambiguous,
/// so each resolves to exactly one candidate.
///
/// Anything else — `crate`, a crate-name-qualified path, a real module path,
/// a plain relative child-module reference, or no qualifier at all — is kept
/// as its own single candidate, unresolved. A plain qualified entry
/// (`v1::show`, no `self`/`super`/`crate`/crate-name prefix) is genuinely
/// ambiguous without full symbol resolution: real Rust would resolve it
/// against the invocation's local scope first, which can be a child module
/// of `module_path` — but trying that reading *in addition to* the as-written
/// one, tried once, produced a real false positive: with a genuine top-level
/// `v1::show` AND a nested `api::v1::show`, an entry `v1::show` written
/// inside `mod api` would then satisfy both, hiding a real "unregistered"
/// warning for whichever one Rust does not actually mean (Codex review on
/// #2739, round 6, P2, reverting round 5's attempt at this). Leaving it
/// unresolved keeps the scanner's documented safe direction: a relative
/// reference like this reports as an extra false-positive "unregistered"
/// warning instead.
fn registration_candidates(entry: &str, module_path: &[String]) -> Vec<String> {
    // A leading `::` (Rust's "anchor to a crate root" form, `::my_crate::show`)
    // means the same thing as the unanchored `my_crate::show` — it only rules
    // out a same-named local item shadowing the crate name. Left in place, it
    // splits off as a phantom empty leading segment that matches neither the
    // `crate` nor the crate-name branch below in `is_registered`, so every
    // absolute-path registration missed its handler even though Cargo
    // resolves it fine (Codex review on #2739, round 9, P2).
    let entry = entry.strip_prefix("::").unwrap_or(entry);
    let segments: Vec<&str> = entry.split("::").collect();
    let (mut base, rest): (Vec<String>, &[&str]) = match segments.first().copied() {
        Some("self") => (module_path.to_vec(), &segments[1..]),
        Some("super") => {
            let mut base = module_path.to_vec();
            let mut rest: &[&str] = &segments;
            while rest.first().copied() == Some("super") {
                base.pop();
                rest = &rest[1..];
            }
            (base, rest)
        }
        _ => return vec![entry.to_owned()],
    };
    if base.is_empty() {
        base.push("crate".to_owned());
    }
    base.extend(rest.iter().map(|s| (*s).to_owned()));
    vec![base.join("::")]
}

/// Split a macro argument list on top-level commas and keep each entry's full
/// path text, so `handlers::greet, note,` yields `["handlers::greet",
/// "note"]`.
fn registered_idents(stream: &TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for tree in stream.clone() {
        match tree {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            TokenTree::Punct(p) if p.as_char() == ':' => current.push(':'),
            TokenTree::Ident(ident) => current.push_str(&ident.to_string()),
            _ => {}
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // "src/lib.rs", not "src/main.rs": since round 15 gave the package's
    // implicit `src/main.rs` binary its own distinct crate identity
    // (`"bin:main"`, separate from the library's `""`), a fixture standing
    // in for "the crate root" needs to be the library specifically — most
    // callers only care about self-consistency (same file in, same file
    // out) and would work with either, but the crate-name-qualified tests
    // require a REAL library crate root to mean anything (Codex review on
    // #2739, round 15, P2).
    fn scan_one(src: &str) -> EdgeScan {
        scan_sources(&[("src/lib.rs", src)])
    }

    fn scan_one_with_features(src: &str, features: &[&str]) -> EdgeScan {
        let default_features: BTreeSet<String> = features.iter().map(|s| (*s).to_owned()).collect();
        scan_sources_with_features(&[("src/lib.rs", src)], &default_features)
    }

    #[test]
    fn bare_edge_attribute_is_found() {
        let scan = scan_one(
            r#"
            #[get("/hello")]
            #[edge]
            pub async fn hello() -> &'static str { "hi" }
            "#,
        );
        assert_eq!(scan.names(), vec!["hello"]);
        assert_eq!(scan.functions[0].file, "src/lib.rs");
        assert!(scan.functions[0].guards.is_empty());
        assert!(!scan.is_empty());
    }

    #[test]
    fn qualified_edge_paths_are_recognized() {
        let scan = scan_sources(&[
            ("src/a.rs", "#[autumn_web::edge]\nfn a() {}"),
            ("src/b.rs", "#[macros::edge]\nfn b() {}"),
            ("src/c.rs", "#[edge(needs(kv))]\nfn c() {}"),
        ]);
        assert_eq!(scan.names(), vec!["a", "b", "c"]);
        assert_eq!(scan.files_scanned, 3);
    }

    #[test]
    fn unmarked_functions_are_ignored() {
        let scan = scan_one("#[get(\"/x\")]\nfn plain() {}\n#[edgy]\nfn other() {}");
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    /// `#[cfg_attr(feature = "edge-routes", edge)]` expands to exactly
    /// `#[edge]` once `edge-routes` is on — real, valid Rust — but this
    /// scan's marker search previously only ever checked each attribute's
    /// OWN top-level path, never looking inside `cfg_attr`'s payload, so a
    /// handler whose ONLY `#[edge]` marker arrived this way was invisible
    /// to the scan entirely (Codex review on #2739, round 27, P1).
    #[test]
    fn an_edge_marker_behind_an_active_cfg_attr_is_found() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "edge-routes", edge)]
            pub fn hello() {}
            "#,
            &["edge-routes"],
        );
        assert_eq!(scan.names(), vec!["hello"]);
    }

    /// Same shape, but the condition is off — the marker genuinely does not
    /// apply, so the function must NOT be treated as an edge handler at
    /// all.
    #[test]
    fn an_edge_marker_behind_an_inactive_cfg_attr_is_not_found() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "edge-routes", edge)]
            pub fn hello() {}
            "#,
            &[],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    /// An unresolvable `cfg_attr` condition (a key this scan's `CfgPredicate`
    /// grammar does not parse) stays conservative — the same "unresolvable
    /// stays visible" direction `eval_cfg_attr` already uses for a plain
    /// `#[cfg(...)]` — since wrongly hiding a real `#[edge]` marker is the
    /// dangerous direction here, not wrongly keeping one.
    #[test]
    fn an_edge_marker_behind_an_unresolvable_cfg_attr_condition_is_still_found() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(target_os = "wasi", edge)]
            pub fn hello() {}
            "#,
            &[],
        );
        assert_eq!(scan.names(), vec!["hello"]);
    }

    /// A guard attribute conditionally applied the same way is recognized
    /// too, since both go through the same attribute-name computation.
    #[test]
    fn a_guard_attribute_behind_an_active_cfg_attr_is_recorded() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            #[cfg_attr(feature = "strict-auth", secured)]
            pub fn hello() {}
            "#,
            &["strict-auth"],
        );
        assert_eq!(scan.functions[0].guards, vec!["secured".to_owned()]);
    }

    /// `#[cfg_attr(feature = "outer", cfg(feature = "inner"))]` injects a
    /// real `#[cfg(feature = "inner")]` once `outer` is on — verified
    /// directly via a real build reporting the function "configured out"
    /// with only `outer` enabled and `inner` off. This scan's exclusion
    /// check previously only ever looked at each attribute's own top-level
    /// `cfg(...)`, never a `cfg(...)` injected through an active
    /// `cfg_attr`, so it credited a phantom edge route the real build
    /// strips out entirely (Codex review on #2739, round 29, P2).
    #[test]
    fn an_edge_fn_with_a_cfg_attr_injected_false_cfg_is_excluded() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "outer", cfg(feature = "inner"))]
            #[edge]
            pub fn hello() {}
            "#,
            &["outer"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    /// Same shape, but `inner` is ALSO on this time — the injected cfg
    /// genuinely holds, so the function is a real edge handler.
    #[test]
    fn an_edge_fn_with_a_cfg_attr_injected_true_cfg_is_included() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "outer", cfg(feature = "inner"))]
            #[edge]
            pub fn hello() {}
            "#,
            &["outer", "inner"],
        );
        assert_eq!(scan.names(), vec!["hello"]);
    }

    /// The outer `cfg_attr` condition itself being off means the injected
    /// `cfg(...)` never applies at all — the function must stay a real
    /// edge handler, not be excluded by a cfg that was never really
    /// present.
    #[test]
    fn an_edge_fn_with_an_inactive_cfg_attr_injected_cfg_is_included() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "outer", cfg(feature = "inner"))]
            #[edge]
            pub fn hello() {}
            "#,
            &[],
        );
        assert_eq!(scan.names(), vec!["hello"]);
    }

    /// `#[cfg_attr(feature = "a", cfg_attr(feature = "b", edge))]` — rustc
    /// expands `cfg_attr` recursively (verified directly via a real build:
    /// with both features on, a function marked ONLY this way and denying
    /// `dead_code` still compiles clean because the nested `allow` reached
    /// it too) — but this scan previously only ever collected the
    /// IMMEDIATE payload's meta names, so a nested `cfg_attr`'s own `edge`
    /// was never discovered, just the literal name `cfg_attr` (Codex review
    /// on #2739, round 30, P1).
    #[test]
    fn an_edge_marker_behind_a_nested_cfg_attr_is_found() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "a", cfg_attr(feature = "b", edge))]
            pub fn hello() {}
            "#,
            &["a", "b"],
        );
        assert_eq!(scan.names(), vec!["hello"]);
    }

    /// Same nesting, but only the OUTER condition holds — the inner
    /// `cfg_attr` never activates, so its `edge` marker never applies
    /// either.
    #[test]
    fn an_edge_marker_behind_a_nested_cfg_attr_with_only_the_outer_condition_on_is_not_found() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "a", cfg_attr(feature = "b", edge))]
            pub fn hello() {}
            "#,
            &["a"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    /// The same nesting fix applies to an injected `cfg(...)` exclusion,
    /// not just a marker: `#[cfg_attr(feature = "a", cfg_attr(feature =
    /// "b", cfg(feature = "c")))]` with `a`/`b` on and `c` off must still
    /// exclude the function.
    #[test]
    fn an_edge_fn_with_a_nested_cfg_attr_injected_false_cfg_is_excluded() {
        let scan = scan_one_with_features(
            r#"
            #[cfg_attr(feature = "a", cfg_attr(feature = "b", cfg(feature = "c")))]
            #[edge]
            pub fn hello() {}
            "#,
            &["a", "b"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn guard_attributes_on_the_same_fn_are_recorded() {
        let scan = scan_one(
            r#"
            #[get("/dash")]
            #[edge]
            #[secured]
            fn dash() {}

            #[get("/t")]
            #[edge]
            #[autumn_web::throttle(per_minute = 5)]
            fn throttled() {}

            #[get("/ok")]
            #[edge]
            fn ok() {}
            "#,
        );
        let guarded: Vec<&str> = scan.guarded().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(guarded, vec!["dash", "throttled"]);
        assert_eq!(scan.functions[0].guards, vec!["secured".to_owned()]);
        assert_eq!(scan.functions[1].guards, vec!["throttle".to_owned()]);
    }

    #[test]
    fn every_guard_attribute_is_detected() {
        for guard in EDGE_GUARD_ATTRS {
            let scan = scan_one(&format!("#[edge]\n#[{guard}]\nfn g() {{}}"));
            assert_eq!(
                scan.guarded().len(),
                1,
                "#[{guard}] must be detected next to #[edge]"
            );
        }
    }

    #[test]
    fn registrations_collect_handler_idents() {
        let scan = scan_one(
            r"
            #[edge]
            fn greet() {}
            #[edge]
            fn note() {}

            pub fn edge_routes() -> Vec<EdgeRoute> {
                autumn_edge::edge_routes![handlers::greet, note,]
            }
            ",
        );
        assert_eq!(scan.registrations, 1);
        // Full path text, not just the last segment, tagged with the
        // invocation's own crate (the library crate, `""`, for `src/lib.rs`).
        assert!(
            scan.registered
                .contains(&(String::new(), "handlers::greet".to_owned()))
        );
        assert!(
            scan.registered
                .contains(&(String::new(), "note".to_owned()))
        );
        // `greet` is genuinely declared at crate root here (`src/lib.rs`),
        // not inside a `handlers` module, so the qualified entry does not
        // match it — only the bare `note` entry does.
        let unregistered: Vec<&str> = scan
            .unregistered()
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(unregistered, vec!["greet"]);
        assert_eq!(
            scan.registered_fns()
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["note"]
        );
    }

    #[test]
    fn a_function_named_edge_routes_is_not_a_registration() {
        let scan = scan_one(
            r"
            #[edge]
            fn greet() {}
            pub fn edge_routes() -> Vec<EdgeRoute> { Vec::new() }
            ",
        );
        assert_eq!(scan.registrations, 0);
        assert_eq!(
            scan.unregistered()
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["greet"]
        );
    }

    #[test]
    fn unregistered_lists_only_the_missing_handlers() {
        let scan = scan_one(
            r"
            #[edge]
            fn greet() {}
            #[edge]
            fn stats() {}
            fn wire() { edge_routes![greet]; }
            ",
        );
        let missing: Vec<String> = scan.unregistered().iter().map(|f| f.location()).collect();
        assert_eq!(missing.len(), 1);
        assert!(missing[0].starts_with("stats @ src/lib.rs:"), "{missing:?}");
    }

    #[test]
    fn inline_modules_are_scanned() {
        let scan = scan_one(
            r"
            mod routes {
                #[edge]
                pub fn nested() {}
            }
            ",
        );
        assert_eq!(scan.names(), vec!["nested"]);
    }

    #[test]
    fn line_numbers_point_at_the_declaration() {
        let scan = scan_one("// header\n\n#[edge]\nfn marked() {}\n");
        assert_eq!(scan.functions[0].line, 4);
    }

    #[test]
    fn unparseable_source_is_skipped() {
        let scan = scan_one("fn broken( {");
        assert!(scan.is_empty());
        assert_eq!(scan.registrations, 0);
    }

    #[test]
    fn registrations_are_found_across_files() {
        let scan = scan_sources(&[
            ("src/handlers.rs", "#[edge]\nfn greet() {}"),
            (
                "src/lib.rs",
                "fn main() { autumn_edge::serve(edge_routes![greet]); }",
            ),
        ]);
        assert_eq!(scan.registrations, 1);
        assert!(scan.unregistered().is_empty());
    }

    #[test]
    fn resolve_reads_src_tree_relative_to_project_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/routes")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(
            dir.path().join("src/routes/home.rs"),
            "#[edge]\npub fn home() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        // Build output must never be scanned.
        std::fs::write(dir.path().join("target/gen.rs"), "#[edge]\nfn gen() {}\n").unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.names(), vec!["home"]);
        assert_eq!(scan.functions[0].file, "src/routes/home.rs");
        assert_eq!(scan.files_scanned, 2);
    }

    #[test]
    fn resolve_without_src_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let scan = resolve_edge_scan(dir.path());
        assert!(scan.is_empty());
        assert_eq!(scan.files_scanned, 0);
    }

    // --- Item 1: safe-direction #[cfg(...)] evaluation ---

    #[test]
    fn cfg_feature_gated_fn_excluded_when_feature_not_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "unused")]
            #[edge]
            fn hidden() {}
            "#,
            &[],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    /// A `#[cfg(...)]` on the *enclosing inline module*, not just the
    /// function itself, must exclude the handler too — real Rust strips the
    /// whole module (Codex review on #2739, round 11, P2).
    #[test]
    fn cfg_feature_gated_inline_module_excludes_its_handlers() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "premium")]
            mod routes {
                #[edge]
                pub fn show() {}
            }
            "#,
            &[],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn cfg_feature_gated_inline_module_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "premium")]
            mod routes {
                #[edge]
                pub fn show() {}
            }
            "#,
            &["premium"],
        );
        assert_eq!(scan.names(), vec!["show"]);
    }

    /// A registration written inside a cfg'd-out inline module must not
    /// count either — `edge_routes![crate::show]` inside
    /// `#[cfg(feature = "premium")] mod wiring { ... }` does not exist in
    /// the compiled capsule when `premium` is off, so `show` must report as
    /// unregistered, not falsely served (Codex review on #2739, round 15,
    /// P2).
    #[test]
    fn cfg_feature_gated_inline_module_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            mod wiring {
                fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().len() == 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same setup, but with `premium` on: the registration genuinely exists
    /// in the compiled capsule, so `show` must report as registered.
    #[test]
    fn cfg_feature_gated_inline_module_registration_is_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            mod wiring {
                fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &["premium"],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A `cfg(...)` INJECTED by an active `#[cfg_attr(condition,
    /// cfg(inner))]` on an inline `mod` must exclude it entirely, the same
    /// way a literal `#[cfg(...)]` already does — verified directly via a
    /// real build (`#[cfg_attr(feature = "outer", cfg(feature = "inner"))]
    /// mod premium { ... }` with only `outer` on fails to resolve
    /// `premium` at all). Both the `#[edge]` handler discovery half
    /// (`scan_items`'s inline-mod check) and the `edge_routes![]`
    /// registration half (`collect_registrations`'s token-level check) must
    /// honor it (Codex review on #2739, round 33, P2).
    #[test]
    fn cfg_attr_injected_false_cfg_on_an_inline_module_excludes_it_entirely() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg_attr(feature = "outer", cfg(feature = "inner"))]
            mod premium {
                #[edge]
                pub fn extra() {}

                fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &["outer"],
        );
        assert_eq!(
            scan.names(),
            vec!["show".to_owned()],
            "{:?}",
            scan.functions
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// Same setup, but with `inner` ALSO on: the injected cfg genuinely
    /// holds, so the module (its handler and its registration both) is
    /// real.
    #[test]
    fn cfg_attr_injected_true_cfg_on_an_inline_module_includes_it() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg_attr(feature = "outer", cfg(feature = "inner"))]
            mod premium {
                #[edge]
                pub fn extra() {}

                fn wire() {
                    edge_routes![crate::show];
                    edge_routes![crate::premium::extra];
                }
            }
            "#,
            &["outer", "inner"],
        );
        assert_eq!(scan.functions.len(), 2, "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same idea, but the cfg'd-out module has a `pub` visibility modifier
    /// between the attribute and `mod` — the `mod` special case looked
    /// immediately before `mod` itself for the attribute, same as `fn`/
    /// `impl` used to before they gained `skip_back_over_fn_modifiers`, so
    /// `pub` hid the attribute and the registration was wrongly credited
    /// (Codex review on #2739, round 27, P2).
    #[test]
    fn cfg_feature_gated_pub_inline_module_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            pub mod wiring {
                pub fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().len() == 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same setup, but with `premium` on: the registration genuinely exists
    /// in the compiled capsule, so `show` must report as registered.
    #[test]
    fn cfg_feature_gated_pub_inline_module_registration_is_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            pub mod wiring {
                pub fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &["premium"],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// Same as the cfg'd-out inline module case above, but the
    /// `edge_routes![]` call is inside a cfg'd-out `impl` block instead of a
    /// `mod { ... }` — the catch-all `Group` branch only recognizes a cfg
    /// attribute with NOTHING between it and the group, but an `impl`
    /// block's brace is always preceded by at least the implementing type,
    /// so without a dedicated `impl` special case (mirroring `mod` and
    /// `fn`) this recursed unconditionally into a block real Rust strips
    /// entirely with the feature off (Codex review on #2739, round 23, P2).
    #[test]
    fn cfg_feature_gated_impl_block_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            struct Routes;

            #[cfg(feature = "premium")]
            impl Routes {
                fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().len() == 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same setup, but with `premium` on: the registration genuinely exists
    /// in the compiled capsule, so `show` must report as registered.
    #[test]
    fn cfg_feature_gated_impl_block_registration_is_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            struct Routes;

            #[cfg(feature = "premium")]
            impl Routes {
                fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &["premium"],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// The same arrow-inside-angle-brackets ambiguity `simple_fn_body_index`
    /// guards against, but inside `simple_impl_body_index`'s own generic
    /// parameter list this time (`impl<T: Fn() -> u8> Routes<T>`) — the
    /// identical fix applies there too (Codex review on #2739, round 25,
    /// P2).
    #[test]
    fn cfg_feature_gated_impl_block_with_an_arrow_bound_registration_is_excluded_when_feature_is_off()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            struct Routes<T>(T);

            #[cfg(feature = "premium")]
            impl<T: Fn() -> u8> Routes<T> {
                fn wire() { edge_routes![crate::show]; }
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same as the cfg'd-out inline module case above, but the
    /// `edge_routes![]` call is inside a cfg'd-out FUNCTION body instead of
    /// a `mod { ... }` — the generic group recursion, not just the
    /// dedicated `mod` pattern, must also honor a preceding `#[cfg(...)]`
    /// (Codex review on #2739, round 16, P2).
    #[test]
    fn cfg_feature_gated_fn_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire() { edge_routes![crate::show]; }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// `#[cfg(...)]` directly on the `edge_routes![...]` STATEMENT itself
    /// (legal Rust: statement attributes apply to a macro-invocation
    /// statement same as any other), inside an otherwise-ENABLED wiring
    /// function — a different shape from a whole cfg'd-out `fn`/`mod`,
    /// which this scan already handled. Without checking this position too,
    /// a disabled route's registration was still credited (Codex review on
    /// #2739, round 22, P2).
    #[test]
    fn cfg_feature_gated_registration_statement_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same idea, but the `edge_routes![...]` call is nested a level deeper
    /// still — inside a method-call argument list — rather than being the
    /// whole statement itself. The catch-all `Group` recursion only ever
    /// checked whether the group it was about to recurse into was ITSELF
    /// immediately preceded by the attribute; here the attribute sits before
    /// `routes`, not before `extend(...)`'s parenthesized group, so that
    /// check never saw it and recursed unconditionally (Codex review on
    /// #2739, round 24, P2).
    #[test]
    fn cfg_feature_gated_statement_with_a_nested_registration_call_is_excluded_when_feature_is_off()
    {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire(routes: &mut Vec<()>) {
                #[cfg(feature = "premium")]
                routes.extend(edge_routes![crate::show]);
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same setup, but with `premium` on: the registration genuinely exists
    /// in the compiled capsule, so `show` must report as registered.
    #[test]
    fn cfg_feature_gated_statement_with_a_nested_registration_call_is_included_when_feature_is_default()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire(routes: &mut Vec<()>) {
                #[cfg(feature = "premium")]
                routes.extend(edge_routes![crate::show]);
            }
            "#,
            &["premium"],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A cfg'd-out `if` block used as a whole statement never ends in `;` —
    /// so `skip_cfg_excluded_statement`'s fallback of scanning to the next
    /// top-level `;` must not run through it into the FOLLOWING, still-real
    /// statement. Before the fix, `#[cfg(feature = "premium")] if true {
    /// edge_routes![premium]; }` followed by an active
    /// `edge_routes![show];` had the excluded `if`'s scan swallow the real
    /// statement too, stopping only at ITS semicolon — dropping `show`'s
    /// genuine registration (Codex review on #2739, round 25, P2).
    #[test]
    fn cfg_feature_gated_if_statement_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[edge]
            pub fn premium() {}

            fn wire() {
                #[cfg(feature = "premium")]
                if true {
                    edge_routes![crate::premium];
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "premium");
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// Same setup, but with `premium` on: both registrations genuinely
    /// exist in the compiled capsule.
    #[test]
    fn cfg_feature_gated_if_statement_registration_is_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[edge]
            pub fn premium() {}

            fn wire() {
                #[cfg(feature = "premium")]
                if true {
                    edge_routes![crate::premium];
                }
                edge_routes![crate::show];
            }
            "#,
            &["premium"],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            2,
            "{:?}",
            scan.registered_fns()
        );
    }

    /// Same idea, but the excluded statement is a bare, anonymous `const {
    /// ... }` block (stable since Rust 1.79) instead of an `if` — another
    /// semicolon-free statement shape `skip_cfg_excluded_statement`'s
    /// scan-to-`;` fallback cannot see the end of, so it would run right
    /// through it into the next, still-real `edge_routes![show];` (Codex
    /// review on #2739, round 28, P2).
    #[test]
    fn cfg_false_const_block_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[edge]
            pub fn premium() {}

            fn wire() {
                #[cfg(feature = "premium")]
                const {
                    edge_routes![crate::premium];
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "premium");
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// Same idea again, but the excluded statement is a labeled loop
    /// (`'label: loop { ... }`) — labeled blocks/loops share the exact same
    /// "no `;` needed" property as their unlabeled form (Codex review on
    /// #2739, round 28, P2).
    #[test]
    fn cfg_false_labeled_loop_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[edge]
            pub fn premium() {}

            fn wire() {
                #[cfg(feature = "premium")]
                'outer: loop {
                    edge_routes![crate::premium];
                    break 'outer;
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "premium");
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// An ordinary `const NAME: TYPE = { ... };` ITEM (not the bare
    /// anonymous block form) must still be recognized as ending in `;`, not
    /// mistaken for the semicolon-free block form — its own name always
    /// intervenes between `const` and any brace, so
    /// `statement_without_semicolon_end`'s Brace-directly-after-`const`
    /// check correctly declines it and the generic scan-to-`;` fallback
    /// still applies, correctly skipping the WHOLE statement (including
    /// the excluded `premium` registration nested inside its own braced
    /// initializer) without ever recursing into it.
    #[test]
    fn cfg_false_const_item_with_a_braced_initializer_does_not_swallow_the_following_registration()
    {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[edge]
            pub fn premium() {}

            fn wire() {
                #[cfg(feature = "premium")]
                const VALUE: () = { edge_routes![crate::premium]; };
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "premium");
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// A cfg'd-out `struct` with a BRACED body has no trailing `;` — so the
    /// generic scan-to-`;` fallback, having no way to know this item ends
    /// at its own closing brace rather than a semicolon, would run right
    /// through it into the next, unrelated, still-real
    /// `edge_routes![show];` and consume that too (Codex review on #2739,
    /// round 30, P2).
    #[test]
    fn cfg_false_struct_item_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                struct Premium {
                    field: i32,
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// Same idea for `pub enum` (a visibility modifier before the item
    /// keyword, exercising `skip_forward_over_item_modifiers`) and `trait`,
    /// both also braced-body items with no trailing `;`.
    #[test]
    fn cfg_false_pub_enum_and_trait_items_do_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                pub enum Premium {
                    A,
                    B,
                }
                #[cfg(feature = "premium")]
                trait PremiumTrait {
                    fn method(&self);
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// A tuple struct (`struct Foo(T);`) or unit struct (`struct Foo;`)
    /// DOES need a trailing `;`, unlike the braced-body form above —
    /// `braced_or_semicolon_item_end` must still find it correctly rather
    /// than mistaking the tuple's own parenthesized group for a Brace body
    /// it isn't.
    #[test]
    fn cfg_false_tuple_and_unit_struct_items_do_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                struct Premium(i32);
                #[cfg(feature = "premium")]
                struct PremiumUnit;
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// A cfg'd-out `macro_rules! name { ... }` definition (the brace form)
    /// has no trailing `;` — verified directly via a real build — so the
    /// generic scan-to-`;` fallback would run right through it into the
    /// next, unrelated, still-real `edge_routes![show]` invocation and
    /// consume that too (Codex review on #2739, round 32, P2).
    #[test]
    fn cfg_false_macro_rules_brace_form_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                macro_rules! unused {
                    () => {};
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// The parenthesized `macro_rules! name(...);` form DOES need a
    /// trailing `;`, unlike the brace form above — verified directly via a
    /// real build — so `braced_or_semicolon_item_end` must still find it
    /// correctly.
    #[test]
    fn cfg_false_macro_rules_paren_form_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                macro_rules! unused(
                    () => {};
                );
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// An ordinary (non-`macro_rules!`) macro invocation using brace
    /// delimiters (`configure! { ... }`) also has no trailing `;` — verified
    /// directly via a real build — so a cfg'd-out one would let the generic
    /// scan-to-`;` fallback run through it into the next, unrelated, still-
    /// real `edge_routes![show]` invocation and consume that too (Codex
    /// review on #2739, round 36, P2).
    #[test]
    fn cfg_false_brace_delimited_macro_invocation_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            macro_rules! configure {
                ($($t:tt)*) => {};
            }

            fn wire() {
                #[cfg(feature = "premium")]
                configure! { a b c }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// A grouped return type (`-> ()`, `-> Result<(), E>`, `-> [T; N]`) has
    /// a non-brace `Group` token (a parenthesized or bracketed group) inside
    /// it — previously unrecognized by the return-type/`where`-clause
    /// tolerant loop, which only accepted bare `Ident`/`Punct`/`Literal`
    /// tokens, so it fell through to `None` and the unprotected old
    /// fallback scanned the body of what could be (and, here, is) a
    /// `#[cfg(...)]`-excluded function (Codex review on #2739, round 22,
    /// P2).
    #[test]
    fn cfg_feature_gated_fn_with_a_grouped_return_type_registration_is_excluded_when_feature_is_off()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire() -> Result<(), ()> { edge_routes![crate::show]; Ok(()) }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// A braced const-generic expression in the return type (`-> R<{ 1 + 2
    /// }>`) is legal, stable Rust and is itself a Brace-delimited `Group`
    /// nested inside `<...>` — indistinguishable from the function's own
    /// body by delimiter alone. Previously the return-type/`where`-clause
    /// loop took the FIRST Brace group it saw as the body regardless of
    /// angle-bracket nesting, misidentifying the const-generic expression's
    /// braces as the body and leaving the function's REAL body (and its
    /// `#[cfg(...)]`-gated `edge_routes![]` call) to be reached only via the
    /// outer, cfg-blind fallback scan (Codex review on #2739, round 22, P2).
    #[test]
    fn cfg_feature_gated_fn_with_a_braced_const_generic_return_type_registration_is_excluded_when_feature_is_off()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            struct R<const N: usize>;

            #[cfg(feature = "premium")]
            fn wire<const N: usize>() -> R<{ 1 + 2 }> { edge_routes![crate::show]; R }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// A return type nesting its OWN arrow (`-> impl Fn() -> ()`) is legal,
    /// ordinary Rust — no generics or const-generics involved at all. The
    /// angle-bracket-depth tracking added to guard against braced
    /// const-generics treated the inner arrow's `>` half as a closing angle
    /// bracket, decrementing `angle_depth` below zero and wrongly returning
    /// `None` for a function that has no angle brackets whatsoever, sending
    /// it through the unprotected, cfg-blind fallback scan instead of being
    /// correctly recognized and excluded (Codex review on #2739, round 25,
    /// P2).
    #[test]
    fn cfg_feature_gated_fn_with_a_nested_arrow_return_type_registration_is_excluded_when_feature_is_off()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire() -> impl Fn() -> () {
                edge_routes![crate::show];
                || ()
            }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// `pub`/`async`/`const`/`unsafe` (and `pub(crate)`, `extern "C"`)
    /// between the `#[cfg(...)]` and `fn` must not hide the attribute —
    /// `preceding_cfg_excludes` has to look past every modifier, not just
    /// immediately before `fn` itself (Codex review on #2739, round 18, P2).
    #[test]
    fn cfg_feature_gated_pub_async_fn_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            pub async fn wire() { edge_routes![crate::show]; }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    #[test]
    fn cfg_feature_gated_fn_registration_is_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire() { edge_routes![crate::show]; }
            "#,
            &["premium"],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A body-less function signature (a trait method, here) must not be
    /// mistaken for a function with a body belonging to some LATER,
    /// unrelated item — `simple_fn_body_index` bails out rather than
    /// forward-scanning for "the next brace group" (which would wrongly
    /// grab `wiring`'s body), so the real registration inside `wiring`
    /// still resolves relative to its own module, not the trait's.
    #[test]
    fn a_body_less_fn_signature_does_not_swallow_a_later_items_body() {
        let scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            trait Greeter {
                fn bar();
            }

            mod wiring {
                fn wire() { edge_routes![crate::show]; }
            }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A return type built only from plain path segments (no nested groups)
    /// is also resolved, matching the shape a real wiring function tends to
    /// have.
    #[test]
    fn cfg_feature_gated_fn_with_a_return_type_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire() -> Vec<String> { edge_routes![crate::show]; Vec::new() }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// A generic wiring function (`fn wire<T>() { ... }`) must not defeat
    /// `simple_fn_body_index`'s narrow token-shape match — falling through to
    /// the old, unprotected fallback would wrongly credit a registration
    /// whose enclosing function is cfg'd out (Codex review on #2739, round
    /// 21, P2).
    #[test]
    fn cfg_feature_gated_generic_fn_registration_is_excluded_when_feature_is_off() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire<T: Default>() { edge_routes![crate::show]; }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// A generic wiring function with a `where` clause and NO return type
    /// (`fn wire<T>() where T: Default { ... }`) hits neither the
    /// body-brace check nor the `->` branch right after its parameter list,
    /// since the next token is `where`, not `{` or `-`. Previously this fell
    /// through to `None`, letting the old, unprotected fallback scan the
    /// body of a function that could be (and, here, is) `#[cfg(...)]`-
    /// excluded, crediting a registration that does not really exist (Codex
    /// review on #2739, round 22, P2 — the round-21 generic-parameter fix
    /// covered `<T>` itself but not a trailing `where` clause with no
    /// return type).
    #[test]
    fn cfg_feature_gated_generic_fn_with_a_where_clause_registration_is_excluded_when_feature_is_off()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire<T>() where T: Default { edge_routes![crate::show]; }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// A generic BOUND nesting an arrow (`fn wire<T: Fn() -> U>()`) hits the
    /// same ambiguity as a return type nesting one, but inside the
    /// generic-parameter-list tolerance loop instead: that loop's own
    /// angle-bracket depth tracking mistook the bound's `-> U`'s `>` half
    /// for the generic list's real closing bracket, ending the loop one
    /// token early and leaving the true `>` (and the parameter list after
    /// it) as unexpected tokens this function could not parse, wrongly
    /// returning `None` for an ordinary generic function (Codex review on
    /// #2739, round 25, P2).
    #[test]
    fn cfg_feature_gated_generic_fn_with_an_arrow_bound_registration_is_excluded_when_feature_is_off()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            #[cfg(feature = "premium")]
            fn wire<T: Fn() -> u8>() { edge_routes![crate::show]; }
            "#,
            &[],
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    #[test]
    fn cfg_feature_gated_fn_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "default-on")]
            #[edge]
            fn shown() {}
            "#,
            &["default-on"],
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn cfg_not_feature_included_when_feature_absent() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(not(feature = "unused"))]
            #[edge]
            fn shown() {}
            "#,
            &[],
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    /// `true`/`false` tokenize as a plain `Ident`, not a `syn::Lit`, and are
    /// Cargo's own stable constant cfg predicates: `#[cfg(false)]` always
    /// excludes, `#[cfg(true)]` never does. Previously unrecognized, falling
    /// through to this scan's own safe-direction default ("unresolvable
    /// stays included"), which for a `#[cfg(false)]` handler wrongly kept a
    /// route the real build compiles out entirely (Codex review on #2739,
    /// round 22, P2).
    #[test]
    fn cfg_false_excludes_and_cfg_true_includes() {
        let scan = scan_one(
            r"
            #[cfg(false)]
            #[edge]
            fn hidden() {}

            #[cfg(true)]
            #[edge]
            fn shown() {}
            ",
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    /// The same `cfg(false)`/`cfg(true)` recognition on the WIRING side: a
    /// `#[cfg(false)]`-disabled function's `edge_routes![...]` must not
    /// credit a registration that the real build never compiles in (Codex
    /// review on #2739, round 22, P2).
    #[test]
    fn cfg_false_excludes_a_registration() {
        let scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            #[cfg(false)]
            fn wire() { edge_routes![show]; }
            ",
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
    }

    #[test]
    fn cfg_any_included_when_one_default_feature_present() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(any(feature = "a", feature = "unused"))]
            #[edge]
            fn shown() {}
            "#,
            &["a"],
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn cfg_all_excludes_when_one_branch_is_not_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(all(feature = "a", feature = "unused"))]
            #[edge]
            fn hidden() {}
            "#,
            &["a"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn cfg_non_feature_predicate_stays_conservative() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(target_os = "linux")]
            #[edge]
            fn maybe() {}
            "#,
            &[],
        );
        assert_eq!(
            scan.names(),
            vec!["maybe"],
            "a non-feature cfg predicate must stay conservative (included)"
        );
    }

    #[test]
    fn cfg_multiple_attributes_are_anded() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "a")]
            #[cfg(feature = "unused")]
            #[edge]
            fn hidden() {}
            "#,
            &["a"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn compute_default_features_expands_transitive_defaults() {
        let manifest = r#"
            [features]
            default = ["a"]
            a = ["b"]
            b = []
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("a"));
        assert!(enabled.contains("b"));
    }

    #[test]
    fn enabled_features_from_manifest_includes_a_requested_feature() {
        let manifest = r"
            [features]
            default = []
            other = []
        ";
        let enabled = enabled_features_from_manifest(manifest, &["premium"]);
        assert!(enabled.contains("premium"));
        assert!(!enabled.contains("other"));
    }

    /// `autumn build -p blog --features blog/extra-routes` forwards
    /// `blog/extra-routes` here, Cargo's package-qualified `--features`
    /// syntax. It must resolve to `extra-routes`, not be dropped as a
    /// dependency-feature reference — dropping it would make the scan miss a
    /// route the real build turns on.
    #[test]
    fn a_package_qualified_requested_feature_resolves_to_its_bare_name() {
        let manifest = r#"
            [package]
            name = "blog"

            [features]
            default = []
        "#;
        let enabled = enabled_features_from_manifest(manifest, &["blog/extra-routes"]);
        assert!(enabled.contains("extra-routes"));
        assert!(!enabled.iter().any(|f| f.contains('/')));
    }

    /// `dep/extra` on the command line turns on `extra` in the dependency
    /// `dep`, not in this crate — even when this crate happens to declare its
    /// own feature literally named `extra`. Stripping the qualifier here
    /// would wrongly enable this crate's `#[cfg(feature = "extra")]` code
    /// that Cargo actually left off.
    #[test]
    fn a_dependency_qualified_requested_feature_is_not_mistaken_for_a_local_one() {
        let manifest = r#"
            [package]
            name = "blog"

            [features]
            default = []
            extra = []
        "#;
        let enabled = enabled_features_from_manifest(manifest, &["dep/extra"]);
        assert!(!enabled.contains("extra"));
    }

    /// `default` is itself a real feature name when the manifest declares
    /// `[features] default = [...]` — Cargo compiles a
    /// `#[cfg(feature = "default")]` route in every ordinary build of such a
    /// manifest, `autumn build` has no `--no-default-features` flag to turn
    /// it off — so it must be in the enabled set even though it never
    /// appears inside its own `default = [...]` list (Codex review on #2739,
    /// round 9, P1).
    #[test]
    fn the_default_feature_itself_is_enabled_when_declared() {
        let manifest = r#"
            [features]
            default = ["a"]
            a = []
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("default"));
        assert!(enabled.contains("a"));
    }

    /// Verified directly against `cargo rustc -- --print cfg`: with no
    /// `[features]` table at all, Cargo never emits `feature = "default"` —
    /// there is no implicit always-on `default` to seed. Round 9 assumed
    /// otherwise and unconditionally seeded it, which made a
    /// `#[cfg(feature = "default")]` route look included in a scan even
    /// though Cargo genuinely compiles it out of such a manifest — a phantom
    /// route, the same false-inclusion danger the round-11 module-cfg fix
    /// closed for inline modules (Codex review on #2739, round 12, P2).
    #[test]
    fn the_default_feature_is_not_enabled_with_no_features_table() {
        let manifest = r#"
            [package]
            name = "blog"
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("default"));
    }

    /// Same as above for a `[features]` table that exists but never mentions
    /// `default` at all — the missing key, not merely a missing table, is
    /// what must gate this.
    #[test]
    fn the_default_feature_is_not_enabled_when_the_features_table_omits_it() {
        let manifest = r"
            [features]
            other = []
        ";
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("default"));
    }

    /// An explicit but empty `default = []` still counts as declared: Cargo
    /// enables the (empty) `default` feature for such a manifest.
    #[test]
    fn the_default_feature_is_enabled_when_declared_empty() {
        let manifest = r"
            [features]
            default = []
            other = []
        ";
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("default"));
        assert!(!enabled.contains("other"));
    }

    /// `default = ["dep/extra"]` (no `?`) also activates the optional
    /// dependency `dep` itself — Cargo auto-generates a same-named local
    /// feature for every optional dependency — so a `#[cfg(feature =
    /// "dep")]` route this manifest gates that way is really compiled and
    /// must not be scanned out.
    #[test]
    fn a_strong_dependency_feature_reference_also_enables_the_dependency_itself() {
        let manifest = r#"
            [dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
        assert!(!enabled.contains("extra"));
        assert!(!enabled.iter().any(|f| f.contains('/')));
    }

    /// Cargo only auto-generates a same-named local feature for an
    /// *optional* dependency — a normal dependency shares no such link, so
    /// `foo/extra` must not turn on an unrelated local feature that happens
    /// to also be named `foo` (Codex review on #2739, round 7, P2).
    #[test]
    fn a_strong_reference_to_a_normal_dependency_does_not_enable_a_same_named_feature() {
        let manifest = r#"
            [dependencies]
            foo = { version = "1" }

            [features]
            default = ["foo/extra"]
            foo = []
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("foo"));
    }

    /// Verified directly against `cargo rustc -- --print cfg`: once any
    /// feature entry anywhere in the manifest spells `dep:foo`, Cargo
    /// suppresses `foo`'s implicit local feature crate-wide — even for an
    /// unrelated `default = ["foo/bar"]` entry that would otherwise turn it
    /// on. Missing this made `foo` look enabled here even though such a
    /// manifest's real build never sets `cfg(feature = "foo")` (Codex review
    /// on #2739, round 13, P2).
    #[test]
    fn a_dep_colon_reference_suppresses_the_implicit_feature_even_via_an_unrelated_entry() {
        let manifest = r#"
            [dependencies]
            foo = { version = "1", optional = true }

            [features]
            default = ["foo/bar"]
            explicit = ["dep:foo"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("foo"));
    }

    /// The `dep:foo`-anywhere suppression above is specific to the
    /// AUTO-GENERATED implicit feature — it has no bearing on an EXPLICIT
    /// `[features] foo = [...]` entry, which merely happens to share the
    /// optional dependency's name. Verified directly against `cargo rustc
    /// -- --print cfg`: `feature="foo"` is still active for exactly this
    /// manifest shape (`default = ["foo/bar"]` + explicit `foo = []` +
    /// unrelated `explicit = ["dep:foo"]`), unlike the previous test which
    /// has no explicit `foo` entry at all. Missing this made the scan
    /// treat a route gated on `foo` as compiled out even though a real
    /// release build still sets `cfg(feature = "foo")` (Codex review on
    /// #2739, round 34, P1).
    #[test]
    fn an_explicit_feature_sharing_an_optional_dependencys_name_is_still_enabled() {
        let manifest = r#"
            [dependencies]
            foo = { version = "1", optional = true }

            [features]
            default = ["foo/bar"]
            foo = []
            explicit = ["dep:foo"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("foo"), "{enabled:?}");
    }

    /// Without any `dep:foo` in the picture, the strong form still enables
    /// the optional dependency's implicit feature as usual — the suppression
    /// is specific to `dep:foo` appearing somewhere, not a general change in
    /// behavior.
    #[test]
    fn a_dep_colon_reference_to_an_unrelated_package_does_not_suppress_anything() {
        let manifest = r#"
            [dependencies]
            foo = { version = "1", optional = true }
            other = { version = "1", optional = true }

            [features]
            default = ["foo/bar"]
            explicit = ["dep:other"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("foo"));
    }

    /// A `[target.'cfg(...)'.dependencies]` optional dependency's implicit
    /// local feature turns on from a `dep/feat` reference the same as a
    /// top-level one, when that target's predicate applies to the edge
    /// capsule's own `wasm32-wasip1` build target. Missing this made a
    /// target-only optional dependency look non-optional, so its implicit
    /// feature never got queued and a `#[cfg(feature = "dep")]` route was
    /// scanned out even though Cargo genuinely compiles it for the capsule
    /// (Codex review on #2739, round 8, P1).
    #[test]
    fn a_target_specific_optional_dependency_feature_reference_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(target_arch = "wasm32")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// The literal-triple form of a target table (no `cfg(...)` wrapper) is
    /// recognized the same way when it names the capsule's own target
    /// exactly.
    #[test]
    fn a_literal_wasm32_wasip1_target_table_also_enables_the_dependency() {
        let manifest = r#"
            [target.wasm32-wasip1.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// Verified directly against `cargo rustc -- --print cfg`: a
    /// `[target.'cfg(...)'.dependencies]` table whose predicate can never
    /// hold for the edge capsule's `wasm32-wasip1` build (here,
    /// `cfg(windows)`) must NOT credit its optional dependency — Cargo never
    /// sets that dependency's implicit feature for a build that isn't
    /// Windows, so crediting it here would include a
    /// `#[cfg(feature = "dep")]` route in the scan that the capsule build
    /// never really compiles: a phantom route (Codex review on #2739, round
    /// 13, P2, correcting round 8's unconditional target-table scan).
    #[test]
    fn a_target_specific_dependency_whose_target_cannot_be_the_edge_capsule_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(windows)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// A `not(target_arch = "wasm32")` predicate is the direct opposite of
    /// the capsule's own target and must not be credited either.
    #[test]
    fn a_not_wasm32_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(not(target_arch = "wasm32"))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// `windows` and `unix` are rustc's own bare-flag aliases for
    /// `target_family = "windows"`/`"unix"` — always false for
    /// wasm32-wasip1, whose only `target_family` is "wasm" — so
    /// `not(windows)` is always TRUE for it: a common real pattern for a
    /// dependency meant for every non-Windows target, capsule included
    /// (Codex review on #2739, round 18, P1).
    #[test]
    fn a_not_windows_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(not(windows))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// The un-negated form must evaluate false, confirming `windows` really
    /// resolves as an explicit `Leaf(false)`, not merely "unresolvable" by
    /// coincidence of the same top-level answer.
    #[test]
    fn a_windows_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(windows)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// `[target.'cfg(true)'.dependencies]` applies unconditionally, to every
    /// target including wasm32-wasip1 — verified directly against a real
    /// `cargo rustc --target wasm32-wasip1` build. `true`/`false` tokenize
    /// as a plain `Ident`, so the leading-identifier parse succeeds, but
    /// neither the known-key nor the `not`/`all`/`any`/`windows`/`unix`
    /// branches recognized it, falling through to the parse error this
    /// evaluator treats as "does not match" (Codex review on #2739, round
    /// 22, P1).
    #[test]
    fn a_literal_true_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(true)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// The `false` counterpart never applies to any target.
    #[test]
    fn a_literal_false_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(false)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// The same manifest as above, but under Cargo's feature resolver v1:
    /// verified directly (`cargo rustc --target wasm32-wasip1 -- --print
    /// cfg`, resolver v1) that `cfg(windows)`'s own optional dependency IS
    /// unified in regardless of the actual build target — resolver v1
    /// unifies target-specific dependency features "no matter where" they
    /// are declared, a documented Cargo behavior, not a scanner heuristic
    /// (Codex review on #2739, round 22, P1).
    #[test]
    fn a_windows_target_specific_dependency_is_enabled_under_resolver_v1() {
        let manifest = r#"
            [target.'cfg(windows)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest_for_resolver(manifest, &[], true);
        assert!(enabled.contains("dep"));
    }

    /// `cfg(debug_assertions)` DOES enable a target-specific optional
    /// dependency, even for the capsule's always-`--release` build —
    /// verified directly (`cargo build --release --target wasm32-wasip1 -v`
    /// on exactly this manifest shape): Cargo resolves a target-table
    /// predicate's `debug_assertions` from its own fixed per-target cfg
    /// database, always true, not from the profile's actual
    /// `-C debug-assertions=off` flag — that flag only governs how the
    /// crate's OWN source-level `#[cfg(debug_assertions)]` compiles, a
    /// later and separate step. Round 20 treated this as unresolvable
    /// (hence false) reasoning from the wrong pipeline; round 25 corrects
    /// it (Codex review on #2739, round 25, P1).
    #[test]
    fn a_debug_assertions_target_specific_dependency_is_enabled() {
        let manifest = r#"
            [target.'cfg(debug_assertions)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"), "{enabled:?}");
    }

    /// Same reasoning for `unix`.
    #[test]
    fn a_not_unix_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(not(unix))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// A custom/unrecognized bare cfg flag this scan has no value table for
    /// (not `windows`/`unix`/`debug_assertions`, not a known `key =
    /// "value"` key in bare form) must still compose correctly under
    /// `not(...)` — verified directly (`cargo tree --target wasm32-wasip1`
    /// on this exact manifest includes the dependency; the bare,
    /// un-negated form of the same predicate does not): Cargo treats an
    /// unknown bare flag as simply absent for the target, not as
    /// "unparseable." Previously this fell through to a parse `Err`, which
    /// fails the WHOLE enclosing `not(...)` (not just the leaf), evaluating
    /// as unresolvable-so-false instead of the real true (Codex review on
    /// #2739, round 26, P1).
    #[test]
    fn a_not_unknown_bare_flag_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(not(my_custom_flag))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"), "{enabled:?}");
    }

    /// The un-negated mirror: a bare unknown flag alone is false, so the
    /// dependency stays disabled — confirming this is genuine "absent, not
    /// unparseable" semantics, not "any unknown bare flag matches."
    #[test]
    fn an_unknown_bare_flag_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(my_custom_flag)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"), "{enabled:?}");
    }

    /// The same "unknown is absent, not unparseable" fix, but for a `key =
    /// "value"` pair whose key this scan doesn't recognize, not just a bare
    /// flag — verified directly the same way (`cargo tree --target
    /// wasm32-wasip1` on `[target.'cfg(not(my_key = "x"))'.…]` includes the
    /// dependency; the bare, un-negated form does not) (Codex review on
    /// #2739, round 28, P1).
    #[test]
    fn a_not_unknown_valued_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(not(my_key = "x"))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"), "{enabled:?}");
    }

    /// The un-negated mirror: an unknown `key = "value"` pair alone is
    /// false, so the dependency stays disabled.
    #[test]
    fn an_unknown_valued_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(my_key = "x")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"), "{enabled:?}");
    }

    // --- lexically_normalize_path ---

    #[test]
    fn lexically_normalize_path_collapses_a_parent_dir_component() {
        assert_eq!(
            lexically_normalize_path(Path::new("/project/src/sub/../app.rs")),
            Path::new("/project/src/app.rs")
        );
    }

    #[test]
    fn lexically_normalize_path_collapses_multiple_parent_dir_components() {
        assert_eq!(
            lexically_normalize_path(Path::new("/project/a/b/../../c")),
            Path::new("/project/c")
        );
    }

    #[test]
    fn lexically_normalize_path_keeps_a_leading_parent_dir_with_nothing_to_pop() {
        assert_eq!(
            lexically_normalize_path(Path::new("../a/b")),
            Path::new("../a/b")
        );
    }

    /// CONSECUTIVE leading `..` components must not cancel each other out —
    /// `PathBuf::pop()` alone can't tell "a real directory name to cancel
    /// against" from "an earlier unresolved `..` kept for lack of one," so
    /// naively popping on `out.pop()`'s success wrongly cancelled the
    /// second `..` against the first, normalizing `../../shared/lib.rs` to
    /// `shared/lib.rs` — a different file entirely from the one a real
    /// `rustc`/Cargo build resolves (Codex review on #2739, round 30, P2).
    #[test]
    fn lexically_normalize_path_keeps_consecutive_leading_parent_dir_components() {
        assert_eq!(
            lexically_normalize_path(Path::new("../../shared/lib.rs")),
            Path::new("../../shared/lib.rs")
        );
    }

    #[test]
    fn lexically_normalize_path_drops_current_dir_components() {
        assert_eq!(
            lexically_normalize_path(Path::new("/project/./src/./app.rs")),
            Path::new("/project/src/app.rs")
        );
    }

    // --- resolver_v1_is_in_effect ---

    #[test]
    fn resolver_v1_is_in_effect_honors_an_explicit_resolver_field() {
        let table: toml::Table = toml::from_str(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\nresolver = \"1\"\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(resolver_v1_is_in_effect(dir.path(), Some(&table)));
    }

    #[test]
    fn resolver_v1_is_in_effect_is_false_for_an_explicit_resolver_2_edition_2018() {
        let table: toml::Table = toml::from_str(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2018\"\nresolver = \"2\"\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(!resolver_v1_is_in_effect(dir.path(), Some(&table)));
    }

    #[test]
    fn resolver_v1_is_in_effect_defaults_from_edition_2018() {
        let table: toml::Table =
            toml::from_str("[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2018\"\n")
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(resolver_v1_is_in_effect(dir.path(), Some(&table)));
    }

    #[test]
    fn resolver_v1_is_in_effect_defaults_from_edition_2021() {
        let table: toml::Table =
            toml::from_str("[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(!resolver_v1_is_in_effect(dir.path(), Some(&table)));
    }

    #[test]
    fn resolver_v1_is_in_effect_defaults_to_true_with_no_edition_at_all() {
        let table: toml::Table =
            toml::from_str("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(resolver_v1_is_in_effect(dir.path(), Some(&table)));
    }

    /// An ancestor workspace's own `resolver = "1"` governs the member
    /// regardless of the member's own edition-2021-implied v2 (Codex review
    /// on #2739, round 22, P1 — verified directly against a real `cargo
    /// rustc --target wasm32-wasip1` build).
    #[test]
    fn resolver_v1_is_in_effect_honors_an_ancestor_workspaces_explicit_resolver() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"mainpkg\"]\nresolver = \"1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        assert!(resolver_v1_is_in_effect(
            &dir.path().join("mainpkg"),
            Some(&member)
        ));
    }

    /// A package nested beneath a workspace but named in that workspace's
    /// own `[workspace] exclude` is NOT governed by it at all — verified
    /// directly against a real build (an excluded edition-2018 package
    /// under a `resolver = "2"` ancestor still uses resolver v1, the
    /// ancestor's setting entirely ignored). Falls back to the package's
    /// own edition instead of adopting the ancestor's resolver (Codex
    /// review on #2739, round 22, P1).
    #[test]
    fn resolver_v1_is_in_effect_ignores_an_ancestor_workspace_that_excludes_this_package() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"other\"]\nexclude = [\"mainpkg\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2018\"\n",
        )
        .unwrap();
        assert!(resolver_v1_is_in_effect(
            &dir.path().join("mainpkg"),
            Some(&member)
        ));
    }

    /// Same shape, but the nested package is NOT excluded (only some other
    /// directory is) — the ancestor workspace's resolver still governs.
    #[test]
    fn resolver_v1_is_in_effect_still_honors_an_ancestor_workspace_that_excludes_something_else() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"mainpkg\"]\nexclude = [\"other\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2018\"\n",
        )
        .unwrap();
        assert!(!resolver_v1_is_in_effect(
            &dir.path().join("mainpkg"),
            Some(&member)
        ));
    }

    /// A `crates/*` glob-shaped `exclude` entry still covers a package
    /// nested one segment under it.
    #[test]
    fn resolver_v1_is_in_effect_ignores_an_ancestor_workspace_excluding_via_a_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\nexclude = [\"crates/*\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2018\"\n",
        )
        .unwrap();
        assert!(resolver_v1_is_in_effect(
            &dir.path().join("crates/mainpkg"),
            Some(&member)
        ));
    }

    /// A `members = ["crates/*"]` glob must NOT reach a package one segment
    /// FURTHER than the pattern itself names — verified directly: `cargo
    /// metadata` on this exact shape (only `crates/group/app` present, no
    /// `crates/group` package) errors trying to load `crates/group` itself,
    /// never even considering `crates/group/app` a candidate. Before this
    /// fix, `any_glob_matches` treated `crates/*` as matching
    /// `crates/group/app` by truncating the comparison to the pattern's own
    /// two segments and ignoring the rel path's third — so the "explicit
    /// membership overrides an overlapping exclude" check above (which must
    /// run first) wrongly short-circuited on a member match that was never
    /// real, and an `exclude = ["crates/group/app"]` entry sitting right
    /// next to it was never even consulted (Codex review on #2739, round
    /// 25, P1).
    #[test]
    fn resolver_v1_is_in_effect_does_not_extend_a_members_glob_across_an_extra_segment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/group/app\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/group/app")).unwrap();
        let member: toml::Table =
            toml::from_str("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2018\"\n")
                .unwrap();
        assert!(resolver_v1_is_in_effect(
            &dir.path().join("crates/group/app"),
            Some(&member)
        ));
    }

    /// An explicit `[workspace] members` entry covering the package wins
    /// over an OVERLAPPING `exclude` entry — verified directly: `members =
    /// ["crates/app"]` alongside `exclude = ["crates"]` still governs
    /// `crates/app` with the workspace's own resolver, Cargo's documented
    /// precedence for this exact overlap shape. Checking `exclude` alone
    /// (without checking `members` first) would wrongly treat the package
    /// as excluded (Codex review on #2739, round 22, P2).
    #[test]
    fn resolver_v1_is_in_effect_lets_explicit_membership_override_an_overlapping_exclude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\"]\nexclude = [\"crates\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/app")).unwrap();
        let member: toml::Table =
            toml::from_str("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2018\"\n")
                .unwrap();
        assert!(!resolver_v1_is_in_effect(
            &dir.path().join("crates/app"),
            Some(&member)
        ));
    }

    /// A virtual workspace (no `[package]` of its own) with no explicit
    /// `resolver` field always defaults to `"1"`, regardless of any
    /// member's own edition — verified directly against Cargo's own
    /// warning for exactly this shape ("virtual workspace defaulting to
    /// `resolver = \"1\"`...").
    #[test]
    fn resolver_v1_is_in_effect_defaults_true_for_a_virtual_workspace_with_no_explicit_resolver() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"mainpkg\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        assert!(resolver_v1_is_in_effect(
            &dir.path().join("mainpkg"),
            Some(&member)
        ));
    }

    /// A NON-virtual workspace (it has its own `[package]`) with no explicit
    /// `resolver` field defaults from the ROOT package's own edition, not
    /// the member's — verified directly (a 2021-edition root plus a
    /// 2018-edition member yields resolver v2).
    #[test]
    fn resolver_v1_is_in_effect_defaults_from_the_root_packages_edition_in_a_non_virtual_workspace()
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"mainpkg\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2018\"\n",
        )
        .unwrap();
        assert!(!resolver_v1_is_in_effect(
            &dir.path().join("mainpkg"),
            Some(&member)
        ));
    }

    /// A non-virtual workspace's root package can spell its own edition as
    /// `edition.workspace = true` (a TOML table, not a string) instead of a
    /// literal edition string, inheriting from `[workspace.package]
    /// edition` in that SAME manifest — a real, sanctioned Cargo pattern,
    /// verified directly against a real build (a root package inheriting
    /// edition `"2021"` this way resolves as v2). Reading the root
    /// package's `edition` as a plain string alone would see `None` for a
    /// table value and default to "no edition at all" (2015, v1) — silently
    /// wrong here (Codex review on #2739, round 22, P2).
    #[test]
    fn resolver_v1_is_in_effect_resolves_the_root_packages_inherited_workspace_edition() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition.workspace = true\n\n\
             [workspace]\nmembers = [\"mainpkg\"]\n\n\
             [workspace.package]\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("mainpkg")).unwrap();
        let member: toml::Table = toml::from_str(
            "[package]\nname = \"mainpkg\"\nversion = \"0.1.0\"\nedition = \"2018\"\n",
        )
        .unwrap();
        assert!(!resolver_v1_is_in_effect(
            &dir.path().join("mainpkg"),
            Some(&member)
        ));
    }

    /// The scanned package's own manifest can itself declare `[workspace]`
    /// (a non-virtual workspace root that is also a package) — this must be
    /// recognized without needing a filesystem walk at all.
    #[test]
    fn resolver_v1_is_in_effect_honors_a_self_owned_workspace_resolver() {
        let table: toml::Table = toml::from_str(
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = []\nresolver = \"1\"\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(resolver_v1_is_in_effect(dir.path(), Some(&table)));
    }

    /// Integration-level: a `#[cfg(feature = "dep")]`-gated `#[edge]`
    /// handler, `dep` optional and declared ONLY under
    /// `[target.'cfg(windows)'.dependencies]`, referenced via `default =
    /// ["dep/extra"]` — under an edition-2018 (resolver v1) manifest, the
    /// real wasm32-wasip1 build turns this feature on (verified directly),
    /// so the scan must include the handler rather than treating it as
    /// cfg'd-out (Codex review on #2739, round 22, P1).
    #[test]
    fn resolve_edge_scan_includes_a_resolver_v1_unified_target_specific_feature_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"
            edition = "2018"

            [features]
            default = ["dep/extra"]

            [target.'cfg(windows)'.dependencies]
            dep = { version = "1", optional = true }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[cfg(feature = "dep")]
            #[edge]
            pub fn show() {}
            "#,
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.names(), vec!["show"]);
    }

    /// Same manifest, but edition 2021 (resolver v2): the real build never
    /// turns `dep`'s feature on for wasm32-wasip1, so the handler stays
    /// cfg'd-out — the existing, still-correct behavior this fix must not
    /// regress.
    #[test]
    fn resolve_edge_scan_excludes_a_resolver_v2_target_specific_feature_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"
            edition = "2021"

            [features]
            default = ["dep/extra"]

            [target.'cfg(windows)'.dependencies]
            dep = { version = "1", optional = true }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[cfg(feature = "dep")]
            #[edge]
            pub fn show() {}
            "#,
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert!(scan.is_empty());
    }

    /// Verified directly: a bare `#[cfg(target_has_atomic)]` (no value) does
    /// NOT compile in even on a native target with real
    /// `target_has_atomic = "..."` values — rustc never emits these keys as
    /// a bare flag, only as `key = "value"` pairs — so it is always false,
    /// and `not(target_has_atomic)` is always TRUE. Previously the bare form
    /// failed to parse, which failed the WHOLE `not(...)` predicate rather
    /// than resolving to `Leaf(false)` for just that one leaf (Codex review
    /// on #2739, round 19, P1).
    #[test]
    fn a_bare_known_key_inside_not_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(not(target_has_atomic))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// The un-negated bare form must evaluate false.
    #[test]
    fn a_bare_known_key_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(target_has_atomic)'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// Verified directly against `rustc --print cfg --target wasm32-wasip1`:
    /// that target reports `target_os = "wasi"`, a real and commonly used
    /// predicate distinct from `target_arch = "wasm32"` — round 13's grammar
    /// only recognized the latter, so this predicate evaluated as "does not
    /// match" even though it genuinely does, excluding a route Cargo really
    /// compiles for the capsule (Codex review on #2739, round 14, P1).
    #[test]
    fn a_target_os_wasi_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(target_os = "wasi")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// Same for `target_family = "wasm"`, wasm32-wasip1's third resolvable
    /// key.
    #[test]
    fn a_target_family_wasm_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(target_family = "wasm")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// Verified directly against `rustc --print cfg --target wasm32-wasip1`:
    /// that target reports `target_env = "p1"`, `target_pointer_width =
    /// "32"`, and `target_vendor = "unknown"` — three more real predicates
    /// round 14's grammar did not recognize (Codex review on #2739, round
    /// 15, P1).
    #[test]
    fn further_wasm32_wasip1_target_cfg_keys_also_enable_the_dependency() {
        for cfg in [
            r#"target_env = "p1""#,
            r#"target_pointer_width = "32""#,
            r#"target_vendor = "unknown""#,
            r#"target_endian = "little""#,
        ] {
            let manifest = format!(
                r#"
                [target.'cfg({cfg})'.dependencies]
                dep = {{ version = "1", optional = true }}

                [features]
                default = ["dep/extra"]
                "#
            );
            let enabled = enabled_features_from_manifest(&manifest, &[]);
            assert!(
                enabled.contains("dep"),
                "cfg({cfg}) should match: {enabled:?}"
            );
        }
    }

    /// A recognized key with the WRONG value (wasm32-wasip1 is not Linux)
    /// must evaluate as "does not match," not as unresolvable — the two
    /// currently produce the same top-level answer, but only because the
    /// wrong-value case is an explicit `Leaf(false)`, not a parse failure.
    #[test]
    fn a_target_os_linux_target_specific_dependency_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(target_os = "linux")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// Verified directly against `rustc --print cfg --target wasm32-wasip1`:
    /// `target_has_atomic` legitimately lists SEVERAL simultaneous values for
    /// that target, and a real predicate checks membership —
    /// `target_has_atomic = "32"` is satisfied by one of five reported
    /// values, not because it is the target's only value (Codex review on
    /// #2739, round 16, P1).
    #[test]
    fn multi_valued_wasm32_wasip1_target_cfg_keys_also_enable_the_dependency() {
        let manifest = r#"
            [target.'cfg(target_has_atomic = "32")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"), "{enabled:?}");
    }

    /// Verified directly against a real Cargo build (`cargo tree --target
    /// wasm32-wasip1`): a `[target.'cfg(target_feature = "crt-static")'.…]`
    /// table pulls in NOTHING for wasm32-wasip1, even though rustc reports
    /// `crt-static` as one of that target's own default features — Cargo's
    /// dependency-table evaluator does not resolve `target_feature` against
    /// real target data at all, unlike every other key here. Previously this
    /// scan checked `target_feature` the same way as `target_has_atomic`
    /// (real membership), wrongly enabling a dependency Cargo never actually
    /// activates for the capsule (Codex review on #2739, round 23, P1).
    #[test]
    fn a_target_feature_target_specific_dependency_is_never_enabled() {
        let manifest = r#"
            [target.'cfg(target_feature = "crt-static")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"), "{enabled:?}");
    }

    /// The mirror image: `not(target_feature = "...")` is a real,
    /// unconditionally-true predicate in Cargo's dependency-table evaluator
    /// for every target, including wasm32-wasip1, once again verified
    /// directly against a real `cargo tree --target wasm32-wasip1` (it
    /// includes the dependency). A sole `#[cfg(feature = "dep")] #[edge]`
    /// handler behind exactly this table must be scanned as enabled — Codex
    /// review on #2739, round 23, P1.
    #[test]
    fn a_negated_target_feature_target_specific_dependency_is_always_enabled() {
        let manifest = r#"
            [target.'cfg(not(target_feature = "crt-static"))'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"), "{enabled:?}");
    }

    /// A value NOT among the target's real values for that key must still
    /// evaluate as "does not match," confirming this is genuine set
    /// membership, not "any value accepted once the key is known."
    #[test]
    fn a_target_has_atomic_value_the_target_does_not_report_is_not_enabled() {
        let manifest = r#"
            [target.'cfg(target_has_atomic = "128")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    /// Verified directly against `rustc --print cfg --target wasm32-wasip1`:
    /// that target always reports `panic = "abort"` — not a target-arch
    /// property like the rest of the table, but still a real, always-true
    /// predicate for the capsule's own build (Codex review on #2739, round
    /// 17, P1).
    #[test]
    fn a_panic_abort_target_specific_dependency_also_enables_the_dependency() {
        let manifest = r#"
            [target.'cfg(panic = "abort")'.dependencies]
            dep = { version = "1", optional = true }

            [features]
            default = ["dep/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("dep"));
    }

    /// `dep?/extra` (the weak-dependency form) makes no promise that `dep`
    /// itself is on — only that IF something else enables it, `extra`
    /// comes along too — so it must not enable `dep`.
    #[test]
    fn a_weak_dependency_feature_reference_does_not_enable_the_dependency() {
        let manifest = r#"
            [features]
            default = ["dep?/extra"]
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(!enabled.contains("dep"));
    }

    #[test]
    fn cfg_transitive_default_feature_is_included() {
        let mut features = BTreeSet::new();
        // Mirrors `default = ["a"]`, `a = ["b"]` already expanded by
        // `enabled_features_from_manifest`.
        features.insert("a".to_owned());
        features.insert("b".to_owned());
        let scan = scan_sources_with_features(
            &[(
                "src/main.rs",
                r#"
                #[cfg(feature = "b")]
                #[edge]
                fn shown() {}
                "#,
            )],
            &features,
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn resolve_edge_scan_honors_cargo_toml_default_features() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [features]
            default = ["a"]
            a = ["b"]
            unused = []
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            r#"
            #[cfg(feature = "b")]
            #[edge]
            fn shown() {}

            #[cfg(feature = "unused")]
            #[edge]
            fn hidden() {}
            "#,
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.names(), vec!["shown"]);
    }

    /// A hyphenated package name (`my-app`) compiles to the Rust crate
    /// identifier `my_app` — `-` is not legal in a Rust identifier — so
    /// `edge_routes![my_app::show]` must match, not `edge_routes![my-app::show]`
    /// (which is not even valid Rust and could never appear as written).
    ///
    /// Written to `src/lib.rs`, not `src/main.rs`: the crate-name-qualified
    /// form only means anything against a real library crate — real Rust has
    /// no way for `src/main.rs` to reference itself by the package's own
    /// name, only `crate::` does that (round 15, P2, see
    /// `crate_context_from_file`'s own doc on why `src/main.rs` now gets a
    /// distinct identity from the library's).
    #[test]
    fn resolve_edge_scan_derives_the_rust_crate_name_from_a_hyphenated_package() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.crate_name.as_deref(), Some("my_app"));
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    #[test]
    fn rust_crate_name_from_manifest_converts_hyphens_to_underscores() {
        let table =
            toml::from_str::<toml::Table>("[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n")
                .unwrap();
        assert_eq!(
            rust_crate_name_from_manifest(&table).as_deref(),
            Some("my_app")
        );
    }

    #[test]
    fn rust_crate_name_from_manifest_prefers_an_explicit_lib_name() {
        let table = toml::from_str::<toml::Table>(
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\nname = \"totally_different\"\n",
        )
        .unwrap();
        assert_eq!(
            rust_crate_name_from_manifest(&table).as_deref(),
            Some("totally_different")
        );
    }

    /// `premium` just because `premium` is not in the manifest's own
    /// `default = [...]` — the real build turns it on, so the scan must too
    /// (Codex review on #2739, P1).
    #[test]
    fn resolve_edge_scan_with_extra_file_honors_explicitly_requested_features() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [features]
            default = []
            premium = []
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            r#"
            #[cfg(feature = "premium")]
            #[edge]
            fn premium_only() {}
            "#,
        )
        .unwrap();

        // Without the requested feature, the route looks cfg'd-out.
        assert!(resolve_edge_scan(dir.path()).is_empty());
        // With it requested, exactly as `--features premium` would forward,
        // the route must be found.
        let scan = resolve_edge_scan_with_extra_file(dir.path(), &["premium"], None);
        assert_eq!(scan.names(), vec!["premium_only"]);
    }

    // --- Item 2: qualified registration matching ---

    #[test]
    fn qualified_registration_does_not_match_a_same_named_fn_in_another_module() {
        let scan = scan_one(
            r"
            mod users {
                #[edge]
                pub fn show() {}
            }
            mod admin {
                #[edge]
                pub fn show() {}
            }

            fn wire() { edge_routes![users::show]; }
            ",
        );
        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].name, "show");
        assert_eq!(unregistered[0].module_path, vec!["admin".to_owned()]);

        let registered: Vec<&str> = scan
            .registered_fns()
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(registered, vec!["show"]);
        assert_eq!(
            scan.registered_fns()[0].module_path,
            vec!["users".to_owned()]
        );
    }

    #[test]
    fn qualified_registration_matches_a_genuinely_nested_fn() {
        let scan = scan_one(
            r"
            mod routes {
                #[edge]
                pub fn nested() {}
            }
            fn wire() { edge_routes![routes::nested]; }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `edge_routes![crate::users::show]` is the same route as `users::show`
    /// — `crate::` is Cargo's own absolute-path prefix, not part of the
    /// module chain `f.module_path` records. Without stripping it, this
    /// registration would compare `["crate", "users"]` against `["users"]`,
    /// never match, and wrongly warn that `show` is unregistered.
    #[test]
    fn a_crate_qualified_registration_matches_its_module_path() {
        let scan = scan_one(
            r"
            mod users {
                #[edge]
                pub fn show() {}
            }
            fn wire() { edge_routes![crate::users::show]; }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `show`'s module path is genuinely empty here — declared directly in
    /// `src/lib.rs`, a crate root — not "unknown". A qualified registration
    /// for a *different* module must not wildcard-match it just because its
    /// path happens to be empty: that would let `edge_routes![users::show]`
    /// silently satisfy this crate-root `show`, hiding that it is really
    /// unregistered while a same-named `users::show` (if one existed) would
    /// look doubly registered.
    #[test]
    fn a_qualified_registration_does_not_wildcard_match_a_crate_root_fn() {
        let scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            fn wire() { edge_routes![users::show]; }
            ",
        );
        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].name, "show");
        assert!(unregistered[0].module_path.is_empty());
        assert!(scan.registered_fns().is_empty());
    }

    /// `edge_routes![self::show]` written inside `mod users { ... }` names
    /// `users::show`, the same as writing `users::show` (or just `show`)
    /// there — `self` is Cargo's own name for "the current module", not a
    /// literal path segment to compare against `show`'s recorded module
    /// path of `["users"]`.
    #[test]
    fn a_self_qualified_registration_resolves_to_its_enclosing_module() {
        let scan = scan_one(
            r"
            mod users {
                #[edge]
                pub fn show() {}

                fn wire() { edge_routes![self::show]; }
            }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `edge_routes![super::show]` written inside a nested `mod admin { ... }`
    /// names the parent module's `show`, not a same-named `admin::show`.
    #[test]
    fn a_super_qualified_registration_resolves_to_the_parent_module() {
        let scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            mod admin {
                fn wire() { edge_routes![super::show]; }
            }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `edge_routes![my_crate::show]` — the crate's own `[package] name`
    /// used as a path root — is Rust's other spelling for `crate::show`.
    /// `scan.crate_name` is set the way `resolve_edge_scan_with_extra_file`
    /// sets it from a real manifest; `scan_one`'s inline-source tests have
    /// no manifest to read it from, so it is set directly here.
    #[test]
    fn a_crate_name_qualified_registration_matches_its_module_path() {
        let mut scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            fn wire() { edge_routes![edgeapp::show]; }
            ",
        );
        scan.crate_name = Some("edgeapp".to_owned());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A package name that collides with a Rust keyword (`name = "type"`)
    /// compiles to a crate identifier only legal in path position spelled as
    /// a raw identifier (`edge_routes![r#type::show]`) — the tokenizer keeps
    /// that `r#` prefix verbatim, but `crate_name` itself never carries one,
    /// so the comparison must normalize it away (Codex review on #2739,
    /// round 22, P2).
    #[test]
    fn a_raw_identifier_crate_name_qualified_registration_matches_its_module_path() {
        let mut scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            fn wire() { edge_routes![r#type::show]; }
            ",
        );
        scan.crate_name = Some("type".to_owned());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A CONVENTIONAL library module declared `mod r#type;` at the crate
    /// root: Rust loads `src/type.rs` (verified directly), so the ordinary
    /// `src/` walk's `crate_context_from_file` derives module path `["type"]`
    /// for that file — no `r#` at all, since it comes from the literal file
    /// name — while a registration written as
    /// `edge_routes![my_app::r#type::show]` tokenizes its qualifier with the
    /// `r#` intact. The two spellings must still compare equal (Codex review
    /// on #2739, round 22, P2).
    #[test]
    fn a_raw_identifier_modules_conventional_file_still_matches_its_registration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "mod r#type;\nfn wire() { edge_routes![my_app::r#type::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/type.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `edge_routes![::my_crate::show]` — the leading `::` anchors the path
    /// to a crate root the same way an explicit `my_crate::show` already
    /// does, ruling out a same-named local item shadowing the crate name.
    /// It must match the same as the unanchored form, not fail to match at
    /// all because of the leading separator (Codex review on #2739, round 9,
    /// P2).
    #[test]
    fn a_leading_double_colon_qualified_registration_matches_its_module_path() {
        let mut scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            fn wire() { edge_routes![::edgeapp::show]; }
            ",
        );
        scan.crate_name = Some("edgeapp".to_owned());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// A qualifier that is neither `crate` nor the scanned crate's own name
    /// still does not wildcard-match a crate-root function — only the exact
    /// `crate_name` earns the same treatment as `crate`.
    #[test]
    fn an_unrelated_qualifier_does_not_match_even_with_a_crate_name_set() {
        let mut scan = scan_one(
            r"
            #[edge]
            pub fn show() {}

            fn wire() { edge_routes![otherpkg::show]; }
            ",
        );
        scan.crate_name = Some("edgeapp".to_owned());
        assert_eq!(
            scan.registered_fns().len(),
            0,
            "{:?}",
            scan.registered_fns()
        );
    }

    /// `edge_routes![v1::show]` written inside `mod api { mod v1 { ... } }`
    /// is an ordinary relative reference: real Rust resolves `v1` against
    /// the invocation's own module first, giving `api::v1::show`. This
    /// scanner deliberately does not resolve that — round 5 tried it and
    /// round 6's Codex review found a real false positive it could cause
    /// (see [`registration_candidates`]'s doc) — so this reports as a
    /// false-positive "unregistered" warning, the documented safe direction,
    /// rather than as a match.
    #[test]
    fn a_relative_child_module_registration_is_left_unresolved() {
        let scan = scan_one(
            r"
            mod api {
                mod v1 {
                    #[edge]
                    pub fn show() {}
                }
                fn wire() { edge_routes![v1::show]; }
            }
            ",
        );
        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(
            unregistered[0].module_path,
            vec!["api".to_owned(), "v1".to_owned()]
        );
        assert!(scan.registered_fns().is_empty());
    }

    /// A `pub use crate::handlers::*;` glob re-export makes `handlers::show`
    /// also reachable as `api::show` in real Rust, but this scan does not
    /// track `use` at all (see the module doc's "Recognition limits"), so a
    /// registration written against the re-exported path reports the same
    /// safe-direction false-positive "unregistered" warning as an unresolved
    /// `use`-alias, rather than crediting `handlers::show` (Codex review on
    /// #2739, round 22, P2 — investigated, not applied).
    #[test]
    fn a_glob_reexported_registration_is_left_unresolved() {
        let scan = scan_one(
            r"
            mod handlers {
                #[edge]
                pub fn show() {}
            }
            mod api {
                pub use crate::handlers::*;
            }
            fn wire() { edge_routes![api::show]; }
            ",
        );
        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].module_path, vec!["handlers".to_owned()]);
        assert!(scan.registered_fns().is_empty());
    }

    /// Leaving a relative reference unresolved must not accidentally
    /// wildcard-match either side of a sibling-module pair: with `mod a {
    /// mod x { ... } }` and `mod b { mod x { ... } }`, `edge_routes![x::f]`
    /// written inside `mod b` matches neither — the exact scenario the
    /// exact-equality match in `is_registered` exists to keep from silently
    /// muting a real "unregistered" warning.
    #[test]
    fn a_relative_child_module_registration_does_not_shadow_a_sibling() {
        let scan = scan_one(
            r"
            mod a {
                mod x {
                    #[edge]
                    pub fn f() {}
                }
            }
            mod b {
                mod x {
                    #[edge]
                    pub fn f() {}
                }
                fn wire() { edge_routes![x::f]; }
            }
            ",
        );
        assert_eq!(scan.unregistered().len(), 2, "{:?}", scan.unregistered());
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
    }

    /// `mod users;` / `mod admin;` (separate files, not inline `mod { }`
    /// blocks) must derive the same module path from each file's location, so
    /// a same-named `admin::show` cannot pass as a match for a registration of
    /// `users::show`. This is the out-of-line counterpart to
    /// `qualified_registration_does_not_match_a_same_named_fn_in_another_module`.
    #[test]
    fn module_path_is_derived_from_an_out_of_line_modules_file() {
        let scan = scan_sources(&[
            ("src/users.rs", "#[edge]\npub fn show() {}\n"),
            ("src/admin.rs", "#[edge]\npub fn show() {}\n"),
            (
                "src/lib.rs",
                "mod users; mod admin; fn wire() { edge_routes![users::show]; }",
            ),
        ]);

        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].name, "show");
        assert_eq!(unregistered[0].module_path, vec!["admin".to_owned()]);

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].name, "show");
        assert_eq!(registered[0].module_path, vec!["users".to_owned()]);
    }

    /// `src/bin/<name>.rs` is Cargo's own crate root for that `[[bin]]`
    /// target, not a `bin::edge_capsule` submodule of the app's library
    /// crate — so a handler declared directly in it has an empty module
    /// path, and `edge_routes![crate::show]` written in that same file
    /// matches it.
    #[test]
    fn a_flat_bin_target_file_is_its_own_crate_root() {
        let scan = scan_sources(&[(
            "src/bin/edge-capsule.rs",
            "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
        )]);
        assert_eq!(scan.functions[0].module_path, Vec::<String>::new());
        assert_eq!(scan.functions[0].crate_root, "bin:edge-capsule");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// `src/lib.rs` and `src/bin/edge-capsule.rs` can each declare a
    /// crate-root `show` — both with an *empty* `module_path`, since each is
    /// its own crate's root — so `edge_routes![crate::show]` written in the
    /// capsule must credit only the capsule's own `show`, never the
    /// library's unrelated same-named one (Codex review on #2739, round 7,
    /// P2).
    #[test]
    fn crate_qualified_registrations_do_not_cross_a_bin_targets_crate_boundary() {
        let scan = scan_sources(&[
            ("src/lib.rs", "#[edge]\npub fn show() {}\n"),
            (
                "src/bin/edge-capsule.rs",
                "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
            ),
        ]);

        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "src/lib.rs");

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "src/bin/edge-capsule.rs");
    }

    /// `src/main.rs` (the package's own implicit default binary) and
    /// `src/lib.rs` are two separate crates too, exactly like a named
    /// `[[bin]]` — `crate::show` written in `main.rs` must credit only
    /// `main.rs`'s own `show`, never the library's unrelated same-named one
    /// (Codex review on #2739, round 15, P2).
    #[test]
    fn crate_qualified_registrations_do_not_cross_the_default_binarys_crate_boundary() {
        let scan = scan_sources(&[
            ("src/lib.rs", "#[edge]\npub fn show() {}\n"),
            (
                "src/main.rs",
                "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
            ),
        ]);

        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "src/lib.rs");

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "src/main.rs");
    }

    /// A crate-name-qualified registration (`my_app::show`) can only ever
    /// mean the library crate in real Rust — a crate has no way to refer to
    /// itself by its own external name, only `crate::` does that. Written
    /// from within `src/main.rs`, `edgeapp::show` must reach the library's
    /// `show`, not `main.rs`'s own same-named one — so `main.rs`'s `show`
    /// stays unregistered even though a same-named function elsewhere was
    /// credited.
    #[test]
    fn a_crate_name_qualified_registration_does_not_match_the_default_binarys_own_function() {
        let mut scan = scan_sources(&[
            ("src/lib.rs", "#[edge]\npub fn show() {}\n"),
            (
                "src/main.rs",
                "#[edge]\npub fn show() {}\nfn main() { edge_routes![edgeapp::show]; }\n",
            ),
        ]);
        scan.crate_name = Some("edgeapp".to_owned());

        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "src/main.rs");

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "src/lib.rs");
    }

    /// Also verified through a real project directory (not just in-memory
    /// sources): `src/main.rs`'s own `show`, registered there with a bare
    /// `edge_routes![show]`, must not accidentally satisfy the library's
    /// unrelated `show`.
    #[test]
    fn resolve_edge_scan_gives_src_main_rs_its_own_crate_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "#[edge]\npub fn show() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "src/lib.rs");
    }

    /// A custom `[lib] path` (the library target compiled from a file other
    /// than the conventional `src/lib.rs`) must be treated as the crate
    /// root too — an empty module path, not one derived from its own file
    /// name — the same way a custom `[[bin]] path` already is (Codex review
    /// on #2739, round 20, P2).
    #[test]
    fn resolve_edge_scan_honors_a_custom_lib_path_as_the_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"src/app.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions[0].module_path, Vec::<String>::new());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A custom `[lib] path` nested under its own subdirectory
    /// (`src/custom/app.rs`) whose root declares an out-of-line submodule
    /// (`mod routes;`, resolving to `src/custom/routes.rs`) must resolve
    /// that submodule's module path the same way real Rust does — relative
    /// to the crate root (`routes`), not `crate_context_from_file`'s
    /// directory-mirrors-module heuristic, which would invent a spurious
    /// leading `custom` segment from the enclosing directory name (Codex
    /// review on #2739, round 22, P2).
    #[test]
    fn resolve_edge_scan_resolves_a_nested_custom_lib_paths_own_submodule() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/custom")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"src/custom/app.rs\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/custom/app.rs"), "mod routes;\n").unwrap();
        std::fs::write(
            dir.path().join("src/custom/routes.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::routes::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].module_path, vec!["routes".to_owned()]);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A custom `[lib] path`'s own out-of-line submodule can ALSO carry a
    /// `#[path = "..."]` override — `scan_bin_crate_tree`'s BFS (following
    /// `mod` declarations directly, since a non-conventional crate tree has
    /// no directory convention to fall back on) previously never consulted
    /// the attribute at all, so the redirected file was invisible to it
    /// entirely: neither the ordinary `src/` walk's own `#[path]`-override
    /// pass (round 31, scoped to that walk only) nor this BFS ever reached
    /// it (Codex review on #2739, round 35, P1).
    #[test]
    fn resolve_edge_scan_follows_a_path_attribute_in_a_custom_lib_paths_crate_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"src/app.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.rs"),
            "#[path = \"actual.rs\"]\nmod handlers;\nfn wire() { edge_routes![my_app::handlers::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].module_path, vec!["handlers".to_owned()]);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// With `[package] autobins = false` and no `[[bin]]` entry naming it,
    /// `src/bin/foo.rs` is not compiled as its own crate at all — verified
    /// directly via a real build (no binary is produced, and `cargo
    /// metadata` lists no `bin` target). Here it is an ordinary library
    /// submodule instead, reached via `mod bin { mod foo; }` in `src/lib.rs`
    /// (a real, if coincidentally-named, inline module — Rust resolves its
    /// own out-of-line `foo` at `src/bin/foo.rs` by the same
    /// directory-mirrors-modules rule). Before this fix, the plain `src/`
    /// walk unconditionally classified any file under `src/bin/` as its own
    /// `bin:<name>` crate regardless of `autobins`, so `foo`'s real module
    /// path (`bin::foo`) was never matched by `edge_routes![crate::bin::foo::show]`
    /// (Codex review on #2739, round 35, P2).
    #[test]
    fn resolve_edge_scan_treats_an_unreal_src_bin_file_as_an_ordinary_library_submodule_when_autobins_is_off()
     {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\nautobins = false\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r"
            mod bin {
                mod foo;
            }
            fn wire() { edge_routes![crate::bin::foo::show]; }
            ",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bin/foo.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].crate_root, "");
        assert_eq!(
            scan.functions[0].module_path,
            vec!["bin".to_owned(), "foo".to_owned()]
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same idea, but an EXPLICIT `[[bin]] name = "foo"` entry names it —
    /// `foo` genuinely is its own bin crate despite `autobins = false`, so
    /// `src/bin/foo.rs` must still get the `bin:foo` identity.
    #[test]
    fn resolve_edge_scan_still_recognizes_an_explicitly_declared_bin_when_autobins_is_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\nautobins = false\n\n\
             [[bin]]\nname = \"foo\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bin/foo.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].crate_root, "bin:foo");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// The identical `autobins = false` mistake, but for `src/main.rs`
    /// itself — verified directly via a real build that `autobins = false`
    /// suppresses Cargo's automatic discovery of `src/main.rs` too, not
    /// just `src/bin/*.rs` (a real build with no `[[bin]]` entry produces no
    /// binary whatsoever). Without a real bin target, `src/main.rs` here
    /// falls back to an ordinary (if inert) library-relative treatment
    /// rather than the definitely-wrong `bin:main` identity.
    #[test]
    fn resolve_edge_scan_does_not_treat_src_main_as_its_own_crate_when_autobins_is_off_and_undeclared()
     {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\nautobins = false\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![crate::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].crate_root, "", "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A `#[path = "actual.rs"] mod handlers;` declaration redirects which
    /// FILE backs the module without renaming it — real Rust still resolves
    /// `handlers::show`, never `actual::show`. Before this scan resolved
    /// `#[path]` overrides, the ordinary `src/` walk (plain directory
    /// listing, not `mod`-declaration following) found `src/actual.rs` and
    /// guessed its module path straight from that filename instead (Codex
    /// review on #2739, round 31, P2).
    #[test]
    fn resolve_edge_scan_resolves_a_path_attribute_out_of_line_modules_real_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[path = \"actual.rs\"]\nmod handlers;\nfn wire() { edge_routes![crate::handlers::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].module_path, vec!["handlers".to_owned()]);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same idea, but the `#[path]`-attributed `mod` is nested inside an
    /// INLINE module — the override's real module path must include that
    /// enclosing inline segment too, not just the mod's own name, AND the
    /// target file must be resolved relative to the DIRECTORY THAT MODULE
    /// PATH IMPLIES (`src/api/actual.rs`), not the declaring file's own
    /// physical directory (`src/actual.rs`) — verified directly via a real
    /// build that `mod api { #[path = "actual.rs"] mod handlers; }` in
    /// `src/lib.rs` finds `src/api/actual.rs` (Codex review on #2739,
    /// round 33, P2 — round 31's own fix joined `#[path]`'s value directly
    /// onto the declaring file's directory, ignoring any enclosing inline
    /// module).
    #[test]
    fn resolve_edge_scan_resolves_a_path_attribute_mod_nested_inside_an_inline_module() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/api")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            mod api {
                #[path = "actual.rs"]
                mod handlers;
            }
            fn wire() { edge_routes![crate::api::handlers::show]; }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/api/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(
            scan.functions[0].module_path,
            vec!["api".to_owned(), "handlers".to_owned()]
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same idea again, but the enclosing inline module is declared inside
    /// an already OUT-OF-LINE file (`src/foo.rs`, reached via a plain,
    /// non-aliased `mod foo;`) rather than directly in `src/lib.rs` —
    /// verified directly via a real build that `mod inline { #[path =
    /// "actual.rs"] mod handlers; }` in `src/foo.rs` finds
    /// `src/foo/inline/actual.rs`, never `src/inline/actual.rs`. The
    /// declaring file's own physical PARENT directory (`src/`, from
    /// `declaring_file.parent()`) is the wrong base for this fold: `foo.rs`
    /// itself is not `src/lib.rs`, so its own conventional child directory
    /// is `src/foo/`, not `src/` (Codex review on #2739, round 46, P2).
    #[test]
    fn resolve_edge_scan_resolves_a_path_attribute_mod_nested_inside_an_inline_module_of_an_out_of_line_file()
     {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/foo/inline")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "mod foo;\n").unwrap();
        std::fs::write(
            dir.path().join("src/foo.rs"),
            r#"
            mod inline {
                #[path = "actual.rs"]
                mod handlers;
            }
            fn wire() { edge_routes![crate::foo::inline::handlers::show]; }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/foo/inline/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(
            scan.functions[0].module_path,
            vec!["foo".to_owned(), "inline".to_owned(), "handlers".to_owned()]
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same as above, but the manifest spells the custom `[lib] path` with a
    /// leading `./` (`./src/app.rs`) — a dot-component Cargo normalizes away
    /// to the same `src/app.rs` target, so the scan's own crate-root
    /// identity check must resolve the same way rather than comparing the
    /// raw manifest string against the walk's already-normalized `rel`
    /// (Codex review on #2739, round 21, P2).
    #[test]
    fn resolve_edge_scan_honors_a_dot_component_custom_lib_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"./src/app.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].module_path, Vec::<String>::new());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same as above, but the manifest spells the custom `[lib] path` with a
    /// `..` component (`src/sub/../app.rs`) that lexically resolves to the
    /// same `src/app.rs` the ordinary walk finds directly. Unlike a `.`
    /// component, Rust's own `Path`/`PathBuf` equality does NOT collapse
    /// `..` (verified directly), so without normalizing it first, the file
    /// was scanned twice — once via `scan_bin_crate_tree` with the correct
    /// crate-root identity, once via the ordinary walk with a wrong
    /// library-submodule one — reporting the same handler as both
    /// registered and (under its wrong identity) unregistered (Codex review
    /// on #2739, round 22, P2).
    #[test]
    fn resolve_edge_scan_honors_a_dot_dot_component_custom_lib_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"src/sub/../app.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].module_path, Vec::<String>::new());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// Same as above, but the custom `[lib] path` points OUTSIDE `src/`
    /// entirely — the ordinary `src/`-directory walk never reaches it at
    /// all, so it needs its own dedicated scan via `scan_bin_crate_tree`
    /// (Codex review on #2739, round 21, P1).
    #[test]
    fn resolve_edge_scan_honors_a_custom_lib_path_outside_src() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"lib/app.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("lib/app.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].module_path, Vec::<String>::new());
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A custom `[lib] path` REPLACES the conventional `src/lib.rs` as this
    /// crate's library root — Cargo never compiles `src/lib.rs` at all once
    /// `[lib] path` points elsewhere, even when the file still exists on
    /// disk with its own `#[edge]` handler — verified directly via a real
    /// build (a `compile_error!` placed in such a `src/lib.rs` does not fail
    /// the build). The ordinary `src/`-directory walk previously still
    /// scanned that now-inactive file, reporting its stale, never-compiled
    /// handler as a real but unregistered route and wrongly failing
    /// preflight for an otherwise valid capsule (Codex review on #2739,
    /// round 46, P2).
    #[test]
    fn resolve_edge_scan_excludes_the_inactive_conventional_lib_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"src/app.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.rs"),
            "#[edge]\npub fn show() {}\nfn wire() { edge_routes![my_app::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "#[edge]\npub fn stale() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].name, "show");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A project combining an out-of-`src/` custom `[lib] path` AND an
    /// out-of-tree custom `[[bin]]` path at once: both must get their own,
    /// independent crate roots, and a bare `edge_routes![show]` inside the
    /// bin crate must not be credited by the library's same-named function
    /// (Codex review on #2739, round 21, P2).
    #[test]
    fn resolve_edge_scan_with_extra_file_honors_a_custom_lib_path_outside_src_alongside_a_custom_bin_path()
     {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [lib]
            path = "lib/app.rs"

            [[bin]]
            name = "edge-capsule"
            path = "cmd/edge.rs"
            "#,
        )
        .unwrap();
        std::fs::write(dir.path().join("lib/app.rs"), "#[edge]\npub fn show() {}\n").unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );

        let unregistered = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "lib/app.rs");

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "cmd/edge.rs");
    }

    /// A *bare* registration, unlike the `crate::`-qualified one above, DOES
    /// cross a `[[bin]]` target's crate boundary: `use my_app::handlers::*;`
    /// followed by a bare `edge_routes![show]` is ordinary, valid Rust this
    /// scanner cannot see (it does not track `use` imports), so restricting
    /// the match to the entry's own recorded crate turned that pattern into a
    /// false "unregistered" scan — a hard, build-blocking error from
    /// `run_edge_capsule_build` when it is the only route, not merely a
    /// missed warning (Codex review on #2739, round 10, P2).
    #[test]
    fn a_bare_registration_crosses_a_bin_targets_crate_boundary() {
        let scan = scan_sources(&[
            ("src/lib.rs", "#[edge]\npub fn show() {}\n"),
            (
                "src/bin/edge-capsule.rs",
                "fn main() { edge_routes![show]; }\n",
            ),
        ]);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// Same as above for the directory-style bin layout
    /// (`src/bin/<name>/main.rs`), and its own sibling submodule
    /// (`src/bin/<name>/routes.rs`) is relative to THAT crate's root, not to
    /// `src/`.
    #[test]
    fn a_directory_style_bin_targets_own_submodule_is_relative_to_its_root() {
        let scan = scan_sources(&[
            (
                "src/bin/edge-capsule/main.rs",
                "mod routes; fn main() { edge_routes![routes::show]; }\n",
            ),
            (
                "src/bin/edge-capsule/routes.rs",
                "#[edge]\npub fn show() {}\n",
            ),
        ]);
        assert_eq!(scan.functions[0].module_path, vec!["routes".to_owned()]);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A custom `[[bin]] path` for the edge-capsule target pointing outside
    /// `src/` (e.g. `path = "cmd/edge.rs"`) reaches the scan only through
    /// `resolve_edge_scan_with_extra_file`'s `extra_file`. It must get its
    /// own crate root, not the library crate's — a `crate::`-qualified
    /// registration written in it should resolve to its own handler, never
    /// to a same-named library-crate function (Codex review on #2739, round
    /// 8, P2).
    #[test]
    fn a_custom_out_of_tree_bin_path_is_its_own_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [[bin]]
            name = "edge-capsule"
            path = "cmd/edge.rs"
            "#,
        )
        .unwrap();
        // A same-named library-crate function must not be credited by the
        // capsule's own bare `edge_routes![crate::show]`.
        std::fs::write(dir.path().join("src/lib.rs"), "#[edge]\npub fn show() {}\n").unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );

        let unregistered = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "src/lib.rs");

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "cmd/edge.rs");
    }

    /// A custom `[[bin]] path` nested several segments under `src/bin/`
    /// itself (`src/bin/tools/capsule.rs`) is NEITHER of Cargo's own two
    /// auto-discovery shapes (`src/bin/<name>.rs`, `src/bin/<name>/main.rs`)
    /// — `crate_context_from_file`'s heuristic would otherwise treat `tools`
    /// as a phantom bin name and `capsule` as its submodule, so a bare
    /// `crate::`-qualified registration in the real capsule root would never
    /// match its own root-level handler (Codex review on #2739, round 22,
    /// P2).
    #[test]
    fn a_custom_bin_path_nested_under_src_bin_is_its_own_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin/tools")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [[bin]]
            name = "edge-capsule"
            path = "src/bin/tools/capsule.rs"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bin/tools/capsule.rs"),
            "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("src/bin/tools/capsule.rs")),
        );

        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "src/bin/tools/capsule.rs");
    }

    /// Even a CONVENTIONAL flat capsule root (`src/bin/edge-capsule.rs`,
    /// `autobins = false`) needs `scan_bin_crate_tree`'s treatment: its own
    /// out-of-line submodule (`mod routes;`, resolving to
    /// `src/bin/routes.rs`) is indistinguishable on disk from an unrelated
    /// second flat bin file, so the ordinary walk's `crate_context_from_file`
    /// heuristic credited it with a phantom `bin:routes` identity instead of
    /// treating it as `edge-capsule`'s own `routes` submodule (Codex review
    /// on #2739, round 22, P2).
    #[test]
    fn a_conventional_flat_capsule_roots_own_submodule_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nautobins = false\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bin/edge-capsule.rs"),
            "mod routes;\nfn main() { edge_routes![crate::routes::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bin/routes.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("src/bin/edge-capsule.rs")),
        );

        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "src/bin/routes.rs");
        assert_eq!(registered[0].module_path, vec!["routes".to_owned()]);
    }

    /// A custom `[[bin]] path` INSIDE `src/` but outside the conventional
    /// `src/bin/` (e.g. `path = "src/edge.rs"`) must also get its own crate
    /// root — the ordinary library `src/` walk would otherwise credit it to
    /// a fictitious library module named `edge`, and a bare
    /// `crate::`-qualified registration written there would then look like
    /// it lives in the wrong module entirely (Codex review on #2739, round
    /// 13, P2).
    #[test]
    fn a_custom_in_src_bin_path_is_its_own_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [[bin]]
            name = "edge-capsule"
            path = "src/edge.rs"
            "#,
        )
        .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "#[edge]\npub fn show() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/edge.rs"),
            "#[edge]\npub fn show() {}\nfn main() { edge_routes![crate::show]; }\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("src/edge.rs")),
        );

        let unregistered = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].file, "src/lib.rs");

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "src/edge.rs");
        assert_eq!(registered[0].module_path, Vec::<String>::new());
    }

    /// An out-of-tree custom bin's own `mod routes;` must be followed and
    /// scanned with the same bin crate identity — a valid registration
    /// written only in that submodule was previously invisible, since the
    /// capsule was scanned as a single file with no submodule traversal at
    /// all (Codex review on #2739, round 13, P2).
    #[test]
    fn a_custom_out_of_tree_bins_out_of_line_submodule_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [[bin]]
            name = "edge-capsule"
            path = "cmd/edge.rs"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            "mod routes;\nfn main() { edge_routes![routes::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/routes.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );

        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "cmd/routes.rs");
        assert_eq!(registered[0].module_path, vec!["routes".to_owned()]);
        assert_eq!(registered[0].crate_root, "bin:edge-capsule");
    }

    /// Same as above, but the out-of-line submodule is declared with a
    /// keyword-escaping raw identifier (`mod r#type;`) — `syn::Ident::to_string()`
    /// keeps the `r#` prefix verbatim, but Rust's own file-system
    /// module-resolution convention strips it (`mod r#type;` resolves to
    /// `type.rs`, never `r#type.rs`, verified directly against a real
    /// build), so the file lookup must normalize it away even though the
    /// registration path keeps it (Codex review on #2739, round 22, P2).
    #[test]
    fn a_custom_out_of_tree_bins_raw_identifier_submodule_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [[bin]]
            name = "edge-capsule"
            path = "cmd/edge.rs"
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            "mod r#type;\nfn main() { edge_routes![r#type::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/type.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );

        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].file, "cmd/type.rs");
    }

    /// `if let Foo { x } = value() { .. }` has a struct PATTERN of its own,
    /// with its own brace group, before the real body's brace — unlike an
    /// expression, Rust's grammar allows a bare struct pattern here, so the
    /// naive "first Brace ends the block" rule finds the pattern's brace
    /// instead of the body's. A cfg'd-out one previously credited
    /// `edge_routes![show]` INSIDE the still-excluded body as if it were a
    /// separate, live statement, registering a handler real Rust drops
    /// along with the rest of the `if let` (Codex review on #2739, round 38,
    /// P2).
    #[test]
    fn cfg_false_if_let_struct_pattern_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            struct Foo {
                x: i32,
            }

            fn value() -> Foo {
                Foo { x: 1 }
            }

            fn wire() {
                #[cfg(feature = "premium")]
                if let Foo { x } = value() {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// A `for Foo { x } in items { .. }` loop has the identical struct-
    /// pattern-brace ambiguity as the `if let` case above, in its own
    /// pattern rather than a `let`.
    #[test]
    fn cfg_false_for_loop_struct_pattern_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            struct Foo {
                x: i32,
            }

            fn items() -> Vec<Foo> {
                Vec::new()
            }

            fn wire() {
                #[cfg(feature = "premium")]
                for Foo { x } in items() {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// A cfg'd-out `extern "C" { ... }` foreign block (no leading `unsafe`)
    /// has no trailing `;` — verified directly via a real build — and after
    /// `skip_forward_over_item_modifiers` skips `extern`/the ABI literal,
    /// nothing distinguishes it from an ordinary bare Brace: the old code
    /// only recognized `struct`/`enum`/`union`/`trait` there, so the generic
    /// scan-to-`;` fallback ran through the excluded block and matched the
    /// FIRST semicolon inside it (or past it), consuming the still-real
    /// `edge_routes![show];` that followed along with it (Codex review on
    /// #2739, round 38, P2).
    #[test]
    fn cfg_false_extern_block_without_unsafe_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                extern "C" {
                    fn premium_only();
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// `unsafe extern "C" { ... }` already reaches its own brace correctly
    /// through the `"unsafe"` branch at the top of
    /// `statement_without_semicolon_end` (which hands off to
    /// `control_flow_block_end`'s opaque scan) — this locks that in as a
    /// regression test alongside the no-`unsafe` case above, which needed
    /// the fix.
    #[test]
    fn cfg_false_extern_block_with_unsafe_does_not_swallow_the_following_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                unsafe extern "C" {
                    fn premium_only();
                }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// An unparenthesized or-pattern over struct-LIKE enum-variant patterns
    /// (`E::A { x } | E::B { x }`, no wrapping tuple parens around the
    /// brace, unlike a tuple-variant payload) has MULTIPLE top-level
    /// struct-pattern braces, each followed by `|` except the last —
    /// verified directly via a real build. The round-38 fix only recognized
    /// `=`/`in` after a pattern's brace, so an or-pattern's first brace
    /// (followed by `|`) was still mistaken for the real body, crediting
    /// `edge_routes![show]` from inside the still-excluded `if let` body as
    /// a separate, live statement (Codex review on #2739, round 39, P2).
    #[test]
    fn cfg_false_if_let_or_pattern_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            enum E {
                A { x: i32 },
                B { x: i32 },
            }

            fn value() -> E {
                E::A { x: 1 }
            }

            fn wire() {
                #[cfg(feature = "premium")]
                if let E::A { x } | E::B { x } = value() {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// A cfg'd-out bodyless trait method with a return type (`fn disabled()
    /// -> ();`) is legal Rust — verified directly via a real build.
    /// `simple_fn_body_index`'s return-type-tolerant loop previously
    /// accepted the declaration's own `;` as ordinary punctuation and kept
    /// scanning past it, mistaking the NEXT method's body brace for
    /// `disabled`'s own — so that unrelated, still-active method's body
    /// (and its `edge_routes![show]` call) was treated as part of the
    /// excluded declaration and never visited (Codex review on #2739,
    /// round 39, P2).
    #[test]
    fn cfg_false_bodyless_trait_method_with_return_type_does_not_swallow_the_next_methods_body() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            trait Wiring {
                #[cfg(feature = "premium")]
                fn disabled() -> ();

                fn routes() {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// A `#[path]` override nested inside an out-of-line child of a custom
    /// capsule root — `cmd/edge.rs` has `mod wiring;`, and `cmd/wiring.rs`
    /// itself has `#[path = "actual.rs"] mod handlers;` — resolves relative
    /// to `wiring.rs`'s OWN directory (`cmd/`), never a subdirectory named
    /// after its full logical module path (`cmd/wiring/`) — verified
    /// directly via a real build. `scan_bin_crate_tree`'s BFS previously
    /// folded the full accumulated `module_path` (correct for the
    /// conventional, non-`#[path]` case, where it really does correspond to
    /// nested subdirectories) onto `root_dir` for the `#[path]` case too,
    /// so it searched `cmd/wiring/actual.rs` — a file that doesn't exist —
    /// and never found `show` at all, rather than merely getting its module
    /// path wrong (Codex review on #2739, round 39, P2).
    #[test]
    fn resolve_edge_scan_resolves_a_nested_path_attribute_in_a_custom_capsule_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"edge-capsule\"\npath = \"cmd/edge.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            "mod wiring;\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/wiring.rs"),
            "#[path = \"actual.rs\"]\nmod handlers;\nfn wire() { edge_routes![crate::wiring::handlers::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].file, "cmd/actual.rs");
        assert_eq!(
            scan.functions[0].module_path,
            vec!["wiring".to_owned(), "handlers".to_owned()]
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// `#[cfg_attr(feature = "alt", path = "actual.rs")] mod handlers;`
    /// expands to a real `#[path = "actual.rs"]` once `alt` is enabled,
    /// exactly like any other `cfg_attr`-gated attribute — verified
    /// directly via a real build. Before this, only a literal `#[path]`
    /// was recognized, so the scan fell back to the conventional
    /// `handlers.rs` (which doesn't exist here) instead of the real
    /// `actual.rs` (Codex review on #2739, round 40, P1).
    #[test]
    fn resolve_edge_scan_expands_a_path_attribute_introduced_by_cfg_attr() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[features]\nalt = []\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[cfg_attr(feature = "alt", path = "actual.rs")]
            mod handlers;
            fn wire() { edge_routes![crate::handlers::show]; }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(dir.path(), &["alt"], None);
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].file, "src/actual.rs");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// `#[path = "../routes.rs"] mod handlers;` in `src/lib.rs` compiles a
    /// file OUTSIDE `src/` entirely — verified directly via a real build.
    /// The ordinary walk only ever reads files found under `project_root/src`
    /// (`collect_rs_files`), so a target this far outside was recorded by
    /// `path_attribute_module_paths` but never actually read or scanned at
    /// all, and its sole `#[edge]` handler was invisible to the whole scan
    /// (Codex review on #2739, round 40, P1).
    #[test]
    fn resolve_edge_scan_scans_a_path_attributes_target_outside_src() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[path = "../routes.rs"]
            mod handlers;
            fn wire() { edge_routes![crate::handlers::show]; }
            "#,
        )
        .unwrap();
        std::fs::write(dir.path().join("routes.rs"), "#[edge]\npub fn show() {}\n").unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].file, "routes.rs");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// An external `#[path]` target can itself declare further out-of-line
    /// children (`pub mod child;` inside `outside/shared.rs`, resolving to
    /// the sibling `outside/child.rs`) — verified directly via a real build,
    /// where `handlers::child::show` compiles from exactly this layout. The
    /// external-path loop previously read and scanned only the target file
    /// itself with a flat `scan_source_with_context` call, never following
    /// its own further declarations, so a handler defined one level deeper
    /// than the `#[path]` target was invisible to the whole scan (Codex
    /// review on #2739, round 45, P1).
    #[test]
    fn resolve_edge_scan_follows_a_path_attributes_targets_own_child_module() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("outside")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[path = "../outside/shared.rs"]
            mod handlers;
            fn wire() { edge_routes![crate::handlers::child::show]; }
            "#,
        )
        .unwrap();
        std::fs::write(dir.path().join("outside/shared.rs"), "pub mod child;\n").unwrap();
        std::fs::write(
            dir.path().join("outside/child.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].file, "outside/child.rs");
        assert_eq!(
            scan.functions[0].module_path,
            vec!["handlers".to_string(), "child".to_string()]
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// The same file can be `#[path]`-included under two different module
    /// names — `#[path = "shared.rs"] mod a;` and `#[path = "shared.rs"]
    /// mod b;` both compile `shared.rs`, once per name — verified directly
    /// via a real build. `path_attribute_module_paths` previously kept only
    /// ONE identity per target file (a plain map overwrite), so the file
    /// was scanned as only `b`, and a registration naming the other
    /// (`edge_routes![a::show]`) never matched (Codex review on #2739,
    /// round 40, P2).
    #[test]
    fn resolve_edge_scan_preserves_every_identity_for_a_shared_path_attribute_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[path = "shared.rs"]
            mod a;
            #[path = "shared.rs"]
            mod b;
            fn wire() {
                edge_routes![crate::a::show];
                edge_routes![crate::b::show];
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/shared.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 2, "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let module_paths: BTreeSet<Vec<String>> = scan
            .functions
            .iter()
            .map(|f| f.module_path.clone())
            .collect();
        assert!(
            module_paths.contains(&vec!["a".to_owned()]),
            "{module_paths:?}"
        );
        assert!(
            module_paths.contains(&vec!["b".to_owned()]),
            "{module_paths:?}"
        );
    }

    /// A qualified brace-delimited macro invocation (`crate::configure! {
    /// ... }`, `self::configure! { ... }`) needs no trailing `;` either,
    /// exactly like the unqualified form — verified directly via a real
    /// build. The round-36 fix only recognized the bare `configure! { ...
    /// }` shape (requiring `!` immediately after the first identifier), so
    /// a cfg'd-out qualified invocation let the generic scan-to-`;`
    /// fallback run through it into the next, unrelated, still-real
    /// `edge_routes![show]` invocation and consume that too (Codex review
    /// on #2739, round 41, P2).
    #[test]
    fn cfg_false_qualified_brace_delimited_macro_invocation_does_not_swallow_the_following_registration()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            macro_rules! configure {
                ($($t:tt)*) => {};
            }

            fn wire() {
                #[cfg(feature = "premium")]
                crate::configure! { a b c }
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// A `#[cfg_attr]` NESTED inside another `#[cfg_attr]`'s own payload can
    /// still introduce a `path = "..."` override once BOTH conditions hold
    /// — verified directly via a real build. `path_attribute_value`'s
    /// round-40 fix only checked the immediate payload's metas, not a
    /// nested `cfg_attr` meta inside it, so this two-level form still fell
    /// back to the conventional (nonexistent) `handlers.rs` instead of the
    /// real `actual.rs` (Codex review on #2739, round 41, P1).
    #[test]
    fn resolve_edge_scan_expands_a_path_attribute_introduced_by_a_nested_cfg_attr() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[features]\na = []\nb = []\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            #[cfg_attr(feature = "a", cfg_attr(feature = "b", path = "actual.rs"))]
            mod handlers;
            fn wire() { edge_routes![crate::handlers::show]; }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/actual.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(dir.path(), &["a", "b"], None);
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].file, "src/actual.rs");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A cfg'd-out MATCH ARM (`#[cfg(feature = "premium")] true =>
    /// edge_routes![premium],`) is comma-delimited, not semicolon-delimited
    /// — verified directly via a real build. The generic scan-to-`;`
    /// fallback found no semicolon anywhere in the match's flattened arm
    /// tokens and consumed every remaining arm, including a later,
    /// unrelated, still-active one's own `edge_routes![show]` (Codex review
    /// on #2739, round 42, P2).
    #[test]
    fn cfg_false_match_arm_does_not_swallow_the_following_arms_registration() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire(x: bool) {
                match x {
                    #[cfg(feature = "premium")]
                    true => edge_routes![crate::premium_thing],
                    _ => edge_routes![crate::show],
                }
            }
            "#,
            &[],
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
    }

    /// The same file can be `#[path]`-included under two different module
    /// names inside a CUSTOM crate tree (a `[[bin]] path` capsule root, not
    /// just the ordinary `src/` walk) — verified directly via a real build.
    /// `scan_bin_crate_tree`'s own BFS `visited` set deduped purely by file
    /// path, so once `path_attribute_module_paths`'s round-40 fix let both
    /// aliases reach the queue, whichever was dequeued second was silently
    /// dropped — the file was scanned under only one of its two real
    /// identities (Codex review on #2739, round 42, P2).
    #[test]
    fn resolve_edge_scan_preserves_every_identity_for_a_shared_path_file_in_a_custom_crate_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"edge-capsule\"\npath = \"cmd/edge.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            r#"
            #[path = "shared.rs"]
            mod a;
            #[path = "shared.rs"]
            mod b;
            fn main() {
                edge_routes![crate::a::show];
                edge_routes![crate::b::show];
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/shared.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );
        assert_eq!(scan.functions.len(), 2, "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let module_paths: BTreeSet<Vec<String>> = scan
            .functions
            .iter()
            .map(|f| f.module_path.clone())
            .collect();
        assert!(
            module_paths.contains(&vec!["a".to_owned()]),
            "{module_paths:?}"
        );
        assert!(
            module_paths.contains(&vec!["b".to_owned()]),
            "{module_paths:?}"
        );
    }

    /// A shared `#[path]`-included file's OWN out-of-line submodule is
    /// re-traversed for EACH of the file's aliases, not just the first one
    /// to reach it — verified directly via a real build: `shared.rs`'s own
    /// `#[path = "child.rs"] mod child;` compiles as both `a::child` and
    /// `b::child` when `shared.rs` itself is reached as both `mod a;` and
    /// `mod b;`. Round 42's fix kept both of `shared.rs`'s own identities
    /// but still discovered its children only once (via a single, global
    /// per-file "children enqueued" set), so `child`'s module path came out
    /// right for whichever alias reached it first and simply missing for
    /// the other (Codex review on #2739, round 43, P2).
    #[test]
    fn resolve_edge_scan_traverses_a_shared_path_files_children_for_every_alias() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"edge-capsule\"\npath = \"cmd/edge.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            r#"
            #[path = "shared.rs"]
            mod a;
            #[path = "shared.rs"]
            mod b;
            fn main() {
                edge_routes![crate::a::child::show];
                edge_routes![crate::b::child::show];
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/shared.rs"),
            "#[path = \"child.rs\"]\npub mod child;\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/child.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );
        assert_eq!(scan.functions.len(), 2, "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let module_paths: BTreeSet<Vec<String>> = scan
            .functions
            .iter()
            .map(|f| f.module_path.clone())
            .collect();
        assert!(
            module_paths.contains(&vec!["a".to_owned(), "child".to_owned()]),
            "{module_paths:?}"
        );
        assert!(
            module_paths.contains(&vec!["b".to_owned(), "child".to_owned()]),
            "{module_paths:?}"
        );
    }

    /// A genuine `#[path]`-induced module cycle (`a.rs` -> `#[path =
    /// "b.rs"] mod b;` -> `b.rs` -> `#[path = "a.rs"] mod a;`, pointing
    /// back) never compiles at all in real Rust (rustc rejects it as
    /// "circular modules") — verified directly via a real build. This scan
    /// must still terminate on such input rather than loop forever, the
    /// same safety the old single global `visited`-by-file set gave before
    /// round 43 replaced it with per-branch ancestor tracking to fix the
    /// shared-alias case above.
    #[test]
    fn resolve_edge_scan_terminates_on_a_circular_path_attribute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"edge-capsule\"\npath = \"cmd/edge.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            "#[path = \"a.rs\"]\nmod a;\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("cmd/a.rs"), "#[path = \"b.rs\"]\nmod b;\n").unwrap();
        std::fs::write(dir.path().join("cmd/b.rs"), "#[path = \"a.rs\"]\nmod a;\n").unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );
        assert!(scan.functions.is_empty(), "{:?}", scan.functions);
    }

    /// A `#[path]`-overridden file's own CONVENTIONAL (non-`#[path]`) child
    /// resolves relative to the overridden file's OWN physical directory,
    /// never the logical module path used to reach it — verified directly
    /// via a real build: `#[path = "shared.rs"] mod handlers;` where
    /// `shared.rs` has a plain `mod child;` resolves `child` at
    /// `child.rs`, sitting right beside `shared.rs`, not at
    /// `handlers/child.rs`. Reconstructing the resolution base from
    /// `root_dir` plus the full accumulated logical `module_path` (this
    /// function's previous approach) only stayed correct up to the first
    /// `#[path]` in the chain (Codex review on #2739, round 43, P1).
    #[test]
    fn resolve_edge_scan_resolves_a_conventional_child_of_a_path_overridden_module() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cmd")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n\
             [[bin]]\nname = \"edge-capsule\"\npath = \"cmd/edge.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cmd/edge.rs"),
            r#"
            #[path = "shared.rs"]
            mod handlers;
            fn main() {
                edge_routes![crate::handlers::child::show];
            }
            "#,
        )
        .unwrap();
        std::fs::write(dir.path().join("cmd/shared.rs"), "mod child;\n").unwrap();
        std::fs::write(
            dir.path().join("cmd/child.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("cmd/edge.rs")),
        );
        assert_eq!(scan.functions.len(), 1, "{:?}", scan.functions);
        assert_eq!(scan.functions[0].file, "cmd/child.rs");
        assert_eq!(
            scan.functions[0].module_path,
            vec!["handlers".to_owned(), "child".to_owned()]
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A brace-delimited macro invocation used as a match SCRUTINEE
    /// (`match value!{} { .. }`) has its OWN brace group — the macro's
    /// argument list — directly preceded by `!`, immediately before the
    /// real match body's own brace. Verified directly via a real build:
    /// the whole cfg'd-out match (scrutinee and all) drops its body along
    /// with it. `control_flow_block_end` previously took the FIRST brace
    /// group reached as the body unconditionally, so it mistook the
    /// macro's own (here, empty) argument group for the body and credited
    /// `edge_routes![show]` from the REAL body as a separate, live
    /// statement (Codex review on #2739, round 44, P2).
    #[test]
    fn cfg_false_match_with_macro_scrutinee_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            macro_rules! value {
                () => { 1 };
            }

            fn wire() {
                #[cfg(feature = "premium")]
                match value!{} {
                    _ => edge_routes![crate::show],
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// The same physical file can be reached BOTH by the ordinary library
    /// `src/` walk (`mod handlers;` in `src/lib.rs`) AND, independently, by
    /// a capsule's own `[[bin]] path` tree via a `#[path]`-aliased
    /// declaration (`#[path = "../handlers.rs"] mod local_handlers;` in
    /// `src/bin/edge-capsule.rs`) — real Rust compiles it twice, once per
    /// identity, verified directly via a real build. `resolve_edge_scan_impl`
    /// filtered the ordinary `src/` walk's file list against `claimed` (the
    /// SET OF PHYSICAL PATHS `scan_bin_crate_tree` touched), dropping the
    /// file from the library scan entirely once the capsule tree touched it
    /// under its own identity — losing the library's own separate
    /// registration match (Codex review on #2739, round 44, P2).
    #[test]
    fn resolve_edge_scan_keeps_the_librarys_own_identity_for_a_file_the_capsule_also_touches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "mod handlers;\nfn wire() { edge_routes![my_app::handlers::show]; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/handlers.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bin/edge-capsule.rs"),
            r#"
            #[path = "../handlers.rs"]
            mod local_handlers;
            fn main() { edge_routes![crate::local_handlers::show]; }
            "#,
        )
        .unwrap();

        let scan = resolve_edge_scan_with_extra_file(
            dir.path(),
            &[],
            Some(&dir.path().join("src/bin/edge-capsule.rs")),
        );
        assert_eq!(scan.functions.len(), 2, "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let identities: BTreeSet<(String, Vec<String>)> = scan
            .functions
            .iter()
            .map(|f| (f.crate_root.clone(), f.module_path.clone()))
            .collect();
        assert!(
            identities.contains(&(String::new(), vec!["handlers".to_owned()])),
            "{identities:?}"
        );
        assert!(
            identities.contains(&(
                "bin:edge-capsule".to_owned(),
                vec!["local_handlers".to_owned()]
            )),
            "{identities:?}"
        );
    }

    /// An ungrouped generic-argument comma (`Result<(), ()>`) is ALSO a
    /// top-level, ungrouped comma at this token-level scan's flat depth —
    /// verified directly via a real build: the whole cfg'd-out `let _:
    /// Result<(), ()> = { edge_routes![show]; Ok(()) };` drops the
    /// registration inside its own initializer block along with it. The
    /// round-42 comma-as-terminator fix (for match arms) didn't track
    /// angle-bracket depth, so it stopped mid-type-argument-list instead of
    /// at the statement's real end, recursing into the still-excluded
    /// initializer block and crediting `show` (Codex review on #2739,
    /// round 44, P2).
    #[test]
    fn cfg_false_let_with_generic_type_argument_comma_does_not_leak_its_initializer() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                let _: Result<(), ()> = {
                    edge_routes![crate::show];
                    Ok(())
                };
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// An ordinary comparison operator with no matching partner (`1 < 2`)
    /// leaves the fallback's `angle_depth` counter elevated forever, since
    /// nothing in an ordinary boolean expression ever supplies the closing
    /// `>` half — verified directly via a real build: `#[cfg(false)] let
    /// disabled = 1 < 2;` compiles fine, dropping the whole statement, and a
    /// following ACTIVE statement compiles as its own, separate statement.
    /// The round-44 angle-depth fix made the fallback's `;` case share the
    /// same `angle_depth == 0` guard as its `,` case, so a `<` with no
    /// matching `>` left the scan unable to ever recognize a genuine
    /// terminating `;` again, running straight through the excluded
    /// statement's own semicolon into a later, unrelated, still-active
    /// `edge_routes![show];` and crediting it as part of the excluded
    /// statement (Codex review on #2739, round 45, P2).
    #[test]
    fn cfg_false_let_with_unmatched_comparison_operator_does_not_swallow_the_next_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                let disabled = 1 < 2;
                edge_routes![crate::show];
            }
            "#,
            &[],
        );
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// A cfg'd-out match arm ending in a bare comparison (`0 => 1 < 2,`) —
    /// real, valid Rust verified directly via a real build — has an
    /// unmatched `<` with no closing `>` anywhere ahead, the same shape that
    /// broke semicolon-termination in round 45. Unlike a semicolon, a comma
    /// still needs real angle-bracket depth tracking (a comma legitimately
    /// occurs inside an open turbofish/generic argument list), so simply
    /// making commas ignore depth entirely — round 45's semicolon fix —
    /// would silently break `Result<(), ()>`. `matched_angle_bracket_positions`
    /// instead confirms `<`/`>` belong to a real matched pair before they
    /// affect depth, so a stray, never-closed `<` no longer raises it
    /// forever and swallows the following, still-active arm's own
    /// registration (Codex review on #2739, round 46, P2).
    #[test]
    fn cfg_false_match_arm_with_unmatched_comparison_operator_does_not_swallow_the_next_arm() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire(x: i32) {
                match x {
                    #[cfg(feature = "premium")]
                    0 => 1 < 2,
                    _ => edge_routes![crate::show],
                };
            }
            "#,
            &[],
        );
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// The round-46 fix scoped its bracket-matching to skip a match arm's
    /// own `=>` as an atomic unit, which only ever fixed a stray `<`
    /// spuriously pairing with the VERY NEXT arm's own fat arrow. Fresh
    /// evidence beyond that is a stray `<` pairing with an entirely
    /// unrelated real comparison several tokens further on, past the next
    /// arm's own `=>` — `#[cfg(false)] 0 => 1 < 2, _ =>
    /// edge_routes![show].len() > 0,` — verified directly via a real build
    /// that this compiles with the second arm remaining real, active code.
    /// A `=>` now clears every still-open stack entry outright (not just
    /// skips past it), since a bare `=>` can never legitimately occur
    /// inside a real generic argument list, bounding the match to at most
    /// one arm's own tokens (Codex review on #2739, round 47, P2).
    #[test]
    fn cfg_false_match_arm_with_unmatched_comparison_operator_does_not_pair_across_the_next_arm() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() -> Vec<i32> { vec![] }

            fn wire(x: i32) {
                match x {
                    #[cfg(feature = "premium")]
                    0 => 1 < 2,
                    _ => edge_routes![crate::show].len() > 0,
                };
            }
            "#,
            &[],
        );
        assert_eq!(
            scan.registered_fns().len(),
            1,
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.registered_fns()[0].name, "show");
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
    }

    /// An async block used in the condition or scrutinee (`if async {
    /// predicate().await }.await { .. }`) has its OWN brace group
    /// immediately preceded by the `async` keyword itself — verified
    /// directly via a real build. The round-44 fix for a macro's own brace
    /// (preceded by `!`) didn't cover this shape, so a cfg'd-out `if` with
    /// an async-block condition let the real body's own `edge_routes![show]`
    /// get credited as a separate, live statement (Codex review on #2739,
    /// round 45, P2).
    #[test]
    fn cfg_false_if_with_async_block_condition_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            async fn predicate() -> bool { true }

            async fn wire() {
                #[cfg(feature = "premium")]
                if async { predicate().await }.await {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// `async move { .. }` (a moving async block) used in the condition or
    /// scrutinee (`if async move { true }.await { .. }`, real, valid Rust —
    /// verified directly via a real build) inserts `move` between the
    /// keyword and the brace, so the brace is preceded by `move`, not
    /// `async` directly. The round-45 async-block fix only checked for
    /// `async` immediately preceding the brace, so it missed this shape and
    /// mistook the condition block for the real body, leaving the actual
    /// body an ordinary, unexcluded sibling group that credited `show`
    /// (Codex review on #2739, round 46, P2).
    #[test]
    fn cfg_false_if_with_async_move_block_condition_does_not_leak_its_body_as_a_separate_statement()
    {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            async fn wire() {
                #[cfg(feature = "premium")]
                if async move { true }.await {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// A bare block expression (`if { true } { .. }`) can stand as the
    /// WHOLE `if` condition itself, with nothing else preceding it — real,
    /// valid Rust verified directly via a real build (warned as
    /// "unnecessary braces", not rejected), since Rust's "no struct literal
    /// in condition" restriction forbids a `Path { .. }` literal, not a bare,
    /// path-less block. `control_flow_block_end` previously treated the
    /// FIRST Brace group reached as unambiguously the real body, so it
    /// mistook this condition block for the body and left the actual body
    /// an ordinary, unexcluded sibling group that credited `show` even
    /// though the whole `if` sits behind an inactive `#[cfg(feature =
    /// "premium")]` (Codex review on #2739, round 45, P2).
    #[test]
    fn cfg_false_if_with_bare_block_condition_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                if { true } {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// An inline `const { .. }` block used as the WHOLE `if` condition
    /// (`if const { true } { .. }`) is real, valid Rust verified directly
    /// via a real build — its own brace group is immediately preceded by
    /// the `const` keyword, the identical shape as the `async`/`async move`
    /// cases. `control_flow_block_end` previously had no exception for
    /// `const`, so it mistook this condition block for the real body,
    /// leaving the actual body an ordinary, unexcluded sibling group that
    /// credited `show` (Codex review on #2739, round 46, P2).
    #[test]
    fn cfg_false_if_with_inline_const_block_condition_does_not_leak_its_body_as_a_separate_statement()
     {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                if const { true } {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// An `unsafe { .. }` block used as the WHOLE `if` condition (`if
    /// unsafe { true } { .. }`) is real, valid Rust verified directly via a
    /// real build (warned as "unnecessary `unsafe` block", not rejected) —
    /// the identical shape as the `async`/`const` cases, except `unsafe` is
    /// ALSO one of the six keywords `control_flow_block_end` is itself
    /// dispatched for (a top-level `unsafe { .. }` statement has no
    /// condition at all — its body brace comes immediately after the
    /// keyword). `control_flow_block_end` previously had no exception for a
    /// NESTED `unsafe` block condition, so it mistook this condition block
    /// for the real body, leaving the actual body an ordinary, unexcluded
    /// sibling group that credited `show` (Codex review on #2739, round 47,
    /// P2).
    #[test]
    fn cfg_false_if_with_unsafe_block_condition_does_not_leak_its_body_as_a_separate_statement() {
        let scan = scan_one_with_features(
            r#"
            #[edge]
            pub fn show() {}

            fn wire() {
                #[cfg(feature = "premium")]
                if unsafe { true } {
                    edge_routes![crate::show];
                }
            }
            "#,
            &[],
        );
        assert!(
            scan.registered_fns().is_empty(),
            "{:?}",
            scan.registered_fns()
        );
        assert_eq!(scan.unregistered().len(), 1, "{:?}", scan.unregistered());
        assert_eq!(scan.unregistered()[0].name, "show");
    }

    /// A file reached both conventionally (`mod handlers;`) AND via a
    /// `#[path]` alias (`#[path = "handlers.rs"] mod alias;`) is compiled
    /// under BOTH names by real Rust — verified directly via a real build.
    /// `path_attribute_module_paths` previously recorded only the alias
    /// identity for such a file, so the conventional registration
    /// (`edge_routes![crate::handlers::show]`) never matched (Codex review
    /// on #2739, round 45, P2).
    #[test]
    fn resolve_edge_scan_keeps_the_conventional_identity_beside_a_path_alias() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
            mod handlers;
            #[path = "handlers.rs"]
            mod alias;
            fn wire() {
                edge_routes![crate::handlers::show];
                edge_routes![crate::alias::show];
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/handlers.rs"),
            "#[edge]\npub fn show() {}\n",
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.functions.len(), 2, "{:?}", scan.functions);
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        let module_paths: BTreeSet<Vec<String>> = scan
            .functions
            .iter()
            .map(|f| f.module_path.clone())
            .collect();
        assert!(
            module_paths.contains(&vec!["handlers".to_owned()]),
            "{module_paths:?}"
        );
        assert!(
            module_paths.contains(&vec!["alias".to_owned()]),
            "{module_paths:?}"
        );
    }
}
