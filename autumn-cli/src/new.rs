//! Project scaffolding for `autumn new <name>`.
//!
//! Generates a complete Autumn project directory from embedded templates.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use autumn_web::credentials::{MasterKey, encrypt};

pub mod templates {
    pub const CARGO_TOML: &str = include_str!("templates/Cargo.toml.tmpl");
    /// JSON-first API flavor (`autumn new --api`): drops the HTML/CSS view
    /// stack (`maud`) and disables `autumn-web`'s default view features.
    pub const CARGO_API_TOML: &str = include_str!("templates/Cargo.api.toml.tmpl");
    pub const README: &str = include_str!("templates/README.md.tmpl");
    pub const MAIN_RS: &str = include_str!("templates/main.rs.tmpl");
    /// JSON-first API flavor: `Json<...>` handlers, no `maud`/layout/HTML.
    pub const MAIN_API_RS: &str = include_str!("templates/main.api.rs.tmpl");
    pub const AUTUMN_TOML: &str = include_str!("templates/autumn.toml.tmpl");
    pub const DOCKERFILE: &str = include_str!("templates/Dockerfile.tmpl");
    /// JSON-first API flavor: no Tailwind download / CSS build / static copy.
    pub const DOCKERFILE_API: &str = include_str!("templates/Dockerfile.api.tmpl");
    pub const DOCKERIGNORE: &str = include_str!("templates/.dockerignore.tmpl");
    pub const BUILD_RS: &str = include_str!("templates/build.rs.tmpl");
    /// JSON-first API flavor: build provenance only, no Tailwind CSS step.
    pub const BUILD_API_RS: &str = include_str!("templates/build.api.rs.tmpl");
    pub const INPUT_CSS: &str = include_str!("templates/input.css.tmpl");
    pub const TAILWIND_CONFIG: &str = include_str!("templates/tailwind.config.js.tmpl");
    pub const GITIGNORE: &str = include_str!("templates/gitignore.tmpl");
    pub const ENV_EXAMPLE: &str = include_str!("templates/env.example.tmpl");
    pub const SEED_RS: &str = include_str!("templates/seed.rs.tmpl");
    pub const SEED_CARGO_TOML: &str = include_str!("templates/seed_Cargo.toml.tmpl");
    pub const INTEGRATION_TEST: &str = include_str!("templates/tests/integration_test.rs.tmpl");
    pub const CI_WORKFLOW: &str = include_str!("templates/.github/workflows/ci.yml.tmpl");
    /// Dependency advisory policy read by the generated CI's `cargo deny check
    /// advisories` gate (issue #1600). Deliberately *not* framework-owned — see
    /// [`framework_owned_files`].
    pub const DENY_TOML: &str = include_str!("templates/deny.toml.tmpl");
    pub const RUST_TOOLCHAIN: &str = include_str!("templates/rust-toolchain.toml.tmpl");
    pub const RUSTFMT: &str = include_str!("templates/rustfmt.toml.tmpl");
    pub const CLIPPY: &str = include_str!("templates/clippy.toml.tmpl");
}

/// Variables substituted into project and starter template files.
///
/// Shared by the base `autumn new` scaffold and the starter render path so both
/// honour the exact same substitution tokens (issue #993 reuses the existing
/// `new` render path for starters).
pub struct TemplateVars<'a> {
    /// The project name exactly as given on the CLI (e.g. `my-app`).
    pub project_name: &'a str,
    /// The Rust crate name (`project_name` with `-` replaced by `_`).
    pub crate_name: &'a str,
    /// The `autumn-web` version this CLI was built against.
    pub autumn_version: &'a str,
    /// The MSRV stamped into generated `Cargo.toml` files.
    pub rust_version: &'a str,
}

/// Render a single embedded template, substituting the standard `{{…}}` tokens.
///
/// Templates are embedded at compile time and may be checked out with CRLF line
/// endings on Windows (git autocrlf); normalising to LF first keeps the
/// `\n`-anchored rewrites (and the generated output) deterministic across hosts.
pub fn render_template(content: &str, vars: &TemplateVars<'_>) -> String {
    content
        .replace("\r\n", "\n")
        .replace("{{project_name}}", vars.project_name)
        .replace("{{crate_name}}", vars.crate_name)
        .replace("{{autumn_version}}", vars.autumn_version)
        .replace("{{rust_version}}", vars.rust_version)
}

/// Errors that can occur during project generation.
#[derive(Debug, thiserror::Error)]
pub enum NewError {
    /// The project name is not a valid Rust package name.
    #[error("invalid project name '{0}': {1}")]
    InvalidName(String, String),

    /// A directory with this name already exists.
    #[error("directory '{0}' already exists")]
    AlreadyExists(String),

    /// The requested option combination is not supported.
    #[error("incompatible options: {0}")]
    IncompatibleOptions(String),

    /// Filesystem error during project creation.
    #[error("failed to create project: {0}")]
    Io(#[from] std::io::Error),
}

/// Entry point called from `main.rs` and delegates to [`generate_with`].
pub fn run(name: &str, opts: GenerateOptions) {
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("Error: cannot determine current directory: {e}");
        std::process::exit(1);
    });
    let result = if opts == GenerateOptions::default() {
        generate(name, &cwd)
    } else {
        generate_with(name, &cwd, opts)
    };
    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

/// Optional toggles applied to project generation.
// Independent on/off scaffolding toggles; a bitflags/enum here would be less
// clear than named booleans at the (few) call sites.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct GenerateOptions {
    /// Scaffold the optional i18n module (`i18n/en.ftl`, `[i18n]` block,
    /// `i18n` feature flag on `autumn-web`).
    pub with_i18n: bool,
    /// Scaffold the optional seed binary and enable `autumn-web/seed`.
    pub with_seed: bool,
    /// Daemon-flavored starter: a model-free app that builds with **no** Postgres
    /// (drops the default `db` feature and migrations), ready for `autumn serve`.
    pub with_daemon: bool,
    /// Managed/bundled-Postgres daemon starter: keeps `db`, enables the
    /// `managed-pg` feature, and wires a managed local Postgres provider.
    /// Implies [`Self::with_daemon`]-style serve usage. Mutually exclusive with a
    /// DB-free daemon.
    pub with_bundled_pg: bool,
    /// JSON-first API flavor (`autumn new --api`): emit a lean skeleton with no
    /// HTML/CSS/Tailwind artifacts. Handlers return `Json<...>`; the `maud`
    /// dependency and `autumn-web`'s view features (maud/htmx/tailwind) are
    /// dropped, and the Tailwind/CSS build step, `input.css`, `tailwind.config.js`,
    /// and vendored JS/static assets are not scaffolded. Keeps `db`/migrations so
    /// database features still work. Mutually exclusive with the daemon flavors.
    pub with_api: bool,
}

/// Generate a new Autumn project under `parent_dir/name` with default options.
pub fn generate(name: &str, parent_dir: &Path) -> Result<(), NewError> {
    generate_with(name, parent_dir, GenerateOptions::default())
}

/// Reject unsupported flag combinations before any files are written.
pub fn check_option_combination(opts: GenerateOptions) -> Result<(), NewError> {
    // The API flavor and the daemon flavors are different app shapes with
    // conflicting `autumn-web` feature sets: `--api` drops the view stack
    // (maud/htmx/tailwind) for a pure-JSON app, while `--daemon`/`--bundled-pg`
    // keep it. Composing them would produce contradictory Cargo features, so
    // reject the combination rather than scaffolding an incoherent project.
    // (`--api` still composes with `--with-i18n` and `--with-seed`.)
    if opts.with_api && (opts.with_daemon || opts.with_bundled_pg) {
        return Err(NewError::IncompatibleOptions(
            "--api scaffolds a JSON-first app without the HTML/CSS view stack, so \
             it cannot be combined with --daemon or --bundled-pg (which scaffold \
             daemon apps that keep the view stack)"
                .to_owned(),
        ));
    }
    // The DB-free daemon starter builds with no database, so a seed binary
    // (which needs `autumn_web::seed::SeedContext` and the `db` feature) cannot
    // compile. Reject the combination rather than scaffolding a broken project.
    if opts.with_daemon && !opts.with_bundled_pg && opts.with_seed {
        return Err(NewError::IncompatibleOptions(
            "--daemon scaffolds a database-free app, so --with-seed is not \
             supported (seeding requires a database; use --bundled-pg for a \
             daemon with a managed Postgres)"
                .to_owned(),
        ));
    }
    // A managed-Postgres daemon owns its database URL at runtime (chosen by the
    // provider); the `autumn seed` CLI is a separate process that only reads
    // env/config URLs, so it can't reach the managed DB. Reject the combo.
    if opts.with_bundled_pg && opts.with_seed {
        return Err(NewError::IncompatibleOptions(
            "--bundled-pg manages Postgres inside the daemon, so the `autumn \
             seed` CLI cannot reach its database; --with-seed is not supported \
             with --bundled-pg. Seed from the app instead (e.g. a startup hook)."
                .to_owned(),
        ));
    }
    Ok(())
}

/// Generate a new Autumn project under `parent_dir/name`, honouring `opts`.
///
/// Prints a human-readable creation summary to stdout. Callers that need clean
/// stdout (e.g. machine-readable output) should use [`generate_with_quiet`].
pub fn generate_with(name: &str, parent_dir: &Path, opts: GenerateOptions) -> Result<(), NewError> {
    generate_inner(name, parent_dir, opts, false)
}

/// Like [`generate_with`] but suppresses the stdout creation summary.
///
/// Used by tooling that emits machine-readable output on stdout (e.g. the
/// cold-start benchmark with `--json`), where the scaffold summary would
/// otherwise corrupt the output stream.
pub fn generate_with_quiet(
    name: &str,
    parent_dir: &Path,
    opts: GenerateOptions,
) -> Result<(), NewError> {
    generate_inner(name, parent_dir, opts, true)
}

#[allow(clippy::too_many_lines)]
fn generate_inner(
    name: &str,
    parent_dir: &Path,
    opts: GenerateOptions,
    quiet: bool,
) -> Result<(), NewError> {
    validate_name(name)?;
    check_option_combination(opts)?;

    let project_dir = parent_dir.join(name);
    if project_dir.exists() {
        return Err(NewError::AlreadyExists(name.to_owned()));
    }

    let crate_name = name.replace('-', "_");
    let autumn_version = env!("CARGO_PKG_VERSION");
    let rust_version = option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0");

    fs::create_dir_all(project_dir.join("src"))?;
    // The JSON-first API flavor ships no CSS/JS assets, so it has no `static/`
    // tree; the fullstack scaffold seeds `static/css` + `static/js`.
    if !opts.with_api {
        fs::create_dir_all(project_dir.join("static/css"))?;
        fs::create_dir_all(project_dir.join("static/js"))?;
    }
    fs::create_dir_all(project_dir.join("migrations"))?;
    fs::create_dir_all(project_dir.join("tests"))?;
    fs::create_dir_all(project_dir.join("config/credentials"))?;
    fs::create_dir_all(project_dir.join(".github/workflows"))?;
    if opts.with_i18n {
        fs::create_dir_all(project_dir.join("i18n"))?;
    }

    let vars = TemplateVars {
        project_name: name,
        crate_name: &crate_name,
        autumn_version,
        rust_version,
    };
    let render = |template: &str| -> String { render_template(template, &vars) };

    let cargo_template = if opts.with_api {
        templates::CARGO_API_TOML
    } else {
        templates::CARGO_TOML
    };
    let cargo_toml = render_cargo_toml(
        opts,
        autumn_version,
        render(cargo_template),
        &render(templates::SEED_CARGO_TOML),
    );
    fs::write(project_dir.join("Cargo.toml"), cargo_toml)?;

    fs::write(
        project_dir.join("README.md"),
        render_readme(render(templates::README), opts, &vars),
    )?;

    // The dependency advisory policy the generated CI enforces (issue #1600).
    // Written here rather than through `framework_owned_files` because its
    // waiver list is the app author's to grow: a file the developer is *asked*
    // to edit would otherwise come back as a scaffold-reconciliation conflict
    // on every `autumn upgrade`, exactly like `Cargo.toml` would.
    fs::write(project_dir.join("deny.toml"), render(templates::DENY_TOML))?;

    let main_template = if opts.with_api {
        templates::MAIN_API_RS
    } else {
        templates::MAIN_RS
    };
    let mut main_rs = match (opts.with_api, opts.with_i18n) {
        (true, true) => inject_i18n_api(&render(main_template)),
        (false, true) => inject_i18n(&render(main_template)),
        (_, false) => render(main_template),
    };
    if opts.with_bundled_pg {
        // Managed-Postgres daemon: keep migrations, install the pool provider.
        main_rs = inject_managed_pg(&main_rs);
    } else if opts.with_daemon {
        // DB-free daemon: the `migrate` module is db-gated, so drop migrations.
        main_rs = strip_migrations(&main_rs);
    }
    fs::write(project_dir.join("src/main.rs"), main_rs)?;

    // Every framework-owned file comes from one renderer, shared with `autumn
    // upgrade`'s scaffold reconciliation (issue #1593). Writing them from a
    // second, parallel code path here is how the two would silently disagree —
    // and a byte of disagreement reads to the reconciler as a permanent
    // conflict in every project ever generated.
    let owned = framework_owned_files(&vars, opts);
    for (relative, contents) in &owned {
        fs::write(project_dir.join(relative), contents)?;
    }
    // Record what this release wrote, so a later `autumn upgrade` can tell a
    // template that moved from a file the developer edited (issue #1593). Best
    // effort by design: the manifest only ever *sharpens* a later upgrade, and
    // failing a scaffold over bookkeeping would be a worse trade than losing
    // conflict precision.
    let _ = crate::upgrade::scaffold::Manifest::for_files(autumn_version, opts, &owned)
        .save(&project_dir);
    fs::write(project_dir.join("migrations/.gitkeep"), "")?;

    // The API flavor serves no HTML, so it needs no vendored htmx/SSE JS or the
    // static asset manifest — skip the whole `static/` vendoring step.
    if !opts.with_api {
        scaffold_vendor_assets(&project_dir)?;
    }
    scaffold_credentials(&project_dir, name)?;
    fs::write(
        project_dir.join("tests/integration_test.rs"),
        render(templates::INTEGRATION_TEST),
    )?;

    write_optional_scaffold_files(&project_dir, name, opts, &render)?;

    if !quiet {
        print_scaffold_summary(name, opts);
    }

    Ok(())
}

