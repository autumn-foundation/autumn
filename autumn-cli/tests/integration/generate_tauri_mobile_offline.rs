//! Integration tests for `autumn generate tauri-mobile --offline-sync`
//! (issue #1508, Option C).
//!
//! The `--offline-sync` flag layers local-first storage onto the in-process
//! mobile scaffold from issue #1507: app data lives in a `SyncStore`-backed
//! `SQLite` file inside the app sandbox, and a background `SyncEngine` pushes
//! and pulls changes to the remote Autumn deployment's `/sync` endpoints
//! (`autumn_web::sync`, feature `offline-sync`) whenever the network allows.
//!
//! Without the flag the emitted scaffold must stay exactly what issue #1507
//! produces — the tests in `generate_tauri_mobile.rs` pin that baseline and
//! `default_output_has_no_sync_wiring` below pins the absence of any sync
//! delta.

use std::fs;
use std::path::Path;

use super::generate_tauri_mobile::{fresh_project, read, run_autumn};

/// Read every generated `src-tauri/` text file into one string, so
/// "no sync wiring anywhere in the shell" assertions can't miss a file.
fn read_shell_sources(project: &Path) -> String {
    let mut combined = String::new();
    for file in &[
        "src-tauri/tauri.conf.json",
        "src-tauri/Cargo.toml",
        "src-tauri/build.rs",
        "src-tauri/src/main.rs",
        "src-tauri/src/lib.rs",
        "src-tauri/.gitignore",
    ] {
        combined.push_str(&read(&project.join(file)));
    }
    combined
}

/// Scaffold a fresh app and run `generate tauri-mobile --offline-sync` on it.
fn offline_project(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let (tmp, project) = fresh_project(name);
    run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);
    (tmp, project)
}

// ── The offline-sync shell wiring ───────────────────────────────────────────

