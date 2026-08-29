//! #1654: the approved path compiles -- a `#[classified]` column released at a
//! declared boundary is a plain value again, and goes to the `Json` sink.
use autumn_web::extract::Json;
use autumn_web::reexports::axum::response::{IntoResponse as _, Response};

diesel::table! {
    customers (id) {
        id -> Integer,
        name -> Text,
        email -> Text,
    }
}

#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    pub name: String,
    #[classified]
    pub email: String,
}

autumn_web::declassify! {
    /// Support agents need the customer's email address to answer the ticket.
    pub SUPPORT_LOOKUP: CustomerEmailField => JsonResponse,
    purpose = "support_lookup",
    reason = "Support agents need the email address to answer the ticket.",
}

#[derive(serde::Serialize)]
struct SupportView {
    name: String,
    email: String,
}

fn released(customer: Customer) -> Response {
    Json(SupportView {
        name: customer.name,
        email: customer.email.declassify(&SUPPORT_LOOKUP),
    })
    .into_response()
}

fn main() {
    let _ = released;
}
