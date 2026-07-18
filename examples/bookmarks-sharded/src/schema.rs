autumn_web::reexports::diesel::table! {
    bookmarks (id) {
        id -> Int8,
        tenant_id -> Text,
        url -> Text,
        title -> Text,
        tag -> Text,
        created_at -> Timestamp,
    }
}
