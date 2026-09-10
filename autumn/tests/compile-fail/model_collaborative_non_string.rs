diesel::table! { notes (id) { id -> Integer, body -> Integer, } }

#[autumn_web::model(table = "notes")]
struct Note { id: i32, #[collaborative] body: i32 }

fn main() {}
