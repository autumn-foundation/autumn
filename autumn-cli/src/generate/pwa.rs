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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use autumn_web::config::DatabaseBackend;
use autumn_web::push::PUSH_SUBSCRIPTIONS_TABLE;

use super::dsl::{Field, FieldConstraints, FieldKind, IdType};
use super::emit::Plan;
use super::schema_edit::{
    create_table_sql_with_metadata_and_id_for, drop_table_sql, update_main_rs,
};
use super::system_test::patch_cargo_toml as patch_system_test_cargo_toml;
use super::{GenerateError, detect_backend, ensure_project_root, timestamp_now};

// ── Public API ────────────────────────────────────────────────────────────────

/// The actions and reverts common to both `plan_pwa` and
/// [`plan_pwa_destroy_fallback`]: every PWA-generated file is fully static
/// (parameter-free — none of it depends on `src/main.rs`'s current shape),
/// and the revert descriptors only need paths, not content (they read
/// `main.rs`/`Cargo.toml` fresh off disk when `Plan::revert` runs them).
fn plan_pwa_shared(project_root: &Path) -> Plan {
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

    // Pushed unconditionally — see `plan_cargo_deps`'s matching comment in
    // model.rs: destroy recomputes this plan against the already-generated
    // main.rs, where these edits are by definition already present.
    let main_path = project_root.join("src").join("main.rs");
    plan.push_revert(crate::generate::emit::Revert::PwaMainRsInjection {
        path: main_path.clone(),
    });
    plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
        path: main_path,
        entries: vec![
            "pwa_manifest".to_owned(),
            "pwa_service_worker".to_owned(),
            "pwa_register_js".to_owned(),
            "pwa_offline".to_owned(),
        ],
    });

    // migrations/<ts>_create_push_subscriptions/{up,down}.sql — the table the
    // framework's `DbPushSubscriptionStore` reads (issue #1392).
    //
    // Like the notifications feed, push subscriptions are a singleton scaffold:
    // re-running (especially with `--force`) must not mint a SECOND
    // `*_create_push_subscriptions` directory, since two `CREATE TABLE
    // push_subscriptions` migrations would fail the next `autumn migrate` on
    // the duplicate table and make destroy's suffix match ambiguous. So reuse
    // an existing directory when one is present, minting a fresh timestamped
    // one only on a clean project.
    let backend = detect_backend(project_root);
    let migration_dir = existing_push_migration_dir(project_root).unwrap_or_else(|| {
        project_root.join("migrations").join(format!(
            "{}_create_{PUSH_SUBSCRIPTIONS_TABLE}",
            timestamp_now()
        ))
    });
    plan.create(
        migration_dir.join("up.sql"),
        push_subscriptions_up_sql(backend),
    );
    plan.create(
        migration_dir.join("down.sql"),
        drop_table_sql(PUSH_SUBSCRIPTIONS_TABLE),
    );

    // System test
    let system_test_path = project_root
        .join("tests")
        .join("system")
        .join("pwa_smoke.rs");
    plan.create(system_test_path, render_pwa_system_test());

    // Cargo.toml: add system-tests feature if absent
    let cargo_path = project_root.join("Cargo.toml");
    plan.push_revert(crate::generate::emit::Revert::SystemTestCargoPatch {
        path: cargo_path,
        snake_name: "pwa_smoke".to_owned(),
    });

    plan
}

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
            "{}'s layout() takes fewer than 4 parameters — the current \
             nav_bar-based scaffold needs layout(title: &str, current_path: &str, \
             flash: Markup, content: Markup) (see autumn-cli/src/templates/main.rs.tmpl \
             for the current shape; the path parameter's name doesn't matter) \
             before running `autumn generate pwa`",
            main_path.display()
        )));
    }

    let mut plan = plan_pwa_shared(project_root);

    // src/main.rs: inject PWA meta tags + route handlers (idempotent)
    let updated_main = inject_pwa_into_main(&main_existing);
    // A silently unmounted router is the worst outcome here: the command exits
    // 0, the client snippet ships, and every `fetch` it makes 404s with
    // nothing pointing at the cause. Say so instead.
    if !updated_main.contains("autumn_web::push::router()") {
        plan.warn(
            "could not find the app builder in src/main.rs, so the Web Push routes were not \
             mounted. Add this line to your builder chain by hand:\n    \
             .merge(autumn_web::push::router())\n  \
             Without it the generated subscribe snippet will 404. See \
             docs/guide/web-push.md.",
        );
    }
    if updated_main != main_existing {
        plan.modify(main_path, updated_main);
    }

    // Cargo.toml: add system-tests feature if absent
    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = std::fs::read_to_string(&cargo_path).map_err(GenerateError::Io)?;
    let patched_cargo = patch_system_test_cargo_toml(&cargo_existing, "pwa_smoke");
    if patched_cargo != cargo_existing {
        plan.modify(cargo_path, patched_cargo);
    }

    Ok(plan)
}

/// Destroy-only fallback (issue #1048 PR review): `plan_pwa` validates that
/// `src/main.rs`'s `layout()` takes 4 parameters before building anything —
/// necessary so a *fresh* generate never emits a call with the wrong arity,
/// but that precondition is irrelevant (and can wrongly block cleanup) once
/// `main.rs` has since been hand-edited, reverted, or otherwise no longer
/// matches the shape `plan_pwa` expects. Every PWA-generated file is fully
/// static, and `Plan::revert` never consults `Action::Modify` content —
/// only `self.reverts`, which read `main.rs`/`Cargo.toml` fresh off disk at
/// revert time — so `plan_pwa_shared` alone is already a complete, exact
/// destroy plan; unlike the model-dependent admin/migration fallbacks, no
/// `--force` is needed to use it.
pub fn plan_pwa_destroy_fallback(project_root: &Path) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    Ok(plan_pwa_shared(project_root))
}

// ── Content renderers ─────────────────────────────────────────────────────────

