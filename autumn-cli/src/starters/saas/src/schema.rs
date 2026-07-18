// Diesel table definitions. These mirror `migrations/`.

diesel::table! {
    users (id) {
        id -> Int8,
        email -> Text,
        password_hash -> Text,
        tenant_id -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    projects (id) {
        id -> Int8,
        tenant_id -> Text,
        name -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    // Persistent "remember-me" login chains (issue #1397). One row per device
    // login-chain, keyed by the stable opaque `series`; `token_hash` rotates on
    // every use for theft detection. Only the SHA-256 hash of the current token
    // is stored, never the raw token.
    remember_tokens (series) {
        series -> Text,
        user_id -> Int8,
        tenant_id -> Text,
        token_hash -> Text,
        previous_token_hash -> Nullable<Text>,
        rotated_at -> Nullable<Timestamptz>,
        expires_at -> Timestamptz,
        ip -> Text,
        user_agent -> Text,
        ua_family -> Text,
        ua_os -> Text,
        ua_device -> Text,
        label -> Nullable<Text>,
        created_at -> Timestamptz,
        last_used_at -> Nullable<Timestamptz>,
    }
}
