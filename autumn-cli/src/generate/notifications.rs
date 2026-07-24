//! `autumn generate notifications` — scaffold the in-app notification feed
//! (issue #1148) on top of the framework's `Notifications` extractor.
//!
//! Unlike the name-parameterised generators, notifications are a fixed,
//! single-instance resource (`autumn_web::notifications` reads one
//! conventional `notifications` table), so the command takes no name. The
//! generator produces:
//! - `migrations/<ts>_create_notifications/{up,down}.sql` — backend-aware
//!   DDL for the `notifications` table, matching the framework store's
//!   diesel `table!` mapping byte-for-type (see [`migration_up_sql`]).
//! - `src/notifications.rs` — notify / feed / unread-count / mark-read /
//!   mark-all-read route handlers over the `Notifications` extractor. The
//!   feed/mark routes are **session-scoped**: the recipient is derived
//!   server-side from the `Session` (seeded by a clearly-marked demo-only
//!   login route), never from a path or body parameter, so the scaffold
//!   never teaches an IDOR pattern (PR #2144 review).
//! - `src/main.rs` — `mod notifications;` declaration and the generated
//!   routes registered in `routes![...]`.
//! - `tests/notifications_feed.rs` — a smoke test exercising the full
//!   demo-login → notify → list → mark-read → unread-count flow through
//!   the in-process `TestApp`, including the cross-recipient isolation
//!   check (no database needed: with no DB configured the extractor falls
//!   back to its in-process memory store).
//! - `Cargo.toml` — `serde`/`serde_json` dependencies and the `tokio`
//!   dev-dependency features the smoke test needs for `#[tokio::test]`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use autumn_web::config::DatabaseBackend;
use autumn_web::notifications::NOTIFICATIONS_TABLE;

use super::dsl::{Field, FieldConstraints, FieldKind, IdType};
use super::emit::Plan;
use super::model::ensure_cargo_dependencies;
use super::schema_edit::{
    create_table_sql_with_metadata_and_id_for, drop_table_sql,
    ensure_dev_dependency_tokio_test_features, update_main_rs,
};
use super::{GenerateError, detect_backend, ensure_project_root, read_or_empty, timestamp_now};

/// Cargo dependencies the generated module and smoke test require: the
/// notify body derives `Deserialize` and carries a `serde_json::Value`
/// payload (the same payload type `Notifications::notify` takes).
const NOTIFICATIONS_DEPS: &[(&str, &str)] = &[
    ("serde", "{ version = \"1\", features = [\"derive\"] }"),
    ("serde_json", "\"1\""),
];

/// Compute the file actions for `autumn generate notifications`.
///
/// # Errors
/// Project layout errors surface here.
pub fn plan_notifications(project_root: &Path) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;

    // The emitted DDL is backend-aware, resolved the same way `autumn
    // migrate` resolves the database URL (env vars, then the profile-merged
    // `autumn.toml` / `.env`); Postgres is the default when nothing is
    // configured.
    let backend = detect_backend(project_root);

    let mut plan = Plan::new(project_root);

    // ── src/notifications.rs ────────────────────────────────────────────────
    plan.create(
        project_root.join("src").join("notifications.rs"),
        render_app_module(),
    );

    // ── src/main.rs: add mod notifications; and route entries ───────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|e| {
        GenerateError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", main_path.display()),
        ))
    })?;
    let route_entries = route_entries();
    plan.modify(
        main_path.clone(),
        update_main_rs(&main_existing, &["notifications"], &route_entries),
    );
    // `mod notifications;` is stripped by `emit::sync_main_rs_mod_declarations`
    // once `src/notifications.rs` is gone — only the route entries need an
    // explicit revert here (same split the channel generator uses).
    plan.push_revert(crate::generate::emit::Revert::RoutesEntries {
        path: main_path,
        entries: route_entries,
    });

    // ── migrations/<ts>_create_notifications/{up,down}.sql ──────────────────
    // Notifications are a singleton scaffold: re-running (especially with
    // `--force` to refresh it) must not mint a SECOND
    // `*_create_notifications` directory — two `CREATE TABLE notifications`
    // migrations would fail the next `autumn migrate` on the duplicate table
    // and make destroy's suffix match ambiguous (PR #2144 finding B). So
    // reuse an existing `*_create_notifications` dir when one is present,
    // minting a fresh timestamped dir only on a clean project. Destroy still
    // matches by the `_create_notifications` suffix either way.
    let migration_dir = existing_notifications_migration_dir(project_root).unwrap_or_else(|| {
        project_root
            .join("migrations")
            .join(format!("{}_create_{NOTIFICATIONS_TABLE}", timestamp_now()))
    });
    plan.create(migration_dir.join("up.sql"), migration_up_sql(backend));
    plan.create(
        migration_dir.join("down.sql"),
        drop_table_sql(NOTIFICATIONS_TABLE),
    );

    // ── tests/notifications_feed.rs ─────────────────────────────────────────
    plan.create(
        project_root.join("tests").join("notifications_feed.rs"),
        render_smoke_test(),
    );

    // ── Cargo.toml: serde/serde_json deps + tokio dev-dep test features ─────
    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = read_or_empty(&cargo_path);
    let mut updated_cargo = ensure_cargo_dependencies(&cargo_existing, NOTIFICATIONS_DEPS);
    updated_cargo = ensure_dev_dependency_tokio_test_features(&updated_cargo);
    if updated_cargo != cargo_existing {
        plan.modify(cargo_path.clone(), updated_cargo);
    }
    // Pushed unconditionally — see `plan_cargo_deps`'s matching comment in
    // model.rs: destroy recomputes this plan against the already-generated
    // Cargo.toml, where these entries are by definition already present.
    // Notifications are a fixed, single-instance resource whose only owned
    // file is `src/notifications.rs` itself, so there is no sibling resource
    // directory to gate on — passing the module file as `owner_dir` makes
    // the same-generator sibling check vacuous (`read_dir` on a file never
    // yields siblings), leaving `Revert::CargoDeps`'s project-wide
    // crate-reference scan as the real guard against stripping a dependency
    // some other code still uses.
    plan.push_revert(crate::generate::emit::Revert::CargoDeps {
        path: cargo_path,
        names: NOTIFICATIONS_DEPS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect(),
        owner_dir: project_root.join("src").join("notifications.rs"),
    });

    Ok(plan)
}

