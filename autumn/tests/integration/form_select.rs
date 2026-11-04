//! Select: `<optgroup>`, `multiple` and the opt-in style (issue #3409).
//!
//! Run with `cargo test --test integration_tests form_select`.

#![cfg(feature = "maud")]

use std::collections::HashMap;

use autumn_web::form::{
    Changeset, FieldControl, FieldOptions, FormField, FormModel, form_for, from_urlencoded,
    grouped_select_input, grouped_select_input_with, required_grouped_select_input,
    required_select_input, select_input, select_input_with, text_input_with,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Trip {
    city: String,
    tags: Vec<String>,
}

fn trip() -> Trip {
    Trip {
        city: "lyon".into(),
        tags: vec!["rust".into(), "db".into()],
    }
}

const TOP: &[(&str, &str)] = &[("", "Choose a city")];
const GROUPS: &[(&str, &[(&str, &str)])] = &[
    ("France", &[("paris", "Paris"), ("lyon", "Lyon")]),
    ("Japan", &[("tokyo", "Tokyo"), ("osaka", "Osaka")]),
];
const TAGS: &[(&str, &str)] = &[("rust", "Rust"), ("web", "Web"), ("db", "Databases")];

fn selected_count(html: &str) -> usize {
    html.matches(" selected").count()
}

// ── Optgroup ─────────────────────────────────────────────────────────────────

#[test]
fn grouped_select_writes_top_options_then_one_optgroup_per_group() {
    let html =
        grouped_select_input(&Changeset::new(trip()), "city", "City", TOP, GROUPS).into_string();
    let want = concat!(
        r#"<option value="">Choose a city</option>"#,
        r#"<optgroup label="France">"#,
        r#"<option value="paris">Paris</option>"#,
        r#"<option value="lyon" selected>Lyon</option>"#,
        "</optgroup>",
        r#"<optgroup label="Japan">"#,
        r#"<option value="tokyo">Tokyo</option>"#,
        r#"<option value="osaka">Osaka</option>"#,
        "</optgroup>",
        "</select>",
    );
    assert!(html.contains(want), "{html}");
    assert!(
        html.starts_with(r#"<div id="city-field" class="autumn-field"><label for="city""#),
        "{html}"
    );
    assert!(html.contains(r#"<select id="city" name="city""#), "{html}");
}

#[test]
fn grouped_select_escapes_the_group_label() {
    let groups: &[(&str, &[(&str, &str)])] = &[(r#"A "q" <b>"#, &[("a", "A")])];
    let html =
        grouped_select_input(&Changeset::new(trip()), "city", "City", &[], groups).into_string();
    assert!(
        html.contains(r#"<optgroup label="A &quot;q&quot; &lt;b&gt;">"#),
        "{html}"
    );
}

#[test]
fn grouped_select_with_no_top_options_starts_with_a_group() {
    let html =
        grouped_select_input(&Changeset::new(trip()), "city", "City", &[], GROUPS).into_string();
    assert!(
        html.contains(r#"aria-invalid="false"><optgroup label="France">"#),
        "{html}"
    );
}

#[test]
fn grouped_select_marks_a_top_level_blank_for_a_missing_value() {
    let cs = Changeset::new(serde_json::json!({}));
    let html = grouped_select_input(&cs, "city", "City", TOP, GROUPS).into_string();
    assert!(html.contains(r#"<option value="" selected>"#), "{html}");
    assert_eq!(selected_count(&html), 1, "{html}");
}

#[test]
fn required_grouped_select_is_required() {
    let html = required_grouped_select_input(&Changeset::new(trip()), "city", "City", TOP, GROUPS)
        .into_string();
    assert!(html.contains(r#" required aria-required="true""#), "{html}");
    assert!(html.contains("autumn-field__required"), "{html}");
}

#[test]
fn grouped_select_wires_errors_and_the_hint() {
    let mut errors = HashMap::new();
    errors.insert("city".to_owned(), vec!["is <bad>".to_owned()]);
    let cs = Changeset::from_errors(trip(), errors);
    let html = grouped_select_input_with(
        &cs,
        "city",
        "City",
        TOP,
        GROUPS,
        &FieldOptions::new().hint("Pick one."),
    )
    .into_string();
    assert!(html.contains(r#"aria-invalid="true""#), "{html}");
    assert!(
        html.contains(r#"aria-describedby="city-hint city-error""#),
        "{html}"
    );
    assert!(html.contains("is &lt;bad&gt;"), "{html}");
    // The selection stays after a 422.
    assert!(html.contains(r#"value="lyon" selected"#), "{html}");
}

#[test]
fn a_single_select_marks_only_the_first_match() {
    // `lyon` is in two groups. HTML keeps the last `selected` option, so two
    // marks would show the wrong one.
    let groups: &[(&str, &[(&str, &str)])] = &[
        ("France", &[("lyon", "Lyon")]),
        ("Also", &[("lyon", "Lyon again")]),
    ];
    let html =
        grouped_select_input(&Changeset::new(trip()), "city", "City", &[], groups).into_string();
    assert_eq!(selected_count(&html), 1, "{html}");
    assert!(
        html.contains(r#"<option value="lyon" selected>Lyon</option>"#),
        "{html}"
    );

    let flat = select_input(
        &Changeset::new(trip()),
        "city",
        "City",
        &[("lyon", "A"), ("lyon", "B")],
    )
    .into_string();
    assert_eq!(selected_count(&flat), 1, "{flat}");
    assert!(flat.contains(r#"value="lyon" selected>A<"#), "{flat}");
}

// ── Multiple ─────────────────────────────────────────────────────────────────

#[test]
fn multiple_select_writes_multiple_and_marks_each_value() {
    let html = select_input_with(
        &Changeset::new(trip()),
        "tags",
        "Tags",
        TAGS,
        &FieldOptions::new().multiple(),
    )
    .into_string();
    assert!(
        html.contains(r#"<select id="tags" name="tags" multiple"#),
        "{html}"
    );
    assert!(html.contains(r#"value="rust" selected"#), "{html}");
    assert!(!html.contains(r#"value="web" selected"#), "{html}");
    assert!(html.contains(r#"value="db" selected"#), "{html}");
    assert_eq!(selected_count(&html), 2, "{html}");
}

#[test]
fn multiple_select_with_an_empty_list_marks_nothing() {
    let cs = Changeset::new(Trip {
        city: String::new(),
        tags: Vec::new(),
    });
    let html =
        select_input_with(&cs, "tags", "Tags", TAGS, &FieldOptions::new().multiple()).into_string();
    assert_eq!(selected_count(&html), 0, "{html}");
}

#[test]
fn multiple_select_on_a_scalar_field_marks_that_value() {
    let html = select_input_with(
        &Changeset::new(trip()),
        "city",
        "City",
        &[("paris", "Paris"), ("lyon", "Lyon")],
        &FieldOptions::new().multiple(),
    )
    .into_string();
    assert!(html.contains(r#"value="lyon" selected"#), "{html}");
    assert_eq!(selected_count(&html), 1, "{html}");
}

#[test]
fn required_multiple_select_needs_one_or_more_values() {
    // HTML `required` on a multiple select means "choose one or more".
    let html = select_input_with(
        &Changeset::new(trip()),
        "tags",
        "Tags",
        TAGS,
        &FieldOptions::new().multiple().required(),
    )
    .into_string();
    assert!(html.contains(" multiple"), "{html}");
    assert!(html.contains(r#" required aria-required="true""#), "{html}");
}

#[test]
fn grouped_multiple_select_marks_values_in_each_group() {
    let cs = Changeset::new(serde_json::json!({ "city": ["paris", "tokyo"] }));
    let html = grouped_select_input_with(
        &cs,
        "city",
        "Cities",
        &[],
        GROUPS,
        &FieldOptions::new().multiple(),
    )
    .into_string();
    assert!(html.contains(r#"value="paris" selected"#), "{html}");
    assert!(html.contains(r#"value="tokyo" selected"#), "{html}");
    assert_eq!(selected_count(&html), 2, "{html}");
}

#[test]
fn single_select_ignores_extra_values_of_a_list_field() {
    // No `multiple`: a list field shows no selection, as before.
    let html = select_input(&Changeset::new(trip()), "tags", "Tags", TAGS).into_string();
    assert!(!html.contains(" multiple"), "{html}");
    assert_eq!(selected_count(&html), 0, "{html}");
}

#[test]
fn a_posted_multiple_select_decodes_and_shows_again() {
    #[derive(Deserialize, Serialize)]
    struct Filter {
        #[serde(default)]
        tags: Vec<String>,
    }
    let filter: Filter = from_urlencoded(b"tags=rust&tags=web").expect("decode");
    assert_eq!(filter.tags, ["rust", "web"]);
    let html = select_input_with(
        &Changeset::new(filter),
        "tags",
        "Tags",
        TAGS,
        &FieldOptions::new().multiple(),
    )
    .into_string();
    assert!(html.contains(r#"value="rust" selected"#), "{html}");
    assert!(html.contains(r#"value="web" selected"#), "{html}");
    assert!(!html.contains(r#"value="db" selected"#), "{html}");

    // No option chosen posts no key.
    let none: Filter = from_urlencoded(b"").expect("decode");
    assert!(none.tags.is_empty());
}

#[test]
fn multiple_is_ignored_by_a_text_input() {
    let html = text_input_with(
        &Changeset::new(trip()),
        "city",
        "City",
        &FieldOptions::new().multiple().styled_select(),
    )
    .into_string();
    assert!(!html.contains(" multiple"), "{html}");
    assert!(!html.contains("autumn-select"), "{html}");
}

#[test]
fn field_options_report_the_new_flags() {
    let opts = FieldOptions::new().multiple().styled_select();
    assert!(opts.is_multiple());
    assert!(opts.is_styled_select());
    let none = FieldOptions::new();
    assert!(!none.is_multiple());
    assert!(!none.is_styled_select());
}

// ── Styled ───────────────────────────────────────────────────────────────────

#[test]
fn styled_select_adds_the_autumn_select_class() {
    let html = select_input_with(
        &Changeset::new(trip()),
        "city",
        "City",
        &[("lyon", "Lyon")],
        &FieldOptions::new().styled_select(),
    )
    .into_string();
    assert!(
        html.contains(r#"class="autumn-field__input autumn-select" aria-invalid="false""#),
        "{html}"
    );
}

#[test]
fn styled_select_keeps_the_invalid_class() {
    let mut errors = HashMap::new();
    errors.insert("city".to_owned(), vec!["bad".to_owned()]);
    let html = grouped_select_input_with(
        &Changeset::from_errors(trip(), errors),
        "city",
        "City",
        TOP,
        GROUPS,
        &FieldOptions::new().styled_select(),
    )
    .into_string();
    assert!(
        html.contains(r#"class="autumn-field__input autumn-field__input--invalid autumn-select""#),
        "{html}"
    );
}

#[test]
fn a_plain_select_is_unchanged() {
    let html = required_select_input(
        &Changeset::new(trip()),
        "city",
        "City",
        &[("", "None"), ("lyon", "Lyon")],
    )
    .into_string();
    assert_eq!(
        html,
        concat!(
            r#"<div id="city-field" class="autumn-field">"#,
            r#"<label for="city" class="autumn-field__label">City"#,
            r#"<span class="autumn-field__required" aria-hidden="true">*</span></label>"#,
            r#"<select id="city" name="city" required aria-required="true" "#,
            r#"class="autumn-field__input" aria-invalid="false">"#,
            r#"<option value="">None</option>"#,
            r#"<option value="lyon" selected>Lyon</option>"#,
            "</select></div>",
        )
    );
}

// ── form_for ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Visit {
    city: String,
    tags: Vec<String>,
}

impl FormModel for Visit {
    fn form_fields() -> Vec<FormField> {
        vec![
            FormField::new(
                "city",
                "City",
                FieldControl::GroupedSelect {
                    options: vec![(String::new(), "Choose".into())],
                    groups: vec![
                        (
                            "France".into(),
                            vec![
                                ("paris".into(), "Paris".into()),
                                ("lyon".into(), "Lyon".into()),
                            ],
                        ),
                        ("Japan".into(), vec![("tokyo".into(), "Tokyo".into())]),
                    ],
                },
                true,
            ),
            FormField::new(
                "tags",
                "Tags",
                FieldControl::Select {
                    options: TAGS
                        .iter()
                        .map(|(v, l)| ((*v).to_owned(), (*l).to_owned()))
                        .collect(),
                },
                true,
            ),
        ]
    }
}

fn visit() -> Changeset<Visit> {
    Changeset::new(Visit {
        city: "lyon".into(),
        tags: vec!["web".into(), "db".into()],
    })
}

#[test]
fn form_for_renders_a_grouped_select() {
    let html = form_for(&visit(), "/v", "post").render().into_string();
    assert!(html.contains(r#"<optgroup label="France">"#), "{html}");
    assert!(html.contains(r#"<optgroup label="Japan">"#), "{html}");
    assert!(html.contains(r#"value="lyon" selected"#), "{html}");
    assert!(
        html.contains(r#"<select id="city" name="city" required aria-required="true""#),
        "{html}"
    );
}

#[test]
fn form_for_multiple_select_marks_each_value_and_skips_model_required() {
    let html = form_for(&visit(), "/v", "post")
        .field_options("tags", FieldOptions::new().multiple().styled_select())
        .render()
        .into_string();
    let start = html.find(r#"<select id="tags""#).expect("tags select");
    let select = &html[start..start + html[start..].find("</select>").expect("end")];
    assert!(select.contains(" multiple"), "{select}");
    assert!(select.contains("autumn-select"), "{select}");
    assert!(select.contains(r#"value="web" selected"#), "{select}");
    assert!(select.contains(r#"value="db" selected"#), "{select}");
    // A `Vec` column can be empty. The model flag does not make it required.
    assert!(!select.contains(" required"), "{select}");
}

#[test]
fn form_for_multiple_select_keeps_an_explicit_required() {
    let html = form_for(&visit(), "/v", "post")
        .field_options("tags", FieldOptions::new().multiple().required())
        .render()
        .into_string();
    let start = html.find(r#"<select id="tags""#).expect("tags select");
    assert!(
        html[start..].contains(r#" required aria-required="true""#),
        "{html}"
    );
}

#[test]
fn form_for_grouped_multiple_select_skips_model_required() {
    let html = form_for(&visit(), "/v", "post")
        .field_options("city", FieldOptions::new().multiple())
        .render()
        .into_string();
    let start = html.find(r#"<select id="city""#).expect("city select");
    let select = &html[start..start + html[start..].find("</select>").expect("end")];
    assert!(select.contains(" multiple"), "{select}");
    assert!(!select.contains(" required"), "{select}");
    assert!(select.contains(r#"value="lyon" selected"#), "{select}");
}

/// A widget-pack select that writes its tag with `FieldContext::control`.
struct PackSelect;

impl autumn_web::form::FieldRenderer for PackSelect {
    fn name(&self) -> &'static str {
        "pack-select"
    }

    fn render(&self, field: &autumn_web::form::FieldContext<'_>) -> maud::Markup {
        let options = maud::html! {
            @for (value, label) in TAGS {
                option value=(value) selected[field.values().iter().any(|v| v == value)] {
                    (label)
                }
            }
        };
        field.control("select", &[], options)
    }
}

#[test]
fn a_custom_select_control_gets_multiple() {
    let cs = Changeset::new(trip());
    let html = autumn_web::form::custom_input_with(
        &cs,
        "tags",
        "Tags",
        &PackSelect,
        &FieldOptions::new().multiple(),
    )
    .into_string();
    assert!(
        html.contains(r#"<select id="tags" name="tags" multiple"#),
        "{html}"
    );
    assert!(html.contains(r#"value="db" selected"#), "{html}");

    let single =
        autumn_web::form::custom_input_with(&cs, "tags", "Tags", &PackSelect, &FieldOptions::new())
            .into_string();
    assert!(!single.contains(" multiple"), "{single}");
}

#[test]
fn form_for_file_input_takes_multiple() {
    #[derive(Serialize)]
    struct Upload {
        files: String,
    }
    impl FormModel for Upload {
        fn form_fields() -> Vec<FormField> {
            vec![FormField::new("files", "Files", FieldControl::File, false)]
        }
    }
    let html = form_for(
        &Changeset::new(Upload {
            files: String::new(),
        }),
        "/u",
        "post",
    )
    .field_options("files", FieldOptions::new().multiple())
    .render()
    .into_string();
    assert!(
        html.contains(r#"<input type="file" id="files" name="files" multiple"#),
        "{html}"
    );
    assert!(html.contains(r#"enctype="multipart/form-data""#), "{html}");
}

// ── Selection rule (see verification/select_marks.rs) ───────────────────────

/// The options a select marks, in document order.
fn marks(html: &str) -> Vec<bool> {
    html.split("<option")
        .skip(1)
        .map(|option| option[..option.find('>').expect("option end")].ends_with(" selected"))
        .collect()
}

proptest! {
    /// A single select marks the first match only. A multiple select marks
    /// each match. Groups share one rule.
    #[test]
    fn select_marks_follow_the_spec(
        top in prop::collection::vec(0u8..6, 0..4),
        groups in prop::collection::vec(prop::collection::vec(0u8..6, 0..4), 0..4),
        current in prop::collection::vec(0u8..8, 0..3),
        multiple in any::<bool>(),
    ) {
        let label = |v: &u8| v.to_string();
        let top_pairs: Vec<(String, String)> = top.iter().map(|v| (label(v), label(v))).collect();
        let group_pairs: Vec<(String, Vec<(String, String)>)> = groups
            .iter()
            .enumerate()
            .map(|(i, g)| (format!("G{i}"), g.iter().map(|v| (label(v), label(v))).collect()))
            .collect();
        let top_refs: Vec<(&str, &str)> =
            top_pairs.iter().map(|(v, l)| (v.as_str(), l.as_str())).collect();
        let group_items: Vec<Vec<(&str, &str)>> = group_pairs
            .iter()
            .map(|(_, g)| g.iter().map(|(v, l)| (v.as_str(), l.as_str())).collect())
            .collect();
        let group_refs: Vec<(&str, &[(&str, &str)])> = group_pairs
            .iter()
            .zip(&group_items)
            .map(|((name, _), items)| (name.as_str(), items.as_slice()))
            .collect();

        let current: Vec<String> = current.iter().map(label).collect();
        let cs = if multiple {
            Changeset::new(serde_json::json!({ "f": current }))
        } else {
            Changeset::new(serde_json::json!({ "f": current.first() }))
        };
        let shown: Vec<String> = if multiple {
            current
        } else {
            current.into_iter().take(1).collect()
        };
        let opts = if multiple { FieldOptions::new().multiple() } else { FieldOptions::new() };
        let html = grouped_select_input_with(&cs, "f", "F", &top_refs, &group_refs, &opts)
            .into_string();

        let values: Vec<String> = top
            .iter()
            .chain(groups.iter().flatten())
            .map(label)
            .collect();
        // The spec rule: option `i` is marked when it matches, and, for a
        // single select, when no option before it matches.
        let want: Vec<bool> = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                shown.contains(v)
                    && (multiple || !values[..i].iter().any(|w| shown.contains(w)))
            })
            .collect();
        prop_assert_eq!(marks(&html), want);
        if !multiple {
            prop_assert!(selected_count(&html) <= 1);
        }
    }
}

// ── Stylesheet ───────────────────────────────────────────────────────────────

/// The body of the first `@supports (appearance: base-select)` block.
fn base_select_block(css: &str) -> &str {
    let start = css
        .find("@supports (appearance: base-select) {")
        .expect("a base-select @supports block");
    let body = &css[start..];
    let mut depth = 0usize;
    for (at, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &body[..=at];
                }
            }
            _ => {}
        }
    }
    panic!("unclosed @supports block");
}

#[test]
fn stylesheet_puts_base_select_behind_a_feature_query() {
    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
    let block = base_select_block(css);
    // Every `appearance: base-select` is inside the feature query, so a
    // browser with no support keeps the native control.
    assert_eq!(
        css.matches("appearance: base-select;").count(),
        block.matches("appearance: base-select;").count(),
        "an `appearance: base-select` is outside @supports"
    );
    assert!(
        block.contains(".autumn-select:not([multiple]) {"),
        "{block}"
    );
    assert!(
        block.contains(".autumn-select:not([multiple])::picker(select) {"),
        "{block}"
    );
    // A browser drops a whole selector list when it does not know
    // `::picker(select)`. So no `::picker` selector is in a list.
    for rule in block.split('}') {
        let Some(open) = rule.rfind('{') else {
            continue;
        };
        let selector = &rule[..open];
        let new_syntax = ["::picker", "::checkmark", ":open"];
        if new_syntax.iter().any(|part| selector.contains(part)) {
            assert!(!selector.contains(','), "new syntax in a list: {selector}");
        }
    }
}

#[test]
fn stylesheet_picker_sets_its_own_colors_and_respects_reduced_motion() {
    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
    let block = base_select_block(css);
    let start = block
        .find(".autumn-select:not([multiple])::picker(select) {")
        .expect("picker rule");
    let rule = &block[start..start + block[start..].find('}').expect("rule end")];
    assert!(rule.contains("color: var(--text)"), "{rule}");
    assert!(rule.contains("background-color: var(--surface)"), "{rule}");
    // The icon turn is declared only for a user who did not ask for less
    // motion, so no specificity contest with a `reduce` rule can lose.
    let motion = "@media (prefers-reduced-motion: no-preference) {";
    let at = block.find(motion).expect("a no-preference block");
    let inner = &block[at..at + block[at..].find("}\n    }").expect("block end")];
    assert!(
        inner.contains(".autumn-select:not([multiple])::picker-icon {"),
        "{inner}"
    );
    assert_eq!(
        block.matches("transition").count(),
        inner.matches("transition").count(),
        "a select transition is outside the no-preference block"
    );
}
