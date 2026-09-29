//!
//! Integration tests for the route macros.
//!
use autumn_web::{get, routes};

#[get("/one")]
async fn one() -> &'static str {
    "one"
}

#[get("/two")]
async fn two() -> &'static str {
    "two"
}

#[test]
fn routes_collects_single_handler() {
    let r = routes![one];
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].path, "/one");
    assert_eq!(r[0].name, "one");
}

#[test]
fn routes_collects_multiple_handlers() {
    let r = routes![one, two];
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].path, "/one");
    assert_eq!(r[1].path, "/two");
}

#[test]
fn routes_empty_returns_empty_vec() {
    let r: Vec<autumn_web::Route> = routes![];
    assert!(r.is_empty());
}

#[test]
fn routes_trailing_comma() {
    let r = routes![one, two,];
    assert_eq!(r.len(), 2);
}
