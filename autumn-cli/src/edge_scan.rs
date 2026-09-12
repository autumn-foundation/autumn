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
//!   `handlers::greet`. A leading `self` or `super` in an entry is resolved
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
//!   --features x` — see [`resolve_edge_scan_with_features`]) through the
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

use std::collections::BTreeSet;
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
            Some(seg) if crate_name.is_some_and(|name| name == seg) => {
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
        qualifier_segments.eq(f.module_path.iter().map(String::as_str))
    })
}

/// Scan a set of in-memory `(file, source)` pairs, given the set of feature
/// names enabled for this build — see [`enabled_features_from_manifest`] —
/// used to evaluate `#[cfg(...)]` on `#[edge]`-marked functions. The pure
/// core of the scan: [`resolve_edge_scan_with_features`] is the thin
/// filesystem wrapper around it.
fn scan_sources_with_features(
    sources: &[(&str, &str)],
    default_features: &BTreeSet<String>,
) -> EdgeScan {
    let mut scan = EdgeScan::default();
    for (file, src) in sources {
        scan_source(file, src, default_features, &mut scan);
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
/// goes through [`resolve_edge_scan_with_features`], which needs the real
/// feature set.
#[cfg(test)]
#[must_use]
pub fn scan_sources(sources: &[(&str, &str)]) -> EdgeScan {
    scan_sources_with_features(sources, &BTreeSet::new())
}

/// [`resolve_edge_scan_with_features`] with no explicitly-requested
/// features. Every real caller now passes its own requested-features list
/// (`build.rs`'s `--features x`, `doctor.rs`'s always-empty `&[]`), so this
/// convenience wrapper is test-only — like [`scan_sources`], nothing outside
/// a test build calls it.
#[cfg(test)]
#[must_use]
fn resolve_edge_scan(project_root: &Path) -> EdgeScan {
    resolve_edge_scan_with_features(project_root, &[])
}

/// Walk `project_root/src` and scan every `.rs` file below it. Paths are
/// recorded relative to `project_root` (`src/routes/home.rs`). A missing `src/`
/// yields an empty scan — a project without sources simply has no edge routes.
///
/// `requested_features` are feature names asked for on the command line
/// (`autumn build --features x`, forwarded here from `build.rs`), on top of
/// the manifest's own `[features] default = [...]`. Without them, a route
/// gated on a non-default feature the caller explicitly requested would look
/// cfg'd-out to this scan even though the real build the caller is about to
/// run turns it on — exactly the dangerous direction (a route that will
/// really be served, silently missing from the scan) this module's `#[cfg]`
/// evaluation is designed to never risk.
#[must_use]
pub fn resolve_edge_scan_with_features(
    project_root: &Path,
    requested_features: &[&str],
) -> EdgeScan {
    // A missing or unparseable Cargo.toml yields no default features, which is
    // the safe direction: every `#[cfg(feature = "...")]` then stays
    // unresolved, and the function it gates stays in the scan.
    let manifest = std::fs::read_to_string(project_root.join("Cargo.toml")).ok();
    let default_features = manifest
        .as_deref()
        .map(|manifest| enabled_features_from_manifest(manifest, requested_features))
        .unwrap_or_default();
    let crate_name = manifest
        .as_deref()
        .and_then(|manifest| toml::from_str::<toml::Table>(manifest).ok())
        .and_then(|table| rust_crate_name_from_manifest(&table));

    let mut files = Vec::new();
    collect_rs_files(&project_root.join("src"), &mut files);
    // Sorted so warnings, doctor details, and the build's route list are stable
    // across platforms and filesystem orderings.
    files.sort();

    // Read first, scan second, so the filesystem half and the pure half stay
    // separable: `scan_sources` is the same entry point the unit tests drive
    // with inline sources. An unreadable file is skipped, like the sibling
    // scanners do.
    let sources: Vec<(String, String)> = files
        .iter()
        .filter_map(|path| {
            let src = std::fs::read_to_string(path).ok()?;
            let rel = path
                .strip_prefix(project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            Some((rel, src))
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = sources
        .iter()
        .map(|(file, src)| (file.as_str(), src.as_str()))
        .collect();
    let mut scan = scan_sources_with_features(&borrowed, &default_features);
    scan.crate_name = crate_name;
    scan
}

/// [`resolve_edge_scan_with_features`], plus `extra_file` — the edge-capsule
/// bin's own resolved root file, scanned as its own `[[bin]]` crate tree
/// (root file plus every out-of-line submodule it transitively declares)
/// rather than as part of the library.
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
/// no-op when `None` or already under the conventional `src/bin/` (the
/// `src/` walk already gives that the right identity).
#[must_use]
pub fn resolve_edge_scan_with_extra_file(
    project_root: &Path,
    requested_features: &[&str],
    extra_file: Option<&Path>,
) -> EdgeScan {
    let Some(file) = extra_file else {
        return resolve_edge_scan_with_features(project_root, requested_features);
    };
    if file.starts_with(project_root.join("src").join("bin")) {
        return resolve_edge_scan_with_features(project_root, requested_features);
    }

    let manifest = std::fs::read_to_string(project_root.join("Cargo.toml")).ok();
    let default_features = manifest
        .as_deref()
        .map(|manifest| enabled_features_from_manifest(manifest, requested_features))
        .unwrap_or_default();
    let crate_name = manifest
        .as_deref()
        .and_then(|manifest| toml::from_str::<toml::Table>(manifest).ok())
        .and_then(|table| rust_crate_name_from_manifest(&table));

    let mut scan = EdgeScan {
        crate_name,
        ..EdgeScan::default()
    };

    // Scan the capsule's own tree FIRST, so its file set is known before the
    // library walk below — excluding those files there is what stops a root
    // file living inside `src/` from being double-scanned under two
    // different (and for the library one, wrong) crate identities.
    let capsule_files = scan_bin_crate_tree(
        file,
        project_root,
        "bin:edge-capsule",
        &default_features,
        &mut scan,
    );

    let mut files = Vec::new();
    collect_rs_files(&project_root.join("src"), &mut files);
    files.sort();

    let sources: Vec<(String, String)> = files
        .iter()
        .filter(|path| !capsule_files.contains(path.as_path()))
        .filter_map(|path| {
            let src = std::fs::read_to_string(path).ok()?;
            let rel = path
                .strip_prefix(project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            Some((rel, src))
        })
        .collect();
    for (rel, src) in &sources {
        scan_source(rel, src, &default_features, &mut scan);
    }
    scan.files_scanned += sources.len();

    scan
}

/// The scanned crate's own `[package] name`, when the manifest table parses
/// far enough to say. Used only by [`enabled_features_from_manifest`], which
/// matches Cargo's own `-p <package>` / `--features pkg/feat` CLI syntax —
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
/// value)` pairs — checking *membership*, since a key like `target_feature`
/// legitimately has several simultaneous real values — combined with
/// `not`/`all`/`any`, is resolved; anything else (an OS-family shorthand
/// like `windows`/`unix`, a key this table does not list, or a predicate
/// this scan cannot parse) is treated as NOT applying. That is the opposite
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

/// wasm32-wasip1's own value(s) for every `cfg(...)` key this scan resolves,
/// verified directly against the complete
/// `rustc --print cfg --target wasm32-wasip1` output (Codex review on
/// #2739, rounds 14, 15 and 16, each extending the previous round's grammar
/// after a real predicate it did not recognize was found to evaluate as
/// "does not match" when it actually does — the exact dangerous-direction
/// mistake this evaluator exists to avoid: a route Cargo really compiles for
/// the capsule was scanned out). A `cfg(...)` key can list MULTIPLE
/// simultaneous values for one target — `target_feature` and
/// `target_has_atomic` both do here — and a real predicate checks
/// *membership* in that set: `cfg(target_has_atomic = "32")` is satisfied
/// because "32" is one of five values this target reports for that key, not
/// because it is the only one. Round 15 treated a multi-valued key as
/// entirely unresolvable instead of checking membership, which is why this
/// list holds one entry per `(key, value)` PAIR, not one entry per key — a
/// key with several real values simply appears several times.
const WASM32_WASIP1_CFG_VALUES: &[(&str, &str)] = &[
    ("target_arch", "wasm32"),
    ("target_os", "wasi"),
    ("target_family", "wasm"),
    ("target_env", "p1"),
    ("target_pointer_width", "32"),
    ("target_vendor", "unknown"),
    ("target_endian", "little"),
    ("target_abi", ""),
    ("target_feature", "bulk-memory"),
    ("target_feature", "crt-static"),
    ("target_feature", "multivalue"),
    ("target_feature", "mutable-globals"),
    ("target_feature", "nontrapping-fptoint"),
    ("target_feature", "reference-types"),
    ("target_feature", "sign-ext"),
    ("target_has_atomic", "8"),
    ("target_has_atomic", "16"),
    ("target_has_atomic", "32"),
    ("target_has_atomic", "64"),
    ("target_has_atomic", "ptr"),
];

impl syn::parse::Parse for TargetCfgPredicate {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        let key_is_known = WASM32_WASIP1_CFG_VALUES.iter().any(|(key, _)| ident == key);
        if key_is_known {
            input.parse::<syn::Token![=]>()?;
            let lit: syn::LitStr = input.parse()?;
            let value = lit.value();
            let matches = WASM32_WASIP1_CFG_VALUES
                .iter()
                .any(|(key, known_value)| ident == key && value == *known_value);
            return Ok(Self::Leaf(matches));
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
        // `windows`, `unix`, `target_env`, `target_pointer_width`, ... — not
        // part of the resolvable grammar; the caller treats this as "does
        // not match."
        Err(input.error("target cfg predicate not resolvable by this scan"))
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

/// Whether `<name>` is declared `optional = true` in `[dependencies]` or in
/// a `[target.'cfg(...)'.dependencies]` table whose target actually applies
/// to the edge capsule's own build (see
/// [`target_key_matches_edge_capsule`]) — dev/build dependency tables are
/// still not consulted, matching this scanner's other best-effort limits. A
/// version-string dependency (`name = "1"`) is never optional; only the
/// expanded table form can set the flag.
#[must_use]
fn is_optional_dependency(table: &toml::Table, name: &str) -> bool {
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
                .filter(|(target_key, _)| target_key_matches_edge_capsule(target_key))
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

/// The scanned crate's own Rust library-crate identifier — what
/// `edge_routes![this_name::item]` would actually have to spell to reach one
/// of its items, same as `crate::item`. This is not always the bare
/// `[package] name`: `[lib] name = "..."` can rename the library target
/// outright, and even without that override Cargo turns every `-` in the
/// package name into `_` for the crate identifier (a package named
/// `my-app` compiles to `extern crate my_app`, never `my-app` — that is not
/// a legal Rust identifier). [`resolve_edge_scan_with_features`] uses this
/// for [`EdgeScan::crate_name`]; [`enabled_features_from_manifest`] does not
/// — see [`package_name_from_manifest`].
#[must_use]
fn rust_crate_name_from_manifest(table: &toml::Table) -> Option<String> {
    table
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .or_else(|| package_name_from_manifest(table).map(|name| name.replace('-', "_")))
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
#[must_use]
fn enabled_features_from_manifest(manifest: &str, requested: &[&str]) -> BTreeSet<String> {
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
            // such promise either way. That implicit feature is itself
            // suppressed crate-wide once any feature entry spells `dep:pkg`
            // — see `implicit_feature_is_suppressed` (round 13, P2).
            if !pkg.ends_with('?')
                && table
                    .as_ref()
                    .is_some_and(|t| is_optional_dependency(t, pkg))
                && !implicit_feature_is_suppressed(features_table.as_ref(), pkg)
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
fn scan_source(file: &str, src: &str, default_features: &BTreeSet<String>, scan: &mut EdgeScan) {
    let (crate_root, module_path) = crate_context_from_file(file);
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
/// A non-standard layout (`#[path = "..."]`, e.g.) can make this wrong, like
/// every other heuristic in this best-effort scanner — see the module doc's
/// "Recognition limits". A wrong prefix can only ever produce an extra
/// false-positive "unregistered" warning, never a missed one: this scanner's
/// documented safe direction.
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
fn crate_context_from_file(file: &str) -> (String, Vec<String>) {
    let without_ext = file.strip_suffix(".rs").unwrap_or(file);
    let Some(without_src) = without_ext.strip_prefix("src/") else {
        return (format!("bin:{without_ext}"), Vec::new());
    };
    if without_src.is_empty() {
        return (String::new(), Vec::new());
    }
    if let Some(rest) = without_src.strip_prefix("bin/") {
        // `<name>` alone (the flat `src/bin/<name>.rs` form) has nothing left
        // once the bin target's own name is dropped — it IS that crate's root.
        let (name, bin_relative) = rest.split_once('/').unwrap_or((rest, ""));
        return (
            format!("bin:{name}"),
            module_path_from_segments(bin_relative),
        );
    }
    if without_src == "main" {
        // The package's own implicit default binary, `src/main.rs` — a
        // separate crate from the library's `src/lib.rs`, even though both
        // map to the same empty module path. No real `[[bin]]` can be named
        // literally "main" while a default `src/main.rs` binary also
        // exists (Cargo rejects the name collision), so reusing the
        // `"bin:<name>"` convention here is always unambiguous.
        return ("bin:main".to_owned(), Vec::new());
    }
    (String::new(), module_path_from_segments(without_src))
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
                    // `--embed` (Codex review on #2739, round 11, P2).
                    let cfg_excludes = item_mod
                        .attrs
                        .iter()
                        .any(|attr| eval_cfg_attr(attr, default_features) == Some(false));
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
/// name)` — `enclosing_path` is the chain of *inline* module names between
/// `items`' own file and the declaration, empty for a top-level one.
///
/// [`scan_bin_crate_tree`] uses this to follow a custom `[[bin]] path]`'s own
/// module graph: unlike the library's `src/` walk (which reaches every file
/// under `src/` regardless of whether any `mod` declaration actually
/// references it), a bin target outside the conventional layout has no such
/// directory convention to lean on, so its submodules are invisible unless
/// this scan actually follows its `mod` declarations (Codex review on #2739,
/// round 13, P2).
fn out_of_line_mod_declarations(
    items: &[syn::Item],
    default_features: &BTreeSet<String>,
) -> Vec<(Vec<String>, String)> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    collect_out_of_line_mods(items, default_features, &mut path, &mut out);
    out
}

fn collect_out_of_line_mods(
    items: &[syn::Item],
    default_features: &BTreeSet<String>,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, String)>,
) {
    for item in items {
        let syn::Item::Mod(item_mod) = item else {
            continue;
        };
        let cfg_excludes = item_mod
            .attrs
            .iter()
            .any(|attr| eval_cfg_attr(attr, default_features) == Some(false));
        if cfg_excludes {
            continue;
        }
        match &item_mod.content {
            Some((_, inner)) => {
                path.push(item_mod.ident.to_string());
                collect_out_of_line_mods(inner, default_features, path, out);
                path.pop();
            }
            None => out.push((path.clone(), item_mod.ident.to_string())),
        }
    }
}

/// Resolve an out-of-line `mod name;` to its file, following Rust's own rule:
/// relative to `root_dir` (the crate root's own directory) plus
/// `dir_segments` (the accumulated module path so far), the file is either
/// `<dir>/name.rs` or, failing that, `<dir>/name/mod.rs`. `None` when neither
/// exists — a non-standard `#[path = "..."]` override, like every other
/// heuristic in this best-effort scanner, is not resolved (see the module
/// doc's "Recognition limits").
fn resolve_out_of_line_module_file(
    root_dir: &Path,
    dir_segments: &[String],
    name: &str,
) -> Option<PathBuf> {
    let dir = dir_segments
        .iter()
        .fold(root_dir.to_path_buf(), |dir, segment| dir.join(segment));
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
/// A conventional `src/bin/<name>.rs` / `src/bin/<name>/main.rs` capsule
/// never reaches this function — [`resolve_edge_scan_with_extra_file`] only
/// calls it for `extra_file`, which is `None` for the conventional layout
/// (the library walk already reaches it there, with the right identity via
/// [`crate_context_from_file`]'s own `src/bin/` handling).
fn scan_bin_crate_tree(
    root_file: &Path,
    project_root: &Path,
    crate_root: &str,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) -> BTreeSet<PathBuf> {
    let root_dir = root_file.parent().unwrap_or_else(|| Path::new(""));
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<(PathBuf, Vec<String>)> =
        std::collections::VecDeque::new();
    queue.push_back((root_file.to_path_buf(), Vec::new()));

    while let Some((file, module_path)) = queue.pop_front() {
        if !visited.insert(file.clone()) {
            continue; // Already scanned — a module cycle can't loop forever.
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
        for (nested, name) in out_of_line_mod_declarations(&ast.items, default_features) {
            let mut dir_segments = module_path.clone();
            dir_segments.extend(nested);
            let Some(resolved) = resolve_out_of_line_module_file(root_dir, &dir_segments, &name)
            else {
                continue;
            };
            let mut child_module_path = dir_segments;
            child_module_path.push(name);
            queue.push_back((resolved, child_module_path));
        }
    }

    visited
}

/// The last `::` segment of an attribute path, e.g. `edge` for
/// `#[autumn_web::edge]`. Attribute paths with generic arguments are not
/// attribute macros, so a plain segment match is enough.
fn attr_name(attr: &syn::Attribute) -> Option<String> {
    attr.path().segments.last().map(|s| s.ident.to_string())
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
    let names: Vec<String> = attrs.iter().filter_map(attr_name).collect();
    if !names.iter().any(|n| n == "edge") {
        return None;
    }
    // Several `#[cfg(...)]` attributes on one function are ANDed, like real
    // Rust: any one of them resolving to definitely false excludes it.
    let cfg_excludes = attrs
        .iter()
        .any(|attr| eval_cfg_attr(attr, default_features) == Some(false));
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
/// `feature = "x"` leaves, combined with `not`, `all`, and `any`.
enum CfgPredicate {
    Feature(String),
    Not(Box<Self>),
    All(Vec<Self>),
    Any(Vec<Self>),
}

impl syn::parse::Parse for CfgPredicate {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        if ident == "feature" {
            input.parse::<syn::Token![=]>()?;
            let lit: syn::LitStr = input.parse()?;
            return Ok(Self::Feature(lit.value()));
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
        if let TokenTree::Ident(ident) = &trees[index]
            && ident == "mod"
            && let Some(TokenTree::Ident(name)) = trees.get(index + 1)
            && let Some(TokenTree::Group(group)) = trees.get(index + 2)
            && group.delimiter() == Delimiter::Brace
        {
            if !preceding_cfg_excludes(&trees, index, default_features) {
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
            if let TokenTree::Group(body) = &trees[body_index]
                && !preceding_cfg_excludes(&trees, index, default_features)
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
    if !matches!(trees.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
    {
        return None;
    }
    i += 1;
    if matches!(trees.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
        return Some(i);
    }
    if matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '-')
        && matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '>')
    {
        i += 2;
        loop {
            match trees.get(i) {
                Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => return Some(i),
                Some(TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_)) => i += 1,
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
/// excluded) as [`eval_cfg_attr`].
fn cfg_attribute_group_is_false(
    group: &proc_macro2::Group,
    default_features: &BTreeSet<String>,
) -> bool {
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let (Some(TokenTree::Ident(ident)), Some(TokenTree::Group(cfg_group))) =
        (inner.first(), inner.get(1))
    else {
        return false;
    };
    if ident != "cfg" || cfg_group.delimiter() != Delimiter::Parenthesis {
        return false;
    }
    syn::parse2::<CfgPredicate>(cfg_group.stream()).is_ok_and(|pred| !pred.eval(default_features))
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
    /// `target_has_atomic` and `target_feature` each legitimately list
    /// SEVERAL simultaneous values for that target, and a real predicate
    /// checks membership — `target_has_atomic = "32"` is satisfied by one of
    /// five reported values, not because it is the target's only value
    /// (Codex review on #2739, round 16, P1).
    #[test]
    fn multi_valued_wasm32_wasip1_target_cfg_keys_also_enable_the_dependency() {
        for cfg in [
            r#"target_has_atomic = "32""#,
            r#"target_feature = "bulk-memory""#,
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
    fn resolve_edge_scan_with_features_honors_explicitly_requested_features() {
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
        let scan = resolve_edge_scan_with_features(dir.path(), &["premium"]);
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
    /// `scan.crate_name` is set the way `resolve_edge_scan_with_features`
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
}
