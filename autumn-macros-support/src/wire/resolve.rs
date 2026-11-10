//! Type identity: which `WireShape` type a path names.
//!
//! A proc macro sees a type path as written, not the type it resolves to.
//! The path, the crate and the source-file module of each descriptor, and the
//! workspace dependency graph together pick one type. When they cannot pick
//! one, the resolver refuses. It does not guess.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::TypeDescriptor;

/// The type name of the framework's body-less sentinel.
pub const NO_BODY: &str = "NoBody";

/// The package that defines the framework's `NoBody`.
pub const FRAMEWORK_CRATE: &str = "autumn-web";

/// The module of the framework's `NoBody`.
const FRAMEWORK_MODULE: &str = "wire";

/// Where a type path is written: the crate and the module of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope<'a> {
    /// The package that holds the path.
    pub krate: &'a str,
    /// The module of the source file. See [`module_of_file`].
    pub module: &'a str,
}

/// The type a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved<'a> {
    /// A type with a descriptor.
    Type(&'a TypeDescriptor),
    /// The framework's `NoBody`. It writes no descriptor. It is JSON `null`.
    NoBody,
}

/// The dependencies between the packages of a workspace.
///
/// Each edge has the name the dependent crate uses in source: the package
/// name with `_`, or the name a rename gives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crates {
    deps: BTreeMap<String, BTreeMap<String, String>>,
}

impl Crates {
    /// Record that `package` depends on `dependency`, and names it
    /// `extern_name` in source.
    pub fn add_dependency(&mut self, package: &str, extern_name: &str, dependency: &str) {
        self.deps
            .entry(norm(package))
            .or_default()
            .insert(norm(extern_name), dependency.to_owned());
    }

    /// The package that `package` names `extern_name` in source.
    #[must_use]
    pub fn dependency(&self, package: &str, extern_name: &str) -> Option<&str> {
        self.deps
            .get(&norm(package))?
            .get(&norm(extern_name))
            .map(String::as_str)
    }

    /// Whether `from` is `to`, or depends on it directly or through others.
    #[must_use]
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        let to = norm(to);
        let mut seen = BTreeSet::new();
        let mut queue = vec![norm(from)];
        while let Some(at) = queue.pop() {
            if at == to {
                return true;
            }
            if !seen.insert(at.clone()) {
                continue;
            }
            if let Some(deps) = self.deps.get(&at) {
                queue.extend(deps.values().map(|d| norm(d)));
            }
        }
        false
    }
}

/// A package name as source writes it: `-` becomes `_`.
fn norm(name: &str) -> String {
    name.replace('-', "_")
}

/// The id of a type: `krate::module::Name`, or `krate::Name` at the root.
#[must_use]
pub fn type_id(krate: &str, module: &str, name: &str) -> String {
    if module.is_empty() {
        format!("{krate}::{name}")
    } else {
        format!("{krate}::{module}::{name}")
    }
}

/// The module of a source file, relative to the package's `src` root.
///
/// `src/lib.rs` and a file outside `src` give `""`. `src/api/items.rs` and
/// `src/api/items/mod.rs` give `api::items`. A binary root gets its own
/// name, so it does not share ids with the library: `src/main.rs` gives
/// `main`, and `src/bin/tool.rs` and `src/bin/tool/main.rs` give `bin::tool`.
/// An inline `mod` in a file is not visible here.
#[must_use]
pub fn module_of_file(file: &Path, manifest_dir: &Path) -> String {
    let Ok(relative) = file.strip_prefix(manifest_dir) else {
        return String::new();
    };
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let Some(("src", rest)) = parts.split_first().map(|(f, r)| (f.as_str(), r)) else {
        return String::new();
    };
    let mut segments: Vec<&str> = rest.iter().map(String::as_str).collect();
    if let Some(last) = segments.last_mut() {
        *last = last.strip_suffix(".rs").unwrap_or(last);
    }
    let root = segments.first() == Some(&"bin") && segments.len() == 3;
    if segments.last() == Some(&"mod") || (root && segments.last() == Some(&"main")) {
        segments.pop();
    }
    if segments.as_slice() == ["lib"] {
        return String::new();
    }
    segments.join("::")
}

