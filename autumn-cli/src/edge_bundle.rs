//! `autumn edge bundle` (ADR-0024): a deployable edge bundle.
//!
//! The bundle is the capsule, the web-standard JS host
//! (`autumn_edge::js::HOST`) and a generated entry module for one platform.
//! Nothing in it is a vendor SDK: the entry is the platform's handler
//! signature, and the config file is the platform's own. The platform's CLI
//! deploys it (`wrangler deploy` for Cloudflare).
//!
//! The entry, the host and the capsule are build outputs: every bundle
//! rewrites them. The config file is the author's: it is written once, so
//! a KV namespace id or a route added to it survives the next bundle.

use std::path::{Path, PathBuf};

use autumn_edge::host::EdgeArtifact;
use autumn_edge::js;

/// The platform a bundle is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum BundleTarget {
    /// A Cloudflare Worker: `worker.mjs` + `wrangler.toml`.
    Cloudflare,
}

/// Options for `autumn edge bundle`.
pub struct BundleOptions<'a> {
    pub target: BundleTarget,
    pub capsule: &'a str,
    pub origin: Option<&'a str>,
    pub out: &'a str,
    pub name: Option<&'a str>,
    pub response_headers: &'a [String],
    pub probe_path: &'a str,
    pub no_probe: bool,
}

/// What a bundle wrote.
#[derive(Debug)]
pub struct BundleReport {
    pub out: PathBuf,
    pub written: Vec<&'static str>,
    /// `false` when the config file already existed and was kept.
    pub config_written: bool,
    pub config_file: &'static str,
    pub capsule_bytes: usize,
}

/// The capsule's file name inside a bundle.
const CAPSULE_FILE: &str = "edge-capsule.wasm";
/// The generated Cloudflare entry module.
const WORKER_FILE: &str = "worker.mjs";
/// The Cloudflare config, written once.
const WRANGLER_FILE: &str = "wrangler.toml";
/// The KV binding name the generated entry reads.
pub const KV_BINDING: &str = "EDGE_KV";
/// The `[vars]` entry naming the origin.
pub const ORIGIN_VAR: &str = "ORIGIN_URL";
/// The `compatibility_date` a new `wrangler.toml` gets. A fixed, past date:
/// a date newer than the deploying `wrangler` is refused, and the bundle only
/// uses `fetch`, `Request`/`Response` and `WebAssembly`, which every date has.
const COMPATIBILITY_DATE: &str = "2026-09-01";

/// Run `autumn edge bundle`. Exits 1 on an error.
pub fn run(options: &BundleOptions<'_>) {
    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match bundle(options, &project) {
        Ok(report) => print_report(options, &report),
        Err(err) => {
            eprintln!("\u{2717} {err}");
            std::process::exit(1);
        }
    }
}

fn print_report(options: &BundleOptions<'_>, report: &BundleReport) {
    println!(
        "\u{1F342} Edge bundle ({}) in {} — capsule {} KB",
        match options.target {
            BundleTarget::Cloudflare => "cloudflare",
        },
        report.out.display(),
        report.capsule_bytes / 1024
    );
    for file in &report.written {
        println!("  wrote {file}");
    }
    if report.config_written {
        println!(
            "  wrote {} (yours to edit; later bundles keep it)",
            report.config_file
        );
    } else {
        println!(
            "  kept {} (delete it to have it generated again)",
            report.config_file
        );
        if options.origin.is_some() {
            println!(
                "  note: --origin was not applied; set {ORIGIN_VAR} in the kept {} instead",
                report.config_file
            );
        }
    }
    println!(
        "\nDeploy it:\n  cd {} && npx wrangler deploy\n\
         Bind a KV namespace as {KV_BINDING} in {WRANGLER_FILE} to give `needs(kv)` routes \
         their store.",
        report.out.display()
    );
}

