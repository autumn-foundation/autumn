//! `autumn generate mailer` — scaffold a `#[mailer]` struct, HTML+text templates,
//! preview registration, and a smoke test.
//!
//! For a name like `Welcome`, the generator produces:
//! - `templates/mailers/_layout.html` / `_layout.txt` — shared email layout shell
//!   (created once; subsequent `generate mailer` calls skip if already present).
//! - `src/mailers/welcome.rs` — `WelcomeMailer` struct with a `#[mailer]` impl.
//!   The macro generates `send_welcome` (async) and `deliver_later_welcome`
//!   (fire-and-forget) from each method.
//! - `src/mailers/previews/welcome.rs` — dev-only `#[mailer_preview]` impl,
//!   kept separate from production code (mirrors Rails `test/mailers/previews/`).
//! - `src/mailers/previews/mod.rs` — created or updated with `pub mod welcome;`.
//! - `templates/mailers/welcome.html` — HTML body fragment (no `<head>`/`<body>`).
//! - `templates/mailers/welcome.txt` — plain-text body fragment.
//! - `src/mailers/mod.rs` — created or updated with `pub mod welcome;` and
//!   `pub mod previews;`.
//! - `src/main.rs` — `mod mailers;` declaration and
//!   `.mail_previews(mail_previews![mailers::welcome::WelcomeMailer])` wired
//!   into the app builder chain.
//! - `Cargo.toml` — `"mail"` feature added to the `autumn-web` dependency.
//!
//! Use `--no-layout` to opt out of the shared layout for a specific mailer.

use std::path::{Path, PathBuf};

use super::emit::Plan;
use super::model::validate_resource_name;
use super::naming::{pascal, snake};
use super::schema_edit::{
    add_mail_preview_to_app, add_mod_declaration, ensure_autumn_web_feature, update_main_rs,
};
use super::{GenerateError, ensure_project_root, read_or_empty, timestamp_now};

/// Compute the file actions for `autumn generate mailer`.
///
/// When `list_unsubscribe` is `Some(scope)`, the mailer opts into RFC 8058
/// one-click List-Unsubscribe and a `mail_unsubscribes` suppression migration
/// is added (idempotently).
///
/// When `no_layout` is `true`, the per-mailer template is emitted as a
/// self-contained full HTML document and the generated mailer omits the
/// `.layout(...)` builder call — useful for one-line plaintext or fully-custom
/// HTML. When `false` (the default), a shared `_layout.html`/`_layout.txt` is
/// created (idempotently) and the generated mailer composes the body fragment
/// into the layout slot at build time.
///
/// # Errors
/// Project layout and name validation errors surface here.
#[allow(
    clippy::too_many_lines,
    reason = "linear sequence of independent file/revert steps mirroring the files this \
              generator emits; splitting it up would not make any single step clearer"
)]
pub fn plan_mailer(
    project_root: &Path,
    name: &str,
    list_unsubscribe: Option<&str>,
    no_layout: bool,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;
    if let Some(scope) = list_unsubscribe {
        validate_list_unsubscribe_scope(scope)?;
    }

    let snake_name = snake(name);
    let pascal_name = pascal(name);
    let struct_name = format!("{pascal_name}Mailer");
    let mailer_type = format!("mailers::{snake_name}::{struct_name}");

    let mut plan = Plan::new(project_root);

    // ── templates/mailers/_layout.html (idempotent) ────────────────────────
    // Created on the first `generate mailer` call; subsequent calls skip it so
    // user edits to the shared layout are not overwritten. Uses create_if_absent
    // so concurrent generator runs are safe (exclusive-create, no TOCTOU race).
    if !no_layout {
        plan.create_if_absent(
            project_root
                .join("templates")
                .join("mailers")
                .join("_layout.html"),
            render_layout_html(),
        );

        plan.create_if_absent(
            project_root
                .join("templates")
                .join("mailers")
                .join("_layout.txt"),
            render_layout_txt(),
        );
    }

    // ── src/mailers/<snake>.rs ─────────────────────────────────────────────
    plan.create(
        project_root
            .join("src")
            .join("mailers")
            .join(format!("{snake_name}.rs")),
        render_mailer_file(&struct_name, &snake_name, list_unsubscribe, no_layout),
    );

    // ── templates/mailers/<snake>.html ─────────────────────────────────────
    plan.create(
        project_root
            .join("templates")
            .join("mailers")
            .join(format!("{snake_name}.html")),
        render_html_template(&struct_name, no_layout),
    );

    // ── templates/mailers/<snake>.txt ──────────────────────────────────────
    plan.create(
        project_root
            .join("templates")
            .join("mailers")
            .join(format!("{snake_name}.txt")),
        render_txt_template(&struct_name, no_layout),
    );

    // ── src/mailers/previews/<snake>.rs ────────────────────────────────────
    plan.create(
        project_root
            .join("src")
            .join("mailers")
            .join("previews")
            .join(format!("{snake_name}.rs")),
        render_preview_file(&struct_name, &snake_name, no_layout),
    );

    // ── src/mailers/previews/mod.rs (create or update) ─────────────────────
    let previews_mod_path = project_root
        .join("src")
        .join("mailers")
        .join("previews")
        .join("mod.rs");
    plan.modify(
        previews_mod_path.clone(),
        add_mod_declaration(&read_or_empty(&previews_mod_path), &snake_name),
    );
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: previews_mod_path,
        name: snake_name.clone(),
    });

    // ── src/mailers/mod.rs (create or update) ──────────────────────────────
    // The "previews" sub-module declared here is shared by every generated
    // mailer, not owned by this one — `emit::sync_mod_declarations_in`
    // removes it once `src/mailers/previews/mod.rs` no longer exists, so no
    // static revert is pushed for it here.
    let mod_path = project_root.join("src").join("mailers").join("mod.rs");
    let with_mailer_mod = add_mod_declaration(&read_or_empty(&mod_path), &snake_name);
    let with_both_mods = add_mod_declaration(&with_mailer_mod, "previews");
    plan.modify(mod_path.clone(), with_both_mods);
    plan.push_revert(crate::generate::emit::Revert::ModDecl {
        path: mod_path,
        name: snake_name,
    });

    // ── src/main.rs: add mod mailers; and .mail_previews(…) ────────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|e| {
        GenerateError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", main_path.display()),
        ))
    })?;
    let with_mods = update_main_rs(&main_existing, &["mailers"], &[]);
    let updated_main = add_mail_preview_to_app(&with_mods, &mailer_type);
    plan.modify(main_path.clone(), updated_main);
    plan.push_revert(crate::generate::emit::Revert::MailPreview {
        path: main_path,
        mailer_type,
    });

    // ── Cargo.toml: ensure autumn-web has the "mail" feature ───────────────
    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = read_or_empty(&cargo_path);
    let updated_cargo = ensure_autumn_web_feature(&cargo_existing, "mail");
    if updated_cargo != cargo_existing {
        plan.modify(cargo_path.clone(), updated_cargo);
    }
    plan.push_revert(crate::generate::emit::Revert::CargoAutumnWebFeature {
        path: cargo_path,
        feature: "mail".to_owned(),
        owner_dir: Some(project_root.join("src").join("mailers")),
    });

    // ── migrations/<ts>_create_mail_unsubscribes (opt-in, idempotent) ──────
    if list_unsubscribe.is_some() {
        plan_unsubscribe_migration(project_root, &mut plan);
    }

    Ok(plan)
}

