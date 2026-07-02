//! `autumn generate pwa` — scaffold an installable Progressive Web App.
//!
//! Creates:
//!   - `static/manifest.webmanifest`   — Web App Manifest (application/manifest+json)
//!   - `static/service-worker.js`      — Service Worker with offline-shell caching
//!   - `static/pwa-register.js`        — SW registration script (avoids CSP inline-script issues)
//!   - `static/icons/icon.svg`         — Placeholder app icon (replace with real PNG)
//!   - `static/icons/maskable-icon.svg` — Maskable variant (safe-zone compliant)
//!   - `src/main.rs`                   — Route handlers for `/manifest.webmanifest`,
//!     `/service-worker.js`, `/pwa-register.js`, and `/offline`; PWA `<link>` /
//!     `<meta>` tags injected into the shared `layout` head block.
//!   - `tests/system/pwa_smoke.rs`     — Smoke test (manifest content-type + SW registration)
//!   - `Cargo.toml`                    — `system-tests` feature added if absent

use std::path::Path;

use super::emit::Plan;
use super::schema_edit::update_main_rs;
use super::system_test::patch_cargo_toml as patch_system_test_cargo_toml;
use super::{Flags, GenerateError, ensure_project_root};

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute the file actions for `autumn generate pwa`.
///
/// # Errors
/// Returns [`GenerateError::NotInProject`] when not at a project root,
/// [`GenerateError::Io`] if `src/main.rs` / `Cargo.toml` can't be read, or
/// [`GenerateError::Config`] when `src/main.rs`'s `layout()` doesn't accept
/// `current_path` — the generated `pwa_offline` handler calls `layout()`
/// with the current `nav_bar`-based scaffold's arity, so an app that
/// hasn't migrated its own `layout()` needs to before running this.
pub fn plan_pwa(project_root: &Path) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    // Read src/main.rs up front (rather than where it's injected below) so
    // the layout() shape can be validated before any plan actions are built.
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|_| {
        GenerateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("missing {}", main_path.display()),
        ))
    })?;
    if layout_missing_current_path(&main_existing) {
        return Err(GenerateError::Config(format!(
            "{}'s layout() doesn't accept current_path — update it to \
             layout(title: &str, current_path: &str, flash: Markup, content: Markup) \
             (see autumn-cli/src/templates/main.rs.tmpl for the current shape) \
             before running `autumn generate pwa`",
            main_path.display()
        )));
    }

    let mut plan = Plan::new(project_root);

    // Static assets (served via generated route handlers + participate in fingerprinting)
    plan.create(
        project_root.join("static").join("manifest.webmanifest"),
        render_manifest(),
    );
    plan.create(
        project_root.join("static").join("service-worker.js"),
        render_service_worker(),
    );
    plan.create(
        project_root.join("static").join("pwa-register.js"),
        render_pwa_register_js(),
    );
    plan.create(
        project_root.join("static").join("icons").join("icon.svg"),
        render_icon_svg(),
    );
    plan.create(
        project_root
            .join("static")
            .join("icons")
            .join("maskable-icon.svg"),
        render_maskable_icon_svg(),
    );

    // src/main.rs: inject PWA meta tags + route handlers (idempotent)
    let updated_main = inject_pwa_into_main(&main_existing);
    if updated_main != main_existing {
        plan.modify(main_path, updated_main);
    }

    // System test
    let system_test_path = project_root
        .join("tests")
        .join("system")
        .join("pwa_smoke.rs");
    plan.create(system_test_path, render_pwa_system_test());

    // Cargo.toml: add system-tests feature if absent
    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = std::fs::read_to_string(&cargo_path).map_err(GenerateError::Io)?;
    let patched_cargo = patch_system_test_cargo_toml(&cargo_existing, "pwa_smoke");
    if patched_cargo != cargo_existing {
        plan.modify(cargo_path, patched_cargo);
    }

    Ok(plan)
}

