//! `autumn build` -- compile the app and pre-render static routes.
//!
//! Orchestrates three steps:
//! 1. `cargo build [--release] [-p <package>]` to compile the user's binary.
//! 2. In release mode: fingerprint every file under `static/`, write
//!    content-hashed copies alongside the originals, and emit
//!    `static/.autumn-manifest.json` so the static renderer can resolve
//!    fingerprinted URLs when pre-rendering HTML pages.
//! 3. Run the binary with `AUTUMN_BUILD_STATIC=1` so the runtime renders
//!    static routes to `dist/` instead of starting the HTTP server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// Build the `cargo build` command for the static pipeline.
///
/// Factored out so the flags are unit-testable. With `embed`, the
/// `autumn-web/embed-assets` feature is enabled so the binary bakes in the
/// `static/` tree (and its manifest) plus i18n locales.
fn build_cargo_command(
    debug: bool,
    embed: bool,
    package: Option<&str>,
    bin: Option<&str>,
    extra_features: Option<&str>,
) -> Command {
    let mut cargo = Command::new("cargo");
    cargo.arg("build");
    if !debug {
        cargo.arg("--release");
    }
    if let Some(pkg) = package {
        cargo.args(["-p", pkg]);
    }
    if let Some(b) = bin {
        cargo.args(["--bin", b]);
    }
    // Build the feature string. `embed-assets` (the app-crate feature that pulls
    // in `autumn-web/embed-assets`) is added when we are in the embed phase.
    // `extra_features` (e.g. `autumn-web/managed-pg-bundled`) is forwarded from
    // the CLI so that apps wiring ManagedPostgresPoolProvider can compile in both
    // the fingerprint phase and the final embed phase without feature-gate errors.
    match (embed, extra_features) {
        (true, Some(extra)) => {
            cargo.args(["--features", &format!("embed-assets,{extra}")]);
        }
        (true, None) => {
            cargo.args(["--features", "embed-assets"]);
        }
        (false, Some(extra)) => {
            cargo.args(["--features", extra]);
        }
        (false, None) => {}
    }
    cargo
}

/// Run a cargo command, exiting the process on failure.
fn run_cargo_or_exit(mut cargo: Command) {
    let status = cargo.status().expect("failed to run cargo build");
    if !status.success() {
        eprintln!("\u{2717} Compilation failed");
        std::process::exit(1);
    }
}

/// Build a self-contained release binary with `static/` (and its fingerprint
/// manifest) plus i18n locales embedded.
///
/// Three phases so the embedded tree is complete and consistent:
/// 1. Compile **without** the embed feature so the app's build scripts (e.g.
///    Tailwind CSS generation) populate `static/` first.
/// 2. Fingerprint the now-complete `static/` tree of the **selected package**
///    (not the CLI cwd), writing the manifest + hashed copies.
/// 3. Recompile **with** the embed feature so `include_dir!` bakes the
///    fingerprinted tree into the binary.
fn build_embedded(
    debug: bool,
    profile: &str,
    package: Option<&str>,
    bin: Option<&str>,
    features: Option<&str>,
) {
    // Resolve the selected package's directory so `-p <pkg>` fingerprints that
    // package's `static/` (which `embed_static!` reads via $CARGO_MANIFEST_DIR),
    // not whatever `static/` happens to sit next to the CLI's cwd.
    let (_, manifest_dir, _) = find_binary(debug, package, bin);
    let static_dir = manifest_dir
        .unwrap_or_else(|| std::env::current_dir().expect("current dir"))
        .join("static");

    eprintln!("Compiling ({profile} profile)...");
    // Phase 1: compile WITHOUT embed-assets so build scripts populate static/.
    // Pass extra features (e.g. managed-pg-bundled) so apps wiring
    // ManagedPostgresPoolProvider can compile even in this pre-embed phase.
    run_cargo_or_exit(build_cargo_command(debug, false, package, bin, features));

    eprintln!("\nFingerprinting static assets for embedding...");
    fingerprint_assets_in(&static_dir);

    eprintln!("\nEmbedding assets and locales into the binary...");
    // Phase 3: recompile WITH embed-assets so include_dir! bakes the tree in.
    run_cargo_or_exit(build_cargo_command(debug, true, package, bin, features));

    eprintln!("\n\u{1F342} Build complete! Assets and locales embedded into the binary.");
}