/// Validate a `--list-unsubscribe` scope. Restricted to a safe identifier-like
/// charset so it can be embedded verbatim in a generated Rust string literal and
/// used as a stable logical list id (DB key, token claim) without escaping.
fn validate_list_unsubscribe_scope(scope: &str) -> Result<(), GenerateError> {
    if scope.is_empty() {
        return Err(GenerateError::InvalidName(
            scope.to_owned(),
            "list_unsubscribe scope cannot be empty".into(),
        ));
    }
    if !scope
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(GenerateError::InvalidName(
            scope.to_owned(),
            "list_unsubscribe scope may only contain ASCII letters, digits, '_' or '-'".into(),
        ));
    }
    Ok(())
}

/// Add the `mail_unsubscribes` suppression migration, reusing the existing
/// one (by its real on-disk path) if a prior mailer already created it.
///
/// The old design skipped pushing any action at all once a
/// `*_create_mail_unsubscribes` directory existed — correct for `generate`,
/// but it meant a second `--list-unsubscribe` mailer's plan carried no
/// migration action, so `autumn destroy` recomputing that same plan could
/// never locate (and thus never remove) the suppression migration. Instead,
/// always push `create_if_absent` actions, targeting the *existing*
/// directory's real path when one is already on disk (so `generate` is
/// still a silent no-op there) and a fresh timestamp only when none exists
/// yet. Either way the action is present for `autumn destroy`'s
/// suffix-based migration matching (`resolve_migration_removal`) to find.
fn plan_unsubscribe_migration(project_root: &Path, plan: &mut Plan) {
    let migrations_dir = project_root.join("migrations");
    let dir = existing_mail_unsubscribes_dir(&migrations_dir).unwrap_or_else(|| {
        migrations_dir.join(format!("{}_create_mail_unsubscribes", timestamp_now()))
    });
    plan.create_if_absent(dir.join("up.sql"), UNSUBSCRIBE_MIGRATION_UP.to_owned());
    plan.create_if_absent(dir.join("down.sql"), UNSUBSCRIBE_MIGRATION_DOWN.to_owned());
}

/// The real on-disk `*_create_mail_unsubscribes` migration directory, if one
/// already exists (from a previous `--list-unsubscribe` mailer).
fn existing_mail_unsubscribes_dir(migrations_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(migrations_dir).ok()?;
    entries.filter_map(Result::ok).map(|e| e.path()).find(|p| {
        p.is_dir()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with("_create_mail_unsubscribes"))
    })
}

const UNSUBSCRIBE_MIGRATION_UP: &str = "\
-- Suppression list for RFC 8058 List-Unsubscribe.
-- Keyed by (subscriber, list_id, unsubscribed_at); send-time checks skip any
-- recipient with a matching (subscriber, list_id) row.
CREATE TABLE mail_unsubscribes (
    id BIGSERIAL PRIMARY KEY,
    subscriber TEXT NOT NULL,
    list_id TEXT NOT NULL,
    unsubscribed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (subscriber, list_id)
);
";

