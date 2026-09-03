//! #1702: a `through = <join_table>` association's foreign key names a column on
//! the **join table**, not on the target model, so a cascade emitted for it
//! would call the target repository with a column that does not exist there
//! (`tags.post_id`) — deleting or nullifying the wrong rows. The combination is
//! a directed compile error rather than a silent mis-cascade.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
    }
}

diesel::table! {
    tags (id) {
        id -> BigInt,
        label -> Text,
    }
}

#[model]
pub struct Tag {
    #[id]
    pub id: i64,
    pub label: String,
}

#[model]
#[has_many(Tag, through = post_tags, dependent = destroy)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

fn main() {}
