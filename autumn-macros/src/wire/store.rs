//! Where wire descriptors land, and how they are found again.
//!
//! `#[endpoint]` writes one JSON file per endpoint; `#[contract_checked]` reads
//! them back. Both resolve the same directory:
//!
//! 1. `AUTUMN_CONTRACT_DIR`, when set — the escape hatch, and what tests use.
//! 2. `<workspace root>/target/autumn-contracts`, where the workspace root is
//!    the nearest ancestor of `CARGO_MANIFEST_DIR` whose `Cargo.toml` declares
//!    `[workspace]`.
//! 3. `<CARGO_MANIFEST_DIR>/target/autumn-contracts` otherwise.
//!
//! Writing is best effort. A read-only or sandboxed build must still compile,
//! so every I/O error here is swallowed: the artifact only ever *enriches* a
//! diagnostic, and the const assertion `#[contract_checked]` emits alongside it
//! is what actually holds the contract.

use std::path::{Path, PathBuf};

use crate::wire::ir::{EndpointDescriptor, ResolvedEndpoint, TypeDescriptor, WireTypeDescriptor};

/// Resolve the contract directory from the given environment.
///
/// Split from [`contract_dir`] so tests can drive it without touching process
/// environment variables.
#[must_use]
pub fn contract_dir_for(explicit: Option<&Path>, manifest_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        return Some(dir.to_path_buf());
    }
    let manifest = manifest_dir?;
    let root = workspace_root(manifest).unwrap_or_else(|| manifest.to_path_buf());
    Some(root.join("target").join("autumn-contracts"))
}

/// The contract directory for the crate currently being compiled.
///
/// `None` turns descriptors off entirely: nothing is written and nothing is
/// read, so diagnostics fall back to the const assertions. Set
/// `AUTUMN_CONTRACT_DIR=""` to force that.
#[must_use]
pub fn contract_dir() -> Option<PathBuf> {
    // This crate's own unit tests expand macros against fixtures. Those are
    // not a build of anyone's service, and their descriptors would collide
    // with real ones in the shared directory.
    if cfg!(test) {
        return None;
    }
    let explicit = std::env::var_os("AUTUMN_CONTRACT_DIR");
    if explicit.as_ref().is_some_and(|dir| dir.is_empty()) {
        return None;
    }
    let explicit = explicit.map(PathBuf::from);
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    contract_dir_for(explicit.as_deref(), manifest.as_deref())
}