const UNSUBSCRIBE_MIGRATION_DOWN: &str = "DROP TABLE mail_unsubscribes;\n";

fn render_mailer_file(
    struct_name: &str,
    snake_name: &str,
    list_unsubscribe: Option<&str>,
    no_layout: bool,
) -> String {
    let mailer_attr = list_unsubscribe.map_or_else(
        || "#[mailer]".to_owned(),
        |scope| format!("#[mailer(list_unsubscribe = \"{scope}\")]"),
    );
    let layout_call = if no_layout {
        String::new()
    } else {
        "\n            .layout(\n                include_str!(\"../../templates/mailers/_layout.html\"),\n                include_str!(\"../../templates/mailers/_layout.txt\"),\n            )".to_owned()
    };
    let base = format!(
        r#"//! Generated by `autumn generate mailer`.
//!
//! Edit freely — once generated, this is ordinary user code.
//!
//! The `#[mailer]` macro generates two helpers for every method:
//!   - `send_{snake_name}` — async, awaits delivery.
//!   - `deliver_later_{snake_name}` — fire-and-forget (uses `deliver_later` semantics).
//!
//! Dev-only preview fixtures live in `src/mailers/previews/{snake_name}.rs`.

use autumn_web::prelude::*;

pub struct {struct_name};

{mailer_attr}
impl {struct_name} {{
    pub fn {snake_name}(&self, to: String) -> Mail {{
        Mail::builder()
            .to(to)
            .subject("{struct_name}")
            .html(include_str!("../../templates/mailers/{snake_name}.html"))
            .text(include_str!("../../templates/mailers/{snake_name}.txt")){layout_call}
            .build()
            .expect("valid mail")
    }}
}}
"#
    );
    format!(
        "{}{}",
        base,
        render_smoke_test(struct_name, snake_name, no_layout)
    )
}

fn render_preview_file(struct_name: &str, snake_name: &str, no_layout: bool) -> String {
    let layout_call = if no_layout {
        String::new()
    } else {
        "\n            .layout(\n                include_str!(\"../../../templates/mailers/_layout.html\"),\n                include_str!(\"../../../templates/mailers/_layout.txt\"),\n            )".to_owned()
    };
    format!(
        r#"//! Dev-only mail preview for [`{struct_name}`].
//!
//! Generated by `autumn generate mailer`. Edit freely.
//!
//! The `#[mailer_preview]` macro exposes preview fixtures to the Autumn
//! dev UI at `/_autumn/mail`. Methods must be zero-argument and return `Mail`.

use autumn_web::prelude::*;
use crate::mailers::{snake_name}::{struct_name};

#[mailer_preview]
impl {struct_name} {{
    fn {snake_name}_preview() -> Mail {{
        Mail::builder()
            .to("preview@example.com")
            .subject("{struct_name}")
            .html(include_str!("../../../templates/mailers/{snake_name}.html"))
            .text(include_str!("../../../templates/mailers/{snake_name}.txt")){layout_call}
            .build()
            .expect("valid preview mail")
    }}
}}
"#
    )
}

/// Shared HTML layout shell. Contains the document skeleton (charset, viewport,
/// table-based responsive wrapper with inline CSS, header, body slot, footer).
/// Per-mailer templates are body-only fragments composed into `{{ content }}`.
fn render_layout_html() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="X-UA-Compatible" content="IE=edge">
  <title></title>
</head>
<body style="margin:0;padding:0;background-color:#f4f4f4;font-family:Arial,Helvetica,sans-serif;">
  <table role="presentation" width="100%" cellspacing="0" cellpadding="0" border="0" style="background-color:#f4f4f4;">
    <tr>
      <td align="center" style="padding:24px 0;">
        <!-- Header / branding -->
        <table role="presentation" width="600" cellspacing="0" cellpadding="0" border="0" style="max-width:600px;width:100%;background-color:#ffffff;border-radius:6px;overflow:hidden;">
          <tr>
            <td style="background-color:#1a1a2e;padding:24px 32px;">
              <p style="margin:0;color:#ffffff;font-size:20px;font-weight:bold;">Your App</p>
            </td>
          </tr>
          <!-- Body content slot -->
          <tr>
            <td style="padding:32px;">
              {{ content }}
            </td>
          </tr>
          <!-- Footer -->
          <tr>
            <td style="background-color:#f4f4f4;padding:16px 32px;border-top:1px solid #e0e0e0;">
              <p style="margin:0;font-size:12px;color:#888888;text-align:center;">
                You are receiving this email because you signed up for our service.
              </p>
            </td>
          </tr>
        </table>
      </td>
    </tr>
  </table>
</body>
</html>
"#
    .to_owned()
}

/// Shared plain-text layout shell.
fn render_layout_txt() -> String {
    "========================================\n\
     Your App\n\
     ========================================\n\
     \n\
     {{ content }}\n\
     \n\
     ----------------------------------------\n\
     You are receiving this email because you signed up for our service.\n"
        .to_owned()
}

