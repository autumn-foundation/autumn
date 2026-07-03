//! `autumn routes` -- list mounted routes without booting the dev server.
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_DUMP_ROUTES=1`, and parses the JSON route listing from its
//! stdout. Applies any user-requested filters, then displays the result
//! as either a human-readable table or machine-readable JSON.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::text_width::display_width;

/// Output format for `autumn routes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unknown format '{other}'; expected 'table' or 'json'"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub method: String,
    pub path: String,
    pub handler: String,
    pub source: String,
    pub middleware: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sunset_opt_out: Option<bool>,
}

/// Options controlling `autumn routes` behaviour.
pub struct RoutesOptions<'a> {
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    pub format: OutputFormat,
    pub filter: Option<&'a str>,
    pub methods: &'a [String],
    pub user_only: bool,
}

/// Run `autumn routes`.
pub fn run(opts: &RoutesOptions<'_>) {
    eprintln!("\u{1F342} autumn routes\n");
    compile_binary(opts.package, opts.bin);
    let binary = find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping routes",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut routes: Vec<RouteInfo> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        eprintln!("\u{2717} Failed to parse route listing JSON: {e}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    // Apply filters and sort
    routes = apply_filters(routes, opts.filter, opts.methods, opts.user_only);
    sort_routes(&mut routes);

    match &opts.format {
        OutputFormat::Table => print_table(&routes),
        OutputFormat::Json => print_json(&routes),
    }
}

/// Filter routes by path prefix, HTTP methods, and/or source.
pub fn apply_filters(
    routes: Vec<RouteInfo>,
    filter: Option<&str>,
    methods: &[String],
    user_only: bool,
) -> Vec<RouteInfo> {
    routes
        .into_iter()
        .filter(|r| {
            if filter.is_some_and(|prefix| !r.path.starts_with(prefix)) {
                return false;
            }
            if !methods.is_empty() && !methods.iter().any(|m| m.eq_ignore_ascii_case(&r.method)) {
                return false;
            }
            if user_only && r.source == "framework" {
                return false;
            }
            true
        })
        .collect()
}

/// Sort routes by path (lexicographic) then method (lexicographic).
pub fn sort_routes(routes: &mut [RouteInfo]) {
    routes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.method.cmp(&b.method)));
}

/// Print routes as a human-readable aligned table.
pub fn print_table(routes: &[RouteInfo]) {
    if routes.is_empty() {
        println!("No routes found.");
        return;
    }

    let table = format_table(routes);
    print!("{table}");
}

/// Build the table string (extracted for testability).
pub fn format_table(routes: &[RouteInfo]) -> String {
    const HEADERS: [&str; 7] = [
        "Method",
        "Path",
        "Handler",
        "Version",
        "Status",
        "Source",
        "Middleware",
    ];

    // Compute column widths
    let widths = compute_column_widths(routes, &HEADERS);

    let mut out = String::new();

    // Header row
    out.push_str(&format_row(
        &HEADERS.map(std::borrow::ToOwned::to_owned),
        &widths,
    ));
    out.push('\n');

    // Separator row
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&"-".repeat(w));
    }
    out.push('\n');

    // Data rows
    for route in routes {
        let middleware = if route.middleware.is_empty() {
            String::new()
        } else {
            route.middleware.join(", ")
        };
        let version = route.api_version.as_deref().unwrap_or("-");
        let status = route.status.as_deref().unwrap_or("-");
        let cells = [
            route.method.clone(),
            route.path.clone(),
            route.handler.clone(),
            version.to_string(),
            status.to_string(),
            route.source.clone(),
            middleware,
        ];
        out.push_str(&format_row(&cells, &widths));
        out.push('\n');
    }

    out
}

fn compute_column_widths(routes: &[RouteInfo], headers: &[&str; 7]) -> [usize; 7] {
    let mut widths = [0usize; 7];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = display_width(h);
    }
    for route in routes {
        let middleware = if route.middleware.is_empty() {
            String::new()
        } else {
            route.middleware.join(", ")
        };
        let version = route.api_version.as_deref().unwrap_or("-");
        let status = route.status.as_deref().unwrap_or("-");
        let cols = [
            display_width(&route.method),
            display_width(&route.path),
            display_width(&route.handler),
            display_width(version),
            display_width(status),
            display_width(&route.source),
            display_width(&middleware),
        ];
        for (i, &w) in cols.iter().enumerate() {
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }
    widths
}

