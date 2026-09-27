diesel::table! {
    api_credentials (id) {
        id -> Int8,
        label -> Text,
        // Stored as an opaque AES-256-GCM envelope (#805), not plaintext.
        token -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    collections (id) {
        id -> Int8,
        title -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    collection_links (id) {
        id -> Int8,
        collection_id -> Int8,
        label -> Text,
        url -> Text,
        position -> Int4,
        created_at -> Timestamp,
    }
}

diesel::table! {
    pages (id) {
        id -> Int8,
        title -> Text,
        slug -> Text,
        body -> Text,
        status -> Text,
        lock_version -> Int4,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    revisions (id) {
        id -> Int8,
        page_id -> Int8,
        op -> Text,
        title -> Text,
        body -> Text,
        status -> Text,
        changed_by -> Nullable<Text>,
        summary -> Nullable<Text>,
        created_at -> Timestamp,
    }
}
