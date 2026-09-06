// Diesel table definition matching `migrations/00000000000000_create_notes`.
diesel::table! {
    notes (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
        pinned -> Bool,
        created_at -> Timestamp,
    }
}
