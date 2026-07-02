diesel::table! {
    users (id) {
        id -> Int8,
        username -> Text,
        password_hash -> Text,
        karma -> Int8,
        role -> Text,
        created_at -> Timestamp,
        avatar -> Nullable<Jsonb>,
    }
}

diesel::table! {
    subreddits (id) {
        id -> Int8,
        name -> Text,
        slug -> Text,
        description -> Text,
        creator_id -> Int8,
        subscriber_count -> Int8,
        created_at -> Timestamp,
    }
}

diesel::table! {
    posts (id) {
        id -> Int8,
        title -> Text,
        slug -> Text,
        body -> Text,
        url -> Nullable<Text>,
        author_id -> Int8,
        subreddit_id -> Int8,
        score -> Int8,
        hot_rank -> Float8,
        comment_count -> Int8,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    comments (id) {
        id -> Int8,
        body -> Text,
        author_id -> Int8,
        post_id -> Int8,
        parent_id -> Nullable<Int8>,
        score -> Int8,
        created_at -> Timestamp,
    }
}

diesel::table! {
    votes (id) {
        id -> Int8,
        user_id -> Int8,
        post_id -> Nullable<Int8>,
        comment_id -> Nullable<Int8>,
        value -> Int2,
        created_at -> Timestamp,
    }
}

diesel::table! {
    live_feed_events (id) {
        id -> Int8,
        subreddit_slug -> Text,
        event -> Jsonb,
        created_at -> Timestamp,
    }
}

// `post_tags` (the Post <-> Tag join table, #1324) has no entry here --
// `#[has_many(Tag, through = post_tags)]` on `Post` generates its own hidden
// `diesel::table!` for it.
diesel::table! {
    tags (id) {
        id -> Int8,
        name -> Text,
        slug -> Text,
    }
}

diesel::joinable!(posts -> users (author_id));
diesel::joinable!(posts -> subreddits (subreddit_id));
diesel::joinable!(comments -> users (author_id));
diesel::joinable!(comments -> posts (post_id));
diesel::joinable!(subreddits -> users (creator_id));

diesel::allow_tables_to_appear_in_same_query!(
    users,
    subreddits,
    posts,
    comments,
    votes,
    live_feed_events,
    tags,
);
