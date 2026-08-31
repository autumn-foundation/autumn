//! `autumn plugin package` / `autumn plugin inspect` — the consent surface.
//!
//! A sandboxed plugin arrives as opaque bytes. These two commands are what turn
//! that into a decision an operator can actually make:
//!
//! ```text
//! autumn plugin package --manifest plugin.toml --module target/…/plugin.wasm \
//!                       --out hello.autumn-plugin
//! autumn plugin inspect hello.autumn-plugin
//! ```
//!
//! `package` binds a manifest to a module and stamps the module's digest, so an
//! author cannot ship a manifest that describes different bytes than the ones
//! in the box. `inspect` is the consent screen: what the plugin may do, what it
//! may not, which bytes were reviewed, every host function the module imports —
//! and a real verdict, because it loads the module into **the same sandbox the
//! runtime uses** and runs the same conformance checks `autumn plugin-check`
//! runs against a native plugin.
//!
//! Both refuse rather than warn. A plugin that would fail at the operator's
//! boot should fail at the author's desk.

use std::path::Path;

use autumn_web::plugin_sandbox::{
    MAX_MANIFEST_BYTES, MAX_MODULE_BYTES, SandboxArtifact, SandboxHost, SandboxManifest,
    read_bounded,
};
use serde::Serialize;

use crate::plugin_check::{self, ConformanceReport, ReportFormat};
use crate::routes::RouteInfo;

/// Inputs to `autumn plugin package`.
pub struct PackageOptions<'a> {
    /// The authored manifest, as TOML.
    pub manifest: &'a Path,
    /// The `wasm32-wasip1` module the manifest describes.
    pub module: &'a Path,
    /// Where to write the `.autumn-plugin` artifact.
    pub out: &'a Path,
}

/// Bind a manifest to a module and write the artifact.
///
/// The module's digest is computed here and stamped into the manifest, so the
/// author never types it and can never get it wrong. The module is also loaded
/// into the sandbox before anything is written: an artifact that could not run
/// is not an artifact worth shipping.
///
/// # Errors
///
/// Returns a human-readable message when either input cannot be read, the
/// manifest does not validate, the payload is not a WebAssembly module, or the
/// module imports something the sandbox does not provide.
pub fn package(opts: &PackageOptions<'_>) -> Result<SandboxArtifact, String> {
    // Bounded reads, for the same reason the loader's are: a ceiling applied
    // after `fs::read` returns is a decision made after the damage. Packaging
    // takes both of its inputs from whoever ran the command, and a
    // multi-gigabyte one should be an error message rather than an OOM.
    let manifest_bytes = read_bounded(opts.manifest, MAX_MANIFEST_BYTES)
        .map_err(|err| format!("{}: {err}", opts.manifest.display()))?;
    let manifest_src = String::from_utf8(manifest_bytes)
        .map_err(|_| format!("{} is not valid UTF-8", opts.manifest.display()))?;
    let manifest = SandboxManifest::parse(&manifest_src).map_err(|err| err.to_string())?;
    let module = read_bounded(opts.module, MAX_MODULE_BYTES)
        .map_err(|err| format!("{}: {err}", opts.module.display()))?;

    let artifact = SandboxArtifact::seal(manifest, module).map_err(|err| err.to_string())?;
    // Load it before writing it. Refusing here costs the author a second;
    // refusing at the operator's boot costs them a deploy.
    SandboxHost::load(&artifact).map_err(|err| err.to_string())?;

    artifact
        .write_file(opts.out)
        .map_err(|err| err.to_string())?;
    Ok(artifact)
}

/// Read and verify an artifact from disk.
///
/// # Errors
///
/// Returns a human-readable message when the file cannot be read, is not an
/// Autumn plugin artifact, or does not match its own declared digest.
pub fn inspect(path: &Path) -> Result<SandboxArtifact, String> {
    SandboxArtifact::read_file(path).map_err(|err| err.to_string())
}