/// The nearest ancestor (inclusive) whose `Cargo.toml` declares `[workspace]`.
fn workspace_root(from: &Path) -> Option<PathBuf> {
    for dir in from.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if declares_workspace(&text) {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Whether a `Cargo.toml`'s text opens a `[workspace]` table.
///
/// A line-oriented scan, not a TOML parse: the macro crate has no TOML reader,
/// and the only question asked is whether a bare table header is present.
fn declares_workspace(manifest: &str) -> bool {
    manifest.lines().any(|line| {
        let line = line.trim();
        line == "[workspace]" || line.starts_with("[workspace.")
    })
}

/// Write one endpoint's descriptor. Best effort — errors are swallowed.
pub fn write_endpoint(dir: &Path, descriptor: &EndpointDescriptor) {
    write_json(dir, &descriptor.artifact_file_name(), descriptor);
}

/// Write one DTO's descriptor. Best effort — errors are swallowed.
pub fn write_type(dir: &Path, descriptor: &TypeDescriptor) {
    write_json(dir, &descriptor.artifact_file_name(), descriptor);
}

/// Whether `file_name` is a plain file name that cannot escape its directory.
///
/// `#[endpoint]` already refuses a service or endpoint name outside the safe
/// character set. This is the second lock on the same door: a descriptor write
/// must never be able to land outside the contract directory.
fn is_plain_file_name(file_name: &str) -> bool {
    !file_name.is_empty() && !file_name.starts_with('.') && !file_name.contains(['/', '\\', '\0'])
}

/// Serialize `value` to `dir/file_name`, or give up quietly.
fn write_json<T: serde::Serialize>(dir: &Path, file_name: &str, value: &T) {
    if !is_plain_file_name(file_name) {
        return;
    }
    let Ok(json) = serde_json::to_vec_pretty(value) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Two compilations of the same crate (different feature sets, a test
    // target alongside the lib) can race on one path. Write elsewhere and
    // rename, so a reader never sees a half-written file.
    let temp_path = dir.join(format!("{file_name}.{}.tmp", std::process::id()));
    if std::fs::write(&temp_path, &json).is_err() {
        return;
    }
    if std::fs::rename(&temp_path, dir.join(file_name)).is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
}

/// Every endpoint descriptor in `dir` whose `endpoint_ident` matches, with
/// both type shapes joined in.
///
/// `#[contract_checked]` knows a call site's endpoint only as a Rust path, so
/// it looks the descriptor up by the marker type's identifier. More than one
/// match means two crates named an endpoint the same way; the caller treats
/// that as "unknown" rather than guessing.
#[must_use]
pub fn find_by_ident(dir: &Path, endpoint_ident: &str) -> Vec<ResolvedEndpoint> {
    let mut found: Vec<EndpointDescriptor> = read_all(dir, "endpoint.")
        .into_iter()
        .filter(|d: &EndpointDescriptor| d.endpoint_ident == endpoint_ident)
        .collect();
    found.sort_by_key(EndpointDescriptor::artifact_file_name);
    let types: Vec<TypeDescriptor> = read_all(dir, "type.");
    found.into_iter().map(|e| resolve(e, &types)).collect()
}

/// Join an endpoint with the type shapes it references.
///
/// A type defined in the endpoint's own crate wins; otherwise any crate's
/// type of that name is used, so a DTO shared through a third crate still
/// resolves. An unresolvable name yields an empty shape, which suppresses
/// enrichment rather than inventing a contract.
fn resolve(endpoint: EndpointDescriptor, types: &[TypeDescriptor]) -> ResolvedEndpoint {
    let shape = |name: &str| -> WireTypeDescriptor {
        types
            .iter()
            .find(|t| t.shape.name == name && t.krate == endpoint.krate)
            .or_else(|| types.iter().find(|t| t.shape.name == name))
            .map(|t| t.shape.clone())
            .unwrap_or_default()
    };
    let request = shape(&endpoint.request_type);
    let response = shape(&endpoint.response_type);
    ResolvedEndpoint {
        endpoint,
        request,
        response,
    }
}

/// Read every artifact in `dir` whose file name starts with `prefix`.
fn read_all<T: serde::de::DeserializeOwned>(dir: &Path, prefix: &str) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(prefix) && name.ends_with(".json")
        })
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<T>(&bytes).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::ir::WireFieldDescriptor;

    fn endpoint(krate: &str, name: &str) -> EndpointDescriptor {
        EndpointDescriptor {
            service: "catalog".to_owned(),
            name: name.to_owned(),
            endpoint_ident: format!("{name}_endpoint"),
            krate: krate.to_owned(),
            method: "GET".to_owned(),
            path: "/items/{id}".to_owned(),
            request_type: "NoBody".to_owned(),
            response_type: "Item".to_owned(),
        }
    }

    fn item_type(krate: &str) -> TypeDescriptor {
        TypeDescriptor {
            krate: krate.to_owned(),
            shape: WireTypeDescriptor {
                name: "Item".to_owned(),
                serialized: vec![WireFieldDescriptor {
                    rust_name: "id".to_owned(),
                    wire_name: "id".to_owned(),
                    ty: "String".to_owned(),
                    required: true,
                }],
                deserialized: Vec::new(),
            },
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("autumn-wire-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn an_endpoint_round_trips_with_its_type_shapes_joined_in() {
        let dir = scratch("rt");
        write_endpoint(&dir, &endpoint("catalog", "get_item"));
        write_type(&dir, &item_type("catalog"));
        let got = find_by_ident(&dir, "get_item_endpoint");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].response.serialized[0].rust_name, "id");
        assert!(got[0].request.serialized.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolvable_type_yields_an_empty_shape_rather_than_a_guess() {
        let dir = scratch("unres");
        write_endpoint(&dir, &endpoint("catalog", "get_item"));
        let got = find_by_ident(&dir, "get_item_endpoint");
        assert_eq!(got.len(), 1);
        assert!(got[0].response.serialized.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_type_from_the_endpoints_own_crate_wins_over_a_same_named_one() {
        let dir = scratch("samename");
        write_endpoint(&dir, &endpoint("catalog", "get_item"));
        write_type(&dir, &item_type("warehouse"));
        let mut mine = item_type("catalog");
        mine.shape.serialized[0].rust_name = "sku".to_owned();
        write_type(&dir, &mine);
        let got = find_by_ident(&dir, "get_item_endpoint");
        assert_eq!(got[0].response.serialized[0].rust_name, "sku");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ident_defined_by_two_crates_returns_both_so_the_caller_can_refuse_to_guess() {
        let dir = scratch("dup");
        write_endpoint(&dir, &endpoint("catalog", "get_item"));
        write_endpoint(&dir, &endpoint("warehouse", "get_item"));
        assert_eq!(find_by_ident(&dir, "get_item_endpoint").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_name_that_could_escape_the_directory_is_never_written() {
        assert!(is_plain_file_name("endpoint.catalog.get_item.json"));
        assert!(!is_plain_file_name("../escape.json"));
        assert!(!is_plain_file_name("a/b.json"));
        assert!(!is_plain_file_name(".hidden"));
        assert!(!is_plain_file_name(""));

        let dir = scratch("escape");
        let mut descriptor = endpoint("catalog", "get_item");
        descriptor.krate = "../..".to_owned();
        write_endpoint(&dir, &descriptor);
        assert!(find_by_ident(&dir, "get_item_endpoint").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_reads_as_empty_rather_than_failing_the_build() {
        let dir = std::env::temp_dir().join("autumn-wire-does-not-exist");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(find_by_ident(&dir, "get_item_endpoint").is_empty());
    }

    #[test]
    fn explicit_dir_wins_over_the_workspace_walk() {
        let explicit = PathBuf::from("/tmp/explicit");
        assert_eq!(
            contract_dir_for(Some(&explicit), Some(Path::new("/tmp/crate"))),
            Some(explicit)
        );
    }

    #[test]
    fn the_dir_hangs_off_the_workspace_root_not_the_member_crate() {
        let root = scratch("ws");
        let member = root.join("members").join("catalog");
        std::fs::create_dir_all(&member).expect("fixture dirs");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("root toml");
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"catalog\"\n")
            .expect("member toml");
        assert_eq!(
            contract_dir_for(None, Some(&member)),
            Some(root.join("target").join("autumn-contracts"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_standalone_crate_falls_back_to_its_own_target_dir() {
        let krate = scratch("solo");
        std::fs::create_dir_all(&krate).expect("fixture dir");
        std::fs::write(krate.join("Cargo.toml"), "[package]\nname = \"solo\"\n").expect("toml");
        assert_eq!(
            contract_dir_for(None, Some(&krate)),
            Some(krate.join("target").join("autumn-contracts"))
        );
        let _ = std::fs::remove_dir_all(&krate);
    }

    #[test]
    fn a_workspace_dot_table_alone_marks_the_root() {
        assert!(declares_workspace("[workspace.package]\nversion = \"1\"\n"));
        assert!(!declares_workspace("[package]\nname = \"x\"\n"));
        assert!(!declares_workspace(
            "[dependencies]\nworkspace-hack = \"1\"\n"
        ));
    }
}
