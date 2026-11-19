// Compile-fail: a `chrono` date-time field on `#[model]` is rejected, and the
// error names the `jiff` type that replaces it. The local `chrono` module
// stands in for the crate, which the framework no longer depends on.
use autumn_web::model;

mod chrono {
    pub struct NaiveDateTime;
}

#[model]
pub struct Event {
    #[id]
    pub id: i64,
    pub starts_at: chrono::NaiveDateTime,
}

fn main() {}
