//! Production deployment scaffolding for `autumn release init`.
//!
//! Emits a curated set of files (Dockerfile, .dockerignore, config example,
//! and optional target-specific scaffolds) at the project root.

use std::fs;
use std::path::Path;

mod templates {
    pub const DOCKERFILE: &str = include_str!("templates/release/Dockerfile.tmpl");
    pub const DOCKERIGNORE: &str = include_str!("templates/release/.dockerignore.tmpl");
    pub const PRODUCTION_CONFIG: &str =
        include_str!("templates/release/autumn.production.toml.example.tmpl");
    pub const FLY_TOML: &str = include_str!("templates/release/fly.toml.tmpl");
    pub const DOCKER_COMPOSE: &str = include_str!("templates/release/docker-compose.yml.tmpl");
    pub const AZURE_MAIN_TF: &str = include_str!("templates/release/main.tf.tmpl");
    pub const AZURE_VARIABLES_TF: &str = include_str!("templates/release/variables.tf.tmpl");
    pub const AZURE_OUTPUTS_TF: &str = include_str!("templates/release/outputs.tf.tmpl");
    pub const AZURE_TFVARS_EXAMPLE: &str =
        include_str!("templates/release/terraform.tfvars.example.tmpl");
    pub const AZURE_DEPLOY_WORKFLOW: &str = include_str!("templates/release/azure-deploy.yml.tmpl");
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("'{0}' already exists — run with --force to overwrite")]
    FileExists(String),

    #[error("failed to read Cargo.toml: {0}")]
    CargoToml(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Default,
    Fly,
    DockerCompose,
    AzureContainerApps,
}

impl std::str::FromStr for Target {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fly" => Ok(Self::Fly),
            "docker-compose" => Ok(Self::DockerCompose),
            "azure-container-apps" => Ok(Self::AzureContainerApps),
            other => Err(format!(
                "unknown target '{other}'; expected 'fly', 'docker-compose', or \
                 'azure-container-apps'"
            )),
        }
    }
}

#[derive(Clone, Copy)]
pub enum ReleaseAction {
    Init {
        force: bool,
        target: Target,
        /// Scaffold a separate `worker` service (docker-compose target) that runs
        /// the app's worker role alongside a web-only `app` service.
        split_workers: bool,
    },
}