/// The existing `migrations/<ts>_create_push_subscriptions/` directory, if the
/// project already has one — so a re-run reuses it in place rather than
/// minting a second singleton migration. Matched by the same
/// `_create_push_subscriptions` suffix destroy uses, so the two stay in
/// agreement. If more than one somehow exists (a hand-added duplicate), the
/// lexicographically-smallest is chosen deterministically.
fn existing_push_migration_dir(project_root: &Path) -> Option<PathBuf> {
    let suffix = format!("_create_{PUSH_SUBSCRIPTIONS_TABLE}");
    let mut matches: Vec<PathBuf> = std::fs::read_dir(project_root.join("migrations"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// The four non-`id` columns of the `push_subscriptions` table, expressed in
/// the migration DSL so the DDL comes out of the same shared helper every
/// other generator uses (`id` and `created_at` come from the helper itself).
///
/// The column types must match the framework store's diesel `table!` in
/// `autumn_web::push::store` (`Text` throughout; `Timestamptz` on Postgres,
/// `TimestamptzSqlite` — RFC 3339 `TEXT` — on SQLite): that store, not any
/// generated model, is what reads this table. The two key columns hold the
/// browser's own base64url strings rather than raw bytes, which keeps one
/// `table!` definition working on both backends.
fn push_subscription_fields() -> Vec<Field> {
    let field = |name: &str, unique: bool| Field {
        name: name.to_owned(),
        kind: FieldKind::String,
        nullable: false,
        variants: Vec::new(),
        unique,
        constraints: FieldConstraints::default(),
        state_machine: None,
    };
    vec![
        field("principal_id", false),
        // UNIQUE is load-bearing, not decoration: it is what makes the store's
        // `ON CONFLICT (endpoint) DO UPDATE` upsert atomic rather than a racy
        // select-then-insert, and what stops one browser accumulating a row
        // per page load.
        field("endpoint", true),
        field("p256dh", false),
        field("auth", false),
    ]
}

/// Build the `push_subscriptions` `up.sql` for the target `backend`.
///
/// The shared helper's stock `created_at` is `TIMESTAMP`; the Postgres output
/// rewrites that one column to `TIMESTAMPTZ` to stay in lockstep with the
/// framework store's `Timestamptz` mapping — the same adjustment the
/// notifications generator makes, for the same reason. The `SQLite` output
/// already matches and is left exactly as the helper emits it.
fn push_subscriptions_up_sql(backend: DatabaseBackend) -> String {
    let indexes: BTreeSet<String> = std::iter::once("principal_id".to_owned()).collect();
    let sql = create_table_sql_with_metadata_and_id_for(
        backend,
        PUSH_SUBSCRIPTIONS_TABLE,
        &push_subscription_fields(),
        &indexes,
        &BTreeMap::new(),
        IdType::BigSerial,
    );
    match backend {
        DatabaseBackend::Postgres => sql.replace(
            "created_at TIMESTAMP NOT NULL DEFAULT NOW()",
            "created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()",
        ),
        DatabaseBackend::Sqlite => sql,
    }
}

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

// ── Web Push ────────────────────────────────────────────────────────────────
// Fired by the browser even when every tab of this site is closed. The payload
// is what `autumn_web::push::PushMessage` serializes: {title, body, url?, icon?}.

self.addEventListener('push', (event) => {
  // A push can legitimately arrive with no payload, and a payload from any
  // other sender need not be JSON. Throwing here makes the browser show a
  // generic 'site updated in the background' notice instead, so always fall
  // back to something readable.
  let payload = {};
  if (event.data) {
    try {
      payload = event.data.json();
    } catch (e) {
      payload = { body: event.data.text() };
    }
  }
  const title = payload.title || 'Notification';
  const options = {
    body: payload.body || '',
    icon: payload.icon || '/static/icons/icon.svg',
    // Carried through to `notificationclick` below.
    data: { url: payload.url || '/' },
  };
  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const target = new URL(
    (event.notification.data && event.notification.data.url) || '/',
    self.location.origin
  );
  // Cross-origin targets are dropped: the payload travels through a third-party
  // push service, so a notification must never be able to navigate this app's
  // users off-origin.
  const url = target.origin === self.location.origin ? target.href : self.location.origin + '/';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((windows) => {
      // Prefer focusing a tab that is already on the target rather than
      // opening a duplicate one.
      for (const client of windows) {
        if (client.url === url && 'focus' in client) {
          return client.focus();
        }
      }
      if (windows.length > 0 && 'navigate' in windows[0] && 'focus' in windows[0]) {
        // `navigate()` REJECTS for a client this service worker does not
        // control — and `includeUncontrolled: true` above is exactly how such
        // clients get into this list. An unhandled rejection inside
        // `waitUntil` means the click does nothing at all, so fall back.
        return windows[0]
          .navigate(url)
          .then((client) => (client ? client.focus() : undefined))
          .catch(() => self.clients.openWindow(url));
      }
      return self.clients.openWindow(url);
    })
  );
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

/// The `static/pwa-register.js` the generator emits.
///
/// Served as a same-origin script to avoid CSP `script-src 'self'` blocking
/// inline scripts that the default `SecurityHeadersLayer` enforces.
///
/// Two halves, split across two functions so each stays readable: unconditional
/// service-worker registration, and the Web Push opt-in (issue #1392).
fn render_pwa_register_js() -> String {
    format!(
        "{}\n{}",
        render_service_worker_registration(),
        render_push_opt_in()
    )
}

/// The service-worker registration half of `pwa-register.js`.
///
/// Sets `data-sw-registered="true"` on `<html>` once registration resolves;
/// the generated system test polls for that attribute.
fn render_service_worker_registration() -> String {
    r"if ('serviceWorker' in navigator) {
  navigator.serviceWorker
    .register('/service-worker.js', { scope: '/' })
    .then(() => { document.documentElement.dataset.swRegistered = 'true'; })
    .catch(console.error);
}
"
    .to_owned()
}

/// The Web Push opt-in half of `pwa-register.js` (issue #1392).
///
/// Deliberately NOT run on load: a permission prompt fired without a user
/// gesture is the fastest way to get permanently blocked, and both Chrome and
/// Firefox penalise it. The subscribe flow is exposed as
/// `window.autumnPushSubscribe()` and wired to any element carrying
/// `data-autumn-push-subscribe`, so an app gets an opt-in button with no
/// JavaScript of its own:
///
/// ```html
/// <button data-autumn-push-subscribe>Enable notifications</button>
/// ```
///
/// Every path and header name below comes from `autumn_web::push::router`'s own
/// constants, so the snippet and the mounted routes can never drift apart.
fn render_push_opt_in() -> String {
    format!(
        r"// Autumn's CSRF layer rejects an unaccompanied mutating request, so both POSTs
// below must carry a token. Without one they 403 the moment
// `[security.csrf] enabled = true` — which the production smart defaults do —
// and the tempting workaround (exempting `/push/`) would be far worse than the
// bug it hides: a forced subscribe would register an ATTACKER's keys under the
// victim's session, letting them decrypt every notification sent to that user.
//
// The CSRF cookie is HttpOnly, so this cannot read it. Two sources, in order:
// the public-key response (which the subscribe flow fetches anyway, and which
// carries the token for exactly this purpose), then `<meta name=csrf-token>`
// for an app that already publishes it the way `autumn generate auth` does.
let autumnPushCsrf = null;

function autumnPushHeaders() {{
  const headers = {{ 'content-type': 'application/json' }};
  if (autumnPushCsrf && autumnPushCsrf.token) {{
    headers[autumnPushCsrf.header || 'X-CSRF-Token'] = autumnPushCsrf.token;
    return headers;
  }}
  // Unquoted attribute values (both are valid CSS identifiers) so this stays
  // inside a Rust raw string that cannot contain a double quote.
  const token = document.querySelector('meta[name=csrf-token]');
  if (token && token.content) {{
    const header = document.querySelector('meta[name=csrf-token-header]');
    headers[(header && header.content) || 'X-CSRF-Token'] = token.content;
  }}
  return headers;
}}

// Read the CSRF token off a public-key response. Same-origin only: a
// cross-origin reader cannot see these headers without the app opting in via
// CORS, which is the property double-submit CSRF already depends on.
function autumnPushCaptureCsrf(response) {{
  const token = response.headers.get('{csrf_token_header}');
  if (token) {{
    autumnPushCsrf = {{
      token: token,
      header: response.headers.get('{csrf_header_name_header}'),
    }};
  }}
}}

// `applicationServerKey` takes a BufferSource, not the base64url string the
// endpoint serves, and `atob` only understands standard base64 — so translate
// base64url's `-` and `_` back to `+` and `/` and re-pad before decoding.
function autumnDecodeVapidKey(base64url) {{
  const padding = '='.repeat((4 - (base64url.length % 4)) % 4);
  const base64 = (base64url + padding).replace(/-/g, '+').replace(/_/g, '/');
  const raw = atob(base64);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i += 1) {{
    bytes[i] = raw.charCodeAt(i);
  }}
  return bytes;
}}

// Opt the current visitor in to Web Push. Call it from a click handler — it
// prompts for notification permission, so it needs a user gesture.
// Resolves `true` once the subscription has been recorded server-side.
window.autumnPushSubscribe = async function autumnPushSubscribe() {{
  // Safari on older iOS has service workers but no Push API at all.
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) {{
    return false;
  }}
  if ((await Notification.requestPermission()) !== 'granted') {{
    return false;
  }}
  const response = await fetch('{key_path}', {{ credentials: 'same-origin' }});
  if (!response.ok) {{
    // 503 means the app has not configured `[push] private_key` yet.
    console.error('web push is not configured on this server');
    return false;
  }}
  autumnPushCaptureCsrf(response);
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.subscribe({{
    // Chrome refuses a subscription without this, and it is also the honest
    // contract: every push this app sends raises a visible notification.
    userVisibleOnly: true,
    applicationServerKey: autumnDecodeVapidKey((await response.text()).trim()),
  }});
  const recorded = await fetch('{subscribe_path}', {{
    method: 'POST',
    headers: autumnPushHeaders(),
    // Send the session cookie so the server knows whose subscription this is.
    credentials: 'same-origin',
    body: JSON.stringify(subscription),
  }});
  return recorded.ok;
}};

// Undo the above: forget this browser's subscription, server-side and locally.
window.autumnPushUnsubscribe = async function autumnPushUnsubscribe() {{
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) {{
    return false;
  }}
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.getSubscription();
  if (!subscription) {{
    return true;
  }}
  // Unsubscribe can be the first call of the page, before anything primed the
  // token; one cheap GET is enough to obtain it.
  if (!autumnPushCsrf) {{
    autumnPushCaptureCsrf(
      await fetch('{key_path}', {{ credentials: 'same-origin' }})
    );
  }}
  await fetch('{unsubscribe_path}', {{
    method: 'POST',
    headers: autumnPushHeaders(),
    credentials: 'same-origin',
    body: JSON.stringify({{ endpoint: subscription.endpoint }}),
  }});
  return subscription.unsubscribe();
}};

// Declarative opt-in: no application JavaScript required.
document.addEventListener('click', (event) => {{
  // A synthetic `document.dispatchEvent(new MouseEvent('click'))` — the common
  // close-the-open-dropdown idiom — has `document` as its target, which has no
  // `closest`. Guard rather than throw on every such click.
  if (!(event.target instanceof Element)) {{
    return;
  }}
  const trigger = event.target.closest('[data-autumn-push-subscribe]');
  if (trigger) {{
    event.preventDefault();
    window.autumnPushSubscribe().catch(console.error);
  }}
}});
",
        key_path = autumn_web::push::router::VAPID_PUBLIC_KEY_PATH,
        subscribe_path = autumn_web::push::router::SUBSCRIBE_PATH,
        unsubscribe_path = autumn_web::push::router::UNSUBSCRIBE_PATH,
        csrf_token_header = autumn_web::push::router::CSRF_TOKEN_HEADER,
        csrf_header_name_header = autumn_web::push::router::CSRF_TOKEN_HEADER_NAME_HEADER,
    )
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
    let with_push_router = inject_push_router(&with_handlers);
    let route_entries = vec![
        "pwa_manifest".to_owned(),
        "pwa_service_worker".to_owned(),
        "pwa_register_js".to_owned(),
        "pwa_offline".to_owned(),
    ];
    update_main_rs(&with_push_router, &[], &route_entries)
}