/// Write the bundle under `project`. `Err` is a message for the author.
///
/// # Errors
///
/// An unreadable or invalid capsule, a capsule that imports something the JS
/// host does not provide, a bad `--origin` or `--response-header`, a missing
/// `--origin` for a new config, or an I/O failure.
pub fn bundle(options: &BundleOptions<'_>, project: &Path) -> Result<BundleReport, String> {
    let capsule_path = project.join(options.capsule);
    let wasm = std::fs::read(&capsule_path).map_err(|err| {
        format!(
            "cannot read the capsule `{}`: {err}\n  Run `autumn build` first, or give the path \
             with --capsule.",
            options.capsule
        )
    })?;
    let artifact = EdgeArtifact::from_bytes(&wasm)
        .map_err(|err| format!("`{}` is not a capsule: {err}", options.capsule))?;
    let imports = artifact.imports();
    let missing = js::unprovided_imports(imports.iter().map(String::as_str));
    if !missing.is_empty() {
        return Err(format!(
            "`{}` imports what the edge host does not provide: {}\n  A capsule gets stdio, \
             the clock and entropy, and nothing else (no filesystem, no sockets). Remove the \
             code that needs it from the edge-safe module.",
            options.capsule,
            missing.join(", ")
        ));
    }

    let origin = options.origin.map(origin_base).transpose()?;
    let mut headers = Vec::new();
    for raw in options.response_headers {
        let (name, value) = crate::edge::parse_response_header(raw)
            .map_err(|err| format!("--response-header: {err}"))?;
        let value = value
            .to_str()
            .map_err(|_| format!("--response-header `{raw}`: the value must be visible ASCII"))?
            .to_owned();
        headers.push((name.as_str().to_owned(), value));
    }
    if !options.probe_path.starts_with('/') {
        return Err(format!(
            "--probe-path `{}` must start with `/`",
            options.probe_path
        ));
    }

    let out = project.join(options.out);
    std::fs::create_dir_all(&out)
        .map_err(|err| format!("cannot create `{}`: {err}", out.display()))?;
    let write = |file: &str, bytes: &[u8]| {
        std::fs::write(out.join(file), bytes)
            .map_err(|err| format!("cannot write `{}`: {err}", out.join(file).display()))
    };

    match options.target {
        BundleTarget::Cloudflare => {
            let config_path = out.join(WRANGLER_FILE);
            let config_written = !config_path.exists();
            if config_written {
                let origin = origin.as_deref().ok_or_else(|| {
                    format!(
                        "--origin is required for a new bundle: it becomes {ORIGIN_VAR} in \
                         `{}`",
                        config_path.display()
                    )
                })?;
                let name = match options.name {
                    Some(name) => worker_name(name)?,
                    None => default_worker_name(project),
                };
                write(WRANGLER_FILE, wrangler_toml(&name, origin).as_bytes())?;
            }
            write(CAPSULE_FILE, &wasm)?;
            write(js::HOST_FILE_NAME, js::HOST.as_bytes())?;
            write(
                WORKER_FILE,
                worker_module(&headers, !options.no_probe, options.probe_path).as_bytes(),
            )?;
            write(".gitignore", GITIGNORE.as_bytes())?;
            Ok(BundleReport {
                out,
                written: vec![CAPSULE_FILE, js::HOST_FILE_NAME, WORKER_FILE, ".gitignore"],
                config_written,
                config_file: WRANGLER_FILE,
                capsule_bytes: wasm.len(),
            })
        }
    }
}

/// The build outputs are regenerated; only the config is the author's.
const GITIGNORE: &str = "\
# Written by `autumn edge bundle`: regenerated on every bundle.
edge-capsule.wasm
autumn-edge-host.mjs
worker.mjs
.wrangler/
";

/// `https://origin.example.com` with no trailing `/`. `Err` names the rule.
fn origin_base(raw: &str) -> Result<String, String> {
    let (scheme, rest) = raw
        .strip_prefix("https://")
        .map(|rest| ("https://", rest))
        .or_else(|| raw.strip_prefix("http://").map(|rest| ("http://", rest)))
        .ok_or_else(|| format!("--origin `{raw}` must start with http:// or https://"))?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.starts_with('/') || rest.contains(['?', '#', ' ', '"', '\\']) {
        return Err(format!(
            "--origin `{raw}` must be a base URL: a host and an optional path, no query"
        ));
    }
    Ok(format!("{scheme}{rest}"))
}

/// A Worker name: lowercase letters, digits and `-`, at most 63, starting and
/// ending with a letter or digit.
fn worker_name(raw: &str) -> Result<String, String> {
    let valid = !raw.is_empty()
        && raw.len() <= 63
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !raw.starts_with('-')
        && !raw.ends_with('-');
    if valid {
        Ok(raw.to_owned())
    } else {
        Err(format!(
            "--name `{raw}` is not a Worker name: use lowercase letters, digits and `-` \
             (at most 63)"
        ))
    }
}

