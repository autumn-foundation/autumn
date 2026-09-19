//! Diesel table definitions. These mirror `migrations/`.

diesel::table! {
    users (id) {
        id -> Int8,
        username -> Text,
        email -> Text,
        password_hash -> Text,
        display_name -> Text,
        role -> Text,
        bio -> Text,
        website -> Text,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    options (id) {
        id -> Int8,
        name -> Text,
        value -> Text,
        autoload -> Bool,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    attachments (id) {
        id -> Int8,
        title -> Text,
        slug -> Text,
        file -> Nullable<Jsonb>,
        mime_type -> Text,
        byte_size -> Int8,
        width -> Nullable<Int4>,
        height -> Nullable<Int4>,
        alt_text -> Text,
        caption -> Text,
        uploader_id -> Nullable<Int8>,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    posts (id) {
        id -> Int8,
        post_type -> Text,
        title -> Text,
        slug -> Text,
        excerpt -> Text,
        body -> Text,
        status -> Text,
        author_id -> Int8,
        parent_id -> Nullable<Int8>,
        featured_media_id -> Nullable<Int8>,
        menu_order -> Int4,
        comment_status -> Text,
        password -> Text,
        sticky -> Bool,
        comment_count -> Int8,
        published_at -> Nullable<Timestamp>,
        lock_version -> Int4,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    post_meta (id) {
        id -> Int8,
        post_id -> Int8,
        meta_key -> Text,
        meta_value -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    terms (id) {
        id -> Int8,
        taxonomy -> Text,
        name -> Text,
        slug -> Text,
        description -> Text,
        parent_id -> Nullable<Int8>,
        post_count -> Int8,
        created_at -> Timestamp,
    }
}

// The post <-> term join. `#[has_many(Term, through = post_terms)]` on `Post`
// declares its own private copy of this table for the write path; this one is
// the read path's — the archive screens join it to `posts` to list the content
// filed under a term. Two `table!` expansions of one physical table is the
// documented m2m pattern, as long as they never share a scope.
diesel::table! {
    post_terms (id) {
        id -> Int8,
        post_id -> Int8,
        term_id -> Int8,
    }
}

diesel::table! {
    revisions (id) {
        id -> Int8,
        post_id -> Int8,
        title -> Text,
        excerpt -> Text,
        body -> Text,
        status -> Text,
        author_id -> Nullable<Int8>,
        summary -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    comments (id) {
        id -> Int8,
        post_id -> Int8,
        parent_id -> Nullable<Int8>,
        author_id -> Nullable<Int8>,
        author_name -> Text,
        author_email -> Text,
        author_url -> Text,
        author_ip -> Text,
        body -> Text,
        status -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    menus (id) {
        id -> Int8,
        name -> Text,
        slug -> Text,
        location -> Text,
        created_at -> Timestamp,
    }
}

diesel::table! {
    menu_items (id) {
        id -> Int8,
        menu_id -> Int8,
        parent_id -> Nullable<Int8>,
        label -> Text,
        url -> Text,
        post_id -> Nullable<Int8>,
        term_id -> Nullable<Int8>,
        position -> Int4,
    }
}

diesel::table! {
    widgets (id) {
        id -> Int8,
        sidebar -> Text,
        kind -> Text,
        title -> Text,
        settings -> Jsonb,
        position -> Int4,
    }
}

// `posts.parent_id`, `terms.parent_id`, `comments.parent_id` and
// `menu_items.parent_id` are self-references, so they get no `joinable!` — a
// table cannot be joined to itself through the generated DSL. Hierarchy walks
// go through the explicit ancestor queries in `models.rs` instead.
diesel::joinable!(posts -> users (author_id));
diesel::joinable!(posts -> attachments (featured_media_id));
diesel::joinable!(post_meta -> posts (post_id));
diesel::joinable!(post_terms -> posts (post_id));
diesel::joinable!(post_terms -> terms (term_id));
diesel::joinable!(revisions -> posts (post_id));
diesel::joinable!(comments -> posts (post_id));
diesel::joinable!(menu_items -> menus (menu_id));

diesel::allow_tables_to_appear_in_same_query!(
    attachments,
    comments,
    menu_items,
    menus,
    options,
    post_meta,
    post_terms,
    posts,
    revisions,
    terms,
    users,
    widgets,
);
