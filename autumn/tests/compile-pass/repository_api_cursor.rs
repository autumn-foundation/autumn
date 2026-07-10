//! Compile-pass: `#[repository(api = ..., cursor_key = ...)]` opts the
//! generated JSON list endpoint into keyset/cursor mode. The list handler
//! must call `cursor_page()` and return a `CursorPage`, which requires the
//! `IntoResponse for CursorPage` impl to satisfy the handler return type.

mod schema {
    autumn_web::reexports::diesel::table! {
        events (id) {
            id -> Int8,
            created_at -> Timestamptz,
        }
    }
}

use schema::events;

#[autumn_web::model]
pub struct Event {
    #[id]
    pub id: i64,
    pub created_at: autumn_web::reexports::chrono::DateTime<autumn_web::reexports::chrono::Utc>,
}

#[autumn_web::repository(
    Event,
    api = "/api/events",
    cursor_key = created_at,
    cursor_key_type = autumn_web::reexports::chrono::DateTime<autumn_web::reexports::chrono::Utc>,
)]
pub trait EventRepository {}

fn main() {
    let _ = event_api_list;
    let _ = event_api_get;
    let _ = event_api_create;
    let _ = event_api_update;
    let _ = event_api_delete;
}
