//! `generate scaffold --schema-org <Type>`: schema.org fields and JSON-LD.
//!
//! Two jobs, both pure:
//!
//! - [`seed_field_tokens`] turns a type into field DSL tokens, for a
//!   scaffold given a type and no fields. The required and recommended
//!   properties become columns.
//! - [`bind`] matches the scaffold's columns to the type's properties.
//!   [`render_json_ld_fn`] then writes the `json_ld` function the show view
//!   renders.
//!
//! A column binds to a property by name: `date_published` is
//! `datePublished`. A column `<property>_<sub>` binds to a property of a
//! nested node: `offers_price` is `offers.price`. A few common names bind by
//! alias (`title` is `headline` on an article). `--schema-org-map
//! column=property` binds any other column.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::GenerateError;
use super::dsl::{Field, FieldKind, is_rust_keyword};
use crate::schema_org::vocab::{self, Property, SchemaType};

/// How a column's value goes into the JSON-LD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// The value as it is.
    Scalar,
    /// One item per line, as an array.
    Lines,
    /// A node known by its name: `{"@type": T, "name": value}`.
    Named(&'static str),
}

/// One column bound to a property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The column (model field) name.
    pub column: String,
    /// The property, then the nested node's type and property, if any.
    pub property: &'static str,
    /// `Some((type, property))` for a property of a nested node.
    pub nested: Option<(&'static str, &'static str)>,
    /// How the value is written.
    pub shape: Shape,
    /// The column is `Option<…>`.
    pub nullable: bool,
    /// The column's Rust type is `Copy`, so it is passed by value.
    pub copy: bool,
}

/// A column the JSON-LD can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The field name.
    pub name: String,
    /// `Option<…>`.
    pub nullable: bool,
    /// A `String` in Rust, so it can be split into lines.
    pub string_like: bool,
    /// The Rust type is `Copy`: a number, a bool, a date or time, a UUID, a
    /// decimal or a generated enum.
    pub copy: bool,
}

impl Column {
    /// The columns of `fields` the JSON-LD may read, then `created_at`, which
    /// every model has. A reference id, a file, bytes, a translated or an
    /// encrypted value, and a server-managed counter are left out: none of
    /// them is a value to publish as is.
    #[must_use]
    pub fn from_fields(fields: &[Field]) -> Vec<Self> {
        let mut out: Vec<Self> = fields
            .iter()
            .filter(|f| {
                !matches!(
                    f.kind,
                    FieldKind::References
                        | FieldKind::Attachment
                        | FieldKind::Bytea
                        | FieldKind::Position
                        | FieldKind::Commentable
                ) && !f.is_encrypted()
                    && !f.is_translatable()
            })
            .map(|f| {
                let string_like = matches!(
                    f.kind,
                    FieldKind::String | FieldKind::Text | FieldKind::RichText | FieldKind::Slug
                );
                Self {
                    name: f.name.clone(),
                    nullable: f.nullable,
                    string_like,
                    copy: !string_like && !matches!(f.kind, FieldKind::Json),
                }
            })
            .collect();
        out.push(Self {
            name: "created_at".to_owned(),
            nullable: false,
            string_like: false,
            copy: true,
        });
        out
    }
}

/// Resolve `--schema-org <Type>`.
///
/// # Errors
///
/// [`GenerateError::Config`] for an unknown type or an enumeration, with a
/// "did you mean" hint when one is close.
pub fn resolve_type(name: &str) -> Result<&'static SchemaType, GenerateError> {
    let Some(t) = vocab::lookup(name) else {
        let hint = vocab::suggest_type(vocab::strip_schema_prefix(name))
            .map(|s| format!(" Did you mean `{s}`?"))
            .unwrap_or_default();
        return Err(GenerateError::Config(format!(
            "`--schema-org {name}`: not a schema.org type this CLI knows.{hint} \
             Run `autumn schema-org list` to see them."
        )));
    };
    if t.is_enumeration() {
        return Err(GenerateError::Config(format!(
            "`--schema-org {}`: an enumeration is a set of values, not a record. Use it as \
             the value of a property instead.",
            t.name
        )));
    }
    Ok(t)
}