/// Every framework-owned file `autumn new` writes outside the application's own
/// source, rendered for `opts`.
///
/// This is the single definition of "what the current release's scaffold looks
/// like". [`generate_inner`] writes these files, and `autumn upgrade`'s
/// scaffold reconciliation (issue #1593) compares an existing project against
/// exactly the same rendering — so the two cannot drift apart, which is the one
/// bug that would make the reconciler report a conflict in every project on
/// earth.
///
/// # What is *not* here
///
/// The set is an allowlist, not "everything `autumn new` writes", and two
/// exclusions are load bearing:
///
/// - **`src/**`.** Application source is out of bounds for the reconciler
///   (issue #1593); `src/main.rs` and the optional seed binary are the app's,
///   not the framework's, the moment the project exists. Enforced by the
///   assertion below, not just by convention.
/// - **Files the framework generates but does not own thereafter**:
///   `Cargo.toml` (the app's dependencies), `README.md` (the app's prose),
///   `tests/`, `migrations/`, `i18n/`, `config/credentials/` (secrets), and the
///   vendored `static/js/` assets, which `autumn assets` — not this — keeps
///   current.
///
/// Keys are project-relative and always `/`-separated, so they are equally
/// valid as `Path` joins and as the keys of the provenance manifest on every
/// host.
#[must_use]
pub fn framework_owned_files(
    vars: &TemplateVars<'_>,
    opts: GenerateOptions,
) -> BTreeMap<&'static str, String> {
    let render = |template: &str| -> String { render_template(template, vars) };

    let mut autumn_toml = if opts.with_i18n {
        let mut s = render(templates::AUTUMN_TOML);
        s.push_str("\n[i18n]\ndefault_locale = \"en\"\nsupported_locales = [\"en\"]\n");
        s
    } else {
        render(templates::AUTUMN_TOML)
    };
    if opts.with_daemon && !opts.with_bundled_pg {
        autumn_toml.push_str(
            "\n# Daemon starter: this app uses no database. Run it as a local\n\
             # daemon with `autumn serve --daemon` (no Postgres required).\n",
        );
    }
    if opts.with_bundled_pg {
        // The managed cluster is private to this daemon and has no URL outside
        // the provider, so `autumn migrate` can't reach it and `--release` runs
        // under the `prod` profile (where migrations are otherwise only logged).
        // Apply embedded migrations automatically so a fresh release data dir
        // doesn't come up with missing tables.
        autumn_toml.push_str(
            "\n# Managed local Postgres (`autumn serve --bundled-pg`): the cluster is\n\
             # owned by the daemon, so apply embedded migrations automatically even\n\
             # under the production profile (a fresh data dir would otherwise start\n\
             # with no tables).\n\
             [database]\n\
             auto_migrate_in_production = true\n",
        );
    }

    let dockerfile = if opts.with_api {
        // The `--api` Dockerfile carries i18n `COPY` anchors resolved by flag:
        // ship the `i18n/` sidecar into the image for `--with-i18n`, or strip
        // the anchors so a non-i18n build context (which has no `i18n/` dir)
        // still builds.
        inject_i18n_dockerfile_api(&render(templates::DOCKERFILE_API), opts.with_i18n)
    } else {
        // The fullstack `Dockerfile.tmpl` carries the same i18n `COPY` anchors:
        // ship the `i18n/` sidecar into the image for `--with-i18n`, or strip
        // the anchors so a non-i18n build context (which has no `i18n/` dir)
        // still builds.
        inject_i18n_dockerfile(&render(templates::DOCKERFILE), opts.with_i18n)
    };

    let build_rs = if opts.with_api {
        render(templates::BUILD_API_RS)
    } else {
        render(templates::BUILD_RS)
    };

    let ci_yml = if opts.with_api {
        strip_ci_tailwind_note(&render(templates::CI_WORKFLOW))
    } else {
        render(templates::CI_WORKFLOW)
    };

    let mut files = BTreeMap::new();
    files.insert("autumn.toml", autumn_toml);
    files.insert("Dockerfile", dockerfile);
    files.insert(".dockerignore", render(templates::DOCKERIGNORE));
    files.insert("build.rs", build_rs);
    files.insert(".gitignore", render(templates::GITIGNORE));
    files.insert(".env.example", render(templates::ENV_EXAMPLE));
    files.insert(".github/workflows/ci.yml", ci_yml);
    files.insert("rust-toolchain.toml", render(templates::RUST_TOOLCHAIN));
    files.insert("rustfmt.toml", render(templates::RUSTFMT));
    files.insert("clippy.toml", render(templates::CLIPPY));
    // The API flavor has no Tailwind/CSS pipeline, so it owns no CSS input and
    // no Tailwind config (there is no `static/css` directory either).
    if !opts.with_api {
        files.insert("tailwind.config.js", render(templates::TAILWIND_CONFIG));
        files.insert("static/css/input.css", render(templates::INPUT_CSS));
    }

    debug_assert!(
        files.keys().all(|path| !path.starts_with("src/")),
        "application source is out of bounds for scaffold reconciliation"
    );
    files
}

fn scaffold_vendor_assets(project_dir: &Path) -> Result<(), NewError> {
    let htmx_bytes = autumn_web::HTMX_JS;
    let htmx_version = autumn_web::HTMX_VERSION;
    let htmx_source =
        format!("https://cdn.jsdelivr.net/npm/htmx.org@{htmx_version}/dist/htmx.min.js");
    let htmx_file = "js/htmx.min.js";
    let integrity = crate::assets::compute_sri(htmx_bytes);

    fs::write(project_dir.join("static").join(htmx_file), htmx_bytes)?;

    let sse_bytes = autumn_web::HTMX_SSE_JS;
    let sse_source = "https://unpkg.com/htmx-ext-sse@2.2.2/sse.js".to_owned();
    let sse_file = "js/htmx-ext-sse.min.js";
    let sse_integrity = crate::assets::compute_sri(sse_bytes);

    fs::write(project_dir.join("static").join(sse_file), sse_bytes)?;

    let mut assets = std::collections::BTreeMap::new();
    assets.insert(
        "htmx".to_owned(),
        crate::assets::VendorAsset {
            version: htmx_version.to_owned(),
            source: htmx_source,
            file: htmx_file.to_owned(),
            integrity,
        },
    );
    assets.insert(
        "htmx-ext-sse".to_owned(),
        crate::assets::VendorAsset {
            version: "2.2.2".to_owned(),
            source: sse_source,
            file: sse_file.to_owned(),
            integrity: sse_integrity,
        },
    );
    let manifest = crate::assets::VendorManifest {
        version: "1".to_owned(),
        assets,
    };
    let manifest_path = project_dir.join("static").join(".autumn-assets.json");
    crate::assets::save_manifest(&manifest_path, &manifest)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    Ok(())
}

fn scaffold_credentials(project_dir: &Path, name: &str) -> Result<(), NewError> {
    let master_key = MasterKey::generate();
    let key_path = project_dir.join("config/master.key");

    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&key_path)?;
    f.write_all(master_key.to_hex().as_bytes())?;

    let template = format!(
        "# Encrypted credentials for '{name}'\n\
         # Run `autumn credentials edit` to update these values.\n\
         # Do NOT commit config/master.key to version control.\n\
         \n\
         # stripe_secret_key = \"sk_live_...\"\n\
         # sendgrid_api_key = \"SG...\"\n\
         # s3_access_key_id = \"AKIA...\"\n"
    );
    let ciphertext = encrypt(&master_key, template.as_bytes());
    fs::write(
        project_dir.join("config/credentials/development.toml.enc"),
        ciphertext,
    )?;

    Ok(())
}

fn print_scaffold_summary(name: &str, opts: GenerateOptions) {
    println!("  Created {name}/");
    println!("  Created {name}/Cargo.toml");
    println!("  Created {name}/README.md");
    println!("  Created {name}/autumn.toml");
    println!("  Created {name}/Dockerfile");
    println!("  Created {name}/.dockerignore");
    println!("  Created {name}/build.rs");
    println!("  Created {name}/src/main.rs");
    if opts.with_seed {
        println!("  Created {name}/src/bin/seed.rs");
    }
    // The JSON-first API flavor ships no CSS/Tailwind pipeline.
    if !opts.with_api {
        println!("  Created {name}/static/css/input.css");
        println!("  Created {name}/tailwind.config.js");
    }
    println!("  Created {name}/.gitignore");
    println!("  Created {name}/.env.example");
    println!("  Created {name}/rust-toolchain.toml");
    println!("  Created {name}/rustfmt.toml");
    println!("  Created {name}/clippy.toml");
    // Named with its purpose attached: when the advisory gate first fires, a
    // developer who does not know this file exists reaches for disabling the CI
    // step instead of recording a waiver here.
    println!("  Created {name}/deny.toml (dependency advisory policy — CI audits against it)");
    println!("  Created {name}/migrations/");
    println!("  Created {name}/tests/integration_test.rs");
    println!("  Created {name}/config/master.key (keep secret — never commit)");
    println!("  Created {name}/config/credentials/development.toml.enc");
    // Named with its purpose attached: it is the only generated file whose
    // value depends entirely on being committed, and the only one a developer
    // would otherwise be tempted to gitignore as machine bookkeeping.
    println!("  Created {name}/.autumn/scaffold.toml (commit it — `autumn upgrade` reads it)");
    if opts.with_i18n {
        println!("  Created {name}/i18n/en.ftl");
    }
    println!();
    println!("Get started:");
    println!("  cd {name}");
    println!("  cargo run");
    println!();
    println!("Your app will be available at http://localhost:3000");
    if opts.with_i18n {
        println!();
        println!("i18n: edit i18n/en.ftl, add more locales as i18n/<tag>.ftl,");
        println!("      and use the t!() macro in handlers — see docs/guide/i18n.md.");
    }
    if opts.with_seed {
        println!();
        println!("Seed your database:");
        println!("  autumn migrate && autumn seed");
    }
}

/// Replace `from` with `to`, asserting the anchor exists first.
///
/// The scaffold generators below patch the generated `main.rs` by string
/// replacement against anchors in `main.rs.tmpl`. If the template and an anchor
/// drift out of sync, a plain `.replace()` silently no-ops and produces a
/// broken scaffold (this exact class of bug has bitten the mailer wiring once
/// already). Asserting the anchor turns that into a loud, test-caught failure.
fn replace_anchor(src: &str, from: &str, to: &str) -> String {
    assert!(
        src.contains(from),
        "scaffold template anchor not found: {from:?} — src/templates/main.rs.tmpl and the \
         inject_* helpers in new.rs have drifted out of sync"
    );
    src.replace(from, to)
}

/// Enable i18n in a generated `main.rs`: call `.i18n_auto()` and embed the
/// `i18n/` locale bundles alongside the static assets for single-binary deploys.
fn inject_i18n(main_rs: &str) -> String {
    let with_locale = replace_anchor(
        main_rs,
        "        .routes(routes![index, hello, hello_name, consent_accept, consent_reject, consent_manage])",
        "        .i18n_auto()\n        .routes(routes![index, hello, hello_name, consent_accept, consent_reject, consent_manage])",
    );
    let with_static = replace_anchor(
        &with_locale,
        "static EMBEDDED_STATIC: autumn_web::include_dir::Dir = autumn_web::embed_static!();",
        "static EMBEDDED_STATIC: autumn_web::include_dir::Dir = autumn_web::embed_static!();\n\
         #[cfg(feature = \"embed-assets\")]\n\
         static EMBEDDED_LOCALES: autumn_web::include_dir::Dir = autumn_web::embed_locales!();",
    );
    replace_anchor(
        &with_static,
        "    let app = app.embedded_static(&EMBEDDED_STATIC);\n",
        "    let app = app.embedded_static(&EMBEDDED_STATIC);\n\
         \x20   #[cfg(feature = \"embed-assets\")]\n\
         \x20   let app = app.embedded_locales(&EMBEDDED_LOCALES);\n",
    )
}

/// i18n variant of [`inject_i18n`] for the JSON-first API scaffold's `main.rs`,
/// which has no HTML/static asset layer. Enables locale auto-detection with
/// `.i18n_auto()` and embeds the `i18n/` locale bundles into the binary (behind
/// the `embed-assets` feature) for single-binary deploys.
fn inject_i18n_api(main_rs: &str) -> String {
    let with_locale_call = replace_anchor(
        main_rs,
        "        .routes(routes![index, hello_name])",
        "        .i18n_auto()\n        .routes(routes![index, hello_name])",
    );
    let with_locales_static = replace_anchor(
        &with_locale_call,
        "const MIGRATIONS: EmbeddedMigrations = embed_migrations!();\n",
        "const MIGRATIONS: EmbeddedMigrations = embed_migrations!();\n\n\
         #[cfg(feature = \"embed-assets\")]\n\
         static EMBEDDED_LOCALES: autumn_web::include_dir::Dir = autumn_web::embed_locales!();\n",
    );
    replace_anchor(
        &with_locales_static,
        "        .migrations(MIGRATIONS);\n\n    app\n",
        "        .migrations(MIGRATIONS);\n\n\
         \x20   #[cfg(feature = \"embed-assets\")]\n\
         \x20   let app = app.embedded_locales(&EMBEDDED_LOCALES);\n\n    app\n",
    )
}

/// Anchor: the builder-stage i18n `COPY` insertion point in
/// `Dockerfile.api.tmpl` (an otherwise-inert comment line). Replaced with a
/// `COPY i18n ./i18n` line for `--api --with-i18n`, or stripped entirely
/// otherwise so a non-i18n project's build context has no missing `i18n/` dir.
const DOCKERFILE_API_I18N_BUILDER_ANCHOR: &str = "# __AUTUMN_I18N_BUILDER_COPY__\n";
/// Anchor: the runtime-stage i18n `COPY` insertion point in
/// `Dockerfile.api.tmpl`. Replaced with a `COPY --from=builder /app/i18n
/// /app/i18n` line for `--api --with-i18n`, or stripped otherwise.
const DOCKERFILE_API_I18N_RUNTIME_ANCHOR: &str = "# __AUTUMN_I18N_RUNTIME_COPY__\n";

