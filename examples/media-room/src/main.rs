//! A minimal `autumn-media-plugin` app.
//!
//! It installs the media plugin with the **rooms** primitive
//! (`MediaPlugin::new().config(media).with_rooms()`), which mounts the room
//! signaling routes under `/api/media` and installs a `RoomService` extension.
//! This app then serves its own small surface — a home page and a JSON endpoint
//! — that call `RoomService` to **create** and **list** rooms, plus a `Create`
//! form, so the whole room lifecycle is reachable from a browser.
//!
//! The plugin uses the single-process defaults here: local storage and the
//! in-memory room store. A production app would load `[media]` from its profile
//! (`MediaConfig::from_autumn_toml` / `from_autumn_dir`) and select the
//! multi-process-safe `db` room store.

use std::sync::{Arc, Mutex};

use autumn_media_plugin::{MediaConfig, MediaPlugin, RoomService};
use autumn_web::{
    AppState, AutumnError, AutumnResult, Json, Markup, PreEscaped, Redirect, State, get, html,
    post, routes,
};

/// One created room, tracked by this demo so the home page can list them.
#[derive(Clone, serde::Serialize)]
struct RoomEntry {
    id: String,
    max_participants: usize,
    created_at: String,
}

/// A shared, in-memory log of the rooms this demo created.
///
/// The plugin's [`RoomService`] owns the real room state; this log is only the
/// demo's own "list rooms" surface (the plugin exposes create / join / leave /
/// roster, not a list-all endpoint). It is installed as an `AppState` extension,
/// so every request shares the same inner `Arc<Mutex<..>>`.
#[derive(Clone, Default)]
struct RoomLog(Arc<Mutex<Vec<RoomEntry>>>);

impl RoomLog {
    fn push(&self, entry: RoomEntry) {
        self.0
            .lock()
            .expect("room log lock not poisoned")
            .push(entry);
    }

    fn snapshot(&self) -> Vec<RoomEntry> {
        self.0.lock().expect("room log lock not poisoned").clone()
    }
}

/// Resolve the plugin-installed [`RoomService`], or fail with a `500`.
fn rooms_service(state: &AppState) -> AutumnResult<Arc<RoomService>> {
    state
        .extension::<RoomService>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("RoomService is not installed"))
}

/// Resolve this app's [`RoomLog`] extension, or fail with a `500`.
fn room_log(state: &AppState) -> AutumnResult<Arc<RoomLog>> {
    state
        .extension::<RoomLog>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("RoomLog is not installed"))
}

/// The home page: a create-room form plus the list of created rooms.
#[get("/")]
async fn index(State(state): State<AppState>) -> AutumnResult<Markup> {
    let rooms = room_log(&state)?.snapshot();
    Ok(page(&rooms))
}

/// Create a room through the plugin's [`RoomService`] and record it in the log.
#[post("/rooms")]
async fn create_room(State(state): State<AppState>) -> AutumnResult<Redirect> {
    let rooms = rooms_service(&state)?;
    let log = room_log(&state)?;

    let room = rooms.create().await.map_err(|error| error.into_autumn())?;
    log.push(RoomEntry {
        id: room.id.clone(),
        max_participants: room.max_participants,
        created_at: room.created_at.to_rfc3339(),
    });
    Ok(Redirect::to("/"))
}

/// List the rooms this demo has created, as JSON.
#[get("/api/rooms")]
async fn list_rooms(State(state): State<AppState>) -> AutumnResult<Json<Vec<RoomEntry>>> {
    Ok(Json(room_log(&state)?.snapshot()))
}

/// Render the home page.
fn page(rooms: &[RoomEntry]) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Autumn Media Rooms" }
                link rel="stylesheet" href="/static/css/autumn.css";
            }
            body {
                main class="container" {
                    h1 { "Autumn Media Rooms" }
                    p {
                        "A minimal " code { "autumn-media-plugin" } " app. The plugin is installed "
                        "with " code { ".with_rooms()" } ", which mounts the room signaling routes "
                        "under " code { "/api/media" } " and installs a " code { "RoomService" }
                        " this app calls to create and list rooms."
                    }

                    section {
                        h2 { "Create a room" }
                        form method="post" action="/rooms" {
                            button type="submit" { "Create a room" }
                        }
                    }

                    section {
                        h2 { "Rooms" }
                        @if rooms.is_empty() {
                            p id="rooms-empty" { "No rooms yet — create one above." }
                        } @else {
                            table {
                                thead {
                                    tr { th { "Room id" } th { "Seats" } th { "Created" } }
                                }
                                tbody {
                                    @for room in rooms {
                                        tr {
                                            td { code { (room.id) } }
                                            td { (room.max_participants) }
                                            td { (room.created_at) }
                                        }
                                    }
                                }
                            }
                        }
                        p {
                            "Also available as JSON at "
                            a href="/api/rooms" { code { "/api/rooms" } } "."
                        }
                    }

                    section {
                        h2 { "Room API (mounted by the plugin)" }
                        ul {
                            li { code { "POST /api/media/rooms" } " — create a room" }
                            li {
                                code { "POST /api/media/rooms/{room_id}/join" }
                                " — join (returns a session token + mesh WHIP/WHEP targets)"
                            }
                            li { code { "POST /api/media/rooms/{room_id}/leave" } " — leave" }
                            li {
                                code { "GET /api/media/rooms/{room_id}" }
                                " — member-gated roster (Authorization: Bearer <token>)"
                            }
                        }
                        p {
                            "To join a room, POST its id to the join route, e.g. "
                            code {
                                "curl -X POST localhost:3000/api/media/rooms/<id>/join "
                                "-H 'content-type: application/json' -d '{\"display_name\":\"Ada\"}'"
                            }
                            "."
                        }
                    }
                }
            }
        }
    }
}

#[autumn_web::main]
async fn main() {
    // Single-process defaults: local storage + the in-memory room store. A
    // production app would load `[media]` from its profile and select the `db`
    // room store for multi-process safety.
    let media = MediaConfig::default();

    autumn_web::app()
        .plugin(MediaPlugin::new().config(media).with_rooms())
        .state_initializer(|state| {
            state.insert_extension(RoomLog::default());
        })
        .routes(routes![index, create_room, list_rooms])
        .run()
        .await;
}
