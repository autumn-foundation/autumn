//! Production deployment scaffolding for `autumn release init`.
//!
//! Emits a curated set of files (Dockerfile, .dockerignore, config example,
//! and optional target-specific scaffolds) at the project root.

use std::fs;
use std::path::{Path, PathBuf};

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

    pub const AWS_APP_RUNNER_MAIN_TF: &str =
        include_str!("templates/release/aws-app-runner-main.tf.tmpl");
    pub const AWS_APP_RUNNER_VARIABLES_TF: &str =
        include_str!("templates/release/aws-app-runner-variables.tf.tmpl");
    pub const AWS_APP_RUNNER_OUTPUTS_TF: &str =
        include_str!("templates/release/aws-app-runner-outputs.tf.tmpl");
    pub const AWS_APP_RUNNER_TFVARS_EXAMPLE: &str =
        include_str!("templates/release/aws-app-runner-terraform.tfvars.example.tmpl");

    pub const AWS_ECS_MAIN_TF: &str = include_str!("templates/release/aws-ecs-main.tf.tmpl");
    pub const AWS_ECS_VARIABLES_TF: &str =
        include_str!("templates/release/aws-ecs-variables.tf.tmpl");
    pub const AWS_ECS_OUTPUTS_TF: &str = include_str!("templates/release/aws-ecs-outputs.tf.tmpl");
    pub const AWS_ECS_TFVARS_EXAMPLE: &str =
        include_str!("templates/release/aws-ecs-terraform.tfvars.example.tmpl");
    pub const AWS_DEPLOY_WORKFLOW: &str = include_str!("templates/release/aws-deploy.yml.tmpl");

    pub const GCP_CLOUD_RUN_MAIN_TF: &str =
        include_str!("templates/release/gcp-cloud-run-main.tf.tmpl");
    pub const GCP_CLOUD_RUN_VARIABLES_TF: &str =
        include_str!("templates/release/gcp-cloud-run-variables.tf.tmpl");
    pub const GCP_CLOUD_RUN_OUTPUTS_TF: &str =
        include_str!("templates/release/gcp-cloud-run-outputs.tf.tmpl");
    pub const GCP_CLOUD_RUN_TFVARS_EXAMPLE: &str =
        include_str!("templates/release/gcp-cloud-run-terraform.tfvars.example.tmpl");
    pub const GCP_DEPLOY_WORKFLOW: &str = include_str!("templates/release/gcp-deploy.yml.tmpl");
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
    AwsAppRunner,
    AwsEcs,
    GcpCloudRun,
}

impl std::str::FromStr for Target {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fly" => Ok(Self::Fly),
            "docker-compose" => Ok(Self::DockerCompose),
            "azure-container-apps" => Ok(Self::AzureContainerApps),
            "aws-app-runner" => Ok(Self::AwsAppRunner),
            "aws-ecs" => Ok(Self::AwsEcs),
            "gcp-cloud-run" => Ok(Self::GcpCloudRun),
            other => Err(format!(
                "unknown target '{other}'; expected 'fly', 'docker-compose', \
                 'azure-container-apps', 'aws-app-runner', 'aws-ecs', or 'gcp-cloud-run'"
            )),
        }
    }
}

