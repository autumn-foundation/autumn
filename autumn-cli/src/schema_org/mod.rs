//! `autumn schema-org`: browse schema.org types and audit JSON-LD.
//!
//! - `list` prints the types the CLI knows.
//! - `show <Type>` prints a type's properties, what Google asks for, and the
//!   columns `autumn generate scaffold --schema-org <Type>` would make.
//! - `audit <URL|FILE|DIR>…` reads the JSON-LD blocks of pages and reports
//!   problems. See [`audit`].
//!
//! The vocabulary is in [`vocab`]. See `docs/guide/schema-org.md`.

pub mod audit;
pub mod vocab;

use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::generate::schema_org::{column_for, seed_field_tokens};
use vocab::{Level, SchemaType};

/// Output format of the `schema-org` commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Text for a person.
    Text,
    /// JSON for a script.
    Json,
}

/// Run `autumn schema-org list`.
pub fn run_list(format: Format) {
    print!("{}", render_list(format));
}

/// The output of `autumn schema-org list`.
#[must_use]
pub fn render_list(format: Format) -> String {
    match format {
        Format::Json => {
            let types: Vec<Value> = vocab::TYPES
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "parents": t.parents,
                        "description": t.description,
                        "enumeration": t.is_enumeration(),
                        "google_docs": t.google_docs,
                    })
                })
                .collect();
            format!("{}\n", json!({ "types": types }))
        }
        Format::Text => {
            let width = vocab::TYPES.iter().map(|t| t.name.len()).max().unwrap_or(0);
            let mut out =
                String::from("schema.org types this CLI knows. * marks a Google rich result.\n\n");
            for t in vocab::TYPES {
                let mark = if t.google_docs.is_some() { "*" } else { " " };
                let _ = writeln!(out, "{mark} {:width$}  {}", t.name, t.description);
            }
            out.push_str("\nRun `autumn schema-org show <Type>` for a type's properties.\n");
            out
        }
    }
}

