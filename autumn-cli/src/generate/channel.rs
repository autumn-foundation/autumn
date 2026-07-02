//! `autumn generate channel` — scaffold a real-time broadcast channel.
//!
//! For a name like `Chat`, the generator produces:
//! - `src/channels/chat.rs` — channel handler(s) that subscribe/publish
//!   through the existing `Channels` API. SSE transport (the default) emits
//!   a live view wired to htmx's `sse-connect`/`sse-swap` (zero client JS
//!   authored by the user); `--ws` emits a raw `#[ws]` socket handler instead.
//! - `src/channels/mod.rs` — created or updated with `pub mod chat;`.
//! - `src/main.rs` — `mod channels;` declaration and the generated route(s)
//!   registered in `routes![...]`.
//! - `tests/chat_channel.rs` — a smoke test that publishes a message through
//!   the in-process `TestApp`/`TestClient` and asserts a subscriber receives
//!   it (not a stub).
//! - `Cargo.toml` — the `"ws"` feature added to `autumn-web` (channels, SSE,
//!   and `#[ws]` are all gated behind it), plus `serde` (and `maud` for the
//!   SSE transport) and the `tokio` dev-dependency features the smoke test
//!   needs for `#[tokio::test]`.

use std::path::Path;

use super::emit::Plan;
use super::model::{ensure_cargo_dependencies, validate_resource_name};
use super::naming::{pascal, snake};
use super::schema_edit::{
    add_mod_declaration, ensure_autumn_web_feature, ensure_dev_dependency_tokio_test_features,
    remove_routes_entries_with_prefix, update_main_rs,
};
use super::{Flags, GenerateError, ensure_project_root, read_or_empty};

/// Which transport a generated channel uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// SSE-over-htmx: `sse-connect`/`sse-swap`, zero client JS authored by the user.
    Sse,
    /// A raw `#[ws]` WebSocket handler.
    Ws,
}

/// Cargo dependencies always required by generated channel files (the
/// publish handler and smoke test both derive `Deserialize`/use `Form`).
const CHANNEL_DEPS_BASE: &[(&str, &str)] =
    &[("serde", "{ version = \"1\", features = [\"derive\"] }")];

/// Extra dependency needed by the SSE transport's Maud-rendered view.
const CHANNEL_DEPS_MAUD: (&str, &str) = ("maud", "{ version = \"0.27\", features = [\"axum\"] }");

