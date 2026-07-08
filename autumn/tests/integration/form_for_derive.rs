//! Derived-`FormModel` integration tests for `#[model]` (issue #1135).
//!
//! Proves the acceptance criteria that the framework knows every field and its
//! type at the derive layer: a plain `#[model]` struct gains a `FormModel`
//! implementation whose `form_fields()` lists one type-appropriate descriptor
//! per editable column (primary key excluded), and that `form_for` renders a
//! complete form from it in a single call.
//!
//! NB: diesel prelude imports are deliberately omitted so the sync
//! `RunQueryDsl` never clashes with the model's generated async query code
//! (same convention as `encryption_columns.rs`).

#![cfg(all(feature = "db", feature = "maud"))]
// `Article`'s `f64` column makes the `#[model]` expansion (change tracking
// uses strict equality) trip clippy's `float_cmp`; that generated comparison
// is the macro's concern, not this test's, and an item-level allow can't
// reach the macro-emitted impls.
#![allow(clippy::float_cmp)]

use autumn_web::form::{Changeset, FieldControl, FormModel, form_for};

diesel::table! {
    articles (id) {
        id -> Integer,
        title -> Text,
        body -> Nullable<Text>,
        views -> Integer,
        rating -> Double,
        published -> Bool,
    }
}

#[autumn_web::model(table = "articles")]
pub struct Article {
    pub id: i32,
    pub title: String,
    pub body: Option<String>,
    pub views: i32,
    pub rating: f64,
    pub published: bool,
}

#[test]
fn derived_form_fields_exclude_pk_and_map_types() {
    let fields = Article::form_fields();

    // Primary key is excluded; remaining columns keep declaration order.
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["title", "body", "views", "rating", "published"]);

    // Humanized labels.
    assert_eq!(fields[0].label, "Title");

    // String -> Text, required (non-Option).
    assert!(matches!(fields[0].control, FieldControl::Text));
    assert!(fields[0].required);

    // Option<String> -> Text, not required.
    assert!(matches!(fields[1].control, FieldControl::Text));
    assert!(!fields[1].required);

    // i32 -> Number { step: "1" }, required.
    assert!(
        matches!(&fields[2].control, FieldControl::Number { step } if step.as_deref() == Some("1"))
    );
    assert!(fields[2].required);

    // f64 -> Number { step: "any" }.
    assert!(
        matches!(&fields[3].control, FieldControl::Number { step } if step.as_deref() == Some("any"))
    );

    // bool -> Checkbox.
    assert!(matches!(fields[4].control, FieldControl::Checkbox));
}

#[test]
fn form_for_renders_full_form_from_derived_model() {
    let changeset = Changeset::new(Article {
        id: 1,
        title: "Hello".into(),
        body: None,
        views: 3,
        rating: 4.5,
        published: true,
    });

    let html = form_for(&changeset, "/articles", "post")
        .csrf("tok")
        .render()
        .into_string();

    // Opening form tag + auto-injected CSRF + one control per field + submit.
    assert!(html.contains("<form"));
    assert!(html.contains(r#"name="_csrf""#));
    assert!(html.contains(r#"value="tok""#));
    assert!(html.contains(r#"name="title""#));
    assert!(html.contains(r#"name="body""#));
    assert!(html.contains(r#"name="views""#));
    assert!(html.contains(r#"type="checkbox""#));
    assert!(html.contains(r#"name="published""#));
    assert!(html.contains(r#"type="submit""#));
    // Current values are pre-filled from the changeset.
    assert!(html.contains(r#"value="Hello""#));
}

#[test]
fn derived_new_struct_decodes_unchecked_checkbox_as_false() {
    // Regression test (Codex P2 on #1587): `form_for` renders a non-nullable
    // `bool` as a checkbox, and `checkbox_input` deliberately emits no hidden
    // `false` fallback (serde_urlencoded rejects duplicate keys, so a hidden
    // sibling would 400 every *checked* submission). An unchecked box thus
    // submits no `published` key at all — the `#[model]`-generated insert
    // struct must decode that as `false` via `#[serde(default)]`, not reject
    // the whole submission with "missing field `published`".
    let changeset = Changeset::new(Article {
        id: 1,
        title: "Hello".into(),
        body: None,
        views: 3,
        rating: 4.5,
        published: false,
    });
    let html = form_for(&changeset, "/articles", "post")
        .csrf("tok")
        .render()
        .into_string();
    // Exactly one control carries the field name (no hidden fallback input).
    assert_eq!(html.matches(r#"name="published""#).count(), 1, "{html}");

    // A browser submitting this form with the box UNCHECKED sends every other
    // field but omits `published` entirely. Decode through the same
    // serde_urlencoded machinery axum's `Form` extractor and ChangesetForm
    // use, straight into the generated insert struct.
    let new_article: NewArticle =
        serde_urlencoded::from_str("title=Hello&body=&views=3&rating=4.5")
            .expect("unchecked checkbox submission must decode, not 400");
    assert!(!new_article.published);
    assert_eq!(new_article.title, "Hello");

    // And a CHECKED box submits `published=true` exactly once.
    let new_article: NewArticle =
        serde_urlencoded::from_str("title=Hello&body=&views=3&rating=4.5&published=true").unwrap();
    assert!(new_article.published);
}

#[test]
fn form_for_override_promotes_field_to_select() {
    // The scaffold uses this exact escape hatch to turn an enum column into a
    // <select>; here we prove it works on a derived model.
    let changeset = Changeset::new(Article {
        id: 1,
        title: "Hello".into(),
        body: None,
        views: 3,
        rating: 4.5,
        published: true,
    });

    let html = form_for(&changeset, "/articles", "post")
        .exclude("rating")
        .override_field(
            "title",
            FieldControl::Select {
                options: vec![
                    ("draft".to_string(), "Draft".to_string()),
                    ("live".to_string(), "Live".to_string()),
                ],
            },
        )
        .render()
        .into_string();

    assert!(html.contains("<select"));
    assert!(html.contains(r#"name="title""#));
    // Excluded field is gone.
    assert!(!html.contains(r#"name="rating""#));
}
