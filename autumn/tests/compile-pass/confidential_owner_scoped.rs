diesel::table! { records (id) { id -> Integer, owner_id -> Integer, email -> Text, } }
#[autumn_web::model(table = "records")]
pub struct Record { pub id: i32, pub owner_id: i32, #[confidential] pub email: String }
fn owner_scoped(row: Record, owner: i32) -> Option<Record> { (row.owner_id == owner).then_some(row) }
fn main() {}
