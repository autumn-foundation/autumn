//! #1654: the Diesel column wrapper carries the column's field marker, so a
//! value cannot be converted in as one classified column and back out as
//! another. An `F`-erasing wrapper would have let this release the email
//! through the phone column's boundary -- and record it against the wrong
//! column.
use autumn_web::classify::{Classified, ClassifiedText};

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

fn retype(customer: Customer) -> Classified<String, CustomerPhoneClassified> {
    let erased: ClassifiedText<CustomerEmailClassified> = customer.email.into();
    erased.into()
}

fn main() {}