/// Every path a written type names, in order: `Option<Vec<shared::Item>>`
/// gives `Option`, `Vec`, `shared::Item`. Lifetimes, numbers and keywords
/// are left out.
#[must_use]
pub fn type_paths(ty: &str) -> Vec<String> {
    const KEYWORDS: [&str; 11] = [
        "dyn", "mut", "impl", "const", "for", "where", "fn", "as", "unsafe", "extern", "Self",
    ];
    let mut out = Vec::new();
    let mut word = String::new();
    // A lifetime, or the length of an array after `;`, is not a type.
    let mut skip = false;
    let mut flush = |word: &mut String, skip: &mut bool| {
        let path = word.trim_matches(':');
        let numeric = path.chars().next().is_some_and(|c| c.is_ascii_digit());
        if !path.is_empty() && !*skip && !numeric && !KEYWORDS.contains(&path) {
            out.push(path.to_owned());
        }
        word.clear();
        *skip = false;
    };
    for ch in ty.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == ':' {
            word.push(ch);
        } else if ch == '\'' || ch == ';' {
            flush(&mut word, &mut skip);
            skip = true;
        } else if !word.is_empty() || !ch.is_whitespace() {
            let keep = skip && word.is_empty();
            flush(&mut word, &mut skip);
            skip = keep;
        }
    }
    flush(&mut word, &mut skip);
    out
}

/// Type names that need no descriptor.
const KNOWN_LEAVES: &[&str] = &[
    // Primitives and strings.
    "bool",
    "char",
    "str",
    "String",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "f32",
    "f64",
    "NonZeroU8",
    "NonZeroU16",
    "NonZeroU32",
    "NonZeroU64",
    "NonZeroU128",
    "NonZeroUsize",
    "NonZeroI8",
    "NonZeroI16",
    "NonZeroI32",
    "NonZeroI64",
    "NonZeroI128",
    "NonZeroIsize",
    "NonZero",
    // Containers and pointers. Their type arguments are checked on their own.
    "Option",
    "Vec",
    "VecDeque",
    "LinkedList",
    "BinaryHeap",
    "HashMap",
    "BTreeMap",
    "HashSet",
    "BTreeSet",
    "IndexMap",
    "IndexSet",
    "SmallVec",
    "Box",
    "Rc",
    "Arc",
    "Cow",
    "Cell",
    "RefCell",
    "Reverse",
    "Wrapping",
    "PhantomData",
    "Map",
    // Well-known scalars.
    "Value",
    "Number",
    "RawValue",
    "Uuid",
    "DateTime",
    "NaiveDate",
    "NaiveDateTime",
    "NaiveTime",
    "Utc",
    "FixedOffset",
    "Local",
    "Tz",
    "Date",
    "Time",
    "OffsetDateTime",
    "PrimitiveDateTime",
    "UtcOffset",
    "Duration",
    "SystemTime",
    "Decimal",
    "BigDecimal",
    "Url",
    "IpAddr",
    "Ipv4Addr",
    "Ipv6Addr",
    "SocketAddr",
    "PathBuf",
    "Path",
    "OsString",
    "Bytes",
    "ByteBuf",
];

/// Whether a type name needs no descriptor: a primitive, a standard
/// container, or a well-known scalar such as `Uuid` or `DateTime`.
#[must_use]
pub fn is_known_leaf(name: &str) -> bool {
    KNOWN_LEAVES.contains(&name)
}

/// One type that a name can mean.
#[derive(Clone, Copy)]
struct Candidate<'a> {
    krate: &'a str,
    module: &'a str,
    found: Resolved<'a>,
}

