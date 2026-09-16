//! Collaborative notes — two browsers, one text field, no lost characters.
//!
//! The whole collaboration story in one small app:
//!
//! - `Note::body` is `#[collaborative]`, so the column holds a
//!   `CollabText` document instead of a plain string.
//! - `GET /notes/{id}` renders the editor; `static/collab.js` keeps a thin
//!   replica of the document so it can anchor an edit to a character id.
//! - `#[ws]` + `serve_socket` is the whole server side: the hub merges every
//!   edit, streams the operations, and reports who is editing and where.
//!
//! Notes live in memory here so the example runs with `cargo run -p
//! collab-notes` and nothing else. A real app stores `note.body` in its
//! `TEXT` column — the same value, through the same Diesel codec — and
//! seeds the hub from the row with `CollabHub::open_with`. See
//! `docs/guide/collaboration.md`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use autumn_web::collab::hub::{doc_key, serve_socket};
use autumn_web::collab::{CollabHub, CollabText};
use autumn_web::prelude::*;
use autumn_web::ws::{WebSocket, WsHandler};

diesel::table! {
    notes (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

/// A note whose body merges concurrent edits.
///
/// `#[collaborative]` is the whole opt-in. It generates `body_text()`,
/// `body_insert(..)`, `body_set_text(..)` and `body_merge(..)`, and it
/// registers the column so framework surfaces can find it.
#[autumn_web::model(table = "notes")]
pub struct Note {
    #[id]
    pub id: i64,
    pub title: String,
    #[collaborative]
    pub body: CollabText,
}

/// The app's note store. Stands in for a table: a real app uses
/// `#[repository]` and lets Diesel encode the column.
#[derive(Clone, Default)]
struct Notes(Arc<Mutex<BTreeMap<i64, Note>>>);

impl Notes {
    fn seeded() -> Self {
        let store = Self::default();
        for (id, title, body) in [
            (1, "Shopping list", "eggs\nmilk\n"),
            (2, "Release notes", "Autumn ships collaborative fields.\n"),
        ] {
            store.put(Note {
                id,
                title: title.to_owned(),
                body: CollabText::from_text("seed", body),
            });
        }
        store
    }

    fn put(&self, note: Note) {
        self.0
            .lock()
            .expect("note store lock")
            .insert(note.id, note);
    }

    fn get(&self, id: i64) -> Option<Note> {
        self.0.lock().expect("note store lock").get(&id).cloned()
    }

    fn all(&self) -> Vec<Note> {
        self.0
            .lock()
            .expect("note store lock")
            .values()
            .cloned()
            .collect()
    }

    /// Write the live document back, the way a real app writes the column.
    fn save_body(&self, id: i64, body: CollabText) {
        if let Some(note) = self.0.lock().expect("note store lock").get_mut(&id) {
            note.body = body;
        }
    }
}

fn notes(state: &AppState) -> AutumnResult<Arc<Notes>> {
    state
        .extension::<Notes>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("note store is not installed"))
}

/// The note index.
#[get("/")]
#[public]
async fn index(State(state): State<AppState>) -> AutumnResult<Markup> {
    let all = notes(&state)?.all();
    Ok(html! {
        (page_head("Collaborative notes"))
        body {
            h1 { "Collaborative notes" }
            p { "Open one note in two browser windows and type in both." }
            ul {
                @for note in &all {
                    li { a href=(format!("/notes/{}", note.id)) { (note.title) } }
                }
            }
        }
    })
}

/// The editor. Everything live happens over the socket below.
#[get("/notes/{id}")]
#[public]
async fn editor(State(state): State<AppState>, id: Path<i64>) -> AutumnResult<Markup> {
    let note = notes(&state)?
        .get(*id)
        .ok_or_else(|| AutumnError::not_found_msg("no such note"))?;
    let socket = format!("/notes/{}/collab", note.id);
    Ok(html! {
        (page_head(&note.title))
        body {
            h1 { (note.title) }
            p { a href="/" { "← all notes" } }
            main {
                textarea id="editor" rows="14" cols="60" data-socket=(socket)
                    aria-label="Note body" {}
                aside {
                    h2 { "Editing now" }
                    ul id="roster" {}
                    p id="status" { "connecting…" }
                }
            }
            script src=(asset_url("collab.js")) defer {}
        }
    })
}

/// The collaboration socket: join the document, then let the hub do the rest.
///
/// The actor id must be unique per connection — it is the id every character
/// this editor types carries.
#[ws("/notes/{id}/collab")]
#[public]
async fn collaborate(state: AppState, hub: CollabHub, id: Path<i64>) -> impl WsHandler {
    let note_id = *id;
    let store = state.extension::<Notes>();
    // `open_with` seeds from the record only the first time; a second editor
    // joins the live document rather than a stale copy of the row. It refuses
    // when the registry is full, which closes the socket: an app under that
    // load asks the editor to come back rather than serving a second, private
    // copy of the note.
    let doc = store
        .as_ref()
        .and_then(|store| store.get(note_id))
        .and_then(|note| {
            // The hub logs the refusal itself, so `ok()` loses nothing.
            hub.open_with(&doc_key("notes", note_id, "body"), || note.body.clone())
                .ok()
        });
    let actor = state.entropy().uuid_v4().to_string();
    let label = format!("Guest {}", &actor[..4]);

    move |socket: WebSocket| async move {
        // An unknown note, or a full registry, closes the socket.
        let (Some(doc), Some(store)) = (doc, store) else {
            return;
        };
        serve_socket(&doc, actor, label, socket).await;
        // Persist on leave. The hub evicts the document when its last editor
        // goes, but this handle still reads it, so the final state is safe to
        // take here. A busier app also saves on a timer.
        store.save_body(note_id, doc.document());
    }
}

fn page_head(title: &str) -> Markup {
    html! {
        (maud::DOCTYPE)
        meta charset="utf-8";
        meta name="viewport" content="width=device-width, initial-scale=1";
        title { (title) }
        style {
            "body{font-family:system-ui,sans-serif;margin:2rem;max-width:60rem}"
            "main{display:flex;gap:2rem;align-items:flex-start}"
            "textarea{font:1rem/1.5 ui-monospace,monospace;padding:.5rem}"
            "aside{min-width:12rem}"
            "#status{color:#666;font-size:.9rem}"
        }
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .state_initializer(|state| {
            state.insert_extension(Notes::seeded());
        })
        .routes(routes![index, editor, collaborate])
        .run()
        .await;
}