/// Set up environment variables for the static-renderer command.
///
/// Factored out of [`run`] so the env-setup logic is unit-testable without
/// invoking `cargo build` or the app binary.
fn apply_renderer_env(
    cmd: &mut Command,
    debug: bool,
    package: Option<&str>,
    bin: Option<&str>,
    resolved_pkg: Option<&str>,
    manifest_dir: Option<&PathBuf>,
) {
    // Clear any inherited attach URL so a stale value can't redirect the static
    // renderer to the wrong database; re-set below only when a live cluster is
    // discovered.
    cmd.env_remove(crate::serve::MANAGED_PG_ATTACH_URL_ENV);
    // When --bin selects a workspace member without -p, use the resolved package
    // name so the renderer shares that member's cluster, not the workspace root's.
    let effective_pkg = package.or_else(|| bin.and(resolved_pkg));
    if let Some(pg) = crate::serve::managed_pg_env(effective_pkg) {
        cmd.env(crate::serve::MANAGED_PG_DATA_DIR_ENV, &pg.data_dir);
        if let Some(url) = pg.attach_url {
            cmd.env(crate::serve::MANAGED_PG_ATTACH_URL_ENV, url);
        }
    }
    // Mirror cargo's profile selection so dev builds skip production-only
    // validation and release builds apply prod config overrides.
    // Users can override by setting AUTUMN_PROFILE explicitly.
    if std::env::var("AUTUMN_PROFILE").is_err() {
        cmd.env("AUTUMN_PROFILE", if debug { "dev" } else { "prod" });
    }
    // Pin CWD to the package directory so config loading and dist/ output are
    // relative to the correct project root when -p <package> points to a subdir.
    if let Some(dir) = manifest_dir {
        let cwd = std::env::current_dir().expect("current dir");
        if dir != &cwd {
            cmd.current_dir(dir);
        }
    }
}

/// Run the static build pipeline.
pub fn run(
    debug: bool,
    embed: bool,
    package: Option<&str>,
    bin: Option<&str>,
    features: Option<&str>,
) {
    eprintln!("\u{1F342} autumn build\n");

    let profile = if debug { "dev" } else { "release" };

    // Embedding produces a self-contained release binary; it is not static-site
    // generation, so it skips the static renderer (which requires `#[static_get]`
    // routes and the app's runtime state) and lets dynamic-server apps build a
    // single binary without a database or pre-render step.
    if embed {
        build_embedded(debug, profile, package, bin, features);
        return;
    }

    eprintln!("Compiling ({profile} profile)...");
    run_cargo_or_exit(build_cargo_command(debug, embed, package, bin, features));

    // Resolve the selected package's directory before fingerprinting so that
    // when --bin selects a member of a workspace without -p, we fingerprint
    // that member's static/ tree rather than the workspace root's.
    let (binary, manifest_dir, resolved_pkg) = find_binary(debug, package, bin);

    // Release builds fingerprint *after* the compile (the runtime reads the
    // manifest from disk, so order doesn't matter, and the static renderer below
    // then resolves the new hashed URLs).
    if !debug {
        eprintln!("\nFingerprinting static assets...");
        let static_dir = manifest_dir.as_deref().map_or_else(
            || std::path::Path::new("static").to_owned(),
            |d| d.join("static"),
        );
        fingerprint_assets_in(&static_dir);
    }
    eprintln!("\nRunning static renderer...\n");

    let mut cmd = Command::new(&binary);
    cmd.env("AUTUMN_BUILD_STATIC", "1");
    apply_renderer_env(
        &mut cmd,
        debug,
        package,
        bin,
        resolved_pkg.as_deref(),
        manifest_dir.as_ref(),
    );
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
        std::process::exit(1);
    });

    if !status.success() {
        eprintln!("\n\u{2717} Static build failed");
        std::process::exit(1);
    }

    eprintln!("\n\u{1F342} Build complete!");
}

/// Core fingerprinting implementation.
///
/// Accepts an explicit `static_dir` so both production code (which passes
/// `Path::new("static")` relative to CWD) and tests (which pass an absolute
/// temp-dir path) can exercise the same logic without changing the process CWD.
///
/// For each file `<static_dir>/css/autumn.css` the function:
/// 1. Computes the SHA-256 digest of its contents.
/// 2. Truncates the digest to 8 lowercase hex characters.
/// 3. Writes a copy named `<static_dir>/css/autumn.<hash8>.css`.
/// 4. Records `"css/autumn.css" -> "css/autumn.<hash8>.css"` in the manifest.
///
/// Existing fingerprinted copies recorded in the previous manifest are removed
/// first so stale hashes don't accumulate across builds.
fn fingerprint_assets_in(static_dir: &Path) {
    if !static_dir.exists() {
        return;
    }

    // Remove only the fingerprinted copies recorded in the previous manifest
    // so we never accidentally delete user-authored assets whose names happen
    // to match the `<stem>.<8hex>.<ext>` pattern (e.g. vendor.deadbeef.js).
    remove_previous_fingerprints(static_dir);

    let mut manifest_files: HashMap<String, String> = HashMap::new();
    collect_and_fingerprint(static_dir, static_dir, &mut manifest_files);

    let manifest = serde_json::json!({
        "version": "1",
        "files": manifest_files,
    });

    let manifest_path = static_dir.join(".autumn-manifest.json");
    match serde_json::to_string_pretty(&manifest) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&manifest_path, json) {
                eprintln!("\u{2717} Failed to write asset manifest: {e}");
            } else {
                eprintln!(
                    "  \u{2713} Fingerprinted {} asset(s) \u{2192} {}",
                    manifest_files.len(),
                    manifest_path.display()
                );
            }
        }
        Err(e) => eprintln!("\u{2717} Failed to serialize asset manifest: {e}"),
    }
}