/// The trailing marker on the mount line [`inject_push_router`] inserts.
///
/// Load-bearing for [`remove_push_router`]: destroy must remove only the line
/// *this generator* wrote. Without a marker it would also delete a mount the
/// developer had written themselves before ever running the generator — which
/// `Plan::revert` documents it never does.
const PUSH_ROUTER_MARKER: &str = "// added by `autumn generate pwa`";

/// The line [`inject_push_router`] inserts, and [`remove_push_router`] removes.
///
/// Mounting the framework's built-in push routes is what makes the generated
/// client snippet work: without it every `fetch` in `pwa-register.js` 404s.
const PUSH_ROUTER_LINE: &str =
    "        .merge(autumn_web::push::router()) // added by `autumn generate pwa`";

/// Mount `autumn_web::push::router()` on the app builder in `main()`.
///
/// Anchored on the `autumn_web::app()` line that every scaffolded `main.rs`
/// opens its builder chain with. Idempotent, and — like
/// [`inject_pwa_meta_into_head`] — a **no-op** when no usable anchor is found,
/// rather than a guess: an app whose `main.rs` has been restructured keeps
/// compiling and mounts the router itself with the one line the guide shows.
/// The caller reports that no-op as a warning; a silently unmounted router
/// would look like success and 404 on every call from the generated snippet.
///
/// # Anchor selection
///
/// Matching `autumn_web::app()` by text alone is not safe: this project's own
/// documentation (and `autumn_web::push::router`'s doc comment) shows a
/// quick-start snippet containing that exact line, so a `main.rs` that pasted
/// it into a comment would get the mount spliced **into the comment** —
/// producing a file that does not compile *and* a `main()` that never mounts
/// the router. So the scan walks lines with a byte cursor (never re-locating
/// by text, which could land on an earlier occurrence), skips anything inside
/// a comment, and only accepts an anchor at or after `async fn main`.
fn inject_push_router(source: &str) -> String {
    if source.contains("autumn_web::push::router()") {
        return source.to_owned();
    }
    let Some(anchor_end) = push_router_anchor(source) else {
        return source.to_owned();
    };
    let mut result = String::with_capacity(source.len() + PUSH_ROUTER_LINE.len() + 1);
    result.push_str(&source[..anchor_end]);
    result.push('\n');
    result.push_str(PUSH_ROUTER_LINE);
    result.push_str(&source[anchor_end..]);
    result
}

