//! #1654: a `#[model]` holding a `#[classified]` column cannot be handed to the
//! `Json` response sink. The diagnostic names the model and the sink.
use autumn_web::extract::Json;
use autumn_web::reexports::axum::response::IntoResponse;

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

// The handler shape a leak actually takes: the response type is what the sink
// bound is checked against.
fn leak(customer: Customer) -> impl IntoResponse {
    Json(customer)
}

fn main() {}
