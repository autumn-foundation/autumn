//! Compile-fail test: a ledgered `tenant_scoped` repository needs a
//! non-nullable tenant column (issue #2319).
//!
//! A revision written with a NULL tenant is not visible to a tenant-scoped
//! read, and a cross-tenant read is refused. No read can reach that chain.

mod schema {
    autumn_web::reexports::diesel::table! {
        ledgered_nullable_tenant_notes (id) {
            id -> Int8,
            tenant_id -> Nullable<Text>,
            content -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::ledgered_nullable_tenant_notes;

#[autumn_web::model(table = "ledgered_nullable_tenant_notes")]
pub struct LedgeredNullableTenantNote {
    #[id]
    pub id: i64,
    pub tenant_id: Option<String>,
    pub content: String,
    #[default]
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgeredNullableTenantNote,
    table = "ledgered_nullable_tenant_notes",
    tenant_scoped,
    soft_delete,
    ledgered = true
)]
pub trait LedgeredNullableTenantNoteRepository {}

fn main() {}