/// Byte offset just past the builder-opening `autumn_web::app()` inside
/// `main()`, or `None` when there is no unambiguous anchor.
///
/// See [`inject_push_router`]'s "Anchor selection" for why each rule is here.
fn push_router_anchor(source: &str) -> Option<usize> {
    let mut offset = 0_usize;
    let mut in_block_comment = false;
    let mut seen_main = false;

    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let line_start = offset;
        offset += line.len();

        // Track block comments so a commented-out builder never anchors.
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        // `//`, `///`, `//!`, and continuation lines of a block comment.
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }

        if trimmed.contains("async fn main") {
            seen_main = true;
        }
        // Only a line that ENDS with the builder opener is an anchor: a
        // single-line chain (`autumn_web::app().routes(…).run().await;`) has
        // no place to insert a `.merge(…)` line, so it is deliberately left
        // alone and reported instead of being spliced mid-expression.
        if seen_main && line.trim_end().ends_with("autumn_web::app()") {
            return Some(line_start + line.trim_end().len());
        }
    }
    None
}

/// Inverse of [`inject_push_router`]: remove exactly the line it inserted.
///
/// Matched on the [`PUSH_ROUTER_MARKER`] this generator writes, so a mount the
/// developer added themselves is left untouched — `Plan::revert`'s contract is
/// that it never removes content the plan did not itself add. A no-op when the
/// marked line is absent (already destroyed, or hand-edited away).
fn remove_push_router(existing: &str) -> String {
    if !existing.contains(PUSH_ROUTER_MARKER) {
        return existing.to_owned();
    }
    let mut out = String::with_capacity(existing.len());
    for line in existing.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.contains("autumn_web::push::router()") && trimmed.ends_with(PUSH_ROUTER_MARKER) {
            continue;
        }
        out.push_str(line);
    }
    out
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

/// True when `source` defines a `layout()` function with fewer than 4
/// parameters — the pre-`nav_bar` scaffold shape (`layout(title, flash,
/// content)`) rather than the current one (`layout(title, current_path,
/// flash, content)`). `pwa_offline`'s generated call assumes 4 positional
/// arguments, so this gates [`plan_pwa`] with an actionable error instead
/// of emitting code with the wrong arity.
///
/// Checks parameter *count*, not a specific name — Rust calls are
/// positional, so a caller who named their path parameter `path` or
/// `request_path` instead of `current_path` still compiles fine and must
/// not be rejected.
fn layout_missing_current_path(source: &str) -> bool {
    let Some(start) = source.find("fn layout(") else {
        return false;
    };
    let after_paren = &source[start + "fn layout(".len()..];
    let Some(params) = balanced_prefix(after_paren) else {
        return false;
    };
    count_params(params) < 4
}

/// Count comma-separated parameters in a (possibly multi-line, possibly
/// trailing-comma) parameter list — the trailing comma rustfmt always adds
/// when it wraps a signature onto separate lines doesn't itself count as an
/// extra parameter.
fn count_params(params: &str) -> usize {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let commas = count_top_level_commas(trimmed);
    if trimmed.ends_with(',') {
        commas
    } else {
        commas + 1
    }
}

/// The prefix of `s` up to (not including) the `)` that closes the opening
/// paren implicit in the caller's position — tracks `(`/`[` nesting (e.g. a
/// closure-typed parameter like `Fn(i32) -> bool`) so an inner `)` doesn't
/// end the scan early. Returns `None` if unbalanced.
fn balanced_prefix(s: &str) -> Option<&str> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' if depth == 0 => return Some(&s[..i]),
            ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Count commas in `s` that aren't nested inside `(...)`/`[...]` — a cheap
