use autumn_web::confidential::{BlindIndexToken, ConfidentialField};
struct Email;
fn token() -> BlindIndexToken { BlindIndexToken::from_encoded("opaque") }
fn main() {
    use diesel::QueryDsl as _;
    diesel::table! { records (id) { id -> Integer, } }
    let predicate = ConfidentialField::<Email>::blind_index_eq(token());
    let _ = records::table.filter(predicate);
}