/// Compute the file actions for `autumn generate channel`.
///
/// # Errors
/// Project layout and name validation errors surface here.
pub fn plan_channel(
    project_root: &Path,
    name: &str,
    transport: Transport,
) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;

    let snake_name = snake(name);

    let mut plan = Plan::new(project_root);

    // ── src/channels/<snake>.rs ─────────────────────────────────────────────
    plan.create(
        project_root
            .join("src")
            .join("channels")
            .join(format!("{snake_name}.rs")),
        render_channel_file(&snake_name, transport),
    );

    // ── src/channels/mod.rs (create or update) ──────────────────────────────
    let mod_path = project_root.join("src").join("channels").join("mod.rs");
    plan.modify(
        mod_path.clone(),
        add_mod_declaration(&read_or_empty(&mod_path), &snake_name),
    );

    // ── src/main.rs: add mod channels; and route entries ────────────────────
    let main_path = project_root.join("src").join("main.rs");
    let main_existing = std::fs::read_to_string(&main_path).map_err(|e| {
        GenerateError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", main_path.display()),
        ))
    })?;
    // Drop any route entries a prior generation of this channel left behind
    // under a different transport (e.g. `--ws --force` after a plain SSE
    // run) before splicing in the current transport's entries — otherwise
    // main.rs would keep referencing functions the just-overwritten
    // src/channels/<snake>.rs no longer defines.
    let main_existing =
        remove_routes_entries_with_prefix(&main_existing, &format!("channels::{snake_name}::"));
    let route_entries = route_entries(&snake_name, transport);
    let updated_main = update_main_rs(&main_existing, &["channels"], &route_entries);
    plan.modify(main_path, updated_main);

    // ── tests/<snake>_channel.rs ──────────────────────────────────────────
    plan.create(
        project_root
            .join("tests")
            .join(format!("{snake_name}_channel.rs")),
        render_smoke_test(&snake_name, transport),
    );

    // ── Cargo.toml: autumn-web feature(s) + deps + tokio dev-dep test features ──
    let cargo_path = project_root.join("Cargo.toml");
    let cargo_existing = read_or_empty(&cargo_path);
    let mut updated_cargo = cargo_existing.clone();
    // `channels`/`sse::stream`/`#[ws]` are all gated behind "ws". The SSE
    // transport's generated view additionally uses `Markup`/`html!` and the
    // `HTMX_JS_PATH`/`HTMX_SSE_JS_PATH` constants, which are gated behind
    // "maud"/"htmx" — both are in autumn-web's default feature set, but a
    // project that opts out with `default-features = false` needs them
    // enabled explicitly too (mirrors `generate scaffold --live`).
    let autumn_web_features: &[&str] = match transport {
        Transport::Sse => &["ws", "htmx", "maud"],
        Transport::Ws => &["ws"],
    };
    for feature in autumn_web_features {
        updated_cargo = ensure_autumn_web_feature(&updated_cargo, feature);
    }
    let mut deps = CHANNEL_DEPS_BASE.to_vec();
    if matches!(transport, Transport::Sse) {
        deps.push(CHANNEL_DEPS_MAUD);
    }
    updated_cargo = ensure_cargo_dependencies(&updated_cargo, &deps);
    updated_cargo = ensure_dev_dependency_tokio_test_features(&updated_cargo);
    if updated_cargo != cargo_existing {
        plan.modify(cargo_path, updated_cargo);
    }

    Ok(plan)
}

/// Fully-qualified route entries for `routes![...]` wiring in `main.rs`.
fn route_entries(snake_name: &str, transport: Transport) -> Vec<String> {
    match transport {
        Transport::Sse => vec![
            format!("channels::{snake_name}::{snake_name}_page"),
            format!("channels::{snake_name}::{snake_name}_events"),
            format!("channels::{snake_name}::{snake_name}_publish"),
        ],
        Transport::Ws => vec![
            format!("channels::{snake_name}::{snake_name}_ws"),
            format!("channels::{snake_name}::{snake_name}_publish"),
        ],
    }
}

/// Render `src/channels/<snake>.rs`.
fn render_channel_file(snake_name: &str, transport: Transport) -> String {
    match transport {
        Transport::Sse => render_sse_channel_file(snake_name),
        Transport::Ws => render_ws_channel_file(snake_name),
    }
}