/// proxy for "how many parameters does this signature have" without a real
/// parser. Doesn't track `<...>` generic nesting, so a parameter type with a
/// top-level comma inside angle brackets (e.g. `HashMap<K, V>`) would
/// over-count — harmless here since over-counting only makes this check
/// *more* permissive, never a false rejection.
fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0i32;
    let mut count = 0;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

/// Append `pwa_manifest`, `pwa_service_worker`, `pwa_register_js`, and `pwa_offline`
/// handler functions just before `#[autumn_web::main]`.  Idempotent — skipped when
/// `pwa_manifest` is already defined.
const PWA_HANDLERS_BLOCK: &str = "\
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

fn inject_pwa_handlers(source: &str) -> String {
    if source.contains("async fn pwa_manifest()") {
        return source.to_owned();
    }

    // Insert before the line that is exactly `#[autumn_web::main]` — a naive
    // substring search would also match the text inside a comment that merely
    // mentions the macro (e.g. `// routes are wired under #[autumn_web::main]`)
    // and slice the file mid-comment. Appends at end as fallback.
    find_main_attr_line_start(source).map_or_else(
        || {
            let mut result = source.to_owned();
            if !result.ends_with('\n') {
                result.push('\n');
            }
            result.push('\n');
            result.push_str(PWA_HANDLERS_BLOCK);
            result
        },
        |pos| {
            let mut result = String::with_capacity(source.len() + PWA_HANDLERS_BLOCK.len());
            result.push_str(&source[..pos]);
            result.push_str(PWA_HANDLERS_BLOCK);
            result.push_str(&source[pos..]);
            result
        },
    )
}