/// One declared route, as the JSON report renders it.
#[derive(Debug, Serialize)]
pub struct ReportRoute {
    /// HTTP method.
    pub method: String,
    /// Full mounted path.
    pub path: String,
}

/// The operator-facing verdict on an artifact.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// The prefix it mounts under.
    pub prefix: String,
    /// The module digest that was reviewed.
    pub sha256: String,
    /// Capabilities the manifest asks for.
    pub capabilities: Vec<String>,
    /// Classes of authority this build denies unconditionally.
    pub denied: Vec<String>,
    /// The routes it serves.
    pub routes: Vec<ReportRoute>,
    /// Every host function the module imports, or `None` when they could not
    /// be read at all.
    ///
    /// An empty list and an unreadable one are different facts, and collapsing
    /// them printed `(none)` — "this artifact asks for no host authority" — for
    /// a module whose shape was refused before its imports could be enumerated.
    /// That is the consent screen stating the opposite of the truth about
    /// exactly the artifacts whose authority matters most.
    pub imports: Option<Vec<String>>,
    /// Whether the module loads into this build's sandbox.
    pub loads: bool,
    /// Why it did not load, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
    /// The route-conformance report.
    pub conformance: ConformanceReport,
    /// The full consent summary, verbatim.
    pub consent: String,
}

/// Authority the sandbox denies unconditionally in this version.
///
/// Rendered alongside the grant because a consent screen that only lists what
/// was asked for tells a reader nothing about the shape of the "no".
const DENIED_CLASSES: &[&str] = &[
    "filesystem",
    "network",
    "environment",
    "database",
    "process-control",
];

/// Characters of artifact-supplied text this screen will render.
const EXCERPT: usize = 512;

/// Render text lifted out of an unaudited artifact, bounded and neutralised.
///
/// Both halves matter and neither substitutes for the other. The **escaping**
/// is why this screen can be trusted at all: a terminal escape in an import
/// name could rewrite the lines above it — the routes, the grant, the verdict —
/// which is an attack on the decision the screen exists to inform.
///
/// The **bound** is why rendering it is safe to attempt. `escape_debug()` on a
/// name that fills most of the 64 MiB module allowance expands every byte
/// before anything truncates the result, so `inspect` could exhaust memory on
/// the very artifact it is meant to refuse. Truncating during the expansion
/// rather than after it is the whole point: the long string is never built.
fn excerpt(text: &str) -> String {
    let mut out = String::new();
    for (kept, ch) in text.chars().enumerate() {
        if kept == EXCERPT {
            out.push_str(" … (truncated)");
            break;
        }
        out.extend(ch.escape_debug());
    }
    out
}

impl Report {
    /// Build the verdict for a verified artifact.
    #[must_use]
    pub fn of(artifact: &SandboxArtifact) -> Self {
        let manifest = artifact.manifest();
        // The import list is a property of the module, not of whether the
        // sandbox will run it — and it is *most* interesting when the sandbox
        // will not. Reporting "(none)" for a module that was refused for what
        // it imports would make the consent screen contradict itself.
        // Excerpted *here*, not where they are rendered. Both strings come out
        // of an artifact nobody has audited, and `to_json` serializes the
        // report's own fields — so a bound applied in `to_text` leaves the JSON
        // path carrying a name that fills most of the module allowance, which
        // `serde_json` then expands again escaping it. Capping at construction
        // is what makes every renderer safe rather than the one that remembered.
        let imports = SandboxHost::imports_of(artifact.module())
            .ok()
            .map(|imports| imports.iter().map(|import| excerpt(import)).collect());
        let (loads, load_error) = match SandboxHost::load(artifact) {
            Ok(_) => (true, None),
            Err(err) => (false, Some(excerpt(&err.to_string()))),
        };
        Self {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            prefix: manifest.prefix.clone(),
            sha256: manifest.sha256.clone(),
            capabilities: manifest
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            denied: DENIED_CLASSES.iter().map(|&name| name.to_owned()).collect(),
            routes: manifest
                .routes
                .iter()
                .map(|route| ReportRoute {
                    method: route.method.clone(),
                    path: route.path.clone(),
                })
                .collect(),
            imports,
            loads,
            load_error,
            conformance: conformance(manifest),
            consent: manifest.consent_summary(),
        }
    }

