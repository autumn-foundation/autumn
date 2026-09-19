//! Reading validation details off an `AutumnError` (issue #2587).
//!
//! The per-field map used to be reachable only by rendering the error as
//! `application/problem+json` and parsing the body. These tests exercise the
//! accessors from outside the crate, the way a GraphQL resolver, a `#[task]`,
//! a CLI or an MCP tool would, and pin the accessors to the rendered body.

use std::collections::HashMap;

use autumn_web::error::AutumnError;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::validation::ValidateExt;
use axum::response::IntoResponse;
use http::StatusCode;
use validator::Validate;

#[derive(Validate)]
struct NewNote {
    #[validate(length(min = 1, max = 120, message = "Title must be 1-120 characters"))]
    title: String,
    #[validate(email(message = "Must be a valid email address"))]
    author: String,
}

/// The `before_create` hook shape from the issue: validate, then report why.
fn reject() -> AutumnError {
    let note = NewNote {
        title: String::new(),
        author: "nope".into(),
    };
    let Err(err) = note.validate() else {
        panic!("the fixture passed validation; it is meant to fail");
    };
    err
}

#[test]
fn details_name_the_failing_fields() {
    let err = reject();
    let details = err.details().expect("a validation error carries details");

    assert_eq!(
        details["title"],
        ["Title must be 1-120 characters".to_owned()]
    );
    assert_eq!(
        details["author"],
        ["Must be a valid email address".to_owned()]
    );
    assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err.code(), "autumn.validation_failed");
}

#[test]
fn display_says_which_field_failed() {
    // Fields are sorted, so the message is stable run to run.
    assert_eq!(
        reject().to_string(),
        "Validation failed: author: Must be a valid email address; \
         title: Title must be 1-120 characters"
    );
}

#[test]
fn other_errors_carry_no_details() {
    assert!(
        AutumnError::not_found_msg("no such note")
            .details()
            .is_none()
    );
    assert_eq!(
        AutumnError::not_found_msg("no such note").code(),
        "autumn.not_found"
    );
}

#[tokio::test]
async fn accessors_agree_with_the_problem_details_body() -> Result<(), axum::Error> {
    let expected_code = reject().code();
    let expected_details = reject()
        .details()
        .cloned()
        .expect("a validation error carries details");

    let response = reject().into_response();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("problem+json body");

    assert_eq!(json["code"], &*expected_code);
    // `detail` still renders the bare title; the HTTP contract is unchanged.
    assert_eq!(json["detail"], "Validation failed");

    let rendered: HashMap<String, Vec<String>> = json["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .map(|entry| {
            let field = entry["field"].as_str().expect("field").to_owned();
            let messages = entry["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .map(|message| message.as_str().expect("message").to_owned())
                .collect();
            (field, messages)
        })
        .collect();
    assert_eq!(rendered, expected_details);
    Ok(())
}

#[post("/notes")]
async fn create_note() -> AutumnResult<&'static str> {
    Err(reject())
}

#[tokio::test]
async fn the_rendered_detail_is_still_the_bare_title() {
    // Through the whole stack, not `into_response` alone: the exception
    // filter re-renders the body from `AutumnErrorInfo`, which carries the
    // wrapped message. The field list must not reach `detail`.
    let response = TestApp::new()
        .routes(routes![create_note])
        .build()
        .post("/notes")
        .header("accept", "application/json")
        .send()
        .await;

    response.assert_status(422);
    let json: serde_json::Value = response.json();

    assert_eq!(json["detail"], "Validation failed");
    assert_eq!(json["code"], "autumn.validation_failed");
    let fields: Vec<&str> = json["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .map(|entry| entry["field"].as_str().expect("field"))
        .collect();
    assert_eq!(fields, ["author", "title"]);
}