fn format_row(cells: &[String; 7], widths: &[usize; 7]) -> String {
    cells
        .iter()
        .zip(widths.iter())
        .enumerate()
        .map(|(i, (cell, &w))| {
            if i == 0 {
                format!("{cell:<w$}")
            } else {
                format!("  {cell:<w$}")
            }
        })
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// Print routes as pretty JSON.
pub fn print_json(routes: &[RouteInfo]) {
    let json =
        serde_json::to_string_pretty(routes).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"));
    println!("{json}");
}

// ── Binary discovery (mirrored from build.rs) ──────────────────────────────

pub fn find_binary(package: Option<&str>, bin: Option<&str>) -> PathBuf {
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

    resolve_binary_from_metadata(&metadata, package, &cwd, bin).unwrap_or_else(|error| {
        eprintln!("\u{2717} {error}");
        std::process::exit(1);
    })
}

fn resolve_binary_from_metadata(
    metadata: &serde_json::Value,
    package: Option<&str>,
    cwd: &Path,
    bin: Option<&str>,
) -> Result<PathBuf, String> {
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
                        .is_some_and(|dir| cwd.starts_with(dir) || dir.starts_with(cwd))
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

    // Collect (package-name, binary-name) pairs for every matching package
    // that has at least one `bin` target.  When --bin is given, pick that
    // specific target; otherwise error if a package exposes more than one.
    let mut candidates: Vec<(String, String)> = Vec::new();
    for pkg in &matching_packages {
        let pkg_name = match pkg["name"].as_str() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let bins: Vec<String> = pkg["targets"]
            .as_array()
            .map(|targets| {
                targets
                    .iter()
                    .filter_map(|t| {
                        let is_bin = t["kind"].as_array()?.iter().any(|k| k == "bin");
                        if is_bin {
                            t["name"].as_str().map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let chosen = if let Some(bin_name) = bin {
            if bins.iter().any(|b| b == bin_name) {
                Some(bin_name.to_owned())
            } else if !bins.is_empty() {
                return Err(format!(
                    "package '{pkg_name}' has no binary named '{bin_name}' \
                     (available: {})",
                    bins.join(", ")
                ));
            } else {
                None
            }
        } else if bins.len() > 1 {
            return Err(format!(
                "package '{pkg_name}' has multiple binary targets ({}); \
                 use --bin to select one",
                bins.join(", ")
            ));
        } else {
            bins.into_iter().next()
        };

        if let Some(b) = chosen {
            candidates.push((pkg_name, b));
        }
    }

    // When no explicit --package was given and multiple workspace members each
    // have a binary, we can't pick one safely — ask the user to be explicit.
    if package.is_none() && candidates.len() > 1 {
        let names: Vec<&str> = candidates.iter().map(|(n, _)| n.as_str()).collect();
        return Err(format!(
            "multiple binary packages found in workspace ({}); \
             use -p / --package to select one",
            names.join(", ")
        ));
    }

    let bin_name = candidates.pop().map(|(_, b)| b).ok_or_else(|| {
        package.map_or_else(
            || "no binary target found in current package".to_owned(),
            |pkg_name| format!("no binary target found in package '{pkg_name}'"),
        )
    })?;

    let mut path = PathBuf::from(target_dir);
    path.push("debug");
    path.push(bin_name);

    if cfg!(windows) {
        path.set_extension("exe");
    }

    Ok(path)
}

// ── Also compile the binary before running ─────────────────────────────────

pub fn compile_binary(package: Option<&str>, bin: Option<&str>) {
    let mut cargo = Command::new("cargo");
    cargo.arg("build");
    if let Some(pkg) = package {
        cargo.args(["-p", pkg]);
    }
    if let Some(b) = bin {
        cargo.args(["--bin", b]);
    }

    let status = cargo.status().expect("failed to run cargo build");
    if !status.success() {
        eprintln!("\u{2717} Compilation failed");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_route(method: &str, path: &str, source: &str) -> RouteInfo {
        RouteInfo {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: format!("{}_handler", path.trim_start_matches('/').replace('/', "_")),
            source: source.to_owned(),
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        }
    }

    fn sample_routes() -> Vec<RouteInfo> {
        vec![
            make_route("GET", "/posts", "user"),
            make_route("POST", "/posts", "user"),
            make_route("GET", "/posts/{id}", "user"),
            make_route("GET", "/actuator/health", "framework"),
            make_route("GET", "/about", "user"),
            make_route("GET", "/api/posts", "plugin:harvest"),
        ]
    }

    // ── OutputFormat parsing ───────────────────────────────────────────────

    #[test]
    fn parse_format_table() {
        let f: OutputFormat = "table".parse().unwrap();
        assert_eq!(f, OutputFormat::Table);
    }

    #[test]
    fn parse_format_json() {
        let f: OutputFormat = "json".parse().unwrap();
        assert_eq!(f, OutputFormat::Json);
    }

    #[test]
    fn parse_format_case_insensitive() {
        let f: OutputFormat = "JSON".parse().unwrap();
        assert_eq!(f, OutputFormat::Json);
        let f: OutputFormat = "Table".parse().unwrap();
        assert_eq!(f, OutputFormat::Table);
    }

    #[test]
    fn parse_format_unknown_is_error() {
        let result: Result<OutputFormat, _> = "xml".parse();
        assert!(result.is_err());
    }

    // ── apply_filters ──────────────────────────────────────────────────────

    #[test]
    fn filter_no_constraints_returns_all() {
        let routes = sample_routes();
        let count = routes.len();
        let result = apply_filters(routes, None, &[], false);
        assert_eq!(result.len(), count);
    }

    #[test]
    fn filter_by_path_prefix() {
        let routes = sample_routes();
        let result = apply_filters(routes, Some("/posts"), &[], false);
        assert!(result.iter().all(|r| r.path.starts_with("/posts")));
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn filter_path_prefix_exact_match() {
        let routes = sample_routes();
        let result = apply_filters(routes, Some("/about"), &[], false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/about");
    }

    #[test]
    fn filter_by_method_get() {
        let routes = sample_routes();
        let methods = vec!["GET".to_owned()];
        let result = apply_filters(routes, None, &methods, false);
        assert!(result.iter().all(|r| r.method == "GET"));
    }

    #[test]
    fn filter_by_method_post() {
        let routes = sample_routes();
        let methods = vec!["POST".to_owned()];
        let result = apply_filters(routes, None, &methods, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].method, "POST");
    }

    #[test]
    fn filter_by_multiple_methods() {
        let routes = sample_routes();
        let methods = vec!["GET".to_owned(), "POST".to_owned()];
        let result = apply_filters(routes, None, &methods, false);
        assert!(
            result
                .iter()
                .all(|r| r.method == "GET" || r.method == "POST")
        );
    }

    #[test]
    fn filter_method_case_insensitive() {
        let routes = sample_routes();
        let methods = vec!["get".to_owned()];
        let result = apply_filters(routes, None, &methods, false);
        assert!(!result.is_empty());
        assert!(result.iter().all(|r| r.method == "GET"));
    }

    #[test]
    fn filter_user_only_excludes_framework() {
        let routes = sample_routes();
        let result = apply_filters(routes, None, &[], true);
        assert!(result.iter().all(|r| r.source != "framework"));
    }

    #[test]
    fn filter_user_only_keeps_plugin_routes() {
        let routes = sample_routes();
        let result = apply_filters(routes, None, &[], true);
        assert!(
            result.iter().any(|r| r.source.starts_with("plugin:")),
            "plugin routes should be kept with --user-only"
        );
    }

    #[test]
    fn filter_combines_path_and_method() {
        let routes = sample_routes();
        let methods = vec!["GET".to_owned()];
        let result = apply_filters(routes, Some("/posts"), &methods, false);
        assert!(
            result
                .iter()
                .all(|r| r.path.starts_with("/posts") && r.method == "GET")
        );
        assert_eq!(result.len(), 2);
    }

    // ── sort_routes ────────────────────────────────────────────────────────

    #[test]
    fn sort_routes_by_path_then_method() {
        let mut routes = vec![
            make_route("POST", "/posts", "user"),
            make_route("GET", "/about", "user"),
            make_route("GET", "/posts", "user"),
        ];
        sort_routes(&mut routes);
        assert_eq!(routes[0].path, "/about");
        assert_eq!(routes[1].path, "/posts");
        assert_eq!(routes[1].method, "GET");
        assert_eq!(routes[2].path, "/posts");
        assert_eq!(routes[2].method, "POST");
    }

    // ── format_table ──────────────────────────────────────────────────────

    #[test]
    fn format_table_contains_headers() {
        let routes = vec![make_route("GET", "/posts", "user")];
        let table = format_table(&routes);
        assert!(table.contains("Method"), "missing Method header");
        assert!(table.contains("Path"), "missing Path header");
        assert!(table.contains("Handler"), "missing Handler header");
        assert!(table.contains("Source"), "missing Source header");
        assert!(table.contains("Middleware"), "missing Middleware header");
    }

    #[test]
    fn format_table_contains_route_data() {
        let routes = vec![make_route("GET", "/posts", "user")];
        let table = format_table(&routes);
        assert!(table.contains("GET"), "missing method");
        assert!(table.contains("/posts"), "missing path");
        assert!(table.contains("user"), "missing source");
    }

    #[test]
    fn format_table_has_separator_line() {
        let routes = vec![make_route("GET", "/", "user")];
        let table = format_table(&routes);
        assert!(table.contains("---"), "missing separator line");
    }

    #[test]
    fn format_table_empty_routes_still_has_headers() {
        let routes: Vec<RouteInfo> = vec![];
        let table = format_table(&routes);
        assert!(table.contains("Method"));
    }

    #[test]
    fn format_table_middleware_shown_when_present() {
        let route = RouteInfo {
            method: "GET".to_owned(),
            path: "/admin".to_owned(),
            handler: "admin".to_owned(),
            source: "user".to_owned(),
            middleware: vec!["secured".to_owned(), "cached(60s)".to_owned()],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        };
        let table = format_table(&[route]);
        assert!(table.contains("secured"), "missing middleware label");
        assert!(table.contains("cached(60s)"), "missing middleware label");
    }

    // ── print_json ─────────────────────────────────────────────────────────

    #[test]
    fn print_json_produces_valid_json() {
        let routes = sample_routes();
        let json_str = serde_json::to_string_pretty(&routes).unwrap();
        let parsed: Vec<RouteInfo> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.len(), routes.len());
    }

    // ── resolve_binary_from_metadata ──────────────────────────────────────

    #[test]
    fn resolve_binary_by_package_name() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "hello",
                "manifest_path": "/projects/hello/Cargo.toml",
                "targets": [{
                    "name": "hello",
                    "kind": ["bin"],
                    "src_path": "/projects/hello/src/main.rs"
                }]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, Some("hello"), Path::new("/projects"), None);
        let expected = if cfg!(windows) {
            PathBuf::from("/tmp/target/debug/hello.exe")
        } else {
            PathBuf::from("/tmp/target/debug/hello")
        };
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn resolve_binary_by_cwd() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "hello",
                "manifest_path": "/projects/hello/Cargo.toml",
                "targets": [{
                    "name": "hello",
                    "kind": ["bin"],
                    "src_path": "/projects/hello/src/main.rs"
                }]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, None, Path::new("/projects/hello"), None);
        let expected = if cfg!(windows) {
            PathBuf::from("/tmp/target/debug/hello.exe")
        } else {
            PathBuf::from("/tmp/target/debug/hello")
        };
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn resolve_binary_reports_missing_package() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "hello",
                "manifest_path": "/projects/hello/Cargo.toml",
                "targets": [{"name": "hello", "kind": ["bin"]}]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, Some("missing"), Path::new("/projects"), None);
        assert!(result.unwrap_err().contains("package 'missing'"));
    }

    #[test]
    fn resolve_binary_errors_on_multiple_workspace_candidates() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [
                {
                    "name": "alpha",
                    "manifest_path": "/ws/alpha/Cargo.toml",
                    "targets": [{"name": "alpha", "kind": ["bin"]}]
                },
                {
                    "name": "beta",
                    "manifest_path": "/ws/beta/Cargo.toml",
                    "targets": [{"name": "beta", "kind": ["bin"]}]
                }
            ]
        });
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/ws"), None);
        let err = result.unwrap_err();
        assert!(
            err.contains("multiple binary packages"),
            "expected ambiguity error, got: {err}"
        );
        assert!(
            err.contains("-p") || err.contains("--package"),
            "should hint at -p flag"
        );
    }

    #[test]
    fn resolve_binary_cwd_with_single_match_succeeds() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [
                {
                    "name": "alpha",
                    "manifest_path": "/ws/alpha/Cargo.toml",
                    "targets": [{"name": "alpha", "kind": ["bin"]}]
                },
                {
                    "name": "beta",
                    "manifest_path": "/ws/beta/Cargo.toml",
                    "targets": [{"name": "beta", "kind": ["bin"]}]
                }
            ]
        });
        // Narrowing cwd to /ws/alpha means only "alpha" matches.
        let result =
            resolve_binary_from_metadata(&metadata, None, Path::new("/ws/alpha"), None).unwrap();
        assert!(result.to_string_lossy().contains("alpha"));
    }

    #[test]
    fn resolve_binary_lib_only_package_is_skipped() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [
                {
                    "name": "mylib",
                    "manifest_path": "/ws/mylib/Cargo.toml",
                    "targets": [{"name": "mylib", "kind": ["lib"]}]
                },
                {
                    "name": "myapp",
                    "manifest_path": "/ws/myapp/Cargo.toml",
                    "targets": [{"name": "myapp", "kind": ["bin"]}]
                }
            ]
        });
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/ws"), None).unwrap();
        assert!(result.to_string_lossy().contains("myapp"));
    }

    #[test]
    fn resolve_binary_no_binary_target_errors() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "mylib",
                "manifest_path": "/ws/mylib/Cargo.toml",
                "targets": [{"name": "mylib", "kind": ["lib"]}]
            }]
        });
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/ws/mylib"), None);
        assert!(result.unwrap_err().contains("no binary target"));
    }

    #[test]
    fn resolve_binary_from_subdirectory_finds_parent_package() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "hello",
                "manifest_path": "/projects/hello/Cargo.toml",
                "targets": [{"name": "hello", "kind": ["bin"]}]
            }]
        });
        // Running from a subdirectory of the package root should still find it.
        let result =
            resolve_binary_from_metadata(&metadata, None, Path::new("/projects/hello/src"), None);
        assert!(
            result.unwrap().to_string_lossy().contains("hello"),
            "should resolve binary from subdirectory"
        );
    }

    #[test]
    fn resolve_binary_errors_on_multi_bin_package() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "myapp",
                "manifest_path": "/ws/myapp/Cargo.toml",
                "targets": [
                    {"name": "server", "kind": ["bin"]},
                    {"name": "migrate", "kind": ["bin"]}
                ]
            }]
        });
        let result = resolve_binary_from_metadata(&metadata, None, Path::new("/ws/myapp"), None);
        let err = result.unwrap_err();
        assert!(
            err.contains("multiple binary targets"),
            "expected multi-bin error, got: {err}"
        );
        assert!(
            err.contains("--bin"),
            "should hint at --bin flag, got: {err}"
        );
    }

    #[test]
    fn resolve_binary_bin_flag_selects_named_target() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "myapp",
                "manifest_path": "/ws/myapp/Cargo.toml",
                "targets": [
                    {"name": "server", "kind": ["bin"]},
                    {"name": "migrate", "kind": ["bin"]}
                ]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, None, Path::new("/ws/myapp"), Some("migrate"))
                .unwrap();
        assert!(
            result.to_string_lossy().contains("migrate"),
            "should resolve to the named binary"
        );
    }

    #[test]
    fn resolve_binary_bin_flag_wrong_name_errors() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target",
            "packages": [{
                "name": "myapp",
                "manifest_path": "/ws/myapp/Cargo.toml",
                "targets": [{"name": "server", "kind": ["bin"]}]
            }]
        });
        let result =
            resolve_binary_from_metadata(&metadata, None, Path::new("/ws/myapp"), Some("missing"));
        let err = result.unwrap_err();
        assert!(
            err.contains("no binary named 'missing'"),
            "expected named-binary error, got: {err}"
        );
        assert!(
            err.contains("server"),
            "should list available binaries, got: {err}"
        );
    }
}