/// Walk `dir` recursively, hash each regular file, write a fingerprinted copy,
/// and record the mapping in `out`.
fn collect_and_fingerprint(root: &Path, dir: &Path, out: &mut HashMap<String, String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("  \u{26A0} Could not read {}: {e}", dir.display());
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        // Skip hidden files (the manifest itself, .DS_Store, etc.).
        if name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            collect_and_fingerprint(root, &path, out);
            continue;
        }

        // Skip files that already look fingerprinted (safety guard).
        if is_fingerprinted_filename(&name_str) {
            continue;
        }

        let contents = match std::fs::read(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  \u{26A0} Could not read {}: {e}", path.display());
                continue;
            }
        };

        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(&contents);
            let result = hasher.finalize();
            hex::encode(&result[..4]) // 4 bytes = 8 hex chars
        };

        // Build the fingerprinted filename: stem + hash + extension.
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = path
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fp_name = if ext.is_empty() {
            format!("{stem}.{hash}")
        } else {
            format!("{stem}.{hash}.{ext}")
        };
        let fp_path = path.with_file_name(&fp_name);

        if let Err(e) = std::fs::write(&fp_path, &contents) {
            eprintln!("  \u{26A0} Could not write {}: {e}", fp_path.display());
            continue;
        }

        // Record logical path -> fingerprinted path (both relative to static/).
        if let (Ok(logical), Ok(fingerprinted)) =
            (path.strip_prefix(root), fp_path.strip_prefix(root))
        {
            out.insert(
                logical.to_string_lossy().replace('\\', "/"),
                fingerprinted.to_string_lossy().replace('\\', "/"),
            );
        }
    }
}

/// Delete only the fingerprinted copies that were written by the previous
/// build, identified by the values listed in `static/.autumn-manifest.json`.
///
/// This avoids accidentally removing user-authored assets whose filenames
/// happen to match the `<stem>.<8hex>.<ext>` pattern (e.g. `vendor.deadbeef.js`).
fn remove_previous_fingerprints(static_dir: &Path) {
    let manifest_path = static_dir.join(".autumn-manifest.json");
    let Ok(contents) = std::fs::read_to_string(&manifest_path) else {
        return; // No previous manifest — nothing to clean up.
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return;
    };
    let Some(files) = manifest["files"].as_object() else {
        return;
    };
    for fingerprinted_rel in files.values() {
        if let Some(rel) = fingerprinted_rel.as_str() {
            // Reject any path that tries to escape the static directory.
            // The manifest is written by this tool and should never contain
            // traversal components, but guard against tampered manifests.
            if rel.contains("..") || Path::new(rel).is_absolute() {
                continue;
            }
            let fp_path = static_dir.join(rel);
            if fp_path.exists() {
                let _ = std::fs::remove_file(&fp_path);
            }
        }
    }
}

