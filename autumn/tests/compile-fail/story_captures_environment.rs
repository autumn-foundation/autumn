//! AC1/AC2 (issue #1526): a `story!` block is coerced to a plain
//! `fn() -> Markup` pointer, so capturing anything from the surrounding
//! environment (a Db handle, AppState, request data, or any local) must be
//! a compile error — stories are zero-arg pure functions by construction.

use autumn_web::stories::story;

fn main() {
    let database_url = String::from("postgres://localhost/demo");

    let _story = story! {
        "App",
        "Leaky",
        {
            maud::html! { p { (database_url) } }
        }
    };
}