/// `datePublished` → `date_published`, `taxID` → `tax_id`,
/// `thumbnailUrl` → `thumbnail_url`.
#[must_use]
pub fn camel_to_snake(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(char::is_ascii_lowercase);
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_lower)
            {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// The column name for `property`: its snake case, with `_value` after a
/// Rust keyword (`abstract`, `yield`).
#[must_use]
pub fn column_for(property: &str) -> String {
    let snake = camel_to_snake(property);
    if is_rust_keyword(&snake) {
        format!("{snake}_value")
    } else {
        snake
    }
}

/// The column name for `sub` of the nested node at `property`. The parent
/// name is not repeated: `address.addressLocality` is `address_locality`.
#[must_use]
pub fn nested_column_for(property: &str, sub: &str) -> String {
    let parent = camel_to_snake(property);
    let child = camel_to_snake(sub);
    if child.starts_with(&format!("{parent}_")) {
        child
    } else {
        format!("{parent}_{child}")
    }
}

/// What a property's value is, in the order its range prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// A primitive data type (`Text`, `Date`).
    Primitive(&'static str),
    /// A member of an enumeration, stored as text.
    Enumeration,
    /// A node known by its name.
    Named(&'static str),
    /// A node with properties of its own.
    Nested(&'static SchemaType),
}

/// The first entry of the range a single column can hold. With `nested`
/// (seeding), a node type that is not known by name, or any node type of a
/// `seed_nested` property, is a nested node instead.
fn pick(property: &Property, nested: bool) -> Option<Pick> {
    property.range.iter().find_map(|r| {
        if vocab::is_primitive(r) {
            return Some(Pick::Primitive(r));
        }
        let t = vocab::find(r)?;
        if t.is_enumeration() {
            Some(Pick::Enumeration)
        } else if nested && (property.seed_nested || !t.nameable) {
            Some(Pick::Nested(t))
        } else if t.nameable {
            Some(Pick::Named(t.name))
        } else {
            None
        }
    })
}

/// The shape for a column holding `property`, or `None` when one column
/// cannot hold it.
fn direct_shape(property: &Property, column: &Column) -> Option<Shape> {
    match pick(property, false)? {
        Pick::Primitive(_) | Pick::Enumeration if property.list && column.string_like => {
            Some(Shape::Lines)
        }
        Pick::Primitive(_) | Pick::Enumeration => Some(Shape::Scalar),
        Pick::Named(t) => Some(Shape::Named(t)),
        Pick::Nested(_) => None,
    }
}

/// The node type of `property` whose own properties can be columns.
fn nested_type(property: &Property) -> Option<&'static SchemaType> {
    property.range.iter().find_map(|r| {
        let t = vocab::find(r)?;
        (!t.is_enumeration() && !vocab::properties(t).is_empty()).then_some(t)
    })
}

/// A column name and the property it binds: a property of the type, and for
/// a nested node, its type and property too.
type Candidate = (
    String,
    &'static Property,
    Option<(&'static SchemaType, &'static Property)>,
);

/// Each column name the type can bind, with its target, in property order.
fn candidates(t: &'static SchemaType) -> Vec<Candidate> {
    let props = vocab::properties(t);
    let mut out = Vec::new();
    for r in &props {
        out.push((column_for(r.property.name), r.property, None));
    }
    for r in &props {
        let Some(inner) = nested_type(r.property) else {
            continue;
        };
        for sub in vocab::properties(inner) {
            out.push((
                nested_column_for(r.property.name, sub.property.name),
                r.property,
                Some((inner, sub.property)),
            ));
        }
    }
    out
}

/// Aliases: a column name and the properties it stands for, tried in order.
const ALIASES: &[(&str, &[&str])] = &[
    ("title", &["headline", "name"]),
    (
        "body",
        &["articleBody", "reviewBody", "text", "description"],
    ),
    (
        "content",
        &["articleBody", "reviewBody", "text", "description"],
    ),
    ("summary", &["description", "abstract"]),
    ("excerpt", &["description", "abstract"]),
    (
        "published_at",
        &["datePublished", "uploadDate", "datePosted"],
    ),
    ("updated_at", &["dateModified"]),
    ("modified_at", &["dateModified"]),
    ("created_at", &["dateCreated"]),
    ("starts_at", &["startDate"]),
    ("ends_at", &["endDate"]),
    ("image_url", &["image"]),
    ("photo", &["image"]),
    ("cover", &["image"]),
    ("thumbnail", &["thumbnailUrl", "image"]),
    ("website", &["url"]),
    ("homepage", &["url"]),
    ("phone", &["telephone"]),
    ("venue", &["location"]),
    ("author_name", &["author"]),
    ("tags", &["keywords"]),
    ("rating", &["ratingValue", "reviewRating"]),
];

/// Bind `columns` to the properties of `t`.
///
/// `explicit` holds `--schema-org-map column=property` pairs. A property
/// there may be `offers.price`, a property of a nested node. An explicit
/// pair wins. Then a column binds by its exact name, then by alias. Each
/// property binds once, to the first column that claims it.
///
/// # Errors
///
/// [`GenerateError::Config`] when an explicit pair names a column the
/// scaffold does not have, or a property the type does not have.
pub fn bind(
    t: &'static SchemaType,
    columns: &[Column],
    explicit: &[String],
) -> Result<Vec<Binding>, GenerateError> {
    let explicit = parse_explicit(t, columns, explicit)?;
    let candidates = candidates(t);
    let mut out: Vec<Binding> = Vec::new();
    // Explicit pairs bind first, so a column bound by name cannot take a
    // property an explicit pair asked for.
    for column in columns {
        if let Some(&target) = explicit.get(column.name.as_str())
            && let Some(binding) = make_binding(column, target)
        {
            out.push(binding);
        }
    }
    for column in columns
        .iter()
        .filter(|c| !explicit.contains_key(c.name.as_str()))
    {
        let target = candidates
            .iter()
            .find(|(name, prop, sub)| {
                *name == column.name && !is_taken(&out, prop.name, sub.map(|(_, s)| s.name))
            })
            .map(|(_, prop, sub)| (*prop, *sub))
            .or_else(|| {
                let (_, targets) = ALIASES.iter().find(|(alias, _)| *alias == column.name)?;
                targets.iter().find_map(|name| {
                    let r = vocab::property(t, name)?;
                    (!is_taken(&out, r.property.name, None)).then_some((r.property, None))
                })
            });
        if let Some(binding) = target.and_then(|target| make_binding(column, target)) {
            out.push(binding);
        }
    }
    // Column order, so the output follows the model.
    out.sort_by_key(|b| columns.iter().position(|c| c.name == b.column));
    Ok(out)
}

/// Whether a binding already holds `property` (with `sub`, a property of
/// its nested node). A direct binding and a nested one on the same property
/// conflict.
fn is_taken(out: &[Binding], property: &str, sub: Option<&str>) -> bool {
    out.iter().any(|b| {
        b.property == property
            && match (b.nested, sub) {
                (Some((_, held)), Some(sub)) => held == sub,
                _ => true,
            }
    })
}

fn make_binding(column: &Column, (property, sub): Target) -> Option<Binding> {
    match sub {
        None => direct_shape(property, column).map(|shape| Binding {
            column: column.name.clone(),
            property: property.name,
            nested: None,
            shape,
            nullable: column.nullable,
            copy: column.copy,
        }),
        Some((inner, sub)) => direct_shape(sub, column).map(|shape| Binding {
            column: column.name.clone(),
            property: property.name,
            nested: Some((inner.name, sub.name)),
            shape,
            nullable: column.nullable,
            copy: column.copy,
        }),
    }
}

type Target = (
    &'static Property,
    Option<(&'static SchemaType, &'static Property)>,
);

fn parse_explicit<'c>(
    t: &'static SchemaType,
    columns: &'c [Column],
    explicit: &[String],
) -> Result<BTreeMap<&'c str, Target>, GenerateError> {
    let mut out = BTreeMap::new();
    for pair in explicit {
        let Some((column, path)) = pair.split_once('=') else {
            return Err(GenerateError::Config(format!(
                "`--schema-org-map {pair}`: expected `column=property`, like `title=headline` \
                 or `cost=offers.price`."
            )));
        };
        let (column, path) = (column.trim(), path.trim());
        let Some(col) = columns.iter().find(|c| c.name == column) else {
            return Err(GenerateError::Config(format!(
                "`--schema-org-map {pair}`: the scaffold has no column `{column}` the JSON-LD \
                 can read."
            )));
        };
        let unknown = |t: &'static SchemaType, name: &str| {
            let hint = vocab::suggest_property(t, name)
                .map(|s| format!(" Did you mean `{s}`?"))
                .unwrap_or_default();
            GenerateError::Config(format!(
                "`--schema-org-map {pair}`: `{}` has no property `{name}`.{hint}",
                t.name
            ))
        };
        let target = match path.split_once('.') {
            None => {
                let r = vocab::property(t, path).ok_or_else(|| unknown(t, path))?;
                if direct_shape(r.property, col).is_none() {
                    return Err(GenerateError::Config(format!(
                        "`--schema-org-map {pair}`: `{path}` holds a `{}` node, which one \
                         column cannot hold. Map its parts instead, like `{path}.<property>`.",
                        r.property.range.join("` or `")
                    )));
                }
                (r.property, None)
            }
            Some((outer, inner)) => {
                let r = vocab::property(t, outer).ok_or_else(|| unknown(t, outer))?;
                let nested = nested_type(r.property).ok_or_else(|| {
                    GenerateError::Config(format!(
                        "`--schema-org-map {pair}`: `{outer}` is not a node with properties."
                    ))
                })?;
                let sub = vocab::property(nested, inner).ok_or_else(|| unknown(nested, inner))?;
                if direct_shape(sub.property, col).is_none() {
                    return Err(GenerateError::Config(format!(
                        "`--schema-org-map {pair}`: `{inner}` is itself a node; one level of \
                         nesting is supported."
                    )));
                }
                (r.property, Some((nested, sub.property)))
            }
        };
        // Two pairs on one property would write it twice, and the second
        // value would replace the first. A direct pair and a nested pair on
        // the same property conflict too.
        let (property, sub) = target;
        if let Some((other, _)) = out.iter().find(|(_, (p, s)): &(&&str, &Target)| {
            p.name == property.name
                && match (s, sub) {
                    (Some((_, a)), Some((_, b))) => a.name == b.name,
                    _ => true,
                }
        }) {
            return Err(GenerateError::Config(format!(
                "`--schema-org-map {pair}`: column `{other}` is already mapped to \
                 `{path}`. Map each property once."
            )));
        }
        out.insert(col.name.as_str(), target);
    }
    Ok(out)
}