    /// Whether the artifact is fit to install.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.loads && self.conformance.passed()
    }

    /// Render the report for a human.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = self.consent.clone();
        out.push_str("  host functions it imports:\n");
        match self.imports.as_ref() {
            // Not the same as none, and saying so matters: the artifacts whose
            // imports cannot be read are the ones already refused for their
            // shape, which is when an operator most needs the screen not to
            // claim they ask for nothing.
            None => out
                .push_str("    (could not be read; the module was refused before they could be)\n"),
            Some(imports) if imports.is_empty() => out.push_str("    (none)\n"),
            Some(imports) => {
                for import in imports {
                    // Import names are arbitrary UTF-8 lifted straight out of a
                    // module an operator has explicitly not audited. A terminal
                    // escape here could rewrite the lines above it — the routes,
                    // the grant, the verdict — which is an attack on the
                    // decision this screen exists to inform.
                    out.push_str("    ");
                    // Already excerpted in `of`; escaping twice would render
                    // `\n` as `\\n`.
                    out.push_str(import);
                    out.push('\n');
                }
            }
        }
        out.push('\n');
        match &self.load_error {
            Some(error) => {
                out.push_str("\u{2717} this artifact does not load in this build's sandbox:\n  ");
                out.push_str(error);
                out.push('\n');
            }
            None => out.push_str("\u{2713} loads into this build's sandbox\n"),
        }
        out.push('\n');
        out.push_str(&self.conformance.to_text_report());
        out
    }

    /// Render the report as JSON.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message if the report cannot be serialized.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|err| err.to_string())
    }
}