fn render_sse_channel_file(snake_name: &str) -> String {
    format!(
        r#"//! Generated by `autumn generate channel`. Edit freely.
//!
//! Round trip: `POST /{snake_name}/messages` publishes on the `"{snake_name}"`
//! topic via the `Channels` API; `GET /{snake_name}/events` is an SSE
//! subscription on that topic; the `GET /{snake_name}` view wires htmx's
//! `sse-connect`/`sse-swap` to append each published fragment — zero client
//! JS authored by the user.

use autumn_web::prelude::*;
use serde::Deserialize;

/// Topic this channel publishes and subscribes on.
pub const TOPIC: &str = "{snake_name}";

/// Server-rendered fragment for one message. Shared by the publish handler
/// (rendered once, broadcast verbatim) so every SSE subscriber sees exactly
/// what the publisher saw.
fn message_fragment(text: &str) -> Markup {{
    html! {{ li ."{snake_name}-message" {{ (text) }} }}
}}

#[derive(Deserialize)]
pub struct {struct_name}Form {{
    pub message: String,
}}

/// `GET /{snake_name}` — live view. Zero client JS: htmx and its SSE
/// extension (loaded below) drive the `sse-connect`/`sse-swap` wiring
/// declaratively.
#[get("/{snake_name}")]
pub async fn {snake_name}_page() -> Markup {{
    html! {{
        html {{
            head {{
                title {{ "{struct_name}" }}
                script src=(HTMX_JS_PATH) {{}}
                script src=(HTMX_SSE_JS_PATH) {{}}
            }}
            body {{
                h1 {{ "{struct_name}" }}
                ul id="{snake_name}-messages" hx-ext="sse" sse-connect="/{snake_name}/events" sse-swap="message" hx-swap="beforeend" {{}}
                form hx-post="/{snake_name}/messages" hx-swap="none" {{
                    input type="text" name="message" required;
                    button type="submit" {{ "Send" }}
                }}
            }}
        }}
    }}
}}

/// `GET /{snake_name}/events` — SSE subscription on [`TOPIC`].
///
/// Optional: to show live viewer counts on this channel, add a `Presence`
/// param here (requires the `presence` feature on autumn-web) and publish
/// join/leave events on a `presence:{{TOPIC}}` topic — see
/// `autumn_web::presence::Presence::track`.
#[get("/{snake_name}/events")]
pub async fn {snake_name}_events(State(state): State<AppState>) -> impl IntoResponse {{
    autumn_web::sse::stream(&state, TOPIC)
}}

/// `POST /{snake_name}/messages` — publish a message to every subscriber.
///
/// Returns a minimal acknowledgement rather than the rendered fragment: the
/// live update comes from the SSE broadcast above, and the view's form uses
/// `hx-swap="none"` so the response body is never used by the browser.
#[post("/{snake_name}/messages")]
pub async fn {snake_name}_publish(
    State(state): State<AppState>,
    Form(form): Form<{struct_name}Form>,
) -> AutumnResult<&'static str> {{
    let fragment = message_fragment(&form.message).into_string();
    state
        .broadcast()
        .publish(TOPIC, fragment)
        .map_err(AutumnError::internal_server_error)?;
    Ok("published")
}}

#[cfg(test)]
mod {snake_name}_channel_tests {{
    use super::*;

    #[test]
    fn message_fragment_renders_text() {{
        let html = message_fragment("hello").into_string();
        assert!(html.contains("hello"));
        assert!(html.contains("{snake_name}-message"));
    }}
}}
"#,
        struct_name = pascal(snake_name),
    )
}

fn render_ws_channel_file(snake_name: &str) -> String {
    format!(
        r#"//! Generated by `autumn generate channel`. Edit freely.
//!
//! `WS /{snake_name}/ws` bridges a WebSocket connection onto the
//! `"{snake_name}"` topic via the `Channels` API: inbound text frames are
//! published, and every published message (including ones sent through the
//! `POST /{snake_name}/messages` route below) is relayed back out to every
//! connected socket.

use autumn_web::prelude::*;
use autumn_web::reexports::{{tokio, tracing}};
use autumn_web::ws::{{CancellationToken, Message, WebSocket, WithShutdown, WsHandler}};
use serde::Deserialize;

/// Topic this channel publishes and subscribes on.
pub const TOPIC: &str = "{snake_name}";

#[derive(Deserialize)]
pub struct {struct_name}Form {{
    pub message: String,
}}

/// `WS /{snake_name}/ws` — bidirectional relay over [`TOPIC`].
#[ws("/{snake_name}/ws")]
pub async fn {snake_name}_ws(state: AppState) -> impl WsHandler {{
    let channels = state.channels().clone();
    let mut rx = channels.subscribe(TOPIC);

    WithShutdown(
        move |mut socket: WebSocket, shutdown: CancellationToken| async move {{
            loop {{
                tokio::select! {{
                    incoming = socket.recv() => match incoming {{
                        Some(Ok(Message::Text(text))) => {{
                            if let Err(err) = channels.publish(TOPIC, text.as_str()) {{
                                tracing::warn!("{{TOPIC}} channel publish failed: {{err}}");
                            }}
                        }}
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {{}}
                        _ => break,
                    }},
                    published = rx.recv() => match published {{
                        Ok(msg) => {{
                            if socket.send(Message::Text(msg.into_string().into())).await.is_err() {{
                                break;
                            }}
                        }}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {{}}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }},
                    () = shutdown.cancelled() => {{
                        socket.send(Message::Close(None)).await.ok();
                        break;
                    }}
                }}
            }}
        }},
    )
}}

/// `POST /{snake_name}/messages` — publish a message to every connected client.
#[post("/{snake_name}/messages")]
pub async fn {snake_name}_publish(
    State(state): State<AppState>,
    Form(form): Form<{struct_name}Form>,
) -> AutumnResult<&'static str> {{
    state
        .channels()
        .publish(TOPIC, form.message.as_str())
        .map_err(AutumnError::internal_server_error)?;
    Ok("published")
}}
"#,
        struct_name = pascal(snake_name),
    )
}