/// The DSL type for a column holding `property` (`String`, `Timestamp`).
fn dsl_type(property: &Property, pick: Pick) -> &'static str {
    match pick {
        Pick::Primitive(p) => match p {
            _ if property.list => "Text",
            "Text" if property.long => "Text",
            "URL" => "String{url}",
            "Date" => "Date",
            "DateTime" => "Timestamp",
            "Time" => "time",
            "Boolean" => "bool",
            "Integer" => "i32",
            "Number" if matches!(property.name, "price" | "value" | "minValue" | "maxValue") => {
                "money"
            }
            "Number" if property.name.ends_with("Year") => "i32",
            "Number" => "f64",
            _ => "String",
        },
        Pick::Enumeration | Pick::Named(_) | Pick::Nested(_) => "String",
    }
}

/// `name:Type`, or `name:Option<Type>` when not `required`. A `{…}`
/// modifier stays outside the `Option<…>`.
fn token(column: &str, ty: &str, required: bool) -> String {
    if required {
        format!("{column}:{ty}")
    } else if let Some((base, modifier)) = ty.split_once('{') {
        format!("{column}:Option<{base}>{{{modifier}")
    } else {
        format!("{column}:Option<{ty}>")
    }
}

/// The property names of `t` to seed, each with whether it is required. The
/// first member of each "one of" group counts as required.
fn seeded_properties(t: &SchemaType) -> Vec<(&'static str, bool)> {
    let mut out: Vec<(&'static str, bool)> = Vec::new();
    let required = t
        .required
        .iter()
        .copied()
        .chain(t.required_any.iter().filter_map(|g| g.first().copied()));
    for name in required {
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, true));
        }
    }
    for &name in t.recommended {
        if !out.iter().any(|(n, _)| *n == name) {
            out.push((name, false));
        }
    }
    out
}

