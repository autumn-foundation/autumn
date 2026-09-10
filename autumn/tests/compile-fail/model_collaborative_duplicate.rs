diesel::table! { notes (id) { id -> Integer, body -> Text, } }

#[autumn_web::model(table = "notes")]
struct Note { id: i32, #[collaborative] #[collaborative] body: String }

fn main() {}