impl Candidate<'_> {
    fn id(&self, name: &str) -> String {
        type_id(self.krate, self.module, name)
    }
}

/// What the path prefix says about where the type is.
enum Target {
    /// The path names this crate. `module` is a hint.
    Crate { krate: String, module: String },
    /// A bare name, or a prefix that names no crate in the pass: a module of
    /// the scope's crate, or a dependency the pass cannot name.
    Unqualified { module: String },
    /// The standard library. It has no descriptor.
    Std,
}

/// The type that `path` names, written in `scope`.
///
/// `Ok(None)`: no type of that name has a descriptor. `Err`: more than one
/// type can be the one meant.
///
/// `crates` limits an unqualified name to the crates that the scope's crate
/// can reach, and maps a renamed dependency. Without it, every crate counts.
///
/// The module of a descriptor is the module of its source file. It picks
/// between two types of one crate, and never drops the only candidate.
///
/// # Errors
/// The path can name more than one type.
pub fn resolve_type<'a>(
    path: &str,
    scope: Scope<'_>,
    types: &'a [TypeDescriptor],
    crates: Option<&Crates>,
) -> Result<Option<Resolved<'a>>, String> {
    let segments: Vec<&str> = path.split("::").filter(|s| !s.is_empty()).collect();
    let Some((&name, prefix)) = segments.split_last() else {
        return Ok(None);
    };
    let mut all: Vec<Candidate<'a>> = types
        .iter()
        .filter(|t| t.shape.name == name)
        .map(|t| Candidate {
            krate: &t.krate,
            module: &t.module,
            found: Resolved::Type(t),
        })
        .collect();
    if name == NO_BODY {
        all.push(Candidate {
            krate: FRAMEWORK_CRATE,
            module: FRAMEWORK_MODULE,
            found: Resolved::NoBody,
        });
    }
    if all.is_empty() {
        return Ok(None);
    }
    let refuse = |candidates: &[Candidate<'_>]| -> String {
        let mut ids: Vec<String> = candidates
            .iter()
            .map(|c| format!("`{}`", c.id(name)))
            .collect();
        if is_known_leaf(name) {
            ids.push(format!("a standard `{name}`"));
        }
        let graph = if crates.is_none() && is_known_leaf(name) {
            " With no dependency graph (`export --from`), only a `std::` path names a \
             standard type. Run the export without `--from`, or mark the field \
             `#[wire(opaque)]`"
        } else {
            ""
        };
        format!(
            "`{path}` can name more than one type: {}. Write the full path in the signature, \
             for example `crate::…::{name}` or `<crate>::{name}`.{graph}",
            ids.join(", ")
        )
    };
    match target(prefix, scope, &all, crates) {
        Target::Std => Ok(None),
        Target::Crate { krate, module } => {
            let in_crate: Vec<Candidate<'a>> = all
                .iter()
                .copied()
                .filter(|c| norm(c.krate) == norm(&krate))
                .collect();
            if !in_crate.is_empty() {
                return pick(&in_crate, &module).map(Some).map_err(|c| refuse(&c));
            }
            // The named crate re-exports a type of a crate it depends on.
            let passed_on: Vec<Candidate<'a>> = all
                .iter()
                .copied()
                .filter(|c| {
                    norm(c.krate) != norm(&krate)
                        && crates.is_none_or(|g| g.reaches(&krate, c.krate))
                })
                .collect();
            match passed_on.as_slice() {
                [] => Ok(None),
                [one] => Ok(Some(one.found)),
                many => Err(refuse(many)),
            }
        }
        Target::Unqualified { module } => {
            let reachable: Vec<Candidate<'a>> = all
                .iter()
                .copied()
                .filter(|c| {
                    norm(c.krate) == norm(scope.krate)
                        || matches!(c.found, Resolved::NoBody)
                        || crates.is_none_or(|g| g.reaches(scope.krate, c.krate))
                })
                .collect();
            let (own, others): (Vec<Candidate<'a>>, Vec<Candidate<'a>>) = reachable
                .iter()
                .partition(|c| norm(c.krate) == norm(scope.krate));
            // A type defined in this very module is the one in scope: an
            // import of another type with that name does not compile.
            let here: Vec<Candidate<'a>> =
                own.iter().copied().filter(|c| c.module == module).collect();
            match (here.as_slice(), own.as_slice(), others.as_slice()) {
                ([one], _, _) => Ok(Some(one.found)),
                (_, [], []) => Ok(None),
                // A bare `Duration` that is not defined in this module can be
                // `std::time::Duration`.
                (_, own, others) if is_known_leaf(name) => Err(refuse(&[own, others].concat())),
                (_, own, []) => pick(own, &module).map(Some).map_err(|c| refuse(&c)),
                (_, [], others)
                    if others
                        .iter()
                        .all(|c| norm(c.krate) == norm(others[0].krate)) =>
                {
                    pick(others, "").map(Some).map_err(|c| refuse(&c))
                }
                _ => Err(refuse(&reachable)),
            }
        }
    }
}