/// Fully-qualified route entries for `routes![...]` wiring in `main.rs`.
///
/// The demo login is deliberately **absent**: it is a test-only
/// session-seeding route declared inside the smoke test, never wired into
/// the production router — shipping it would expose
/// `POST /notifications/demo_login/{id}` and let an unauthenticated visitor
/// impersonate any recipient (PR #2144 finding A).
fn route_entries() -> Vec<String> {
    [
        "notify",
        "feed",
        "unread_count",
        "mark_read",
        "mark_all_read",
    ]
    .iter()
    .map(|handler| format!("notifications::{handler}"))
    .collect()
}

/// The existing `migrations/<ts>_create_notifications/` directory, if the
/// project already has one — so a re-run reuses it in place rather than
/// minting a second singleton migration (PR #2144 finding B). Matched by the
/// same `_create_notifications` suffix destroy uses (see
/// `emit::resolve_migration_removal`), so the two stay in agreement.
///
/// If more than one somehow exists (a hand-added duplicate), the
/// lexicographically-smallest is chosen deterministically — the same tie-break
/// `emit`'s destroy scan applies — rather than depending on `read_dir` order.
fn existing_notifications_migration_dir(project_root: &Path) -> Option<std::path::PathBuf> {
    let suffix = format!("_create_{NOTIFICATIONS_TABLE}");
    let mut matches: Vec<std::path::PathBuf> = std::fs::read_dir(project_root.join("migrations"))
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

/// The four non-`id` columns of the `notifications` table, expressed in the
/// migration DSL so the DDL comes out of the same
/// [`create_table_sql_with_metadata_and_id_for`] helper every other
/// generator uses (`id` and `created_at` are contributed by the helper
/// itself).
fn notification_fields() -> Vec<Field> {
    let field = |name: &str, kind: FieldKind, nullable: bool| Field {
        name: name.to_owned(),
        kind,
        nullable,
        variants: Vec::new(),
        unique: false,
        constraints: FieldConstraints::default(),
        state_machine: None,
    };
    vec![
        field("recipient_id", FieldKind::I64, false),
        field("kind", FieldKind::String, false),
        field("payload", FieldKind::String, false),
        field("read_at", FieldKind::DateTime, true),
    ]
}

/// Build `up.sql` for the target `backend`.
///
/// The column types must match the framework store's diesel `table!` in
/// `autumn_web::notifications` (`BigInt`/`Text`/`Text`/
/// `Nullable<Timestamptz>`/`Timestamptz`; `TimestamptzSqlite` — RFC 3339
/// `TEXT` — on `SQLite`): that store, not any generated model, is what reads
/// this table. The shared helper's stock `created_at` column is `TIMESTAMP`
/// (the model generator's convention, paired with a `Timestamp` `schema.rs`
/// entry), so the Postgres output rewrites that one column to `TIMESTAMPTZ`,
/// keeping it in lockstep with the framework's `Timestamptz` mapping and the
/// `CREATE_NOTIFICATIONS_SQL` DDL its integration tests pin. The `SQLite`
/// output already matches (`TEXT` + `CURRENT_TIMESTAMP` default) and is left
/// exactly as the helper emits it.
fn migration_up_sql(backend: DatabaseBackend) -> String {
    let indexes: BTreeSet<String> = std::iter::once("recipient_id".to_owned()).collect();
    let sql = create_table_sql_with_metadata_and_id_for(
        backend,
        NOTIFICATIONS_TABLE,
        &notification_fields(),
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

/// The session key, `NotifyBody` type, `current_recipient_id` helper, and
/// the five production route handlers, shared **verbatim** by
/// `src/notifications.rs` and `tests/notifications_feed.rs` — `tests/*.rs`
/// integration binaries cannot import the app's own binary crate (there is
/// no `src/lib.rs`), so the smoke test re-declares the production handlers,
/// and rendering both files from this one blob keeps them byte-for-byte in
/// sync by construction (the same contract `generate channel`'s smoke test
/// promises).
///
/// The demo login is **not** here — it is a test-only session-seeding route
/// ([`render_test_only_demo_login`]) emitted only into the smoke test, never
/// the production module, so a deployed app never exposes it (PR #2144
/// finding A).
///
/// Security shape (PR #2144 review): the feed/mark routes never take a
/// recipient id from the request — the recipient is derived server-side
/// from the signed session via `current_recipient_id`, so one user can
/// never read or mark another user's feed. Until real auth populates the
/// session key the feed simply 401s (a safe, dormant default).
const fn render_handlers() -> &'static str {
    r#"/// The session key `current_recipient_id` reads. Populate it from your
/// auth (see the TODO on `current_recipient_id`); until then the feed is
/// dormant and every request 401s.
const RECIPIENT_SESSION_KEY: &str = "notifications.recipient_id";

/// Resolve the signed-in recipient for this request.
///
/// The recipient is derived server-side from the signed session — never
/// from a path or body parameter — so a caller can only ever see or mark
/// their own feed. An unauthenticated request gets a 401, so these routes
/// stay dormant until real auth populates the session key.
///
/// TODO: replace this session lookup with your real auth (e.g. an
/// `Auth<CurrentUser>` extractor returning `user.id`), which is what should
/// populate `RECIPIENT_SESSION_KEY` on sign-in.
async fn current_recipient_id(session: &Session) -> AutumnResult<i64> {
    session
        .get(RECIPIENT_SESSION_KEY)
        .await
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AutumnError::unauthorized_msg("not signed in"))
}

/// `POST /notifications` body: who to notify, an application-defined kind
/// discriminator (e.g. `"comment.created"`), and a free-form JSON payload.
#[derive(Deserialize)]
pub struct NotifyBody {
    pub recipient_id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// `POST /notifications` — create a notification from a JSON body and return
/// the stored record.
///
/// The recipient comes from the body here because server-side notifies are
/// legitimately cross-recipient (one user's action notifies another) — but
/// that makes this demo route a spam vector as-is.
///
/// SECURITY / TODO: before shipping, protect or remove this route — e.g.
/// restrict it to an admin/service caller, or delete it and call
/// `notifications.notify(...)` from the domain action that triggers the
/// notification (a new comment, a mention, …). Otherwise anyone can notify
/// anyone.
#[post("/notifications")]
pub async fn notify(
    notifications: Notifications,
    Json(body): Json<NotifyBody>,
) -> AutumnResult<Json<Notification>> {
    let notification = notifications
        .notify(body.recipient_id, body.kind, body.payload)
        .await?;
    Ok(Json(notification))
}

/// `GET /notifications` — one page of the signed-in recipient's feed.
///
/// Supports `?page=`/`?size=` (the `PageRequest` extractor) plus
/// `?filter[unread]=true`, `?filter[kind]=…`, and `?sort=id|created_at`
/// (the `ListQuery` extractor); defaults to newest-first.
#[get("/notifications")]
pub async fn feed(
    session: Session,
    query: ListQuery,
    page: PageRequest,
    notifications: Notifications,
) -> AutumnResult<Json<Page<Notification>>> {
    let recipient_id = current_recipient_id(&session).await?;
    Ok(Json(notifications.list(recipient_id, &query, &page).await?))
}

/// `GET /notifications/unread_count` — the signed-in recipient's bell-badge
/// number.
#[get("/notifications/unread_count")]
pub async fn unread_count(
    session: Session,
    notifications: Notifications,
) -> AutumnResult<Json<u64>> {
    let recipient_id = current_recipient_id(&session).await?;
    Ok(Json(notifications.unread_count(recipient_id).await?))
}

/// `POST /notifications/{id}/read` — mark one of the signed-in recipient's
/// notifications read.
///
/// Uses the recipient-scoped `mark_read_for` with the session-derived
/// recipient, rather than the unscoped `mark_read`: a notification owned by
/// a different recipient is left untouched (a no-op, not an error), so even
/// a guessed `id` can never mark another user's notification read
/// (IDOR-safe). Idempotent on re-marking.
#[post("/notifications/{id}/read")]
pub async fn mark_read(
    Path(id): Path<i64>,
    session: Session,
    notifications: Notifications,
) -> AutumnResult<&'static str> {
    let recipient_id = current_recipient_id(&session).await?;
    notifications.mark_read_for(recipient_id, id).await?;
    Ok("ok")
}

/// `POST /notifications/read_all` — mark the signed-in recipient's whole
/// feed read; returns how many notifications transitioned (0 on a repeat
/// call).
#[post("/notifications/read_all")]
pub async fn mark_all_read(
    session: Session,
    notifications: Notifications,
) -> AutumnResult<Json<u64>> {
    let recipient_id = current_recipient_id(&session).await?;
    Ok(Json(notifications.mark_all_read(recipient_id).await?))
}
"#
}

/// The **test-only** session-seeding handler emitted into
/// `tests/notifications_feed.rs` — and deliberately nowhere else.
///
/// It is never part of [`render_handlers`] (so it never reaches the
/// production `src/notifications.rs`) and never listed in [`route_entries`]
/// (so it is never wired into the deployed router): a shipped
/// `POST /notifications/demo_login/{recipient_id}` would let any
/// unauthenticated visitor sign in as an arbitrary recipient and read or
/// mark that user's feed (PR #2144 finding A). It exists only so the smoke
/// test can establish a session the way a real login eventually will.
const fn render_test_only_demo_login() -> &'static str {
    r#"/// TEST-ONLY: seed the session with a recipient id so the session-scoped
/// routes above are reachable from this smoke test. This handler is NOT part
/// of the generated `src/notifications.rs` and is NOT registered in the real
/// router — shipping it would be an impersonation endpoint (anyone could POST
/// `/notifications/demo_login/<victim>`). A real app populates
/// `RECIPIENT_SESSION_KEY` from its own authentication instead.
#[post("/notifications/demo_login/{recipient_id}")]
pub async fn demo_login(Path(recipient_id): Path<i64>, session: Session) -> &'static str {
    session
        .insert(RECIPIENT_SESSION_KEY, recipient_id.to_string())
        .await;
    "ok"
}
"#
}