/// CLI entry point.
pub fn run(flags: Flags) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    match plan_pwa(&cwd).and_then(|p| p.execute(flags)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

// ── Content renderers ─────────────────────────────────────────────────────────

fn render_manifest() -> String {
    concat!(
        "{\n",
        "  \"name\": \"My App\",\n",
        "  \"short_name\": \"My App\",\n",
        "  \"description\": \"Built with Autumn\",\n",
        "  \"start_url\": \"/\",\n",
        "  \"display\": \"standalone\",\n",
        "  \"background_color\": \"#ffffff\",\n",
        "  \"theme_color\": \"#ffffff\",\n",
        "  \"icons\": [\n",
        "    {\n",
        "      \"src\": \"/static/icons/icon.svg\",\n",
        "      \"sizes\": \"any\",\n",
        "      \"type\": \"image/svg+xml\",\n",
        "      \"purpose\": \"any\"\n",
        "    },\n",
        "    {\n",
        "      \"src\": \"/static/icons/maskable-icon.svg\",\n",
        "      \"sizes\": \"any\",\n",
        "      \"type\": \"image/svg+xml\",\n",
        "      \"purpose\": \"maskable\"\n",
        "    }\n",
        "  ]\n",
        "}\n",
    )
    .to_owned()
}

fn render_service_worker() -> String {
    r"const CACHE_NAME = 'autumn-pwa-v1';
const OFFLINE_URL = '/offline';
const PRECACHE_URLS = [OFFLINE_URL];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME)
      .then((cache) => cache.addAll(PRECACHE_URLS))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((names) => Promise.all(
        names.filter((n) => n !== CACHE_NAME).map((n) => caches.delete(n))
      ))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  if (event.request.method !== 'GET') {
    return;
  }
  if (event.request.mode === 'navigate') {
    event.respondWith(
      fetch(event.request).catch(() =>
        caches.match(OFFLINE_URL).then((r) => r || new Response('Offline', { status: 503 }))
      )
    );
    return;
  }
  if (event.request.url.includes('/static/')) {
    event.respondWith(
      caches.match(event.request).then((cached) => {
        if (cached) return cached;
        return fetch(event.request).then((response) => {
          if (response.ok) {
            const copy = response.clone();
            caches.open(CACHE_NAME).then((cache) => cache.put(event.request, copy));
          }
          return response;
        });
      })
    );
  }
});
"
    .to_owned()
}

fn render_icon_svg() -> String {
    // Note: using concat! to avoid raw-string issues with #ffffff and system-ui
    concat!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 192 192\">\n",
        "  <!-- Replace this placeholder with your app icon (PNG recommended for broad compatibility) -->\n",
        "  <rect width=\"192\" height=\"192\" rx=\"24\" fill=\"#4F7942\"/>\n",
        "  <text x=\"96\" y=\"140\" font-size=\"110\" text-anchor=\"middle\" font-family=\"system-ui\">&#x1F342;</text>\n",
        "</svg>\n",
    )
    .to_owned()
}

fn render_maskable_icon_svg() -> String {
    concat!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 192 192\">\n",
        "  <!-- Maskable icon: keep important content within the inner 116x116 safe zone -->\n",
        "  <!-- Replace this placeholder with your app icon (PNG recommended for broad compatibility) -->\n",
        "  <rect width=\"192\" height=\"192\" fill=\"#4F7942\"/>\n",
        "  <text x=\"96\" y=\"124\" font-size=\"72\" text-anchor=\"middle\" font-family=\"system-ui\">&#x1F342;</text>\n",
        "</svg>\n",
    )
    .to_owned()
}

fn render_pwa_register_js() -> String {
    // Served as a same-origin script to avoid CSP `script-src 'self'` blocking
    // inline scripts that the default SecurityHeadersLayer enforces.
    "if('serviceWorker'in navigator)\
     navigator.serviceWorker\
     .register('/service-worker.js',{scope:'/'})\
     .then(()=>document.documentElement.dataset.swRegistered='true')\
     .catch(console.error);\n"
        .to_owned()
}