/// Where a path prefix points.
fn target(
    prefix: &[&str],
    scope: Scope<'_>,
    all: &[Candidate<'_>],
    crates: Option<&Crates>,
) -> Target {
    let own = scope.krate.to_owned();
    let here: Vec<&str> = scope.module.split("::").filter(|s| !s.is_empty()).collect();
    let root = crate_root(&here).len();
    let join = |base: &[&str], rest: &[&str]| -> String {
        base.iter()
            .chain(rest)
            .copied()
            .collect::<Vec<_>>()
            .join("::")
    };
    match prefix {
        [] => Target::Unqualified {
            module: scope.module.to_owned(),
        },
        ["crate", rest @ ..] => Target::Crate {
            krate: own,
            module: join(&here[..root], rest),
        },
        ["self", rest @ ..] => Target::Crate {
            krate: own,
            module: join(&here, rest),
        },
        ["super", ..] => {
            let mut base = here;
            let mut rest = prefix;
            while let Some((&"super", tail)) = rest.split_first() {
                if base.len() > root {
                    base.pop();
                }
                rest = tail;
            }
            Target::Crate {
                krate: own,
                module: join(&base, rest),
            }
        }
        ["std" | "core" | "alloc", ..] => Target::Std,
        [first, rest @ ..] => {
            // With the graph, a first segment is a crate only when it is the
            // own crate or a dependency. Else it is a local module, even when
            // another crate in the pass has that name.
            let named = crates
                .map_or_else(
                    || {
                        all.iter()
                            .find(|c| norm(c.krate) == *first)
                            .map(|c| c.krate.to_owned())
                    },
                    |g| g.dependency(scope.krate, first).map(str::to_owned),
                )
                .or_else(|| (norm(scope.krate) == *first).then(|| own.clone()));
            named.map_or_else(
                || Target::Unqualified {
                    module: join(&here, prefix),
                },
                |krate| Target::Crate {
                    krate,
                    module: rest.join("::"),
                },
            )
        }
    }
}

/// The root module of the crate that holds `module`: `main` and `bin::<name>`
/// for a binary (see [`module_of_file`]), else the library root.
#[must_use]
pub fn crate_root<'m>(module: &'m [&'m str]) -> &'m [&'m str] {
    match module {
        ["main", ..] => &module[..1],
        ["bin", _, ..] => &module[..2],
        _ => &[],
    }
}

/// The one candidate, else the one in `module`. On failure, the candidates.
fn pick<'a>(
    candidates: &[Candidate<'a>],
    module: &str,
) -> Result<Resolved<'a>, Vec<Candidate<'a>>> {
    if let [one] = candidates {
        return Ok(one.found);
    }
    match candidates
        .iter()
        .filter(|c| c.module == module)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [one] => Ok(one.found),
        _ => Err(candidates.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireTypeDescriptor;

    fn ty(krate: &str, module: &str, name: &str) -> TypeDescriptor {
        TypeDescriptor {
            krate: krate.to_owned(),
            module: module.to_owned(),
            shape: WireTypeDescriptor {
                name: name.to_owned(),
                ..WireTypeDescriptor::default()
            },
        }
    }

    const fn at<'a>(krate: &'a str, module: &'a str) -> Scope<'a> {
        Scope { krate, module }
    }

    /// The id of the type a path resolves to.
    fn pick(
        path: &str,
        scope: Scope<'_>,
        types: &[TypeDescriptor],
        crates: Option<&Crates>,
    ) -> Result<Option<String>, String> {
        resolve_type(path, scope, types, crates).map(|r| {
            r.map(|r| match r {
                Resolved::Type(t) => t.id(),
                Resolved::NoBody => "NoBody".to_owned(),
            })
        })
    }

    fn catalog_and_shared() -> Vec<TypeDescriptor> {
        vec![ty("catalog", "models", "Item"), ty("shared", "", "Item")]
    }

    #[test]
    fn a_crate_qualified_path_picks_that_crate_beside_a_local_type() {
        let types = catalog_and_shared();
        assert_eq!(
            pick("shared::Item", at("catalog", ""), &types, None),
            Ok(Some("shared::Item".to_owned()))
        );
    }

    #[test]
    fn a_crate_relative_path_picks_the_own_crate() {
        let types = catalog_and_shared();
        assert_eq!(
            pick("crate::models::Item", at("catalog", "api"), &types, None),
            Ok(Some("catalog::models::Item".to_owned()))
        );
    }

    #[test]
    fn a_bare_name_defined_in_the_same_module_is_that_type() {
        let types = vec![ty("catalog", "", "Item"), ty("shared", "", "Item")];
        assert_eq!(
            pick("Item", at("catalog", ""), &types, None),
            Ok(Some("catalog::Item".to_owned()))
        );
    }

    #[test]
    fn a_bare_name_of_a_local_type_and_a_reachable_dependency_is_refused() {
        let types = catalog_and_shared();
        let err = pick("Item", at("catalog", "api"), &types, None).expect_err("ambiguous");
        assert!(err.contains("`catalog::models::Item`"), "{err}");
        assert!(err.contains("`shared::Item`"), "{err}");
        assert!(err.contains("full path"), "{err}");
    }

    #[test]
    fn a_crate_the_scope_cannot_reach_is_not_a_candidate() {
        let types = catalog_and_shared();
        let crates = Crates::default();
        assert_eq!(
            pick("Item", at("catalog", "api"), &types, Some(&crates)),
            Ok(Some("catalog::models::Item".to_owned()))
        );
    }

    #[test]
    fn a_bare_name_with_one_other_crate_resolves_there() {
        let types = vec![ty("shared", "", "Item")];
        assert_eq!(
            pick("Item", at("catalog", ""), &types, None),
            Ok(Some("shared::Item".to_owned()))
        );
    }

    #[test]
    fn two_types_with_one_name_in_one_crate_pick_by_module() {
        let types = vec![
            ty("catalog", "models", "Item"),
            ty("catalog", "api", "Item"),
        ];
        assert_eq!(
            pick("crate::api::Item", at("catalog", ""), &types, None),
            Ok(Some("catalog::api::Item".to_owned()))
        );
        assert_eq!(
            pick("models::Item", at("catalog", ""), &types, None),
            Ok(Some("catalog::models::Item".to_owned()))
        );
        assert_eq!(
            pick("super::api::Item", at("catalog", "models"), &types, None),
            Ok(Some("catalog::api::Item".to_owned()))
        );
        assert!(pick("Item", at("catalog", ""), &types, None).is_err());
    }

    #[test]
    fn a_renamed_dependency_resolves_through_the_graph() {
        let types = catalog_and_shared();
        let mut crates = Crates::default();
        crates.add_dependency("catalog", "contracts", "shared");
        assert_eq!(
            pick("contracts::Item", at("catalog", ""), &types, Some(&crates)),
            Ok(Some("shared::Item".to_owned()))
        );
    }

    #[test]
    fn a_crate_that_re_exports_a_type_resolves_to_its_source() {
        let types = vec![ty("shared", "", "Item"), ty("catalog", "", "Item")];
        let mut crates = Crates::default();
        crates.add_dependency("catalog", "facade", "facade");
        crates.add_dependency("facade", "shared", "shared");
        assert_eq!(
            pick("facade::Item", at("catalog", ""), &types, Some(&crates)),
            Ok(Some("shared::Item".to_owned()))
        );
    }

    #[test]
    fn a_local_module_named_like_another_crate_is_a_module_with_the_graph() {
        let types = vec![
            ty("storefront", "catalog", "Item"),
            ty("catalog", "", "Item"),
        ];
        let crates = Crates::default();
        assert_eq!(
            pick("catalog::Item", at("storefront", ""), &types, Some(&crates)),
            Ok(Some("storefront::catalog::Item".to_owned()))
        );
    }

    #[test]
    fn a_standard_library_path_is_never_a_workspace_type() {
        let types = vec![ty("shared", "", "Duration")];
        assert_eq!(
            pick("std::time::Duration", at("catalog", ""), &types, None),
            Ok(None)
        );
        let err = pick("Duration", at("catalog", ""), &types, None).expect_err("ambiguous");
        assert!(err.contains("a standard `Duration`"), "{err}");
    }

    #[test]
    fn a_crate_relative_path_to_a_re_exported_type_resolves_to_its_source() {
        let types = vec![ty("shared", "", "Item")];
        let mut crates = Crates::default();
        crates.add_dependency("catalog", "shared", "shared");
        assert_eq!(
            pick("crate::dto::Item", at("catalog", ""), &types, Some(&crates)),
            Ok(Some("shared::Item".to_owned()))
        );
    }

    #[test]
    fn a_bare_leaf_name_not_defined_here_is_ambiguous_also_in_the_own_crate() {
        let types = vec![ty("catalog", "models", "Duration")];
        assert!(pick("Duration", at("catalog", "api"), &types, None).is_err());
        assert_eq!(
            pick("Duration", at("catalog", "models"), &types, None),
            Ok(Some("catalog::models::Duration".to_owned()))
        );
    }

    #[test]
    fn a_leaf_refusal_without_a_graph_says_how_to_go_on() {
        let types = vec![ty("shared", "", "Value")];
        let err = pick("serde_json::Value", at("catalog", ""), &types, None).expect_err("refused");
        assert!(err.contains("--from"), "{err}");
    }

    #[test]
    fn crate_in_a_binary_means_the_binary_root() {
        let types = vec![ty("catalog", "", "Item"), ty("catalog", "main", "Item")];
        assert_eq!(
            pick("crate::Item", at("catalog", "main"), &types, None),
            Ok(Some("catalog::main::Item".to_owned()))
        );
        assert_eq!(
            pick("super::Item", at("catalog", "main::api"), &types, None),
            Ok(Some("catalog::main::Item".to_owned()))
        );
    }

    #[test]
    fn no_descriptor_is_none() {
        assert_eq!(pick("Item", at("catalog", ""), &[], None), Ok(None));
    }

    #[test]
    fn the_framework_no_body_is_found_without_a_descriptor() {
        assert_eq!(
            pick("NoBody", at("catalog", ""), &[], None),
            Ok(Some("NoBody".to_owned()))
        );
        assert_eq!(
            pick("autumn_web::wire::NoBody", at("catalog", ""), &[], None),
            Ok(Some("NoBody".to_owned()))
        );
    }

    #[test]
    fn a_user_no_body_in_the_same_module_is_the_user_type() {
        let types = vec![ty("catalog", "", "NoBody")];
        assert_eq!(
            pick("NoBody", at("catalog", ""), &types, None),
            Ok(Some("catalog::NoBody".to_owned()))
        );
        assert_eq!(
            pick("autumn_web::wire::NoBody", at("catalog", ""), &types, None),
            Ok(Some("NoBody".to_owned()))
        );
    }

    #[test]
    fn a_user_no_body_in_another_module_is_ambiguous_with_the_framework_one() {
        let types = vec![ty("catalog", "dto", "NoBody")];
        assert!(pick("NoBody", at("catalog", ""), &types, None).is_err());
    }

    #[test]
    fn the_graph_is_transitive() {
        let mut crates = Crates::default();
        crates.add_dependency("a", "b", "b");
        crates.add_dependency("b", "c_renamed", "c");
        assert!(crates.reaches("a", "c"));
        assert!(crates.reaches("a", "a"));
        assert!(!crates.reaches("c", "a"));
        assert_eq!(crates.dependency("b", "c_renamed"), Some("c"));
        assert_eq!(crates.dependency("a", "c"), None);
    }

    #[test]
    fn a_type_id_joins_crate_module_and_name() {
        assert_eq!(type_id("shared", "", "Item"), "shared::Item");
        assert_eq!(type_id("shared", "a::b", "Item"), "shared::a::b::Item");
    }

    #[test]
    fn the_module_of_a_file_follows_the_src_layout() {
        let root = Path::new("/w/catalog");
        let module = |f: &str| module_of_file(&root.join(f), root);
        assert_eq!(module("src/lib.rs"), "");
        assert_eq!(module("src/main.rs"), "main");
        assert_eq!(module("src/api/items.rs"), "api::items");
        assert_eq!(module("src/api/mod.rs"), "api");
        assert_eq!(module("src/bin/tool.rs"), "bin::tool");
        assert_eq!(module("src/bin/tool/main.rs"), "bin::tool");
        assert_eq!(module("src/bin/tool/dto.rs"), "bin::tool::dto");
        assert_eq!(module("examples/demo.rs"), "");
        assert_eq!(module_of_file(Path::new("/elsewhere/src/a.rs"), root), "");
    }

    #[test]
    fn type_paths_lists_each_named_path_in_order() {
        assert_eq!(
            type_paths("Option<Vec<shared::Details>>"),
            ["Option", "Vec", "shared::Details"]
        );
        assert_eq!(
            type_paths("HashMap<String,Details>"),
            ["HashMap", "String", "Details"]
        );
        assert_eq!(type_paths("&'a str"), ["str"]);
        assert_eq!(type_paths("[Details;3]"), ["Details"]);
        assert_eq!(
            type_paths("(u8,::shared::Details)"),
            ["u8", "shared::Details"]
        );
        assert_eq!(type_paths("&'static mut dyn Trait"), ["Trait"]);
        assert_eq!(type_paths("Vec<Self>"), ["Vec"]);
        assert_eq!(type_paths("[u8;LEN]"), ["u8"]);
        assert_eq!(type_paths("[u8; LEN]"), ["u8"]);
        assert_eq!(type_paths("Vec::<Item>"), ["Vec", "Item"]);
        assert_eq!(type_paths("fn(u8)->Item"), ["u8", "Item"]);
        assert_eq!(type_paths("<T as Tr>::X"), ["T", "Tr", "X"]);
    }

    #[test]
    fn primitives_containers_and_common_scalars_are_known_leaves() {
        for name in [
            "String", "u32", "bool", "Option", "Vec", "HashMap", "Box", "Uuid", "DateTime", "Utc",
            "Value", "Decimal",
        ] {
            assert!(is_known_leaf(name), "{name}");
        }
        for name in ["Details", "Status", "Item"] {
            assert!(!is_known_leaf(name), "{name}");
        }
    }
}