/// Per-mailer HTML body fragment. No `<!DOCTYPE>`, `<head>`, or `<body>` —
/// those belong in `_layout.html`. When `no_layout` is `true` a self-contained
/// full document is emitted instead.
fn render_html_template(struct_name: &str, no_layout: bool) -> String {
    if no_layout {
        format!(
            "<!DOCTYPE html>\n\
             <html>\n\
             <head><meta charset=\"utf-8\"></head>\n\
             <body>\n\
               <p>Hello from {struct_name}!</p>\n\
             </body>\n\
             </html>\n"
        )
    } else {
        format!(
            "<h1 style=\"margin:0 0 16px;font-size:24px;color:#1a1a2e;\">Hello from {struct_name}!</h1>\n\
             <p style=\"margin:0;font-size:16px;line-height:1.6;color:#333333;\">Your email body goes here.</p>\n"
        )
    }
}

/// Per-mailer plain-text body fragment. When `no_layout` is `true` a
/// self-contained message is emitted instead.
fn render_txt_template(struct_name: &str, no_layout: bool) -> String {
    if no_layout {
        format!("Hello from {struct_name}!\n")
    } else {
        format!("Hello from {struct_name}!\n\nYour email body goes here.\n")
    }
}

fn render_smoke_test(struct_name: &str, snake_name: &str, no_layout: bool) -> String {
    let layout_assertions = if no_layout {
        String::new()
    } else {
        // Assert the layout {{ content }} marker was replaced (composition
        // happened) without asserting the default layout's HTML structure —
        // users may customize _layout.html to use divs, custom CSS, etc.
        r#"                assert!(
                    !html.contains("{{ content }}"),
                    "layout marker must be replaced after composition; got: {html}"
                );
"#
        .to_owned()
    };
    format!(
        "\n\
         #[cfg(test)]\n\
         mod {snake_name}_mailer_tests {{\n\
             use super::{struct_name};\n\
         \n\
             #[test]\n\
             fn {snake_name}_mailer_renders_both_bodies() {{\n\
                 let mailer_instance = {struct_name};\n\
                 let mail = mailer_instance.{snake_name}(\"test@example.com\".to_owned());\n\
                 let html = mail.html.expect(\"html body should be set\");\n\
                 let text = mail.text.expect(\"text body should be set\");\n\
                 assert!(\n\
                     html.contains(\"{struct_name}\"),\n\
                     \"html body must contain the mailer name; got: {{html}}\"\n\
                 );\n\
                 assert!(\n\
                     text.contains(\"{struct_name}\"),\n\
                     \"text body must contain the mailer name; got: {{text}}\"\n\
                 );\n\
                 {layout_assertions}\
             }}\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use tempfile::TempDir;

    fn project_with_main(main_content: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), main_content).unwrap();
        tmp
    }

    fn default_main() -> &'static str {
        r#"use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str { "ok" }

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index])
        .run()
        .await;
}
"#
    }

    #[test]
    fn generate_then_destroy_mailer_round_trips_to_original_project_state() {
        let tmp = project_with_main(default_main());
        let cargo_path = tmp.path().join("Cargo.toml");
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(tmp.path().join("src/mailers/welcome.rs").exists());
        assert!(
            fs::read_to_string(&main_path)
                .unwrap()
                .contains("mail_previews!")
        );
        assert!(
            fs::read_to_string(&cargo_path)
                .unwrap()
                .contains("\"mail\"")
        );

        let destroy_plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(!tmp.path().join("src/mailers/welcome.rs").exists());
        assert!(!tmp.path().join("src/mailers/mod.rs").exists());
        assert!(!tmp.path().join("src/mailers/previews").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn destroying_one_of_two_mailers_keeps_shared_mail_feature_the_other_still_needs() {
        let tmp = project_with_main(default_main());
        let cargo_path = tmp.path().join("Cargo.toml");

        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_mailer(tmp.path(), "Goodbye", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            fs::read_to_string(&cargo_path)
                .unwrap()
                .contains("\"mail\"")
        );

        // Destroying Welcome alone must NOT strip the "mail" feature —
        // Goodbye's mailer still needs it.
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/mailers/welcome.rs").exists());
        assert!(tmp.path().join("src/mailers/goodbye.rs").exists());
        let cargo_after = fs::read_to_string(&cargo_path).unwrap();
        assert!(
            cargo_after.contains("\"mail\""),
            "mail feature must survive — Goodbye's mailer still uses it: {cargo_after}"
        );

        // Now destroy the last remaining mailer — the feature must finally go.
        plan_mailer(tmp.path(), "Goodbye", None, false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();
        assert!(!tmp.path().join("src/mailers/goodbye.rs").exists());
        assert!(
            !fs::read_to_string(&cargo_path)
                .unwrap()
                .contains("\"mail\""),
            "mail feature must be removed once no mailer uses it anymore"
        );
    }

    #[test]
    fn destroying_one_of_two_mailers_keeps_shared_layout_the_other_still_includes() {
        let tmp = project_with_main(default_main());
        let layout_html = tmp.path().join("templates/mailers/_layout.html");
        let layout_txt = tmp.path().join("templates/mailers/_layout.txt");

        // Welcome creates the shared layout files (create_if_absent);
        // Goodbye's own plan carries the SAME create_if_absent actions
        // (they're silently skipped at execute time since the files already
        // exist), so both mailers' plans mention them.
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_mailer(tmp.path(), "Goodbye", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(layout_html.exists());
        assert!(layout_txt.exists());

        // Destroying Welcome alone must NOT delete the shared layout —
        // Goodbye's mailer/preview files still `include_str!` it.
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/mailers/welcome.rs").exists());
        assert!(
            layout_html.exists(),
            "shared _layout.html must survive — Goodbye's mailer still includes it"
        );
        assert!(
            layout_txt.exists(),
            "shared _layout.txt must survive — Goodbye's mailer still includes it"
        );

        // Now destroy the last remaining mailer — the shared layout must
        // finally go too.
        plan_mailer(tmp.path(), "Goodbye", None, false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();
        assert!(!layout_html.exists());
        assert!(!layout_txt.exists());
    }

    // ── RED: file plan assertions ─────────────────────────────────────────

    #[test]
    fn plan_creates_mailer_file() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/mailers/welcome.rs")),
            "plan must include src/mailers/welcome.rs"
        );
    }

    #[test]
    fn plan_creates_preview_file() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/mailers/previews/welcome.rs")),
            "plan must include src/mailers/previews/welcome.rs"
        );
    }

    #[test]
    fn plan_includes_previews_mod_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/mailers/previews/mod.rs")),
            "plan must include src/mailers/previews/mod.rs"
        );
    }

    #[test]
    fn plan_creates_html_template() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("templates/mailers/welcome.html")),
            "plan must include templates/mailers/welcome.html"
        );
    }

    #[test]
    fn plan_creates_txt_template() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("templates/mailers/welcome.txt")),
            "plan must include templates/mailers/welcome.txt"
        );
    }

    #[test]
    fn plan_includes_mailer_mod_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/mailers/mod.rs")),
            "plan must include src/mailers/mod.rs"
        );
    }

    #[test]
    fn plan_does_not_create_external_smoke_test() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            !plan.actions.iter().any(|a| a
                .path()
                .to_string_lossy()
                .contains("tests/welcome_mailer.rs")),
            "plan must not include tests/welcome_mailer.rs"
        );
    }

    #[test]
    fn plan_updates_main_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/main.rs")),
            "plan must update src/main.rs"
        );
    }

    // ── List-Unsubscribe scaffolding ──────────────────────────────────────

    #[test]
    fn list_unsubscribe_sets_attribute_and_plans_migration() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "WeeklyDigest", Some("weekly_digest"), false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/weekly_digest.rs")).unwrap();
        assert!(
            mailer.contains("#[mailer(list_unsubscribe = \"weekly_digest\")]"),
            "mailer must carry the list_unsubscribe attribute: {mailer}"
        );

        // Exactly one migration directory, with the expected SQL.
        let migration_dir = fs::read_dir(tmp.path().join("migrations"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with("_create_mail_unsubscribes"))
            })
            .expect("a mail_unsubscribes migration must be generated");
        let up = fs::read_to_string(migration_dir.path().join("up.sql")).unwrap();
        assert!(up.contains("CREATE TABLE mail_unsubscribes"));
        assert!(up.contains("UNIQUE (subscriber, list_id)"));
        let down = fs::read_to_string(migration_dir.path().join("down.sql")).unwrap();
        assert!(down.contains("DROP TABLE mail_unsubscribes"));
    }

    #[test]
    fn rejects_unsafe_list_unsubscribe_scope() {
        let tmp = project_with_main(default_main());
        // A scope with a quote would inject invalid Rust into the attribute.
        assert!(plan_mailer(tmp.path(), "Welcome", Some("a\" )]"), false).is_err());
        assert!(plan_mailer(tmp.path(), "Welcome", Some("with space"), false).is_err());
        assert!(plan_mailer(tmp.path(), "Welcome", Some(""), false).is_err());
        // Identifier-like scopes are accepted.
        assert!(plan_mailer(tmp.path(), "Welcome", Some("weekly_digest"), false).is_ok());
        assert!(plan_mailer(tmp.path(), "Welcome", Some("product-updates"), false).is_ok());
    }

    #[test]
    fn rejects_unicode_and_special_chars_in_list_unsubscribe_scope() {
        let tmp = project_with_main(default_main());
        // Unicode characters are not allowed (alphanumeric ASCII + _ + - only).
        assert!(plan_mailer(tmp.path(), "Welcome", Some("wöchentlich"), false).is_err());
        assert!(plan_mailer(tmp.path(), "Welcome", Some("list.name"), false).is_err());
        assert!(plan_mailer(tmp.path(), "Welcome", Some("list/name"), false).is_err());
        // Dash and underscore are fine.
        assert!(plan_mailer(tmp.path(), "Welcome", Some("a-b_c"), false).is_ok());
    }

    #[test]
    fn without_list_unsubscribe_no_migration_and_plain_attribute() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| a.path().to_string_lossy().contains("mail_unsubscribes")),
            "no migration without --list-unsubscribe"
        );
    }

    #[test]
    fn second_list_unsubscribe_mailer_does_not_duplicate_migration() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "WeeklyDigest", Some("weekly_digest"), false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let migrations_dir = tmp.path().join("migrations");
        let existing_dirs = || {
            fs::read_dir(&migrations_dir)
                .unwrap()
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .count()
        };
        assert_eq!(existing_dirs(), 1, "first mailer creates one migration dir");
        let original_dir = fs::read_dir(&migrations_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .unwrap();

        // A second list mailer must reuse the existing suppression table:
        // its plan still carries the migration's actions (so `autumn
        // destroy` can find it later), but they all target the SAME
        // already-existing directory, and executing must not create a
        // second migration directory or error.
        let second =
            plan_mailer(tmp.path(), "ProductUpdates", Some("product_updates"), false).unwrap();
        let migration_paths: Vec<_> = second
            .actions
            .iter()
            .map(super::super::emit::Action::path)
            .filter(|p| p.to_string_lossy().contains("mail_unsubscribes"))
            .collect();
        assert!(
            !migration_paths.is_empty(),
            "the plan must still carry the migration's actions so `autumn destroy` can find it"
        );
        for path in &migration_paths {
            assert_eq!(
                path.parent().unwrap(),
                original_dir,
                "migration action must target the existing on-disk directory, not a fresh timestamp"
            );
        }

        second.execute(Flags::default()).unwrap();
        assert_eq!(
            existing_dirs(),
            1,
            "must not plan or create a duplicate suppression migration"
        );
    }

    #[test]
    fn destroying_one_of_two_list_unsubscribe_mailers_keeps_the_shared_migration() {
        let tmp = project_with_main(default_main());
        let migrations_dir = tmp.path().join("migrations");
        let migration_dir_exists =
            || fs::read_dir(&migrations_dir).is_ok_and(|mut d| d.next().is_some());

        plan_mailer(tmp.path(), "WeeklyDigest", Some("weekly_digest"), false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_mailer(tmp.path(), "ProductUpdates", Some("product_updates"), false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(migration_dir_exists());

        // Destroying WeeklyDigest alone must NOT remove the shared
        // mail_unsubscribes migration — ProductUpdates still opts into
        // --list-unsubscribe and needs the suppression table.
        plan_mailer(tmp.path(), "WeeklyDigest", Some("weekly_digest"), false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("src/mailers/weekly_digest.rs").exists());
        assert!(tmp.path().join("src/mailers/product_updates.rs").exists());
        assert!(
            migration_dir_exists(),
            "shared mail_unsubscribes migration must survive — ProductUpdates still needs it"
        );

        // Now destroy the last remaining list-unsubscribe mailer — the
        // migration must finally go too.
        plan_mailer(tmp.path(), "ProductUpdates", Some("product_updates"), false)
            .unwrap()
            .revert(Flags::default())
            .unwrap();
        assert!(!tmp.path().join("src/mailers/product_updates.rs").exists());
        assert!(
            !migration_dir_exists(),
            "mail_unsubscribes migration must be removed once no mailer uses list_unsubscribe anymore"
        );
    }

    // ── GREEN: execute and inspect written content ────────────────────────

    #[test]
    fn execute_writes_mailer_struct_and_macro_annotations() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        assert!(
            mailer.contains("pub struct WelcomeMailer"),
            "mailer file must define the struct"
        );
        assert!(
            mailer.contains("#[mailer]"),
            "must have #[mailer] attribute"
        );
        assert!(
            !mailer.contains("#[mailer_preview]"),
            "mailer file must NOT contain #[mailer_preview] — it lives in previews/"
        );

        let preview =
            fs::read_to_string(tmp.path().join("src/mailers/previews/welcome.rs")).unwrap();
        assert!(
            preview.contains("#[mailer_preview]"),
            "preview file must have #[mailer_preview] attribute"
        );
    }

    #[test]
    fn execute_mailer_has_deliver_later_capable_method() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        assert!(
            mailer.contains("pub fn welcome("),
            "#[mailer] impl must expose a method; the macro generates deliver_later_welcome from it"
        );
        assert!(
            mailer.contains("deliver_later"),
            "the generated comment must describe the deliver_later API"
        );
    }

    #[test]
    fn execute_writes_html_template_with_expected_content() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let html = fs::read_to_string(tmp.path().join("templates/mailers/welcome.html")).unwrap();
        assert!(
            html.contains("WelcomeMailer"),
            "html template must reference the mailer name"
        );
        // With layout, per-mailer template is body-only; full document is in _layout.html.
        assert!(!html.is_empty(), "html template must not be empty");
    }

    #[test]
    fn execute_writes_txt_template_with_expected_content() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let txt = fs::read_to_string(tmp.path().join("templates/mailers/welcome.txt")).unwrap();
        assert!(
            txt.contains("WelcomeMailer"),
            "text template must reference the mailer name"
        );
        assert!(!txt.trim().is_empty(), "text template must not be empty");
    }

    #[test]
    fn execute_writes_preview_file_with_mailer_preview() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let preview =
            fs::read_to_string(tmp.path().join("src/mailers/previews/welcome.rs")).unwrap();
        assert!(
            preview.contains("#[mailer_preview]"),
            "preview file must have #[mailer_preview] attribute"
        );
        assert!(
            preview.contains("welcome_preview"),
            "preview file must define a preview method"
        );
        assert!(
            preview.contains("../../../templates/mailers/welcome.html"),
            "preview include_str! path must be three levels up from previews/"
        );
    }

    #[test]
    fn execute_updates_mailer_mod_rs() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/mailers/mod.rs")).unwrap();
        assert!(
            mod_rs.contains("pub mod welcome;"),
            "mod.rs must declare pub mod welcome"
        );
        assert!(
            mod_rs.contains("pub mod previews;"),
            "mod.rs must declare pub mod previews"
        );

        let previews_mod =
            fs::read_to_string(tmp.path().join("src/mailers/previews/mod.rs")).unwrap();
        assert!(
            previews_mod.contains("pub mod welcome;"),
            "previews/mod.rs must declare pub mod welcome"
        );
    }

    #[test]
    fn execute_updates_main_rs_with_mod_declaration() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("mod mailers;"),
            "main.rs must declare mod mailers"
        );
    }

    #[test]
    fn execute_updates_main_rs_with_mail_previews_call() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("mail_previews!["),
            "main.rs must include a mail_previews![] call"
        );
        assert!(
            main.contains("mailers::welcome::WelcomeMailer"),
            "main.rs must reference the generated mailer type"
        );
    }

    #[test]
    fn execute_writes_smoke_test_with_body_assertions() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let test = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        assert!(
            test.contains("WelcomeMailer"),
            "smoke test must reference the mailer struct"
        );
        assert!(
            test.contains("welcome_mailer_renders_both_bodies")
                || test.contains("renders_both_bodies"),
            "smoke test must include a renders-both-bodies test function"
        );
        assert!(
            test.contains("html.contains") || test.contains("html body"),
            "smoke test must assert the html body"
        );
        assert!(
            test.contains("text.contains") || test.contains("text body"),
            "smoke test must assert the text body"
        );
        assert!(
            test.contains("WelcomeMailer"),
            "smoke test body assertion must check for the mailer name"
        );
        assert!(
            !test.contains("cfg(feature = \"mail\")"),
            "smoke test must not be gated by cfg(feature = \"mail\")"
        );
    }

    #[test]
    fn execute_adds_mail_feature_to_cargo_toml() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("\"mail\""),
            "Cargo.toml must include the mail feature for autumn-web: {cargo}"
        );
    }

    // ── Flag behaviour ────────────────────────────────────────────────────

    #[test]
    fn dry_run_writes_no_new_files() {
        let tmp = project_with_main(default_main());
        let original_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();

        assert!(
            !tmp.path().join("src/mailers/welcome.rs").exists(),
            "dry run must not write the mailer file"
        );
        assert!(
            !tmp.path().join("src/mailers/previews/welcome.rs").exists(),
            "dry run must not write the preview file"
        );
        assert!(
            !tmp.path().join("templates/mailers/welcome.html").exists(),
            "dry run must not write html template"
        );
        assert!(
            !tmp.path().join("templates/mailers/welcome.txt").exists(),
            "dry run must not write txt template"
        );
        let main_after = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(
            original_main, main_after,
            "dry run must not modify src/main.rs"
        );
    }

    #[test]
    fn collision_without_force_returns_error() {
        let tmp = project_with_main(default_main());
        fs::create_dir_all(tmp.path().join("src/mailers")).unwrap();
        fs::write(tmp.path().join("src/mailers/welcome.rs"), "// existing").unwrap();
        let err = plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap_err();
        assert!(
            matches!(err, GenerateError::Collisions(_)),
            "should return collision error; got {err:?}"
        );
    }

    #[test]
    fn force_overwrites_existing_mailer() {
        let tmp = project_with_main(default_main());
        fs::create_dir_all(tmp.path().join("src/mailers")).unwrap();
        fs::write(tmp.path().join("src/mailers/welcome.rs"), "// old").unwrap();
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        assert!(
            mailer.contains("WelcomeMailer"),
            "force must regenerate the mailer file"
        );
    }

    // ── Name normalisation ────────────────────────────────────────────────

    #[test]
    fn snake_case_input_is_normalised() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "welcome_email", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        assert!(tmp.path().join("src/mailers/welcome_email.rs").exists());
        let content = fs::read_to_string(tmp.path().join("src/mailers/welcome_email.rs")).unwrap();
        assert!(
            content.contains("WelcomeEmailMailer"),
            "PascalCase struct must include both words"
        );
    }

    // ── Error conditions ──────────────────────────────────────────────────

    #[test]
    fn plan_errors_when_main_rs_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let err = plan_mailer(tmp.path(), "Welcome", None, false).unwrap_err();
        assert!(matches!(err, GenerateError::Io(_)));
    }

    #[test]
    fn plan_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_mailer(tmp.path(), "Welcome", None, false).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn second_mailer_augments_mod_rs_and_previews() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_mailer(tmp.path(), "Notification", None, false)
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/mailers/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod welcome;"));
        assert!(mod_rs.contains("pub mod notification;"));
        assert!(
            mod_rs.contains("pub mod previews;"),
            "previews module must appear once"
        );
        assert_eq!(
            mod_rs.matches("pub mod previews;").count(),
            1,
            "pub mod previews; must appear exactly once (idempotent)"
        );

        let previews_mod =
            fs::read_to_string(tmp.path().join("src/mailers/previews/mod.rs")).unwrap();
        assert!(previews_mod.contains("pub mod welcome;"));
        assert!(previews_mod.contains("pub mod notification;"));

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mailers::welcome::WelcomeMailer"));
        assert!(main.contains("mailers::notification::NotificationMailer"));
    }

    // ── Shared layout: RED tests ──────────────────────────────────────────

    #[test]
    fn plan_creates_layout_html() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("templates/mailers/_layout.html")),
            "plan must include templates/mailers/_layout.html"
        );
    }

    #[test]
    fn plan_creates_layout_txt() {
        let tmp = project_with_main(default_main());
        let plan = plan_mailer(tmp.path(), "Welcome", None, false).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("templates/mailers/_layout.txt")),
            "plan must include templates/mailers/_layout.txt"
        );
    }

    #[test]
    fn layout_html_contains_table_and_style_and_content_marker() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let layout = fs::read_to_string(tmp.path().join("templates/mailers/_layout.html")).unwrap();
        assert!(
            layout.contains("<table"),
            "_layout.html must contain a table-based wrapper"
        );
        assert!(
            layout.contains("style="),
            "_layout.html must use inline style= attributes"
        );
        assert!(
            layout.contains("{{ content }}"),
            "_layout.html must contain the content slot marker"
        );
        assert!(
            layout.contains("<!DOCTYPE html>"),
            "_layout.html must be a complete document shell"
        );
    }

    #[test]
    fn layout_txt_contains_content_marker() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let layout = fs::read_to_string(tmp.path().join("templates/mailers/_layout.txt")).unwrap();
        assert!(
            layout.contains("{{ content }}"),
            "_layout.txt must contain the content slot marker"
        );
    }

    #[test]
    fn per_mailer_html_template_is_body_fragment_only() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let html = fs::read_to_string(tmp.path().join("templates/mailers/welcome.html")).unwrap();
        assert!(
            !html.contains("<!DOCTYPE"),
            "per-mailer template must NOT contain <!DOCTYPE — that belongs in _layout.html"
        );
        assert!(
            !html.contains("<head"),
            "per-mailer template must NOT contain <head>"
        );
        assert!(
            !html.contains("<body"),
            "per-mailer template must NOT contain <body>"
        );
        assert!(
            html.contains("WelcomeMailer"),
            "per-mailer template must still reference the mailer name"
        );
    }

    #[test]
    fn second_mailer_does_not_recreate_layout_files() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        // Overwrite _layout.html with a sentinel to detect re-creation.
        let layout_path = tmp.path().join("templates/mailers/_layout.html");
        fs::write(&layout_path, "<!-- sentinel -->").unwrap();

        plan_mailer(tmp.path(), "Receipt", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        // The second generator run must NOT recreate the layout file.
        let layout = fs::read_to_string(&layout_path).unwrap();
        assert_eq!(
            layout, "<!-- sentinel -->",
            "second mailer generation must not overwrite _layout.html"
        );
    }

    #[test]
    fn second_mailer_template_has_no_doctype_or_head() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_mailer(tmp.path(), "Receipt", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let html = fs::read_to_string(tmp.path().join("templates/mailers/receipt.html")).unwrap();
        assert!(
            !html.contains("<!DOCTYPE"),
            "2nd mailer template must add 0 lines of <!DOCTYPE boilerplate"
        );
        assert!(
            !html.contains("<head"),
            "2nd mailer template must add 0 lines of <head> boilerplate"
        );
    }

    #[test]
    fn generated_mailer_file_includes_layout_builder_call() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        assert!(
            mailer.contains(".layout("),
            "generated mailer must call .layout(...) to compose body into shared layout"
        );
    }

    #[test]
    fn generated_preview_file_includes_layout_builder_call() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let preview =
            fs::read_to_string(tmp.path().join("src/mailers/previews/welcome.rs")).unwrap();
        assert!(
            preview.contains(".layout("),
            "generated preview must call .layout(...) to show layout-wrapped output"
        );
    }

    #[test]
    fn smoke_test_asserts_layout_composition() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, false)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        // The smoke test should assert that the {{ content }} marker was
        // replaced (composition happened), not that the default layout uses
        // tables — users may customize _layout.html to any structure.
        assert!(
            mailer.contains("{{ content }}"),
            "smoke test must assert the layout content marker was replaced"
        );
    }

    // ── --no-layout flag ──────────────────────────────────────────────────

    #[test]
    fn no_layout_flag_omits_layout_call_in_mailer() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mailer = fs::read_to_string(tmp.path().join("src/mailers/welcome.rs")).unwrap();
        assert!(
            !mailer.contains(".layout("),
            "--no-layout must not emit .layout() call"
        );
    }

    #[test]
    fn no_layout_flag_emits_full_document_html_template() {
        let tmp = project_with_main(default_main());
        plan_mailer(tmp.path(), "Welcome", None, true)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let html = fs::read_to_string(tmp.path().join("templates/mailers/welcome.html")).unwrap();
        assert!(
            html.contains("<!DOCTYPE html>"),
            "--no-layout template must be a self-contained full-document HTML"
        );
    }
}