/// Field DSL tokens for a scaffold of `t` given no fields: one column per
/// required or recommended property, and for a property that holds a node,
/// one column per required (else recommended) property of that node.
/// Required properties are `NOT NULL`. The rest are `Option<…>`.
#[must_use]
pub fn seed_field_tokens(t: &'static SchemaType) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut push = |tokens: &mut Vec<String>, column: String, tok: String| {
        if column != "created_at" && !seen.contains(&column) {
            seen.push(column);
            tokens.push(tok);
        }
    };
    for (name, required) in seeded_properties(t) {
        let Some(r) = vocab::property(t, name) else {
            continue;
        };
        match pick(r.property, true) {
            Some(Pick::Nested(inner)) => {
                let subs = seeded_properties(inner);
                let required_subs: Vec<_> = subs.iter().filter(|(_, req)| *req).copied().collect();
                let subs = if required_subs.is_empty() {
                    subs
                } else {
                    required_subs
                };
                for (sub_name, sub_required) in subs {
                    let Some(sub) = vocab::property(inner, sub_name) else {
                        continue;
                    };
                    let Some(sub_pick) = pick(sub.property, false) else {
                        continue;
                    };
                    let column = nested_column_for(name, sub_name);
                    let ty = dsl_type(sub.property, sub_pick);
                    let tok = token(&column, ty, required && sub_required);
                    push(&mut tokens, column, tok);
                }
            }
            Some(p) => {
                let column = column_for(name);
                let tok = token(&column, dsl_type(r.property, p), required);
                push(&mut tokens, column, tok);
            }
            None => {}
        }
    }
    tokens
}