/// Resolve the two i18n `COPY` anchors in the rendered `--api` Dockerfile.
///
/// The `--api` scaffold's `main.rs` calls `.i18n_auto()` when `--with-i18n`,
/// which loads `i18n/en.ftl` from disk at startup and panics if it is missing.
/// The API image must therefore ship the `i18n/` sidecar into both the builder
/// (so `cargo build` sees it for any embed) and the runtime stage (so the
/// running binary can read it). The `COPY` lines are gated on `with_i18n`: an
/// unconditional `COPY i18n ./i18n` would break `docker build` for non-i18n
/// projects, whose build context has no `i18n/` directory. When `with_i18n` is
/// false the anchors are stripped, leaving the Dockerfile byte-for-byte as it
/// was before this wiring (no leftover anchor markers).
fn inject_i18n_dockerfile_api(dockerfile: &str, with_i18n: bool) -> String {
    if with_i18n {
        let with_builder = replace_anchor(
            dockerfile,
            DOCKERFILE_API_I18N_BUILDER_ANCHOR,
            "COPY i18n ./i18n\n",
        );
        replace_anchor(
            &with_builder,
            DOCKERFILE_API_I18N_RUNTIME_ANCHOR,
            "COPY --from=builder /app/i18n /app/i18n\n",
        )
    } else {
        let no_builder = replace_anchor(dockerfile, DOCKERFILE_API_I18N_BUILDER_ANCHOR, "");
        replace_anchor(&no_builder, DOCKERFILE_API_I18N_RUNTIME_ANCHOR, "")
    }
}

/// Anchor: the builder-stage i18n `COPY` insertion point in the fullstack
/// `Dockerfile.tmpl` (an otherwise-inert comment line). Replaced with a
/// `COPY i18n ./i18n` line for `--with-i18n`, or stripped entirely otherwise so
/// a non-i18n project's build context has no missing `i18n/` dir.
const DOCKERFILE_I18N_BUILDER_ANCHOR: &str = "# __AUTUMN_I18N_BUILDER_COPY__\n";
/// Anchor: the runtime-stage i18n `COPY` insertion point in the fullstack
/// `Dockerfile.tmpl`. Replaced with a `COPY --from=builder /app/i18n /app/i18n`
/// line for `--with-i18n`, or stripped otherwise.
const DOCKERFILE_I18N_RUNTIME_ANCHOR: &str = "# __AUTUMN_I18N_RUNTIME_COPY__\n";

/// Resolve the two i18n `COPY` anchors in the rendered fullstack Dockerfile.
///
/// The default (fullstack) scaffold's `main.rs` calls `.i18n_auto()` when
/// `--with-i18n`, which loads `i18n/en.ftl` from disk at startup and panics if
/// it is missing. The image must therefore ship the `i18n/` sidecar into both
/// the builder (so `cargo build` sees it for any embed) and the runtime stage
/// (so the running binary can read it). The `COPY` lines are gated on
/// `with_i18n`: an unconditional `COPY i18n ./i18n` would break `docker build`
/// for non-i18n projects, whose build context has no `i18n/` directory. When
/// `with_i18n` is false the anchors are stripped, leaving the Dockerfile
/// byte-for-byte as it was before this wiring (no leftover anchor markers).
/// Mirrors [`inject_i18n_dockerfile_api`] for the `--api` scaffold.
fn inject_i18n_dockerfile(dockerfile: &str, with_i18n: bool) -> String {
    if with_i18n {
        let with_builder = replace_anchor(
            dockerfile,
            DOCKERFILE_I18N_BUILDER_ANCHOR,
            "COPY i18n ./i18n\n",
        );
        replace_anchor(
            &with_builder,
            DOCKERFILE_I18N_RUNTIME_ANCHOR,
            "COPY --from=builder /app/i18n /app/i18n\n",
        )
    } else {
        let no_builder = replace_anchor(dockerfile, DOCKERFILE_I18N_BUILDER_ANCHOR, "");
        replace_anchor(&no_builder, DOCKERFILE_I18N_RUNTIME_ANCHOR, "")
    }
}

/// Anchor: the Tailwind CI extension note in `ci.yml.tmpl`. The JSON-first API
/// scaffold has no Tailwind/CSS step, so this note is stripped for `--api` (it
/// is also the only `tailwind` mention in the generated tree).
const CI_TAILWIND_NOTE: &str = "#   - Tailwind: add `autumn setup --tailwind` and run the downloaded binary\n\
     #     before `cargo build` to compile CSS in CI.\n";

/// Remove the Tailwind CI extension note from a rendered `ci.yml` for the API
/// scaffold (which ships no CSS pipeline).
fn strip_ci_tailwind_note(ci_yml: &str) -> String {
    replace_anchor(ci_yml, CI_TAILWIND_NOTE, "")
}

/// Inject a managed-Postgres pool provider plus a shutdown hook into a
/// generated `main.rs` so the bundled cluster is supervised by the daemon.
fn inject_managed_pg(main_rs: &str) -> String {
    replace_anchor(
        main_rs,
        "    let app = autumn_web::app()\n",
        "    let pg = autumn_web::managed_pg::ManagedPostgresPoolProvider::new();\n\
         \x20   let pg_shutdown = pg.clone();\n\
         \x20   let app = autumn_web::app()\n\
         \x20       .with_pool_provider(pg)\n\
         \x20       .on_shutdown(move || {\n\
         \x20           let pg = pg_shutdown.clone();\n\
         \x20           async move {\n\
         \x20               pg.stop().await;\n\
         \x20           }\n\
         \x20       })\n",
    )
}

/// Remove the diesel-migrations wiring from a generated `main.rs` so a DB-free
/// app compiles without the db-gated `migrate` module.
fn strip_migrations(main_rs: &str) -> String {
    let no_use = replace_anchor(
        main_rs,
        "use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};\n",
        "",
    );
    let no_const = replace_anchor(
        &no_use,
        "const MIGRATIONS: EmbeddedMigrations = embed_migrations!();\n\n",
        "",
    );
    replace_anchor(&no_const, "\n        .migrations(MIGRATIONS)", "")
}

/// Default `autumn-web` features minus `db` — the DB-free daemon feature set.
const DAEMON_NO_DB_FEATURES: &[&str] = &[
    "maud",
    "htmx",
    "tailwind",
    "cache-moka",
    "http-client",
    "reporting",
];

/// Default `autumn-web` features minus the HTML view stack (`maud`/`htmx`/
/// `tailwind`) — the JSON-first API (`--api`) feature set. Keeps `db` so
/// migrations and database features still work.
const API_FEATURES: &[&str] = &["db", "cache-moka", "http-client", "reporting", "flash"];

/// Anchor: first line of the DB-specific prerequisites/steps block in
/// `README.md.tmpl` (the `- **A reachable Postgres**` bullet). Everything from
/// here up to [`README_TAIL_ANCHOR`] is the default DB-first golden path and is
/// swapped out for daemon / bundled-Postgres app shapes.
const README_DB_BODY_ANCHOR: &str = "- **A reachable Postgres**";
/// Anchor: start of the shared CLI-reference tail in `README.md.tmpl`.
const README_TAIL_ANCHOR: &str = "## CLI reference";
/// The CLI-reference row documenting `autumn migrate`, stripped from the daemon
/// / bundled-Postgres READMEs (a DB-free daemon has no migrations to apply, and
/// a bundled-Postgres daemon applies them automatically inside the process).
const README_MIGRATE_ROW: &str = "| `autumn migrate` | Apply pending database migrations. |\n";
/// The CLI-reference row documenting `autumn generate scaffold`, stripped from
/// the **DB-free daemon** README only. `generate scaffold` emits Diesel
/// models/repositories/migrations and re-enables `db-diesel2-postgres`, so the
/// code it produces cannot compile in a `--daemon` scaffold (which disables the
/// `db` feature). The bundled-Postgres shape keeps the `db` feature, so the verb
/// stays valid there and this row is retained.
const README_SCAFFOLD_ROW: &str = "| `autumn generate scaffold <Name> field:Type …` | Scaffold a CRUD resource — model, migration, routes, and views. |\n";
/// The project-layout row documenting the `migrations/` directory, stripped from
/// the DB-free daemon README (that scaffold has no migrations directory).
const README_MIGRATIONS_LAYOUT_ROW: &str =
    "| `migrations/` | Diesel migrations — one directory per migration. |\n";
/// Anchor: the Tailwind binary prerequisite bullet in `README.md.tmpl`. The
/// JSON-first API scaffold has no CSS pipeline, so this bullet is removed for
/// `--api` (it is the only `tailwind` mention in the generated README).
const README_TAILWIND_PREREQ: &str =
    "- **The Tailwind binary** (downloaded by `autumn setup`):\n  ```sh\n  autumn setup\n  ```\n";
/// Anchor: the `static/` project-layout row in `README.md.tmpl`. The API
/// scaffold ships no `static/` directory, so the row is removed for `--api`.
const README_STATIC_LAYOUT_ROW: &str = "| `static/` | Static assets served under `/static/`. |\n";

/// Render the project README, tailoring the golden path to the app shape and
/// appending flag-specific sections.
///
/// The template body is the default **DB-first** golden path (configure
/// `[database]`, `autumn migrate`, `autumn dev`). For the daemon app shapes the
/// DB-bootstrap block is swapped for prose that matches what the scaffold
/// actually produces:
///
/// * `--daemon` builds with **no** database (the `db` feature is off and
///   migrations are stripped), so it must not tell users to install `libpq`,
///   configure Postgres, or run `autumn migrate` — it runs via `autumn serve`.
/// * `--bundled-pg` embeds and manages its own Postgres, so there is no external
///   server to configure and migrations apply automatically — it runs via
///   `autumn serve --bundled-pg`.
///
/// `render_template` only substitutes the four fixed `{{…}}` tokens, so the
/// flag-aware quickstart notes are built here in Rust rather than gated inside
/// the template. Text produced here must not introduce any new `{{…}}` token —
/// `no_unsubstituted_placeholders` walks the generated tree and would flag it
/// (crate/project names are interpolated from `vars`, not left as tokens).
fn render_readme(rendered: String, opts: GenerateOptions, vars: &TemplateVars<'_>) -> String {
    // The JSON-first API scaffold shares the DB-first golden path (it keeps
    // `db`/migrations) but has no Tailwind/CSS pipeline and no `static/` tree,
    // so strip the Tailwind prerequisite and the `static/` layout row.
    if opts.with_api {
        let mut readme = rendered.replace(README_TAILWIND_PREREQ, "");
        readme = readme.replace(README_STATIC_LAYOUT_ROW, "");
        append_optional_readme_sections(&mut readme, opts);
        return readme;
    }
    // `--bundled-pg` implies `with_daemon`, so test it first.
    let mut readme = if opts.with_bundled_pg {
        // Bundled Postgres keeps the `db` feature, so `generate scaffold` and the
        // `migrations/` layout stay valid — only the hand-run `migrate` row goes.
        rewrite_readme_body(&rendered, &bundled_pg_readme_body(vars), false)
    } else if opts.with_daemon {
        // Pure DB-free daemon: also strip the DB-coupled CLI/layout rows.
        rewrite_readme_body(&rendered, &daemon_readme_body(vars), true)
    } else {
        rendered
    };
    append_optional_readme_sections(&mut readme, opts);
    readme
}

/// Append the `--with-i18n` and `--with-seed` README sections when those flags
/// are set. Shared by every app shape (fullstack, daemon, and `--api`) so the
/// flag-specific guidance is identical regardless of the golden-path body.
fn append_optional_readme_sections(readme: &mut String, opts: GenerateOptions) {
    if opts.with_i18n {
        readme.push_str(
            "\n## Internationalization (i18n)\n\
             \n\
             This project was generated with `--with-i18n`. Fluent translations\n\
             live in `i18n/en.ftl`; add more locales by dropping additional files\n\
             like `i18n/es.ftl`. Reach translations from your handlers by taking a\n\
             `Locale` extractor and passing it to the `t!()` macro — e.g.\n\
             `t!(locale, \"welcome.title\")` — and use the `[i18n]` block in\n\
             `autumn.toml` to set the default and supported locales. See\n\
             `docs/guide/i18n.md` for the full guide.\n",
        );
    }
    if opts.with_seed {
        readme.push_str(
            "\n## Seed data\n\
             \n\
             This project was generated with `--with-seed`. Edit `src/bin/seed.rs`\n\
             to insert representative data, then seed the database (after applying\n\
             migrations):\n\
             \n\
             ```sh\n\
             autumn migrate && autumn seed\n\
             ```\n",
        );
    }
}

/// Replace the default DB-first golden-path block (from
/// [`README_DB_BODY_ANCHOR`] up to [`README_TAIL_ANCHOR`]) with `new_body`, then
/// drop the CLI-reference `autumn migrate` row — neither daemon shape runs it by
/// hand. `new_body` supplies the app-shape-specific prerequisites and run steps.
///
/// When `db_free` is set (the pure `--daemon` shape, which disables the `db`
/// feature), the DB-coupled CLI/layout rows are stripped too: `generate scaffold`
/// (it emits Diesel code that needs the disabled `db` feature) and the
/// `migrations/` layout row (no migrations directory exists). The
/// bundled-Postgres shape keeps `db`, so it passes `db_free = false` and retains
/// those rows.
fn rewrite_readme_body(rendered: &str, new_body: &str, db_free: bool) -> String {
    let start = rendered.find(README_DB_BODY_ANCHOR).unwrap_or_else(|| {
        panic!(
            "README.md.tmpl anchor not found: {README_DB_BODY_ANCHOR:?} — the template and \
             render_readme have drifted out of sync"
        )
    });
    let tail = rendered.find(README_TAIL_ANCHOR).unwrap_or_else(|| {
        panic!(
            "README.md.tmpl anchor not found: {README_TAIL_ANCHOR:?} — the template and \
             render_readme have drifted out of sync"
        )
    });
    let spliced = format!("{}{}{}", &rendered[..start], new_body, &rendered[tail..]);
    // The daemon shapes have no hand-run migration step; drop the reference row
    // so the CLI table stays consistent with the golden path above it.
    let mut out = spliced.replace(README_MIGRATE_ROW, "");
    if db_free {
        // The DB-free daemon disables the `db` feature, so `generate scaffold`
        // would emit code that cannot compile, and there is no `migrations/`
        // directory. Drop both rows so the reference matches what actually works.
        out = out.replace(README_SCAFFOLD_ROW, "");
        out = out.replace(README_MIGRATIONS_LAYOUT_ROW, "");
    }
    out
}

