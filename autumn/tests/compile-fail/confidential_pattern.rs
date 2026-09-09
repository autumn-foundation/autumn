use autumn_web::confidential::{BlindIndexToken, ConfidentialField};
struct Email;
fn token() -> BlindIndexToken { BlindIndexToken::from_encoded("opaque") }
fn main() { let _ = ConfidentialField::<Email>::blind_index_eq(token()).like("%@example.com"); }