/// Run `autumn schema-org show <Type>`. Exits 1 for an unknown type.
pub fn run_show(name: &str, all: bool, format: Format) {
    match render_show(name, all, format) {
        Ok(out) => print!("{out}"),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

/// The output of `autumn schema-org show <Type>`.
///
/// Text lists the required and recommended properties and the type's own
/// properties. `all` adds the inherited ones. JSON always holds all of them.
///
/// # Errors
///
/// A message for an unknown type, with a "did you mean" hint.
pub fn render_show(name: &str, all: bool, format: Format) -> Result<String, String> {
    let t = vocab::lookup(name).ok_or_else(|| {
        let hint = vocab::suggest_type(vocab::strip_schema_prefix(name))
            .map(|s| format!(" Did you mean `{s}`?"))
            .unwrap_or_default();
        format!("`{name}` is not a schema.org type this CLI knows.{hint}")
    })?;
    let lineage: Vec<&str> = {
        let mut chain = vec![t.name];
        let mut cur = t;
        while let Some(parent) = cur.parents.first().and_then(|p| vocab::find(p)) {
            chain.push(parent.name);
            cur = parent;
        }
        chain.reverse();
        chain
    };
    let seed = if t.is_enumeration() {
        Vec::new()
    } else {
        seed_field_tokens(t)
    };
    let props = vocab::properties(t);
    match format {
        Format::Json => {
            let properties: Vec<Value> = props
                .iter()
                .map(|r| {
                    json!({
                        "name": r.property.name,
                        "range": r.property.range,
                        "description": r.property.description,
                        "declared_on": r.declared_on,
                        "list": r.property.list,
                        "google": vocab::level(t, r.property.name).as_str(),
                        "column": column_for(r.property.name),
                    })
                })
                .collect();
            Ok(format!(
                "{}\n",
                json!({
                    "name": t.name,
                    "lineage": lineage,
                    "description": t.description,
                    "google_docs": t.google_docs,
                    "required": t.required,
                    "required_any": t.required_any,
                    "recommended": t.recommended,
                    "members": t.members,
                    "properties": properties,
                    "scaffold_fields": seed,
                })
            ))
        }
        Format::Text => Ok(render_show_text(t, &lineage, &seed, all)),
    }
}

fn render_show_text(
    t: &'static SchemaType,
    lineage: &[&str],
    seed: &[String],
    all: bool,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}  ({})", t.name, lineage.join(" > "));
    let _ = writeln!(out, "{}", t.description);
    if let Some(page) = t.google_docs {
        let _ = writeln!(out, "Google rich result: {page}");
    }
    let _ = writeln!(out, "https://schema.org/{}", t.name);
    if t.is_enumeration() {
        let _ = writeln!(out, "\nMembers:");
        for m in t.members {
            let _ = writeln!(out, "  {m}  (https://schema.org/{m})");
        }
        return out;
    }
    for group in t.required_any {
        let _ = writeln!(out, "Google requires one of: {}", group.join(", "));
    }
    let shown: Vec<_> = vocab::properties(t)
        .into_iter()
        .filter(|r| {
            all || r.declared_on == t.name || vocab::level(t, r.property.name) != Level::Optional
        })
        .collect();
    let mut rows: Vec<[String; 4]> = shown
        .iter()
        .map(|r| {
            [
                r.property.name.to_owned(),
                r.property.range.join(" | ") + if r.property.list { " (list)" } else { "" },
                vocab::level(t, r.property.name).as_str().to_owned(),
                if r.declared_on == t.name {
                    String::new()
                } else {
                    format!("from {}", r.declared_on)
                },
            ]
        })
        .collect();
    rows.sort_by_key(|row| match row[2].as_str() {
        "required" => 0,
        "recommended" => 1,
        _ => 2,
    });
    let header = [
        "Property".to_owned(),
        "Expected type".to_owned(),
        "Google".to_owned(),
        String::new(),
    ];
    let widths: Vec<usize> = (0..4)
        .map(|i| {
            rows.iter()
                .chain(std::iter::once(&header))
                .map(|r| r[i].len())
                .max()
                .unwrap_or(0)
        })
        .collect();
    out.push('\n');
    for row in std::iter::once(&header).chain(rows.iter()) {
        let line = format!(
            "  {:w0$}  {:w1$}  {:w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        );
        let _ = writeln!(out, "{}", line.trim_end());
    }
    if !all {
        let hidden = vocab::properties(t).len() - shown.len();
        if hidden > 0 {
            let _ = writeln!(out, "\n  {hidden} more inherited properties: pass --all.");
        }
    }
    if !seed.is_empty() {
        let _ = writeln!(
            out,
            "\n`autumn generate scaffold {} --schema-org {}` with no fields makes:",
            t.name, t.name
        );
        for token in seed {
            let _ = writeln!(out, "  {token}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_text_marks_rich_results() {
        let out = render_list(Format::Text);
        assert!(out.contains("* Recipe"), "{out}");
        assert!(out.contains("  Thing"), "{out}");
    }

    #[test]
    fn list_json_has_every_type() {
        let out: Value = serde_json::from_str(&render_list(Format::Json)).expect("json");
        let types = out["types"].as_array().expect("types");
        assert_eq!(types.len(), vocab::TYPES.len());
        assert!(
            types
                .iter()
                .any(|t| t["name"] == "Product" && t["google_docs"].is_string())
        );
    }

    #[test]
    fn show_text_lists_required_first_and_the_seed() {
        let out = render_show("recipe", false, Format::Text).expect("show");
        assert!(
            out.starts_with("Recipe  (Thing > CreativeWork > HowTo > Recipe)"),
            "{out}"
        );
        let name_at = out.find("  name ").expect("name row");
        let ingredient_at = out.find("  recipeIngredient").expect("ingredient row");
        assert!(name_at < ingredient_at, "{out}");
        assert!(out.contains("Text (list)"), "{out}");
        assert!(
            out.contains("more inherited properties: pass --all"),
            "{out}"
        );
        assert!(out.contains("  name:String\n"), "{out}");
    }

    #[test]
    fn show_all_has_no_hidden_note() {
        let out = render_show("Recipe", true, Format::Text).expect("show");
        assert!(!out.contains("pass --all"), "{out}");
        assert!(out.contains("isAccessibleForFree"), "{out}");
    }

    #[test]
    fn show_lists_required_any_groups() {
        let out = render_show("Product", false, Format::Text).expect("show");
        assert!(
            out.contains("Google requires one of: offers, review, aggregateRating"),
            "{out}"
        );
    }

    #[test]
    fn show_enumeration_lists_members() {
        let out = render_show("ItemAvailability", false, Format::Text).expect("show");
        assert!(
            out.contains("  InStock  (https://schema.org/InStock)"),
            "{out}"
        );
        assert!(!out.contains("scaffold"), "{out}");
    }

    #[test]
    fn show_json_holds_all_properties() {
        let out: Value =
            serde_json::from_str(&render_show("Event", false, Format::Json).expect("show"))
                .expect("json");
        assert_eq!(out["name"], "Event");
        let props = out["properties"].as_array().expect("properties");
        let start = props
            .iter()
            .find(|p| p["name"] == "startDate")
            .expect("startDate");
        assert_eq!(start["google"], "required");
        assert_eq!(start["column"], "start_date");
        assert!(
            props
                .iter()
                .any(|p| p["name"] == "sameAs" && p["declared_on"] == "Thing")
        );
        assert!(
            out["scaffold_fields"]
                .as_array()
                .is_some_and(|f| !f.is_empty())
        );
    }

    #[test]
    fn show_unknown_type_suggests() {
        let err = render_show("Prodcut", false, Format::Text).expect_err("unknown");
        assert!(err.contains("Did you mean `Product`"), "{err}");
    }
}