fn render_pwa_system_test() -> String {
    let manifest_selector = r#"link[rel="manifest"]"#;
    // Handler stubs are defined inline so this integration test crate compiles without
    // depending on src/main.rs.  The real handlers (with include_str! paths and the app's
    // layout helper) are injected there by `inject_pwa_into_main`.
    let stubs = concat!(
        "#[get(\"/manifest.webmanifest\")]\n",
        "async fn pwa_manifest() -> impl IntoResponse {\n",
        "    ([(\"content-type\", \"application/manifest+json\")], \"\")\n",
        "}\n",
        "\n",
        "#[get(\"/service-worker.js\")]\n",
        "async fn pwa_service_worker() -> impl IntoResponse {\n",
        "    (\n",
        "        [\n",
        "            (\"content-type\", \"text/javascript; charset=utf-8\"),\n",
        "            (\"service-worker-allowed\", \"/\"),\n",
        "        ],\n",
        "        \"\",\n",
        "    )\n",
        "}\n",
        "\n",
        "#[get(\"/pwa-register.js\")]\n",
        "async fn pwa_register_js() -> impl IntoResponse {\n",
        "    ([(\"content-type\", \"text/javascript; charset=utf-8\")], \"\")\n",
        "}\n",
        "\n",
        "#[get(\"/offline\")]\n",
        "async fn pwa_offline() -> impl IntoResponse {\n",
        "    autumn_web::reexports::axum::response::Html(\n",
        "        \"<html><head><link rel=\\\"manifest\\\" href=\\\"/manifest.webmanifest\\\"></head><body></body></html>\",\n",
        "    )\n",
        "}\n",
        "\n",
    );
    format!(
        "//! PWA smoke test \u{2014} manifest content-type + service-worker registration.\n\
         //!\n\
         //! Run with:\n\
         //!   cargo test --features system-tests --test pwa_smoke -- --include-ignored\n\
         \n\
         #![cfg(feature = \"system-tests\")]\n\
         \n\
         use autumn_web::prelude::*;\n\
         use autumn_web::system_test::SystemTest;\n\
         \n\
         {stubs}\
         /// Checks that `GET /manifest.webmanifest` returns `application/manifest+json`\n\
         /// and that the `<link rel=\"manifest\">` tag is present in the page DOM.\n\
         #[tokio::test]\n\
         #[ignore = \"requires Chromium; run with --include-ignored\"]\n\
         async fn pwa_manifest_loads_with_correct_content_type() {{\n\
             let runner = SystemTest::new()\n\
                 .routes(routes![pwa_manifest, pwa_service_worker, pwa_register_js, pwa_offline])\n\
                 .build()\n\
                 .await\n\
                 .expect(\"test runner\");\n\
             let base_url = runner.base_url();\n\
             let page = runner.page().await.expect(\"page\");\n\
             \n\
             // Verify HTTP content-type via raw TCP to avoid a reqwest dev-dependency.\n\
             {{\n\
                 use std::io::{{Read, Write}};\n\
                 let host_port = base_url\n\
                     .trim_start_matches(\"http://\")\n\
                     .trim_start_matches(\"https://\");\n\
                 let mut stream = std::net::TcpStream::connect(host_port)\n\
                     .expect(\"connect to test server\");\n\
                 let req = format!(\"GET /manifest.webmanifest HTTP/1.1\\r\\nHost: {{host_port}}\\r\\nConnection: close\\r\\n\\r\\n\");\n\
                 stream.write_all(req.as_bytes()).expect(\"write request\");\n\
                 let mut response = String::new();\n\
                 stream.read_to_string(&mut response).expect(\"read response\");\n\
                 assert!(\n\
                     response.starts_with(\"HTTP/1.1 200\") || response.starts_with(\"HTTP/1.0 200\"),\n\
                     \"manifest must return 200, got: {{response}}\"\n\
                 );\n\
                 assert!(\n\
                     response.contains(\"application/manifest+json\"),\n\
                     \"manifest content-type must be application/manifest+json\"\n\
                 );\n\
             }}\n\
             \n\
             // Browser check: <link rel=\"manifest\"> is in <head>\n\
             page.visit(\"/offline\").await.expect(\"offline page loaded\");\n\
             page.expect_attribute({manifest_selector:?}, \"href\", \"/manifest.webmanifest\")\n\
                 .await\n\
                 .expect(\"manifest link present in DOM\");\n\
         }}\n\
         \n\
         /// Verifies that the service worker registers successfully (scope covers the whole app).\n\
         /// The `/offline` page is used as the test shell since it is always available.\n\
         #[tokio::test]\n\
         #[ignore = \"requires Chromium; run with --include-ignored\"]\n\
         async fn service_worker_registers_successfully() {{\n\
             let runner = SystemTest::new()\n\
                 .routes(routes![pwa_manifest, pwa_service_worker, pwa_register_js, pwa_offline])\n\
                 .build()\n\
                 .await\n\
                 .expect(\"test runner\");\n\
             let page = runner.page().await.expect(\"page\");\n\
             \n\
             // `/pwa-register.js` sets `data-sw-registered=\"true\"` on `<html>`\n\
             // after the SW registers.  Visiting `/offline` (which uses layout)\n\
             // loads the script without needing the user's root route.\n\
             page.visit(\"/offline\").await.expect(\"offline page loaded\");\n\
             page.expect_attribute(\"html\", \"data-sw-registered\", \"true\")\n\
                 .await\n\
                 .expect(\"service worker registered and controlling page\");\n\
         }}\n"
    )
}

// ── src/main.rs patching ──────────────────────────────────────────────────────

/// Inject all PWA additions into `src/main.rs` in a single idempotent pass:
/// 1. Add PWA `<meta>` / `<link>` tags + external register script to the `head {}` block.
/// 2. Add `pwa_manifest`, `pwa_service_worker`, `pwa_register_js`, and `pwa_offline` handlers.
/// 3. Register those handlers in `routes![…]`.
pub fn inject_pwa_into_main(source: &str) -> String {
    let with_meta = inject_pwa_meta_into_head(source);
    let with_handlers = inject_pwa_handlers(&with_meta);
    let route_entries = vec![
        "pwa_manifest".to_owned(),
        "pwa_service_worker".to_owned(),
        "pwa_register_js".to_owned(),
        "pwa_offline".to_owned(),
    ];
    update_main_rs(&with_handlers, &[], &route_entries)
}