/// Run the existing route-conformance checks over a manifest's declared routes.
///
/// A sandboxed plugin needs no binary built and no process run: the manifest
/// *is* the route table, and the runtime mounts exactly it. So the same checks
/// `autumn plugin-check` applies to a native plugin apply here, offline.
fn conformance(manifest: &SandboxManifest) -> ConformanceReport {
    let routes: Vec<RouteInfo> = manifest
        .routes
        .iter()
        .map(|route| RouteInfo {
            method: route.method.clone(),
            path: route.path.clone(),
            handler: format!("sandbox:{}", manifest.name),
            source: format!("plugin:{}", manifest.name),
            middleware: vec!["sandboxed".to_owned()],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        })
        .collect();

    // A sandboxed plugin holds no session, auth or database capability, so a
    // route it serves is unauthenticated by construction. Declaring that here
    // is the honest answer to the sensitive-surface check — the gating is the
    // sandbox itself.
    let gating = format!(
        "sandboxed: no session, auth, filesystem, network or database capability ({granted})",
        granted = manifest
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let sensitive = vec![plugin_check::SensitiveRouteDecl {
        path_pattern: manifest.prefix.clone(),
        auth_mechanism: gating,
    }];

    plugin_check::build_report(
        &plugin_check::PluginCheckOptions {
            package: None,
            bin: None,
            plugin_name: &manifest.name,
            expected_prefix: Some(&manifest.prefix),
            sensitive_routes: &sensitive,
            format: ReportFormat::Text,
        },
        &routes,
    )
}

/// Run `autumn plugin package`, printing the consent summary and exiting
/// non-zero on refusal.
pub fn run_package(opts: &PackageOptions<'_>) {
    eprintln!("\u{1F342} autumn plugin package\n");
    match package(opts) {
        Ok(artifact) => {
            print!("{}", artifact.manifest().consent_summary());
            println!("\nWrote {}", opts.out.display());
        }
        Err(err) => {
            eprintln!("\u{2717} {err}");
            std::process::exit(1);
        }
    }
}

/// Run `autumn plugin inspect`, printing the consent screen and exiting
/// non-zero when the artifact is not fit to install.
pub fn run_inspect(path: &Path, format: &ReportFormat) {
    if matches!(format, ReportFormat::Text) {
        eprintln!("\u{1F342} autumn plugin inspect\n");
    }
    let artifact = inspect(path).unwrap_or_else(|err| {
        eprintln!("\u{2717} {err}");
        std::process::exit(1);
    });
    let report = Report::of(&artifact);
    match format {
        ReportFormat::Text => print!("{}", report.to_text()),
        ReportFormat::Json => match report.to_json() {
            Ok(json) => println!("{json}"),
            Err(err) => {
                eprintln!("\u{2717} {err}");
                std::process::exit(1);
            }
        },
    }
    if !report.passed() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MODULE: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start") (nop))
)
"#;

    #[test]
    fn artifact_text_is_bounded_and_escaped_before_it_is_rendered() {
        // Both halves, on one string. The name is long enough that expanding it
        // whole would dwarf the excerpt, and it carries the escape sequence a
        // hostile artifact would use to rewrite the verdict printed above it.
        let hostile = format!("wasi_snapshot_preview1::\u{1b}[2K{}", "a".repeat(100_000));
        let rendered = excerpt(&hostile);

        assert!(
            rendered.len() < 4 * 1024,
            "the whole name was expanded: {} bytes",
            rendered.len()
        );
        assert!(rendered.contains("truncated"), "{rendered}");
        assert!(
            !rendered.contains('\u{1b}'),
            "an escape survived and can repaint the operator's screen"
        );
        // Still legible enough to act on — an operator has to recognise which
        // import the refusal is about.
        assert!(
            rendered.starts_with("wasi_snapshot_preview1::"),
            "{rendered}"
        );
    }

    fn manifest_toml() -> String {
        // The digest is stamped by packaging, so an authored manifest carries a
        // placeholder — proving `package` computes it rather than trusting it.
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"
"#,
            digest = "0".repeat(64)
        )
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        manifest: PathBuf,
        module: PathBuf,
        out: PathBuf,
    }

    fn fixture(manifest_src: &str, module_bytes: Vec<u8>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("plugin.toml");
        let module = dir.path().join("plugin.wasm");
        let out = dir.path().join("plugin.autumn-plugin");
        std::fs::write(&manifest, manifest_src).expect("writes");
        std::fs::write(&module, module_bytes).expect("writes");
        Fixture {
            _dir: dir,
            manifest,
            module,
            out,
        }
    }

    fn good_fixture() -> Fixture {
        fixture(&manifest_toml(), wat::parse_str(MODULE).expect("valid WAT"))
    }

    fn pack(fixture: &Fixture) -> SandboxArtifact {
        package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect("packages")
    }

    #[test]
    fn packaging_stamps_the_digest_the_author_could_not_know() {
        let fixture = good_fixture();
        let artifact = pack(&fixture);
        let module = std::fs::read(&fixture.module).expect("reads");
        assert_eq!(artifact.manifest().sha256, SandboxArtifact::digest(&module));
        assert_ne!(artifact.manifest().sha256, "0".repeat(64));
    }

    #[test]
    fn a_packaged_artifact_reads_back() {
        let fixture = good_fixture();
        pack(&fixture);
        let bytes = std::fs::read(&fixture.out).expect("reads");
        let artifact = SandboxArtifact::read(&bytes).expect("reads back");
        assert_eq!(artifact.manifest().name, "autumn-plugin-hello");
    }

    #[test]
    fn packaging_refuses_a_payload_that_is_not_wasm() {
        let fixture = fixture(&manifest_toml(), b"#!/bin/sh\n".to_vec());
        let err = package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect_err("must refuse");
        assert!(err.contains("WebAssembly"), "{err}");
        assert!(!fixture.out.exists(), "nothing may be written on refusal");
    }

    #[test]
    fn packaging_refuses_an_oversized_input_before_reading_it() {
        // `fs::read` sizes its buffer from the file, so a ceiling applied after
        // it returns is a decision made after the damage. Sparse files, so the
        // test does not allocate what it is proving is refused.
        for oversize_manifest in [true, false] {
            let fixture = good_fixture();
            let target = if oversize_manifest {
                &fixture.manifest
            } else {
                &fixture.module
            };
            let file = std::fs::File::create(target).expect("creates");
            file.set_len(128 * 1024 * 1024).expect("sets len");
            drop(file);

            let err = package(&PackageOptions {
                manifest: &fixture.manifest,
                module: &fixture.module,
                out: &fixture.out,
            })
            .expect_err("must refuse");
            assert!(
                err.contains("ceiling"),
                "expected a size refusal, got a {}-byte message starting {:?}",
                err.len(),
                err.chars().take(120).collect::<String>()
            );
            assert!(!fixture.out.exists(), "nothing may be written on refusal");
        }
    }

    #[test]
    fn packaging_refuses_a_manifest_that_does_not_validate() {
        let fixture = fixture(
            &manifest_toml().replace(r#"prefix = "/hello""#, r#"prefix = "/""#),
            wat::parse_str(MODULE).expect("valid WAT"),
        );
        let err = package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect_err("must refuse");
        assert!(err.contains("prefix"), "{err}");
    }

    #[test]
    fn packaging_refuses_a_module_the_sandbox_could_never_run() {
        // Better to fail at packaging than at the operator's boot.
        let module = wat::parse_str(
            r#"(module
                 (import "autumn_db" "query" (func (param i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "_start") (nop)))"#,
        )
        .expect("valid WAT");
        let fixture = fixture(&manifest_toml(), module);
        let err = package(&PackageOptions {
            manifest: &fixture.manifest,
            module: &fixture.module,
            out: &fixture.out,
        })
        .expect_err("must refuse");
        assert!(err.contains("autumn_db"), "{err}");
    }

    #[test]
    fn the_inspect_report_is_a_consent_screen() {
        let fixture = good_fixture();
        let artifact = pack(&fixture);
        let report = Report::of(&artifact);
        let text = report.to_text();
        for expected in [
            "autumn-plugin-hello",
            "/hello",
            "http-request",
            "GET /hello/greet",
            &artifact.manifest().sha256,
            "filesystem",
            "wasi_snapshot_preview1::fd_write",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
    }

    #[test]
    fn the_inspect_report_runs_the_conformance_checks_with_no_binary_to_build() {
        let artifact = pack(&good_fixture());
        let report = Report::of(&artifact);
        assert!(report.loads);
        assert!(
            report.conformance.passed(),
            "{}",
            report.conformance.to_text_report()
        );
        assert!(report.passed());
    }

    #[test]
    fn the_json_report_carries_what_a_reviewer_would_diff() {
        let artifact = pack(&good_fixture());
        let json = Report::of(&artifact).to_json().expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(value["name"], "autumn-plugin-hello");
        assert_eq!(value["prefix"], "/hello");
        assert_eq!(value["sha256"], artifact.manifest().sha256.as_str());
        assert_eq!(value["capabilities"][0], "http-request");
        assert_eq!(value["routes"][0]["path"], "/hello/greet");
        assert_eq!(value["loads"], true);
        assert!(value["denied"].as_array().is_some_and(|d| !d.is_empty()));
    }

    #[test]
    fn the_json_report_bounds_artifact_strings_the_way_the_text_one_does() {
        // The text renderer excerpted; `to_json` serialized the report's own
        // fields, so the bound stopped at whichever renderer remembered it.
        //
        // The wasm parser caps one name near 100 KiB, so a single import cannot
        // fill the module allowance — but the reported list holds up to 256 of
        // them, and `serde_json` expands every control character again escaping
        // it. The bound belongs on the report either way: which renderer is
        // safe should not depend on which one remembered to call `excerpt`.
        let name = format!("\\1b[2K{}", "a".repeat(50_000));
        let module = wat::parse_str(format!(
            r#"(module
                 (import "env" "{name}" (func))
                 (memory (export "memory") 1)
                 (func (export "_start") (nop)))"#
        ))
        .expect("valid WAT");
        let artifact = SandboxArtifact::seal(
            SandboxManifest::parse(&manifest_toml()).expect("valid manifest"),
            module,
        )
        .expect("seals");

        let built = Report::of(&artifact);
        let json = built.to_json().expect("serializes");
        assert!(
            json.len() < 64 * 1024,
            "the whole name reached the JSON: {} bytes",
            json.len()
        );
        // Bounded, and still an honest report: the reader must be able to see
        // that it was cut rather than that the artifact was small.
        assert!(json.contains("truncated"), "the cut is not disclosed");
        assert!(
            !json.contains('\u{1b}'),
            "a raw escape reached a consumer that may render it"
        );
    }

    #[test]
    fn inspecting_a_tampered_artifact_refuses_with_a_reason() {
        let fixture = good_fixture();
        pack(&fixture);
        let mut bytes = std::fs::read(&fixture.out).expect("reads");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&fixture.out, &bytes).expect("writes");

        let err = inspect(&fixture.out).expect_err("must refuse");
        assert!(err.contains("digest mismatch"), "{err}");
    }

    #[test]
    fn a_report_for_a_module_that_cannot_load_fails_rather_than_warns() {
        // Packaging refuses these, but an artifact built by an older or
        // friendlier packer must still not pass inspection here.
        let module = wat::parse_str(
            r#"(module
                 (import "env" "system" (func (param i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "_start") (nop)))"#,
        )
        .expect("valid WAT");
        // `seal` binds and stamps; it does not judge what the module imports.
        // That judgement is the report's, and it must be a refusal.
        let artifact = SandboxArtifact::seal(
            SandboxManifest::parse(&manifest_toml()).expect("valid manifest"),
            module,
        )
        .expect("seals");
        let report = Report::of(&artifact);
        assert!(!report.loads);
        assert!(!report.passed());
        assert!(
            report.to_text().contains("env::system"),
            "{}",
            report.to_text()
        );
    }

    #[test]
    fn a_module_refused_for_its_shape_reports_unread_imports_not_no_imports() {
        // The consent screen exists to answer "what authority does this ask
        // for?", and the artifacts where that answer matters most are the ones
        // the sandbox already refuses. Enumeration is refused with them — the
        // shape ceiling fires before the module is compiled — and reporting
        // that as an empty list made the screen answer "none" for an artifact
        // refused *for how much it imports*.
        let imports = (0..=autumn_web::plugin_sandbox::host::MAX_IMPORTS)
            .map(|_| {
                r#"  (import "wasi_snapshot_preview1" "fd_write"
                       (func (param i32 i32 i32 i32) (result i32)))"#
            })
            .collect::<Vec<_>>()
            .join("\n");
        let module = wat::parse_str(format!(
            "(module\n{imports}\n  (memory (export \"memory\") 1)\n  (func (export \"_start\") (nop)))"
        ))
        .expect("valid WAT");
        let artifact = SandboxArtifact::seal(
            SandboxManifest::parse(&manifest_toml()).expect("valid manifest"),
            module,
        )
        .expect("seals");

        let report = Report::of(&artifact);
        assert!(!report.loads, "the fixture must be refused to be the case");
        assert!(
            report.imports.is_none(),
            "an unreadable list must not be recorded as an empty one: {:?}",
            report.imports
        );

        let text = report.to_text();
        assert!(
            !text.contains("(none)"),
            "the screen still claims it asks for nothing:\n{text}"
        );
        assert!(text.contains("could not be read"), "{text}");

        // …and the JSON surface agrees, since a reviewer diffing reports reads
        // that one instead. `null` is distinguishable from `[]`; `[]` is not
        // distinguishable from the truth.
        let json = report.to_json().expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert!(value["imports"].is_null(), "{}", value["imports"]);
    }
}