/// Byte offset of the start of the first line whose entire content (modulo
/// surrounding whitespace) is `#[autumn_web::main]`, or `None` when no such
/// line exists. Line-exact matching prevents false positives on comments
/// that mention the attribute.
fn find_main_attr_line_start(source: &str) -> Option<usize> {
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        if line.trim() == "#[autumn_web::main]" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn indent_count(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Inverse of [`inject_pwa_into_main`] (`autumn destroy`, issue #1048).
///
/// Removes the PWA `<link>`/`<meta>` tags from the `head {}` block and the
/// `pwa_manifest`/`pwa_service_worker`/`pwa_register_js`/`pwa_offline`
/// handler functions this generator injected. A no-op wherever its target
/// isn't present (already destroyed, or hand-edited away).
pub(super) fn remove_pwa_injection(existing: &str) -> String {
    let without_router = remove_push_router(existing);
    let without_handlers = remove_pwa_handlers(&without_router);
    remove_pwa_meta_from_head(&without_handlers)
}

/// Inverse of [`inject_pwa_meta_into_head`]. Removes exactly the four lines
/// that function inserts (three tag lines + the register-script line),
/// identified by the unique `/pwa-register.js` script line together with the
/// three lines immediately preceding it matching the known tag text
/// (ignoring indentation) — restoring the `head {}` block byte-identically.
///
/// A no-op if the script line isn't present, or the three preceding lines
/// don't match (hand-edited — never guesses at a partial match).
fn remove_pwa_meta_from_head(existing: &str) -> String {
    const SCRIPT_LINE: &str = "script src=\"/pwa-register.js\" {}";
    const PRECEDING_LINES: [&str; 3] = [
        "link rel=\"manifest\" href=\"/manifest.webmanifest\";",
        "meta name=\"theme-color\" content=\"#ffffff\";",
        "link rel=\"apple-touch-icon\" href=\"/static/icons/icon.svg\";",
    ];
    let lines: Vec<&str> = existing.lines().collect();
    let Some(script_idx) = lines.iter().position(|l| l.trim() == SCRIPT_LINE) else {
        return existing.to_owned();
    };
    if script_idx < PRECEDING_LINES.len() {
        return existing.to_owned();
    }
    let start = script_idx - PRECEDING_LINES.len();
    let matches = PRECEDING_LINES
        .iter()
        .enumerate()
        .all(|(offset, expected)| lines[start + offset].trim() == *expected);
    if !matches {
        return existing.to_owned();
    }
    let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len());
    new_lines.extend_from_slice(&lines[..start]);
    new_lines.extend_from_slice(&lines[script_idx + 1..]);
    let mut out = new_lines.join("\n");
    if existing.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Inverse of [`inject_pwa_handlers`]. Removes the exact
/// [`PWA_HANDLERS_BLOCK`] text that function inserts verbatim.
///
/// A no-op if the block isn't present (already destroyed, or hand-edited
/// away).
fn remove_pwa_handlers(existing: &str) -> String {
    let Some(pos) = existing.find(PWA_HANDLERS_BLOCK) else {
        return existing.to_owned();
    };
    let mut out = String::with_capacity(existing.len() - PWA_HANDLERS_BLOCK.len());
    out.push_str(&existing[..pos]);
    out.push_str(&existing[pos + PWA_HANDLERS_BLOCK.len()..]);
    out
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
            "[package]\nname=\"my-app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\n[dependencies]\nautumn-web = \"0.6.0\"\n",
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
    fn plan_pwa_accepts_layout_with_renamed_path_parameter() {
        // Rust calls are positional — a layout() with 4 parameters compiles
        // fine against pwa_offline's 4-arg call regardless of what the
        // second parameter is named. Requiring the literal name
        // "current_path" would reject valid, already-migrated apps that
        // simply chose a different name (e.g. `path`, `request_path`).
        let renamed_main = DEFAULT_MAIN.replace(
            "pub fn layout(title: &str, current_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {",
            "pub fn layout(title: &str, request_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {",
        );
        assert_ne!(
            renamed_main, DEFAULT_MAIN,
            "replacement must actually match"
        );
        let tmp = project_with_main(&renamed_main);
        plan_pwa(tmp.path())
            .expect("a differently-named 4th parameter must still be accepted (arity, not name)");
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
    fn inject_handlers_ignores_main_attr_mentioned_in_comment() {
        // A comment that merely mentions `#[autumn_web::main]` must not be
        // treated as the insertion point — the naive substring search used to
        // slice the file mid-comment, leaving the handlers inside the comment
        // text and the file uncompilable.
        let source = "\
use autumn_web::prelude::*;

// Route handlers are registered under #[autumn_web::main] via routes![].
fn helper() {}

#[autumn_web::main]
async fn main() {
    autumn_web::app().routes(routes![]).run().await;
}
";
        let updated = inject_pwa_handlers(source);
        assert!(
            updated.contains(
                "// Route handlers are registered under #[autumn_web::main] via routes![]."
            ),
            "comment must be untouched: {updated}"
        );
        let handler_pos = updated.find("async fn pwa_manifest()").unwrap();
        let helper_pos = updated.find("fn helper()").unwrap();
        let real_main_pos = updated.find("\n#[autumn_web::main]\n").unwrap();
        assert!(
            helper_pos < handler_pos && handler_pos < real_main_pos,
            "handlers must be inserted immediately before the real attribute line: {updated}"
        );
    }

    #[test]
    fn inject_handlers_appends_when_only_comment_mentions_main_attr() {
        let source = "// see #[autumn_web::main]\nfn main() {}\n";
        let updated = inject_pwa_handlers(source);
        assert!(
            updated.starts_with("// see #[autumn_web::main]\nfn main() {}\n"),
            "original source must be untouched: {updated}"
        );
        assert!(
            updated.ends_with(PWA_HANDLERS_BLOCK),
            "handlers must be appended at end as fallback: {updated}"
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
    fn generate_then_destroy_pwa_round_trips_to_original_project_state() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let cargo_path = tmp.path().join("Cargo.toml");
        let main_path = tmp.path().join("src/main.rs");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("static/manifest.webmanifest").exists());
        assert!(
            fs::read_to_string(&main_path)
                .unwrap()
                .contains("async fn pwa_manifest()")
        );
        assert!(
            fs::read_to_string(&cargo_path)
                .unwrap()
                .contains("system-tests")
        );

        plan_pwa(tmp.path())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("static/manifest.webmanifest").exists());
        assert!(!tmp.path().join("static").exists());
        assert!(!tmp.path().join("tests/system/pwa_smoke.rs").exists());
        assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn destroy_pwa_still_works_after_main_rs_reverted_to_pre_nav_bar_shape() {
        // issue #1048 PR review: `plan_pwa` (used to recompute the plan for
        // destroy too) rejects `main.rs` whenever `layout()` has fewer than
        // 4 parameters. That precondition only matters for a *fresh*
        // generate — but a common cleanup order (hand-revert `main.rs` to
        // the pre-`nav_bar` shape, or run a competing generator that
        // rewrites `layout()`) would otherwise strand every PWA file
        // because `destroy pwa` fails before `Plan::revert` ever runs.
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("static/manifest.webmanifest").exists());

        // Simulate main.rs having been rewritten to the old, incompatible
        // arity after PWA was generated (still contains the PWA injections,
        // just with layout() narrowed back down).
        let main_path = tmp.path().join("src/main.rs");
        let with_pwa = fs::read_to_string(&main_path).unwrap();
        let narrowed = with_pwa.replace(
            "pub fn layout(title: &str, current_path: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {",
            "pub fn layout(title: &str, flash: maud::Markup, content: maud::Markup) -> maud::Markup {",
        );
        assert_ne!(narrowed, with_pwa, "replacement must actually match");
        fs::write(&main_path, &narrowed).unwrap();
        assert!(plan_pwa(tmp.path()).is_err());

        plan_pwa_destroy_fallback(tmp.path())
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(!tmp.path().join("static/manifest.webmanifest").exists());
        assert!(!tmp.path().join("static").exists());
        assert!(!tmp.path().join("tests/system/pwa_smoke.rs").exists());
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

    // ── Web Push (issue #1392) ──────────────────────────────────────────────

    /// The text an emitted action would write.
    fn action_contents(action: &crate::generate::emit::Action) -> String {
        match action {
            crate::generate::emit::Action::Create { contents, .. }
            | crate::generate::emit::Action::Modify { contents, .. }
            | crate::generate::emit::Action::CreateIfAbsent { contents, .. } => contents.clone(),
            crate::generate::emit::Action::CreateBytes { bytes, .. } => {
                String::from_utf8_lossy(bytes).into_owned()
            }
        }
    }

    #[test]
    fn service_worker_has_a_push_handler() {
        let sw = render_service_worker();
        assert!(
            sw.contains("addEventListener('push'"),
            "without a `push` handler the browser shows nothing when the tab is closed — \
             the entire point of Web Push:\n{sw}"
        );
        assert!(
            sw.contains("showNotification"),
            "the push handler must actually raise a notification:\n{sw}"
        );
    }

    #[test]
    fn push_handler_reads_the_frameworks_json_payload() {
        let sw = render_service_worker();
        // The framework sends `{title, body, url?, icon?}` — the SW must read
        // those exact keys or every notification renders blank.
        for key in ["title", "body", "icon"] {
            assert!(
                sw.contains(key),
                "the push handler must read `{key}` from the payload:\n{sw}"
            );
        }
    }

    #[test]
    fn push_handler_survives_a_payload_that_is_not_json() {
        // A push with no payload (or a non-JSON one from another sender) must
        // still show *something* rather than throwing inside the SW, which
        // browsers surface as a generic "site updated in the background".
        let sw = render_service_worker();
        assert!(
            sw.contains("catch"),
            "the push handler must guard payload parsing:\n{sw}"
        );
    }

    #[test]
    fn service_worker_has_a_notificationclick_handler_that_focuses_or_opens() {
        let sw = render_service_worker();
        assert!(
            sw.contains("addEventListener('notificationclick'"),
            "clicking a notification must do something:\n{sw}"
        );
        assert!(
            sw.contains("clients.matchAll"),
            "clicking must FOCUS an already-open tab rather than always opening \
             a duplicate one:\n{sw}"
        );
        assert!(
            sw.contains("openWindow"),
            "…and open a new window when none is focusable:\n{sw}"
        );
        assert!(
            sw.contains("notification.close()"),
            "the notification must be dismissed on click:\n{sw}"
        );
    }

    #[test]
    fn client_snippet_subscribes_against_the_frameworks_public_key_endpoint() {
        let js = render_pwa_register_js();
        assert!(
            js.contains(autumn_web::push::router::VAPID_PUBLIC_KEY_PATH),
            "the snippet must fetch the key from the built-in endpoint so the two \
             can never drift:\n{js}"
        );
        assert!(
            js.contains(autumn_web::push::router::SUBSCRIBE_PATH),
            "the snippet must POST the subscription to the built-in endpoint:\n{js}"
        );
        assert!(
            js.contains("pushManager.subscribe"),
            "the snippet must actually subscribe:\n{js}"
        );
        assert!(
            js.contains("userVisibleOnly"),
            "Chrome refuses a subscription without `userVisibleOnly: true`:\n{js}"
        );
    }

    #[test]
    fn client_snippet_converts_the_key_to_the_uint8array_the_api_requires() {
        // `applicationServerKey` takes a BufferSource, not the base64url
        // string the endpoint serves; passing the string fails at runtime.
        let js = render_pwa_register_js();
        assert!(
            js.contains("Uint8Array"),
            "the base64url key must be decoded to a Uint8Array:\n{js}"
        );
        assert!(js.contains("atob"), "…via base64 decoding:\n{js}");
        assert!(
            js.contains("replace"),
            "…after translating base64url's -/_ back to +/ for atob:\n{js}"
        );
    }

    #[test]
    fn client_snippet_never_prompts_for_permission_on_page_load() {
        // A permission prompt fired on load is the single fastest way to get
        // permanently blocked, and Chrome/Firefox both penalise it. The
        // snippet must expose an explicit opt-in instead.
        //
        // The check has to be positional, not a substring test: `contains`
        // cannot tell a call INSIDE `autumnPushSubscribe` from one at top
        // level, which is exactly the failure being guarded against. So find
        // every `requestPermission` and require each to sit inside a function
        // body — i.e. on an indented line.
        let js = render_pwa_register_js();
        let prompts: Vec<&str> = js
            .lines()
            .filter(|line| line.contains("requestPermission"))
            .collect();
        assert!(
            !prompts.is_empty(),
            "the snippet must request permission somewhere:\n{js}"
        );
        for line in prompts {
            assert!(
                line.starts_with(' '),
                "`requestPermission` must be called inside the opt-in function, never at \
                 top level where it would run on page load:\n{line}"
            );
        }
        assert!(
            js.contains("window.autumnPushSubscribe = "),
            "the snippet must expose a callable opt-in entry point:\n{js}"
        );
        assert!(
            js.contains("data-autumn-push-subscribe"),
            "…and wire it to a declarative opt-in attribute so an app needs no JS \
             of its own:\n{js}"
        );
    }

    #[test]
    fn client_snippet_sends_a_csrf_token_with_both_push_posts() {
        // Autumn's production smart defaults turn CSRF on, and the CSRF cookie
        // is HttpOnly — so without this the generated subscribe/unsubscribe
        // POSTs 403 and push opt-in is unusable in exactly the environment it
        // is meant for.
        let js = render_pwa_register_js();
        assert!(
            js.contains(autumn_web::push::router::CSRF_TOKEN_HEADER),
            "the snippet must read the token the public-key response serves:\n{js}"
        );
        assert!(
            js.contains(autumn_web::push::router::CSRF_TOKEN_HEADER_NAME_HEADER),
            "…and the configured header name to send it back in:\n{js}"
        );
        // Every POST must go through the helper that attaches it — a `fetch`
        // with a hand-written `content-type` header would silently skip it.
        let posts = js.matches("method: 'POST'").count();
        assert_eq!(
            posts, 2,
            "expected exactly the subscribe and unsubscribe POSTs"
        );
        assert_eq!(
            js.matches("headers: autumnPushHeaders()").count(),
            posts,
            "every push POST must attach the CSRF token:\n{js}"
        );
    }

    #[test]
    fn client_snippet_still_falls_back_to_the_csrf_meta_tag() {
        // An app that already publishes the token the way `autumn generate
        // auth` does must keep working.
        let js = render_pwa_register_js();
        assert!(
            js.contains("meta[name=csrf-token]"),
            "the meta-tag fallback must remain:\n{js}"
        );
    }

    #[test]
    fn client_snippet_sends_credentials_on_every_push_request() {
        // The server resolves the subscriber from the session; without the
        // cookie every call is an anonymous 401.
        let js = render_pwa_register_js();
        assert_eq!(
            js.matches("credentials: 'same-origin'").count(),
            4,
            "two POSTs plus the two public-key GETs must all carry the session:\n{js}"
        );
    }

    #[test]
    fn client_snippet_skips_push_entirely_when_the_browser_lacks_support() {
        let js = render_pwa_register_js();
        assert!(
            js.contains("PushManager"),
            "the snippet must feature-detect before touching the Push API — Safari \
             on older iOS has service workers but no PushManager:\n{js}"
        );
    }

    #[test]
    fn plan_creates_the_push_subscriptions_migration() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        let up = plan
            .actions
            .iter()
            .find(|a| {
                a.path()
                    .to_string_lossy()
                    .contains("create_push_subscriptions")
                    && a.path().ends_with("up.sql")
            })
            .expect("the pwa generator must scaffold the push_subscriptions table");
        let sql = action_contents(up);
        assert!(sql.contains("CREATE TABLE"), "{sql}");
        for column in ["principal_id", "endpoint", "p256dh", "auth"] {
            assert!(
                sql.contains(column),
                "the DDL must match the framework store's columns; missing `{column}`:\n{sql}"
            );
        }
        assert!(
            sql.to_uppercase().contains("UNIQUE"),
            "`endpoint` must be UNIQUE — it is what makes the store's upsert atomic \
             rather than a racy select-then-insert:\n{sql}"
        );
    }

    /// Pins the generated Postgres DDL against the exact schema the framework
    /// store is tested on.
    ///
    /// The two live in different crates — `autumn-web`'s tests cannot depend
    /// on `autumn-cli` — so this assertion and the `CREATE_PUSH_SUBSCRIPTIONS_SQL`
    /// constant in `autumn/tests/integration/push_end_to_end.rs` are the only
    /// thing binding them. A column renamed, retyped, or dropped on either
    /// side fails here.
    #[test]
    fn generated_push_ddl_matches_the_ddl_the_framework_store_is_tested_against() {
        let sql = push_subscriptions_up_sql(DatabaseBackend::Postgres);

        for column in [
            "principal_id TEXT NOT NULL",
            "endpoint TEXT NOT NULL",
            "p256dh TEXT NOT NULL",
            "auth TEXT NOT NULL",
        ] {
            assert!(
                sql.contains(column),
                "the store's diesel `table!` expects `{column}`:\n{sql}"
            );
        }
        assert!(
            sql.contains("id BIGSERIAL PRIMARY KEY"),
            "the store selects `id` as BigInt:\n{sql}"
        );
        // The store maps `created_at` as `Timestamptz`. The shared helper emits
        // a plain `TIMESTAMP`, so this generator rewrites that one column — if
        // the helper's spacing ever changes, the rewrite silently no-ops and
        // this is what catches it.
        assert!(
            sql.contains("created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()"),
            "`created_at` must be TIMESTAMPTZ to match the store's mapping:\n{sql}"
        );
        assert!(
            !sql.contains("created_at TIMESTAMP NOT NULL"),
            "the TIMESTAMP → TIMESTAMPTZ rewrite did not apply:\n{sql}"
        );
    }

    #[test]
    fn push_migration_makes_endpoint_unique_specifically() {
        // `contains("UNIQUE")` alone would pass with the constraint on the
        // wrong column — and `endpoint` being unique is what makes the store's
        // `ON CONFLICT (endpoint) DO UPDATE` atomic rather than a racy
        // select-then-insert.
        let sql = push_subscriptions_up_sql(DatabaseBackend::Postgres);
        let unique_line = sql
            .lines()
            .find(|line| line.to_uppercase().contains("UNIQUE"))
            .unwrap_or_else(|| panic!("no UNIQUE constraint in:\n{sql}"));
        assert!(
            unique_line.contains("(endpoint)"),
            "UNIQUE must be on `endpoint`, not another column: {unique_line}"
        );
    }

    #[test]
    fn push_migration_indexes_principal_id_for_the_send_path() {
        // Every send does `WHERE principal_id = …`; without the index that is
        // a sequential scan of every subscription in the system.
        let sql = push_subscriptions_up_sql(DatabaseBackend::Postgres);
        assert!(
            sql.contains("(principal_id)"),
            "`principal_id` must be indexed:\n{sql}"
        );
    }

    #[test]
    fn push_migration_is_emitted_for_sqlite_too() {
        let sql = push_subscriptions_up_sql(DatabaseBackend::Sqlite);
        assert!(sql.contains("CREATE TABLE"), "{sql}");
        assert!(
            !sql.contains("BIGSERIAL"),
            "SQLite has no BIGSERIAL:\n{sql}"
        );
        assert!(
            !sql.contains("TIMESTAMPTZ"),
            "the SQLite lane stores RFC 3339 TEXT, matching TimestamptzSqlite:\n{sql}"
        );
    }

    #[test]
    fn push_migration_has_a_matching_down() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let plan = plan_pwa(tmp.path()).unwrap();
        let down = plan
            .actions
            .iter()
            .find(|a| {
                a.path()
                    .to_string_lossy()
                    .contains("create_push_subscriptions")
                    && a.path().ends_with("down.sql")
            })
            .expect("every migration needs a down");
        let sql = action_contents(down);
        assert!(sql.contains("DROP TABLE"), "{sql}");
    }

    #[test]
    fn re_running_reuses_the_existing_push_migration_directory() {
        // Two `*_create_push_subscriptions` directories would fail the next
        // `autumn migrate` on the duplicate table — the same singleton rule
        // the notifications generator follows.
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags {
                force: true,
                ..Flags::default()
            })
            .unwrap();

        let dirs: Vec<_> = fs::read_dir(tmp.path().join("migrations"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with("_create_push_subscriptions")
            })
            .collect();
        assert_eq!(
            dirs.len(),
            1,
            "re-running must reuse the existing migration directory, not mint a second"
        );
    }

    #[test]
    fn inject_mounts_the_built_in_push_router() {
        let injected = inject_pwa_into_main(DEFAULT_MAIN);
        assert!(
            injected.contains("autumn_web::push::router()"),
            "without the router mounted the generated client snippet 404s on every \
             call:\n{injected}"
        );
    }

    #[test]
    fn inject_push_router_is_idempotent() {
        let once = inject_pwa_into_main(DEFAULT_MAIN);
        let twice = inject_pwa_into_main(&once);
        assert_eq!(once, twice);
        assert_eq!(
            twice.matches("autumn_web::push::router()").count(),
            1,
            "a second run must not mount the router twice:\n{twice}"
        );
    }

    #[test]
    fn generate_then_destroy_removes_the_push_router_mount_too() {
        let tmp = project_with_main(DEFAULT_MAIN);
        let main_path = tmp.path().join("src/main.rs");
        let original_main = fs::read_to_string(&main_path).unwrap();

        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(
            fs::read_to_string(&main_path)
                .unwrap()
                .contains("autumn_web::push::router()")
        );

        plan_pwa(tmp.path())
            .unwrap()
            .revert(Flags::default())
            .unwrap();
        assert_eq!(
            fs::read_to_string(&main_path).unwrap(),
            original_main,
            "destroy must restore main.rs byte-for-byte, router mount included"
        );
    }

    #[test]
    fn dry_run_writes_no_push_files() {
        let tmp = project_with_main(DEFAULT_MAIN);
        plan_pwa(tmp.path())
            .unwrap()
            .execute(Flags {
                dry_run: true,
                ..Flags::default()
            })
            .unwrap();
        assert!(
            !tmp.path().join("migrations").exists(),
            "--dry-run must write nothing"
        );
        assert!(
            !fs::read_to_string(tmp.path().join("src/main.rs"))
                .unwrap()
                .contains("autumn_web::push::router()")
        );
    }
}