/// Render `tests/<snake>_channel.rs` — a real pub/sub round-trip smoke test.
///
/// `tests/*.rs` integration binaries cannot import the app's own binary
/// crate (there is no `src/lib.rs`), so the generated test re-declares the
/// publish handler (and, for SSE, its `message_fragment` helper) under test
/// locally, kept byte-for-byte in sync with the real handler in
/// `src/channels/<snake>.rs` — the same contract `generate scaffold`'s
/// smoke tests use.
fn render_smoke_test(snake_name: &str, transport: Transport) -> String {
    let struct_name = pascal(snake_name);
    let extra_helper = match transport {
        Transport::Sse => format!(
            "\nfn message_fragment(text: &str) -> Markup {{\n    html! {{ li .\"{snake_name}-message\" {{ (text) }} }}\n}}"
        ),
        Transport::Ws => String::new(),
    };
    let publish_body = match transport {
        Transport::Sse => {
            r#"    let fragment = message_fragment(&form.message).into_string();
    state
        .broadcast()
        .publish(TOPIC, fragment)
        .map_err(AutumnError::internal_server_error)?;
    Ok("published")"#
        }
        Transport::Ws => {
            r#"    state
        .channels()
        .publish(TOPIC, form.message.as_str())
        .map_err(AutumnError::internal_server_error)?;
    Ok("published")"#
        }
    };
    format!(
        r#"//! Smoke test generated by `autumn generate channel`.
//!
//! Publishes through the real `Channels` registry inside an in-process
//! `TestApp` and asserts a subscriber receives the message. `{snake_name}_publish`
//! below is a re-declaration of `src/channels/{snake_name}.rs`'s handler of the
//! same name — `tests/` integration binaries cannot import the app's own
//! binary crate (there is no `src/lib.rs`).

use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use serde::Deserialize;
use std::time::Duration;

const TOPIC: &str = "{snake_name}";
{extra_helper}
#[derive(Deserialize)]
struct {struct_name}Form {{
    message: String,
}}

#[post("/{snake_name}/messages")]
async fn {snake_name}_publish(
    State(state): State<AppState>,
    Form(form): Form<{struct_name}Form>,
) -> AutumnResult<&'static str> {{
{publish_body}
}}

#[tokio::test]
async fn {snake_name}_channel_delivers_published_message_to_subscriber() {{
    let client = TestApp::new().routes(routes![{snake_name}_publish]).build();

    // Subscribe BEFORE publishing — broadcast channels drop messages sent
    // when there is no receiver.
    let mut rx = client.state().channels().subscribe(TOPIC);

    client
        .post("/{snake_name}/messages")
        .form("message=hello+from+the+smoke+test")
        .send()
        .await
        .assert_ok();

    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for channel message")
        .expect("channel closed before delivering");
    assert!(msg.into_string().contains("hello from the smoke test"));
}}
"#,
    )
}