/// DB-free daemon (`--daemon`) golden-path body: no Postgres, no `libpq`, no
/// migrations — the app runs via `autumn serve`.
fn daemon_readme_body(vars: &TemplateVars<'_>) -> String {
    format!(
        "\n### 2. Run the server\n\
         \n\
         This project was generated with `--daemon`: it builds with **no** database — the\n\
         `db` feature is off and there are no migrations — so there is no Postgres to\n\
         install, no database client library to link, and nothing in `autumn.toml` to\n\
         configure. Run it locally with:\n\
         \n\
         ```sh\n\
         autumn dev\n\
         ```\n\
         \n\
         Then visit **http://localhost:3000** — you should get a `200` with\n\
         \"Welcome to {project}!\". Health endpoints are auto-mounted at `/health`\n\
         and `/actuator/health`.\n\
         \n\
         For production, `autumn serve --daemon` supervises it in the background as a\n\
         managed daemon (`autumn serve status`, `autumn serve stop`, and `autumn serve\n\
         restart` manage the process). Unlike `autumn dev`, the daemon binds a private\n\
         **Unix domain socket**, not a TCP port — so it is *not* reachable at\n\
         `http://localhost:3000`. Run `autumn serve status` to print its socket address;\n\
         see `docs/guide/daemon.md` for the socket transport and lifecycle details.\n\
         \n\
         Need a database later? Re-enable `autumn-web`'s default features (turn the\n\
         `db` feature back on) in `Cargo.toml` to unlock the database-backed\n\
         workflow — migrations and CRUD scaffolding — that the default (non-daemon)\n\
         starter documents.\n\
         \n\
         ",
        project = vars.project_name,
    )
}

/// Bundled/managed-Postgres daemon (`--bundled-pg`) golden-path body: the app
/// owns its Postgres and auto-applies migrations, so there is no external server
/// to configure — it runs via `autumn serve --bundled-pg`. It keeps the `db`
/// feature, so `libpq` is still linked at build time.
fn bundled_pg_readme_body(vars: &TemplateVars<'_>) -> String {
    format!(
        "- **The PostgreSQL client library (`libpq`).** This starter keeps the `db`\n\
         \x20 feature, so the binary links `libpq` at build time — install it with your\n\
         \x20 package manager (e.g. `libpq-dev` on Debian/Ubuntu, `libpq` via Homebrew on\n\
         \x20 macOS).\n\
         \n\
         ### 2. Run the server\n\
         \n\
         This project was generated with `--bundled-pg`: it embeds and manages its own\n\
         Postgres. You do **not** install a server, edit the `[database]` block, or run\n\
         migrations by hand — the app provisions the cluster on startup and applies the\n\
         embedded migrations automatically (see the `[database]` note in `autumn.toml`).\n\
         Run it locally with:\n\
         \n\
         ```sh\n\
         autumn dev\n\
         ```\n\
         \n\
         Then visit **http://localhost:3000** — you should get a `200` with\n\
         \"Welcome to {project}!\". Health endpoints are auto-mounted at `/health`\n\
         and `/actuator/health`.\n\
         \n\
         For production, `autumn serve --bundled-pg` supervises the app and its managed\n\
         Postgres in the background as a daemon (`autumn serve status`, `autumn serve stop`,\n\
         and `autumn serve restart` manage the process). Unlike `autumn dev`, the daemon\n\
         binds a private **Unix domain socket**, not a TCP port — so it is *not* reachable\n\
         at `http://localhost:3000`. Run `autumn serve status` to print its socket address;\n\
         see `docs/guide/daemon.md` for the socket transport, lifecycle, and bundled-Postgres\n\
         details.\n\
         \n\
         ",
        project = vars.project_name,
    )
}