/// The value expression for `binding` on `row`.
fn value_expr(binding: &Binding) -> (String, bool) {
    let col = &binding.column;
    // A `Copy` value is passed as it is; a borrow of one is needless.
    let by = if binding.copy { "" } else { "&" };
    let expr = match (binding.shape, binding.nullable) {
        (Shape::Scalar, false) => format!("{by}row.{col}"),
        (Shape::Scalar, true) => format!("row.{col}.as_ref()"),
        (Shape::Lines, false) => format!("json_ld_lines(&row.{col})"),
        (Shape::Lines, true) => format!("row.{col}.as_deref().map(json_ld_lines)"),
        (Shape::Named(t), false) => {
            format!("JsonLd::node(\"{t}\").set(\"name\", {by}row.{col})")
        }
        (Shape::Named(t), true) => {
            format!("row.{col}.as_ref().map(|name| JsonLd::node(\"{t}\").set(\"name\", name))")
        }
    };
    (expr, binding.nullable)
}

fn setter(property: &str, binding: &Binding) -> String {
    let (expr, optional) = value_expr(binding);
    if optional {
        format!(".set_opt(\"{property}\", {expr})")
    } else {
        format!(".set(\"{property}\", {expr})")
    }
}

/// The `json_ld` function for the show view, or `None` when no column is
/// bound. It needs no database and no request: the row is enough.
#[must_use]
pub fn render_json_ld_fn(
    t: &SchemaType,
    bindings: &[Binding],
    pascal_name: &str,
    snake_name: &str,
) -> Option<String> {
    if bindings.is_empty() {
        return None;
    }
    let mut nodes: Vec<(&'static str, &'static str, Vec<&Binding>)> = Vec::new();
    for b in bindings {
        if let Some((inner, _)) = b.nested {
            if let Some(entry) = nodes.iter_mut().find(|(p, _, _)| *p == b.property) {
                entry.2.push(b);
            } else {
                nodes.push((b.property, inner, vec![b]));
            }
        }
    }
    let mut body = String::new();
    for (property, inner, parts) in &nodes {
        let var = camel_to_snake(property);
        let _ = write!(body, "    let {var} = JsonLd::node(\"{inner}\")");
        for b in parts {
            let (_, sub) = b.nested.unwrap_or(("", ""));
            let _ = write!(body, "\n        {}", setter(sub, b));
        }
        body.push_str(";\n");
    }
    let _ = write!(body, "    JsonLd::new(\"{}\")", t.name);
    let mut written_nodes: Vec<&str> = Vec::new();
    for b in bindings {
        match b.nested {
            None => {
                let _ = write!(body, "\n        {}", setter(b.property, b));
            }
            Some(_) if !written_nodes.contains(&b.property) => {
                written_nodes.push(b.property);
                let var = camel_to_snake(b.property);
                let _ = write!(
                    body,
                    "\n        .set_opt(\"{}\", Some({var}).filter(JsonLd::has_properties))",
                    b.property
                );
            }
            Some(_) => {}
        }
    }
    body.push('\n');
    let lines_helper = if bindings.iter().any(|b| b.shape == Shape::Lines) {
        "\n/// The non-blank lines of `text`, trimmed: one JSON-LD list item each.\n\
         fn json_ld_lines(text: &str) -> Vec<&str> {\n    \
         text.lines().map(str::trim).filter(|line| !line.is_empty()).collect()\n}\n"
    } else {
        ""
    };
    Some(format!(
        "\n/// The schema.org `{ty}` JSON-LD for one {snake_name}'s show page. Generated by\n\
         /// `autumn generate scaffold --schema-org {ty}`; edit it freely. Check the page with\n\
         /// `autumn schema-org audit`.\n\
         fn json_ld(row: &{pascal_name}) -> autumn_web::json_ld::JsonLd {{\n    \
         use autumn_web::json_ld::JsonLd;\n{body}}}\n{lines_helper}",
        ty = t.name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::dsl::parse_fields;

    fn ty(name: &str) -> &'static SchemaType {
        vocab::find(name).expect(name)
    }

    fn columns(tokens: &[&str]) -> Vec<Column> {
        let tokens: Vec<String> = tokens.iter().map(|s| (*s).to_owned()).collect();
        Column::from_fields(&parse_fields(&tokens).expect("fields parse"))
    }

    #[test]
    fn camel_to_snake_handles_acronyms() {
        assert_eq!(camel_to_snake("datePublished"), "date_published");
        assert_eq!(camel_to_snake("taxID"), "tax_id");
        assert_eq!(camel_to_snake("thumbnailUrl"), "thumbnail_url");
        assert_eq!(camel_to_snake("name"), "name");
        assert_eq!(camel_to_snake("isbn"), "isbn");
    }

    #[test]
    fn keyword_properties_get_a_suffix() {
        assert_eq!(column_for("abstract"), "abstract_value");
        assert_eq!(column_for("yield"), "yield_value");
        assert_eq!(column_for("recipeYield"), "recipe_yield");
    }

    #[test]
    fn nested_columns_do_not_repeat_the_parent() {
        assert_eq!(
            nested_column_for("address", "addressLocality"),
            "address_locality"
        );
        assert_eq!(
            nested_column_for("address", "streetAddress"),
            "address_street_address"
        );
        assert_eq!(nested_column_for("offers", "price"), "offers_price");
    }

    #[test]
    fn resolve_type_rejects_unknown_and_enumerations() {
        assert_eq!(resolve_type("recipe").expect("recipe").name, "Recipe");
        let err = resolve_type("Recipie").expect_err("typo").to_string();
        assert!(err.contains("Did you mean `Recipe`"), "{err}");
        let err = resolve_type("ItemAvailability")
            .expect_err("enum")
            .to_string();
        assert!(err.contains("enumeration"), "{err}");
    }

    #[test]
    fn recipe_seed_has_required_columns_first_and_parses() {
        let tokens = seed_field_tokens(ty("Recipe"));
        assert_eq!(tokens[0], "name:String");
        assert_eq!(tokens[1], "image:String{url}");
        assert!(
            tokens.contains(&"author:Option<String>".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"date_published:Option<Timestamp>".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"recipe_ingredient:Option<Text>".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"cook_time:Option<String>".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"nutrition_calories:Option<String>".to_owned()),
            "{tokens:?}"
        );
        let fields = parse_fields(&tokens).expect("seeded tokens parse");
        assert_eq!(fields.len(), tokens.len());
    }

    #[test]
    fn product_seed_flattens_the_offer() {
        let tokens = seed_field_tokens(ty("Product"));
        assert_eq!(tokens[0], "name:String");
        assert!(
            tokens.contains(&"offers_price:money".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"offers_price_currency:String".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"brand:Option<String>".to_owned()),
            "{tokens:?}"
        );
        parse_fields(&tokens).expect("seeded tokens parse");
    }

    #[test]
    fn event_seed_flattens_the_place() {
        let tokens = seed_field_tokens(ty("Event"));
        assert!(
            tokens.contains(&"start_date:Timestamp".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"location_name:Option<String>".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"location_address:Option<String>".to_owned()),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&"event_status:Option<String>".to_owned()),
            "{tokens:?}"
        );
        parse_fields(&tokens).expect("seeded tokens parse");
    }

    #[test]
    fn every_type_seeds_tokens_that_parse() {
        for t in vocab::TYPES.iter().filter(|t| !t.is_enumeration()) {
            let tokens = seed_field_tokens(t);
            parse_fields(&tokens).unwrap_or_else(|e| panic!("{}: {e}: {tokens:?}", t.name));
        }
    }

    #[test]
    fn seeded_columns_all_bind_back() {
        for t in vocab::TYPES.iter().filter(|t| !t.is_enumeration()) {
            let tokens = seed_field_tokens(t);
            let fields = parse_fields(&tokens).expect("parse");
            let bindings = bind(t, &Column::from_fields(&fields), &[]).expect("bind");
            for f in &fields {
                assert!(
                    bindings.iter().any(|b| b.column == f.name),
                    "{}: seeded column {} binds to nothing",
                    t.name,
                    f.name
                );
            }
        }
    }

    #[test]
    fn binds_by_name_alias_and_nesting() {
        let cols = columns(&[
            "title:String",
            "body:Text",
            "author:Option<String>",
            "date_published:Option<Timestamp>",
            "word_count:i32",
            "internal_notes:String",
        ]);
        let bindings = bind(ty("BlogPosting"), &cols, &[]).expect("bind");
        let find = |c: &str| bindings.iter().find(|b| b.column == c);
        assert_eq!(find("title").map(|b| b.property), Some("headline"));
        assert_eq!(find("body").map(|b| b.property), Some("articleBody"));
        assert_eq!(
            find("author").map(|b| b.shape),
            Some(Shape::Named("Person"))
        );
        assert_eq!(find("date_published").map(|b| b.nullable), Some(true));
        assert_eq!(find("word_count").map(|b| b.property), Some("wordCount"));
        assert_eq!(find("created_at").map(|b| b.property), Some("dateCreated"));
        assert_eq!(find("internal_notes"), None);
    }

    #[test]
    fn a_property_binds_once() {
        let cols = columns(&["headline:String", "title:String"]);
        let bindings = bind(ty("Article"), &cols, &[]).expect("bind");
        assert_eq!(
            bindings.iter().filter(|b| b.property == "headline").count(),
            1
        );
        // `title` falls back to `name`.
        assert_eq!(
            bindings
                .iter()
                .find(|b| b.column == "title")
                .map(|b| b.property),
            Some("name")
        );
    }

    #[test]
    fn list_properties_bind_as_lines_on_text_columns() {
        let cols = columns(&["recipe_ingredient:Text", "same_as:Option<Text>"]);
        let bindings = bind(ty("Recipe"), &cols, &[]).expect("bind");
        assert!(
            bindings
                .iter()
                .all(|b| b.shape == Shape::Lines || b.column == "created_at")
        );
    }

    #[test]
    fn explicit_map_wins_and_reaches_nested_properties() {
        let cols = columns(&["cost:money", "label:String", "name:String"]);
        let bindings = bind(
            ty("Product"),
            &cols,
            &["cost=offers.price".to_owned(), "label=name".to_owned()],
        )
        .expect("bind");
        let cost = bindings.iter().find(|b| b.column == "cost").expect("cost");
        assert_eq!(cost.property, "offers");
        assert_eq!(cost.nested, Some(("Offer", "price")));
        let label = bindings
            .iter()
            .find(|b| b.column == "label")
            .expect("label");
        assert_eq!(label.property, "name");
        // `name` stays unbound: `label` took `name` first.
        assert!(bindings.iter().all(|b| b.column != "name"));
    }

    #[test]
    fn explicit_map_errors_name_the_problem() {
        let cols = columns(&["cost:money"]);
        let err = |pair: &str| {
            bind(ty("Product"), &cols, &[pair.to_owned()])
                .expect_err(pair)
                .to_string()
        };
        assert!(err("cost").contains("expected `column=property`"));
        assert!(err("nope=name").contains("no column `nope`"));
        assert!(
            err("cost=offers.prise")
                .contains("`Offer` has no property `prise`. Did you mean `price`")
        );
        assert!(err("cost=nmae").contains("Did you mean `name`"));
        assert!(err("cost=offers").contains("one column cannot hold"));
        assert!(err("cost=sku.x").contains("not a node"));
        let cols = columns(&["name:String", "label:String", "cost:money", "price:money"]);
        let conflict = |pairs: &[&str]| {
            let pairs: Vec<String> = pairs.iter().map(|p| (*p).to_owned()).collect();
            bind(ty("Product"), &cols, &pairs)
                .expect_err("conflict")
                .to_string()
        };
        let msg = conflict(&["name=name", "label=name"]);
        assert!(
            msg.contains("column `name` is already mapped to `name`"),
            "{msg}"
        );
        let msg = conflict(&["cost=offers.price", "price=offers.price"]);
        assert!(msg.contains("already mapped"), "{msg}");
        let msg = conflict(&["cost=offers.price", "price=offers"]);
        assert!(
            msg.contains("already mapped") || msg.contains("one column cannot hold"),
            "{msg}"
        );
        // Two different properties of one nested node do not conflict.
        bind(
            ty("Product"),
            &columns(&["cost:money", "currency:String"]),
            &[
                "cost=offers.price".to_owned(),
                "currency=offers.priceCurrency".to_owned(),
            ],
        )
        .expect("distinct nested properties");
    }

    #[test]
    fn excluded_kinds_never_bind() {
        let cols = columns(&["author:references", "name:String{encrypted}"]);
        assert!(cols.iter().all(|c| c.name == "created_at"), "{cols:?}");
    }

    #[test]
    fn renders_a_json_ld_fn() {
        let cols = columns(&[
            "name:String",
            "description:Option<Text>",
            "recipe_ingredient:Text",
            "author:Option<String>",
            "nutrition_calories:Option<String>",
        ]);
        let bindings = bind(ty("Recipe"), &cols, &[]).expect("bind");
        let src = render_json_ld_fn(ty("Recipe"), &bindings, "Recipe", "recipe").expect("fn");
        assert!(
            src.contains("fn json_ld(row: &Recipe) -> autumn_web::json_ld::JsonLd {"),
            "{src}"
        );
        assert!(src.contains("JsonLd::new(\"Recipe\")"), "{src}");
        assert!(src.contains(".set(\"name\", &row.name)"), "{src}");
        assert!(
            src.contains(".set_opt(\"description\", row.description.as_ref())"),
            "{src}"
        );
        assert!(
            src.contains(".set(\"recipeIngredient\", json_ld_lines(&row.recipe_ingredient))"),
            "{src}"
        );
        assert!(
            src.contains(
                "row.author.as_ref().map(|name| JsonLd::node(\"Person\").set(\"name\", name))"
            ),
            "{src}"
        );
        assert!(
            src.contains("let nutrition = JsonLd::node(\"NutritionInformation\")"),
            "{src}"
        );
        assert!(
            src.contains(".set_opt(\"nutrition\", Some(nutrition).filter(JsonLd::has_properties))"),
            "{src}"
        );
        assert!(
            src.contains(".set(\"dateCreated\", row.created_at)"),
            "{src}"
        );
        assert!(
            src.contains("fn json_ld_lines(text: &str) -> Vec<&str>"),
            "{src}"
        );
    }

    #[test]
    fn copy_columns_pass_by_value_and_text_by_reference() {
        let cols = columns(&[
            "headline:String",
            "word_count:i32",
            "config:json",
            "date_published:Timestamp",
        ]);
        let mut bindings = bind(ty("Article"), &cols, &[]).expect("bind");
        bindings.push(Binding {
            column: "config".to_owned(),
            property: "about",
            nested: None,
            shape: Shape::Scalar,
            nullable: false,
            copy: cols
                .iter()
                .find(|c| c.name == "config")
                .expect("config")
                .copy,
        });
        let src = render_json_ld_fn(ty("Article"), &bindings, "Article", "article").expect("fn");
        assert!(src.contains(".set(\"headline\", &row.headline)"), "{src}");
        assert!(src.contains(".set(\"wordCount\", row.word_count)"), "{src}");
        assert!(
            src.contains(".set(\"datePublished\", row.date_published)"),
            "{src}"
        );
        assert!(src.contains(".set(\"about\", &row.config)"), "{src}");
    }

    #[test]
    fn no_lines_helper_without_list_columns() {
        let cols = columns(&["name:String"]);
        let bindings = bind(ty("Thing"), &cols, &[]).expect("bind");
        let src = render_json_ld_fn(ty("Thing"), &bindings, "Thing", "thing").expect("fn");
        assert!(!src.contains("json_ld_lines"), "{src}");
    }

    #[test]
    fn no_fn_without_bindings() {
        assert_eq!(render_json_ld_fn(ty("Thing"), &[], "Thing", "thing"), None);
    }
}
