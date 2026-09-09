use autumn_web::confidential::BlindIndexToken;
diesel::table! { records (id) { id -> Integer, email -> Text, } }
#[autumn_web::model(table = "records")]
pub struct Record { pub id: i32, #[confidential] pub email: String }
fn main() { let token = BlindIndexToken::from_encoded("opaque-token"); let _ = Record::email_blind_index(token); }