fn render_cargo_toml(
    opts: GenerateOptions,
    autumn_version: &str,
    mut cargo_toml: String,
    seed_bin_toml: &str,
) -> String {
    use std::fmt::Write;

    // JSON-first API starter: drop the HTML view stack (maud/htmx/tailwind) by
    // switching off default features and pinning the lean API feature set. The
    // API `Cargo.toml` template ships a plain `autumn-web = "…"` dep (no `maud`
    // line), so rewrite it to the explicit `default-features = false` table.
    if opts.with_api {
        let plain_dep = format!(r#"autumn-web = "{autumn_version}""#);
        let mut features: Vec<&str> = API_FEATURES.to_vec();
        if opts.with_i18n {
            features.push("i18n");
        }
        if opts.with_seed {
            features.push("seed");
        }
        let features_str = features
            .iter()
            .map(|f| format!(r#""{f}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let dep = format!(
            r#"autumn-web = {{ version = "{autumn_version}", default-features = false, features = [{features_str}] }}"#
        );
        cargo_toml = cargo_toml.replace(&plain_dep, &dep);
        if opts.with_seed {
            cargo_toml.push('\n');
            cargo_toml.push_str(seed_bin_toml);
        }
        return cargo_toml;
    }

    // DB-free daemon starter: switch off default features (drops `db`) so the
    // binary links no Postgres, and remove the diesel migrations dependency.
    if opts.with_daemon && !opts.with_bundled_pg {
        let plain_dep = format!(r#"autumn-web = "{autumn_version}""#);
        let mut features: Vec<&str> = DAEMON_NO_DB_FEATURES.to_vec();
        if opts.with_i18n {
            features.push("i18n");
        }
        let features_str = features
            .iter()
            .map(|f| format!(r#""{f}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let dep = format!(
            r#"autumn-web = {{ version = "{autumn_version}", default-features = false, features = [{features_str}] }}"#
        );
        cargo_toml = cargo_toml.replace(&plain_dep, &dep);
        cargo_toml = cargo_toml.replace("diesel_migrations = \"2\"\n", "");
        return cargo_toml;
    }

    let mut features = Vec::new();
    if opts.with_bundled_pg {
        // Single-binary mode: embed Postgres in the executable.
        features.push("managed-pg-bundled");
    }
    if opts.with_i18n {
        features.push("i18n");
    }
    if opts.with_seed {
        features.push("seed");
    }
    if !features.is_empty() {
        let plain_dep = format!(r#"autumn-web = "{autumn_version}""#);
        // ⚡ Bolt optimization: Pre-allocate capacity for comma-separated feature strings
        let mut features_str = String::with_capacity(features.len() * 10);
        for (i, feature) in features.iter().enumerate() {
            if i > 0 {
                features_str.push_str(", ");
            }
            write!(features_str, r#""{feature}""#).unwrap();
        }
        let feature_dep = format!(
            r#"autumn-web = {{ version = "{autumn_version}", features = [{features_str}] }}"#
        );
        cargo_toml = cargo_toml.replace(&plain_dep, &feature_dep);
    }
    if opts.with_seed {
        cargo_toml.push('\n');
        cargo_toml.push_str(seed_bin_toml);
    }
    cargo_toml
}

fn write_optional_scaffold_files(
    project_dir: &Path,
    name: &str,
    opts: GenerateOptions,
    render: &impl Fn(&str) -> String,
) -> Result<(), NewError> {
    if opts.with_i18n {
        fs::write(
            project_dir.join("i18n/en.ftl"),
            "# Default-locale translations for {{project_name}}.\n\
             # Add more locales by dropping additional files like `i18n/es.ftl`.\n\
             welcome.title = Welcome to Autumn!\n\
             welcome.greeting = Hello, { $name }!\n"
                .replace("{{project_name}}", name),
        )?;
    }

    if opts.with_seed {
        fs::create_dir_all(project_dir.join("src/bin"))?;
        fs::write(
            project_dir.join("src/bin/seed.rs"),
            render(templates::SEED_RS),
        )?;
    }

    Ok(())
}

/// Rust keywords that would be invalid crate names.
const KEYWORDS: &[&str] = &[
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Validate that a name is a valid Rust package name.
pub fn validate_name(name: &str) -> Result<(), NewError> {
    if name.is_empty() {
        return Err(NewError::InvalidName(
            name.to_owned(),
            "name cannot be empty".into(),
        ));
    }

    let first = name.chars().next().expect("checked non-empty");
    if !first.is_ascii_alphabetic() {
        return Err(NewError::InvalidName(
            name.to_owned(),
            "must start with a letter".into(),
        ));
    }

    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_')
    {
        return Err(NewError::InvalidName(
            name.to_owned(),
            format!("contains invalid character '{bad}'"),
        ));
    }

    if KEYWORDS.contains(&name) {
        return Err(NewError::InvalidName(
            name.to_owned(),
            format!("'{name}' is a Rust keyword"),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn valid_name_simple() {
        assert!(validate_name("myapp").is_ok());
    }

    #[test]
    fn valid_name_with_hyphens() {
        assert!(validate_name("my-app").is_ok());
    }

    #[test]
    fn valid_name_with_underscores() {
        assert!(validate_name("my_app").is_ok());
    }

    #[test]
    fn valid_name_with_digits() {
        assert!(validate_name("app2").is_ok());
    }

    #[test]
    fn empty_name_rejected() {
        let err = validate_name("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn starts_with_digit_rejected() {
        let err = validate_name("3app").unwrap_err();
        assert!(err.to_string().contains("start with a letter"));
    }

    #[test]
    fn starts_with_hyphen_rejected() {
        let err = validate_name("-app").unwrap_err();
        assert!(err.to_string().contains("start with a letter"));
    }

    #[test]
    fn special_chars_rejected() {
        let err = validate_name("my app!").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn keyword_rejected() {
        let err = validate_name("fn").unwrap_err();
        assert!(err.to_string().contains("keyword"));
    }

    #[test]
    fn keyword_async_rejected() {
        let err = validate_name("async").unwrap_err();
        assert!(err.to_string().contains("keyword"));
    }

    #[test]
    fn generates_all_expected_files() {
        let tmp = TempDir::new().unwrap();
        generate("test-app", tmp.path()).unwrap();

        let p = tmp.path().join("test-app");
        assert!(p.join("Cargo.toml").is_file());
        assert!(p.join("README.md").is_file());
        assert!(p.join("src/main.rs").is_file());
        assert!(p.join("autumn.toml").is_file());
        assert!(p.join("Dockerfile").is_file());
        assert!(p.join(".dockerignore").is_file());
        let dockerignore = std::fs::read_to_string(p.join(".dockerignore")).unwrap();
        assert!(
            dockerignore.contains("/config/master.key")
                || dockerignore.contains("config/master.key"),
            ".dockerignore must exclude config/master.key: {dockerignore}"
        );
        assert!(p.join("build.rs").is_file());
        assert!(p.join(".gitignore").is_file());
        assert!(p.join(".env.example").is_file());
        assert!(p.join("static/css/input.css").is_file());
        assert!(p.join("tailwind.config.js").is_file());
        assert!(p.join("migrations/.gitkeep").is_file());
        assert!(!p.join("src/lib.rs").exists());
        assert!(!p.join("src/client.rs").exists());
    }

    // `autumn new` must generate a tests/ directory with a smoke test.
    #[test]
    fn generates_tests_directory_with_smoke_test() {
        let tmp = TempDir::new().unwrap();
        generate("smoke-test-app", tmp.path()).unwrap();
        let p = tmp.path().join("smoke-test-app");
        assert!(
            p.join("tests").is_dir(),
            "`autumn new` should create a tests/ directory"
        );
        assert!(
            p.join("tests/integration_test.rs").is_file(),
            "`autumn new` should generate tests/integration_test.rs"
        );
    }

    // `autumn new` must scaffold a working cookie-consent banner (issue
    // #1214): a policy-version constant the app owner can bump to re-prompt,
    // the auto-injecting middleware wired into the app, and the accept/reject
    // routes it posts to.
    #[test]
    fn generates_consent_banner_wiring_in_main_rs() {
        let tmp = TempDir::new().unwrap();
        generate("consent-app", tmp.path()).unwrap();
        let main_rs = fs::read_to_string(tmp.path().join("consent-app/src/main.rs")).unwrap();
        assert!(
            main_rs.contains("CONSENT_POLICY_VERSION"),
            "generated main.rs must declare a bump-to-reprompt policy version constant: {main_rs}"
        );
        assert!(
            main_rs.contains("autumn_web::consent::inject_consent_banner"),
            "generated main.rs must wire the consent-banner middleware: {main_rs}"
        );
        assert!(
            main_rs.contains("consent_accept") && main_rs.contains("consent_reject"),
            "generated main.rs must define consent_accept/consent_reject routes: {main_rs}"
        );
        assert!(
            main_rs.contains("\"/consent/accept\"") && main_rs.contains("\"/consent/reject\""),
            "generated main.rs must mount the accept/reject routes at their documented paths: {main_rs}"
        );
        assert!(
            main_rs.contains(
                "routes![index, hello, hello_name, consent_accept, consent_reject, consent_manage]"
            ),
            "the new consent routes must be registered alongside the existing routes: {main_rs}"
        );
        assert!(
            main_rs.contains("\"/consent/manage\"")
                && main_rs.contains("autumn_web::consent::consent_banner_markup"),
            "generated main.rs must scaffold a preferences route reusing the consent-banner \
             widget (GDPR Art. 7(3): withdrawing consent must be as easy as giving it): {main_rs}"
        );
        assert!(
            main_rs.contains("href=\"/consent/manage\""),
            "the shared layout's footer must link to the withdrawal route so it's \
             reachable from every page: {main_rs}"
        );
        assert!(
            main_rs.contains("autumn_web::consent::DEFAULT_CSRF_COOKIE_NAME"),
            "the middleware wiring must pass the CSRF cookie name explicitly: {main_rs}"
        );
        assert!(
            main_rs.contains("autumn_web::consent::DEFAULT_CSRF_FORM_FIELD"),
            "the middleware wiring must pass the CSRF form-field name explicitly: {main_rs}"
        );
    }

    // `/consent/manage` must stay a side-effect-free `GET`: it renders the
    // consent-banner widget so the visitor can make a new choice, but the
    // actual state change goes through the existing CSRF-protected
    // `POST /consent/accept` / `POST /consent/reject` handlers. If the GET
    // handler itself mutated the consent cookie (e.g. by calling
    // `expire_consent_cookie` directly), a same-origin prefetcher, browser
    // extension, or cross-site top-level navigation following the footer
    // link could silently reset a visitor's consent, since `GET` is
    // CSRF-exempt by definition.
    #[test]
    fn consent_manage_route_does_not_mutate_state_on_get() {
        let tmp = TempDir::new().unwrap();
        generate("consent-manage-app", tmp.path()).unwrap();
        let main_rs =
            fs::read_to_string(tmp.path().join("consent-manage-app/src/main.rs")).unwrap();
        let start = main_rs
            .find("async fn consent_manage")
            .expect("consent_manage handler must exist");
        let body = &main_rs[start..];
        let end = body[1..]
            .find("\n#[")
            .map_or(body.len(), |offset| offset + 1);
        let handler_body = &body[..end];
        assert!(
            !handler_body.contains("expire_consent_cookie") && !handler_body.contains("SET_COOKIE"),
            "the GET /consent/manage handler must not itself set or expire any cookie: {handler_body}"
        );
    }

    // The JSON-first `--api` flavor has no HTML/layout to show a banner in —
    // it must not scaffold the consent-banner wiring at all.
    #[test]
    fn api_flavor_does_not_scaffold_consent_banner() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "consent-api-app",
            tmp.path(),
            GenerateOptions {
                with_api: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let main_rs = fs::read_to_string(tmp.path().join("consent-api-app/src/main.rs")).unwrap();
        assert!(
            !main_rs.contains("inject_consent_banner"),
            "the --api flavor ships no HTML layout, so it must not scaffold the banner: {main_rs}"
        );
    }

    // The generated `--with-i18n` main.rs must still compile-shape correctly:
    // the i18n injection anchors must stay in sync with the new routes! list
    // (see `inject_i18n`'s anchor constant).
    #[test]
    fn with_i18n_still_wires_consent_routes() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "consent-i18n-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let main_rs = fs::read_to_string(tmp.path().join("consent-i18n-app/src/main.rs")).unwrap();
        assert!(
            main_rs.contains(
                "routes![index, hello, hello_name, consent_accept, consent_reject, consent_manage]"
            ),
            "i18n injection must not drop the consent routes from the routes! list: {main_rs}"
        );
        assert!(main_rs.contains("i18n_auto"));
    }

    // The generated Cargo.toml must have [dev-dependencies] with tokio
    // so that #[tokio::test] compiles without the user adding anything.
    #[test]
    fn generated_cargo_toml_has_dev_deps_for_testing() {
        let tmp = TempDir::new().unwrap();
        generate("dev-dep-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("dev-dep-app/Cargo.toml")).unwrap();
        assert!(
            content.contains("[dev-dependencies]"),
            "generated Cargo.toml must have [dev-dependencies]"
        );
        assert!(
            content.contains("tokio"),
            "generated Cargo.toml must include tokio in dev-dependencies for #[tokio::test]"
        );
    }

    #[test]
    fn cargo_toml_has_project_name() {
        let tmp = TempDir::new().unwrap();
        generate("my-cool-app", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("my-cool-app/Cargo.toml")).unwrap();
        assert!(content.contains(r#"name = "my-cool-app""#));
        assert!(content.contains("autumn-web = "));
    }

    #[test]
    fn cargo_toml_has_autumn_version() {
        let tmp = TempDir::new().unwrap();
        generate("ver-check", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("ver-check/Cargo.toml")).unwrap();
        let expected = format!(r#"autumn-web = "{}""#, env!("CARGO_PKG_VERSION"));
        assert!(content.contains(&expected));
    }

    #[test]
    fn main_rs_has_sample_routes() {
        let tmp = TempDir::new().unwrap();
        generate("route-check", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("route-check/src/main.rs")).unwrap();
        assert!(content.contains(r#"#[get("/")]"#));
        assert!(content.contains(r#"#[get("/hello")]"#));
        assert!(content.contains(r#"#[get("/hello/{name}")]"#));
        assert!(content.contains("#[autumn_web::main]"));
        assert!(content.contains("autumn_web::app()"));
    }

    // ── nav_bar scaffold layout (#1137) ─────────────────────────────

    #[test]
    fn main_template_uses_nav_bar_widget() {
        assert!(
            templates::MAIN_RS.contains("nav_bar("),
            "main.rs.tmpl should render its nav via nav_bar(), got:\n{}",
            templates::MAIN_RS
        );
        assert!(
            templates::MAIN_RS.contains("NavBarConfig::new()"),
            "main.rs.tmpl should build a NavBarConfig, got:\n{}",
            templates::MAIN_RS
        );
        assert!(
            !templates::MAIN_RS.contains(r#"nav aria-label="Main navigation""#),
            "main.rs.tmpl should no longer hand-roll its <nav>, got:\n{}",
            templates::MAIN_RS
        );
    }

    #[test]
    fn main_template_layout_takes_current_path() {
        assert!(
            templates::MAIN_RS.contains("current_path: &str"),
            "layout() should take current_path so nav_bar can mark the active link, got:\n{}",
            templates::MAIN_RS
        );
        assert!(
            templates::MAIN_RS.contains("CurrentPath"),
            "index() should extract CurrentPath to pass to layout(), got:\n{}",
            templates::MAIN_RS
        );
    }

    #[test]
    fn main_template_nav_keeps_descriptive_aria_label() {
        // The old hand-rolled <nav> had aria-label="Main navigation"; nav_bar's
        // own default is just "Main", so the template must call .aria_label(...)
        // explicitly or the landmark's accessible name silently degrades.
        assert!(
            templates::MAIN_RS.contains(r#".aria_label("Main navigation")"#),
            "layout()'s NavBarConfig should keep the descriptive \"Main navigation\" \
             aria-label instead of nav_bar's generic \"Main\" default, got:\n{}",
            templates::MAIN_RS
        );
    }

    #[test]
    fn input_css_mobile_nav_collapse_wraps_below_header() {
        // .autumn-nav is a flex row (nowrap by default) and .autumn-nav__collapse
        // is one of its flex items; giving that item `basis-full` only drops it
        // to a new row if the row itself is allowed to wrap. Without flex-wrap
        // on the enhanced root, the opened mobile menu squeezes/overflows onto
        // the same row as the brand and toggle instead of appearing below them.
        //
        // This rule now ships from the framework itself (#1215), not the
        // per-project input.css.tmpl — assert against the shared stylesheet.
        let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
        let rule_body = css_rule_body(css, ".autumn-nav--enhanced {");
        assert!(
            rule_body.contains("flex-wrap"),
            "the mobile media query must set flex-wrap on .autumn-nav--enhanced itself \
             so .autumn-nav__collapse's basis-full can actually start a new row, got:\n{css}"
        );
    }

    /// Extract the declaration block (without the braces) of the first CSS
    /// rule whose selector text starts with `selector_prefix` (which must
    /// include the trailing ` {`). Scans by byte position rather than
    /// matching a literal multi-line blob, so it doesn't depend on the
    /// source file's line-ending style — this repo checks out `*.tmpl` as
    /// `text=auto`, which normalizes to CRLF on Windows, and a pattern with
    /// an embedded `\n` would never match the CRLF-containing string
    /// `include_str!` reads back on that platform.
    fn css_rule_body<'a>(css: &'a str, selector_prefix: &str) -> &'a str {
        let rule_start = css
            .find(selector_prefix)
            .unwrap_or_else(|| panic!("no bare `{selector_prefix}` rule found in input.css.tmpl"));
        let rest = &css[rule_start..];
        &rest[..rest
            .find('}')
            .unwrap_or_else(|| panic!("unterminated `{selector_prefix}` rule"))]
    }

    #[test]
    fn input_css_nav_collapse_is_flex_row_so_trailing_items_align_right() {
        // .autumn-nav__collapse wraps both the primary .autumn-nav__items
        // list and the .autumn-nav__items--trailing list; without collapse
        // itself being a flex row (and growing to fill the nav's remaining
        // width), the trailing list's ml-auto has no flex row to push
        // against — the two lists just stack vertically as ordinary block
        // children instead of sitting side by side with trailing pinned to
        // the far right, as nav_bar's trailing-slot design intends.
        //
        // This rule now ships from the framework itself (#1215), not the
        // per-project input.css.tmpl — assert against the shared stylesheet.
        let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
        let rule_body = css_rule_body(css, ".autumn-nav__collapse {");
        assert!(
            rule_body.contains("flex") && !rule_body.contains("flex-col"),
            "the base (non-mobile) .autumn-nav__collapse rule must be a flex row \
             so .autumn-nav__items--trailing's ml-auto can push it to the right \
             edge of the nav, got:\n{css}"
        );
    }

    #[test]
    fn autumn_toml_has_defaults() {
        let tmp = TempDir::new().unwrap();
        generate("cfg-check", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("cfg-check/autumn.toml")).unwrap();
        assert!(content.contains("port = 3000"));
        assert!(content.contains(r#"host = "127.0.0.1""#));
        assert!(content.contains(r#"level = "info""#));
        assert!(content.contains(r#"path = "/health""#));
    }

    #[test]
    fn autumn_toml_has_crate_name_in_db_url() {
        let tmp = TempDir::new().unwrap();
        generate("my-db-app", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("my-db-app/autumn.toml")).unwrap();
        assert!(content.contains("my_db_app"));
    }

    #[test]
    fn gitignore_excludes_target_and_css() {
        let tmp = TempDir::new().unwrap();
        generate("gi-check", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("gi-check/.gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains("static/css/autumn.css"));
        assert!(!content.contains("static/autumn/"));
        // `.env` (and local variants) are gitignored, but the committable
        // `.env.example` template is not.
        assert!(content.lines().any(|l| l.trim() == ".env"));
        assert!(!content.lines().any(|l| l.trim() == ".env.example"));
    }

    #[test]
    fn scaffolds_env_example_and_gitignores_env() {
        let tmp = TempDir::new().unwrap();
        generate("dotenv-check", tmp.path()).unwrap();
        let p = tmp.path().join("dotenv-check");

        // `.env.example` is scaffolded and documents the DB URL env key.
        let example = fs::read_to_string(p.join(".env.example")).unwrap();
        assert!(
            example.contains("AUTUMN_DATABASE__URL"),
            ".env.example must document AUTUMN_DATABASE__URL:\n{example}"
        );
        // crate_name token is substituted into the example URL.
        assert!(example.contains("dotenv_check"), "got:\n{example}");

        // `.gitignore` ignores `.env` but the committable `.env.example` remains
        // in the project (and is not ignored).
        let gitignore = fs::read_to_string(p.join(".gitignore")).unwrap();
        assert!(gitignore.lines().any(|l| l.trim() == ".env"));
        assert!(p.join(".env.example").is_file());
    }

    #[test]
    fn generated_build_rs_reruns_on_css_input_changes() {
        let tmp = TempDir::new().unwrap();
        generate("css-watch-check", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("css-watch-check/build.rs")).unwrap();
        assert!(content.contains("cargo:rerun-if-changed=static/css/input.css"));
        assert!(content.contains("cargo:rerun-if-changed=target/autumn/tailwindcss"));
        assert!(content.contains("cargo:rerun-if-env-changed=PATH"));
    }

    #[test]
    fn generated_build_rs_bakes_build_and_git_provenance() {
        // AC #4 of issue #1242: apps created by `autumn new` capture build + git
        // provenance with zero developer action — the generated build.rs emits
        // the AUTUMN_BUILD_* env vars that `#[autumn_web::main]` reads.
        let tmp = TempDir::new().unwrap();
        generate("provenance-check", tmp.path()).unwrap();

        let content = fs::read_to_string(tmp.path().join("provenance-check/build.rs")).unwrap();
        assert!(content.contains("emit_build_provenance"));
        assert!(content.contains("cargo:rustc-env=AUTUMN_BUILD_TIMESTAMP="));
        assert!(content.contains("cargo:rustc-env=AUTUMN_BUILD_GIT_SHA="));
        assert!(content.contains("cargo:rustc-env=AUTUMN_BUILD_GIT_SHA_SHORT="));
        assert!(content.contains("cargo:rustc-env=AUTUMN_BUILD_GIT_BRANCH="));
        assert!(content.contains("cargo:rustc-env=AUTUMN_BUILD_GIT_DIRTY="));
        // Best-effort git: never fails the build outside a checkout.
        assert!(content.contains("rev-parse"));
        // Re-run when HEAD *moves* (commit/amend/reset all rewrite logs/HEAD),
        // not only when HEAD/index files change.
        assert!(content.contains("logs/HEAD"));
        // Gitdir is resolved by asking git itself, so nested/monorepo apps whose
        // package root has no `.git` still register the parent checkout's rerun
        // triggers. `--git-common-dir` covers linked worktrees where `logs/HEAD`
        // lives in the common dir.
        assert!(content.contains("--git-dir"));
        assert!(content.contains("--git-common-dir"));
        // Linked worktrees: `--amend`/`reset` rewrites the per-worktree reflog
        // (`<git-dir>/logs/HEAD`) but not the shared common-dir reflog, so the
        // generated build.rs must watch the per-worktree reflog too — otherwise
        // Cargo never re-runs after an amend in a worktree and `/actuator/info`
        // reports a stale SHA.
        assert!(content.contains("git_dir.join(\"logs/HEAD\")"));
        // Reproducible builds: honor and watch SOURCE_DATE_EPOCH.
        assert!(content.contains("SOURCE_DATE_EPOCH"));
        assert!(content.contains("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH"));
    }

    #[test]
    fn generated_build_rs_prefers_build_arg_provenance_over_git() {
        // Issue #1676: containerized builds exclude `/.git` from the build
        // context, so the generated build.rs must prefer `AUTUMN_BUILD_*` build
        // args (Docker `--build-arg`/`ENV` passthrough) when set, falling back
        // to git only for local checkout builds. Without this, production images
        // report `git.*` as `null` on `/actuator/info`.
        let tmp = TempDir::new().unwrap();
        generate("build-arg-provenance-check", tmp.path()).unwrap();

        let content =
            fs::read_to_string(tmp.path().join("build-arg-provenance-check/build.rs")).unwrap();
        // A dedicated helper reads the passthrough env vars.
        assert!(content.contains("fn build_arg("));
        // Each git field prefers the build arg, then falls back to git.
        assert!(content.contains("build_arg(\"AUTUMN_BUILD_GIT_SHA\").or_else(|| git("));
        assert!(content.contains("build_arg(\"AUTUMN_BUILD_GIT_SHA_SHORT\")"));
        assert!(content.contains("build_arg(\"AUTUMN_BUILD_GIT_BRANCH\")"));
        assert!(content.contains("build_arg(\"AUTUMN_BUILD_GIT_DIRTY\")"));
        // Timestamp passthrough wins over the computed timestamp.
        assert!(
            content
                .contains("build_arg(\"AUTUMN_BUILD_TIMESTAMP\").unwrap_or_else(build_timestamp)")
        );
        // Re-run when a passthrough build arg changes so deploys re-bake it.
        assert!(content.contains("cargo:rerun-if-env-changed={var}"));
    }

    #[test]
    fn generated_build_rs_reports_unknown_dirty_as_absent_not_false() {
        // Codex P2 / issue #1676 regression: a container build supplies
        // `AUTUMN_BUILD_GIT_SHA` but leaves `AUTUMN_BUILD_GIT_DIRTY` blank, and
        // the Dockerfile excludes `/.git` so `git status` cannot run. The dirty
        // state is then genuinely UNKNOWN and must be reported as absent/null on
        // `/actuator/info`, NOT collapsed to a misleading `false`.
        //
        // Prove it end-to-end at the producer: compile the generated `build.rs`
        // (which is verbatim Rust, no placeholders) and run its provenance
        // emitter under a controlled environment. Env is passed to the child
        // process (never mutated in-process), so this is isolated from other
        // concurrently running tests.
        let tmp = TempDir::new().unwrap();
        generate("dirty-unknown-check", tmp.path()).unwrap();
        let project = tmp.path().join("dirty-unknown-check");
        let build_rs = project.join("build.rs");

        // Compile the generated build.rs to a standalone binary. `--cap-lints
        // allow` keeps a stray warning from failing under a strict RUSTFLAGS,
        // and RUSTFLAGS is cleared for the same reason.
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
        let bin = project.join("provenance_probe");
        let compile = std::process::Command::new(&rustc)
            .arg(&build_rs)
            .arg("--edition")
            .arg("2021")
            .arg("--cap-lints")
            .arg("allow")
            .arg("-o")
            .arg(&bin)
            .env_remove("RUSTFLAGS")
            .output()
            .expect("failed to spawn rustc");
        assert!(
            compile.status.success(),
            "generated build.rs failed to compile:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );

        // Run the emitter with a cleared environment (so `PATH` is unset and no
        // `git` binary is reachable → git is genuinely unavailable), plus only
        // the provenance env vars under test. Returns the emitted stdout.
        let run = |vars: &[(&str, &str)]| -> String {
            let mut cmd = std::process::Command::new(&bin);
            cmd.current_dir(&project).env_clear();
            for (k, v) in vars {
                cmd.env(k, v);
            }
            let out = cmd.output().expect("failed to run provenance probe");
            assert!(
                out.status.success(),
                "provenance probe exited non-zero:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };

        let has_dirty = |stdout: &str, value: &str| {
            stdout
                .lines()
                .any(|l| l == format!("cargo:rustc-env=AUTUMN_BUILD_GIT_DIRTY={value}"))
        };
        let emits_any_dirty = |stdout: &str| {
            stdout
                .lines()
                .any(|l| l.starts_with("cargo:rustc-env=AUTUMN_BUILD_GIT_DIRTY="))
        };

        // 1. SHA passthrough, dirty blank, no git → dirty is UNKNOWN → the var
        //    is omitted entirely (consumer renders `git.dirty` as null).
        let unknown = run(&[
            (
                "AUTUMN_BUILD_GIT_SHA",
                "0123456789abcdef0123456789abcdef01234567",
            ),
            ("AUTUMN_BUILD_GIT_DIRTY", ""),
        ]);
        assert!(
            unknown.lines().any(|l| l
                == "cargo:rustc-env=AUTUMN_BUILD_GIT_SHA=0123456789abcdef0123456789abcdef01234567"),
            "SHA passthrough should still be emitted:\n{unknown}"
        );
        assert!(
            !emits_any_dirty(&unknown),
            "unknown dirty state must NOT emit AUTUMN_BUILD_GIT_DIRTY (would render as `false`):\n{unknown}"
        );

        // 2. Explicit dirty=true round-trips even with no git.
        let dirty_true = run(&[
            ("AUTUMN_BUILD_GIT_SHA", "abc123"),
            ("AUTUMN_BUILD_GIT_DIRTY", "true"),
        ]);
        assert!(
            has_dirty(&dirty_true, "true"),
            "explicit dirty=true must round-trip:\n{dirty_true}"
        );

        // 3. Explicit dirty=false round-trips (a genuinely clean, known build).
        let dirty_false = run(&[
            ("AUTUMN_BUILD_GIT_SHA", "abc123"),
            ("AUTUMN_BUILD_GIT_DIRTY", "false"),
        ]);
        assert!(
            has_dirty(&dirty_false, "false"),
            "explicit dirty=false must round-trip:\n{dirty_false}"
        );
    }

    #[test]
    fn no_unsubstituted_placeholders() {
        let tmp = TempDir::new().unwrap();
        generate("placeholder-check", tmp.path()).unwrap();

        let p = tmp.path().join("placeholder-check");
        for entry in walkdir(&p) {
            let content = fs::read_to_string(&entry).unwrap();
            assert!(
                !content.contains("{{"),
                "unsubstituted placeholder in {}",
                entry.display()
            );
        }
    }

    #[test]
    fn daemon_readme_has_no_unsubstituted_placeholders() {
        // The daemon / bundled-pg README bodies are built in Rust (not the
        // template), so guard them against reintroducing `{{…}}` tokens.
        for opts in [daemon_opts(), bundled_pg_opts()] {
            let tmp = TempDir::new().unwrap();
            generate_with("ph-daemon-app", tmp.path(), opts).unwrap();
            let readme = fs::read_to_string(tmp.path().join("ph-daemon-app/README.md")).unwrap();
            assert!(
                !readme.contains("{{"),
                "unsubstituted placeholder in daemon/bundled README (opts {opts:?}):\n{readme}"
            );
        }
    }

    #[test]
    fn daemon_readme_drops_db_bootstrap() {
        // A DB-free daemon must not tell users to run migrations or configure a
        // database; it runs via `autumn serve`.
        let tmp = TempDir::new().unwrap();
        generate_with("daemon-readme", tmp.path(), daemon_opts()).unwrap();
        let readme = fs::read_to_string(tmp.path().join("daemon-readme/README.md")).unwrap();
        assert!(!readme.contains("autumn migrate"), "got:\n{readme}");
        assert!(!readme.contains("libpq"), "got:\n{readme}");
        assert!(readme.contains("autumn serve"), "got:\n{readme}");
        // Project name still substituted.
        assert!(readme.contains("daemon-readme"), "got:\n{readme}");
    }

    #[test]
    fn bundled_pg_readme_documents_managed_db() {
        // A bundled-pg daemon manages its own Postgres and auto-applies
        // migrations; it runs via `autumn serve --bundled-pg`.
        let tmp = TempDir::new().unwrap();
        generate_with("bundled-readme", tmp.path(), bundled_pg_opts()).unwrap();
        let readme = fs::read_to_string(tmp.path().join("bundled-readme/README.md")).unwrap();
        assert!(!readme.contains("autumn migrate"), "got:\n{readme}");
        assert!(
            readme.contains("autumn serve --bundled-pg"),
            "got:\n{readme}"
        );
    }

    #[test]
    fn default_readme_db_bootstrap_does_not_dead_end_on_release_init() {
        // Finding 1: the default golden path must bootstrap a local Postgres
        // with `docker run` (which always works), not lead with
        // `release init --target docker-compose` (which file-errors on a fresh
        // scaffold). The AC-required pointer is still present for deployment.
        let tmp = TempDir::new().unwrap();
        generate("dockerrun-app", tmp.path()).unwrap();
        let readme = fs::read_to_string(tmp.path().join("dockerrun-app/README.md")).unwrap();
        assert!(
            readme.contains("docker run") && readme.contains("postgres:16"),
            "default README must offer a `docker run … postgres:16` DB bootstrap, got:\n{readme}"
        );
        // Codex P2: the runnable `docker run -d …` helper must appear exactly once
        // (in step 2), not be duplicated in the prerequisites — a second identical
        // command dead-ends on a `{crate}-pg` container-name-in-use error.
        assert_eq!(
            readme.matches("docker run -d").count(),
            1,
            "default README must contain the runnable `docker run -d …` helper exactly once, \
             got:\n{readme}"
        );
        assert!(
            readme.contains("autumn release init --target docker-compose"),
            "default README must still point at `release init --target docker-compose`, \
             got:\n{readme}"
        );
    }

    #[test]
    fn already_exists_error() {
        let tmp = TempDir::new().unwrap();
        generate("dupe-check", tmp.path()).unwrap();
        let err = generate("dupe-check", tmp.path()).unwrap_err();
        assert!(matches!(err, NewError::AlreadyExists(_)));
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn invalid_name_error() {
        let tmp = TempDir::new().unwrap();
        let err = generate("123bad", tmp.path()).unwrap_err();
        assert!(matches!(err, NewError::InvalidName(_, _)));
    }

    // ── --with-i18n scaffold ─────────────────────────────────────

    #[test]
    fn default_does_not_scaffold_i18n() {
        let tmp = TempDir::new().unwrap();
        generate("plain-app", tmp.path()).unwrap();
        let p = tmp.path().join("plain-app");
        assert!(!p.join("i18n").exists());
        let cargo = fs::read_to_string(p.join("Cargo.toml")).unwrap();
        assert!(!cargo.contains("features = [\"i18n\"]"));
        let toml = fs::read_to_string(p.join("autumn.toml")).unwrap();
        assert!(!toml.contains("[i18n]"));
        let main = fs::read_to_string(p.join("src/main.rs")).unwrap();
        assert!(!main.contains(".i18n_auto()"));
    }

    fn daemon_opts() -> GenerateOptions {
        GenerateOptions {
            with_daemon: true,
            ..GenerateOptions::default()
        }
    }

    #[test]
    fn daemon_with_seed_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = generate_with(
            "daemon-seed-app",
            tmp.path(),
            GenerateOptions {
                with_daemon: true,
                with_seed: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, NewError::IncompatibleOptions(_)));
        // Nothing should be scaffolded on rejection.
        assert!(!tmp.path().join("daemon-seed-app").exists());
    }

    #[test]
    fn bundled_pg_with_seed_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = generate_with(
            "pg-seed-app",
            tmp.path(),
            GenerateOptions {
                with_bundled_pg: true,
                with_daemon: true,
                with_seed: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, NewError::IncompatibleOptions(_)));
        assert!(!tmp.path().join("pg-seed-app").exists());
    }

    #[test]
    fn daemon_starter_omits_db_feature() {
        let tmp = TempDir::new().unwrap();
        generate_with("daemon-app", tmp.path(), daemon_opts()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("daemon-app/Cargo.toml")).unwrap();
        assert!(
            cargo.contains("default-features = false"),
            "daemon starter must disable default features (drop db): {cargo}"
        );
        assert!(
            !cargo.contains("\"db\""),
            "daemon starter must not enable db"
        );
        assert!(
            !cargo.contains("diesel_migrations"),
            "daemon starter must not depend on diesel_migrations"
        );
    }

    #[test]
    fn daemon_starter_main_has_no_migrations() {
        let tmp = TempDir::new().unwrap();
        generate_with("daemon-main-app", tmp.path(), daemon_opts()).unwrap();
        let main = fs::read_to_string(tmp.path().join("daemon-main-app/src/main.rs")).unwrap();
        assert!(
            !main.contains(".migrations("),
            "daemon main must not call .migrations()"
        );
        assert!(
            !main.contains("embed_migrations"),
            "daemon main must not embed migrations"
        );
    }

    #[test]
    fn daemon_starter_autumn_toml_documents_zero_db() {
        let tmp = TempDir::new().unwrap();
        generate_with("daemon-cfg-app", tmp.path(), daemon_opts()).unwrap();
        let toml = fs::read_to_string(tmp.path().join("daemon-cfg-app/autumn.toml")).unwrap();
        assert!(toml.contains("autumn serve"));
    }

    fn bundled_pg_opts() -> GenerateOptions {
        GenerateOptions {
            with_bundled_pg: true,
            with_daemon: true,
            ..GenerateOptions::default()
        }
    }

    #[test]
    fn bundled_pg_starter_enables_managed_feature_and_keeps_db() {
        let tmp = TempDir::new().unwrap();
        generate_with("pg-app", tmp.path(), bundled_pg_opts()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("pg-app/Cargo.toml")).unwrap();
        assert!(
            cargo.contains("managed-pg-bundled"),
            "bundled starter must enable managed-pg-bundled: {cargo}"
        );
        assert!(
            !cargo.contains("default-features = false"),
            "bundled starter keeps the database (default features on)"
        );
    }

    #[test]
    fn bundled_pg_autumn_toml_enables_auto_migrate_in_production() {
        let tmp = TempDir::new().unwrap();
        generate_with("pg-cfg-app", tmp.path(), bundled_pg_opts()).unwrap();
        let toml = fs::read_to_string(tmp.path().join("pg-cfg-app/autumn.toml")).unwrap();
        // A `--release` daemon runs under the prod profile and the managed DB is
        // unreachable to `autumn migrate`, so migrations must apply automatically.
        assert!(
            toml.contains("[database]") && toml.contains("auto_migrate_in_production = true"),
            "bundled starter must auto-migrate in production: {toml}"
        );
        // Still valid TOML (no duplicate tables with the commented template).
        toml::from_str::<toml::Table>(&toml).expect("generated autumn.toml parses");
    }

    #[test]
    fn bundled_pg_main_installs_provider_and_shutdown_hook() {
        let tmp = TempDir::new().unwrap();
        generate_with("pg-main-app", tmp.path(), bundled_pg_opts()).unwrap();
        let main = fs::read_to_string(tmp.path().join("pg-main-app/src/main.rs")).unwrap();
        assert!(main.contains("ManagedPostgresPoolProvider"));
        assert!(main.contains(".with_pool_provider("));
        assert!(main.contains(".on_shutdown("));
        // Database present: migrations kept.
        assert!(main.contains(".migrations("));
    }

    #[test]
    fn default_generation_has_no_managed_pg() {
        let tmp = TempDir::new().unwrap();
        generate("plain2-app", tmp.path()).unwrap();
        let main = fs::read_to_string(tmp.path().join("plain2-app/src/main.rs")).unwrap();
        assert!(!main.contains("with_pool_provider"));
        let cargo = fs::read_to_string(tmp.path().join("plain2-app/Cargo.toml")).unwrap();
        assert!(!cargo.contains("managed-pg"));
    }

    #[test]
    fn default_generation_still_has_db() {
        let tmp = TempDir::new().unwrap();
        generate("plain-app", tmp.path()).unwrap();
        let cargo = fs::read_to_string(tmp.path().join("plain-app/Cargo.toml")).unwrap();
        // Default keeps the simple full-default dependency and migrations.
        assert!(cargo.contains(r#"autumn-web = ""#));
        assert!(!cargo.contains("default-features = false"));
        assert!(cargo.contains("diesel_migrations"));
        let main = fs::read_to_string(tmp.path().join("plain-app/src/main.rs")).unwrap();
        assert!(main.contains(".migrations("));
    }

    #[test]
    fn with_i18n_scaffolds_translation_dir_and_stub_file() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "i18n-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let p = tmp.path().join("i18n-app");
        assert!(p.join("i18n").is_dir(), "i18n/ dir not created");
        assert!(
            p.join("i18n/en.ftl").is_file(),
            "i18n/en.ftl stub not created"
        );
        let stub = fs::read_to_string(p.join("i18n/en.ftl")).unwrap();
        assert!(stub.contains("welcome.title"));
        assert!(stub.contains("welcome.greeting"));
    }

    #[test]
    fn with_i18n_enables_feature_flag_in_cargo_toml() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "feat-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let cargo = fs::read_to_string(tmp.path().join("feat-app/Cargo.toml")).unwrap();
        assert!(
            cargo.contains(r#"features = ["i18n"]"#),
            "Cargo.toml should enable i18n feature: {cargo}"
        );
    }

    #[test]
    fn with_i18n_adds_block_to_autumn_toml() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "cfg-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let cfg = fs::read_to_string(tmp.path().join("cfg-app/autumn.toml")).unwrap();
        assert!(cfg.contains("[i18n]"));
        assert!(cfg.contains("default_locale = \"en\""));
        assert!(cfg.contains("supported_locales = [\"en\"]"));
    }

    #[test]
    fn with_i18n_calls_i18n_auto_in_main() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "main-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let main = fs::read_to_string(tmp.path().join("main-app/src/main.rs")).unwrap();
        assert!(
            main.contains(".i18n_auto()"),
            "main.rs should call .i18n_auto(): {main}"
        );
    }

    #[test]
    fn with_i18n_copies_i18n_into_fullstack_docker_image() {
        // The fullstack (non-`--api`) scaffold's `main.rs` calls `.i18n_auto()`
        // for `--with-i18n`, which loads `i18n/en.ftl` from disk at startup and
        // panics if missing. The image must therefore ship the `i18n/` sidecar
        // into both the builder and runtime stages (issue #1865, mirroring the
        // `--api` fix in #1847).
        let tmp = TempDir::new().unwrap();
        generate_with(
            "i18n-docker-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let dockerfile = fs::read_to_string(tmp.path().join("i18n-docker-app/Dockerfile")).unwrap();
        assert!(
            dockerfile.contains("COPY i18n ./i18n"),
            "--with-i18n fullstack Dockerfile must copy i18n/ into the builder stage:\n{dockerfile}"
        );
        assert!(
            dockerfile.contains("COPY --from=builder /app/i18n /app/i18n"),
            "--with-i18n fullstack Dockerfile must copy i18n/ into the runtime stage:\n{dockerfile}"
        );
        assert!(
            !dockerfile.contains("__AUTUMN_I18N"),
            "--with-i18n fullstack Dockerfile must not leave anchor markers:\n{dockerfile}"
        );
    }

    #[test]
    fn without_i18n_fullstack_docker_image_has_no_i18n_copy() {
        // A non-i18n fullstack Dockerfile must carry NO i18n `COPY` lines (an
        // unconditional `COPY i18n ./i18n` would break `docker build`, whose
        // context has no `i18n/` dir) and no leftover anchor markers.
        let tmp = TempDir::new().unwrap();
        generate("no-i18n-docker-app", tmp.path()).unwrap();
        let dockerfile =
            fs::read_to_string(tmp.path().join("no-i18n-docker-app/Dockerfile")).unwrap();
        assert!(
            !dockerfile.contains("COPY i18n ./i18n"),
            "non-i18n fullstack Dockerfile must not copy i18n/ (build context has no i18n/ dir):\n{dockerfile}"
        );
        assert!(
            !dockerfile.contains("/app/i18n"),
            "non-i18n fullstack Dockerfile must not reference /app/i18n:\n{dockerfile}"
        );
        assert!(
            !dockerfile.contains("__AUTUMN_I18N"),
            "non-i18n fullstack Dockerfile must not leave anchor markers:\n{dockerfile}"
        );
    }

    fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(walkdir(&path));
                } else {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if ext != "enc" {
                        files.push(path);
                    }
                }
            }
        }
        files
    }

    // ── --with-seed tests ──────────────────────────────────────────────────

    #[test]
    fn no_seed_bin_without_flag() {
        let tmp = TempDir::new().unwrap();
        generate("no-seed-app", tmp.path()).unwrap();
        assert!(!tmp.path().join("no-seed-app/src/bin/seed.rs").exists());
    }

    #[test]
    fn generates_seed_bin_when_with_seed() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "seed-app",
            tmp.path(),
            GenerateOptions {
                with_seed: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert!(tmp.path().join("seed-app/src/bin/seed.rs").is_file());
    }

    #[test]
    fn with_seed_cargo_toml_has_bin_entry_and_seed_feature() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "seed-cargo",
            tmp.path(),
            GenerateOptions {
                with_seed: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let content = fs::read_to_string(tmp.path().join("seed-cargo/Cargo.toml")).unwrap();
        assert!(
            content.contains("[[bin]]"),
            "Cargo.toml should have [[bin]]"
        );
        assert!(
            content.contains("seed"),
            "Cargo.toml [[bin]] entry should mention 'seed'"
        );
        // The seed feature must be enabled on autumn-web so src/bin/seed.rs
        // can import autumn_web::seed::SeedContext without manual edits.
        assert!(
            content.contains(r#"features = ["seed"]"#),
            "autumn-web dependency should include the seed feature, got:\n{content}"
        );
    }

    #[test]
    fn with_i18n_and_seed_combines_feature_flags() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "combo-app",
            tmp.path(),
            GenerateOptions {
                with_i18n: true,
                with_seed: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();

        let p = tmp.path().join("combo-app");
        let cargo = fs::read_to_string(p.join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains(r#"features = ["i18n", "seed"]"#),
            "Cargo.toml should preserve both optional features: {cargo}"
        );
        assert!(p.join("i18n/en.ftl").is_file());
        assert!(p.join("src/bin/seed.rs").is_file());
    }

    #[test]
    fn no_bin_entry_in_cargo_toml_without_flag() {
        let tmp = TempDir::new().unwrap();
        generate("plain-cargo", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("plain-cargo/Cargo.toml")).unwrap();
        assert!(
            !content.contains("[[bin]]"),
            "Cargo.toml should not have [[bin]] when --with-seed is off"
        );
    }

    #[test]
    fn with_seed_no_unsubstituted_placeholders() {
        let tmp = TempDir::new().unwrap();
        generate_with(
            "seed-placeholder-check",
            tmp.path(),
            GenerateOptions {
                with_seed: true,
                ..GenerateOptions::default()
            },
        )
        .unwrap();

        let p = tmp.path().join("seed-placeholder-check");
        for entry in walkdir(&p) {
            let content = fs::read_to_string(&entry).unwrap();
            assert!(
                !content.contains("{{"),
                "unsubstituted placeholder in {}",
                entry.display()
            );
        }
    }

    // ── credentials scaffolding tests ─────────────────────────────────────

    #[test]
    fn generates_config_credentials_directory() {
        let tmp = TempDir::new().unwrap();
        generate("cred-app", tmp.path()).unwrap();
        let p = tmp.path().join("cred-app");
        assert!(
            p.join("config/credentials").is_dir(),
            "config/credentials/ directory must be created by autumn new"
        );
    }

    #[test]
    fn generates_development_enc_file() {
        let tmp = TempDir::new().unwrap();
        generate("cred-enc-app", tmp.path()).unwrap();
        let p = tmp.path().join("cred-enc-app");
        assert!(
            p.join("config/credentials/development.toml.enc").is_file(),
            "config/credentials/development.toml.enc must be created by autumn new"
        );
    }

    #[test]
    fn generates_master_key_file() {
        let tmp = TempDir::new().unwrap();
        generate("key-app", tmp.path()).unwrap();
        let p = tmp.path().join("key-app");
        assert!(
            p.join("config/master.key").is_file(),
            "config/master.key must be created by autumn new"
        );
    }

    #[test]
    fn master_key_file_contains_64_hex_chars() {
        let tmp = TempDir::new().unwrap();
        generate("key-hex-app", tmp.path()).unwrap();
        let key = fs::read_to_string(tmp.path().join("key-hex-app/config/master.key")).unwrap();
        let key = key.trim();
        assert_eq!(key.len(), 64, "master.key must contain 64 hex chars");
        assert!(
            key.chars().all(|c| c.is_ascii_hexdigit()),
            "master.key must be valid hex"
        );
    }

    #[test]
    fn gitignore_includes_master_key() {
        let tmp = TempDir::new().unwrap();
        generate("gi-key-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("gi-key-app/.gitignore")).unwrap();
        assert!(
            content.contains("config/master.key"),
            ".gitignore must exclude config/master.key, got:\n{content}"
        );
    }

    #[test]
    fn gitignore_does_not_exclude_enc_files() {
        let tmp = TempDir::new().unwrap();
        generate("gi-enc-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("gi-enc-app/.gitignore")).unwrap();
        assert!(
            !content.contains("*.enc"),
            ".gitignore must NOT exclude .enc files (they're safe to commit), got:\n{content}"
        );
    }

    #[test]
    fn development_enc_file_is_decryptable_with_master_key() {
        use autumn_web::credentials::{MasterKey, decrypt};
        let tmp = TempDir::new().unwrap();
        generate("roundtrip-cred-app", tmp.path()).unwrap();
        let p = tmp.path().join("roundtrip-cred-app");
        let key_hex = fs::read_to_string(p.join("config/master.key")).unwrap();
        let key = MasterKey::from_hex_pub(key_hex.trim()).expect("master.key should be valid hex");
        let ct = fs::read(p.join("config/credentials/development.toml.enc")).unwrap();
        let pt = decrypt(&key, &ct).expect("development.toml.enc should decrypt with master.key");
        let s = String::from_utf8(pt).unwrap();
        assert!(
            s.contains("stripe_secret_key") || s.contains('#'),
            "decrypted content should have placeholder comments"
        );
    }

    // ── rust-toolchain.toml / rustfmt.toml / clippy.toml scaffolding ─────────

    #[test]
    fn generates_rust_toolchain_toml() {
        let tmp = TempDir::new().unwrap();
        generate("toolchain-app", tmp.path()).unwrap();
        let p = tmp.path().join("toolchain-app");
        assert!(
            p.join("rust-toolchain.toml").is_file(),
            "`autumn new` must write rust-toolchain.toml"
        );
    }

    #[test]
    fn rust_toolchain_pins_channel_to_msrv() {
        let tmp = TempDir::new().unwrap();
        generate("toolchain-ver-app", tmp.path()).unwrap();
        let content =
            fs::read_to_string(tmp.path().join("toolchain-ver-app/rust-toolchain.toml")).unwrap();
        assert!(
            content.contains("channel"),
            "rust-toolchain.toml must set channel: {content}"
        );
        assert!(
            content.contains("1.88.0"),
            "rust-toolchain.toml channel must match the Cargo.toml rust-version (1.88.0): {content}"
        );
    }

    #[test]
    fn rust_toolchain_lists_rustfmt_and_clippy_components() {
        let tmp = TempDir::new().unwrap();
        generate("toolchain-comp-app", tmp.path()).unwrap();
        let content =
            fs::read_to_string(tmp.path().join("toolchain-comp-app/rust-toolchain.toml")).unwrap();
        assert!(
            content.contains("rustfmt"),
            "rust-toolchain.toml must list rustfmt in components: {content}"
        );
        assert!(
            content.contains("clippy"),
            "rust-toolchain.toml must list clippy in components: {content}"
        );
    }

    #[test]
    fn generates_rustfmt_toml() {
        let tmp = TempDir::new().unwrap();
        generate("fmt-app", tmp.path()).unwrap();
        let p = tmp.path().join("fmt-app");
        assert!(
            p.join("rustfmt.toml").is_file(),
            "`autumn new` must write rustfmt.toml"
        );
    }

    #[test]
    fn rustfmt_toml_has_correct_edition_and_max_width() {
        let tmp = TempDir::new().unwrap();
        generate("fmt-cfg-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("fmt-cfg-app/rustfmt.toml")).unwrap();
        assert!(
            content.contains(r#"edition = "2024""#),
            "rustfmt.toml must set edition = \"2024\": {content}"
        );
        assert!(
            content.contains("max_width = 100"),
            "rustfmt.toml must set max_width = 100: {content}"
        );
    }

    #[test]
    fn generates_clippy_toml() {
        let tmp = TempDir::new().unwrap();
        generate("clippy-app", tmp.path()).unwrap();
        let p = tmp.path().join("clippy-app");
        assert!(
            p.join("clippy.toml").is_file(),
            "`autumn new` must write clippy.toml"
        );
    }

    #[test]
    fn clippy_toml_msrv_matches_rust_version() {
        let tmp = TempDir::new().unwrap();
        generate("clippy-msrv-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("clippy-msrv-app/clippy.toml")).unwrap();
        assert!(
            content.contains("msrv"),
            "clippy.toml must set msrv: {content}"
        );
        assert!(
            content.contains("1.88.0"),
            "clippy.toml msrv must match Cargo.toml rust-version (1.88.0): {content}"
        );
    }

    #[test]
    fn gitignore_does_not_exclude_toolchain_files() {
        let tmp = TempDir::new().unwrap();
        generate("gi-toolchain-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("gi-toolchain-app/.gitignore")).unwrap();
        assert!(
            !content.contains("rust-toolchain"),
            ".gitignore must NOT exclude rust-toolchain.toml: {content}"
        );
        assert!(
            !content.contains("rustfmt.toml"),
            ".gitignore must NOT exclude rustfmt.toml: {content}"
        );
        assert!(
            !content.contains("clippy.toml"),
            ".gitignore must NOT exclude clippy.toml: {content}"
        );
    }

    #[test]
    fn scaffold_summary_mentions_toolchain_files() {
        let tmp = TempDir::new().unwrap();
        // Use generate_with (non-quiet) and capture stdout.
        // We can't easily capture stdout in unit tests, so we verify the files
        // exist and the summary helper doesn't strip them — this is covered by
        // the file-existence tests above. This test verifies print_scaffold_summary
        // is at least called without panic for the default case.
        generate("summary-toolchain-app", tmp.path()).unwrap();
        let p = tmp.path().join("summary-toolchain-app");
        assert!(p.join("rust-toolchain.toml").is_file());
        assert!(p.join("rustfmt.toml").is_file());
        assert!(p.join("clippy.toml").is_file());
    }

    #[test]
    fn two_new_projects_get_different_master_keys() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        generate("app-a", tmp1.path()).unwrap();
        generate("app-b", tmp2.path()).unwrap();
        let k1 = fs::read_to_string(tmp1.path().join("app-a/config/master.key")).unwrap();
        let k2 = fs::read_to_string(tmp2.path().join("app-b/config/master.key")).unwrap();
        assert_ne!(
            k1.trim(),
            k2.trim(),
            "each project must get a unique master key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn master_key_file_has_secure_permissions() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        generate("secure-key-app", tmp.path()).unwrap();
        let p = tmp.path().join("secure-key-app/config/master.key");
        let meta = fs::metadata(&p).unwrap();
        let mode = meta.mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "master.key permissions must be 0o600, got {mode:#o}"
        );
    }

    // ── Vendor asset scaffolding ─────────────────────────────────────────────

    #[test]
    fn scaffold_writes_htmx_js_file() {
        let tmp = TempDir::new().unwrap();
        generate("asset-app", tmp.path()).unwrap();
        let htmx = tmp.path().join("asset-app/static/js/htmx.min.js");
        assert!(
            htmx.is_file(),
            "static/js/htmx.min.js must be created by `autumn new`"
        );
        let bytes = fs::read(&htmx).unwrap();
        assert!(!bytes.is_empty(), "htmx.min.js must not be empty");
    }

    #[test]
    fn scaffold_writes_vendor_manifest() {
        let tmp = TempDir::new().unwrap();
        generate("manifest-app", tmp.path()).unwrap();
        let manifest_path = tmp.path().join("manifest-app/static/.autumn-assets.json");
        assert!(
            manifest_path.is_file(),
            "static/.autumn-assets.json must be created by `autumn new`"
        );
        let content = fs::read_to_string(&manifest_path).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&content).expect("manifest must be valid JSON");
        assert_eq!(manifest["version"], "1", "manifest must have version=1");
        let htmx = &manifest["assets"]["htmx"];
        assert!(!htmx.is_null(), "manifest must contain an htmx entry");
        assert!(
            htmx["version"].as_str().unwrap_or("").contains('.'),
            "htmx version must look like a semver: {}",
            htmx["version"]
        );
        let integrity = htmx["integrity"].as_str().unwrap_or("");
        assert!(
            integrity.starts_with("sha384-"),
            "htmx integrity must be a sha384 SRI hash: {integrity}"
        );

        let sse = &manifest["assets"]["htmx-ext-sse"];
        assert!(
            !sse.is_null(),
            "manifest must contain an htmx-ext-sse entry"
        );
        assert!(
            sse["version"].as_str().unwrap_or("").contains('.'),
            "htmx-ext-sse version must look like a semver: {}",
            sse["version"]
        );
        let sse_integrity = sse["integrity"].as_str().unwrap_or("");
        assert!(
            sse_integrity.starts_with("sha384-"),
            "htmx-ext-sse integrity must be a sha384 SRI hash: {sse_integrity}"
        );
    }

    #[test]
    fn manifest_integrity_matches_vendored_file() {
        let tmp = TempDir::new().unwrap();
        generate("sri-app", tmp.path()).unwrap();
        let p = tmp.path().join("sri-app");

        let htmx_bytes = fs::read(p.join("static/js/htmx.min.js")).unwrap();
        let computed = crate::assets::compute_sri(&htmx_bytes);

        let manifest_raw = fs::read_to_string(p.join("static/.autumn-assets.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
        let recorded = manifest["assets"]["htmx"]["integrity"].as_str().unwrap();

        assert_eq!(
            computed, recorded,
            "SRI hash in manifest must match the vendored htmx.min.js"
        );

        let sse_bytes = fs::read(p.join("static/js/htmx-ext-sse.min.js")).unwrap();
        let computed_sse = crate::assets::compute_sri(&sse_bytes);
        let recorded_sse = manifest["assets"]["htmx-ext-sse"]["integrity"]
            .as_str()
            .unwrap();

        assert_eq!(
            computed_sse, recorded_sse,
            "SRI hash in manifest must match the vendored htmx-ext-sse.min.js"
        );
    }

    #[test]
    fn generated_main_rs_uses_javascript_include_tag() {
        let tmp = TempDir::new().unwrap();
        generate("helper-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("helper-app/src/main.rs")).unwrap();
        assert!(
            content.contains("javascript_include_tag(\"htmx\")"),
            "generated main.rs must use javascript_include_tag(\"htmx\"), got:\n{content}"
        );
    }

    #[test]
    fn generated_main_rs_has_no_hardcoded_htmx_script_src() {
        let tmp = TempDir::new().unwrap();
        generate("no-hardcode-app", tmp.path()).unwrap();
        let content = fs::read_to_string(tmp.path().join("no-hardcode-app/src/main.rs")).unwrap();
        assert!(
            !content.contains("/static/js/htmx.min.js"),
            "generated main.rs must not hardcode /static/js/htmx.min.js, got:\n{content}"
        );
    }

    // --- issue #1593: the framework-owned file set `autumn upgrade` reconciles ---

    fn owned(opts: GenerateOptions) -> std::collections::BTreeMap<&'static str, String> {
        let vars = TemplateVars {
            project_name: "demo",
            crate_name: "demo",
            autumn_version: "0.7.0",
            rust_version: "1.88.0",
        };
        framework_owned_files(&vars, opts)
    }

    #[test]
    fn framework_owned_set_covers_the_fullstack_scaffold() {
        let files = owned(GenerateOptions::default());
        for expected in [
            "autumn.toml",
            "Dockerfile",
            ".dockerignore",
            "build.rs",
            ".gitignore",
            ".env.example",
            ".github/workflows/ci.yml",
            "rust-toolchain.toml",
            "rustfmt.toml",
            "clippy.toml",
            "tailwind.config.js",
            "static/css/input.css",
        ] {
            assert!(
                files.contains_key(expected),
                "missing {expected}: {:?}",
                files.keys()
            );
        }
    }

    /// The advisory policy is generated but deliberately *not* reconciled
    /// (issue #1600): its waiver list is the app author's, and a file the
    /// developer is asked to edit would come back as a conflict on every
    /// `autumn upgrade` — the same reason `Cargo.toml` is not owned either.
    #[test]
    fn the_advisory_policy_is_generated_but_not_framework_owned() {
        for opts in [
            GenerateOptions::default(),
            GenerateOptions {
                with_api: true,
                ..GenerateOptions::default()
            },
        ] {
            assert!(
                !owned(opts).contains_key("deny.toml"),
                "deny.toml carries the app's own waivers; reconciling it would \
                 conflict with every waiver its author adds"
            );
        }
        let tmp = TempDir::new().unwrap();
        generate("policy-owner-app", tmp.path()).unwrap();
        assert!(
            tmp.path().join("policy-owner-app/deny.toml").is_file(),
            "…but `autumn new` must still write it"
        );
    }

    #[test]
    fn framework_owned_set_never_reaches_into_src() {
        for opts in [
            GenerateOptions::default(),
            GenerateOptions {
                with_api: true,
                ..GenerateOptions::default()
            },
            GenerateOptions {
                with_i18n: true,
                with_seed: true,
                ..GenerateOptions::default()
            },
        ] {
            for path in owned(opts).keys() {
                assert!(
                    !path.starts_with("src/"),
                    "application source is out of bounds, got {path}"
                );
            }
        }
    }

    #[test]
    fn api_flavor_owns_no_css_or_tailwind() {
        let files = owned(GenerateOptions {
            with_api: true,
            ..GenerateOptions::default()
        });
        assert!(
            !files.contains_key("tailwind.config.js"),
            "{:?}",
            files.keys()
        );
        assert!(
            !files.contains_key("static/css/input.css"),
            "{:?}",
            files.keys()
        );
        // ...but it still owns the common set.
        assert!(files.contains_key("Dockerfile"));
        assert!(files.contains_key("build.rs"));
    }

    #[test]
    fn api_and_fullstack_render_different_dockerfiles_and_build_scripts() {
        let full = owned(GenerateOptions::default());
        let api = owned(GenerateOptions {
            with_api: true,
            ..GenerateOptions::default()
        });
        assert_ne!(full["Dockerfile"], api["Dockerfile"]);
        assert_ne!(full["build.rs"], api["build.rs"]);
    }

    #[test]
    fn i18n_option_is_reflected_in_the_owned_autumn_toml_and_dockerfile() {
        let plain = owned(GenerateOptions::default());
        let i18n = owned(GenerateOptions {
            with_i18n: true,
            ..GenerateOptions::default()
        });
        assert!(!plain["autumn.toml"].contains("[i18n]"));
        assert!(i18n["autumn.toml"].contains("[i18n]"));
        assert_ne!(plain["Dockerfile"], i18n["Dockerfile"]);
        // The unresolved anchors never survive into a generated file.
        assert!(!i18n["Dockerfile"].contains("__AUTUMN_I18N_BUILDER_COPY__"));
        assert!(!plain["Dockerfile"].contains("__AUTUMN_I18N_BUILDER_COPY__"));
    }

    #[test]
    fn daemon_and_bundled_pg_options_reach_the_owned_autumn_toml() {
        let daemon = owned(GenerateOptions {
            with_daemon: true,
            ..GenerateOptions::default()
        });
        assert!(
            daemon["autumn.toml"].contains("uses no database"),
            "{}",
            daemon["autumn.toml"]
        );
        let bundled = owned(GenerateOptions {
            with_daemon: true,
            with_bundled_pg: true,
            ..GenerateOptions::default()
        });
        assert!(
            bundled["autumn.toml"].contains("auto_migrate_in_production = true"),
            "{}",
            bundled["autumn.toml"]
        );
    }

    #[test]
    fn generated_project_files_match_the_framework_owned_rendering() {
        // The reconciler compares a project against `framework_owned_files`, so
        // a byte that `autumn new` writes differently is a permanent phantom
        // conflict. One renderer, one truth.
        let tmp = TempDir::new().unwrap();
        generate("owned-app", tmp.path()).unwrap();
        let vars = TemplateVars {
            project_name: "owned-app",
            crate_name: "owned_app",
            autumn_version: env!("CARGO_PKG_VERSION"),
            rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
        };
        for (path, expected) in framework_owned_files(&vars, GenerateOptions::default()) {
            let actual = fs::read_to_string(tmp.path().join("owned-app").join(path))
                .unwrap_or_else(|e| panic!("{path} was not scaffolded: {e}"));
            assert_eq!(actual, expected, "{path} drifted from its rendering");
        }
    }

    #[test]
    fn a_new_project_records_the_release_that_scaffolded_it() {
        use crate::upgrade::scaffold::{MANIFEST_PATH, Manifest};

        let tmp = TempDir::new().unwrap();
        generate("provenance-app", tmp.path()).unwrap();
        let root = tmp.path().join("provenance-app");

        assert!(root.join(MANIFEST_PATH).is_file(), "no scaffold manifest");
        let manifest = Manifest::load(&root).expect("manifest parses");
        assert_eq!(manifest.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(manifest.options, GenerateOptions::default());
        // Every framework-owned file it just wrote has a baseline digest.
        for path in framework_owned_files(
            &TemplateVars {
                project_name: "provenance-app",
                crate_name: "provenance_app",
                autumn_version: env!("CARGO_PKG_VERSION"),
                rust_version: option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("1.88.0"),
            },
            GenerateOptions::default(),
        )
        .keys()
        {
            assert!(
                manifest.digests.contains_key(*path),
                "no baseline recorded for {path}"
            );
        }
    }

    #[test]
    fn the_recorded_manifest_records_the_options_the_project_was_made_with() {
        use crate::upgrade::scaffold::Manifest;

        let tmp = TempDir::new().unwrap();
        let opts = GenerateOptions {
            with_api: true,
            with_i18n: true,
            ..GenerateOptions::default()
        };
        generate_with("api-provenance", tmp.path(), opts).unwrap();
        let manifest = Manifest::load(&tmp.path().join("api-provenance")).unwrap();
        assert_eq!(manifest.options, opts);
    }

    #[test]
    fn a_new_project_reports_no_scaffold_drift() {
        // The tightest guarantee available: what `autumn new` writes today is
        // exactly what `autumn upgrade` calls current.
        use crate::upgrade::scaffold;

        let tmp = TempDir::new().unwrap();
        generate("fresh-app", tmp.path()).unwrap();
        let report = scaffold::plan(&tmp.path().join("fresh-app"), env!("CARGO_PKG_VERSION"));
        assert!(!report.drifted(), "{}", scaffold::render_text(&report));
    }

    #[test]
    fn the_scaffold_manifest_is_committed_not_ignored() {
        // A manifest that git ignores is a manifest that never reaches the
        // next checkout, which is the only place it has any value.
        let tmp = TempDir::new().unwrap();
        generate("committed-app", tmp.path()).unwrap();
        let gitignore = fs::read_to_string(tmp.path().join("committed-app/.gitignore")).unwrap();
        assert!(!gitignore.contains(".autumn"), "{gitignore}");
    }
}