/// Render `src/notifications.rs`.
fn render_app_module() -> String {
    format!(
        r#"//! Generated by `autumn generate notifications`. Edit freely.
//!
//! A minimal in-app notification feed over the framework's
//! [`Notifications`] extractor (`autumn_web::notifications`). Storage
//! resolves automatically: the `notifications` table scaffolded by the
//! accompanying migration when a database is configured, an in-process
//! memory store otherwise — so these routes work before `autumn migrate`
//! has ever run.
//!
//! The feed/mark routes are session-scoped: the recipient is derived
//! server-side (`current_recipient_id`), never from the request, so one
//! user can never read or mark another user's feed. These routes stay
//! dormant (every request 401s) until your real auth populates the session
//! key — wire that in `current_recipient_id`; see the TODO there. There is
//! deliberately no demo-login route in this module: seeding the session is
//! done only by a test-only route in the generated smoke test, so a
//! deployed app never exposes a "become any user" endpoint.
//!
//! Optional realtime push: with the `ws` feature enabled, swap `notify` for
//! `notify_with_push` to also broadcast each stored notification on the
//! conventional `notifications:{{recipient_id}}` channel topic
//! (`Notifications::topic`).

use autumn_web::notifications::{{Notification, Notifications}};
use autumn_web::prelude::*;
use serde::Deserialize;

{handlers}"#,
        handlers = render_handlers(),
    )
}

