//! The `swatch` widget pack: a small pack that uses each widget seam
//! (issue #3400).
//!
//! - [`SwatchPicker`] binds to a `Changeset<T>` and to `form_for` through
//!   `FieldRenderer`. It is a group of native radios, so it works with
//!   JavaScript off.
//! - [`swatch_toolbar`] uses the keyboard core (`data-autumn-roving`) and
//!   the `autumn:init` event. It has no arrow-key code of its own.
//! - `swatch.css` uses only the public tokens.

use autumn_web::assets::PluginAssets;
use autumn_web::form::{Changeset, FieldContext, FieldOptions, FieldRenderer, custom_input_with};
use autumn_web::stories::story;
use autumn_web::widget_pack::{WidgetPack, element};
use maud::{Markup, html};

/// The swatches: `(token name, label)`. The value is the token name.
pub const SWATCHES: [(&str, &str); 5] = [
    ("primary", "Primary"),
    ("success", "Success"),
    ("warning", "Warning"),
    ("danger", "Danger"),
    ("info", "Info"),
];

/// The pack CSS and JS.
pub static ASSETS: PluginAssets = PluginAssets::from_files(
    "swatch",
    &[
        ("swatch.css", include_bytes!("swatch.css")),
        ("swatch.js", include_bytes!("swatch.js")),
    ],
);

/// A radio group of color swatches, bound to one form field.
#[derive(Debug, Default, Clone, Copy)]
pub struct SwatchPicker;

impl FieldRenderer for SwatchPicker {
    fn name(&self) -> &'static str {
        "swatch"
    }

    fn grouped(&self) -> bool {
        true
    }

    fn render(&self, field: &FieldContext<'_>) -> Markup {
        html! {
            div class="autumn-swatch-picker" {
                @for (value, label) in SWATCHES {
                    @let checked: &[(&str, &str)] = if field.value() == Some(value) {
                        &[("checked", "")]
                    } else {
                        &[]
                    };
                    @let attrs: Vec<(&str, &str)> = [
                        ("type", "radio"),
                        ("class", "autumn-swatch-radio"),
                        ("value", value),
                    ]
                    .into_iter()
                    .chain(checked.iter().copied())
                    .collect();
                    label class="autumn-swatch-option" for=(format!("{}-{value}", field.id())) {
                        // `group_item` adds `id`, `name`, `required`,
                        // `aria-invalid` and the app passthrough.
                        (field.group_item("input", value, &attrs, html! {}))
                        span class={ "autumn-swatch-chip autumn-swatch-chip--" (value) }
                            aria-hidden="true" {}
                        (label)
                    }
                }
            }
        }
    }
}

/// Render a [`SwatchPicker`] bound to `changeset[field]`.
#[must_use]
pub fn swatch_picker<T: serde::Serialize>(
    changeset: &Changeset<T>,
    field: &str,
    label: &str,
    options: &FieldOptions<'_>,
) -> Markup {
    custom_input_with(changeset, field, label, &SwatchPicker, options)
}

/// A toolbar of toggle buttons, one per swatch.
///
/// The keyboard core moves focus with the arrow keys. `swatch.js` toggles
/// `aria-pressed`. The root gets the app `data-*` and `hx-*` passthrough
/// from `options`.
#[must_use]
pub fn swatch_toolbar(label: &str, options: &FieldOptions<'_>) -> Markup {
    element(
        "div",
        &[
            ("class", "autumn-swatch-toolbar"),
            ("role", "toolbar"),
            ("aria-label", label),
            ("data-autumn-roving", "button"),
            ("data-autumn-orientation", "horizontal"),
        ],
        options,
        html! {
            @for (value, name) in SWATCHES {
                button type="button" class="autumn-swatch-toggle" aria-pressed="false"
                    data-swatch=(value) {
                    span class={ "autumn-swatch-chip autumn-swatch-chip--" (value) }
                        aria-hidden="true" {}
                    (name)
                }
            }
        },
    )
}

#[derive(serde::Serialize)]
struct Theme {
    accent: String,
}

/// The pack: its stories and its assets.
#[must_use]
pub fn pack() -> WidgetPack {
    WidgetPack::new("swatch")
        .extend([
            story! {
                "Swatch",
                "Swatch picker",
                {
                    let changeset = Changeset::new(Theme { accent: "success".into() });
                    swatch_picker(
                        &changeset,
                        "accent",
                        "Accent",
                        &FieldOptions::new().hint("The color of links and buttons."),
                    )
                }
            },
            story! {
                "Swatch",
                "Swatch toolbar",
                {
                    swatch_toolbar(
                        "Highlight colors",
                        &FieldOptions::new().data("test-id", "highlight"),
                    )
                }
            },
        ])
        .assets(&ASSETS)
}