/// CLI entry point.
pub fn run(name: &str, transport: Transport, flags: Flags) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    match plan_channel(&cwd, name, transport).and_then(|p| p.execute(flags)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // ── RED: file plan assertions ─────────────────────────────────────────

    #[test]
    fn plan_creates_channel_file() {
        let tmp = project_with_main(default_main());
        let plan = plan_channel(tmp.path(), "Chat", Transport::Sse).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/channels/chat.rs")),
            "plan must include src/channels/chat.rs"
        );
    }

    #[test]
    fn plan_creates_channel_mod_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_channel(tmp.path(), "Chat", Transport::Sse).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/channels/mod.rs")),
            "plan must include src/channels/mod.rs"
        );
    }

    #[test]
    fn plan_modifies_main_rs() {
        let tmp = project_with_main(default_main());
        let plan = plan_channel(tmp.path(), "Chat", Transport::Sse).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("src/main.rs")),
            "plan must modify src/main.rs"
        );
    }

    #[test]
    fn plan_creates_smoke_test() {
        let tmp = project_with_main(default_main());
        let plan = plan_channel(tmp.path(), "Chat", Transport::Sse).unwrap();
        assert!(
            plan.actions
                .iter()
                .any(|a| a.path().ends_with("tests/chat_channel.rs")),
            "plan must include tests/chat_channel.rs"
        );
    }

    #[test]
    fn plan_errors_when_not_in_project() {
        let tmp = TempDir::new().unwrap();
        let err = plan_channel(tmp.path(), "Chat", Transport::Sse).unwrap_err();
        assert!(matches!(err, GenerateError::NotInProject));
    }

    #[test]
    fn plan_errors_when_main_rs_missing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let err = plan_channel(tmp.path(), "Chat", Transport::Sse).unwrap_err();
        assert!(matches!(err, GenerateError::Io(_)));
    }

    #[test]
    fn plan_rejects_invalid_name() {
        let tmp = project_with_main(default_main());
        let err = plan_channel(tmp.path(), "9bad", Transport::Sse).unwrap_err();
        assert!(matches!(err, GenerateError::InvalidName(_, _)));
    }

    // ── GREEN: SSE transport content assertions ───────────────────────────

    #[test]
    fn sse_execute_writes_page_with_htmx_wiring() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(src.contains(r#"sse-connect="/chat/events""#), "{src}");
        assert!(src.contains(r#"sse-swap="message""#), "{src}");
        assert!(src.contains(r#"hx-ext="sse""#), "{src}");
        assert!(src.contains("HTMX_JS_PATH"), "{src}");
        assert!(src.contains("HTMX_SSE_JS_PATH"), "{src}");
        assert!(
            !src.contains("#[ws("),
            "SSE mode must not emit a #[ws] handler: {src}"
        );
    }

    #[test]
    fn sse_execute_writes_events_route_using_sse_stream() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(src.contains(r#"#[get("/chat/events")]"#), "{src}");
        assert!(src.contains("autumn_web::sse::stream"), "{src}");
    }

    #[test]
    fn sse_execute_writes_publish_route() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(src.contains(r#"#[post("/chat/messages")]"#), "{src}");
        assert!(src.contains("broadcast()"), "{src}");
    }

    #[test]
    fn sse_publish_handler_does_not_clone_fragment_for_a_discarded_response() {
        // The view's form uses hx-swap="none", so the POST response body is
        // never used by the browser — publishing must not clone the
        // rendered fragment just to also return it.
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(!src.contains(".clone()"), "{src}");
        assert!(src.contains(r"AutumnResult<&'static str>"), "{src}");
    }

    // ── GREEN: WS transport content assertions ────────────────────────────

    #[test]
    fn ws_execute_writes_handler_with_ws_macro() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(src.contains(r#"#[ws("/chat/ws")]"#), "{src}");
        assert!(src.contains("WithShutdown"), "{src}");
        assert!(src.contains("WsHandler"), "{src}");
        assert!(
            !src.contains("sse-connect"),
            "WS mode must not emit SSE htmx wiring: {src}"
        );
    }

    #[test]
    fn ws_execute_writes_publish_route() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(src.contains(r#"#[post("/chat/messages")]"#), "{src}");
    }

    #[test]
    fn ws_handler_explicitly_breaks_on_close_message() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(
            src.contains("Message::Close(_))) => break"),
            "a client close handshake must terminate the relay loop instead \
             of falling into the Some(Ok(_)) catch-all: {src}"
        );
    }

    #[test]
    fn ws_handler_logs_instead_of_silently_dropping_publish_failures() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(
            !src.contains("channels.publish(TOPIC, text.as_str()).ok();"),
            "a client-sent message's publish failure must not be silently \
             swallowed: {src}"
        );
        assert!(src.contains("tracing::warn!"), "{src}");
    }

    // ── GREEN: main.rs / mod.rs wiring ─────────────────────────────────────

    #[test]
    fn execute_updates_main_rs_with_mod_declaration() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            main.contains("mod channels;"),
            "main.rs must declare mod channels"
        );
    }

    #[test]
    fn execute_updates_main_rs_with_sse_route_entries() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("channels::chat::chat_page"), "{main}");
        assert!(main.contains("channels::chat::chat_events"), "{main}");
        assert!(main.contains("channels::chat::chat_publish"), "{main}");
    }

    #[test]
    fn execute_updates_main_rs_with_ws_route_entries() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(main.contains("channels::chat::chat_ws"), "{main}");
        assert!(main.contains("channels::chat::chat_publish"), "{main}");
    }

    #[test]
    fn execute_writes_mod_rs_with_pub_mod() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/channels/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod chat;"));
    }

    #[test]
    fn second_channel_augments_mod_rs_without_duplication() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_channel(tmp.path(), "Notifications", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let mod_rs = fs::read_to_string(tmp.path().join("src/channels/mod.rs")).unwrap();
        assert!(mod_rs.contains("pub mod chat;"));
        assert!(mod_rs.contains("pub mod notifications;"));
    }

    #[test]
    fn second_channel_does_not_duplicate_main_rs_mod_declaration() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_channel(tmp.path(), "Notifications", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(
            main.matches("mod channels;").count(),
            1,
            "mod channels; must appear exactly once in main.rs"
        );
    }

    #[test]
    fn force_regenerating_with_different_transport_does_not_strand_stale_routes() {
        // Regression test: switching transports via `--force` must not leave
        // main.rs referencing functions the regenerated channel file no
        // longer defines (previously left `chat_page`/`chat_events` behind
        // after switching to `--ws`, which failed to compile).
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            !main.contains("chat_page"),
            "stale SSE page route must be removed after switching to --ws: {main}"
        );
        assert!(
            !main.contains("chat_events"),
            "stale SSE events route must be removed after switching to --ws: {main}"
        );
        assert!(main.contains("channels::chat::chat_ws"), "{main}");
        assert!(main.contains("channels::chat::chat_publish"), "{main}");
    }

    #[test]
    fn force_regenerating_back_to_sse_does_not_strand_stale_ws_route() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert!(
            !main.contains("chat_ws"),
            "stale WS route must be removed after switching to SSE: {main}"
        );
        assert!(main.contains("channels::chat::chat_page"), "{main}");
        assert!(main.contains("channels::chat::chat_events"), "{main}");
    }

    // ── GREEN: smoke test content ───────────────────────────────────────────

    #[test]
    fn execute_writes_smoke_test_with_testapp_and_subscriber_assertion() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let test_src = fs::read_to_string(tmp.path().join("tests/chat_channel.rs")).unwrap();
        assert!(test_src.contains("TestApp"), "{test_src}");
        assert!(test_src.contains(".subscribe("), "{test_src}");
        assert!(test_src.contains("timeout"), "{test_src}");
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
    fn ws_smoke_test_also_asserts_delivery() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let test_src = fs::read_to_string(tmp.path().join("tests/chat_channel.rs")).unwrap();
        assert!(test_src.contains("TestApp"), "{test_src}");
        assert!(test_src.contains(".subscribe("), "{test_src}");
    }

    #[test]
    fn sse_smoke_test_publish_handler_matches_production_handler() {
        // Regression test: the test's re-declared publish handler must
        // actually exercise message_fragment/maud rendering, the same as
        // the real src/channels/chat.rs handler — not a simplified copy
        // that only proves the pub/sub transport works.
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let production_src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        let test_src = fs::read_to_string(tmp.path().join("tests/chat_channel.rs")).unwrap();

        assert!(
            test_src.contains("fn message_fragment("),
            "SSE smoke test must render through message_fragment, the same \
             as production: {test_src}"
        );
        assert!(test_src.contains("Markup"), "{test_src}");

        // Extract each file's chat_publish body (brace-balanced, so trailing
        // content like an inline #[cfg(test)] module or the smoke test's own
        // #[tokio::test] fn is never swept in) and confirm they match — the
        // doc comment promises this is "a re-declaration of the same
        // handler", so it must actually be one.
        let extract_publish_body = |src: &str| {
            let name_start = src.find("chat_publish(").expect("chat_publish present");
            let brace_start = src[name_start..].find('{').unwrap() + name_start;
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
            src[brace_start..=end].to_owned()
        };
        assert_eq!(
            extract_publish_body(&production_src),
            extract_publish_body(&test_src),
            "the test's re-declared chat_publish must be byte-identical to \
             the production handler for SSE mode"
        );
    }

    // ── Flag behaviour ────────────────────────────────────────────────────

    #[test]
    fn dry_run_writes_no_new_files() {
        let tmp = project_with_main(default_main());
        let original_main = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();

        assert!(!tmp.path().join("src/channels/chat.rs").exists());
        assert!(!tmp.path().join("src/channels/mod.rs").exists());
        assert!(!tmp.path().join("tests/chat_channel.rs").exists());
        let main_after = fs::read_to_string(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(original_main, main_after, "dry run must not modify main.rs");
    }

    #[test]
    fn collision_without_force_returns_error() {
        let tmp = project_with_main(default_main());
        fs::create_dir_all(tmp.path().join("src/channels")).unwrap();
        fs::write(tmp.path().join("src/channels/chat.rs"), "// existing").unwrap();
        let err = plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)));
    }

    #[test]
    fn force_overwrites_existing_channel() {
        let tmp = project_with_main(default_main());
        fs::create_dir_all(tmp.path().join("src/channels")).unwrap();
        fs::write(tmp.path().join("src/channels/chat.rs"), "// old").unwrap();
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags {
                force: true,
                dry_run: false,
            })
            .unwrap();

        let src = fs::read_to_string(tmp.path().join("src/channels/chat.rs")).unwrap();
        assert!(src.contains("TOPIC"));
    }

    // ── Cargo.toml ────────────────────────────────────────────────────────

    #[test]
    fn execute_adds_ws_feature_to_cargo_toml() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("\"ws\""), "{cargo}");
    }

    #[test]
    fn sse_transport_ensures_htmx_and_maud_autumn_web_features() {
        // Regression test: SSE-mode generated code uses Markup/html!/
        // HTMX_JS_PATH/HTMX_SSE_JS_PATH, which are gated behind the
        // "maud"/"htmx" autumn-web features. Those are on by default, but a
        // project with `default-features = false` needs them ensured
        // explicitly — the same fix `generate scaffold --live` already
        // applies.
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("\"htmx\""), "{cargo}");
        assert!(cargo.contains("\"maud\""), "{cargo}");
    }

    #[test]
    fn ws_transport_does_not_ensure_htmx_or_maud_features() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(!cargo.contains("\"htmx\""), "{cargo}");
        assert!(!cargo.contains("\"maud\""), "{cargo}");
    }

    #[test]
    fn sse_transport_adds_maud_and_serde_deps() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("maud"), "{cargo}");
        assert!(cargo.contains("serde"), "{cargo}");
    }

    #[test]
    fn ws_transport_adds_serde_dep_but_not_maud() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Ws)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("serde"), "{cargo}");
        assert!(!cargo.contains("maud"), "{cargo}");
    }

    #[test]
    fn execute_adds_tokio_dev_dependency_for_smoke_test() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "Chat", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(cargo.contains("[dev-dependencies]"), "{cargo}");
        assert!(cargo.contains("tokio"), "{cargo}");
    }

    // ── Name normalisation ────────────────────────────────────────────────

    #[test]
    fn snake_case_input_is_normalised() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "chat_room", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src/channels/chat_room.rs").exists());
    }

    #[test]
    fn pascal_case_input_is_normalised() {
        let tmp = project_with_main(default_main());
        plan_channel(tmp.path(), "ChatRoom", Transport::Sse)
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        assert!(tmp.path().join("src/channels/chat_room.rs").exists());
    }
}