/// `<package>-edge` from `Cargo.toml`, made into a Worker name.
fn default_worker_name(project: &Path) -> String {
    let package = std::fs::read_to_string(project.join("Cargo.toml"))
        .ok()
        .and_then(|manifest| manifest.parse::<toml::Table>().ok())
        .and_then(|table| {
            table
                .get("package")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "autumn".to_owned());
    let mut name: String = package
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_owned();
    name.truncate(58);
    let name = name.trim_end_matches('-');
    if name.is_empty() {
        "autumn-edge".to_owned()
    } else {
        format!("{name}-edge")
    }
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn wrangler_toml(name: &str, origin: &str) -> String {
    format!(
        "# Written once by `autumn edge bundle --target cloudflare`; yours to edit.\n\
         # Later bundles rewrite the capsule, the host and worker.mjs, never this file.\n\
         name = {name}\n\
         main = \"{WORKER_FILE}\"\n\
         compatibility_date = \"{COMPATIBILITY_DATE}\"\n\
         \n\
         [vars]\n\
         # Every request the capsule declines is forwarded here, unchanged.\n\
         {ORIGIN_VAR} = {origin}\n\
         \n\
         # The `kv` capability. Without a binding, a `needs(kv)` route falls\n\
         # through to the origin. Create one with\n\
         #   npx wrangler kv namespace create {KV_BINDING}\n\
         # and put its id here. The origin publishes into it; the capsule reads.\n\
         # [[kv_namespaces]]\n\
         # binding = \"{KV_BINDING}\"\n\
         # id = \"<namespace id>\"\n",
        name = toml_string(name),
        origin = toml_string(origin),
    )
}

fn worker_module(headers: &[(String, String)], probe: bool, probe_path: &str) -> String {
    let headers = serde_json::to_string(
        &headers
            .iter()
            .map(|(name, value)| [name.as_str(), value.as_str()])
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned());
    let probe_path = serde_json::to_string(probe_path).unwrap_or_else(|_| "\"/\"".to_owned());
    format!(
        r#"// Written by `autumn edge bundle --target cloudflare` (ADR-0024). Every
// bundle rewrites this file: configure the Worker in wrangler.toml.
//
// The capsule serves the GET/HEAD requests it can; every other request goes
// to the origin unchanged. See docs/guide/edge.md.
import capsule from "./{CAPSULE_FILE}";
import {{ createEdgeGateway }} from "./{host}";

// `--response-header` values, set on every edge response.
const RESPONSE_HEADERS = {headers};

let gateway = null;

export default {{
  async fetch(request, env) {{
    if (!env.{ORIGIN_VAR}) {{
      return new Response("{ORIGIN_VAR} is not set in wrangler.toml\n", {{ status: 500 }});
    }}
    gateway ??= createEdgeGateway({{
      capsule,
      origin: env.{ORIGIN_VAR},
      kv: env.{KV_BINDING} ?? null,
      responseHeaders: RESPONSE_HEADERS,
      probeOrigin: {probe},
      probePath: {probe_path},
      clientIp: (r) => r.headers.get("cf-connecting-ip"),
    }});
    return gateway.fetch(request);
  }},
}};
"#,
        host = js::HOST_FILE_NAME,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_CAPSULE: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")))"#;

    fn project(wat: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"My_App\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("capsule.wasm"),
            wat::parse_str(wat).unwrap(),
        )
        .unwrap();
        dir
    }

    fn options<'a>(origin: Option<&'a str>, headers: &'a [String]) -> BundleOptions<'a> {
        BundleOptions {
            target: BundleTarget::Cloudflare,
            capsule: "capsule.wasm",
            origin,
            out: "edge/cloudflare",
            name: None,
            response_headers: headers,
            probe_path: "/",
            no_probe: false,
        }
    }

    #[test]
    fn a_bundle_holds_the_capsule_the_host_the_entry_and_a_config() {
        let dir = project(OK_CAPSULE);
        let headers = ["access-control-allow-origin: *".to_owned()];
        let report = bundle(
            &options(Some("https://origin.example.com/"), &headers),
            dir.path(),
        )
        .unwrap();
        assert!(report.config_written);
        let out = dir.path().join("edge/cloudflare");
        assert_eq!(
            std::fs::read(out.join("edge-capsule.wasm")).unwrap(),
            std::fs::read(dir.path().join("capsule.wasm")).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(out.join("autumn-edge-host.mjs")).unwrap(),
            js::HOST
        );
        let worker = std::fs::read_to_string(out.join("worker.mjs")).unwrap();
        assert!(worker.contains(r#"import capsule from "./edge-capsule.wasm";"#));
        assert!(
            worker.contains(r#"const RESPONSE_HEADERS = [["access-control-allow-origin","*"]];"#)
        );
        assert!(worker.contains("probeOrigin: true"));
        let config: toml::Table = std::fs::read_to_string(out.join("wrangler.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(config["name"].as_str(), Some("my-app-edge"));
        assert_eq!(config["main"].as_str(), Some("worker.mjs"));
        assert_eq!(
            config["vars"]["ORIGIN_URL"].as_str(),
            Some("https://origin.example.com")
        );
        let ignore = std::fs::read_to_string(out.join(".gitignore")).unwrap();
        assert!(!ignore.contains("wrangler.toml"));
    }

    #[test]
    fn a_later_bundle_keeps_the_config_and_rewrites_the_outputs() {
        let dir = project(OK_CAPSULE);
        bundle(&options(Some("https://a.example"), &[]), dir.path()).unwrap();
        let out = dir.path().join("edge/cloudflare");
        std::fs::write(out.join("wrangler.toml"), "# mine\n").unwrap();
        std::fs::write(out.join("worker.mjs"), "stale").unwrap();
        // No --origin needed once the config exists.
        let report = bundle(&options(None, &[]), dir.path()).unwrap();
        assert!(!report.config_written);
        assert_eq!(
            std::fs::read_to_string(out.join("wrangler.toml")).unwrap(),
            "# mine\n"
        );
        assert_ne!(
            std::fs::read_to_string(out.join("worker.mjs")).unwrap(),
            "stale"
        );
    }

    #[test]
    fn a_new_bundle_needs_an_origin() {
        let dir = project(OK_CAPSULE);
        let err = bundle(&options(None, &[]), dir.path()).unwrap_err();
        assert!(err.contains("--origin is required"), "{err}");
    }

    #[test]
    fn a_capsule_the_host_cannot_link_is_refused_by_name() {
        let dir = project(
            r#"(module
  (import "wasi_snapshot_preview1" "path_open" (func (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")))"#,
        );
        let err = bundle(&options(Some("https://o.example"), &[]), dir.path()).unwrap_err();
        assert!(err.contains("wasi_snapshot_preview1::path_open"), "{err}");
        assert!(!dir.path().join("edge/cloudflare").exists());
    }

    #[test]
    fn bad_inputs_are_named() {
        let dir = project(OK_CAPSULE);
        for (origin, expected) in [
            ("origin.example.com", "must start with http"),
            ("https://", "must be a base URL"),
            ("https://o.example/?x=1", "must be a base URL"),
        ] {
            let err = bundle(&options(Some(origin), &[]), dir.path()).unwrap_err();
            assert!(err.contains(expected), "{origin}: {err}");
        }
        let headers = ["set-cookie: a=b".to_owned()];
        let err = bundle(&options(Some("https://o.example"), &headers), dir.path()).unwrap_err();
        assert!(err.contains("set-cookie"), "{err}");
        let mut named = options(Some("https://o.example"), &[]);
        named.name = Some("Bad_Name");
        assert!(
            bundle(&named, dir.path())
                .unwrap_err()
                .contains("Worker name")
        );
        let mut missing = options(Some("https://o.example"), &[]);
        missing.capsule = "nope.wasm";
        assert!(
            bundle(&missing, dir.path())
                .unwrap_err()
                .contains("autumn build")
        );
    }

    #[test]
    fn worker_names_are_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(default_worker_name(dir.path()), "autumn-edge");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"__Edge.Greeting__\"\n",
        )
        .unwrap();
        assert_eq!(default_worker_name(dir.path()), "edge-greeting-edge");
        assert!(worker_name("ok-name-1").is_ok());
        for bad in ["", "-x", "x-", "A", &"a".repeat(64)] {
            assert!(worker_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn no_probe_turns_the_probe_off() {
        let worker = worker_module(&[], false, "/health");
        assert!(worker.contains("probeOrigin: false"));
        assert!(worker.contains(r#"probePath: "/health""#));
        assert!(worker.contains("const RESPONSE_HEADERS = [];"));
    }
}