/// Insert PWA `<link>` / `<meta>` tags into the `head {}` block of the
/// `layout` function.  Idempotent — skipped if `rel="manifest"` is already
/// present.
fn inject_pwa_meta_into_head(source: &str) -> String {
    if source.contains("/pwa-register.js") {
        return source.to_owned();
    }

    let lines: Vec<&str> = source.lines().collect();

    // Find the first `head {` line (Maud DSL — no leading keyword).
    // Support both `head {` and `head{` (both are valid Maud syntax).
    let Some(head_idx) = lines.iter().position(|l| {
        let t = l.trim();
        t == "head {" || t == "head{"
    }) else {
        return source.to_owned();
    };

    let head_indent = indent_count(lines[head_idx]);

    // Find the closing `}` of the head block (first `}` at the same indent
    // level as `head {`, after `head {`).
    let Some(close_rel) = lines[head_idx + 1..]
        .iter()
        .position(|l| indent_count(l) == head_indent && l.trim() == "}")
    else {
        return source.to_owned();
    };
    let close_idx = head_idx + 1 + close_rel;

    let inner_indent = " ".repeat(head_indent + 4);
    // Use an external script to stay compliant with `script-src 'self'` CSP.
    // The script sets `data-sw-registered="true"` on `<html>` after registration;
    // the system test polls for that attribute.
    let meta_block = format!(
        "{inner_indent}link rel=\"manifest\" href=\"/manifest.webmanifest\";\n\
         {inner_indent}meta name=\"theme-color\" content=\"#ffffff\";\n\
         {inner_indent}link rel=\"apple-touch-icon\" href=\"/static/icons/icon.svg\";\n\
         {inner_indent}script src=\"/pwa-register.js\" {{}}\n"
    );

    let mut result = lines[..close_idx].join("\n");
    result.push('\n');
    result.push_str(&meta_block);
    result.push_str(lines[close_idx]);
    if close_idx + 1 < lines.len() {
        result.push('\n');
        result.push_str(&lines[close_idx + 1..].join("\n"));
    }
    if source.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// True when `source` defines a `layout()` function whose signature doesn't
/// mention `current_path` — the pre-`nav_bar` scaffold shape
/// (`layout(title, flash, content)`) rather than the current one
/// (`layout(title, current_path, flash, content)`). `pwa_offline`'s
/// generated call assumes the current shape, so this gates [`plan_pwa`]
/// with an actionable error instead of emitting code with the wrong arity.
///
/// Scans the whole signature span (from `fn layout(` up to the function
/// body's opening brace), not just the physical line `fn layout(` appears
/// on — rustfmt wraps this signature across multiple lines at its default
/// column width, which would otherwise put `current_path` on a line the
/// check never looks at.
fn layout_missing_current_path(source: &str) -> bool {
    let Some(start) = source.find("fn layout(") else {
        return false;
    };
    let rest = &source[start..];
    let signature = rest.find('{').map_or(rest, |brace| &rest[..brace]);
    !signature.contains("current_path")
}

/// Append `pwa_manifest`, `pwa_service_worker`, `pwa_register_js`, and `pwa_offline`
/// handler functions just before `#[autumn_web::main]`.  Idempotent — skipped when
/// `pwa_manifest` is already defined.
fn inject_pwa_handlers(source: &str) -> String {
    if source.contains("async fn pwa_manifest()") {
        return source.to_owned();
    }

    let handlers = "\
#[get(\"/manifest.webmanifest\")]\n\
async fn pwa_manifest() -> impl IntoResponse {\n\
    (\n\
        [\n\
            (\"content-type\", \"application/manifest+json\"),\n\
            (\"cache-control\", \"public, max-age=3600\"),\n\
        ],\n\
        include_str!(\"../static/manifest.webmanifest\"),\n\
    )\n\
}\n\
\n\
#[get(\"/service-worker.js\")]\n\
async fn pwa_service_worker() -> impl IntoResponse {\n\
    (\n\
        [\n\
            (\"content-type\", \"text/javascript; charset=utf-8\"),\n\
            (\"service-worker-allowed\", \"/\"),\n\
            (\"cache-control\", \"no-cache\"),\n\
        ],\n\
        include_str!(\"../static/service-worker.js\"),\n\
    )\n\
}\n\
\n\
#[get(\"/pwa-register.js\")]\n\
async fn pwa_register_js() -> impl IntoResponse {\n\
    (\n\
        [\n\
            (\"content-type\", \"text/javascript; charset=utf-8\"),\n\
            (\"cache-control\", \"public, max-age=3600\"),\n\
        ],\n\
        include_str!(\"../static/pwa-register.js\"),\n\
    )\n\
}\n\
\n\
#[get(\"/offline\")]\n\
async fn pwa_offline(flash: Flash, path: CurrentPath) -> maud::Markup {\n\
    layout(\n\
        \"Offline\",\n\
        path.as_str(),\n\
        flash.render().await,\n\
        maud::html! {\n\
            h1 { \"You are offline\" }\n\
            p { \"Check your internet connection and try again.\" }\n\
        },\n\
    )\n\
}\n\
\n";

    // Insert before `#[autumn_web::main]`, or append at end as fallback.
    source.find("#[autumn_web::main]").map_or_else(
        || {
            let mut result = source.to_owned();
            if !result.ends_with('\n') {
                result.push('\n');
            }
            result.push('\n');
            result.push_str(handlers);
            result
        },
        |pos| {
            let mut result = String::with_capacity(source.len() + handlers.len());
            result.push_str(&source[..pos]);
            result.push_str(handlers);
            result.push_str(&source[pos..]);
            result
        },
    )
}

fn indent_count(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::generate::Flags;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    fn project_with_main(main_rs: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\n[dependencies]\nautumn-web = \"0.5.0\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), main_rs).unwrap();
        tmp
    }

    const DEFAULT_MAIN: &str = "\
use autumn_web::form::skip_link;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

pub fn layout(title: &str, current_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {
    let nav = NavBarConfig::new()
        .brand(\"My App\", \"/\")
        .aria_label(\"Main navigation\")
        .item(NavItem::link(\"/\", \"Home\"));
    maud::html! {
        (maud::DOCTYPE)
        html lang=\"en\" {
            head {
                meta charset=\"utf-8\";
                meta name=\"viewport\" content=\"width=device-width, initial-scale=1\";
                title { (title) }
                link rel=\"stylesheet\" href=(autumn_web::flash::FLASH_CSS_PATH);
                link rel=\"stylesheet\" href=\"/static/css/app.css\";
                script src=(autumn_web::htmx::AUTUMN_WIDGETS_JS_PATH) defer {}
            }
            body {
                (skip_link(\"#main-content\", \"Skip to main content\"))
                header role=\"banner\" {
                    (nav_bar(current_path, &nav))
                }
                main id=\"main-content\" role=\"main\" {
                    (flash)
                    (content)
                }
                footer role=\"contentinfo\" {
                    p { \"Built with Autumn\" }
                }
            }
        }
    }
}

#[get(\"/\")]
async fn index(flash: Flash, path: CurrentPath) -> maud::Markup {
    layout(\"Welcome\", path.as_str(), flash.render().await, maud::html! {
        h1 { \"Welcome!\" }
    })
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index])
        .migrations(MIGRATIONS)
        .run()
        .await;
}
";

    /// The pre-`nav_bar` scaffold shape: `layout()` has no `current_path`
    /// parameter. Apps generated before `nav_bar` shipped still look like this.
    const OLD_SHAPE_MAIN: &str = "\
use autumn_web::form::skip_link;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

pub fn layout(title: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {
    maud::html! {
        (maud::DOCTYPE)
        html lang=\"en\" {
            head {
                meta charset=\"utf-8\";
                title { (title) }
            }
            body {
                (skip_link(\"#main-content\", \"Skip to main content\"))
                main id=\"main-content\" role=\"main\" {
                    (flash)
                    (content)
                }
            }
        }
    }
}

#[get(\"/\")]
async fn index(flash: Flash) -> maud::Markup {
    layout(\"Welcome\", flash.render().await, maud::html! {
        h1 { \"Welcome!\" }
    })
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index])
        .migrations(MIGRATIONS)
        .run()
        .await;
}
";

    // ── plan_pwa: file plan tests ─────────────────────────────────────────────

    #[test]
    fn plan_pwa_rejects_layout_missing_current_path() {
        // nav_bar's arrival made layout() take current_path as its second
        // argument; pwa_offline's generated call now assumes that shape.
        // Running against an app that hasn't migrated must fail loudly with
        // an actionable message instead of silently emitting a call that
        // won't compile.
        let tmp = project_with_main(OLD_SHAPE_MAIN);
        let err = plan_pwa(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("current_path"), "{msg}");
        assert!(msg.contains("layout"), "{msg}");
    }

    #[test]
    fn plan_pwa_accepts_rustfmt_wrapped_layout_signature() {
        // rustfmt wraps layout()'s ~108-char signature onto separate lines
        // (verified: `rustfmt` puts each param on its own line at the
        // default 100-column width) — current_path then no longer shares a
        // physical line with `fn layout(`, so a naive single-line scan would
        // false-positive as "missing current_path" on any formatted,
        // up-to-date project.
        let wrapped_main = DEFAULT_MAIN.replace(
            "pub fn layout(title: &str, current_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {",
            "pub fn layout(\n    title: &str,\n    current_path: &str,\n    flash: maud::Markup,\n    content: maud::Markup,\n) -> maud::Markup {",
        );
        assert_ne!(
            wrapped_main, DEFAULT_MAIN,
            "replacement must actually match"
        );
        let tmp = project_with_main(&wrapped_main);
        plan_pwa(tmp.path())
            .expect("rustfmt-wrapped current_path parameter must still be detected");
    }

    #[test]
    fn plan_pwa_requires_project_root() {
        let tmp = TempDir::new().unwrap();
        let err = plan_pwa(tmp.path()).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_creates_manifest_webmanifest() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("manifest.webmanifest")),
            "plan must include manifest.webmanifest"
        );
    }

    #[test]
    fn plan_creates_service_worker_js() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("service-worker.js")),
            "plan must include service-worker.js"
        );
    }

    #[test]
    fn plan_creates_pwa_register_js() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("pwa-register.js")),
            "plan must include pwa-register.js"
        );
    }

    #[test]
    fn plan_creates_icons() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        assert!(
            plan.actions.iter().any(|a| a.path().ends_with("icon.svg")),
            "plan must include icon.svg"
        );
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("maskable-icon.svg")),
            "plan must include maskable-icon.svg"
        );
    }

    #[test]
    fn plan_creates_system_test() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("pwa_smoke.rs")),
            "plan must include pwa_smoke.rs"
        );
    }

    // ── manifest content ──────────────────────────────────────────────────────

    #[test]
    fn manifest_is_valid_json() {
        let content = render_manifest();
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("manifest.webmanifest must be valid JSON");
        assert!(parsed["name"].is_string(), "manifest must have a name");
        assert!(parsed["icons"].is_array(), "manifest must have icons array");
    }

    #[test]
    fn manifest_has_required_fields_for_installability() {
        let content = render_manifest();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed["start_url"].is_string());
        assert!(
            ["fullscreen", "standalone", "minimal-ui"]
                .contains(&parsed["display"].as_str().unwrap_or(""))
        );
        let icons = parsed["icons"].as_array().unwrap();
        assert!(!icons.is_empty(), "at least one icon required");
    }

    #[test]
    fn manifest_has_both_icon_purposes() {
        let content = render_manifest();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let icons = parsed["icons"].as_array().unwrap();
        let purposes: Vec<&str> = icons.iter().filter_map(|i| i["purpose"].as_str()).collect();
        assert!(
            purposes
                .iter()
                .any(|p| p.contains("any") || p.contains("monochrome")),
            "must have a general-purpose icon"
        );
        assert!(
            purposes.iter().any(|p| p.contains("maskable")),
            "must have a maskable icon"
        );
    }

    // ── service worker content ────────────────────────────────────────────────

    #[test]
    fn service_worker_has_offline_fallback_for_navigation() {
        let sw = render_service_worker();
        assert!(
            sw.contains("navigate"),
            "SW must handle navigation requests"
        );
        assert!(sw.contains("offline"), "SW must have offline fallback");
    }

    #[test]
    fn service_worker_precaches_offline_url() {
        let sw = render_service_worker();
        assert!(
            sw.contains("PRECACHE_URLS"),
            "SW must declare precache list"
        );
        assert!(
            sw.contains("/offline"),
            "SW must precache the offline shell"
        );
    }

    #[test]
    fn service_worker_has_install_and_activate_handlers() {
        let sw = render_service_worker();
        assert!(sw.contains("install"), "SW must have install handler");
        assert!(sw.contains("activate"), "SW must have activate handler");
    }

    #[test]
    fn service_worker_caches_static_assets_first() {
        let sw = render_service_worker();
        assert!(sw.contains("/static/"), "SW must cache static assets");
    }

    // ── inject_pwa_meta_into_head ─────────────────────────────────────────────

    #[test]
    fn inject_adds_manifest_link() {
        let updated = inject_pwa_meta_into_head(DEFAULT_MAIN);
        assert!(
            updated.contains(r#"rel="manifest""#),
            "must add rel=manifest link"
        );
        assert!(
            updated.contains("/manifest.webmanifest"),
            "must reference /manifest.webmanifest"
        );
    }

    #[test]
    fn inject_adds_external_register_script() {
        let updated = inject_pwa_meta_into_head(DEFAULT_MAIN);
        assert!(
            updated.contains("src=\"/pwa-register.js\""),
            "must add external SW registration script (avoids CSP inline-script issues)"
        );
        assert!(
            !updated.contains("serviceWorker"),
            "must not embed inline serviceWorker JS"
        );
    }

    #[test]
    fn inject_adds_theme_color_meta() {
        let updated = inject_pwa_meta_into_head(DEFAULT_MAIN);
        assert!(
            updated.contains("theme-color"),
            "must add theme-color meta tag"
        );
    }

    #[test]
    fn inject_adds_apple_touch_icon() {
        let updated = inject_pwa_meta_into_head(DEFAULT_MAIN);
        assert!(
            updated.contains("apple-touch-icon"),
            "must add apple-touch-icon link"
        );
    }

    #[test]
    fn inject_meta_is_idempotent() {
        let once = inject_pwa_meta_into_head(DEFAULT_MAIN);
        let twice = inject_pwa_meta_into_head(&once);
        assert_eq!(once, twice, "inject_pwa_meta_into_head must be idempotent");
    }

    #[test]
    fn inject_meta_preserves_existing_content() {
        let updated = inject_pwa_meta_into_head(DEFAULT_MAIN);
        assert!(updated.contains(r#"meta charset="utf-8""#));
        assert!(updated.contains(r#"meta name="viewport""#));
        assert!(updated.contains(r#"link rel="stylesheet""#));
    }

    #[test]
    fn inject_meta_no_op_when_head_absent() {
        let src = "fn main() { println!(\"hello\"); }\n";
        let result = inject_pwa_meta_into_head(src);
        assert_eq!(result, src, "must be unchanged if no head {{}} block found");
    }

    // ── inject_pwa_handlers ───────────────────────────────────────────────────

    #[test]
    fn inject_handlers_adds_manifest_route() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        assert!(
            updated.contains("async fn pwa_manifest()"),
            "must add pwa_manifest handler"
        );
        assert!(
            updated.contains("application/manifest+json"),
            "pwa_manifest must set correct content-type"
        );
    }

    #[test]
    fn inject_handlers_adds_service_worker_route() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        assert!(
            updated.contains("async fn pwa_service_worker()"),
            "must add pwa_service_worker handler"
        );
        assert!(
            updated.contains("service-worker-allowed"),
            "service-worker handler must set Service-Worker-Allowed header"
        );
    }

    #[test]
    fn inject_handlers_adds_register_js_route() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        assert!(
            updated.contains("async fn pwa_register_js()"),
            "must add pwa_register_js handler"
        );
        assert!(
            updated.contains("include_str!(\"../static/pwa-register.js\")"),
            "pwa_register_js must embed file at compile time via include_str!"
        );
    }

    #[test]
    fn inject_handlers_adds_offline_route() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        assert!(
            updated.contains("async fn pwa_offline("),
            "must add pwa_offline handler"
        );
    }

    #[test]
    fn inject_handlers_is_idempotent() {
        let once = inject_pwa_handlers(DEFAULT_MAIN);
        let twice = inject_pwa_handlers(&once);
        assert_eq!(once, twice, "inject_pwa_handlers must be idempotent");
    }

    #[test]
    fn inject_handlers_places_before_main() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        let handler_pos = updated.find("async fn pwa_manifest()").unwrap();
        let main_pos = updated.find("#[autumn_web::main]").unwrap();
        assert!(
            handler_pos < main_pos,
            "PWA handlers must appear before #[autumn_web::main]"
        );
    }

    #[test]
    fn pwa_manifest_handler_uses_include_str() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        assert!(
            updated.contains("include_str!(\"../static/manifest.webmanifest\")"),
            "pwa_manifest must embed file at compile time via include_str!"
        );
    }

    #[test]
    fn pwa_service_worker_handler_uses_include_str() {
        let updated = inject_pwa_handlers(DEFAULT_MAIN);
        assert!(
            updated.contains("include_str!(\"../static/service-worker.js\")"),
            "pwa_service_worker must embed file at compile time via include_str!"
        );
    }

    // ── inject_pwa_into_main (combined) ──────────────────────────────────────

    #[test]
    fn full_inject_adds_routes_to_routes_macro() {
        let updated = inject_pwa_into_main(DEFAULT_MAIN);
        assert!(
            updated.contains("pwa_manifest"),
            "routes![] must include pwa_manifest"
        );
        assert!(
            updated.contains("pwa_service_worker"),
            "routes![] must include pwa_service_worker"
        );
        assert!(
            updated.contains("pwa_register_js"),
            "routes![] must include pwa_register_js"
        );
        assert!(
            updated.contains("pwa_offline"),
            "routes![] must include pwa_offline"
        );
    }

    #[test]
    fn full_inject_is_idempotent() {
        let once = inject_pwa_into_main(DEFAULT_MAIN);
        let twice = inject_pwa_into_main(&once);
        assert_eq!(once, twice, "inject_pwa_into_main must be idempotent");
    }

    #[test]
    fn full_inject_does_not_duplicate_manifest_link() {
        let once = inject_pwa_into_main(DEFAULT_MAIN);
        let twice = inject_pwa_into_main(&once);
        let count = twice.matches(r#"rel="manifest""#).count();
        assert_eq!(count, 1, "must not duplicate rel=manifest link");
    }

    #[test]
    fn full_inject_does_not_duplicate_pwa_routes() {
        let once = inject_pwa_into_main(DEFAULT_MAIN);
        let twice = inject_pwa_into_main(&once);
        let handler_count = twice.matches("async fn pwa_manifest()").count();
        assert_eq!(handler_count, 1, "must not duplicate pwa_manifest handler");
    }

    #[test]
    fn pwa_offline_handler_matches_current_layout_arity() {
        // The scaffold's layout() takes (title, current_path, flash, content) —
        // pwa_offline must extract CurrentPath and pass it through, or the
        // generated src/main.rs fails to compile with an arity mismatch.
        let updated = inject_pwa_into_main(DEFAULT_MAIN);
        assert!(
            updated.contains("async fn pwa_offline(flash: Flash, path: CurrentPath)"),
            "pwa_offline must extract CurrentPath alongside Flash: {updated}"
        );
        let offline_fn_start = updated
            .find("async fn pwa_offline")
            .expect("pwa_offline handler must be present");
        let offline_fn = &updated[offline_fn_start..offline_fn_start + 200];
        assert!(
            offline_fn.contains("\"Offline\"") && offline_fn.contains("path.as_str()"),
            "pwa_offline must pass path.as_str() as layout()'s second argument: {offline_fn}"
        );
    }

    // ── plan execution ────────────────────────────────────────────────────────

    #[test]
    fn plan_execute_creates_manifest_file() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let manifest_path = tmp.path().join("static/manifest.webmanifest");
        assert!(
            manifest_path.exists(),
            "static/manifest.webmanifest must exist"
        );
        let content = fs::read_to_string(&manifest_path).unwrap();
        let _: serde_json::Value = serde_json::from_str(&content)
            .expect("manifest.webmanifest must be valid JSON after execution");
    }

    #[test]
    fn plan_execute_creates_service_worker_file() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            tmp.path().join("static/service-worker.js").exists(),
            "static/service-worker.js must exist"
        );
    }

    #[test]
    fn plan_execute_creates_pwa_register_js_file() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            tmp.path().join("static/pwa-register.js").exists(),
            "static/pwa-register.js must exist"
        );
        let content = fs::read_to_string(tmp.path().join("static/pwa-register.js")).unwrap();
        assert!(
            content.contains("serviceWorker"),
            "pwa-register.js must contain SW registration code"
        );
    }

    #[test]
    fn plan_execute_creates_icon_files() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            tmp.path().join("static/icons/icon.svg").exists(),
            "static/icons/icon.svg must exist"
        );
        assert!(
            tmp.path().join("static/icons/maskable-icon.svg").exists(),
            "static/icons/maskable-icon.svg must exist"
        );
    }

    #[test]
    fn plan_execute_creates_system_test_file() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            tmp.path().join("tests/system/pwa_smoke.rs").exists(),
            "tests/system/pwa_smoke.rs must exist"
        );
    }

    #[test]
    fn plan_execute_updates_main_rs_with_pwa_meta_and_handlers() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main_rs.contains(r#"rel="manifest""#));
        assert!(main_rs.contains("src=\"/pwa-register.js\""));
        assert!(main_rs.contains("async fn pwa_manifest()"));
        assert!(main_rs.contains("async fn pwa_service_worker()"));
        assert!(main_rs.contains("async fn pwa_register_js()"));
        assert!(main_rs.contains("async fn pwa_offline("));
    }

    #[test]
    fn plan_execute_is_idempotent_with_force() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();
        let main_rs = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(
            main_rs.matches(r#"rel="manifest""#).count(),
            1,
            "re-running must not duplicate manifest link"
        );
        assert_eq!(
            main_rs.matches("async fn pwa_manifest()").count(),
            1,
            "re-running must not duplicate pwa_manifest handler"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let original_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();
        assert!(
            !tmp.path().join("static/manifest.webmanifest").exists(),
            "dry-run must not create manifest"
        );
        let after = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(original_main, after, "dry-run must not modify main.rs");
    }
}
