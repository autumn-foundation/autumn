// Diesel table definitions. These mirror `migrations/`.

diesel::table! {
    users (id) {
        id -> Int8,
        email -> Text,
        password_hash -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    organizations (id) {
        id -> Int8,
        name -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    memberships (id) {
        id -> Int8,
        tenant_id -> Text,
        user_id -> Int8,
        role -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    invitations (id) {
        id -> Int8,
        tenant_id -> Text,
        email -> Text,
        role -> Text,
        token_hash -> Text,
        status -> Text,
        invited_by_user_id -> Int8,
        expires_at -> Timestamp,
        created_at -> Timestamp,
    }
}

diesel::joinable!(memberships -> users (user_id));

diesel::allow_tables_to_appear_in_same_query!(users, organizations, memberships, invitations,);