#[test]
fn offline_sync_shell_wires_syncstore_engine_and_resume_trigger() {
    let (_tmp, project) = offline_project("offsync-shell-app");

    let lib_rs = read(&project.join("src-tauri/src/lib.rs"));

    // Local store in the app sandbox, exported to the app's routes.
    assert!(
        lib_rs.contains(r#""AUTUMN_SYNC__DB_PATH""#),
        "shell lib.rs must export AUTUMN_SYNC__DB_PATH for the app's routes"
    );
    assert!(
        lib_rs.contains(r#"data_root.join("sync.db")"#),
        "the sync database must live in the app sandbox data dir"
    );
    assert!(
        lib_rs.contains("autumn_web::sync::SyncStore::open(&sync_db)"),
        "shell lib.rs must open the local SyncStore"
    );

    // Engine over the remote /sync mount, configured via env.
    assert!(
        lib_rs.contains(r#"std::env::var("AUTUMN_SYNC__REMOTE_URL")"#),
        "the engine must read AUTUMN_SYNC__REMOTE_URL at runtime"
    );
    assert!(lib_rs.contains("autumn_web::sync::SyncEngine::new("));
    assert!(lib_rs.contains("autumn_web::sync::SyncConfig::new(remote_url)"));

    // The engine authenticates against the fail-closed server mount: the
    // device sends AUTUMN_SYNC__TOKEN (the deployment's SYNC_TOKEN) as its
    // bearer token, and a missing token is called out at startup.
    assert!(
        lib_rs.contains(r#"std::env::var("AUTUMN_SYNC__TOKEN")"#),
        "the engine must read AUTUMN_SYNC__TOKEN at runtime"
    );
    assert!(
        lib_rs.contains("config.bearer_token = Some(token)"),
        "the token must be wired into SyncConfig::bearer_token"
    );

    // Background sync: spawned on the server runtime, 30 s interval
    // (exponential backoff while offline lives inside the engine).
    assert!(
        lib_rs.contains(".spawn_background(std::time::Duration::from_secs(30))"),
        "shell lib.rs must spawn the background sync task"
    );

    // Connectivity-regain trigger: an app resumed from the background gets
    // an immediate sync pass instead of waiting out the interval/backoff.
    assert!(
        lib_rs.contains("tauri::RunEvent::Resumed"),
        "shell lib.rs must kick a sync pass on RunEvent::Resumed"
    );
    assert!(
        lib_rs.contains("sync_once()"),
        "the resume trigger must call sync_once()"
    );
    assert!(
        lib_rs.contains(".build(tauri::generate_context!())"),
        "the builder must switch to build().run(callback) to observe run events"
    );

    // Offline-first startup: the device needs NO direct database connection —
    // the commented remote-Postgres block stays commented and the sync docs
    // are referenced.
    assert!(
        lib_rs.contains("docs/guide/tauri-mobile-offline-sync.md"),
        "shell lib.rs must point at the offline-sync guide"
    );
    assert!(
        !lib_rs.contains("\n    std::env::set_var(\"AUTUMN_DATABASE__URL\""),
        "the shell must not set a database URL by default (offline startup)"
    );
}

#[test]
fn offline_sync_shell_cargo_toml_enables_autumn_web_offline_sync() {
    let (_tmp, project) = offline_project("offsync-cargo-app");

    let cargo_toml = read(&project.join("src-tauri/Cargo.toml"));
    assert!(
        cargo_toml.contains(r#"autumn-web = { version = ""#),
        "shell must depend on autumn-web directly to use autumn_web::sync"
    );
    assert!(
        cargo_toml.contains(r#"features = ["offline-sync"]"#),
        "the shell's autumn-web dependency must enable the offline-sync feature"
    );
    // Version must match the app's autumn-web requirement so cargo unifies
    // the two dependency edges into one crate instance.
    let app_cargo = read(&project.join("Cargo.toml"));
    let app_req = app_cargo
        .lines()
        .find_map(|l| l.strip_prefix("autumn-web = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("app Cargo.toml must contain the stock autumn-web dependency");
    assert!(
        cargo_toml.contains(&format!(r#"autumn-web = {{ version = "{app_req}""#)),
        "shell autumn-web version must match the app's requirement ({app_req})"
    );
    toml::from_str::<toml::Value>(&cargo_toml).expect("generated Cargo.toml must parse");
}

#[test]
fn offline_sync_shell_cargo_toml_mirrors_crates_io_patch() {
    // The common local-development shape (and this repo's own test harness):
    // a registry autumn-web dep plus a [patch.crates-io] path override. The
    // shell manifest declares its own [workspace], so the app's patch does
    // not reach it — the generator must mirror the patch or the shell would
    // build a different framework source (or fail on the unpublished
    // offline-sync feature).
    let (_tmp, project) = fresh_project("offsync-patch-app");
    let cargo_path = project.join("Cargo.toml");
    let framework_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("autumn")
        .display()
        .to_string()
        .replace('\\', "/");
    let patched = format!(
        "{}\n[patch.crates-io]\nautumn-web = {{ path = \"{framework_path}\" }}\n",
        read(&cargo_path)
    );
    fs::write(&cargo_path, patched).unwrap();

    run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);

    let shell_cargo = read(&project.join("src-tauri/Cargo.toml"));
    assert!(
        shell_cargo.contains("[patch.crates-io]"),
        "the shell manifest must mirror the app's [patch.crates-io] override, got:\n{shell_cargo}"
    );
    assert!(
        shell_cargo.contains(&format!(r#"autumn-web = {{ path = "{framework_path}" }}"#)),
        "the mirrored patch must point at the same framework source, got:\n{shell_cargo}"
    );
    // The dependency edge itself stays at the app's registry requirement.
    assert!(
        shell_cargo.contains(r#"autumn-web = { version = ""#),
        "the dependency edge keeps the app's version requirement"
    );
    toml::from_str::<toml::Value>(&shell_cargo).expect("generated Cargo.toml must parse");
}

#[test]
fn offline_sync_shell_cargo_toml_mirrors_renamed_crates_io_patch() {
    // Same harness shape as the literal-key patch test, but the patch entry
    // is renamed (`aw_local = { package = "autumn-web", ... }`): it must be
    // matched by its effective package and mirrored whole into the shell
    // manifest — a silently dropped patch would leave the shell on the
    // registry while the app builds the patched source.
    let (_tmp, project) = fresh_project("offsync-renamed-patch-app");
    let cargo_path = project.join("Cargo.toml");
    let framework_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("autumn")
        .display()
        .to_string()
        .replace('\\', "/");
    let patched = format!(
        "{}\n[patch.crates-io]\naw_local = {{ package = \"autumn-web\", path = \"{framework_path}\" }}\n",
        read(&cargo_path)
    );
    fs::write(&cargo_path, patched).unwrap();

    run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);

    let shell_cargo = read(&project.join("src-tauri/Cargo.toml"));
    assert!(
        shell_cargo.contains("[patch.crates-io]"),
        "the shell manifest must mirror the renamed patch, got:\n{shell_cargo}"
    );
    assert!(
        shell_cargo.contains(&format!(
            r#"aw_local = {{ package = "autumn-web", path = "{framework_path}" }}"#
        )),
        "the entry must be mirrored whole (key + package + source), got:\n{shell_cargo}"
    );
    toml::from_str::<toml::Value>(&shell_cargo).expect("generated Cargo.toml must parse");
}

#[test]
fn offline_sync_shell_cargo_toml_mirrors_path_dependency() {
    // A direct path dependency on autumn-web must be recomputed relative to
    // src-tauri/ (one directory deeper than the app manifest).
    let (_tmp, project) = fresh_project("offsync-pathdep-app");
    let cargo_path = project.join("Cargo.toml");
    let rewritten = read(&cargo_path)
        .lines()
        .map(|l| {
            if l.starts_with("autumn-web = \"") {
                "autumn-web = { path = \"../vendored-autumn\" }".to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&cargo_path, rewritten).unwrap();

    run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);

    let shell_cargo = read(&project.join("src-tauri/Cargo.toml"));
    assert!(
        shell_cargo.contains(
            r#"autumn-web = { path = "../../vendored-autumn", features = ["offline-sync"] }"#
        ),
        "the shell's autumn-web edge must mirror the app's path source, got:\n{shell_cargo}"
    );
    assert!(
        !shell_cargo.contains("[patch.crates-io]"),
        "a direct path dep needs no patch section"
    );
    toml::from_str::<toml::Value>(&shell_cargo).expect("generated Cargo.toml must parse");
}

#[test]
fn offline_sync_shell_cargo_toml_preserves_no_default_features() {
    // Feature unification is per dependency edge: if the app disables the
    // framework's default features, a mirrored shell edge without
    // `default-features = false` would silently re-enable them for the
    // whole src-tauri build.
    let (_tmp, project) = fresh_project("offsync-nodefault-app");
    let cargo_path = project.join("Cargo.toml");
    let rewritten = read(&cargo_path)
        .lines()
        .map(|l| {
            l.strip_prefix("autumn-web = \"")
                .and_then(|rest| rest.strip_suffix('"'))
                .map_or_else(
                    || l.to_owned(),
                    |version| {
                        format!(
                            "autumn-web = {{ version = \"{version}\", default-features = false }}"
                        )
                    },
                )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&cargo_path, rewritten).unwrap();

    run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);

    let shell_cargo = read(&project.join("src-tauri/Cargo.toml"));
    assert!(
        shell_cargo.contains(r#"autumn-web = { version = ""#)
            && shell_cargo.contains(r#", default-features = false, features = ["offline-sync"] }"#),
        "the shell's autumn-web edge must preserve default-features = false, got:\n{shell_cargo}"
    );
    toml::from_str::<toml::Value>(&shell_cargo).expect("generated Cargo.toml must parse");
}

#[test]
fn offline_sync_handles_renamed_framework_dependency() {
    // Apps may rename the framework dep (`aw = { package = "autumn-web" }`):
    // the dependency must be resolved by PACKAGE, and the feature edit must
    // reference the actual dependency KEY (`aw/offline-sync`) — cargo
    // rejects `autumn-web/offline-sync` when no such dep key exists.
    let (_tmp, project) = fresh_project("offsync-renamed-app");
    let cargo_path = project.join("Cargo.toml");
    let rewritten = read(&cargo_path)
        .lines()
        .map(|l| {
            l.strip_prefix("autumn-web = \"")
                .and_then(|rest| rest.strip_suffix('"'))
                .map_or_else(
                    || l.replace("autumn-web/", "aw/"),
                    |version| {
                        format!("aw = {{ package = \"autumn-web\", version = \"{version}\" }}")
                    },
                )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&cargo_path, rewritten).unwrap();

    run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);

    // The feature edit lands on the renamed dependency key…
    let app_cargo = read(&cargo_path);
    assert!(
        app_cargo.contains(r#"offline-sync = ["aw/offline-sync"]"#),
        "the feature edit must target the renamed dep key, got:\n{app_cargo}"
    );
    assert!(
        !app_cargo.contains(r#"["autumn-web/offline-sync"]"#),
        "cargo would reject a feature naming a nonexistent dep key"
    );
    toml::from_str::<toml::Value>(&app_cargo).expect("edited Cargo.toml must stay valid TOML");

    // …and the shell edge mirrors the resolved source (not the registry
    // fallback the old key-based lookup produced).
    let shell_cargo = read(&project.join("src-tauri/Cargo.toml"));
    let version = app_cargo
        .lines()
        .find(|l| l.starts_with("aw = {"))
        .and_then(|l| l.split("version = \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("renamed dep must carry a version");
    assert!(
        shell_cargo.contains(&format!(
            r#"autumn-web = {{ version = "{version}", features = ["offline-sync"] }}"#
        )),
        "the shell edge must resolve the renamed dep's source, got:\n{shell_cargo}"
    );
}

// ── The app-side (server) wiring ────────────────────────────────────────────

#[test]
fn offline_sync_app_lib_mounts_sync_router_behind_db_guard() {
    let (_tmp, project) = offline_project("offsync-server-app");

    let lib_rs = read(&project.join("src/lib.rs"));
    assert!(lib_rs.contains("pub async fn serve()"));

    // The sync endpoints are mounted feature-gated and only when a database
    // is actually configured: the remote (server) deployment serves /sync,
    // while the same code running in-process on a device (no database in
    // its resolved config) starts fine fully offline as a sync CLIENT.
    assert!(lib_rs.contains(r#"#[cfg(feature = "offline-sync")]"#));
    assert!(
        lib_rs.contains("let app = mount_offline_sync(app).await;"),
        "serve() must route through the sync mounting helper"
    );
    // The guard resolves the database through the app's layered config
    // (files + profiles + env), not one raw env var.
    assert!(lib_rs.contains("autumn_web::config::AutumnConfig::load()"));
    assert!(lib_rs.contains("config.database.effective_primary_url()"));
    assert!(lib_rs.contains("PgSyncBackend::new(database_url)"));
    assert!(
        lib_rs.contains("REQUIRED before shipping"),
        "the generated helper must present HTTPS-only serving as a requirement"
    );
    assert!(
        lib_rs.contains("ensure_schema()"),
        "the sync shadow-table DDL must run at startup"
    );
    assert!(
        lib_rs.contains("app.nest(") && lib_rs.contains(r#""/sync","#),
        "serve() must nest the sync router at /sync"
    );

    // Fail closed (#1612 review): a fresh scaffold must never expose an
    // unauthenticated /sync. The mount is gated on SYNC_TOKEN and wrapped
    // in the generated constant-time bearer-token middleware.
    assert!(
        lib_rs.contains(r#"std::env::var("SYNC_TOKEN")"#),
        "the mount must be gated on SYNC_TOKEN"
    );
    assert!(
        lib_rs.contains("/sync NOT mounted"),
        "a missing SYNC_TOKEN must skip the mount with a loud warning"
    );
    assert!(
        lib_rs.contains(".layer(axum::middleware::from_fn(require_sync_auth)),"),
        "the mounted router must be wrapped in the auth middleware"
    );
    assert!(
        lib_rs.contains("autumn_web::sync::server::constant_time_token_eq"),
        "the bearer-token comparison must be constant-time"
    );
    assert!(
        !lib_rs.contains(r#".nest("/sync", server::router(backend, Arc::new(LwwResolver)))"#),
        "the old unauthenticated one-liner mount must not be emitted"
    );

    // Startup tolerance: an unreachable database must not abort the boot —
    // ensure_schema failures are logged, never unwrapped/expected.
    assert!(
        lib_rs.contains("must not prevent the app from starting"),
        "the DDL failure path must document (and implement) log-and-continue"
    );
}

#[test]
fn offline_sync_app_cargo_toml_gains_default_offline_sync_feature() {
    let (_tmp, project) = offline_project("offsync-feature-app");

    let cargo_toml = read(&project.join("Cargo.toml"));
    assert!(
        cargo_toml.contains(r#"offline-sync = ["autumn-web/offline-sync"]"#),
        "app Cargo.toml must declare the offline-sync feature"
    );
    assert!(
        cargo_toml.contains(r#"default = ["flash", "offline-sync"]"#),
        "offline-sync must be a default feature so plain `cargo run` server \
         deployments mount the /sync endpoints"
    );
    toml::from_str::<toml::Value>(&cargo_toml).expect("edited Cargo.toml must stay valid TOML");
}

#[test]
fn offline_sync_rerun_with_force_is_idempotent() {
    let (_tmp, project) = offline_project("offsync-rerun-app");
    let cargo_after_first = read(&project.join("Cargo.toml"));

    run_autumn(
        &project,
        &["generate", "tauri-mobile", "--offline-sync", "--force"],
    );
    assert_eq!(
        read(&project.join("Cargo.toml")),
        cargo_after_first,
        "a --force re-run must not duplicate the offline-sync feature edit"
    );
}

// ── Prerequisites output ────────────────────────────────────────────────────

#[test]
fn offline_sync_prerequisites_mention_sync_setup() {
    let (_tmp, project) = fresh_project("offsync-prereq-app");
    let output = run_autumn(&project, &["generate", "tauri-mobile", "--offline-sync"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("AUTUMN_SYNC__REMOTE_URL"),
        "prerequisites must tell the user to point the engine at the remote /sync"
    );
    assert!(
        stdout.contains("SYNC_TOKEN"),
        "prerequisites must explain the fail-closed SYNC_TOKEN pairing"
    );
    assert!(
        stdout.contains("tauri-mobile-offline-sync"),
        "prerequisites must point at docs/guide/tauri-mobile-offline-sync.md"
    );
}

// ── Default output is untouched without the flag ────────────────────────────

#[test]
fn default_output_has_no_sync_wiring() {
    let (_tmp, project) = fresh_project("offsync-default-app");
    let app_cargo_before = read(&project.join("Cargo.toml"));

    run_autumn(&project, &["generate", "tauri-mobile"]);

    // The app's Cargo.toml is only edited under the flag.
    assert_eq!(
        read(&project.join("Cargo.toml")),
        app_cargo_before,
        "without --offline-sync the app Cargo.toml must be byte-identical"
    );

    // No sync traces anywhere in the shell or the extracted app lib.
    let shell = read_shell_sources(&project);
    assert!(
        !shell.contains("AUTUMN_SYNC"),
        "no AUTUMN_SYNC__* wiring may be emitted without the flag"
    );
    assert!(
        !shell.contains("offline-sync") && !shell.contains("offline_sync"),
        "no offline-sync references may be emitted without the flag"
    );
    for marker in [
        "SyncStore",
        "SyncEngine",
        "spawn_background",
        "sync_once",
        "SYNC_KICK",
        "RunEvent::Resumed",
    ] {
        assert!(
            !shell.contains(marker),
            "no `{marker}` may be emitted without the flag"
        );
    }
    assert!(
        shell.contains(".run(tauri::generate_context!())"),
        "the default shell must keep the simple run(context) tail, not the \
         offline shell's build().run(callback) shape"
    );
    let app_lib = read(&project.join("src/lib.rs"));
    assert!(
        !app_lib.contains("mount_offline_sync") && !app_lib.contains("offline-sync"),
        "the extracted app lib must not gain sync mounting without the flag"
    );
    // Byte-level identity of the default shell lib.rs is pinned by the
    // golden-snapshot unit test in generate/tauri_mobile.rs.
}

#[test]
fn offline_sync_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("offsync-dryrun-app");
    let cargo_before = read(&project.join("Cargo.toml"));
    let main_before = read(&project.join("src/main.rs"));

    run_autumn(
        &project,
        &["generate", "tauri-mobile", "--offline-sync", "--dry-run"],
    );

    assert!(
        !project.join("src-tauri").exists(),
        "--dry-run must not create src-tauri/"
    );
    assert!(
        !project.join("src/lib.rs").exists(),
        "--dry-run must not create src/lib.rs"
    );
    assert_eq!(
        read(&project.join("Cargo.toml")),
        cargo_before,
        "--dry-run must not edit the app Cargo.toml"
    );
    assert_eq!(read(&project.join("src/main.rs")), main_before);
}

// ── Documentation ───────────────────────────────────────────────────────────

#[test]
fn offline_sync_docs_page_exists_and_is_linked() {
    let docs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/guide");
    let page = read(&docs_dir.join("tauri-mobile-offline-sync.md"));

    // Change tracking / journal semantics.
    assert!(page.contains("change-tracking") || page.contains("change tracking"));
    assert!(
        page.contains("journal"),
        "docs must cover the change journal"
    );

    // Tombstoning, GC horizon, and full resync.
    assert!(page.contains("tombstone"), "docs must cover tombstoning");
    assert!(page.contains("tombstone_horizon"));
    assert!(page.contains("FullResyncRequired"));
    assert!(page.contains("gc_tombstones"));

    // Conflict resolution: default LWW + custom resolver trait with example.
    assert!(page.contains("last-write-wins"));
    assert!(page.contains("ConflictResolver"));
    assert!(
        page.contains("impl ConflictResolver for"),
        "docs must show a custom resolver example"
    );
    assert!(page.contains("Resolution::"));

    // Server-authoritative versioning.
    assert!(
        page.contains("server-authoritative") || page.contains("Server-authoritative"),
        "docs must explain CSN/server-authoritative ordering"
    );

    // Env vars the templates use.
    assert!(page.contains("AUTUMN_SYNC__DB_PATH"));
    assert!(page.contains("AUTUMN_SYNC__REMOTE_URL"));

    // Offline showcase walkthrough with an airplane-mode verify checklist.
    assert!(page.contains("--offline-sync"));
    assert!(
        page.contains("airplane"),
        "docs must include the airplane-mode walkthrough"
    );
    assert!(page.contains("SyncStore"));

    // Honest limitations: scope, repositories, auth.
    assert!(
        page.lines()
            .any(|l| l.starts_with('#') && l.to_lowercase().contains("limitations")),
        "docs must have a limitations section heading"
    );
    assert!(
        page.contains("repositories"),
        "docs must state that diesel repositories are not offline"
    );
    assert!(
        page.contains("auth"),
        "docs must mandate mounting /sync behind authentication"
    );

    // The in-process mobile guide links here (one line, appended).
    let mobile = read(&docs_dir.join("tauri-mobile-in-process.md"));
    assert!(
        mobile.contains("tauri-mobile-offline-sync.md"),
        "docs/guide/tauri-mobile-in-process.md must link to the offline-sync guide"
    );
}

/// Drift check: every `<!-- drift:<path> -->`-marked code block in the docs
/// page must consist of lines that appear VERBATIM in the corresponding
/// generated file — so the documented wiring can never silently diverge from
/// what the generator emits. Lines that are blank or contain an elision
/// marker (`...` / `…`) are skipped.
#[test]
fn offline_sync_docs_code_matches_emitted_templates() {
    let docs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/guide");
    let page = read(&docs_dir.join("tauri-mobile-offline-sync.md"));

    let (_tmp, project) = offline_project("offsync-drift-app");

    let mut checked_blocks = 0;
    let mut lines = page.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("<!-- drift:") else {
            continue;
        };
        let target = rest.trim_end_matches("-->").trim();
        let generated = read(&project.join(target));

        // The marker must be immediately followed by a fenced code block.
        let fence = lines.next().unwrap_or_default();
        assert!(
            fence.trim_start().starts_with("```"),
            "drift marker for {target} must be followed by a code fence"
        );
        for code_line in lines.by_ref() {
            if code_line.trim_start().starts_with("```") {
                break;
            }
            let significant = code_line.trim_end();
            if significant.trim().is_empty()
                || significant.contains("...")
                || significant.contains('…')
            {
                continue;
            }
            assert!(
                generated.contains(significant),
                "docs drift: this documented line is not in the generated {target}:\n{significant}"
            );
        }
        checked_blocks += 1;
    }
    assert!(
        checked_blocks >= 2,
        "the docs page must carry at least two drift-checked snippets \
         (shell wiring + app-side mounting), found {checked_blocks}"
    );
}

// ── Destroy still works on an offline-sync scaffold ─────────────────────────

#[test]
fn destroy_offline_sync_scaffold_removes_shell_keeps_app_edits() {
    let (_tmp, project) = offline_project("offsync-destroy-app");
    let cargo_after_generate = read(&project.join("Cargo.toml"));

    run_autumn(&project, &["destroy", "tauri-mobile", "--offline-sync"]);
    assert!(
        !project.join("src-tauri").exists(),
        "destroy must remove the generated src-tauri/ shell"
    );
    // Like the lib extraction, the app-crate edits survive destroy: main.rs
    // depends on the extracted lib, and the offline-sync feature edit is a
    // Modify (destroy never reverses those for this generator).
    assert!(project.join("src/lib.rs").is_file());
    assert_eq!(
        read(&project.join("Cargo.toml")),
        cargo_after_generate,
        "destroy must not touch the app Cargo.toml"
    );

    let _ = fs::remove_dir_all(project.join("src-tauri"));
}
