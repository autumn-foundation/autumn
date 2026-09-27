//! #1654: a declassification boundary is typed to the field it was declared
//! for, so one field's approved purpose cannot release another field's data.
diesel::table! {
    customers (id) {
        id -> Integer,
        email -> Text,
        phone -> Text,
    }
}

#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    #[classified]
    pub email: String,
    #[classified]
    pub phone: String,
}

autumn_web::declassify! {
    /// Support agents need the customer's email address to answer the ticket.
    pub SUPPORT_LOOKUP: CustomerEmailClassified => JsonResponse,
    purpose = "support_lookup",
    reason = "Support agents need the email address to answer the ticket.",
}

fn leak(customer: Customer) -> String {
    customer.phone.declassify(&SUPPORT_LOOKUP)
}

fn main() {}
