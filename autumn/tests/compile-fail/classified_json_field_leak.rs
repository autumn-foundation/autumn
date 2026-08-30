//! #1654: lifting a `#[classified]` column into a response DTO cannot compile
//! either -- the taint is on the field's type, so a rename or a new endpoint
//! cannot reopen the hole. The diagnostic names the offending field.
use autumn_web::classify::Classified;

diesel::table! {
    customers (id) {
        id -> Integer,
        email -> Text,
    }
}

#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    #[classified]
    pub email: String,
}

#[derive(serde::Serialize)]
struct SupportView {
    email: Classified<String, CustomerEmailClassified>,
}

fn leak(customer: Customer) -> SupportView {
    SupportView {
        email: customer.email,
    }
}

fn main() {}
