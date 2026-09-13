diesel::table! { records (id) { id -> Integer, email -> Text, } }
#[autumn_web::model(table = "records")]
pub struct Record { pub id: i32, #[confidential] #[searchable] pub email: String }
fn main() {}
