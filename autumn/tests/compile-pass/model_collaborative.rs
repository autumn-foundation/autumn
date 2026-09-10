diesel::table! { notes (id) { id -> Integer, body -> Text, title -> Text, } }

#[autumn_web::model(table = "notes")]
struct Note { id: i32, #[collaborative] body: String, title: String }

fn main() {
    assert_eq!(Note::__AUTUMN_COLLABORATIVE_FIELDS, &["body"]);
    let _: &[&str] = <Note as autumn_web::collaboration::CollaborativeField>::COLLABORATIVE_FIELDS;
}