/// Render `tests/notifications_feed.rs` — a real demo-login → notify →
/// list → mark-read → unread-count round trip over HTTP (plus the
/// cross-recipient isolation check), no database required.
#[allow(
    clippy::too_many_lines,
    reason = "one literal template for the generated smoke test; the length is the \
              emitted file's, and splitting the flow across helpers would only \
              obscure what the generated test contains"
)]
fn render_smoke_test() -> String {
    format!(
        r#"//! Smoke test generated by `autumn generate notifications`.
//!
//! Exercises the full demo-login → notify → list → mark-read →
//! unread-count flow over HTTP through the in-process `TestApp` — no
//! database needed: with no DB configured the `Notifications` extractor
//! falls back to its in-process memory store. `TestClient` carries a
//! cookie jar, so the session set by the demo login is replayed on every
//! later request, exactly like a browser. The handlers below are
//! re-declarations of `src/notifications.rs`'s handlers of the same names,
//! kept byte-for-byte in sync — `tests/` integration binaries cannot
//! import the app's own binary crate (there is no `src/lib.rs`).

use autumn_web::notifications::{{Notification, Notifications}};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use serde::Deserialize;

{handlers}
{demo_login}
#[tokio::test]
async fn notifications_feed_round_trips_over_http() {{
    let client = TestApp::new()
        .routes(routes![
            demo_login,
            notify,
            feed,
            unread_count,
            mark_read,
            mark_all_read
        ])
        .build();

    // Anonymous requests are rejected: the feed is session-scoped.
    client.get("/notifications").send().await.assert_status(401);

    // Demo sign-in as recipient 7 — the cookie jar keeps the session for
    // the rest of the flow.
    client
        .post("/notifications/demo_login/7")
        .send()
        .await
        .assert_ok();

    // Notify: POST a notification and get the stored record back.
    let created: Notification = client
        .post("/notifications")
        .json(&serde_json::json!({{
            "recipient_id": 7,
            "kind": "comment.created",
            "payload": {{"post": 42}}
        }}))
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(created.recipient_id, 7);
    assert_eq!(created.kind, "comment.created");

    // Feed (session-scoped): the new notification shows up.
    let feed_page: Page<Notification> = client
        .get("/notifications")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(feed_page.total_elements, 1);
    assert_eq!(feed_page.content[0].id, created.id);

    // The unread badge counts it...
    let unread: u64 = client
        .get("/notifications/unread_count")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(unread, 1);

    // ...until it is marked read (scoped to the session's recipient).
    client
        .post(&format!("/notifications/{{}}/read", created.id))
        .send()
        .await
        .assert_ok();
    let unread: u64 = client
        .get("/notifications/unread_count")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(unread, 0);

    // The unread-only feed is now empty.
    let unread_feed: Page<Notification> = client
        .get("/notifications?filter[unread]=true")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(unread_feed.total_elements, 0);

    // mark-all-read sweeps the rest of the feed in one call.
    client
        .post("/notifications")
        .json(&serde_json::json!({{
            "recipient_id": 7,
            "kind": "like",
            "payload": {{}}
        }}))
        .send()
        .await
        .assert_ok();
    let swept: u64 = client
        .post("/notifications/read_all")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(swept, 1);

    // Cross-recipient isolation: signing in as a different recipient shows
    // an empty feed — the recipient comes from the session, never a URL,
    // so recipient 7's notifications are unreachable from this session.
    client
        .post("/notifications/demo_login/8")
        .send()
        .await
        .assert_ok();
    let other_feed: Page<Notification> = client
        .get("/notifications")
        .send()
        .await
        .assert_ok()
        .json();
    assert_eq!(other_feed.total_elements, 0);
}}
"#,
        handlers = render_handlers(),
        demo_login = render_test_only_demo_login(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use std::path::{Path, PathBuf};
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

    /// Null the database-URL environment variables so backend detection reads
    /// the temp project's `autumn.toml` deterministically (a stray real
    /// `DATABASE_URL` in the dev/CI environment would otherwise win) — same
    /// guard the model generator's `SQLite` tests use.
    fn with_no_db_env<R>(f: impl FnOnce() -> R) -> R {
        temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", None::<&str>),
            ],
            f,
        )
    }

    /// The generated migration directory carries a fresh timestamp, so tests
    /// locate it by its stable `_create_notifications` suffix.
    fn migration_dir(root: &Path) -> PathBuf {
        let entries: Vec<PathBuf> = fs::read_dir(root.join("migrations"))
            .expect("migrations dir must exist")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("_create_notifications"))
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one notifications migration: {entries:?}"
        );
        entries.into_iter().next().unwrap()
    }

    // ── RED: file plan assertions ─────────────────────────────────────────

    #[test]
    fn plan_creates_migration_up_and_down() {
        let tmp = project_with_main(default_main());
        let plan = plan_notifications(tmp.path()).unwrap();
        for suffix in [
            "_create_notifications/up.sql",
            "_create_notifications/down.sql",
        ] {
            assert!(
                plan.actions.iter().any(|a| {
                    // Normalize so the assertion also holds under Windows
                    // path separators.
                    a.path()
                        .to_string_lossy()
                        .replace('\\', "/")
                        .ends_with(suffix)
                }),
                "plan must include migrations/<ts>{suffix}"
            );
        }
    }

    #[test]
    fn plan_creates_app_module_and_smoke_test() {
        let tmp = project_with_main(default_main());
        let plan = plan_notifications(tmp.path()).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/notifications.rs")),
            "plan must include src/notifications.rs"
        );
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("tests/notifications_feed.rs")),
            "plan must include tests/notifications_feed.rs"
        );
    }

    #[test]
    fn plan_modifies_main_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_notifications(tmp.path()).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/main.rs")),
            "plan must modify src/main.rs"
        );
    }

    #[test]
    fn plan_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_notifications(tmp.path()).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_errors_when_main_rs_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let err = plan_notifications(tmp.path()).unwrap_err();
        assert!(matches!(err, GenerateError::Io(_)));
    }

    // ── GREEN: migration DDL ──────────────────────────────────────────────

    #[test]
    fn postgres_migration_matches_framework_store_ddl() {
        with_no_db_env(|| {
            // No autumn.toml / DB env → Postgres, the detection default.
            let tmp = project_with_main(default_main());
            plan_notifications(tmp.path())
                .unwrap()
                .execute(Flags::default())
                .unwrap();

            let dir = migration_dir(tmp.path());
            let up = fs::read_to_string(dir.join("up.sql")).unwrap();
            // Column-for-column the diesel `table!` in
            // `autumn_web::notifications` (BigInt / Text / Text /
            // Nullable<Timestamptz> / Timestamptz) and the framework
            // integration test's CREATE_NOTIFICATIONS_SQL constant — plus
            // the recipient_id index the feed queries want.
            assert_eq!(
                up,
                "CREATE TABLE notifications (\n\
                 \x20   id BIGSERIAL PRIMARY KEY,\n\
                 \x20   recipient_id BIGINT NOT NULL,\n\
                 \x20   kind TEXT NOT NULL,\n\
                 \x20   payload TEXT NOT NULL,\n\
                 \x20   read_at TIMESTAMPTZ NULL,\n\
                 \x20   created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()\n\
                 );\n\
                 CREATE INDEX idx_notifications_recipient_id ON notifications (recipient_id);\n"
            );

            let down = fs::read_to_string(dir.join("down.sql")).unwrap();
            assert_eq!(down, "DROP TABLE notifications;\n");
        });
    }

    #[test]
    fn sqlite_project_emits_sqlite_ddl() {
        with_no_db_env(|| {
            let tmp = project_with_main(default_main());
            fs::write(
                tmp.path().join("autumn.toml"),
                "[database]\nprimary_url = \"sqlite://app.db\"\n",
            )
            .unwrap();
            plan_notifications(tmp.path())
                .unwrap()
                .execute(Flags::default())
                .unwrap();

            let up = fs::read_to_string(migration_dir(tmp.path()).join("up.sql")).unwrap();
            assert!(up.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"), "{up}");
            assert!(up.contains("recipient_id INTEGER NOT NULL"), "{up}");
            assert!(up.contains("kind TEXT NOT NULL"), "{up}");
            assert!(up.contains("payload TEXT NOT NULL"), "{up}");
            // Timestamps are RFC 3339 TEXT on SQLite (diesel's
            // `TimestamptzSqlite`), matching the framework store's SQLite
            // `table!` mapping.
            assert!(up.contains("read_at TEXT NULL"), "{up}");
            assert!(
                up.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
                "{up}"
            );
            assert!(
                up.contains("CREATE INDEX idx_notifications_recipient_id"),
                "{up}"
            );
            // No Postgres-only DDL may leak into a SQLite migration.
            for leak in ["BIGSERIAL", "TIMESTAMPTZ", "NOW()", "BIGINT"] {
                assert!(!up.contains(leak), "SQLite up.sql leaked `{leak}`: {up}");
            }
        });
    }

    // ── GREEN: idempotent migration directory (PR #2144 finding B) ─────────

    /// Count the `*_create_notifications` migration directories under `root`.
    fn count_notification_migration_dirs(root: &Path) -> usize {
        fs::read_dir(root.join("migrations")).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.path().is_dir()
                        && e.file_name()
                            .to_str()
                            .is_some_and(|n| n.ends_with("_create_notifications"))
                })
                .count()
        })
    }

    #[test]
    fn first_run_mints_exactly_one_migration_dir() {
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert_eq!(count_notification_migration_dirs(tmp.path()), 1);
    }

    #[test]
    fn rerun_reuses_existing_migration_dir_instead_of_minting_a_second() {
        // PR #2144 finding B: re-running (esp. with --force to refresh this
        // singleton scaffold) must not create a SECOND
        // `*_create_notifications` dir — two `CREATE TABLE notifications`
        // migrations would make `autumn migrate` fail on the duplicate table
        // and make destroy's suffix match ambiguous.
        //
        // Seed a migration dir with an explicit PAST timestamp so the reuse
        // is genuinely exercised: a fresh `timestamp_now()` would differ from
        // it, so a plan that reused nothing would mint a second directory.
        let tmp = project_with_main(default_main());
        let existing = tmp
            .path()
            .join("migrations")
            .join("20200101000000_create_notifications");
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join("up.sql"), "-- stale placeholder\n").unwrap();
        fs::write(existing.join("down.sql"), "-- stale placeholder\n").unwrap();

        // The plan must target the SAME existing directory (its past
        // timestamp), not a freshly minted current-timestamp one.
        let plan = plan_notifications(tmp.path()).unwrap();
        assert!(
            plan.actions.iter().any(|a| {
                a.path()
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with("20200101000000_create_notifications/up.sql")
            }),
            "re-run must write into the existing migration dir, not a new one: {:?}",
            plan.actions
                .iter()
                .map(|a| a.path().to_owned())
                .collect::<Vec<_>>()
        );

        // Applying it with --force overwrites the placeholder in place —
        // still exactly one `*_create_notifications` dir, and it is the
        // pre-existing one.
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(
            count_notification_migration_dirs(tmp.path()),
            1,
            "a re-run must not mint a second migration directory"
        );
        let up = fs::read_to_string(existing.join("up.sql")).unwrap();
        assert!(
            up.contains("CREATE TABLE notifications"),
            "the reused dir's up.sql must be refreshed with the real DDL: {up}"
        );
    }

    // ── GREEN: app module content ─────────────────────────────────────────

    #[test]
    fn execute_writes_app_module_with_session_scoped_handlers() {
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/notifications.rs")).unwrap();
        assert!(
            src.contains("use autumn_web::notifications::{Notification, Notifications};"),
            "{src}"
        );
        // The five production routes: the demo notify plus the four
        // session-scoped feed routes (no recipient in the URL). The demo
        // login is TEST-ONLY and must never appear in the production
        // module or its router (PR #2144 finding A).
        assert!(
            !src.contains("demo_login"),
            "the demo login must be test-only — it must not exist in the \
             production module: {src}"
        );
        assert!(src.contains(r#"#[post("/notifications")]"#), "{src}");
        assert!(src.contains(r#"#[get("/notifications")]"#), "{src}");
        assert!(
            src.contains(r#"#[get("/notifications/unread_count")]"#),
            "{src}"
        );
        assert!(
            src.contains(r#"#[post("/notifications/{id}/read")]"#),
            "{src}"
        );
        assert!(
            src.contains(r#"#[post("/notifications/read_all")]"#),
            "{src}"
        );
        // Every read/mark route derives the recipient server-side from the
        // session through the shared helper — never from the request.
        assert!(
            src.contains("async fn current_recipient_id(session: &Session)"),
            "{src}"
        );
        assert_eq!(
            src.matches("current_recipient_id(&session).await?").count(),
            4,
            "feed, unread_count, mark_read, and mark_all_read must all resolve \
             the recipient from the session: {src}"
        );
        // An unauthenticated request is rejected, not defaulted — the feed is
        // dormant until real auth populates the session key.
        assert!(src.contains("unauthorized"), "{src}");
        // The mark-read route must additionally use the recipient-scoped
        // (IDOR-safe) store variant, and say why.
        assert!(src.contains(".mark_read_for(recipient_id, id)"), "{src}");
        assert!(src.contains("IDOR"), "{src}");
        // The demo notify must carry prominent SECURITY/TODO markers steering
        // users to real auth / protecting the route.
        assert!(src.contains("SECURITY"), "{src}");
        assert!(src.contains("TODO"), "{src}");
        // The feed goes through the shipped pagination extractors.
        assert!(src.contains("query: ListQuery"), "{src}");
        assert!(src.contains("page: PageRequest"), "{src}");
        assert!(src.contains("Json<Page<Notification>>"), "{src}");
    }

    #[test]
    fn demo_login_is_test_only_never_in_production_router() {
        // PR #2144 finding A (security regression): a production
        // `demo_login` route lets an unauthenticated visitor POST
        // `/notifications/demo_login/<victim>` and then read/mark the
        // victim's feed. The demo login must be test-only — absent from
        // `route_entries()`, from the emitted production module, and from
        // `src/main.rs`'s `routes![...]` — while the smoke test seeds the
        // session with its own local handler.
        assert!(
            !route_entries().iter().any(|e| e.contains("demo_login")),
            "route_entries must not register demo_login: {:?}",
            route_entries()
        );

        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/notifications.rs")).unwrap();
        assert!(
            !src.contains("fn demo_login"),
            "production module must not define a demo_login handler: {src}"
        );
        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            !main.contains("demo_login"),
            "main.rs must not register a demo_login route: {main}"
        );

        // The smoke test still seeds the session (its own test-only route),
        // so the flow stays runnable and honest.
        let test_src = fs::read_to_string(tmp.path().join("tests/notifications_feed.rs")).unwrap();
        assert!(
            test_src.contains("fn demo_login"),
            "smoke test must declare its own test-only session-seeding \
             handler: {test_src}"
        );
        assert!(
            test_src.contains("RECIPIENT_SESSION_KEY"),
            "smoke test's seeding route must write the session key: {test_src}"
        );
    }

    #[test]
    fn production_routes_never_take_the_recipient_from_the_url() {
        // Regression test for the PR #2144 Codex review finding: a
        // `{recipient_id}` path segment on any real route lets any caller
        // read (or mark read) another user's feed — `mark_read_for` scopes
        // to the *supplied* recipient, not the caller. No production route
        // may name a recipient in its URL (the demo login is test-only now).
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/notifications.rs")).unwrap();
        let route_attrs: Vec<&str> = src
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("#[get(") || l.starts_with("#[post("))
            .collect();
        assert_eq!(
            route_attrs.len(),
            5,
            "five routes expected: {route_attrs:?}"
        );
        assert!(
            !route_attrs.iter().any(|l| l.contains("{recipient_id}")),
            "no production route may take a recipient id from the URL: \
             {route_attrs:?}"
        );
        // The demo notify's body-supplied recipient stays (server-side
        // notifies are legitimately cross-recipient), but it must warn that
        // the route needs protection before production.
        assert!(src.contains("pub recipient_id: i64"), "{src}");
    }

    // ── GREEN: main.rs wiring ─────────────────────────────────────────────

    #[test]
    fn execute_updates_main_rs_with_mod_and_route_entries() {
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("mod notifications;"), "{main}");
        for entry in [
            "notifications::notify",
            "notifications::feed",
            "notifications::unread_count",
            "notifications::mark_read",
            "notifications::mark_all_read",
        ] {
            assert!(
                main.contains(entry),
                "main.rs must register {entry}: {main}"
            );
        }
        // The test-only demo login must NOT be wired into the router.
        assert!(
            !main.contains("demo_login"),
            "main.rs must not register the test-only demo login: {main}"
        );
    }

    // ── GREEN: smoke test content ─────────────────────────────────────────

    #[test]
    fn execute_writes_smoke_test_with_testapp_flow() {
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let test_src = fs::read_to_string(tmp.path().join("tests/notifications_feed.rs")).unwrap();
        assert!(test_src.contains("TestApp"), "{test_src}");
        assert!(test_src.contains("#[tokio::test]"), "{test_src}");
        // The full session-scoped HTTP flow: anonymous 401 → demo login →
        // notify → list → mark_read → unread_count → unread-only feed empty.
        // TestClient's cookie jar carries the session across requests.
        assert!(test_src.contains("assert_status(401)"), "{test_src}");
        assert!(
            test_src.contains(r#".post("/notifications/demo_login/7")"#),
            "{test_src}"
        );
        assert!(
            test_src.contains(r#".post("/notifications")"#),
            "{test_src}"
        );
        assert!(test_src.contains(r#".get("/notifications")"#), "{test_src}");
        assert!(
            test_src.contains("/notifications/unread_count"),
            "{test_src}"
        );
        assert!(test_src.contains("/read"), "{test_src}");
        assert!(test_src.contains("filter[unread]=true"), "{test_src}");
        // Cross-recipient protection: a different signed-in recipient sees
        // an empty feed — the recipient comes from the session, not a URL.
        assert!(
            test_src.contains(r#".post("/notifications/demo_login/8")"#),
            "{test_src}"
        );
        // No recipient id in any feed/mark URL.
        assert!(
            !test_src.contains("/notifications/7/"),
            "feed/mark URLs must not carry a recipient id: {test_src}"
        );
        assert!(
            !test_src.contains("todo!"),
            "must not be a stub: {test_src}"
        );
        assert!(
            !test_src.contains("#[ignore"),
            "smoke test must actually run (no DB/Docker needed): {test_src}"
        );
    }

    #[test]
    fn smoke_test_handlers_match_production_handlers_byte_for_byte() {
        // The smoke test's doc comment promises its handlers are
        // "re-declarations of src/notifications.rs's handlers of the same
        // names, kept byte-for-byte in sync" (`tests/` integration binaries
        // cannot import the app's binary crate), so they must actually be —
        // the same contract the channel generator's smoke test enforces.
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let production_src = fs::read_to_string(tmp.path().join("src/notifications.rs")).unwrap();
        let test_src = fs::read_to_string(tmp.path().join("tests/notifications_feed.rs")).unwrap();

        // The shared JSON body type must be re-declared too.
        assert!(
            production_src.contains("pub struct NotifyBody"),
            "{production_src}"
        );
        assert!(test_src.contains("pub struct NotifyBody"), "{test_src}");

        // Extract each handler (signature + brace-balanced body, so trailing
        // content like the smoke test's own #[tokio::test] fn is never swept
        // in) and confirm production and test agree byte-for-byte.
        let extract_handler = |src: &str, name: &str| -> String {
            let anchor = format!("async fn {name}(");
            let start = src
                .find(&anchor)
                .unwrap_or_else(|| panic!("`{anchor}` not found in:\n{src}"));
            let brace_start = src[start..].find('{').unwrap() + start;
            let bytes = src.as_bytes();
            let mut depth = 0usize;
            let mut end = brace_start;
            for (offset, &b) in bytes[brace_start..].iter().enumerate() {
                match b {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = brace_start + offset;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            src[start..=end].to_owned()
        };
        // `demo_login` is test-only (it lives only in the smoke test), so it
        // is excluded from the parity contract — only the shared PRODUCTION
        // handlers must be byte-for-byte identical between the two files.
        for name in [
            "current_recipient_id",
            "notify",
            "feed",
            "unread_count",
            "mark_read",
            "mark_all_read",
        ] {
            assert_eq!(
                extract_handler(&production_src, name),
                extract_handler(&test_src, name),
                "the smoke test's re-declared `{name}` handler must be \
                 byte-identical to the production handler"
            );
        }
        // The shared session-key const must also be re-declared verbatim.
        assert!(
            production_src
                .contains(r#"const RECIPIENT_SESSION_KEY: &str = "notifications.recipient_id";"#),
            "{production_src}"
        );
        assert!(
            test_src
                .contains(r#"const RECIPIENT_SESSION_KEY: &str = "notifications.recipient_id";"#),
            "{test_src}"
        );
    }

    // ── GREEN: Cargo.toml ─────────────────────────────────────────────────

    #[test]
    fn execute_adds_serde_serde_json_and_tokio_dev_dependency() {
        let tmp = project_with_main(default_main());
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        // The handlers derive `Deserialize` and take a `serde_json::Value`
        // payload; the smoke test additionally needs `#[tokio::test]`.
        assert!(cargo.contains("serde"), "{cargo}");
        assert!(cargo.contains("serde_json"), "{cargo}");
        assert!(cargo.contains("[dev-dependencies]"), "{cargo}");
        assert!(cargo.contains("tokio"), "{cargo}");
    }

    // ── Flag behaviour ────────────────────────────────────────────────────

    #[test]
    fn dry_run_writes_no_new_files() {
        let tmp = project_with_main(default_main());
        let original_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        let original_cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();

        assert!(!tmp.path().join("src/notifications.rs").exists());
        assert!(!tmp.path().join("tests/notifications_feed.rs").exists());
        assert!(!tmp.path().join("migrations").exists());
        let main_after = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(original_main, main_after, "dry run must not modify main.rs");
        let cargo_after = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            original_cargo, cargo_after,
            "dry run must not modify Cargo.toml"
        );
    }

    #[test]
    fn collision_without_force_returns_error() {
        let tmp = project_with_main(default_main());
        fs::write(tmp.path().join("src/notifications.rs"), "// existing").unwrap();
        let err = plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags::default())
            .unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)));
    }

    #[test]
    fn force_overwrites_existing_module() {
        let tmp = project_with_main(default_main());
        fs::write(tmp.path().join("src/notifications.rs"), "// old").unwrap();
        plan_notifications(tmp.path())
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/notifications.rs")).unwrap();
        assert!(src.contains("Notifications"), "{src}");
    }

    // ── Destroy round-trip ────────────────────────────────────────────────

    #[test]
    fn generate_then_destroy_round_trips_to_original_project_state() {
        with_no_db_env(|| {
            let tmp = project_with_main(default_main());
            // Mirrors a real `autumn new` project's Cargo.toml: a `tokio`
            // dev-dependency with `rt`/`macros` already present, so
            // `ensure_dev_dependency_tokio_test_features` (which has no
            // destroy wiring yet — a documented gap, same as the channel
            // generator) is a no-op here, matching reality.
            let cargo_path = tmp.path().join("Cargo.toml");
            fs::write(
                &cargo_path,
                "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n\n\
                 [dev-dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"macros\"] }\n",
            )
            .unwrap();
            let main_path = tmp.path().join("src/main.rs");
            let original_cargo = fs::read_to_string(&cargo_path).unwrap();
            let original_main = fs::read_to_string(&main_path).unwrap();

            let plan = plan_notifications(tmp.path()).unwrap();
            plan.execute(Flags::default()).unwrap();
            assert!(tmp.path().join("src/notifications.rs").exists());
            assert!(
                fs::read_to_string(&main_path)
                    .unwrap()
                    .contains("notifications::notify")
            );

            let destroy_plan = plan_notifications(tmp.path()).unwrap();
            destroy_plan.revert(Flags::default()).unwrap();

            assert!(!tmp.path().join("src/notifications.rs").exists());
            assert!(!tmp.path().join("tests/notifications_feed.rs").exists());
            assert!(!tmp.path().join("migrations").exists());
            assert_eq!(fs::read_to_string(&main_path).unwrap(), original_main);
            assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
        });
    }
}
