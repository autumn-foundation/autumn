//! #1325: two counter-cached `belongs_to` legs that resolve onto the **same**
//! counter column would both move one column, double-counting every insert.
//! Because the column name is convention-derived from the *child* model, two
//! legs to the same parent type collide by default — a directed compile error
//! pointing at `counter_cache = "<column>"` rather than silent drift.
use autumn_web::model;

diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
        message_count -> BigInt,
    }
}

diesel::table! {
    messages (id) {
        id -> BigInt,
        body -> Text,
        sender_id -> BigInt,
        recipient_id -> BigInt,
    }
}

#[model]
pub struct User {
    #[id]
    pub id: i64,
    pub name: String,
    pub message_count: i64,
}

#[model]
#[belongs_to(User, fk = sender_id, name = sender, counter_cache)]
#[belongs_to(User, fk = recipient_id, name = recipient, counter_cache)]
pub struct Message {
    #[id]
    pub id: i64,
    pub body: String,
    pub sender_id: i64,
    pub recipient_id: i64,
}

fn main() {}
