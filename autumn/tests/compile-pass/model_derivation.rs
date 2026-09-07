//! #1769 compile-pass: every production of the `#[derivation]` filter grammar
//! and both transforms, so each branch of the spec emitter type-checks and the
//! two lowerings of one filter are visible side by side.
//!
//! * `Comment` carries a plain `counter_cache` **and** six derivations, which
//!   is what proves the two share one spec slice: the counter-cache leg comes
//!   first, unfiltered, and the derivations follow in declaration order.
//! * `Membership` declares an unfiltered `count` with no `#[belongs_to]` at
//!   all, so the foreign key resolves by the `{snake(Parent)}_id` convention.
//! * `Plain` declares neither, proving a model without them still resolves to
//!   the empty blanket impl.
use autumn_web::model;
use autumn_web::repository::AutumnCounterCaches as _;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        comment_count -> BigInt,
        published_comment_count -> BigInt,
        draft_comment_count -> BigInt,
        featured_comment_count -> BigInt,
        long_comment_count -> BigInt,
        anonymous_comment_count -> BigInt,
        visible_score -> BigInt,
    }
}

diesel::table! {
    teams (id) {
        id -> BigInt,
        member_count -> BigInt,
    }
}

diesel::table! {
    comments (id) {
        id -> BigInt,
        post_id -> BigInt,
        published -> Bool,
        status -> Text,
        length -> Nullable<BigInt>,
        author_id -> Nullable<BigInt>,
        score -> Integer,
    }
}

diesel::table! {
    memberships (id) {
        id -> BigInt,
        team_id -> BigInt,
    }
}

diesel::table! {
    plains (id) {
        id -> BigInt,
        post_id -> BigInt,
    }
}

#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
    #[default]
    pub published_comment_count: i64,
    #[default]
    pub draft_comment_count: i64,
    #[default]
    pub featured_comment_count: i64,
    #[default]
    pub long_comment_count: i64,
    #[default]
    pub anonymous_comment_count: i64,
    #[default]
    pub visible_score: i64,
}

#[model]
pub struct Team {
    #[id]
    pub id: i64,
    #[default]
    pub member_count: i64,
}

#[model]
#[belongs_to(Post, counter_cache)]
#[derivation(Post, column = "published_comment_count", filter = published)]
#[derivation(Post, column = "draft_comment_count", filter = !published)]
#[derivation(Post, column = "featured_comment_count", filter = status == "featured")]
#[derivation(Post, column = "long_comment_count", filter = length > 500)]
#[derivation(Post, column = "anonymous_comment_count", filter = author_id.is_none())]
#[derivation(Post, column = "visible_score", transform = sum(score), filter = published && score > 0)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub status: String,
    pub length: Option<i64>,
    pub author_id: Option<i64>,
    pub score: i32,
}

#[model]
#[derivation(Team, column = "member_count")]
pub struct Membership {
    #[id]
    pub id: i64,
    pub team_id: i64,
}

#[model]
#[belongs_to(Post)]
pub struct Plain {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

fn main() {
    // A live, published, featured, long, anonymous comment scoring 5: it
    // qualifies for every derivation.
    let counted = Comment {
        id: 1,
        post_id: 7,
        published: true,
        status: "featured".to_owned(),
        length: Some(900),
        author_id: None,
        score: 5,
    };
    // Its mirror image: it qualifies for `draft_comment_count` only.
    let skipped = Comment {
        id: 2,
        post_id: 7,
        published: false,
        status: "draft".to_owned(),
        length: Some(10),
        author_id: Some(3),
        score: 5,
    };

    let specs = Comment::counter_caches();
    assert_eq!(specs.len(), 7);
    assert!(Comment::HAS_COUNTER_CACHES);

    // The counter-cache leg keeps the unfiltered shape: every live row
    // contributes 1, and it is not a derivation.
    assert_eq!(specs[0].counter_column, "comment_count");
    assert_eq!(specs[0].contrib_sql, "1");
    assert_eq!(specs[0].filter_sql, "");
    assert!(specs[0].derivation.is_none());
    assert_eq!((specs[0].contrib_of)(&skipped), 1);

    // Bare bool field.
    let published = &specs[1];
    assert_eq!(published.counter_column, "published_comment_count");
    assert_eq!(published.parent_table, "posts");
    assert_eq!(published.parent_pk, "id");
    assert_eq!(published.child_table, "comments");
    assert_eq!(published.fk_column, "post_id");
    assert_eq!(published.contrib_sql, "1");
    assert_eq!(published.filter_sql, " AND ({c}.\"published\" = TRUE)");
    assert_eq!((published.contrib_of)(&counted), 1);
    assert_eq!((published.contrib_of)(&skipped), 0);
    let def = published.derivation.expect("registered definition");
    assert_eq!(def.name, "posts.published_comment_count");
    assert_eq!(def.model, "Comment");
    assert_eq!(def.transform, "count");
    assert_eq!(def.column, "published_comment_count");
    assert!(!def.child_soft_delete);
    assert!(def.tenant_column.is_none());

    // Negated bool field.
    let draft = &specs[2];
    assert_eq!(draft.filter_sql, " AND ({c}.\"published\" = FALSE)");
    assert_eq!((draft.contrib_of)(&counted), 0);
    assert_eq!((draft.contrib_of)(&skipped), 1);

    // String equality: the literal is single-quoted in SQL.
    let featured = &specs[3];
    assert_eq!(featured.filter_sql, " AND ({c}.\"status\" = 'featured')");
    assert_eq!((featured.contrib_of)(&counted), 1);
    assert_eq!((featured.contrib_of)(&skipped), 0);

    // Integer comparison on a nullable column: a NULL row is excluded, which
    // is what SQL's `col > 500` already does.
    let long = &specs[4];
    assert_eq!(long.filter_sql, " AND ({c}.\"length\" > 500)");
    assert_eq!((long.contrib_of)(&counted), 1);
    assert_eq!((long.contrib_of)(&skipped), 0);

    // NULL probe.
    let anonymous = &specs[5];
    assert_eq!(anonymous.filter_sql, " AND ({c}.\"author_id\" IS NULL)");
    assert_eq!((anonymous.contrib_of)(&counted), 1);
    assert_eq!((anonymous.contrib_of)(&skipped), 0);

    // Conjunction plus `sum`: the contribution is the summed column, not 1.
    let score = &specs[6];
    assert_eq!(score.counter_column, "visible_score");
    assert_eq!(score.contrib_sql, "{c}.\"score\"");
    assert_eq!(
        score.filter_sql,
        " AND (({c}.\"published\" = TRUE) AND ({c}.\"score\" > 0))"
    );
    assert_eq!((score.contrib_of)(&counted), 5);
    assert_eq!((score.contrib_of)(&skipped), 0);
    assert_eq!(
        score.derivation.expect("registered definition").transform,
        "sum(score)"
    );

    // An unfiltered count with a convention-derived foreign key.
    let specs = Membership::counter_caches();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].fk_column, "team_id");
    assert_eq!(specs[0].parent_table, "teams");
    assert_eq!(specs[0].filter_sql, "");
    assert_eq!(specs[0].contrib_sql, "1");
    assert_eq!(
        specs[0].derivation.expect("registered definition").name,
        "teams.member_count"
    );
    assert_eq!((specs[0].contrib_of)(&Membership { id: 1, team_id: 4 }), 1);

    // Neither declaration -> the empty blanket impl.
    assert!(Plain::counter_caches().is_empty());
    assert!(!Plain::HAS_COUNTER_CACHES);
}