/// Whether `target` scaffolds a Terraform-based deployment (as opposed to
/// `fly`/`docker-compose`'s plain config files). Terraform targets share two
/// behaviors: a merged `.gitignore` (state files hold every secret in
/// plaintext) and, when their workflow lands under a nested Cargo workspace
/// member, the same GitHub Actions discoverability warning.
const fn is_terraform_target(target: Target) -> bool {
    matches!(
        target,
        Target::AzureContainerApps | Target::AwsAppRunner | Target::AwsEcs | Target::GcpCloudRun
    )
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

                    let workflow_file = planned_files(target)
                        .into_iter()
                        .map(|(name, _)| name)
                        .find(|name| name.starts_with(".github/workflows/"));
                    if let Some(workflow_file) = workflow_file
                        && let Some(warning) =
                            nested_workflow_relocation_warning(&cwd, workflow_file)
                    {
                        eprintln!("\n{warning}");
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

/// Locate the nearest ancestor of `dir` (inclusive) containing a `.git`
/// entry. A worktree or submodule uses a `.git` FILE rather than a
/// directory, so this checks existence generally rather than requiring a
/// directory. Returns `None` if no ancestor up to the filesystem root has
/// one (`dir` isn't inside a git repository at all).
fn find_git_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// For targets that scaffold a nested workflow file (currently
/// `.github/workflows/azure-deploy.yml` and `.github/workflows/aws-deploy.yml`),
/// `workflow_rel_path` is written under `dir` — but GitHub Actions only
/// discovers workflow files under the git repository ROOT's
/// `.github/workflows/`, never an arbitrary subdirectory's (see
/// <https://docs.github.com/en/actions/concepts/workflows-and-actions/workflows>).
/// `autumn release init` explicitly supports running from a Cargo workspace
/// member directory (`read_project_name` rejects only the workspace root
/// itself), so a scaffold run from `examples/blog/` writes a workflow that
/// would silently never fire. Returns an actionable warning to print in
/// that case; `None` when `dir` IS the git root (the common, correct case)
/// or isn't inside a git repository at all (nothing more specific to say).
fn nested_workflow_relocation_warning(dir: &Path, workflow_rel_path: &str) -> Option<String> {
    let git_root = find_git_root(dir)?;
    if git_root == dir {
        return None;
    }
    let rel = dir.strip_prefix(&git_root).ok()?;
    // The suggested `working-directory:` always runs inside the generated
    // workflow's own YAML on a Linux CI runner (every template here targets
    // `ubuntu-latest`), regardless of which OS `autumn release init` itself
    // ran on — so this must always render with forward slashes, even when
    // `rel.display()` would use `\` on a Windows host.
    let rel_forward_slash = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Some(format!(
        "Warning: this project lives inside a Git repository whose root is\n\
         {}, but `{workflow_rel_path}` was written under\n\
         {} — GitHub Actions only discovers workflow files under the\n\
         repository ROOT's `.github/workflows/`, so this workflow will never\n\
         run as-is. Move it to {}/{workflow_rel_path} and add\n\
         the following so its `docker build` step still finds this crate's\n\
         Dockerfile:\n\
         \n\
         defaults:\n  run:\n    working-directory: {rel_forward_slash}\n",
        git_root.display(),
        dir.display(),
        git_root.display(),
    ))
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

    // Validate/merge .gitignore BEFORE writing any scaffold file below, not
    // after: this can fail (an existing .gitignore that isn't valid UTF-8,
    // say), and failing after the other files are already on disk would
    // leave a partial, complete-looking scaffold that then blocks a retry
    // without --force on the very files this call just created.
    if is_terraform_target(target) {
        ensure_terraform_gitignore_entries(dir)?;
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

    Ok(created)
}

/// Terraform state (`*.tfstate*`) holds every secret value in plaintext —
/// `sensitive = true` on a variable only redacts CLI plan/apply output, never
/// the state file — and a real `terraform.tfvars` holds the operator's own
/// secret values. None of that may ever land in version control. Shared by
/// every Terraform-based target (`azure-container-apps`, `aws-app-runner`,
/// `aws-ecs`) — the comment names all three so it stays legible however the
/// project got its `.gitignore` merged.
const TERRAFORM_GITIGNORE_ENTRIES: &[&str] = &[
    "# Terraform (autumn release init --target azure-container-apps / aws-app-runner / aws-ecs / gcp-cloud-run)",
    ".terraform/",
    "*.tfstate",
    "*.tfstate.*",
    "terraform.tfvars",
];

/// Whether `pattern` is still in effect by the end of `existing`, applying
/// git's own "last matching rule wins" semantics. Deliberately conservative
/// rather than a full gitignore glob engine (`*`, `**`, `?`, character
/// classes, directory-anchoring rules): once `pattern` has appeared, ANY
/// later negation line — not just an exact `!pattern` match — is treated
/// as potentially un-ignoring it again, since a broader wildcard like
/// `!*.tfvars` or a blanket `!*` also defeats it and being unable to prove
/// a negation doesn't apply must never be mistaken for proof that it
/// doesn't. The cost of this conservatism is, at worst, an unrelated
/// negation elsewhere in the file causing a harmless re-append (a
/// duplicate line) on the next run — never a false "already protected".
fn gitignore_pattern_still_effective(existing: &str, pattern: &str) -> bool {
    let mut effective = false;
    for line in existing.lines() {
        let line = line.trim();
        if line == pattern {
            effective = true;
        } else if effective && line.starts_with('!') {
            effective = false;
        }
    }
    effective
}

/// Ensure `dir/.gitignore` excludes Terraform state and the operator's real
/// `terraform.tfvars`, merging into an existing file (creating one if
/// missing) without touching unrelated lines. Idempotent: a re-run (e.g.
/// under `--force`) never duplicates entries whose protection still holds —
/// but does re-append one whose earlier occurrence has since been negated,
/// since re-asserting it after the negation is the only way to make it the
/// final (and therefore effective) matching rule again.
fn ensure_terraform_gitignore_entries(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(".gitignore");
    // Only a missing file is safe to default to empty. Any other read
    // failure (invalid UTF-8, permission denied, ...) must propagate —
    // silently treating it as "no file" would make the fs::write below
    // replace the operator's existing (unreadable-by-us, but still real)
    // content with just the Terraform entries.
    let existing = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let missing: Vec<&str> = TERRAFORM_GITIGNORE_ENTRIES
        .iter()
        .copied()
        .filter(|line| {
            if line.starts_with('#') {
                !existing.lines().any(|l| l.trim() == *line)
            } else {
                !gitignore_pattern_still_effective(&existing, line)
            }
        })
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
        Target::AwsAppRunner => {
            files.push(("main.tf", templates::AWS_APP_RUNNER_MAIN_TF));
            files.push(("variables.tf", templates::AWS_APP_RUNNER_VARIABLES_TF));
            files.push(("outputs.tf", templates::AWS_APP_RUNNER_OUTPUTS_TF));
            files.push((
                "terraform.tfvars.example",
                templates::AWS_APP_RUNNER_TFVARS_EXAMPLE,
            ));
        }
        Target::AwsEcs => {
            files.push(("main.tf", templates::AWS_ECS_MAIN_TF));
            files.push(("variables.tf", templates::AWS_ECS_VARIABLES_TF));
            files.push(("outputs.tf", templates::AWS_ECS_OUTPUTS_TF));
            files.push((
                "terraform.tfvars.example",
                templates::AWS_ECS_TFVARS_EXAMPLE,
            ));
            files.push((
                ".github/workflows/aws-deploy.yml",
                templates::AWS_DEPLOY_WORKFLOW,
            ));
        }
        Target::GcpCloudRun => {
            files.push(("main.tf", templates::GCP_CLOUD_RUN_MAIN_TF));
            files.push(("variables.tf", templates::GCP_CLOUD_RUN_VARIABLES_TF));
            files.push(("outputs.tf", templates::GCP_CLOUD_RUN_OUTPUTS_TF));
            files.push((
                "terraform.tfvars.example",
                templates::GCP_CLOUD_RUN_TFVARS_EXAMPLE,
            ));
            files.push((
                ".github/workflows/gcp-deploy.yml",
                templates::GCP_DEPLOY_WORKFLOW,
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

    #[test]
    fn dockerignore_excludes_terraform_state() {
        // The azure-container-apps target scaffolds main.tf/terraform.tfvars
        // directly alongside the Dockerfile in the same directory. Docker's
        // build context is whatever the positional path argument points at
        // (`docker build .`), so running that from this directory AFTER
        // `terraform apply` would otherwise upload the plaintext
        // terraform.tfstate — every secret value, `sensitive` flag or not —
        // into the builder/build cache even though no stage ever COPYs it
        // into the final image.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        for pattern in [".terraform/", "*.tfstate", "terraform.tfvars"] {
            assert!(
                content.contains(pattern),
                ".dockerignore must exclude {pattern:?} so terraform.tfstate is never sent \
                 to the Docker build context: {content}"
            );
        }
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
            name_line.contains("local.postgres_database_name"),
            "the database resource must use the bounded/reserved-name-guarded local: {name_line}"
        );
        let raw_local_line = content
            .lines()
            .find(|l| l.trim_start().starts_with("postgres_database_name_raw"))
            .expect("main.tf must declare a postgres_database_name_raw local");
        assert!(
            raw_local_line.contains("substr(") && raw_local_line.contains(", 63)"),
            "the Postgres database name must be truncated to 63 characters: {raw_local_line}"
        );
    }

    #[test]
    fn main_tf_postgres_database_name_avoids_reserved_names() {
        // A fresh Flexible Server already owns "postgres", "azure_maintenance",
        // and "azure_sys" as Azure-specific system databases, plus
        // "template0"/"template1" — every Postgres cluster, on any host, is
        // initialized with those two as its own templates. A Cargo package
        // literally named one of those (or "azure-sys"/"template0", which
        // sanitize to the same underscored form) must not collide with them,
        // or `terraform apply` fails trying to create/manage a database that
        // already exists.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        for reserved in [
            "postgres",
            "azure_maintenance",
            "azure_sys",
            "template0",
            "template1",
        ] {
            assert!(
                content.contains(&format!("\"{reserved}\"")),
                "the reserved-name guard must list {reserved:?}: {content}"
            );
        }
        let database_name_local = content
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("postgres_database_name ="))
            .take_while(|l| !l.trim_start().starts_with('}'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            database_name_local.contains("contains(") && database_name_local.contains("_prod"),
            "postgres_database_name must fall back to a suffixed name when the \
             sanitized value collides with a reserved database name: {content}"
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
    fn main_tf_wires_trusted_hosts_so_prod_actually_binds() {
        // AUTUMN_PROFILE=prod makes fail_fast_on_invalid_trusted_hosts exit
        // the process immediately when security.trusted_hosts.hosts is
        // empty (see docs/guide/deployment.md's "Trusted hosts" section).
        // Without this, the container never binds after the first real
        // deploy — it would crash-loop instead of serving traffic.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS"),
            "main.tf must set AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS on the Container \
             App: {content}"
        );
        assert!(
            content.contains("azurerm_container_app_environment.this.default_domain"),
            "the trusted host must be derived from the environment's default_domain \
             (known before the app is created), not the app's own \
             latest_revision_fqdn (which would be a circular self-reference): {content}"
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
    fn main_tf_acr_pull_role_assignment_skips_aad_check() {
        // The AcrPull grant targets the identity the SAME apply just
        // created above it — Entra ID replication lag can make the role
        // assignment fail with PrincipalNotFound before that object has
        // propagated, even though the identity itself was created
        // successfully. skip_service_principal_aad_check exists for
        // exactly this "newly provisioned principal" case; a user-assigned
        // identity's principal_id IS backed by a Service Principal object
        // in Entra ID, so it applies here.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let role_assignment = content
            .split("resource \"azurerm_role_assignment\" \"acr_pull\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare the acr_pull role assignment");
        assert!(
            role_assignment.contains("skip_service_principal_aad_check = true"),
            "the AcrPull role assignment on the freshly-created identity must set \
             skip_service_principal_aad_check to avoid an intermittent \
             PrincipalNotFound failure from AAD replication lag: {role_assignment}"
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
    fn main_tf_postgres_admin_login_is_alphanumeric() {
        // Azure Database for PostgreSQL Flexible Server rejects
        // administrator_login values containing anything but letters and
        // digits (no underscore, no hyphen) — `terraform apply` fails
        // while creating the server otherwise. Also assert the server
        // resource and the derived database_url secret share a single
        // local so they can't drift out of sync with each other.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();

        let login_local_line = content
            .lines()
            .find(|l| l.trim_start().starts_with("postgres_admin_login"))
            .expect("main.tf must declare a postgres_admin_login local");
        let login_value = login_local_line
            .split('=')
            .nth(1)
            .unwrap()
            .trim()
            .trim_matches('"');
        assert!(
            !login_value.is_empty() && login_value.chars().all(|c| c.is_ascii_alphanumeric()),
            "postgres_admin_login must be alphanumeric-only, got {login_value:?}: \
             {login_local_line}"
        );

        let admin_login_attr = content
            .lines()
            .find(|l| l.trim_start().starts_with("administrator_login"))
            .expect("main.tf must set administrator_login on the Postgres server");
        assert!(
            admin_login_attr.contains("local.postgres_admin_login"),
            "the Postgres server resource must set administrator_login from \
             local.postgres_admin_login: {admin_login_attr}"
        );
        assert!(
            content.contains("postgres://${local.postgres_admin_login}:"),
            "the database_url secret must reuse the same admin-login local as the \
             server resource, not a separately hardcoded username: {content}"
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
    fn main_tf_sanitized_locals_fall_back_when_input_sanitizes_to_nothing_or_a_digit() {
        // A Cargo package name made entirely of characters sanitization
        // strips (e.g. the legal-but-unusual name "_") sanitizes to an
        // empty string, which would otherwise produce a Postgres server
        // name starting with "-" (the "${app_name_alnum}-pg-..." pattern),
        // an empty Postgres database name, and violate resource types that
        // require a letter-led name (Key Vault) rather than just
        // alphanumeric (ACR). Both base locals must fall back to a fixed
        // alphabetic prefix whenever sanitization leaves nothing, or
        // leaves a value not starting with a letter (a leading digit
        // survives sanitization but several consumers don't accept it).
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
            locals_block.contains("app_name_alnum_raw == \"\" ? \"app\"")
                && locals_block.contains("app_name_hyphenated_raw == \"\" ? \"app\""),
            "both base locals must fall back to a non-empty alphabetic value when \
             sanitization leaves nothing: {locals_block}"
        );
        assert!(
            locals_block.matches("regex(\"^[a-z]\"").count() >= 2,
            "both base locals must check for (and fall back on) a non-letter-leading \
             sanitized value, not just an empty one: {locals_block}"
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
    fn main_tf_resource_group_and_identity_use_bounded_name() {
        // Resource groups (90-char limit) and the user-assigned identity
        // (128-char limit) are far more permissive than Container
        // Apps-family resources, but a Cargo package name is unbounded —
        // one longer than 87 characters overflows the resource group's own
        // limit once "-rg" is appended. Both must use the already
        // length-safe local.app_name_safe, not raw var.app_name.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();

        for resource in [
            "azurerm_resource_group\" \"this",
            "azurerm_user_assigned_identity\" \"this",
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
                "{resource} must use the length-bounded local.app_name_safe, not raw \
                 var.app_name (unbounded — a long Cargo package name would overflow \
                 this resource's own name-length limit): {name_line}"
            );
        }
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
    fn outputs_tf_app_fqdn_is_the_stable_ingress_hostname_not_revision_specific() {
        // azurerm_container_app.this.latest_revision_fqdn names a specific
        // *revision*, not the app's stable ingress hostname — visiting it
        // sends a different Host header than AUTUMN_SECURITY__
        // TRUSTED_HOSTS__HOSTS allows (400), and it would go stale as soon
        // as CI creates a new revision outside Terraform. Must use the same
        // local.app_fqdn already wired into the trusted-hosts env var.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        let app_fqdn_block = content
            .split("output \"app_fqdn\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("outputs.tf must declare the app_fqdn output");
        assert!(
            app_fqdn_block.contains("local.app_fqdn"),
            "app_fqdn must be local.app_fqdn, not a revision-specific attribute: \
             {app_fqdn_block}"
        );
        assert!(
            !app_fqdn_block.contains("latest_revision_fqdn"),
            "app_fqdn must not use latest_revision_fqdn, which names a specific \
             revision rather than the stable ingress hostname: {app_fqdn_block}"
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
    fn azure_workflow_passes_git_provenance_build_args_to_docker() {
        // The Dockerfile's AUTUMN_BUILD_* ARGs default to empty unless
        // passed at `docker build` time, and .dockerignore excludes .git
        // from the build context — so without these, every image this
        // workflow builds reports null git provenance at /actuator/info.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        for arg in [
            "AUTUMN_BUILD_GIT_SHA",
            "AUTUMN_BUILD_GIT_SHA_SHORT",
            "AUTUMN_BUILD_GIT_BRANCH",
            "AUTUMN_BUILD_GIT_DIRTY",
            "AUTUMN_BUILD_TIMESTAMP",
        ] {
            assert!(
                content.contains(&format!("--build-arg {arg}=")),
                "azure-deploy.yml's docker build must pass --build-arg {arg}: {content}"
            );
        }
    }

    #[test]
    fn azure_workflow_updates_migration_job_image_before_starting_it() {
        // `az containerapp job start --image ...` sends an execution-TEMPLATE
        // OVERRIDE, which Azure treats as a full replacement, not a merge —
        // an override containing only --image drops the Terraform-configured
        // `command` (autumn migrate) and the AUTUMN_DATABASE__PRIMARY_URL
        // secret env, so the execution would run the container's default
        // command with no DB URL instead of applying migrations. The image
        // must instead be persisted onto the job's stored template via
        // `job update --image` BEFORE a bare `job start` (no --image) runs
        // that complete, up-to-date template.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();

        let migration_step = content
            .split("Run database migrations")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("a 'Run database migrations' step must exist");

        let update_pos = migration_step
            .find("az containerapp job update \\")
            .expect("the migration job's image must be persisted via `job update` first");
        let start_pos = migration_step
            .find("az containerapp job start \\")
            .expect("`job start` must follow to actually run the now-updated template");
        assert!(
            update_pos < start_pos,
            "the job's image must be updated BEFORE it's started: {migration_step}"
        );

        let update_block = &migration_step[update_pos..start_pos];
        assert!(
            update_block.contains("--image"),
            "`job update` must be the one that carries --image: {update_block}"
        );

        // `job start`'s own invocation (up to its `EXECUTION=$(...)` closing
        // paren) must be bare — no --image — since sending one there would
        // reintroduce the template-override bug this test guards against.
        let start_block_end = migration_step[start_pos..]
            .find("--query name -o tsv)")
            .map(|i| start_pos + i)
            .expect("job start must capture the execution name via --query");
        let start_block = &migration_step[start_pos..start_block_end];
        assert!(
            !start_block.contains("--image"),
            "`job start` must not carry --image — that overrides (not merges) the \
             execution template, dropping the command/secret env `job update` just set: \
             {start_block}"
        );
    }

    #[test]
    fn azure_workflow_migration_poll_budget_exceeds_job_timeout() {
        // main.tf sets replica_timeout_in_seconds = 600 on the migration
        // job; Azure self-terminates the execution at that point
        // regardless. Polling for any less risks reporting "timed out" on
        // a migration that's still validly running (and would have
        // succeeded), while leaving it to keep mutating the schema in the
        // background after this workflow has already given up.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        let job_timeout: u64 = main_tf
            .split("replica_timeout_in_seconds = ")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .and_then(|n| n.trim().parse().ok())
            .expect("main.tf must declare the migration job's replica_timeout_in_seconds");

        let workflow = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        let iterations: u64 = workflow
            .split("seq 1 ")
            .nth(1)
            .and_then(|rest| rest.split([')', ' ']).next())
            .and_then(|n| n.trim().parse().ok())
            .expect("the migration poll loop must use `seq 1 N`");
        let sleep_secs: u64 = workflow
            .split("sleep ")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .and_then(|s| s.trim().parse().ok())
            .expect("the migration poll loop must sleep a fixed number of seconds per iteration");
        let poll_budget = iterations * sleep_secs;

        assert!(
            poll_budget > job_timeout,
            "the poll budget ({poll_budget}s = {iterations} x {sleep_secs}s) must exceed \
             the migration job's own replica_timeout_in_seconds ({job_timeout}s), or a \
             still-valid migration can be falsely reported as timed out: {workflow}"
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
    fn azure_workflow_image_tag_never_starts_with_invalid_character() {
        // Docker's tag grammar requires the first character to be a word
        // character ([A-Za-z0-9_]) — "." and "-" are valid elsewhere but
        // not at position zero. `tr` mapping invalid characters to "-" can
        // leave one at the start (e.g. a ref beginning with "+"), which
        // `docker build` rejects.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("sed -E 's/^[.-]+//'"),
            "azure-deploy.yml must strip a leading \".\"/\"-\" left by sanitization: {content}"
        );
        assert!(
            content.contains("[ -z \"$SAFE_REF\" ] && SAFE_REF=\"build\""),
            "azure-deploy.yml must fall back to a valid literal if stripping leaves an \
             empty ref (a ref made entirely of invalid characters): {content}"
        );
    }

    #[test]
    fn azure_workflow_image_tag_is_unique_per_execution() {
        // Two workflow_dispatch runs on the same branch would otherwise
        // compute the identical tag despite different commits — the commit
        // SHA guards against that. But re-running workflow_dispatch on the
        // same branch, or clicking "Re-run jobs" on an existing run, reuses
        // the identical ref AND commit while still producing a genuinely
        // different build (a fresh AUTUMN_BUILD_TIMESTAMP, possibly
        // different base-image bytes) — so the tag must also include
        // GITHUB_RUN_ID (unique per trigger) and GITHUB_RUN_ATTEMPT
        // (disambiguates re-runs of that same trigger) to be unique per
        // actual execution, not just per commit. Re-pushing bytes under a
        // tag Azure already has configured on the Container App isn't
        // guaranteed to register as a revision-scope change, so the old
        // binary could keep serving against a newly migrated schema.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("${GITHUB_SHA:0:12}"),
            "the computed image tag must include the commit SHA: {content}"
        );
        assert!(
            content.contains("${GITHUB_RUN_ID}") && content.contains("${GITHUB_RUN_ATTEMPT}"),
            "the computed image tag must also include the run ID and run attempt, so \
             a re-run of the same trigger (same ref, same commit) never collides with \
             the original run's tag: {content}"
        );
        // Docker tags cap at 128 characters; reserve room for the
        // SHA/run-id/run-attempt suffix rather than letting the sanitized
        // ref alone consume it.
        assert!(
            content.contains("cut -c1-80"),
            "the sanitized ref portion must leave headroom for the rest of the tag \
             within Docker's 128-character limit: {content}"
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
    fn azure_workflow_guards_against_superseded_run_before_migrating() {
        // GitHub does not document strict FIFO ordering for which queued
        // run in a concurrency group goes next. A same-ref check isn't
        // enough either: two DIFFERENT immutable tags (e.g. v1 then v2)
        // each trigger their own run against their own never-moving ref,
        // so the guard must be ref-agnostic — comparing run_number (GitHub's
        // own monotonic-in-trigger-order counter) against other runs of
        // this workflow, not "has my own ref moved".
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/azure-deploy.yml")).unwrap();
        assert!(
            content.contains("actions: read"),
            "azure-deploy.yml must grant actions: read to query other workflow runs: {content}"
        );
        assert!(
            content.contains("run_number > ${{ github.run_number }}"),
            "the guard must compare against other runs' run_number, not just whether \
             this run's own ref has moved: {content}"
        );
        // The guard must NOT filter by status/conclusion (e.g. only
        // "in_progress"/"queued", or "completed" + conclusion == "success")
        // — a newer run can migrate (the actual point of no return, since
        // the schema is advanced at that point) and then fail on a LATER
        // step, reporting an overall conclusion of "failure". There's no
        // cheap way to tell "failed before migrating" apart from "failed
        // after migrating" from a run's top-level status, so the mere
        // existence of any newer run must be disqualifying, full stop.
        let guard_step = content
            .split("Abort if a newer run of this workflow exists")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("the run_number staleness guard step must be present");
        assert!(
            !guard_step.contains("in_progress")
                && !guard_step.contains("queued")
                && !guard_step.contains("conclusion"),
            "the guard must not filter by status or conclusion — any newer run_number \
             must disqualify this run regardless of outcome: {guard_step}"
        );

        let guard_pos = content
            .find("gh api")
            .expect("the run_number staleness guard must be present");
        let job_pos = content
            .find("az containerapp job start \\")
            .expect("migration job start must be present");
        let deploy_pos = content
            .find("az containerapp update \\")
            .expect("deploy step must be present");
        assert!(
            guard_pos < job_pos && job_pos < deploy_pos,
            "the staleness guard must run BEFORE migration, which must run BEFORE \
             deploy: {content}"
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
        for line in TERRAFORM_GITIGNORE_ENTRIES {
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
        for line in TERRAFORM_GITIGNORE_ENTRIES {
            assert!(
                after_first.lines().any(|l| l.trim() == *line),
                "{after_first}"
            );
        }

        // Re-running (e.g. under --force) must not duplicate entries.
        init(&dir, "my-app", true, Target::AzureContainerApps, false).unwrap();
        let after_second = fs::read_to_string(dir.join(".gitignore")).unwrap();
        for line in TERRAFORM_GITIGNORE_ENTRIES {
            let count = after_second.lines().filter(|l| l.trim() == *line).count();
            assert_eq!(
                count, 1,
                "`{line}` must appear exactly once: {after_second}"
            );
        }
    }

    #[test]
    fn azure_target_reasserts_a_gitignore_entry_negated_after_its_earlier_occurrence() {
        // Git applies the LAST matching rule: an existing .gitignore with
        // "terraform.tfvars" followed later by "!terraform.tfvars" makes
        // the file trackable, even though the literal pattern is present.
        // A naive "is this line already there" check would wrongly treat
        // it as already protected and add nothing, letting an operator
        // commit the plaintext secrets this scaffold exists to keep out.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(
            dir.join(".gitignore"),
            "terraform.tfvars\nsome-other-line\n!terraform.tfvars\n",
        )
        .unwrap();

        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();

        // The negation must not have been touched (still there, doing its
        // job for whatever the operator originally wanted un-ignored)...
        assert!(content.contains("!terraform.tfvars"), "{content}");
        // ...but "terraform.tfvars" must be re-asserted AFTER it (not just
        // matched as a substring of "!terraform.tfvars" itself), since
        // that's the only way to make it the final (and therefore
        // effective) matching rule again.
        let after_negation = content.find("!terraform.tfvars").unwrap() + "!terraform.tfvars".len();
        assert!(
            content[after_negation..].contains("terraform.tfvars"),
            "terraform.tfvars must be re-added after the negation that defeated it: {content}"
        );
    }

    #[test]
    fn azure_target_reasserts_entry_after_a_broader_wildcard_negation() {
        // A wildcard negation like "!*.tfvars" or a blanket "!*" also
        // un-ignores "terraform.tfvars" under git's own semantics, not
        // just an exact "!terraform.tfvars" match. Matching gitignore's
        // full glob syntax is out of scope, so any negation line
        // appearing after the pattern must conservatively be treated as
        // potentially applying to it.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), "terraform.tfvars\n!*.tfvars\n").unwrap();

        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();

        let after_negation = content.find("!*.tfvars").unwrap() + "!*.tfvars".len();
        assert!(
            content[after_negation..].contains("terraform.tfvars"),
            "terraform.tfvars must be re-added after a broader wildcard negation too: \
             {content}"
        );
    }

    #[test]
    fn azure_target_propagates_gitignore_read_errors_instead_of_clobbering_it() {
        // A read failure that ISN'T "file doesn't exist" (invalid UTF-8,
        // permission denied, ...) must not be silently treated as "no
        // file" — doing so would make the merge overwrite the operator's
        // real (if unreadable-by-us) .gitignore with just the Terraform
        // entries, destroying whatever rules were already there.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), [0xFF, 0xFE, 0x00, 0xFF]).unwrap();

        let err = init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap_err();
        assert!(
            matches!(err, ReleaseError::Io(_)),
            "an unreadable .gitignore must surface as an I/O error, not be silently \
             replaced: {err}"
        );
        // The original (if unreadable) content must be left untouched.
        let raw = fs::read(dir.join(".gitignore")).unwrap();
        assert_eq!(raw, vec![0xFF, 0xFE, 0x00, 0xFF]);
    }

    #[test]
    fn azure_target_gitignore_failure_leaves_no_partial_scaffold() {
        // The .gitignore merge must be validated BEFORE any scaffold file
        // is written — otherwise a read failure here leaves a partial,
        // complete-looking scaffold on disk, and a retry without --force
        // immediately fails on the files this very call just created.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), [0xFF, 0xFE, 0x00, 0xFF]).unwrap();

        init(&dir, "my-app", false, Target::AzureContainerApps, false).unwrap_err();

        for name in [
            "Dockerfile",
            ".dockerignore",
            "autumn.production.toml.example",
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/azure-deploy.yml",
        ] {
            assert!(
                !dir.join(name).exists(),
                "{name} must not exist after init() fails on the .gitignore merge \
                 (found a partial scaffold, which blocks retrying without --force)"
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

    // ── nested workflow discoverability (git root vs. workspace member) ───────

    #[test]
    fn nested_workflow_relocation_warning_is_silent_at_the_git_root() {
        // The common case: `dir` IS the git repository root (a single-crate
        // repo, or a workspace member the user happens to be running from
        // the top of anyway). `.github/workflows/azure-deploy.yml` lands
        // exactly where GitHub looks for it — nothing to warn about.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(
            nested_workflow_relocation_warning(&dir, ".github/workflows/azure-deploy.yml"),
            None
        );
    }

    #[test]
    fn nested_workflow_relocation_warning_flags_a_nested_workspace_member() {
        // `autumn release init` explicitly supports running from a Cargo
        // workspace member directory (read_project_name only rejects the
        // workspace root itself) — but GitHub Actions only discovers
        // workflows under the git repository ROOT's .github/workflows/, so
        // a workflow written under a member subdirectory would silently
        // never fire. This must be flagged, not silently mis-scaffolded.
        let tmp = TempDir::new().unwrap();
        let git_root = tmp.path().to_path_buf();
        fs::create_dir_all(git_root.join(".git")).unwrap();
        let member_dir = git_root.join("examples").join("blog");
        fs::create_dir_all(&member_dir).unwrap();

        let warning =
            nested_workflow_relocation_warning(&member_dir, ".github/workflows/azure-deploy.yml")
                .expect("a workflow nested under a workspace member must be flagged");
        assert!(
            warning.contains(&git_root.display().to_string()),
            "the warning must name the actual git root: {warning}"
        );
        assert!(
            warning.contains("working-directory: examples/blog"),
            "the warning must give the exact working-directory override needed so \
             the relocated workflow's docker build step still finds this crate's \
             Dockerfile: {warning}"
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
    fn parse_target_aws_app_runner() {
        assert_eq!(
            "aws-app-runner".parse::<Target>().unwrap(),
            Target::AwsAppRunner
        );
    }

    #[test]
    fn parse_target_aws_ecs() {
        assert_eq!("aws-ecs".parse::<Target>().unwrap(), Target::AwsEcs);
    }

    #[test]
    fn parse_target_gcp_cloud_run() {
        assert_eq!(
            "gcp-cloud-run".parse::<Target>().unwrap(),
            Target::GcpCloudRun
        );
    }

    #[test]
    fn parse_target_unknown_is_error() {
        assert!("kubernetes".parse::<Target>().is_err());
    }

    #[test]
    fn parse_target_unknown_error_mentions_all_targets() {
        let err = "kubernetes".parse::<Target>().unwrap_err();
        for name in [
            "fly",
            "docker-compose",
            "azure-container-apps",
            "aws-app-runner",
            "aws-ecs",
            "gcp-cloud-run",
        ] {
            assert!(err.contains(name), "error must mention '{name}': {err}");
        }
    }

    // ── --target=aws-app-runner ─────────────────────────────────────────────────

    #[test]
    fn aws_app_runner_target_creates_all_expected_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
        ] {
            assert!(
                dir.join(name).is_file(),
                "{name} must be created for --target=aws-app-runner"
            );
        }
        assert!(dir.join("Dockerfile").is_file());
        assert!(dir.join(".dockerignore").is_file());
        assert!(dir.join("autumn.production.toml.example").is_file());
    }

    #[test]
    fn aws_app_runner_target_does_not_create_a_workflow() {
        // Per the issue: aws-app-runner is the fast/minimal path with no CI
        // workflow (unlike azure-container-apps and aws-ecs).
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        let files = init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        assert!(
            !files.iter().any(|f| f.contains(".github/workflows")),
            "aws-app-runner must not emit a CI workflow: {files:?}"
        );
        assert!(!dir.join(".github").exists());
    }

    #[test]
    fn default_target_does_not_create_aws_app_runner_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
        ] {
            assert!(
                !dir.join(name).exists(),
                "{name} must NOT be created for the default target"
            );
        }
    }

    #[test]
    fn aws_app_runner_main_tf_has_ecr_and_app_runner_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("aws_ecr_repository"),
            "main.tf must provision an ECR repository: {content}"
        );
        assert!(
            content.contains("resource \"aws_apprunner_service\""),
            "main.tf must provision the App Runner service: {content}"
        );
    }

    #[test]
    fn aws_app_runner_main_tf_has_vpc_and_vpc_connector() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("resource \"aws_vpc\""),
            "main.tf must provision a VPC: {content}"
        );
        assert!(
            content.contains("aws_apprunner_vpc_connector"),
            "main.tf must provision an App Runner VPC connector so App Runner can reach RDS \
             privately: {content}"
        );
    }

    #[test]
    fn aws_app_runner_main_tf_has_nat_gateway_for_general_egress() {
        // network_configuration.egress_configuration.egress_type = "VPC"
        // routes ALL of the app's own outbound traffic through the private
        // subnets, not just RDS-bound traffic — without a NAT gateway, any
        // outbound call the app itself makes would silently hang or fail.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("aws_nat_gateway"),
            "main.tf must provision a NAT gateway so the app's own outbound traffic \
             (not just RDS-bound traffic) keeps working once egress_type = \"VPC\": {content}"
        );
        assert!(
            content.contains("egress_type       = \"VPC\"")
                || content.contains("egress_type = \"VPC\""),
            "main.tf must route App Runner egress through the VPC connector: {content}"
        );
    }

    #[test]
    fn aws_app_runner_main_tf_has_rds_postgres() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("resource \"aws_db_instance\""),
            "main.tf must provision an RDS Postgres instance: {content}"
        );
        assert!(
            content.contains("engine         = \"postgres\"")
                || content.contains("engine = \"postgres\""),
            "the RDS instance must use the postgres engine: {content}"
        );
        assert!(
            content.contains("publicly_accessible    = false")
                || content.contains("publicly_accessible = false"),
            "RDS must not be publicly accessible: {content}"
        );
    }

    #[test]
    fn aws_app_runner_provisions_a_reachable_one_shot_migration_task() {
        // App Runner has no release-phase hook of its own, and RDS is
        // private with no public entry point — main.tf must provision SOME
        // real, reachable compute for `autumn migrate` to run against,
        // rather than leaving that to unwritten operator infrastructure.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        for resource in [
            "resource \"aws_ecs_cluster\" \"migrate\"",
            "resource \"aws_ecs_task_definition\" \"migrate\"",
            "resource \"aws_iam_role\" \"migrate_execution\"",
            "resource \"aws_iam_role\" \"migrate_task\"",
        ] {
            assert!(
                content.contains(resource),
                "main.tf must declare {resource} so the migration task has somewhere \
                 to run: {content}"
            );
        }
        let task_def_block = content
            .split("resource \"aws_ecs_task_definition\" \"migrate\"")
            .nth(1)
            .expect("main.tf must declare the migrate task definition");
        assert!(
            task_def_block.contains("autumn migrate"),
            "the migrate task must actually run `autumn migrate`: {task_def_block}"
        );
        assert!(
            task_def_block.contains("aws_secretsmanager_secret.database_url.arn"),
            "the migrate task must be able to read the derived database URL: {task_def_block}"
        );
    }

    #[test]
    fn aws_app_runner_migration_task_reuses_the_vpc_connector_security_group() {
        // RDS's ingress rule (aws_security_group.database) only trusts
        // aws_security_group.vpc_connector as a source — the migration
        // task must be run with that SAME security group attached (via the
        // vpc_connector_security_group_id output) rather than provisioning
        // a second, parallel security group RDS's rule doesn't know about.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let outputs = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        for name in [
            "migrate_cluster_name",
            "migrate_task_family",
            "private_subnet_ids",
            "vpc_connector_security_group_id",
        ] {
            assert!(
                outputs.contains(&format!("output \"{name}\"")),
                "outputs.tf must declare output \"{name}\" for the migration task's \
                 `aws ecs run-task` call: {outputs}"
            );
        }
        assert!(
            outputs.contains("aws_security_group.vpc_connector.id"),
            "vpc_connector_security_group_id must reuse the SAME security group RDS's \
             ingress rule trusts, not a new one: {outputs}"
        );
    }

    #[test]
    fn aws_app_runner_main_tf_derives_database_url_not_a_variable() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            main_tf.contains("AUTUMN_DATABASE__PRIMARY_URL"),
            "main.tf must wire the primary DB URL: {main_tf}"
        );
        assert!(
            main_tf.contains("aws_db_instance.this.address"),
            "the database URL must be derived from the RDS instance this apply creates, \
             not pre-computed: {main_tf}"
        );
        assert!(
            !variables_tf.contains("variable \"database_url\""),
            "there must be no database_url variable — a single apply must be able to \
             derive it end-to-end: {variables_tf}"
        );
    }

    #[test]
    fn aws_app_runner_secrets_have_no_default_and_are_sensitive() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        for var in ["database_admin_password", "signing_secret"] {
            let block = content
                .split(&format!("variable \"{var}\""))
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .unwrap_or_else(|| {
                    panic!("variables.tf must declare variable \"{var}\": {content}")
                });
            assert!(
                block.contains("sensitive   = true") || block.contains("sensitive = true"),
                "{var} must be sensitive: {block}"
            );
            assert!(
                !block.contains("default"),
                "{var} must not have a default (never commit a real secret value): {block}"
            );
        }
    }

    #[test]
    fn aws_app_runner_no_committed_secret_literals() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        for name in ["main.tf", "variables.tf", "terraform.tfvars.example"] {
            let content = fs::read_to_string(dir.join(name)).unwrap();
            assert!(
                !content.to_lowercase().contains("password =\""),
                "{name} must not contain a literal password assignment: {content}"
            );
        }
    }

    #[test]
    fn aws_app_runner_service_bootstraps_from_public_placeholder_and_ignores_drift() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let service_block = content
            .split("resource \"aws_apprunner_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the App Runner service");
        assert!(
            service_block.contains("var.bootstrap_image"),
            "the App Runner service must start from the bootstrap placeholder image: {service_block}"
        );
        assert!(
            service_block
                .contains("ignore_changes = [source_configuration, health_check_configuration]"),
            "the App Runner service must ignore source_configuration drift once CI/the manual \
             walkthrough deploys the real image: {service_block}"
        );
    }

    #[test]
    fn aws_app_runner_bootstrap_health_check_matches_the_placeholder_not_the_real_app() {
        // aws_apprunner_service blocks `terraform apply` until the service
        // reaches a stable state — declaring port 3000 / path "/health"
        // against a bootstrap image that doesn't serve either would hang
        // the very first apply. The placeholder (nginx) listens on 80 and
        // returns 200 for "/" by default; the real port/path are restored
        // by the deploy walkthrough's cutover call, which owns App
        // Runner's mutable, service-level health check configuration.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("port = \"80\""),
            "the bootstrap image_configuration must declare port 80 (nginx's real default), \
             not 3000: {content}"
        );
        let health_check_block = content
            .split("health_check_configuration {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("main.tf must declare health_check_configuration");
        assert!(
            health_check_block.contains("path     = \"/\""),
            "the bootstrap health check must probe \"/\" (nginx's default 200 response), \
             not \"/health\": {health_check_block}"
        );
    }

    #[test]
    fn aws_app_runner_ignores_health_check_drift_after_cutover_restores_it() {
        // The cutover call (docs/guide/deployment.md) switches
        // health_check_configuration from the bootstrap's "/" to the real
        // app's "/health" — without ignoring this block too, a later
        // `terraform apply` would see that as drift from this resource's
        // own declared "/" and revert it, breaking the real app's health
        // check.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let service_block = content
            .split("resource \"aws_apprunner_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the App Runner service");
        assert!(
            service_block
                .contains("ignore_changes = [source_configuration, health_check_configuration]"),
            "the App Runner service must ignore health_check_configuration drift alongside \
             source_configuration: {service_block}"
        );
    }

    #[test]
    fn aws_app_runner_instance_role_secrets_policy_is_scoped_not_wildcard() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let policy_block = content
            .split("data \"aws_iam_policy_document\" \"apprunner_instance_secrets\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("main.tf must declare the instance role's secrets policy document");
        assert!(
            !policy_block.contains("resources = [\"*\"]") && !policy_block.contains("\"*\""),
            "the instance role's secrets access must be scoped to specific secret ARNs, \
             never a wildcard: {policy_block}"
        );
        assert!(
            policy_block.contains("aws_secretsmanager_secret.database_url.arn"),
            "the secrets policy must reference the database_url secret ARN: {policy_block}"
        );
    }

    #[test]
    fn aws_app_runner_service_waits_for_secret_versions_before_starting() {
        // runtime_environment_secrets only references the secret
        // CONTAINERS (aws_secretsmanager_secret.*.arn), so Terraform's
        // implicit dependency graph doesn't wait for the *_version
        // resources that actually write the secret values — without an
        // explicit depends_on, the service can start (and fail to resolve
        // AUTUMN_DATABASE__PRIMARY_URL) before RDS-derived value exists.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let service_block = content
            .split("resource \"aws_apprunner_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare aws_apprunner_service.this");
        for dep in [
            "aws_secretsmanager_secret_version.database_url",
            "aws_secretsmanager_secret_version.signing_secret",
        ] {
            assert!(
                service_block.contains(dep),
                "aws_apprunner_service.this must depend on {dep}, not just the secret \
                 container it references by ARN: {service_block}"
            );
        }
    }

    #[test]
    fn aws_app_runner_names_are_sanitized_for_underscored_uppercase_project_name() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "My_Test_App");
        init(&dir, "My_Test_App", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("lower(replace(var.app_name"),
            "main.tf must lowercase and sanitize app_name for AWS resource names: {content}"
        );
    }

    #[test]
    fn aws_app_runner_postgres_database_name_avoids_reserved_names() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        for reserved in ["postgres", "rdsadmin", "template0", "template1"] {
            assert!(
                content.contains(&format!("\"{reserved}\"")),
                "the reserved-name guard must list {reserved:?}: {content}"
            );
        }
    }

    #[test]
    fn aws_app_runner_target_adds_terraform_gitignore_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        for line in TERRAFORM_GITIGNORE_ENTRIES {
            assert!(content.lines().any(|l| l.trim() == *line), "{content}");
        }
    }

    #[test]
    fn aws_app_runner_dockerignore_excludes_terraform_state() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        for pattern in [".terraform/", "*.tfstate", "terraform.tfvars"] {
            assert!(content.contains(pattern), "{content}");
        }
    }

    #[test]
    fn aws_app_runner_outputs_tf_declares_expected_outputs_with_no_unsubstituted_placeholders() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        for name in [
            "region",
            "app_url",
            "service_arn",
            "service_name",
            "ecr_repository_url",
            "apprunner_access_role_arn",
            "database_url_secret_arn",
            "signing_secret_secret_arn",
        ] {
            assert!(
                content.contains(&format!("output \"{name}\"")),
                "outputs.tf must declare output \"{name}\": {content}"
            );
        }
        assert!(
            !content.contains("{{"),
            "outputs.tf must not contain any unsubstituted template placeholders: {content}"
        );
    }

    #[test]
    fn aws_app_runner_region_output_reflects_the_configured_region_variable() {
        // The deploy walkthrough sources AWS_REGION from this output so
        // every `aws` CLI call targets the same region Terraform
        // provisioned into, regardless of the operator's ambient CLI
        // config — it must echo var.region, not a hardcoded default.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        assert!(content.contains("value       = var.region"), "{content}");
    }

    #[test]
    fn aws_app_runner_secret_arn_outputs_reference_the_real_secrets() {
        // The deploy walkthrough's cutover `update-service` call must
        // re-supply RuntimeEnvironmentSecrets (the call replaces the image
        // configuration wholesale, not merges it) — these outputs are how
        // it gets the ARNs, so they must point at the actual secrets, not
        // just exist as a name.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        assert!(
            content.contains("aws_secretsmanager_secret.database_url.arn"),
            "{content}"
        );
        assert!(
            content.contains("aws_secretsmanager_secret.signing_secret.arn"),
            "{content}"
        );
    }

    #[test]
    fn aws_app_runner_gitignore_merge_is_idempotent_on_repeat_init() {
        // The merge function itself is exhaustively tested against the
        // shared azure-container-apps target (idempotency, negation
        // re-assertion, I/O-failure propagation); this pins that the
        // aws-app-runner call site is actually wired to it under --force,
        // not just on a fresh project.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), "/target\n.env\n").unwrap();

        init(&dir, "my-app", false, Target::AwsAppRunner, false).unwrap();
        init(&dir, "my-app", true, Target::AwsAppRunner, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            content.contains("/target") && content.contains(".env"),
            "{content}"
        );
        for line in TERRAFORM_GITIGNORE_ENTRIES {
            let count = content.lines().filter(|l| l.trim() == *line).count();
            assert_eq!(count, 1, "`{line}` must appear exactly once: {content}");
        }
    }

    // ── --target=aws-ecs ─────────────────────────────────────────────────────────

    #[test]
    fn aws_ecs_target_creates_all_expected_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/aws-deploy.yml",
        ] {
            assert!(
                dir.join(name).is_file(),
                "{name} must be created for --target=aws-ecs"
            );
        }
        assert!(dir.join("Dockerfile").is_file());
        assert!(dir.join(".dockerignore").is_file());
        assert!(dir.join("autumn.production.toml.example").is_file());
    }

    #[test]
    fn aws_ecs_target_returns_nested_workflow_path_in_created_list() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        let files = init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        assert!(
            files
                .iter()
                .any(|f| f == ".github/workflows/aws-deploy.yml"),
            "created-files list must include the nested workflow path: {files:?}"
        );
    }

    #[test]
    fn default_target_does_not_create_aws_ecs_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/aws-deploy.yml",
        ] {
            assert!(
                !dir.join(name).exists(),
                "{name} must NOT be created for the default target"
            );
        }
    }

    #[test]
    fn aws_ecs_main_tf_has_vpc_with_public_and_private_subnets_across_two_azs() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(content.contains("resource \"aws_vpc\""), "{content}");
        assert!(
            content.contains("resource \"aws_subnet\" \"public\"")
                && content.contains("count                   = 2"),
            "main.tf must provision 2 public subnets: {content}"
        );
        assert!(
            content.contains("resource \"aws_subnet\" \"private\"")
                && content.contains("count             = 2"),
            "main.tf must provision 2 private subnets: {content}"
        );
        assert!(
            content.contains("data \"aws_availability_zones\""),
            "subnets must be spread across AZs discovered at apply time, not hardcoded: {content}"
        );
    }

    #[test]
    fn aws_ecs_main_tf_has_alb_with_https_redirect_and_dns_validated_cert() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("resource \"aws_lb\" \"this\""),
            "main.tf must provision an ALB: {content}"
        );
        assert!(
            content.contains("resource \"aws_acm_certificate\""),
            "main.tf must provision an ACM certificate: {content}"
        );
        assert!(
            content.contains("validation_method = \"DNS\""),
            "the ACM certificate must use DNS validation: {content}"
        );
        assert!(
            content.contains("aws_acm_certificate_validation"),
            "main.tf must wait for certificate validation before wiring the HTTPS listener: {content}"
        );
        let http_listener = content
            .split("resource \"aws_lb_listener\" \"http\"")
            .nth(1)
            .and_then(|rest| rest.split("\nresource").next())
            .expect("main.tf must declare the HTTP listener");
        assert!(
            http_listener.contains("HTTP_301") && http_listener.contains("HTTPS"),
            "the HTTP listener must redirect to HTTPS: {http_listener}"
        );
    }

    #[test]
    fn aws_ecs_alb_and_target_group_names_never_exceed_32_char_limit() {
        // ALB and target group names are capped at 32 characters by AWS.
        // Extract the substr() bound in the sanitization local and verify
        // the worst-case rendered name (base + longest suffix) fits.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let base_cap_line = content
            .lines()
            .find(|l| l.trim_start().starts_with("app_name_safe = trim(substr("))
            .expect("main.tf must declare app_name_safe via a bounded substr()");
        assert!(
            base_cap_line.contains(", 20)"),
            "expected a 20-char base cap: {base_cap_line}"
        );
        // 20 (base) + "-migrate-tg" (11) = 31 <= 32.
        assert!(20 + "-migrate-tg".len() <= 32);
        assert!(
            content.contains("\"${local.app_name_safe}-alb\""),
            "the ALB must be named from the sanitized/capped local: {content}"
        );
        assert!(
            content.contains("\"${local.app_name_safe}-tg\""),
            "the target group must be named from the sanitized/capped local: {content}"
        );
    }

    #[test]
    fn aws_ecs_main_tf_has_ecs_fargate_cluster_task_and_service_with_circuit_breaker() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("resource \"aws_ecs_cluster\""),
            "{content}"
        );
        assert!(
            content.contains("resource \"aws_ecs_task_definition\" \"app\"")
                && content.contains("FARGATE"),
            "{content}"
        );
        assert!(
            content.contains("resource \"aws_ecs_service\" \"this\""),
            "{content}"
        );
        let service_block = content
            .split("resource \"aws_ecs_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the ECS service");
        assert!(
            service_block.contains("deployment_circuit_breaker")
                && service_block.contains("rollback = true"),
            "the ECS service must enable circuit-breaker rollback: {service_block}"
        );
    }

    #[test]
    fn aws_ecs_service_waits_for_secret_versions_before_starting() {
        // local.container_secrets only references the secret CONTAINERS
        // (aws_secretsmanager_secret.*.arn), so Terraform's implicit
        // dependency graph doesn't wait for the *_version resources that
        // actually write the secret values — without an explicit
        // depends_on, the service can schedule tasks before RDS's derived
        // database_url value (or, when enabled, Redis's) exists, and the
        // deployment circuit breaker can permanently fail the first
        // deployment as a result.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let service_block = content
            .split("resource \"aws_ecs_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the ECS service");
        for dep in [
            "aws_secretsmanager_secret_version.database_url",
            "aws_secretsmanager_secret_version.signing_secret",
            "aws_secretsmanager_secret_version.redis_url",
        ] {
            assert!(
                service_block.contains(dep),
                "aws_ecs_service.this must depend on {dep}, not just the secret \
                 container it references by ARN: {service_block}"
            );
        }
    }

    #[test]
    fn aws_ecs_service_waits_for_execution_role_policies_before_starting() {
        // The task definition's execution_role_arn only orders creation
        // after the ROLE itself, not the policies attached to it — without
        // an explicit depends_on, tasks could be scheduled before the
        // execution role can actually pull the image, ship logs, or inject
        // secrets, failing during resource initialization.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let service_block = content
            .split("resource \"aws_ecs_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the ECS service");
        for dep in [
            "aws_iam_role_policy_attachment.execution_managed",
            "aws_iam_role_policy.execution_secrets",
        ] {
            assert!(
                service_block.contains(dep),
                "aws_ecs_service.this must depend on {dep}, not just the execution \
                 role itself: {service_block}"
            );
        }
    }

    #[test]
    fn aws_ecs_task_definitions_bootstrap_from_placeholder_and_ignore_drift() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        for family in ["app", "migrate"] {
            let block = content
                .split(&format!(
                    "resource \"aws_ecs_task_definition\" \"{family}\""
                ))
                .nth(1)
                .and_then(|rest| rest.split("\nresource").next())
                .unwrap_or_else(|| panic!("main.tf must declare the {family} task definition"));
            assert!(
                block.contains("var.bootstrap_image"),
                "{family} task must use the bootstrap image: {block}"
            );
            assert!(
                block.contains("ignore_changes = [container_definitions]"),
                "{family} task definition must ignore container_definitions drift once CI \
                 registers real revisions: {block}"
            );
        }
        let service_block = content
            .split("resource \"aws_ecs_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the ECS service");
        assert!(
            service_block.contains("ignore_changes = [task_definition, desired_count]"),
            "the ECS service must ignore task_definition drift so CI-registered revisions \
             aren't reverted by a later `terraform apply`: {service_block}"
        );
    }

    #[test]
    fn aws_ecs_ignores_desired_count_drift_managed_by_autoscaling() {
        // Application Auto Scaling changes desired_count directly at
        // runtime — without ignoring it, a later `terraform apply` would
        // treat a live scaling decision as drift from var.desired_count
        // and forcibly reset capacity, either yanking it during a load
        // spike or adding unwanted tasks right after a scale-in.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let service_block = content
            .split("resource \"aws_ecs_service\" \"this\"")
            .nth(1)
            .expect("main.tf must declare the ECS service");
        assert!(
            service_block.contains("desired_count]"),
            "the ECS service must ignore desired_count drift once Application Auto Scaling \
             owns it: {service_block}"
        );
    }

    #[test]
    fn aws_ecs_app_bootstrap_container_actually_listens_on_the_alb_health_check_port() {
        // Unlike App Runner, the ALB target group's health check (port
        // 3000, path /health) is a PERMANENT Terraform-managed resource —
        // there's no separate "swap it back after cutover" step available,
        // so the bootstrap container must satisfy it directly. The public
        // placeholder (nginx) doesn't do this out of the box; the "app"
        // container must override its entrypoint/command to configure it.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let app_block = content
            .split("resource \"aws_ecs_task_definition\" \"app\"")
            .nth(1)
            .and_then(|rest| rest.split("\nresource").next())
            .expect("main.tf must declare the app task definition");
        assert!(
            app_block.contains("entryPoint"),
            "the app container must override entryPoint so it runs regardless of the \
             placeholder image's own default command: {app_block}"
        );
        assert!(
            app_block.contains("listen 3000"),
            "the app container's bootstrap command must configure the placeholder to listen \
             on port 3000, matching the ALB target group: {app_block}"
        );
        assert!(
            app_block.contains("/health"),
            "the app container's bootstrap command must serve the same /health path the \
             target group checks: {app_block}"
        );
        // The "migrate" task's command is already overridden to run `autumn
        // migrate` and is never actually invoked against the bootstrap
        // image in practice (CI always registers a real-image revision
        // first) — it doesn't need the nginx trick.
        let migrate_block = content
            .split("resource \"aws_ecs_task_definition\" \"migrate\"")
            .nth(1)
            .and_then(|rest| rest.split("\nresource").next())
            .expect("main.tf must declare the migrate task definition");
        assert!(migrate_block.contains("autumn migrate"), "{migrate_block}");
    }

    #[test]
    fn aws_ecs_main_tf_derives_database_url_not_a_variable() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        assert!(
            main_tf.contains("AUTUMN_DATABASE__PRIMARY_URL"),
            "{main_tf}"
        );
        assert!(
            main_tf.contains("aws_db_instance.this.address"),
            "{main_tf}"
        );
        assert!(
            !variables_tf.contains("variable \"database_url\""),
            "there must be no database_url variable: {variables_tf}"
        );
    }

    #[test]
    fn aws_ecs_trusted_hosts_uses_the_required_domain_name_variable() {
        // Unlike App Runner (whose subdomain is only known after the
        // service is created), ECS's ALB serves under the operator-supplied
        // domain_name, known before the apply even starts — so trusted
        // hosts can be set correctly on the very first apply.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS")
                && content.contains("var.domain_name"),
            "{content}"
        );
    }

    #[test]
    fn aws_ecs_execution_role_secrets_policy_is_scoped_not_wildcard() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let policy_block = content
            .split("data \"aws_iam_policy_document\" \"execution_secrets\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("main.tf must declare the execution role's secrets policy document");
        assert!(
            !policy_block.contains("\"*\""),
            "the execution role's secrets access must be scoped, never a wildcard: {policy_block}"
        );
        assert!(
            policy_block.contains("local.secrets_manager_arns"),
            "{policy_block}"
        );
    }

    #[test]
    fn aws_ecs_task_role_and_execution_role_are_distinct() {
        // Least privilege: the execution role (ECS agent — image pull, logs,
        // secrets injection) must be a different principal from the task
        // role (the running app container's own AWS permissions).
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("resource \"aws_iam_role\" \"execution\""),
            "{content}"
        );
        assert!(
            content.contains("resource \"aws_iam_role\" \"task\""),
            "{content}"
        );
    }

    #[test]
    fn aws_ecs_redis_is_off_by_default_and_gated() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        let redis_var_block = variables_tf
            .split("variable \"enable_redis_cache\"")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("variables.tf must declare variable \"enable_redis_cache\"");
        assert!(
            redis_var_block.contains("default     = false"),
            "enable_redis_cache must default to false: {redis_var_block}"
        );
        assert!(
            main_tf.contains("count                       = var.enable_redis_cache ? 1 : 0")
                || main_tf.contains("count = var.enable_redis_cache ? 1 : 0"),
            "ElastiCache resources must be gated behind enable_redis_cache: {main_tf}"
        );
        assert!(
            main_tf.contains("autumn-cache-redis"),
            "main.tf must document that Redis wiring alone isn't enough — the app must \
             also depend on autumn-cache-redis: {main_tf}"
        );
    }

    #[test]
    fn aws_ecs_scale_defaults_match_the_issue() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        let defaults = [
            ("desired_count", "2"),
            ("min_count", "1"),
            ("max_count", "10"),
        ];
        for (var, expected) in defaults {
            let block = content
                .split(&format!("variable \"{var}\""))
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .unwrap_or_else(|| {
                    panic!("variables.tf must declare variable \"{var}\": {content}")
                });
            assert!(
                block.contains(&format!("default     = {expected}")),
                "{var} must default to {expected}: {block}"
            );
        }
    }

    #[test]
    fn aws_ecs_secrets_have_no_default_and_are_sensitive() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        for var in ["database_admin_password", "signing_secret"] {
            let block = content
                .split(&format!("variable \"{var}\""))
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .unwrap_or_else(|| {
                    panic!("variables.tf must declare variable \"{var}\": {content}")
                });
            assert!(
                block.contains("sensitive   = true") || block.contains("sensitive = true"),
                "{block}"
            );
            assert!(
                !block.contains("default"),
                "{var} must not have a default: {block}"
            );
        }
    }

    #[test]
    fn aws_ecs_no_committed_secret_literals() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        for name in ["main.tf", "variables.tf", "terraform.tfvars.example"] {
            let content = fs::read_to_string(dir.join(name)).unwrap();
            assert!(
                !content.to_lowercase().contains("password =\""),
                "{name} must not contain a literal password assignment: {content}"
            );
        }
    }

    #[test]
    fn aws_ecs_target_adds_terraform_gitignore_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        for line in TERRAFORM_GITIGNORE_ENTRIES {
            assert!(content.lines().any(|l| l.trim() == *line), "{content}");
        }
    }

    #[test]
    fn aws_ecs_dockerignore_excludes_terraform_state() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        for pattern in [".terraform/", "*.tfstate", "terraform.tfvars"] {
            assert!(content.contains(pattern), "{content}");
        }
    }

    #[test]
    fn aws_ecs_outputs_tf_declares_expected_outputs_with_no_unsubstituted_placeholders() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        for name in [
            "region",
            "app_url",
            "alb_dns_name",
            "ecr_repository_url",
            "ecs_cluster_name",
            "ecs_service_name",
            "app_task_family",
            "migrate_task_family",
            "private_subnet_ids",
            "ecs_tasks_security_group_id",
            "execution_role_arn",
            "task_role_arn",
        ] {
            assert!(
                content.contains(&format!("output \"{name}\"")),
                "outputs.tf must declare output \"{name}\" — aws-deploy.yml's header \
                 comment tells operators to source CI config/IAM grants from it: {content}"
            );
        }
        assert!(
            !content.contains("{{"),
            "outputs.tf must not contain any unsubstituted template placeholders: {content}"
        );
    }

    #[test]
    fn aws_ecs_region_output_reflects_the_configured_region_variable() {
        // Both the deploy walkthrough's exported AWS_REGION env var and
        // aws-deploy.yml's documented AWS_REGION repo variable are meant to
        // be sourced from this output, so it must echo var.region, not a
        // hardcoded default.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        assert!(content.contains("value       = var.region"), "{content}");
    }

    #[test]
    fn aws_ecs_execution_and_task_role_arns_are_both_exported_for_ci_pass_role_grants() {
        // aws-deploy.yml's header comment tells operators to grant the CI
        // deploy role iam:PassRole on both roles' ARNs "terraform output
        // prints" — that promise is only true if both are actually
        // exported outputs, not just Terraform-internal resources.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let outputs_tf = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        assert!(
            outputs_tf.contains("aws_iam_role.execution.arn"),
            "{outputs_tf}"
        );
        assert!(outputs_tf.contains("aws_iam_role.task.arn"), "{outputs_tf}");
    }

    #[test]
    fn aws_ecs_gitignore_merge_is_idempotent_on_repeat_init() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::write(dir.join(".gitignore"), "/target\n.env\n").unwrap();

        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        init(&dir, "my-app", true, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            content.contains("/target") && content.contains(".env"),
            "{content}"
        );
        for line in TERRAFORM_GITIGNORE_ENTRIES {
            let count = content.lines().filter(|l| l.trim() == *line).count();
            assert_eq!(count, 1, "`{line}` must appear exactly once: {content}");
        }
    }

    #[test]
    fn init_without_force_errors_if_aws_workflow_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(dir.join(".github/workflows/aws-deploy.yml"), "existing").unwrap();
        let err = init(&dir, "my-app", false, Target::AwsEcs, false).unwrap_err();
        assert!(matches!(err, ReleaseError::FileExists(_)));
    }

    #[test]
    fn init_with_force_overwrites_nested_aws_workflow_file() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(dir.join(".github/workflows/aws-deploy.yml"), "old content").unwrap();
        init(&dir, "my-app", true, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert_ne!(content, "old content");
    }

    // ── aws-deploy.yml workflow ───────────────────────────────────────────────

    #[test]
    fn aws_workflow_triggers_on_tag_push_and_manual_dispatch() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            content.contains("tags:") && content.contains("\"v*\""),
            "{content}"
        );
        assert!(content.contains("workflow_dispatch"), "{content}");
    }

    #[test]
    fn aws_workflow_never_hardcodes_credentials() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            content.contains("secrets.AWS_ROLE_ARN"),
            "the workflow must source the deploy role from a repository secret: {content}"
        );
        assert!(
            content.contains("id-token: write"),
            "the workflow must request OIDC token permission for AWS login: {content}"
        );
        assert!(
            !content.to_lowercase().contains("aws_secret_access_key"),
            "the workflow must not use long-lived AWS access keys: {content}"
        );
    }

    #[test]
    fn aws_workflow_never_uses_the_invalid_output_none_value() {
        // The AWS CLI's --output only accepts json/yaml/yaml-stream/text/
        // table/off — "none" isn't a valid value and makes the command
        // exit non-zero instead of suppressing output. Discarding via
        // shell redirection works regardless of CLI version (older
        // versions may not support "off" either).
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            !content.contains("--output none"),
            "the workflow must not pass the invalid --output none value to the AWS CLI: {content}"
        );
    }

    #[test]
    fn aws_workflow_runs_migrations_before_updating_the_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let migrate_pos = content
            .find("Run database migrations")
            .expect("workflow must run migrations");
        let deploy_pos = content
            .find("Deploy new image to the ECS service")
            .expect("workflow must deploy to ECS");
        assert!(
            migrate_pos < deploy_pos,
            "migrations must run BEFORE the ECS service is updated: {content}"
        );
    }

    #[test]
    fn aws_workflow_aborts_deploy_on_migration_failure() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let migrate_step = content
            .split("Run database migrations")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("must find the migration step");
        assert!(
            migrate_step.contains("exit 1"),
            "the migration step must exit non-zero on failure so the job (and therefore \
             the deploy step after it) aborts: {migrate_step}"
        );
    }

    #[test]
    fn aws_workflow_serializes_overlapping_runs() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(content.contains("concurrency:"), "{content}");
        assert!(content.contains("cancel-in-progress: false"), "{content}");
    }

    #[test]
    fn aws_workflow_guards_against_superseded_run_before_migrating() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let guard_pos = content
            .find("Abort if a newer run of this workflow exists")
            .expect("workflow must guard against superseded runs");
        let migrate_pos = content.find("Run database migrations").unwrap();
        assert!(
            guard_pos < migrate_pos,
            "the staleness guard must run BEFORE migrating: {content}"
        );
    }

    #[test]
    fn aws_workflow_image_tag_is_unique_per_execution() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(content.contains("GITHUB_RUN_ID"), "{content}");
        assert!(content.contains("GITHUB_RUN_ATTEMPT"), "{content}");
    }

    #[test]
    fn aws_workflow_sanitizes_ref_name_for_docker_tag() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            content.contains("tr -c 'A-Za-z0-9_.-' '-'"),
            "the workflow must map every character outside Docker's tag charset to '-': {content}"
        );
    }

    #[test]
    fn aws_workflow_image_tag_never_starts_with_invalid_character() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            content.contains("sed -E 's/^[.-]+//'"),
            "the workflow must strip a leading '.'/'-' left over from ref sanitization — \
             Docker's tag grammar requires the first character to be a word character: {content}"
        );
    }

    #[test]
    fn aws_workflow_grants_actions_read_for_the_staleness_guard() {
        // Declaring `permissions:` at all switches GITHUB_TOKEN from its
        // default repo scopes to exactly the listed allow-list. The
        // "Abort if a newer run" step calls the Actions API via `gh api`,
        // which needs `actions: read` — without it that step 403s on every
        // single run, after the image has already built and pushed.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let permissions_block = content
            .split("permissions:")
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .expect("workflow must declare a permissions block");
        assert!(
            permissions_block.contains("actions: read"),
            "the permissions block must grant actions: read for the staleness-guard \
             step's `gh api .../actions/workflows/.../runs` call: {permissions_block}"
        );
    }

    #[test]
    fn aws_workflow_documents_describe_task_definition_permission() {
        // The task-definition re-registration steps call
        // `aws ecs describe-task-definition` before `register-task-definition`
        // — a distinct IAM action the documented CI role policy must list,
        // or the very first deploy 403s before ever reaching migration.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let header = content
            .split("\nname: aws-deploy")
            .next()
            .expect("workflow must have a header comment before `name:`");
        assert!(
            header.contains("DescribeTaskDefinition"),
            "the header's documented IAM policy must include ecs:DescribeTaskDefinition: {header}"
        );
        assert!(
            !header.contains("granted ecr:*"),
            "the header must not recommend granting account-wide ecr:* — scope ECR access \
             to the repository ARN instead: {header}"
        );
    }

    #[test]
    fn aws_workflow_subnet_ids_env_is_used_as_json_directly_not_reparsed_from_csv() {
        // ECS_PRIVATE_SUBNET_IDS is documented as the compact JSON array
        // `terraform output -json private_subnet_ids | jq -c .` prints. The
        // migration step must consume it as JSON directly (a bare
        // `tr ',' '\n' | jq -R . | jq -s -c .` reconstruction would mangle
        // a real JSON array like ["subnet-1","subnet-2"] into garbage).
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            content.contains("SUBNETS=$(echo \"$ECS_PRIVATE_SUBNET_IDS\" | jq -c .)"),
            "the migration step must treat ECS_PRIVATE_SUBNET_IDS as JSON already, not \
             reconstruct it from a comma-separated split: {content}"
        );
        assert!(
            !content.contains("tr ',' '\\n' | jq -R ."),
            "the migration step must not reconstruct a JSON array via comma-split: {content}"
        );
    }

    #[test]
    fn aws_workflow_passes_git_provenance_build_args_to_docker() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        for arg in [
            "AUTUMN_BUILD_GIT_SHA",
            "AUTUMN_BUILD_GIT_SHA_SHORT",
            "AUTUMN_BUILD_GIT_BRANCH",
            "AUTUMN_BUILD_GIT_DIRTY",
            "AUTUMN_BUILD_TIMESTAMP",
        ] {
            assert!(content.contains(arg), "{content}");
        }
    }

    #[test]
    fn aws_workflow_registers_new_task_definitions_before_running_or_deploying() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let register_app_pos = content
            .find("register_app")
            .expect("must register the app task def");
        let register_migrate_pos = content
            .find("register_migrate")
            .expect("must register the migrate task def");
        let migrate_pos = content.find("Run database migrations").unwrap();
        let deploy_pos = content.find("Deploy new image to the ECS service").unwrap();
        assert!(register_migrate_pos < migrate_pos, "{content}");
        assert!(register_app_pos < deploy_pos, "{content}");
    }

    #[test]
    fn aws_workflow_strips_bootstrap_entrypoint_only_from_the_app_registration() {
        // Terraform's bootstrap "app" task definition overrides
        // entryPoint/command to make the placeholder nginx image satisfy
        // the ALB health check (main.tf) — describe-task-definition would
        // otherwise carry that override forward onto the REAL image, which
        // has no nginx and runs as an unprivileged user, so the container
        // would exit immediately instead of falling through to its own
        // Dockerfile ENTRYPOINT/CMD. The "migrate" family's own `command`
        // (autumn migrate) is intentional and permanent, so its
        // registration step must NOT strip it.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let app_step = content
            .split("Register a new \"app\" task definition revision")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("must find the app registration step");
        assert!(
            app_step.contains(
                "del(.containerDefinitions[0].entryPoint, .containerDefinitions[0].command)"
            ),
            "the app registration step must strip the bootstrap entryPoint/command before \
             registering the real image: {app_step}"
        );
        let migrate_step = content
            .split("Register a new \"migrate\" task definition revision")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("must find the migrate registration step");
        assert!(
            !migrate_step.contains("del(.containerDefinitions[0].entryPoint"),
            "the migrate registration step must NOT strip its own (intentional, permanent) \
             command: {migrate_step}"
        );
    }

    #[test]
    fn aws_workflow_waits_for_service_stability_after_deploying() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        assert!(
            content.contains("aws ecs wait services-stable"),
            "the workflow must wait for the new deployment to stabilize (so the circuit \
             breaker's rollback has a chance to be observed) rather than exiting immediately \
             after force-new-deployment: {content}"
        );
    }

    #[test]
    fn aws_workflow_detects_circuit_breaker_rollback_after_stabilizing() {
        // `services-stable`'s predicate only requires ONE deployment to
        // have runningCount == desiredCount — if the new revision failed
        // its health checks, the circuit breaker rolls the service back to
        // the PREVIOUS revision, and that older deployment satisfies the
        // waiter just as well. Without an explicit check, this step would
        // report success even though the requested task definition was
        // never actually running.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let deploy_step = content
            .split("Deploy new image to the ECS service")
            .nth(1)
            .and_then(|rest| rest.split("env:").next())
            .expect("must find the deploy step");
        assert!(
            deploy_step.contains("deployments[?status=='PRIMARY']")
                || deploy_step.contains("deployments[?status==`PRIMARY`]"),
            "the deploy step must inspect the PRIMARY deployment's task definition after \
             waiting: {deploy_step}"
        );
        assert!(
            deploy_step.contains("DEPLOYED_TASK_DEF") && deploy_step.contains("APP_TASK_DEF_ARN"),
            "the deploy step must compare the deployed task definition against the requested \
             one and fail if they differ: {deploy_step}"
        );
    }

    #[test]
    fn aws_workflow_stops_migration_task_on_timeout() {
        // ECS tasks have no runtime limit of their own; without an
        // explicit stop-task call, a migration that exceeds its polling
        // budget keeps running in the background after this job gives up
        // and releases the concurrency group — letting a later deploy
        // start a second migration while the timed-out one is still
        // mutating the schema.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let migrate_step = content
            .split("Run database migrations")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("must find the migration step");
        let timeout_block = migrate_step
            .split("did not finish within the time budget")
            .nth(1)
            .expect("must find the timeout-budget-exceeded branch");
        assert!(
            timeout_block.contains("aws ecs stop-task"),
            "the timeout branch must explicitly stop the migration task, not just exit: \
             {timeout_block}"
        );
        assert!(
            timeout_block.contains("wait tasks-stopped"),
            "the timeout branch must wait for the stop to actually take effect before this \
             job (and its concurrency group) releases: {timeout_block}"
        );
    }

    #[test]
    fn aws_workflow_documents_stop_task_permission() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::AwsEcs, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/aws-deploy.yml")).unwrap();
        let header = content
            .split("\nname: aws-deploy")
            .next()
            .expect("workflow must have a header comment before `name:`");
        assert!(
            header.contains("StopTask"),
            "the header's documented IAM policy must include ecs:StopTask, used to kill a \
             timed-out migration task: {header}"
        );
    }

    // ── aws-ecs workflow discoverability (git root vs. workspace member) ──────

    #[test]
    fn aws_ecs_workflow_relocation_warning_flags_a_nested_workspace_member() {
        let tmp = TempDir::new().unwrap();
        let git_root = tmp.path().to_path_buf();
        fs::create_dir_all(git_root.join(".git")).unwrap();
        let member_dir = git_root.join("examples").join("blog");
        fs::create_dir_all(&member_dir).unwrap();

        let warning =
            nested_workflow_relocation_warning(&member_dir, ".github/workflows/aws-deploy.yml")
                .expect("a workflow nested under a workspace member must be flagged");
        assert!(warning.contains("aws-deploy.yml"), "{warning}");
        assert!(
            warning.contains("working-directory: examples/blog"),
            "{warning}"
        );
    }

    #[test]
    fn is_terraform_target_covers_all_terraform_targets() {
        assert!(is_terraform_target(Target::AzureContainerApps));
        assert!(is_terraform_target(Target::AwsAppRunner));
        assert!(is_terraform_target(Target::AwsEcs));
        assert!(is_terraform_target(Target::GcpCloudRun));
        assert!(!is_terraform_target(Target::Default));
        assert!(!is_terraform_target(Target::Fly));
        assert!(!is_terraform_target(Target::DockerCompose));
    }

    // ── --target=gcp-cloud-run ───────────────────────────────────────────────

    #[test]
    fn gcp_cloud_run_target_creates_all_expected_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/gcp-deploy.yml",
        ] {
            assert!(
                dir.join(name).is_file(),
                "{name} must be created for --target=gcp-cloud-run"
            );
        }
        // Base scaffolding is still emitted alongside the GCP-specific files.
        assert!(dir.join("Dockerfile").is_file());
        assert!(dir.join(".dockerignore").is_file());
        assert!(dir.join("autumn.production.toml.example").is_file());
    }

    #[test]
    fn gcp_cloud_run_target_returns_nested_workflow_path_in_created_list() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        let files = init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        assert!(
            files
                .iter()
                .any(|f| f == ".github/workflows/gcp-deploy.yml"),
            "created-files list must include the nested workflow path: {files:?}"
        );
    }

    #[test]
    fn default_target_does_not_create_gcp_files() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::Default, false).unwrap();
        for name in [
            "main.tf",
            "variables.tf",
            "outputs.tf",
            "terraform.tfvars.example",
            ".github/workflows/gcp-deploy.yml",
        ] {
            assert!(
                !dir.join(name).exists(),
                "{name} must NOT be created for the default target"
            );
        }
    }

    #[test]
    fn gcp_main_tf_has_artifact_registry_and_cloud_run_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_artifact_registry_repository"),
            "main.tf must provision an Artifact Registry repository: {content}"
        );
        assert!(
            content.contains("resource \"google_cloud_run_v2_service\""),
            "main.tf must provision the Cloud Run service: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_has_cloud_sql_with_private_ip_only() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_sql_database_instance"),
            "main.tf must provision a Cloud SQL PostgreSQL instance: {content}"
        );
        let ip_config = content
            .split("ip_configuration {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("main.tf must declare an ip_configuration block");
        assert!(
            ip_config.contains("ipv4_enabled") && ip_config.contains("false"),
            "Cloud SQL must not have a public IPv4 address — private IP only via the \
             VPC connector: {ip_config}"
        );
        assert!(
            ip_config.contains("private_network"),
            "Cloud SQL's ip_configuration must set private_network: {ip_config}"
        );
    }

    #[test]
    fn gcp_main_tf_has_vpc_access_connector_wired_into_cloud_run() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_vpc_access_connector"),
            "main.tf must provision a Serverless VPC Access connector: {content}"
        );
        assert!(
            content.contains("google_vpc_access_connector.this.id"),
            "the Cloud Run service must reference the VPC connector: {content}"
        );
        assert!(
            content.contains("egress    = \"PRIVATE_RANGES_ONLY\"")
                || content.contains("egress = \"PRIVATE_RANGES_ONLY\""),
            "vpc_access egress should stay scoped to private ranges so general internet \
             egress doesn't route through the connector: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_postgres_database_name_is_length_bounded() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let raw_local_line = content
            .lines()
            .find(|l| l.trim_start().starts_with("postgres_database_name_raw"))
            .expect("main.tf must declare a postgres_database_name_raw local");
        assert!(
            raw_local_line.contains("substr(") && raw_local_line.contains(", 63)"),
            "the Postgres database name must be truncated to 63 characters: {raw_local_line}"
        );
    }

    #[test]
    fn gcp_main_tf_postgres_database_name_avoids_reserved_names() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        for reserved in ["postgres", "cloudsqladmin", "template0", "template1"] {
            assert!(
                content.contains(&format!("\"{reserved}\"")),
                "the reserved-name guard must list {reserved:?}: {content}"
            );
        }
        let database_name_local = content
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("postgres_database_name ="))
            .take_while(|l| !l.trim_start().starts_with('}'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            database_name_local.contains("contains(") && database_name_local.contains("_prod"),
            "postgres_database_name must fall back to a suffixed name when the \
             sanitized value collides with a reserved database name: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_has_secret_manager_with_database_and_signing_secrets() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_secret_manager_secret"),
            "main.tf must provision Secret Manager secrets: {content}"
        );
        assert!(
            content.contains("AUTUMN_DATABASE__PRIMARY_URL"),
            "main.tf must wire the primary DB URL env var from Secret Manager: {content}"
        );
        assert!(
            content.contains("AUTUMN_SECURITY__SIGNING_SECRET"),
            "main.tf must wire the signing secret env var from Secret Manager: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_secret_access_is_scoped_per_secret_not_project_wide() {
        // A project-wide `roles/secretmanager.secretAccessor` grant (via
        // google_project_iam_member) would let a compromised container read
        // every secret in the project — the grant must instead be scoped to
        // exactly the secrets this app uses via
        // google_secret_manager_secret_iam_member.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_secret_manager_secret_iam_member"),
            "secret access must be granted per-secret via \
             google_secret_manager_secret_iam_member: {content}"
        );
        assert!(
            !content.contains("google_project_iam_member\" \"secret"),
            "must not grant a project-wide secretAccessor role: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_wires_trusted_hosts_so_prod_actually_binds() {
        // AUTUMN_PROFILE=prod makes fail_fast_on_invalid_trusted_hosts exit
        // the process immediately when security.trusted_hosts.hosts is
        // empty (see docs/guide/deployment.md's "Trusted hosts" section).
        // Without this, the container never binds after the first real
        // deploy — it would crash-loop instead of serving traffic.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS"),
            "main.tf must set AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS on the Cloud Run \
             service: {content}"
        );
        assert!(
            content.contains("local.service_url_host"),
            "the trusted host must be derived from local.service_url_host (known at \
             plan time from the project number), not require a second apply: {content}"
        );
        assert!(
            content.contains("data.google_project.this.number"),
            "service_url_host must be derived from the project NUMBER (Cloud Run's \
             default URL format), not the project ID: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_grants_public_invoker_access() {
        // Cloud Run services default to requiring IAM-authenticated
        // invocations — without an explicit allUsers invoker grant, the
        // deployed app would 403 every request from a browser.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_cloud_run_v2_service_iam_member"),
            "main.tf must grant an IAM invoker binding on the Cloud Run service: {content}"
        );
        assert!(
            content.contains("roles/run.invoker") && content.contains("allUsers"),
            "main.tf must grant roles/run.invoker to allUsers for public ingress: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_service_account_scoped_to_cloudsql_client_not_broader() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_service_account"),
            "main.tf must provision a dedicated runtime service account: {content}"
        );
        assert!(
            content.contains("roles/cloudsql.client"),
            "the runtime service account must be granted roles/cloudsql.client: {content}"
        );
        assert!(
            !content.contains("roles/editor") && !content.contains("roles/owner"),
            "the runtime service account must never be granted a broad primitive role: \
             {content}"
        );
    }

    #[test]
    fn gcp_main_tf_has_one_shot_migration_job() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_cloud_run_v2_job") && content.contains("\"migrate\""),
            "main.tf must provision a one-shot Cloud Run Job for migrations: {content}"
        );
        assert!(
            content.contains("autumn migrate"),
            "the migration job must run `autumn migrate`: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_redis_is_off_by_default_and_gated() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            main_tf.contains("google_redis_instance"),
            "main.tf must provision an optional Memorystore Redis instance: {main_tf}"
        );
        assert!(
            main_tf.contains("var.enable_redis_cache ? 1 : 0"),
            "the Redis instance must be gated behind enable_redis_cache via count: {main_tf}"
        );

        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        let default_line = variables_tf
            .lines()
            .skip_while(|l| !l.contains("variable \"enable_redis_cache\""))
            .find(|l| l.trim_start().starts_with("default"))
            .expect("enable_redis_cache must declare a default");
        assert!(
            default_line.contains("false"),
            "enable_redis_cache must default to false: {default_line}"
        );
    }

    #[test]
    fn gcp_main_tf_wires_redis_env_vars_into_cloud_run_service() {
        // Provisioning the Redis instance alone does nothing — Autumn's
        // cache subsystem only activates it via these two env vars. A
        // regression dropping the `dynamic "env"` blocks would silently
        // leave the (paid) Memorystore instance unused while every other
        // "redis is gated" assertion still passes.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("AUTUMN_CACHE__BACKEND"),
            "main.tf must wire AUTUMN_CACHE__BACKEND into the Cloud Run service when \
             Redis is enabled: {content}"
        );
        assert!(
            content.contains("AUTUMN_CACHE__REDIS__URL")
                && content.contains("google_secret_manager_secret.redis_url[0].secret_id"),
            "main.tf must wire AUTUMN_CACHE__REDIS__URL from the redis_url secret: {content}"
        );
    }

    #[test]
    fn gcp_main_tf_has_private_services_access_peering_for_cloud_sql() {
        // Cloud SQL's (and Redis's) private IP depends entirely on this
        // peering; ip_configuration alone doesn't provision it.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_compute_global_address") && content.contains("VPC_PEERING"),
            "main.tf must reserve a VPC peering range for private services access: \
             {content}"
        );
        assert!(
            content.contains("google_service_networking_connection"),
            "main.tf must establish the private services access peering connection: \
             {content}"
        );
    }

    #[test]
    fn gcp_main_tf_enables_required_apis() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("google_project_service"),
            "main.tf must enable the GCP APIs this scaffold's resources depend on, so a \
             fresh project works without a manual `gcloud services enable` step: {content}"
        );
        for api in [
            "run.googleapis.com",
            "sqladmin.googleapis.com",
            "secretmanager.googleapis.com",
            "vpcaccess.googleapis.com",
            "artifactregistry.googleapis.com",
            "servicenetworking.googleapis.com",
            // google_compute_network / google_compute_global_address are
            // Compute Engine resources — a fresh project without this API
            // pre-enabled would fail the very first apply at VPC creation.
            "compute.googleapis.com",
        ] {
            assert!(
                content.contains(api),
                "main.tf must enable {api}: {content}"
            );
        }
    }

    #[test]
    fn gcp_main_tf_redis_avoids_a_tls_mode_the_client_cant_verify() {
        // Memorystore's SERVER_AUTHENTICATION mode presents a private,
        // instance-specific CA — not a publicly-trusted one — and
        // autumn-cache-redis's RedisCache::connect has no hook to trust a
        // custom CA. Unlike AWS ElastiCache/Azure Redis Cache (both use
        // publicly-trusted certs and work with the same client), enabling
        // that mode here would generate a rediss:// URL the app can never
        // actually connect with. Traffic stays inside the private VPC
        // regardless, so AUTH-only (no transit encryption) is correct here.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            content.contains("transit_encryption_mode = \"DISABLED\""),
            "google_redis_instance must not claim SERVER_AUTHENTICATION — the client \
             can't verify Memorystore's private CA: {content}"
        );
        assert!(
            content.contains("auth_enabled            = true"),
            "google_redis_instance must still require AUTH: {content}"
        );
        let redis_url_version = content
            .split("resource \"google_secret_manager_secret_version\" \"redis_url\"")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .expect("main.tf must declare the redis_url secret version");
        assert!(
            redis_url_version.contains("\"redis://:"),
            "the derived redis_url secret must use the redis:// scheme, matching \
             transit_encryption_mode = DISABLED, not rediss://: {redis_url_version}"
        );
    }

    #[test]
    fn gcp_scale_defaults_match_the_issue() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let variables_tf = fs::read_to_string(dir.join("variables.tf")).unwrap();
        for (var_name, expected) in [("min_instances", "1"), ("max_instances", "10")] {
            let default_line = variables_tf
                .lines()
                .skip_while(|l| !l.contains(&format!("variable \"{var_name}\"")))
                .find(|l| l.trim_start().starts_with("default"))
                .unwrap_or_else(|| panic!("{var_name} must declare a default"));
            assert!(
                default_line.contains(expected),
                "{var_name} must default to {expected}: {default_line}"
            );
        }

        let main_tf = fs::read_to_string(dir.join("main.tf")).unwrap();
        assert!(
            main_tf.contains("max_instance_request_concurrency = 80"),
            "main.tf must set concurrency to 80 requests per instance (Cloud Run's own \
             default, made explicit and tunable): {main_tf}"
        );
    }

    #[test]
    fn gcp_main_tf_documents_always_allocated_cpu_option() {
        // Issue #1280 asks for a "CPU: always-allocated option commented
        // out for latency-sensitive workloads" — Cloud Run's cost-optimized
        // default only allocates CPU while a request is in flight
        // (cpu_idle = true); the always-allocated alternative must be
        // present, just not the active setting.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("main.tf")).unwrap();
        let resources_block = content
            .split("resources {")
            .nth(1)
            .and_then(|rest| rest.split("\n      }").next())
            .expect("the Cloud Run service container must declare a resources block");
        assert!(
            resources_block.contains("cpu_idle = true"),
            "cpu_idle must default to true (cost-optimized, scale-to-zero-friendly): \
             {resources_block}"
        );
        assert!(
            resources_block
                .lines()
                .any(|l| l.trim_start().starts_with("# cpu_idle = false")),
            "an always-allocated-CPU option (cpu_idle = false) must be present, \
             commented out, for latency-sensitive workloads: {resources_block}"
        );
    }

    #[test]
    fn gcp_secrets_have_no_default_and_are_sensitive() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("variables.tf")).unwrap();
        for var_name in ["database_admin_password", "signing_secret"] {
            let block = content
                .split(&format!("variable \"{var_name}\""))
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .unwrap_or_else(|| panic!("variables.tf must declare {var_name}"));
            assert!(
                block.contains("sensitive") && block.contains("true"),
                "{var_name} must be marked sensitive: {block}"
            );
            assert!(
                !block.contains("default"),
                "{var_name} must have no default value: {block}"
            );
        }
    }

    #[test]
    fn gcp_no_committed_secret_literals() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let tfvars = fs::read_to_string(dir.join("terraform.tfvars.example")).unwrap();
        // The documented placeholder ("# database_admin_password = (set via ...)") is a
        // commented-out line, matching the azure-container-apps/aws-* targets' identical
        // convention — only a non-comment assignment would be a real committed secret.
        for line in tfvars.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.starts_with("database_admin_password")
                    && !trimmed.starts_with("signing_secret"),
                "terraform.tfvars.example must not assign the secret variables directly \
                 on a non-comment line: {line:?} in {tfvars}"
            );
        }
        assert!(
            tfvars.contains("TF_VAR_database_admin_password")
                && tfvars.contains("TF_VAR_signing_secret"),
            "terraform.tfvars.example must document the TF_VAR_* env var pattern instead: \
             {tfvars}"
        );
    }

    #[test]
    fn gcp_target_adds_terraform_gitignore_entries() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
        for pattern in [
            ".terraform/",
            "*.tfstate",
            "*.tfstate.*",
            "terraform.tfvars",
        ] {
            assert!(
                gitignore.contains(pattern),
                ".gitignore must contain {pattern:?} for the gcp-cloud-run target: {gitignore}"
            );
        }
    }

    #[test]
    fn gcp_dockerignore_excludes_terraform_state() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".dockerignore")).unwrap();
        for pattern in [".terraform/", "*.tfstate", "terraform.tfvars"] {
            assert!(
                content.contains(pattern),
                ".dockerignore must exclude {pattern:?} so terraform.tfstate is never sent \
                 to the Docker build context: {content}"
            );
        }
    }

    #[test]
    fn gcp_outputs_tf_declares_expected_outputs_with_no_unsubstituted_placeholders() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-blog");
        init(&dir, "my-blog", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join("outputs.tf")).unwrap();
        for output in [
            "service_url",
            "service_name",
            "artifact_registry_repository_url",
            "migrate_job_name",
            "service_account_email",
        ] {
            assert!(
                content.contains(&format!("output \"{output}\"")),
                "outputs.tf must declare output {output:?}: {content}"
            );
        }
        assert!(
            content.contains("my-blog"),
            "outputs.tf must substitute the project name: {content}"
        );
        assert!(
            !content.contains("{{"),
            "outputs.tf must not contain unsubstituted placeholders: {content}"
        );
    }

    #[test]
    fn init_without_force_errors_if_gcp_workflow_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(dir.join(".github/workflows/gcp-deploy.yml"), "existing").unwrap();
        let err = init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap_err();
        assert!(matches!(err, ReleaseError::FileExists(_)));
    }

    #[test]
    fn gcp_target_gitignore_merge_is_idempotent_on_repeat_init() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        init(&dir, "my-app", true, Target::GcpCloudRun, false).unwrap();
        let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let terraform_dir_count = gitignore
            .lines()
            .filter(|l| l.trim() == ".terraform/")
            .count();
        assert_eq!(
            terraform_dir_count, 1,
            "re-running init with --force must not duplicate gitignore entries: {gitignore}"
        );
    }

    // ── gcp-cloud-run workflow ────────────────────────────────────────────────

    #[test]
    fn gcp_workflow_triggers_on_tag_push_and_manual_dispatch() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("tags:"),
            "gcp-deploy.yml must trigger on tag push: {content}"
        );
        assert!(
            content.contains("workflow_dispatch:"),
            "gcp-deploy.yml must also support manual dispatch: {content}"
        );
    }

    #[test]
    fn gcp_workflow_authenticates_via_workload_identity_federation() {
        // No long-lived service account key: the workflow must use OIDC via
        // google-github-actions/auth with a workload identity provider, not
        // a downloaded JSON key.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("google-github-actions/auth@v2"),
            "gcp-deploy.yml must authenticate via google-github-actions/auth: {content}"
        );
        assert!(
            content.contains("workload_identity_provider"),
            "gcp-deploy.yml must use Workload Identity Federation, not a static key: {content}"
        );
        assert!(
            !content.to_lowercase().contains("credentials_json"),
            "gcp-deploy.yml must not authenticate via a downloaded service-account key: \
             {content}"
        );
    }

    #[test]
    fn gcp_workflow_sets_up_gcloud_and_configures_docker_before_building() {
        // ubuntu-latest doesn't ship gcloud, and `docker push` to Artifact
        // Registry needs a configured credential helper — without both,
        // every build/push step in this workflow fails.
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        let auth_pos = content
            .find("google-github-actions/auth@v2")
            .expect("auth step must be present");
        let setup_pos = content
            .find("google-github-actions/setup-gcloud@v2")
            .expect("gcp-deploy.yml must set up the gcloud CLI");
        let configure_docker_pos = content
            .find("gcloud auth configure-docker")
            .expect("gcp-deploy.yml must configure Docker for Artifact Registry");
        let build_pos = content
            .find("docker build \\")
            .expect("build step must be present");
        assert!(
            auth_pos < setup_pos
                && setup_pos < configure_docker_pos
                && configure_docker_pos < build_pos,
            "auth, then gcloud setup, then Docker configuration must all precede the \
             build step: {content}"
        );
    }

    #[test]
    fn gcp_workflow_builds_pushes_to_artifact_registry_and_deploys() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("docker build"),
            "gcp-deploy.yml must build the release image: {content}"
        );
        assert!(
            content.contains("docker push"),
            "gcp-deploy.yml must push to Artifact Registry: {content}"
        );
        assert!(
            content.contains("gcloud run services update"),
            "gcp-deploy.yml must deploy the new image to the Cloud Run service: {content}"
        );
    }

    #[test]
    fn gcp_workflow_passes_git_provenance_build_args_to_docker() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        for arg in [
            "AUTUMN_BUILD_GIT_SHA",
            "AUTUMN_BUILD_GIT_SHA_SHORT",
            "AUTUMN_BUILD_GIT_BRANCH",
            "AUTUMN_BUILD_GIT_DIRTY",
            "AUTUMN_BUILD_TIMESTAMP",
        ] {
            assert!(
                content.contains(&format!("--build-arg {arg}=")),
                "gcp-deploy.yml's docker build must pass --build-arg {arg}: {content}"
            );
        }
    }

    #[test]
    fn gcp_workflow_updates_migration_job_image_before_executing_it() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();

        let migration_step = content
            .split("Run database migrations")
            .nth(1)
            .and_then(|rest| rest.split("- name:").next())
            .expect("a 'Run database migrations' step must exist");

        let update_pos = migration_step
            .find("gcloud run jobs update")
            .expect("the migration job's image must be updated first");
        let execute_pos = migration_step
            .find("gcloud run jobs execute")
            .expect("`jobs execute` must follow to actually run the now-updated job");
        assert!(
            update_pos < execute_pos,
            "the job's image must be updated BEFORE it's executed: {migration_step}"
        );

        let update_block = &migration_step[update_pos..execute_pos];
        assert!(
            update_block.contains("--image"),
            "`jobs update` must be the one that carries --image: {update_block}"
        );

        let execute_block = &migration_step[execute_pos..];
        assert!(
            !execute_block.contains("--image"),
            "`jobs execute` must not carry --image: {execute_block}"
        );
        assert!(
            execute_block.contains("--wait"),
            "`jobs execute` must block until the execution finishes via --wait, so the \
             deploy step below never runs against an unmigrated schema: {execute_block}"
        );
    }

    #[test]
    fn gcp_workflow_never_hardcodes_credentials() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("secrets."),
            "gcp-deploy.yml must source credentials from GitHub Actions secrets, never \
             hardcode them: {content}"
        );
    }

    #[test]
    fn gcp_workflow_runs_migrations_before_updating_the_service() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        let migrate_pos = content
            .find("gcloud run jobs execute \"$GCP_MIGRATE_JOB_NAME\"")
            .expect("migration job execute must be present");
        let deploy_pos = content
            .find("gcloud run services update \"$GCP_SERVICE_NAME\"")
            .expect("deploy step must be present");
        assert!(
            migrate_pos < deploy_pos,
            "the migration job must run BEFORE the service is updated to the new image: \
             {content}"
        );
    }

    #[test]
    fn gcp_workflow_sanitizes_ref_name_for_docker_tag() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("tr -c 'A-Za-z0-9_.-' '-'"),
            "gcp-deploy.yml must map every character outside Docker's tag charset to \"-\": \
             {content}"
        );
        assert!(
            !content.contains(":${GITHUB_REF_NAME}") && !content.contains(":$GITHUB_REF_NAME"),
            "no docker/gcloud command may use the raw, unsanitized ref as an image tag: \
             {content}"
        );
    }

    #[test]
    fn gcp_workflow_image_tag_is_unique_per_execution() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("${GITHUB_SHA:0:12}"),
            "the computed image tag must include the commit SHA: {content}"
        );
        assert!(
            content.contains("${GITHUB_RUN_ID}") && content.contains("${GITHUB_RUN_ATTEMPT}"),
            "the computed image tag must also include the run ID and run attempt: {content}"
        );
    }

    #[test]
    fn gcp_workflow_serializes_overlapping_runs() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("concurrency:"),
            "gcp-deploy.yml must define a concurrency group so overlapping runs queue \
             instead of racing: {content}"
        );
        assert!(
            content.contains("cancel-in-progress: false"),
            "cancel-in-progress must be false — killing a run mid-migration or \
             mid-cutover is worse than making the next run wait: {content}"
        );
    }

    #[test]
    fn gcp_workflow_guards_against_superseded_run_before_migrating() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "my-app");
        init(&dir, "my-app", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("actions: read"),
            "gcp-deploy.yml must grant actions: read to query other workflow runs: {content}"
        );
        assert!(
            content.contains("run_number > ${{ github.run_number }}"),
            "the guard must compare against other runs' run_number: {content}"
        );

        let guard_pos = content
            .find("gh api")
            .expect("the run_number staleness guard must be present");
        let migrate_pos = content
            .find("gcloud run jobs execute \"$GCP_MIGRATE_JOB_NAME\"")
            .expect("migration job execute must be present");
        let deploy_pos = content
            .find("gcloud run services update \"$GCP_SERVICE_NAME\"")
            .expect("deploy step must be present");
        assert!(
            guard_pos < migrate_pos && migrate_pos < deploy_pos,
            "the staleness guard must run BEFORE migration, which must run BEFORE \
             deploy: {content}"
        );
    }

    #[test]
    fn gcp_workflow_sources_service_name_from_terraform_not_hardcoded() {
        let tmp = TempDir::new().unwrap();
        let dir = make_project(&tmp, "My_Test_App");
        init(&dir, "My_Test_App", false, Target::GcpCloudRun, false).unwrap();
        let content = fs::read_to_string(dir.join(".github/workflows/gcp-deploy.yml")).unwrap();
        assert!(
            content.contains("vars.GCP_SERVICE_NAME"),
            "GCP_SERVICE_NAME must be sourced from a repository variable (terraform \
             output service_name), not hardcoded: {content}"
        );
        assert!(
            !content.contains("My_Test_App") && !content.contains("my-test-app"),
            "gcp-deploy.yml must not bake in any form of the project name as a GCP \
             resource identifier: {content}"
        );
        assert!(
            !content.contains("{{project_name}}"),
            "gcp-deploy.yml must not contain unsubstituted placeholders: {content}"
        );
    }
}