/// Returns `true` when `filename` matches either fingerprinted pattern:
/// - `<stem>.<8hex>.<ext>` for files with an extension
/// - `<stem>.<8hex>` for extensionless files (e.g. `CNAME`)
fn is_fingerprinted_filename(filename: &str) -> bool {
    let parts: Vec<&str> = filename.split('.').collect();
    let hash_candidate = match parts.len() {
        n if n >= 3 => parts[n - 2],
        2 => parts[1],
        _ => return false,
    };
    hash_candidate.len() == 8
        && hash_candidate
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Locate the compiled binary and its package manifest directory.
///
/// When `package` is `Some`, matches by package name directly.
/// Otherwise falls back to matching the package whose manifest is in
/// the current directory.
///
/// When `bin` is `Some`, it is used as the binary name directly instead of
/// resolving via `default-run` or target scanning — mirrors the `--bin` flag
/// passed to `cargo build`.
///
/// Returns `(binary_path, manifest_dir)` so the caller can set
/// `AUTUMN_MANIFEST_DIR` when the binary is run from a different CWD
/// (e.g. `autumn build -p reddit-clone` from the workspace root).
fn find_binary(
    debug: bool,
    package: Option<&str>,
    bin: Option<&str>,
) -> (PathBuf, Option<PathBuf>, Option<String>) {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to run cargo metadata");

    if !output.status.success() {
        eprintln!("\u{2717} Failed to read cargo metadata");
        std::process::exit(1);
    }

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    let cwd = std::env::current_dir().expect("current dir");

    resolve_binary_from_metadata(&metadata, debug, package, bin, &cwd).unwrap_or_else(|error| {
        eprintln!("\u{2717} {error}");
        std::process::exit(1);
    })
}

/// Return `true` when `pkg`'s target list contains a binary named `bin_name`.
fn pkg_owns_bin(pkg: &serde_json::Value, bin_name: &str) -> bool {
    pkg["targets"].as_array().is_some_and(|ts| {
        ts.iter().any(|t| {
            t["name"].as_str() == Some(bin_name)
                && t["kind"]
                    .as_array()
                    .is_some_and(|ks| ks.iter().any(|k| k == "bin"))
        })
    })
}

fn resolve_binary_from_metadata(
    metadata: &serde_json::Value,
    debug: bool,
    package: Option<&str>,
    bin: Option<&str>,
    cwd: &Path,
) -> Result<(PathBuf, Option<PathBuf>, Option<String>), String> {
    let target_dir = metadata["target_directory"]
        .as_str()
        .ok_or("target_directory missing from cargo metadata")?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("packages missing from cargo metadata")?;

    let matching_packages: Vec<_> = package.map_or_else(
        || {
            packages
                .iter()
                .filter(|pkg| {
                    pkg["manifest_path"]
                        .as_str()
                        .and_then(|manifest| Path::new(manifest).parent())
                        .is_some_and(|dir| dir.starts_with(cwd))
                })
                .collect()
        },
        |pkg_name| {
            packages
                .iter()
                .filter(|pkg| pkg["name"].as_str() == Some(pkg_name))
                .collect()
        },
    );

    // Guard: when --bin is given without -p, reject the request if more than one
    // package in the workspace owns a binary with that name.  Cargo would build
    // all matching targets and emit an output-filename-collision warning; we must
    // error here so the caller gets a clear message rather than silently using
    // the first match's manifest_dir with the last-written binary on disk.
    if let (Some(explicit), None) = (bin, package) {
        let owners: Vec<&str> = matching_packages
            .iter()
            .filter(|pkg| pkg_owns_bin(pkg, explicit))
            .filter_map(|pkg| pkg["name"].as_str())
            .collect();
        if owners.len() > 1 {
            return Err(format!(
                "binary target '{explicit}' is defined in multiple packages: {}; \
                 use -p <package> to select one",
                owners.join(", ")
            ));
        }
    }

    let (bin_name, manifest_dir, resolved_pkg_name) = matching_packages
        .iter()
        .find_map(|pkg| {
            // --bin wins when the caller already knows which target to run.
            // Otherwise prefer `default-run` so packages with multiple binaries
            // always start the right one. Mirror the same logic as `dev.rs`.
            let name = if let Some(explicit) = bin {
                // Only accept this package when it actually owns the requested
                // binary target — without this guard, a workspace with multiple
                // members matching the CWD filter would always pick the first
                // member regardless of which one owns the bin, giving the wrong
                // manifest_dir for fingerprinting and static rendering.
                if !pkg_owns_bin(pkg, explicit) {
                    return None;
                }
                explicit.to_owned()
            } else if let Some(name) = pkg["default_run"].as_str() {
                name.to_owned()
            } else {
                pkg["targets"].as_array()?.iter().find_map(|t| {
                    let is_bin = t["kind"].as_array()?.iter().any(|k| k == "bin");
                    if is_bin {
                        t["name"].as_str().map(String::from)
                    } else {
                        None
                    }
                })?
            };
            let dir = pkg["manifest_path"]
                .as_str()
                .and_then(|p| Path::new(p).parent().map(PathBuf::from));
            let pkg_name = pkg["name"].as_str().map(ToOwned::to_owned);
            Some((name, dir, pkg_name))
        })
        .ok_or_else(|| {
            bin.map_or_else(
                || {
                    package.map_or_else(
                        || "no binary target found in current package".to_owned(),
                        |pkg_name| format!("no binary target found in package '{pkg_name}'"),
                    )
                },
                |explicit| {
                    package.map_or_else(
                        || format!("no binary target '{explicit}' found in current package"),
                        |pkg_name| {
                            format!("no binary target '{explicit}' found in package '{pkg_name}'")
                        },
                    )
                },
            )
        })?;

    let profile_dir = if debug { "debug" } else { "release" };
    let mut path = PathBuf::from(target_dir);
    path.push(profile_dir);
    path.push(bin_name);

    if cfg!(windows) {
        path.set_extension("exe");
    }

    Ok((path, manifest_dir, resolved_pkg_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_args(
        debug: bool,
        embed: bool,
        package: Option<&str>,
        bin: Option<&str>,
        extra_features: Option<&str>,
    ) -> Vec<String> {
        build_cargo_command(debug, embed, package, bin, extra_features)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn embed_build_enables_feature_in_release() {
        let args = cargo_args(false, true, None, None, None);
        assert!(args.contains(&"--release".to_string()));
        assert!(
            args.windows(2).any(|w| w == ["--features", "embed-assets"]),
            "embed build must enable the embed-assets feature: {args:?}"
        );
    }

    #[test]
    fn non_embed_build_omits_embed_feature() {
        let args = cargo_args(false, false, Some("blog"), None, None);
        assert!(
            !args.iter().any(|a| a.contains("embed-assets")),
            "non-embed build must not enable embed-assets: {args:?}"
        );
        assert!(args.windows(2).any(|w| w == ["-p", "blog"]));
    }

    #[test]
    fn extra_features_forwarded_to_cargo() {
        // Non-embed: only the extra feature is added.
        let args = cargo_args(
            false,
            false,
            None,
            None,
            Some("autumn-web/managed-pg-bundled"),
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--features", "autumn-web/managed-pg-bundled"]),
            "extra_features must be forwarded when embed is false: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains("embed-assets")),
            "embed-assets must not appear in non-embed build: {args:?}"
        );
    }

    #[test]
    fn extra_features_combined_with_embed_assets() {
        // Embed: extra feature is combined with embed-assets in one --features flag.
        let args = cargo_args(
            false,
            true,
            None,
            None,
            Some("autumn-web/managed-pg-bundled"),
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--features", "embed-assets,autumn-web/managed-pg-bundled"]),
            "embed + extra_features must produce a combined --features value: {args:?}"
        );
    }

    #[test]
    fn bin_arg_is_passed_to_cargo() {
        let args = cargo_args(false, true, Some("blog"), Some("blog-server"), None);
        assert!(
            args.windows(2).any(|w| w == ["--bin", "blog-server"]),
            "--bin must be forwarded to cargo: {args:?}"
        );
        assert!(args.windows(2).any(|w| w == ["-p", "blog"]));
    }

    fn expected_binary(path: &str) -> PathBuf {
        let mut p = PathBuf::from(path);
        if cfg!(windows) {
            p.set_extension("exe");
        }
        p
    }

    fn sample_metadata(target_dir: &str, pkg_name: &str, manifest_dir: &str) -> serde_json::Value {
        serde_json::json!({
            "target_directory": target_dir,
            "packages": [{
                "name": pkg_name,
                "manifest_path": format!("{manifest_dir}/Cargo.toml"),
                "targets": [{
                    "name": pkg_name,
                    "kind": ["bin"],
                    "src_path": format!("{manifest_dir}/src/main.rs")
                }]
            }]
        })
    }

    #[test]
    fn resolve_binary_by_package_name() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let (bin, manifest_dir, _) = resolve_binary_from_metadata(
            &metadata,
            true,
            Some("hello"),
            None,
            Path::new("/projects"),
        )
        .unwrap();
        assert_eq!(bin, expected_binary("/tmp/target/debug/hello"));
        assert_eq!(manifest_dir, Some(PathBuf::from("/projects/hello")));
    }

    #[test]
    fn resolve_binary_by_cwd() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let (bin, manifest_dir, _) =
            resolve_binary_from_metadata(&metadata, true, None, None, Path::new("/projects/hello"))
                .unwrap();
        assert_eq!(bin, expected_binary("/tmp/target/debug/hello"));
        assert_eq!(manifest_dir, Some(PathBuf::from("/projects/hello")));
    }

    #[test]
    fn resolve_binary_uses_release_profile() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let (bin, _, _) = resolve_binary_from_metadata(
            &metadata,
            false,
            Some("hello"),
            None,
            Path::new("/projects"),
        )
        .unwrap();
        assert_eq!(bin, expected_binary("/tmp/target/release/hello"));
    }

    #[test]
    fn resolve_binary_reports_missing_package() {
        let metadata = sample_metadata("/tmp/target", "hello", "/projects/hello");
        let result = resolve_binary_from_metadata(
            &metadata,
            true,
            Some("missing"),
            None,
            Path::new("/projects"),
        );
        assert!(result.unwrap_err().contains("package 'missing'"));
    }

    #[test]
    fn resolve_binary_reports_missing_targets() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "hello",
                "manifest_path": "/projects/hello/Cargo.toml",
                "targets": []
            }]
        });

        let result = resolve_binary_from_metadata(
            &metadata,
            true,
            Some("hello"),
            None,
            Path::new("/projects"),
        );
        assert!(result.unwrap_err().contains("package 'hello'"));
    }

    #[test]
    fn resolve_binary_prefers_default_run_over_first_target() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "todo-app",
                "manifest_path": "/projects/todo-app/Cargo.toml",
                "default_run": "todo-app",
                "targets": [
                    { "name": "seed", "kind": ["bin"], "src_path": "/projects/todo-app/src/bin/seed.rs" },
                    { "name": "todo-app", "kind": ["bin"], "src_path": "/projects/todo-app/src/main.rs" }
                ]
            }]
        });
        let (bin, _, _) = resolve_binary_from_metadata(
            &metadata,
            true,
            Some("todo-app"),
            None,
            Path::new("/projects"),
        )
        .unwrap();
        assert_eq!(bin, expected_binary("/tmp/target/debug/todo-app"));
    }

    #[test]
    fn resolve_binary_explicit_bin_overrides_default_run() {
        // --bin wins over default-run so the static renderer runs the requested target.
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "todo-app",
                "manifest_path": "/projects/todo-app/Cargo.toml",
                "default_run": "todo-app",
                "targets": [
                    { "name": "seed", "kind": ["bin"], "src_path": "/projects/todo-app/src/bin/seed.rs" },
                    { "name": "todo-app", "kind": ["bin"], "src_path": "/projects/todo-app/src/main.rs" }
                ]
            }]
        });
        let (bin, _, _) = resolve_binary_from_metadata(
            &metadata,
            true,
            Some("todo-app"),
            Some("seed"),
            Path::new("/projects"),
        )
        .unwrap();
        assert_eq!(bin, expected_binary("/tmp/target/debug/seed"));
    }

    #[test]
    fn resolve_binary_bin_picks_correct_workspace_member() {
        // Without -p, autumn build --bin web from a workspace root must pick the
        // member that actually owns bin "web", not just the first CWD-matching member.
        let metadata = serde_json::json!({
            "target_directory": "/workspace/target",
            "packages": [
                {
                    "name": "app-a",
                    "manifest_path": "/workspace/app-a/Cargo.toml",
                    "targets": [{ "name": "server", "kind": ["bin"], "src_path": "/workspace/app-a/src/main.rs" }]
                },
                {
                    "name": "app-b",
                    "manifest_path": "/workspace/app-b/Cargo.toml",
                    "targets": [{ "name": "web", "kind": ["bin"], "src_path": "/workspace/app-b/src/main.rs" }]
                }
            ]
        });
        // Both members are under the CWD (/workspace); --bin web belongs to app-b.
        let (bin, manifest_dir, _) = resolve_binary_from_metadata(
            &metadata,
            true,
            None,
            Some("web"),
            Path::new("/workspace"),
        )
        .unwrap();
        assert_eq!(bin, expected_binary("/workspace/target/debug/web"));
        assert_eq!(
            manifest_dir,
            Some(PathBuf::from("/workspace/app-b")),
            "--bin web must resolve to app-b's manifest_dir, not app-a's"
        );
    }

    #[test]
    fn resolve_binary_bin_ambiguous_across_workspace_members_errors() {
        // When two workspace members expose the same binary name and no -p is
        // given, cargo would produce an output-filename collision; autumn must
        // reject the request so the user is told to pass -p.
        let metadata = serde_json::json!({
            "target_directory": "/workspace/target",
            "packages": [
                {
                    "name": "app-a",
                    "manifest_path": "/workspace/app-a/Cargo.toml",
                    "targets": [{ "name": "web", "kind": ["bin"], "src_path": "/workspace/app-a/src/main.rs" }]
                },
                {
                    "name": "app-b",
                    "manifest_path": "/workspace/app-b/Cargo.toml",
                    "targets": [{ "name": "web", "kind": ["bin"], "src_path": "/workspace/app-b/src/main.rs" }]
                }
            ]
        });
        let result = resolve_binary_from_metadata(
            &metadata,
            false,
            None,
            Some("web"),
            Path::new("/workspace"),
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("web") && err.contains("app-a") && err.contains("app-b"),
            "error must name the binary and the conflicting packages so the user \
             knows to add -p; got: {err}"
        );
        assert!(
            err.contains("-p"),
            "error must suggest -p <package> to disambiguate; got: {err}"
        );
    }

    #[test]
    fn resolve_binary_bin_not_in_any_member_errors() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "app-a",
                "manifest_path": "/workspace/app-a/Cargo.toml",
                "targets": [{ "name": "server", "kind": ["bin"], "src_path": "/workspace/app-a/src/main.rs" }]
            }]
        });
        let result = resolve_binary_from_metadata(
            &metadata,
            true,
            None,
            Some("missing-bin"),
            Path::new("/workspace"),
        );
        assert!(
            result.unwrap_err().contains("missing-bin"),
            "error must name the missing binary target"
        );
    }

    #[test]
    fn resolve_binary_returns_manifest_dir_for_workspace_package() {
        let metadata = serde_json::json!({
            "target_directory": "/workspace/target",
            "packages": [{
                "name": "reddit-clone",
                "manifest_path": "/workspace/examples/reddit-clone/Cargo.toml",
                "targets": [{ "name": "reddit-clone", "kind": ["bin"], "src_path": "/workspace/examples/reddit-clone/src/main.rs" }]
            }]
        });
        // Simulates: `autumn build -p reddit-clone` from workspace root
        let (bin, manifest_dir, _) = resolve_binary_from_metadata(
            &metadata,
            false,
            Some("reddit-clone"),
            None,
            Path::new("/workspace"),
        )
        .unwrap();
        assert_eq!(
            bin,
            expected_binary("/workspace/target/release/reddit-clone")
        );
        assert_eq!(
            manifest_dir,
            Some(PathBuf::from("/workspace/examples/reddit-clone"))
        );
    }

    #[test]
    fn resolve_binary_returns_pkg_name_for_bin_without_package() {
        // When --bin selects a workspace member without -p, the resolved package name
        // must be returned so managed_pg_env can namespace to the member rather than
        // the workspace root's CWD-derived identity.
        let metadata = serde_json::json!({
            "target_directory": "/workspace/target",
            "packages": [
                {
                    "name": "api",
                    "manifest_path": "/workspace/api/Cargo.toml",
                    "targets": [{ "name": "api-server", "kind": ["bin"], "src_path": "/workspace/api/src/main.rs" }]
                },
                {
                    "name": "web",
                    "manifest_path": "/workspace/web/Cargo.toml",
                    "targets": [{ "name": "web-server", "kind": ["bin"], "src_path": "/workspace/web/src/main.rs" }]
                }
            ]
        });
        let (bin, manifest_dir, resolved_pkg) = resolve_binary_from_metadata(
            &metadata,
            false,
            None,
            Some("api-server"),
            Path::new("/workspace"),
        )
        .unwrap();
        assert_eq!(bin, expected_binary("/workspace/target/release/api-server"));
        assert_eq!(manifest_dir, Some(PathBuf::from("/workspace/api")));
        assert_eq!(
            resolved_pkg,
            Some("api".to_owned()),
            "--bin without -p must return the resolved package name for managed-PG namespacing"
        );
    }

    #[test]
    fn fingerprint_detection_positive() {
        assert!(is_fingerprinted_filename("autumn.a1b2c3d4.css"));
        assert!(is_fingerprinted_filename("app.00000000.js"));
        assert!(is_fingerprinted_filename("logo.deadbeef.png"));
        // extensionless fingerprinted files (e.g. CNAME -> CNAME.<hash>)
        assert!(is_fingerprinted_filename("CNAME.a1b2c3d4"));
        assert!(is_fingerprinted_filename("robots.deadbeef"));
    }

    #[test]
    fn fingerprint_detection_negative() {
        assert!(!is_fingerprinted_filename("autumn.css"));
        assert!(!is_fingerprinted_filename("htmx.min.js"));
        // hash too short
        assert!(!is_fingerprinted_filename("autumn.abc.css"));
        // hash too long
        assert!(!is_fingerprinted_filename("autumn.a1b2c3d4e5.css"));
        // uppercase hex not accepted
        assert!(!is_fingerprinted_filename("autumn.A1B2C3D4.css"));
        // non-hex chars
        assert!(!is_fingerprinted_filename("autumn.zzzzzzzz.css"));
        // bare name with no dot
        assert!(!is_fingerprinted_filename("CNAME"));
    }

    #[test]
    fn run_cargo_or_exit_succeeds_on_zero_exit() {
        // `cargo --version` is always available and always exits 0.
        let mut cmd = Command::new("cargo");
        cmd.arg("--version");
        run_cargo_or_exit(cmd);
    }

    #[test]
    fn apply_renderer_env_removes_pg_attach_url_unconditionally() {
        let mut cmd = Command::new("echo");
        apply_renderer_env(&mut cmd, false, None, None, None, None);
        let removed = cmd.get_envs().any(|(k, v)| {
            k.to_str() == Some(crate::serve::MANAGED_PG_ATTACH_URL_ENV) && v.is_none()
        });
        assert!(
            removed,
            "MANAGED_PG_ATTACH_URL must be removed from env regardless of managed-PG state"
        );
    }

    #[test]
    fn apply_renderer_env_sets_autumn_profile_when_absent() {
        if std::env::var("AUTUMN_PROFILE").is_ok() {
            // Parent env already has it; helper intentionally won't override it.
            return;
        }
        let find_profile = |cmd: &Command| -> Option<String> {
            cmd.get_envs().find_map(|(k, v)| {
                if k.to_str() == Some("AUTUMN_PROFILE") {
                    v.and_then(|s| s.to_str().map(str::to_owned))
                } else {
                    None
                }
            })
        };
        let mut cmd_debug = Command::new("echo");
        apply_renderer_env(&mut cmd_debug, true, None, None, None, None);
        assert_eq!(
            find_profile(&cmd_debug).as_deref(),
            Some("dev"),
            "debug=true must set AUTUMN_PROFILE=dev when not already in env"
        );

        let mut cmd_release = Command::new("echo");
        apply_renderer_env(&mut cmd_release, false, None, None, None, None);
        assert_eq!(
            find_profile(&cmd_release).as_deref(),
            Some("prod"),
            "debug=false must set AUTUMN_PROFILE=prod when not already in env"
        );
    }

    #[test]
    fn apply_renderer_env_pins_current_dir_when_manifest_differs_from_cwd() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let manifest_dir = tmp.path().to_path_buf();
        let cwd = std::env::current_dir().unwrap();
        if manifest_dir == cwd {
            return; // extremely unlikely, but guard anyway
        }
        let mut cmd = Command::new("echo");
        apply_renderer_env(&mut cmd, false, None, None, None, Some(&manifest_dir));
        assert_eq!(
            cmd.get_current_dir(),
            Some(manifest_dir.as_path()),
            "current_dir must be pinned to manifest_dir when it differs from cwd"
        );
    }

    #[test]
    fn apply_renderer_env_effective_pkg_prefers_package_over_resolved() {
        // When both --package and --bin are given, effective_pkg uses --package.
        // (Verifies the or_else short-circuit without touching managed-PG state.)
        let mut cmd = Command::new("echo");
        // Just verify the function doesn't panic with both args supplied.
        apply_renderer_env(
            &mut cmd,
            false,
            Some("my-pkg"),
            Some("my-bin"),
            Some("resolved"),
            None,
        );
        let removed = cmd.get_envs().any(|(k, v)| {
            k.to_str() == Some(crate::serve::MANAGED_PG_ATTACH_URL_ENV) && v.is_none()
        });
        assert!(removed);
    }

    #[test]
    fn fingerprint_static_assets_writes_manifest_and_copies() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let static_dir = tmp.path().join("static");
        let css_dir = static_dir.join("css");
        std::fs::create_dir_all(&css_dir).unwrap();

        let css_content = b"body { color: red; }";
        std::fs::write(css_dir.join("autumn.css"), css_content).unwrap();

        // Call the inner function directly with an absolute path so the test
        // never touches the process-global CWD (which is racy on all platforms
        // and causes failures on Windows where CWD is a per-process lock).
        fingerprint_assets_in(&static_dir);

        // Manifest must exist.
        let manifest_path = static_dir.join(".autumn-manifest.json");
        assert!(manifest_path.exists(), "manifest must be written");

        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();

        let files = manifest["files"].as_object().unwrap();
        assert_eq!(files.len(), 1, "one asset fingerprinted");

        let fp = files["css/autumn.css"].as_str().unwrap();
        assert!(
            fp.starts_with("css/autumn."),
            "fingerprinted path has correct prefix"
        );
        assert!(
            std::path::Path::new(fp)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("css")),
            "fingerprinted path has correct extension"
        );

        // The fingerprinted copy must exist.
        assert!(
            static_dir.join(fp).exists(),
            "fingerprinted copy must be written: {fp}"
        );
    }
}