pub fn run(action: ReleaseAction) {
    eprintln!("🍂 autumn release\n");

    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("Error: cannot determine current directory: {e}");
        std::process::exit(1);
    });

    match action {
        ReleaseAction::Init {
            force,
            target,
            split_workers,
        } => {
            let project_name = read_project_name(&cwd).unwrap_or_else(|e| {
                eprintln!("Error: {e}");
                std::process::exit(1);
            });

            match init(&cwd, &project_name, force, target, split_workers) {
                Ok(files) => {
                    for f in &files {
                        println!("  Created {f}");
                    }

                    // Smoke gate: verify the generated production config does
                    // not contain a committed signing secret literal.
                    let config_path = cwd.join("autumn.production.toml.example");
                    if let Ok(content) = std::fs::read_to_string(&config_path)
                        && let Err(e) = check_production_config_signing_secret(&content)
                    {
                        eprintln!("Warning: smoke gate failed for generated config: {e}");
                    }

                    println!();
                    println!("Next steps:");
                    println!(
                        "  1. Generate and set your signing secret (REQUIRED before production boot):"
                    );
                    println!(
                        "       export AUTUMN_SECURITY__SIGNING_SECRET=\"$(openssl rand -hex 32)\""
                    );
                    println!("     Smoke-gate check — the app must refuse to start without it:");
                    println!("       AUTUMN_ENV=prod docker run --rm \\");
                    println!("         -e AUTUMN_DATABASE__PRIMARY_URL=... \\");
                    println!("         {project_name} 2>&1 | grep -i 'signing secret'");
                    println!("     And must start with it:");
                    println!("       AUTUMN_ENV=prod docker run --rm \\");
                    println!("         -e AUTUMN_DATABASE__PRIMARY_URL=... \\");
                    println!(
                        "         -e AUTUMN_SECURITY__SIGNING_SECRET=\"$AUTUMN_SECURITY__SIGNING_SECRET\" \\"
                    );
                    println!("         {project_name}");
                    println!();
                    println!("  2. Build, migrate the primary once, then run web replicas:");
                    println!("       docker build -t {project_name} .");
                    println!("       AUTUMN_DATABASE__PRIMARY_URL=... autumn migrate");
                    println!(
                        "       docker run --rm -p 3000:3000 -e AUTUMN_DATABASE__PRIMARY_URL=... \\"
                    );
                    println!(
                        "         -e AUTUMN_SECURITY__SIGNING_SECRET=\"$AUTUMN_SECURITY__SIGNING_SECRET\" \\"
                    );
                    println!("         {project_name}");
                    println!();
                    println!(
                        "  See docs/guide/deployment.md and docs/guide/signing-secrets.md for the full walkthrough."
                    );
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Validate a generated production config file for signing-secret compliance.
///
/// Returns `Ok(())` when the config file correctly documents the signing
/// secret via an environment variable reference (not a committed literal value).
/// Returns `Err` with a human-readable explanation when the file contains a
/// committed secret literal.
///
/// Used by the release checklist smoke gate to verify that generated
/// deployment files obey the "never commit secrets" rule.
///
/// # Errors
///
/// Returns a string error message when a raw signing secret literal is found
/// in a non-comment line of `content`.
pub fn check_production_config_signing_secret(content: &str) -> Result<(), String> {
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        // A non-comment line containing a `secret =` assignment with a non-empty
        // RHS is a committed secret literal — a critical misconfiguration.
        if let Some(rest) = trimmed.strip_prefix("secret") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let value = rest.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() && value != "[]" {
                    return Err(format!(
                        "line {}: production config contains a committed signing secret literal \
                         at `secret = ...`; use AUTUMN_SECURITY__SIGNING_SECRET env var instead",
                        line_num + 1,
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn read_project_name(dir: &Path) -> Result<String, ReleaseError> {
    let path = dir.join("Cargo.toml");
    let content = fs::read_to_string(&path)
        .map_err(|e| ReleaseError::CargoToml(format!("{}: {e}", path.display())))?;

    // Check for workspace root before parsing; workspace-only Cargo.toml files
    // may not parse cleanly as a member manifest.
    if content.contains("[workspace]") && !content.contains("[package]") {
        return Err(ReleaseError::CargoToml(
            "found [workspace] but no [package] — run this command from a member crate directory, not the workspace root".into(),
        ));
    }

    let parsed: toml::Value = toml::from_str(&content)
        .map_err(|e| ReleaseError::CargoToml(format!("parse error: {e}")))?;
    parsed
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_owned)
        .ok_or_else(|| ReleaseError::CargoToml("missing [package] name".into()))
}

/// Emit release scaffolding files into `dir` for the given `project_name`.
///
/// Returns the list of file names written. Returns [`ReleaseError::FileExists`]
/// if any output file already exists and `force` is `false`.
pub fn init(
    dir: &Path,
    project_name: &str,
    force: bool,
    target: Target,
    split_workers: bool,
) -> Result<Vec<String>, ReleaseError> {
    let files = planned_files(target);

    if !force {
        for (name, _) in &files {
            if dir.join(name).exists() {
                return Err(ReleaseError::FileExists(name.to_string()));
            }
        }
    }

    // Embed assets into the binary only when the project opts in via the
    // `embed-assets` feature (as `autumn new` generates). Pre-existing apps
    // without that feature get the disk-based build (`cargo build --release`
    // plus `COPY static`), so their Docker builds keep working.
    let embed = project_has_embed_assets(dir);

    let mut created = Vec::new();
    for (name, template) in files {
        let path = dir.join(name);
        // Most planned files sit at the project root, but some targets (e.g.
        // azure-container-apps' `.github/workflows/...`) nest under a
        // subdirectory that a fresh project won't have yet.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, render(template, project_name, embed, split_workers))?;
        created.push(name.to_string());
    }

    if matches!(target, Target::AzureContainerApps) {
        ensure_azure_gitignore_entries(dir)?;
    }

    Ok(created)
}

/// Terraform state (`*.tfstate*`) holds every secret value in plaintext —
/// `sensitive = true` on a variable only redacts CLI plan/apply output, never
/// the state file — and a real `terraform.tfvars` holds the operator's own
/// secret values. None of that may ever land in version control.
const AZURE_GITIGNORE_ENTRIES: &[&str] = &[
    "# Terraform (autumn release init --target azure-container-apps)",
    ".terraform/",
    "*.tfstate",
    "*.tfstate.*",
    "terraform.tfvars",
];

/// Ensure `dir/.gitignore` excludes Terraform state and the operator's real
/// `terraform.tfvars`, merging into an existing file (creating one if
/// missing) without touching unrelated lines. Idempotent: a re-run (e.g.
/// under `--force`) never duplicates entries already present.
fn ensure_azure_gitignore_entries(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&str> = AZURE_GITIGNORE_ENTRIES
        .iter()
        .copied()
        .filter(|line| !existing.lines().any(|l| l.trim() == *line))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push('\n');
    }
    for line in missing {
        updated.push_str(line);
        updated.push('\n');
    }
    fs::write(path, updated)
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Whether the project at `dir` defines an `embed-assets` Cargo feature, i.e.
/// it is wired for single-binary embedded builds (`autumn build --embed`).
///
/// Parses the `[features]` table rather than substring-matching the file, so a
/// comment or unrelated text mentioning "embed-assets" doesn't cause
/// `autumn build --embed` (which would then fail for a project that lacks the
/// feature) to be baked into the generated Dockerfile.
fn project_has_embed_assets(dir: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(dir.join("Cargo.toml")) else {
        return false;
    };
    let Ok(parsed) = toml::from_str::<toml::Table>(&contents) else {
        return false;
    };
    parsed
        .get("features")
        .and_then(toml::Value::as_table)
        .is_some_and(|features| features.contains_key("embed-assets"))
}

/// The extra `worker:` service block spliced into the docker-compose output when
/// `--split-workers` is set. Runs the SAME image (`build: .`) and default
/// entrypoint as the `app` service, but with `AUTUMN_ROLE=worker` so it runs the
/// job workers + scheduler only. Shares the app's database URL, signing secret,
/// and one-shot migration gate. Both tiers use the durable `postgres` jobs
/// backend so the queue is shared across the separate web and worker processes
/// (the in-process `local` backend can't span processes).
const WORKER_SERVICE_BLOCK: &str = "\n  worker:\n    build: .\n    environment:\n      \
AUTUMN_PROFILE: prod\n      AUTUMN_ROLE: worker\n      AUTUMN_JOBS__BACKEND: postgres\n      \
AUTUMN_DATABASE__PRIMARY_URL: postgres://autumn:autumn@db:5432/{{project_name}}_prod\n      \
AUTUMN_SECURITY__SIGNING_SECRET: \"${AUTUMN_SECURITY__SIGNING_SECRET:?set it first}\"\n    \
depends_on:\n      db:\n        condition: service_healthy\n      migrate:\n        \
condition: service_completed_successfully\n    restart: unless-stopped\n";

/// The web-tier role env spliced into the `app` service when `--split-workers`
/// is set: pin the app to the HTTP-only `web` role and the shared `postgres`
/// jobs backend so enqueues land in the queue the worker process drains.
const WEB_ROLE_ENV: &str = "\n      AUTUMN_ROLE: web\n      AUTUMN_JOBS__BACKEND: postgres";

fn render(template: &str, project_name: &str, embed: bool, split_workers: bool) -> String {
    let (build_step, static_copy) = if embed {
        (
            "# Single-binary build: fingerprint static assets, then compile with the\n\
             # embed-assets feature so the binary serves static/ (incl. the fingerprint\n\
             # manifest) and i18n locales from itself — no sidecar directories. The app\n\
             # opts in via `.embedded_static()` / `.embedded_locales()` (see src/main.rs).\n\
             RUN autumn build --embed",
            // Assets/locales are embedded; only migrations/ is staged below.
            "# Assets and locales are embedded in the binary (`autumn build --embed`);\n\
             # only migrations/ is staged, for the one-shot `autumn migrate` job.\n",
        )
    } else {
        (
            "RUN cargo build --release",
            "COPY --chown=autumn:autumn --from=builder /app/static /app/static\n",
        )
    };
    // Split-topology placeholders exist only in the docker-compose template, so
    // these replacements are no-ops for the other files. Substitute them before
    // `{{project_name}}` so the worker block's own `{{project_name}}` tokens are
    // resolved by the shared replacement below.
    let (worker_service, web_role_env) = if split_workers {
        (WORKER_SERVICE_BLOCK, WEB_ROLE_ENV)
    } else {
        ("", "")
    };
    template
        .replace("{{worker_service}}", worker_service)
        .replace("{{app_role_env}}", web_role_env)
        .replace("{{project_name}}", project_name)
        .replace(
            "{{rust_version}}",
            option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
        )
        .replace("{{autumn_cli_version}}", env!("CARGO_PKG_VERSION"))
        .replace("{{diesel_cli_version}}", "2.3.8")
        .replace("{{build_step}}", build_step)
        .replace("{{static_copy}}", static_copy)
}

fn planned_files(target: Target) -> Vec<(&'static str, &'static str)> {
    let mut files: Vec<(&'static str, &'static str)> = vec![
        ("Dockerfile", templates::DOCKERFILE),
        (".dockerignore", templates::DOCKERIGNORE),
        (
            "autumn.production.toml.example",
            templates::PRODUCTION_CONFIG,
        ),
    ];
    match target {
        Target::Fly => files.push(("fly.toml", templates::FLY_TOML)),
        Target::DockerCompose => files.push(("docker-compose.yml", templates::DOCKER_COMPOSE)),
        Target::AzureContainerApps => {
            files.push(("main.tf", templates::AZURE_MAIN_TF));
            files.push(("variables.tf", templates::AZURE_VARIABLES_TF));
            files.push(("outputs.tf", templates::AZURE_OUTPUTS_TF));
            files.push(("terraform.tfvars.example", templates::AZURE_TFVARS_EXAMPLE));
            files.push((
                ".github/workflows/azure-deploy.yml",
                templates::AZURE_DEPLOY_WORKFLOW,
            ));
        }
        Target::Default => {}
    }
    files
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_project(tmp: &TempDir, name: &str) -> std::path::PathBuf {
        let dir = tmp.path().to_path_buf();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        dir
    }

    // ── default target ────────────────────────────────────────────────────────

    #[test]
    fn init_creates_dockerfile() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        assert!(dir.join("Dockerfile").is_file(), "Dockerfile not created");
    }

    #[test]
    fn init_creates_dockerignore() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        assert!(
            dir.join(".dockerignore").is_file(),
            ".dockerignore not created"
        );
    }

    #[test]
    fn init_creates_production_config_example() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        assert!(
            dir.join("autumn.production.toml.example").is_file(),
            "autumn.production.toml.example not created"
        );
    }

    #[test]
    fn init_returns_list_of_created_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        let files = init(&dir, "my-app", false, Target::Default, false).unwrap();
        assert!(files.contains(&"Dockerfile".to_string()));
        assert!(files.contains(&".dockerignore".to_string()));
        assert!(files.contains(&"autumn.production.toml.example".to_string()));
    }

    // ── Dockerfile content ────────────────────────────────────────────────────

    #[test]
    fn dockerfile_has_cargo_chef_stages() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("cargo-chef"),
            "Dockerfile must use cargo-chef for dependency caching"
        );
        assert!(
            content.contains("cargo chef prepare"),
            "Dockerfile must run 'cargo chef prepare'"
        );
        assert!(
            content.contains("cargo chef cook"),
            "Dockerfile must run 'cargo chef cook'"
        );
    }

    #[test]
    fn dockerfile_uses_declared_msrv_for_rust_build_image() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        let msrv = option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0");

        assert!(
            content.contains(&format!("FROM rust:{msrv}-bookworm AS chef")),
            "Dockerfile build stage must use the declared MSRV {msrv}: {content}"
        );
        assert!(
            !content.contains("rust-1.86") && !content.contains("rust:1.86"),
            "Dockerfile must not pin an older Rust image than the declared MSRV: {content}"
        );
    }

    #[test]
    fn dockerfile_passes_through_git_provenance_build_args() {
        // Issue #1676: the build context excludes `/.git`, so the release
        // Dockerfile must surface the `AUTUMN_BUILD_*` provenance as build-arg
        // ENV before the build step. Otherwise the generated build.rs finds no
        // git repo and the container reports `git.*` as `null` on
        // `/actuator/info`, defeating deploy/rollback verification.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();

        for arg in [
            "AUTUMN_BUILD_GIT_SHA",
            "AUTUMN_BUILD_GIT_SHA_SHORT",
            "AUTUMN_BUILD_GIT_BRANCH",
            "AUTUMN_BUILD_GIT_DIRTY",
            "AUTUMN_BUILD_TIMESTAMP",
        ] {
            assert!(
                content.contains(&format!("ARG {arg}")),
                "Dockerfile must declare `ARG {arg}` for git provenance passthrough: {content}"
            );
            assert!(
                content.contains(&format!("{arg}=${{{arg}}}")),
                "Dockerfile must surface `{arg}` as ENV so build.rs bakes it: {content}"
            );
        }

        // The provenance ENV must precede the build step so the compile sees it.
        let env_pos = content.find("AUTUMN_BUILD_GIT_SHA=${AUTUMN_BUILD_GIT_SHA}");
        let build_pos = content.find("{{build_step}}").or_else(|| {
            // `{{build_step}}` is already substituted; both variants run cargo.
            content
                .find("cargo build --release")
                .or_else(|| content.find("autumn build --embed"))
        });
        assert!(
            matches!((env_pos, build_pos), (Some(e), Some(b)) if e < b),
            "provenance ENV must appear before the build step: {content}"
        );
    }

    #[test]
    fn dockerfile_has_three_stages() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        let from_count = content.lines().filter(|l| l.starts_with("FROM ")).count();
        assert!(
            from_count >= 3,
            "Dockerfile must have at least 3 FROM stages (chef, planner, builder, runtime); got {from_count}"
        );
    }

    #[test]
    fn dockerfile_copies_production_config_as_runtime_autumn_toml() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("autumn.production.toml.example"),
            "Dockerfile must COPY autumn.production.toml.example into the runtime image so \
             the container binds to 0.0.0.0 (not the dev 127.0.0.1) without manual edits"
        );
    }

    #[test]
    fn dockerfile_runtime_uses_debian_slim() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("debian:bookworm-slim"),
            "runtime stage must use debian:bookworm-slim"
        );
    }

    #[test]
    fn dockerfile_runtime_installs_libpq_and_tini() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("libpq"),
            "runtime must install libpq for Diesel"
        );
        assert!(
            content.contains("tini"),
            "runtime must install tini as init process"
        );
        assert!(
            content.contains("ca-certificates"),
            "runtime must install ca-certificates"
        );
    }

    #[test]
    fn dockerfile_uses_disk_build_without_embed_feature() {
        // A project that doesn't define the `embed-assets` feature (e.g. one
        // scaffolded before embedding existed) must get the disk-based build so
        // its Docker build keeps working.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("RUN cargo build --release"),
            "non-embed project must use the disk-based build: {content}"
        );
        assert!(
            !content.contains("autumn build --embed"),
            "non-embed project must not require the embed-assets feature"
        );
        assert!(
            content.contains("/app/static"),
            "non-embed build must COPY the static/ sidecar into the runtime image"
        );
    }

    #[test]
    fn dockerfile_embeds_when_project_has_embed_feature() {
        // A project that opts into the `embed-assets` feature gets a
        // single-binary build with no static/ sidecar.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n\
             [features]\nembed-assets = [\"autumn-web/embed-assets\"]\n",
        )
        .unwrap();
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("autumn build --embed"),
            "embed-feature project must build the embedded single binary: {content}"
        );
        assert!(
            !content.contains("/app/static"),
            "embedded build must not COPY a static/ sidecar into the runtime image"
        );
    }

    #[test]
    fn dockerfile_copies_migrations() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("migrations"),
            "Dockerfile must COPY migrations into runtime image"
        );
    }

    #[test]
    fn dockerfile_defers_migrations_to_one_shot_primary_job() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            !content.contains("auto_migrate_in_production = true"),
            "Dockerfile must not enable startup migrations for every web replica"
        );
        assert!(
            content.contains("autumn migrate"),
            "Dockerfile must document the explicit primary-role migration job"
        );
    }

    #[test]
    fn dockerfile_installs_autumn_cli_for_migration_jobs() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("cargo install")
                && content.contains("autumn-cli")
                && content.contains("/usr/local/bin/autumn"),
            "Dockerfile must include the autumn CLI used by one-shot migration jobs"
        );
    }

    #[test]
    fn dockerfile_installs_diesel_cli_for_migration_jobs() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("cargo install")
                && content.contains("diesel_cli")
                && content.contains("libpq-dev")
                && content.contains("--features postgres")
                && content.contains("/usr/local/bin/diesel"),
            "Dockerfile must include the diesel CLI used by autumn migrate"
        );
    }

    #[test]
    fn dockerfile_has_healthcheck() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("HEALTHCHECK"),
            "Dockerfile must have a HEALTHCHECK directive"
        );
        assert!(
            content.contains("/health"),
            "HEALTHCHECK must probe the /health actuator endpoint"
        );
    }

    #[test]
    fn dockerfile_exposes_port_3000() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("EXPOSE 3000"),
            "Dockerfile must EXPOSE 3000"
        );
    }

    #[test]
    fn dockerfile_substitutes_project_name() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert!(
            content.contains("my-blog"),
            "Dockerfile must contain the substituted project name"
        );
        assert!(
            !content.contains("{{project_name}}"),
            "Dockerfile must not contain unsubstituted {{{{project_name}}}}"
        );
        assert!(
            !content.contains("{{"),
            "Dockerfile must not contain any unsubstituted template placeholders"
        );
    }

    // ── .dockerignore content ─────────────────────────────────────────────────

    #[test]
    fn dockerignore_excludes_target() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        assert!(
            content.contains("target"),
            ".dockerignore must exclude target/"
        );
    }

    #[test]
    fn dockerignore_excludes_git() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        assert!(content.contains(".git"), ".dockerignore must exclude .git");
    }

    #[test]
    fn dockerignore_excludes_node_modules() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        assert!(
            content.contains("node_modules"),
            ".dockerignore must exclude node_modules"
        );
    }

    #[test]
    fn dockerignore_excludes_dist() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        assert!(content.contains("dist"), ".dockerignore must exclude dist/");
    }

    #[test]
    fn dockerignore_excludes_master_key() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        assert!(
            content.contains("/config/master.key") || content.contains("config/master.key"),
            ".dockerignore must exclude config/master.key"
        );
    }

    // ── signing-secret smoke gate ─────────────────────────────────────────────

    #[test]
    fn production_config_template_documents_signing_secret_env_var() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        assert!(
            content.contains("AUTUMN_SECURITY__SIGNING_SECRET"),
            "production config template must document the signing-secret env var"
        );
    }

    #[test]
    fn production_config_template_documents_openssl_rand_command() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        assert!(
            content.contains("openssl rand -hex 32"),
            "production config template must show the secret generation command"
        );
    }

    #[test]
    fn production_config_template_mentions_signing_secrets_guide() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        assert!(
            content.contains("signing-secrets.md"),
            "production config template must link to the signing-secrets guide"
        );
    }

    #[test]
    fn smoke_gate_passes_for_valid_config() {
        let content = r#"
# This is a comment with secret = "ignored"
[server]
port = 3000
"#;
        assert!(check_production_config_signing_secret(content).is_ok());
    }

    #[test]
    fn smoke_gate_fails_when_secret_literal_committed() {
        let content = r#"
[security.signing_secret]
secret = "my-actual-secret-value-here"
"#;
        let err = check_production_config_signing_secret(content).unwrap_err();
        assert!(err.contains("committed signing secret literal"));
    }

    #[test]
    fn smoke_gate_ignores_commented_secret_lines() {
        // Comments are allowed to mention the key name for documentation.
        let content = r#"
# secret = "example-value-for-docs"
# Set AUTUMN_SECURITY__SIGNING_SECRET instead
"#;
        assert!(check_production_config_signing_secret(content).is_ok());
    }

    #[test]
    fn smoke_gate_passes_for_empty_previous_secrets() {
        let content = "
[security.signing_secret]
previous_secrets = []
";
        assert!(check_production_config_signing_secret(content).is_ok());
    }

    // ── production config content ─────────────────────────────────────────────

    #[test]
    fn production_config_has_placeholder_db_url_not_real_credentials() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        // Must have a DB URL entry
        assert!(
            content.contains("DATABASE_URL") || content.contains("url"),
            "production config must document the database URL setting"
        );
        // Must not contain real credentials (no 'password' in a non-commented line)
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.to_lowercase().contains("secret"),
                "production config must not contain real secrets"
            );
        }
    }

    #[test]
    fn production_config_has_placeholder_for_project_name() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        assert!(
            content.contains("my-blog"),
            "production config must substitute project name"
        );
        assert!(
            !content.contains("{{project_name}}"),
            "production config must not contain unsubstituted placeholders"
        );
    }

    #[test]
    fn production_config_documents_port() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        assert!(
            content.contains("port"),
            "production config must document the port setting"
        );
    }

    // ── --force flag ──────────────────────────────────────────────────────────

    #[test]
    fn init_without_force_errors_if_dockerfile_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join("Dockerfile"), "existing content").unwrap();
        let err = init(&dir, "my-app", false, Target::Default, false).unwrap_err();
        assert!(
            matches!(err, ReleaseError::FileExists(_)),
            "expected FileExists, got: {err}"
        );
        assert!(
            err.to_string().contains("Dockerfile"),
            "error message must name the conflicting file"
        );
    }

    #[test]
    fn init_without_force_errors_if_any_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".dockerignore"), "existing").unwrap();
        let err = init(&dir, "my-app", false, Target::Default, false).unwrap_err();
        assert!(matches!(err, ReleaseError::FileExists(_)));
    }

    #[test]
    fn init_with_force_overwrites_existing_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join("Dockerfile"), "old content").unwrap();
        init(&dir, "my-app", true, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("Dockerfile")).unwrap();
        assert_ne!(
            content, "old content",
            "Dockerfile must be overwritten with --force"
        );
    }

    // ── --target=fly ──────────────────────────────────────────────────────────

    #[test]
    fn fly_target_creates_fly_toml() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        assert!(
            dir.join("fly.toml").is_file(),
            "fly.toml must be created for --target=fly"
        );
    }

    #[test]
    fn fly_toml_references_dockerfile() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("Dockerfile"),
            "fly.toml must reference the Dockerfile"
        );
    }

    #[test]
    fn fly_toml_has_app_name() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("my-blog"),
            "fly.toml must contain the app name"
        );
        assert!(
            !content.contains("{{project_name}}"),
            "fly.toml must not contain unsubstituted placeholders"
        );
    }

    #[test]
    fn fly_toml_has_liveness_probe() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("path") && content.contains("/live"),
            "fly.toml must wire /live as the liveness health check"
        );
    }

    #[test]
    fn fly_toml_has_readiness_probe() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("path") && content.contains("/ready"),
            "fly.toml must wire /ready as the readiness check so Fly stops routing \
             traffic before the listener closes during graceful shutdown"
        );
    }

    #[test]
    fn fly_toml_has_kill_timeout() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("kill_timeout"),
            "fly.toml must set kill_timeout so Fly waits at least \
             prestop_grace_secs + shutdown_timeout_secs before sending SIGKILL"
        );
        assert!(
            content.contains("kill_signal"),
            "fly.toml must set kill_signal = \"SIGTERM\" so Autumn receives the expected signal"
        );
    }

    #[test]
    fn fly_toml_has_metrics_endpoint() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("[metrics]"),
            "fly.toml must include a [metrics] section for Fly's Prometheus scraper"
        );
        assert!(
            content.contains("/actuator/prometheus"),
            "fly.toml [metrics] must point to /actuator/prometheus (Prometheus text format), \
             not /actuator/metrics (JSON)"
        );
    }

    #[test]
    fn fly_toml_documents_deploy_release_command() {
        // release_command is commented out by default: autumn migrate exits non-zero
        // when no database URL is configured, which would break non-DB app deploys.
        // The template documents the opt-in pattern so DB users can uncomment it.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Fly, false).unwrap();
        let content = fs::read_to_string(dir.join("fly.toml")).unwrap();
        assert!(
            content.contains("autumn migrate"),
            "fly.toml must document the autumn migrate release command so DB users \
             can uncomment it"
        );
        assert!(
            content.contains("release_command"),
            "fly.toml must document release_command so DB users know where to uncomment"
        );
    }

    #[test]
    fn default_target_does_not_create_fly_toml() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        assert!(
            !dir.join("fly.toml").exists(),
            "fly.toml must NOT be created for the default target"
        );
    }

    // ── --target=docker-compose ───────────────────────────────────────────────

    #[test]
    fn docker_compose_target_creates_docker_compose_yml() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        assert!(
            dir.join("docker-compose.yml").is_file(),
            "docker-compose.yml must be created for --target=docker-compose"
        );
    }

    #[test]
    fn docker_compose_has_app_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();
        assert!(
            content.contains("app:"),
            "docker-compose.yml must have an 'app' service"
        );
    }

    #[test]
    fn docker_compose_has_postgres_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();
        assert!(
            content.contains("postgres") || content.contains("db:"),
            "docker-compose.yml must have a Postgres service"
        );
    }

    #[test]
    fn docker_compose_app_depends_on_db() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();
        assert!(
            content.contains("depends_on"),
            "docker-compose.yml app service must depend_on the db"
        );
    }

    #[test]
    fn docker_compose_app_requires_signing_secret_env() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();

        assert!(
            content.contains(
                r#"AUTUMN_SECURITY__SIGNING_SECRET: "${AUTUMN_SECURITY__SIGNING_SECRET:?set it first}""#
            ),
            "app service must pass the required production signing secret: {content}"
        );
    }

    #[test]
    fn docker_compose_runs_one_shot_migration_before_app() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();

        assert!(
            content.contains("migrate:"),
            "docker-compose.yml must include a one-shot migration service"
        );
        assert!(
            content.contains("autumn migrate"),
            "migration service must run autumn migrate"
        );
        assert!(
            content.contains("condition: service_completed_successfully"),
            "app service must wait for the migration job to complete successfully"
        );
    }

    #[test]
    fn docker_compose_substitutes_project_name() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();
        assert!(
            content.contains("my-blog"),
            "docker-compose.yml must substitute project name"
        );
        assert!(
            !content.contains("{{project_name}}"),
            "docker-compose.yml must not contain unsubstituted placeholders"
        );
    }

    #[test]
    fn default_target_does_not_create_docker_compose() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        assert!(
            !dir.join("docker-compose.yml").exists(),
            "docker-compose.yml must NOT be created for the default target"
        );
    }

    // ── --target=azure-container-apps ─────────────────────────────────────────

    #[test]
    fn azure_target_creates_all_expected_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/azure-deploy.yml",
        ] {
            assert!(
                dir.join(name).is_file(),
                "{name} must be created for --target=azure-container-apps"
            );
        }
        // Base scaffolding is still emitted alongside the Azure-specific files.
        assert!(dir.join("Dockerfile").is_file());
        assert!(dir.join(".dockerignore").is_file());
        assert!(dir.join("autumn.production.toml.example").is_file());
    }

    #[test]
    fn azure_target_returns_nested_workflow_path_in_created_list() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        let files = init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        assert!(
            files
                .iter()
                .any(|f| f == ".github/workflows/azure-deploy.yml"),
            "created-files list must include the nested workflow path: {files:?}"
        );
    }

    #[test]
    fn default_target_does_not_create_azure_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/azure-deploy.yml",
        ] {
            assert!(
                !dir.join(name).exists(),
                "{name} must NOT be created for the default target"
            );
        }
    }

    #[test]
    fn main_tf_has_resource_group_and_registry() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_resource_group"),
            "main.tf must provision a resource group: {content}"
        );
        assert!(
            content.contains("azurerm_container_registry"),
            "main.tf must provision an Azure Container Registry: {content}"
        );
    }

    #[test]
    fn main_tf_has_container_app_environment_and_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_container_app_environment"),
            "main.tf must provision a Container Apps environment: {content}"
        );
        assert!(
            content.contains("resource \"azurerm_container_app\""),
            "main.tf must provision the Container App service: {content}"
        );
    }

    #[test]
    fn main_tf_has_postgres_flexible_server() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_postgresql_flexible_server"),
            "main.tf must provision Azure Database for PostgreSQL Flexible Server: {content}"
        );
    }

    #[test]
    fn main_tf_postgres_server_does_not_hardcode_availability_zone() {
        // Not every Container Apps region offers Postgres Flexible Server
        // availability zone 1 — a hardcoded `zone = "1"` fails
        // `terraform apply` in those regions even though an unzoned server
        // would succeed. Omitting `zone` lets Azure pick a placement itself.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let server_block = content
            .split("resource \"azurerm_postgresql_flexible_server\" \"this\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare the postgresql_flexible_server resource");
        // Check for an actual `zone = ...` attribute assignment, not just
        // the substring "zone" — the resource's own explanatory comment
        // about why zone is omitted legitimately mentions the word.
        assert!(
            !server_block.lines().any(|l| {
                let t = l.trim_start();
                !t.starts_with('#') && t.starts_with("zone ")
            }),
            "the Postgres Flexible Server must not pin an availability zone: {server_block}"
        );
    }

    #[test]
    fn main_tf_postgres_database_name_is_length_bounded() {
        // Postgres database identifiers are capped at 63 bytes; a 64
        // -character Cargo package name (valid) would otherwise overflow
        // it since the name was previously passed through unbounded.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let db_block = content
            .split("resource \"azurerm_postgresql_flexible_server_database\" \"this\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare the postgresql_flexible_server_database resource");
        let name_line = db_block
            .lines()
            .find(|l| l.trim_start().starts_with("name"))
            .expect("the database resource must set a name");
        assert!(
            name_line.contains("substr(") && name_line.contains(", 63)"),
            "the Postgres database name must be truncated to 63 characters: {name_line}"
        );
    }

    #[test]
    fn main_tf_has_key_vault_with_database_and_signing_secrets() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_key_vault"),
            "main.tf must provision a Key Vault secrets store: {content}"
        );
        assert!(
            content.contains("AUTUMN_DATABASE__PRIMARY_URL"),
            "main.tf must wire the primary DB URL env var from Key Vault: {content}"
        );
        assert!(
            content.contains("AUTUMN_SECURITY__SIGNING_SECRET"),
            "main.tf must wire the signing secret env var from Key Vault: {content}"
        );
    }

    #[test]
    fn main_tf_grants_key_vault_access_to_terraform_identity() {
        // Access-policy-model Key Vaults grant NO data-plane access by
        // default (subscription-level Owner/Contributor does not imply Key
        // Vault secret access), so without a policy for Terraform's own
        // caller identity, `terraform apply` fails at the
        // azurerm_key_vault_secret resources with a 403.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("data.azurerm_client_config.current.object_id"),
            "main.tf must grant Key Vault access to Terraform's own caller \
             identity (data.azurerm_client_config.current.object_id), not \
             just the container app's identity: {content}"
        );
    }

    #[test]
    fn main_tf_key_vault_name_never_exceeds_azure_length_limit() {
        // Azure Key Vault names are capped at 24 characters. Extract the
        // substr() bound and random_id byte_length from the template and
        // verify the worst-case rendered name (prefix + "kv" + hex suffix)
        // fits, so a future edit to either constant can't silently regress
        // past the limit.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();

        let substr_bound: usize = content
            .split("substr(local.app_name_alnum, 0, ")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .and_then(|n| n.trim().parse().ok())
            .expect("main.tf must call substr(local.app_name_alnum, 0, N) for the vault name");
        let byte_length: usize = content
            .split("byte_length = ")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .and_then(|n| n.trim().parse().ok())
            .expect("main.tf must declare random_id.suffix's byte_length");
        let hex_len = byte_length * 2;
        let worst_case_len = substr_bound + "kv".len() + hex_len;
        assert!(
            worst_case_len <= 24,
            "worst-case Key Vault name is {worst_case_len} chars (substr={substr_bound} + \
             \"kv\" + {hex_len} hex chars), exceeding Azure's 24-char limit: {content}"
        );
    }

    #[test]
    fn main_tf_sanitizes_names_consistently_via_shared_local() {
        // Postgres Flexible Server and Redis Cache names must use the same
        // sanitized local as ACR/Key Vault, not raw var.app_name — a Cargo
        // package name may contain underscores/uppercase that are invalid
        // in those resource names.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();

        let postgres_block = content
            .split("resource \"azurerm_postgresql_flexible_server\" \"this\"")
            .nth(1)
            .unwrap();
        let postgres_name_line = postgres_block
            .lines()
            .find(|l| l.trim_start().starts_with("name"))
            .unwrap();
        assert!(
            postgres_name_line.contains("local.app_name_alnum"),
            "Postgres server name must use the sanitized local: {postgres_name_line}"
        );

        let redis_block = content
            .split("resource \"azurerm_redis_cache\" \"this\"")
            .nth(1)
            .unwrap();
        let redis_name_line = redis_block
            .lines()
            .find(|l| l.trim_start().starts_with("name"))
            .unwrap();
        assert!(
            redis_name_line.contains("local.app_name_alnum"),
            "Redis cache name must use the sanitized local: {redis_name_line}"
        );
    }

    #[test]
    fn main_tf_derives_database_url_from_created_postgres_server() {
        // A single `terraform apply` must be enough: the connection string
        // is computed from the Postgres server this same apply creates
        // (its FQDN + the admin password variable), never taken as a
        // separate pre-computed `var.database_url` input.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_postgresql_flexible_server.this.fqdn"),
            "main.tf must derive the database URL from the Postgres server's own FQDN: {content}"
        );
        assert!(
            !content.contains("var.database_url"),
            "main.tf must not reference a database_url variable: {content}"
        );
    }

    #[test]
    fn main_tf_container_apps_family_resources_use_sanitized_name() {
        // Log Analytics, the Container Apps environment, the app, its
        // container, and the migration job all require lowercase
        // alphanumerics-and-hyphens — unlike ACR/Key Vault/Postgres/Redis,
        // they DO allow hyphens, so they share `local.app_name_safe` (not
        // `app_name_alnum`, which strips hyphens too) rather than raw
        // `var.app_name`, which may contain underscores/uppercase from a
        // Cargo package name.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();

        for resource in [
            "azurerm_log_analytics_workspace\" \"this",
            "azurerm_container_app_environment\" \"this",
            "azurerm_container_app\" \"this",
            "azurerm_container_app_job\" \"migrate",
        ] {
            let block = content
                .split(&format!("resource \"{resource}\""))
                .nth(1)
                .unwrap_or_else(|| {
                    panic!("main.tf must declare resource \"{resource}\": {content}")
                });
            let name_line = block
                .lines()
                .find(|l| l.trim_start().starts_with("name"))
                .unwrap_or_else(|| panic!("{resource} must set a name: {block}"));
            assert!(
                name_line.contains("local.app_name_safe"),
                "{resource} must use the sanitized local.app_name_safe, not raw \
                 var.app_name or a literal project name: {name_line}"
            );
        }

        assert!(
            !content.contains("\"{{project_name}}\""),
            "main.tf must not hardcode the raw (unsanitized) project name as a \
             resource identifier: {content}"
        );
    }

    #[test]
    fn main_tf_app_name_safe_collapses_and_trims_hyphens() {
        // Naively mapping every invalid character to "-" turns "my__app"
        // into "my--app" (consecutive hyphens, invalid) and "my-" into a
        // name with a trailing hyphen (also invalid — Container App names
        // must end in an alphanumeric character). The locals must collapse
        // hyphen runs and trim leading/trailing hyphens after substitution.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let locals_block = content
            .split("locals {")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare a locals block");
        assert!(
            locals_block.contains("\"/-+/\""),
            "app_name derivation must collapse runs of hyphens to one via a regex like \
             /-+/: {locals_block}"
        );
        assert!(
            locals_block.matches("trim(").count() >= 2,
            "app_name derivation must trim leading/trailing hyphens both after collapsing \
             and after any length truncation: {locals_block}"
        );
    }

    #[test]
    fn main_tf_app_name_safe_is_length_bounded() {
        // Azure Container Apps-family names must be 2-32 characters. A
        // 1-character Cargo package name (valid) would produce a
        // below-minimum app name; a >24-character one would push
        // "${app_name_safe}-migrate" (8-char suffix) past the 32-char cap.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let locals_block = content
            .split("locals {")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare a locals block");
        assert!(
            locals_block.contains("substr("),
            "app_name_safe must truncate to leave headroom for the longest suffix \
             (-migrate, 8 chars) appended to any Container Apps-family resource: \
             {locals_block}"
        );
        assert!(
            locals_block.contains("length(local.app_name_hyphenated) < 2"),
            "app_name_safe must pad a too-short base up to Azure's 2-character minimum: \
             {locals_block}"
        );
    }

    #[test]
    fn main_tf_app_name_alnum_is_length_bounded() {
        // ACR names are capped at 50 characters; "${app_name_alnum}acr" +
        // an 8-hex-char suffix reserves 11, so an unbounded app_name_alnum
        // (Cargo package names may be much longer than 39 characters)
        // overflows it. Postgres (63) and Redis (63) are more permissive
        // but derive from the same local, so bounding it once covers all
        // three rather than truncating per-resource.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let locals_block = content
            .split("locals {")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare a locals block");
        let alnum_line = locals_block
            .lines()
            .find(|l| l.trim_start().starts_with("app_name_alnum ="))
            .expect("locals must declare app_name_alnum");
        assert!(
            alnum_line.contains("substr("),
            "app_name_alnum must be length-bounded so ACR's 50-character limit (after \
             the fixed \"acr\" + 8-hex-char suffix) can never be exceeded: {alnum_line}"
        );
    }

    #[test]
    fn main_tf_uses_bootstrap_image_and_ignores_later_image_drift() {
        // Container Apps must pull an image to create the app's/job's first
        // revision, but a brand-new ACR has none yet, so Terraform points
        // both at a public placeholder and then ignores further image
        // changes so a later `terraform apply` doesn't revert a live
        // `az containerapp update`/job deploy back to the placeholder.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("image  = var.bootstrap_image")
                || content.contains("image = var.bootstrap_image"),
            "main.tf must set the container image to var.bootstrap_image: {content}"
        );
        assert_eq!(
            content
                .matches("ignore_changes = [template[0].container[0].image]")
                .count(),
            2,
            "both the app and the migration job must ignore image drift after bootstrap: {content}"
        );
        assert!(
            !content.contains("${var.image_tag}"),
            "main.tf must not build the Terraform-managed image from var.image_tag — \
             CI manages the real image out-of-band after bootstrap: {content}"
        );
    }

    #[test]
    fn main_tf_has_migration_job() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_container_app_job\" \"migrate\""),
            "main.tf must provision a one-shot migration Container Apps Job: {content}"
        );
        assert!(
            content.contains("manual_trigger_config"),
            "the migration job must only run when CI explicitly starts it: {content}"
        );
        assert!(
            content.contains("autumn migrate"),
            "the migration job must run `autumn migrate`: {content}"
        );
        assert!(
            content.contains("AUTUMN_DATABASE__PRIMARY_URL"),
            "the migration job must be wired to the same database secret as the app: {content}"
        );
    }

    #[test]
    fn main_tf_wires_redis_url_into_container_app_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_key_vault_secret\" \"redis_url\""),
            "main.tf must store the Redis connection string in Key Vault: {content}"
        );
        // Autumn's actual config path is `[cache.redis] url` (env:
        // AUTUMN_CACHE__REDIS__URL, double underscore before URL) — not
        // AUTUMN_CACHE__REDIS_URL, which Autumn never reads.
        assert!(
            content.contains("AUTUMN_CACHE__REDIS__URL"),
            "main.tf must wire AUTUMN_CACHE__REDIS__URL into the Container App \
             when enable_redis_cache is true: {content}"
        );
        assert!(
            !content.contains("AUTUMN_CACHE__REDIS_URL\""),
            "main.tf must not use the single-underscore variant, which Autumn ignores: {content}"
        );
        // Without selecting the backend, Autumn stays on its default
        // in-memory cache and never reads the URL at all.
        assert!(
            content.contains("name  = \"AUTUMN_CACHE__BACKEND\"")
                || content.contains("name = \"AUTUMN_CACHE__BACKEND\""),
            "main.tf must set AUTUMN_CACHE__BACKEND=redis so Autumn actually selects the \
             Redis cache backend: {content}"
        );
        assert!(
            content.contains("value = \"redis\""),
            "AUTUMN_CACHE__BACKEND must be set to \"redis\": {content}"
        );
    }

    #[test]
    fn main_tf_urlencodes_redis_access_key() {
        // Azure Redis access keys are base64-like and may contain "/" — raw
        // in a URL's userinfo segment, that would terminate the authority
        // before "@hostname" and produce a malformed URL depending on the
        // randomly issued key.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("urlencode(azurerm_redis_cache.this[0].primary_access_key)"),
            "the Redis access key must be urlencode()'d before being interpolated into \
             the rediss:// URL, the same way database_admin_password already is: {content}"
        );
    }

    #[test]
    fn main_tf_documents_redis_cache_requires_app_level_plugin() {
        // Provisioning the cache and wiring its env vars is infrastructure
        // only — Autumn's cache subsystem has no built-in Redis
        // implementation (unlike sessions/channels/jobs), so the app must
        // ALSO depend on autumn-cache-redis and register RedisCachePlugin,
        // or the env vars are silently never read.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            main_tf.contains("RedisCachePlugin"),
            "main.tf must document that the app needs RedisCachePlugin registered, not \
             just the env vars set: {main_tf}"
        );
        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            variables_tf.to_lowercase().contains("infrastructure only"),
            "variables.tf's enable_redis_cache description must warn this is \
             infrastructure-only: {variables_tf}"
        );
    }

    #[test]
    fn main_tf_has_optional_redis_cache_gated_by_variable() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("azurerm_redis_cache"),
            "main.tf must optionally provision a Redis Cache: {content}"
        );
        assert!(
            content.contains("enable_redis_cache"),
            "the Redis Cache must be gated by an opt-in feature-flag variable: {content}"
        );
    }

    #[test]
    fn main_tf_never_contains_secret_literals() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            let lower = trimmed.to_lowercase();
            assert!(
                !lower.contains("password = \"") && !lower.contains("password=\""),
                "main.tf must never assign a literal secret value (only var./data. \
                 references are allowed): {trimmed}"
            );
        }
    }

    #[test]
    fn main_tf_substitutes_project_name() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("my-blog"),
            "main.tf must substitute the project name: {content}"
        );
        assert!(
            !content.contains("{{"),
            "main.tf must not contain unsubstituted template placeholders: {content}"
        );
    }

    #[test]
    fn variables_tf_declares_expected_inputs() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        for var in [
            "variable \"app_name\"",
            "variable \"location\"",
            "variable \"image_tag\"",
            "variable \"db_sku\"",
            "variable \"min_replicas\"",
            "variable \"max_replicas\"",
            "variable \"enable_redis_cache\"",
        ] {
            assert!(
                content.contains(var),
                "variables.tf must declare {var}: {content}"
            );
        }
        assert!(
            !content.contains("{{"),
            "variables.tf must not contain unsubstituted template placeholders: {content}"
        );
    }

    #[test]
    fn variables_tf_marks_secret_inputs_sensitive_with_no_default() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            content.contains("variable \"database_admin_password\""),
            "variables.tf must declare a database_admin_password secret variable: {content}"
        );
        assert!(
            content.contains("variable \"signing_secret\""),
            "variables.tf must declare a signing_secret secret variable: {content}"
        );
        assert!(
            content.contains("sensitive   = true") || content.contains("sensitive = true"),
            "secret variables must be marked sensitive so Terraform redacts them in \
             plan/apply output: {content}"
        );
        assert!(
            !content.to_lowercase().contains("default     = \"postgres")
                && !content.contains("default = \"CHANGE_ME\""),
            "secret variables must not ship a literal default value: {content}"
        );
    }

    #[test]
    fn variables_tf_does_not_declare_database_url() {
        // A single `terraform apply` must succeed without a second, targeted
        // apply to fill in a value that depends on a resource the same apply
        // is about to create. main.tf derives the connection string from the
        // Postgres server it creates instead of requiring this as an input —
        // regression guard against reintroducing that two-apply footgun.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            !content.contains("variable \"database_url\""),
            "variables.tf must NOT declare a database_url variable — it must be derived \
             in main.tf from the Postgres server the same apply creates: {content}"
        );
    }

    #[test]
    fn variables_tf_declares_bootstrap_image() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            content.contains("variable \"bootstrap_image\""),
            "variables.tf must declare bootstrap_image, the placeholder image Terraform \
             uses before any real image has been pushed to ACR: {content}"
        );
    }

    #[test]
    fn main_tf_provider_sets_subscription_id() {
        // AzureRM v4 made subscription_id mandatory for plan/apply, even
        // under `az login` CLI auth — without it, `terraform apply` fails
        // before provisioning anything.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        let provider_block = main_tf
            .split("provider \"azurerm\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare the azurerm provider block");
        assert!(
            provider_block.contains("subscription_id = var.subscription_id"),
            "the azurerm provider block must set subscription_id: {provider_block}"
        );

        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            variables_tf.contains("variable \"subscription_id\""),
            "variables.tf must declare subscription_id: {variables_tf}"
        );

        let tfvars = fs::read_to_string(dir.join("terraform.tfvars.example")).unwrap();
        assert!(
            tfvars.contains("subscription_id"),
            "terraform.tfvars.example must mention subscription_id so operators don't \
             discover the AzureRM v4 requirement only after `terraform apply` fails: {tfvars}"
        );
    }

    #[test]
    fn tfvars_example_generates_postgres_compliant_admin_password() {
        // Azure Postgres Flexible Server requires the admin password to use
        // characters from at least 3 of {uppercase, lowercase, digit,
        // symbol}. `openssl rand -hex` only ever produces lowercase hex
        // digits (2 of 4 categories); even `-base64` alone only samples its
        // alphabet randomly and could still land on just 2 categories. The
        // command must deterministically guarantee coverage, not rely on
        // probability, by appending a fixed suffix containing all 4.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("terraform.tfvars.example")).unwrap();
        assert!(
            content.contains("TF_VAR_database_admin_password=\"$(openssl rand -base64 18)Aa1!\""),
            "the documented database_admin_password generator must append a fixed \
             upper/lower/digit/symbol suffix so all 4 character classes are guaranteed, \
             not merely probable: {content}"
        );
    }

    #[test]
    fn outputs_tf_has_app_fqdn_and_acr_login_server() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        assert!(
            content.contains("output \"app_fqdn\""),
            "outputs.tf must expose the app's FQDN: {content}"
        );
        assert!(
            content.contains("output \"acr_login_server\""),
            "outputs.tf must expose the ACR login server: {content}"
        );
        assert!(
            content.contains("output \"migrate_job_name\""),
            "outputs.tf must expose the migration job's name so CI can start it: {content}"
        );
        assert!(
            content.contains("output \"app_name\""),
            "outputs.tf must expose the sanitized Container App name so CI never has to \
             hardcode it: {content}"
        );
        assert!(
            !content.contains("{{"),
            "outputs.tf must not contain unsubstituted template placeholders: {content}"
        );
    }

    #[test]
    fn tfvars_example_documents_non_secret_defaults_without_committing_secrets() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("terraform.tfvars.example")).unwrap();
        assert!(
            content.contains("my-blog"),
            "terraform.tfvars.example must substitute the project name: {content}"
        );
        assert!(
            content.contains("app_name"),
            "terraform.tfvars.example must document app_name: {content}"
        );
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            assert!(
                !trimmed.starts_with("database_url") && !trimmed.starts_with("signing_secret"),
                "terraform.tfvars.example must never assign a literal secret value: {trimmed}"
            );
        }
        assert!(
            !content.contains("{{"),
            "terraform.tfvars.example must not contain unsubstituted placeholders: {content}"
        );
    }

    #[test]
    fn azure_workflow_triggers_on_tag_push_and_manual_dispatch() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("tags:"),
            "azure-deploy.yml must trigger on tag push: {content}"
        );
        assert!(
            content.contains("workflow_dispatch:"),
            "azure-deploy.yml must also support manual dispatch: {content}"
        );
    }

    #[test]
    fn azure_workflow_documents_resource_group_scope_rbac() {
        // RBAC granted on the Container App does not inherit to the
        // sibling migration Container Apps Job — a service principal with
        // Contributor scoped only to the app would 403 when the migration
        // step tries to start the job. The header must say resource-group
        // scope, not "on the Container App".
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        // Scope to the header comment (before the workflow body) — the body
        // legitimately contains `--resource-group` CLI flags, which would
        // make a bare substring check pass regardless of whether the header
        // actually documents the RBAC scoping requirement.
        let header = content
            .split("\nname: azure-deploy")
            .next()
            .expect("azure-deploy.yml must have a header comment before `name:`");
        let header_lower = header.to_lowercase();
        assert!(
            header_lower.contains("resource-group") || header_lower.contains("resource group"),
            "azure-deploy.yml's header must document resource-group-scoped Contributor \
             access, covering both the app and the migration job: {header}"
        );
        assert!(
            header_lower.contains("contributor"),
            "azure-deploy.yml's header must mention the Contributor role: {header}"
        );
    }

    #[test]
    fn azure_workflow_builds_pushes_to_acr_and_deploys() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("docker build") || content.contains("docker/build-push-action"),
            "azure-deploy.yml must build the release image: {content}"
        );
        assert!(
            content.contains("azurecr.io"),
            "azure-deploy.yml must push to the Azure Container Registry: {content}"
        );
        assert!(
            content.contains("az containerapp update")
                || content.contains("containerapps-deploy-action"),
            "azure-deploy.yml must deploy the new image to the Container App: {content}"
        );
    }

    #[test]
    fn azure_workflow_never_hardcodes_credentials() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("secrets."),
            "azure-deploy.yml must source credentials from GitHub Actions secrets, \
             never hardcode them: {content}"
        );
    }

    #[test]
    fn azure_workflow_runs_migrations_before_updating_the_app() {
        // The generated production config sets auto_migrate_in_production =
        // false, so nothing else runs migrations; without this step, new
        // replicas would start against an unmigrated schema on any release
        // that includes a migration.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("az containerapp job start"),
            "azure-deploy.yml must start the one-shot migration job: {content}"
        );
        assert!(
            content.contains("AZURE_MIGRATE_JOB_NAME"),
            "azure-deploy.yml must reference the migration job by its Terraform output: {content}"
        );

        // Match the actual invocations (with their line-continuation
        // backslash), not just the bare phrase — an explanatory comment
        // elsewhere (e.g. about concurrency) may legitimately mention
        // "az containerapp update" in prose without a trailing "\".
        let job_pos = content
            .find("az containerapp job start \\")
            .expect("migration job start must be present");
        let deploy_pos = content
            .find("az containerapp update \\")
            .expect("deploy step must be present");
        assert!(
            job_pos < deploy_pos,
            "the migration job must run BEFORE the app is updated to the new image: {content}"
        );
    }

    #[test]
    fn azure_workflow_aborts_deploy_on_migration_failure() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        let migration_step = content
            .split("Run database migrations")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("a 'Run database migrations' step must exist");
        assert!(
            migration_step.contains("set -euo pipefail") || migration_step.contains("exit 1"),
            "the migration step must fail the job (and therefore never reach the deploy \
             step) when the job execution doesn't succeed: {migration_step}"
        );
    }

    #[test]
    fn azure_workflow_sources_app_name_from_terraform_not_hardcoded() {
        // If an operator edits `app_name` in terraform.tfvars after
        // scaffolding, Terraform renames the Container App to match — the
        // workflow must follow that rename, not target whatever the Cargo
        // package was called when `autumn release init` ran. So it reads
        // AZURE_APP_NAME from `terraform output app_name` (like
        // AZURE_RESOURCE_GROUP/AZURE_MIGRATE_JOB_NAME) rather than having
        // any project-derived name baked in at scaffold time.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "My_Test_App");
        init(
            &dir,
            "My_Test_App",
            false,
            Target::AzureContainerApps,
            false,
        )
        .unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("AZURE_APP_NAME"),
            "azure-deploy.yml must reference AZURE_APP_NAME: {content}"
        );
        assert!(
            content.contains("vars.AZURE_APP_NAME"),
            "AZURE_APP_NAME must be sourced from a repository variable \
             (terraform output app_name), not hardcoded: {content}"
        );
        assert!(
            !content.contains("My_Test_App") && !content.contains("my-test-app"),
            "azure-deploy.yml must not bake in any form of the project name as an \
             Azure resource identifier: {content}"
        );
        assert!(
            !content.contains("{{azure_app_name}}") && !content.contains("{{project_name}}"),
            "azure-deploy.yml must not contain unsubstituted placeholders: {content}"
        );
    }

    #[test]
    fn azure_workflow_sanitizes_ref_name_for_docker_tag() {
        // A `v*` push tag or a workflow_dispatch branch name may contain
        // characters Docker tags reject beyond just "/" (a branch like
        // "feature/login") — e.g. "+" (a valid SemVer tag like
        // "v1.2.3+build"). Docker tags only allow [A-Za-z0-9_.-], so every
        // other character must be sanitized, not just "/" special-cased.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("tr -c 'A-Za-z0-9_.-' '-'"),
            "azure-deploy.yml must map every character outside Docker's tag charset \
             (not just \"/\") to \"-\": {content}"
        );
        assert!(
            !content.contains(":${GITHUB_REF_NAME}") && !content.contains(":$GITHUB_REF_NAME"),
            "no docker/az command may use the raw, unsanitized ref as an image tag: {content}"
        );
    }

    #[test]
    fn azure_workflow_image_tag_includes_commit_sha() {
        // Two workflow_dispatch runs on the same branch would otherwise
        // compute the identical tag despite different commits. Re-pushing
        // bytes under a tag Azure already has configured on the Container
        // App isn't guaranteed to register as a revision-scope change, so
        // the old binary could keep serving against a newly migrated
        // schema. The commit SHA is always unique per run.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("${GITHUB_SHA:0:12}"),
            "the computed image tag must include the commit SHA, not just the \
             sanitized ref, so repeated runs on the same branch never collide: {content}"
        );
        // Docker tags cap at 128 characters; reserve room for the SHA
        // suffix rather than letting the sanitized ref alone consume it.
        assert!(
            content.contains("cut -c1-100"),
            "the sanitized ref portion must leave headroom for the SHA suffix within \
             Docker's 128-character tag limit: {content}"
        );
    }

    #[test]
    fn azure_workflow_serializes_overlapping_runs() {
        // Two overlapping runs (e.g. two rapid tag pushes, or a tag push
        // racing a manual dispatch) must not interleave: the older run's
        // later `az containerapp update` could execute after the newer one
        // and roll production back.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("concurrency:"),
            "azure-deploy.yml must define a concurrency group so overlapping runs \
             queue instead of racing: {content}"
        );
        assert!(
            content.contains("cancel-in-progress: false"),
            "cancel-in-progress must be false — killing a run mid-migration or \
             mid-cutover is worse than making the next run wait: {content}"
        );
    }

    #[test]
    fn init_without_force_errors_if_azure_workflow_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(dir.join(".github/workflows/azure-deploy.yml"), "existing").unwrap();
        let err = init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap_err();
        assert!(matches!(err, ReleaseError::FileExists(_)));
    }

    #[test]
    fn azure_target_adds_terraform_gitignore_entries_when_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        for line in AZURE_GITIGNORE_ENTRIES {
            assert!(
                content.lines().any(|l| l.trim() == *line),
                "azure target must add `{line}` to .gitignore: {content}"
            );
        }
    }

    #[test]
    fn azure_target_gitignore_merge_preserves_existing_lines_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), "/target\n.env\n").unwrap();

        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let after_first = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(after_first.contains("/target"), "{after_first}");
        assert!(after_first.contains(".env"), "{after_first}");
        for line in AZURE_GITIGNORE_ENTRIES {
            assert!(
                after_first.lines().any(|l| l.trim() == *line),
                "{after_first}"
            );
        }

        // Re-running (e.g. under --force) must not duplicate entries.
        init(&dir, "my-app", true, Target::AzureContainerApps, false).unwrap();
        let after_second = fs::read_to_string(dir.join(".gitignore")).unwrap();
        for line in AZURE_GITIGNORE_ENTRIES {
            let count = after_second.lines().filter(|l| l.trim() == *line).count();
            assert_eq!(
                count, 1,
                "`{line}` must appear exactly once: {after_second}"
            );
        }
    }

    #[test]
    fn non_azure_targets_do_not_modify_gitignore() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), "/target\n").unwrap();
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(
            content, "/target\n",
            "non-azure targets must leave .gitignore untouched"
        );
    }

    #[test]
    fn init_with_force_overwrites_nested_azure_workflow_file() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(
            dir.join(".github/workflows/azure-deploy.yml"),
            "old content",
        )
        .unwrap();
        init(&dir, "my-app", true, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert_ne!(content, "old content");
    }

    // ── --split-workers (opt-in split topology) ───────────────────────────────

    #[test]
    fn docker_compose_default_omits_worker_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, false).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();
        assert!(
            !content.contains("worker:"),
            "default docker-compose.yml must NOT scaffold a worker service: {content}"
        );
        assert!(
            !content.contains("AUTUMN_ROLE"),
            "default (combined) docker-compose.yml must not pin a process role: {content}"
        );
        // No leftover template placeholders.
        assert!(
            !content.contains("{{"),
            "docker-compose.yml must not contain unsubstituted placeholders: {content}"
        );
    }

    #[test]
    fn docker_compose_split_workers_emits_worker_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::DockerCompose, true).unwrap();
        let content = fs::read_to_string(dir.join("docker-compose.yml")).unwrap();

        assert!(
            content.contains("worker:"),
            "--split-workers must scaffold a worker service: {content}"
        );
        // Worker runs the SAME image as the web/app service.
        assert_eq!(
            content.matches("build: .").count(),
            3,
            "app, worker, and migrate services must all build the same image: {content}"
        );
        assert!(
            content.contains("AUTUMN_ROLE: worker"),
            "worker service must run the worker role: {content}"
        );
        assert!(
            content.contains("AUTUMN_ROLE: web"),
            "app service must run the web role when split: {content}"
        );
        // Both tiers must use a durable jobs backend (not in-process `local`).
        assert_eq!(
            content.matches("AUTUMN_JOBS__BACKEND: postgres").count(),
            2,
            "both app and worker must share the durable postgres jobs backend: {content}"
        );
        // Worker shares the migration gate and signing secret.
        assert!(
            content.contains("condition: service_completed_successfully"),
            "worker must wait for the one-shot migration job: {content}"
        );
        assert!(
            content.contains(
                r#"AUTUMN_SECURITY__SIGNING_SECRET: "${AUTUMN_SECURITY__SIGNING_SECRET:?set it first}""#
            ),
            "worker must pass the required signing secret like the app service: {content}"
        );
        // Placeholders (including the worker block's own project name) resolved.
        assert!(
            !content.contains("{{"),
            "split docker-compose.yml must not contain unsubstituted placeholders: {content}"
        );
        assert!(
            content.contains("my-app_prod"),
            "worker DB URL must substitute the project name: {content}"
        );
    }

    #[test]
    fn split_workers_only_affects_docker_compose_output() {
        // The split-topology flag is scoped to the compose file; the Dockerfile
        // and other scaffolds are byte-for-byte identical regardless.
        let default_dockerfile = render(templates::DOCKERFILE, "my-app", false, false);
        let split_dockerfile = render(templates::DOCKERFILE, "my-app", false, true);
        assert_eq!(
            default_dockerfile, split_dockerfile,
            "--split-workers must not change the Dockerfile"
        );
    }

    // ── workspace root error ──────────────────────────────────────────────────

    #[test]
    fn workspace_root_gives_actionable_hint() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"my-app\"]\n",
        )
        .unwrap();
        let err = read_project_name(&dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("workspace"),
            "error must mention workspace: {msg}"
        );
        assert!(
            msg.contains("member"),
            "error must hint to run from a member directory: {msg}"
        );
    }

    // ── auto-migration config ─────────────────────────────────────────────────

    #[test]
    fn production_config_disables_startup_migrations_by_default() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        let content = fs::read_to_string(dir.join("autumn.production.toml.example")).unwrap();
        assert!(
            content.contains("auto_migrate_in_production = false"),
            "production config must leave web replicas out of migration ownership"
        );
        assert!(
            content.contains("primary_url"),
            "production config must name the primary/write database role"
        );
        assert!(
            content.contains("autumn migrate"),
            "production config must point operators at the one-shot migration command"
        );
    }

    // ── target parsing ────────────────────────────────────────────────────────

    #[test]
    fn parse_target_fly() {
        assert_eq!("fly".parse::<Target>().unwrap(), Target::Fly);
    }

    #[test]
    fn parse_target_docker_compose() {
        assert_eq!(
            "docker-compose".parse::<Target>().unwrap(),
            Target::DockerCompose
        );
    }

    #[test]
    fn parse_target_azure_container_apps() {
        assert_eq!(
            "azure-container-apps".parse::<Target>().unwrap(),
            Target::AzureContainerApps
        );
    }

    #[test]
    fn parse_target_unknown_is_error() {
        assert!("kubernetes".parse::<Target>().is_err());
    }
}
