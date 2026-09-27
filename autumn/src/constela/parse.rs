//! Turning a JSON string into a [`Program`], under explicit bounds.
//!
//! Parsing is the first place a hostile document can do damage, and it can do
//! it without any of the constructs the later stages check: a 40 MB body, or a
//! 200 000-deep nest of `{"expr":"not","operand":{...}}` that blows the stack
//! inside `serde`'s recursive descent. So the bounds in [`Limits`] are applied
//! *before* and *around* deserialization, not after it.

// autumn-panic-gate: request-path module — it parses, validates and renders a
// document written by a language model, so every panic in it is reachable by
// hostile input and would be a 500 rather than a diagnostic. Production code
// path must be panic-free. See CONTRIBUTING.md "Request-path panic gate".
// Justify exceptions with #[allow(clippy::<lint>, reason = "…")] at the
// narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use serde_json::Value;

use super::ast::Program;
use super::error::{ConstelaError, Diagnostic, codes};

/// Structural bounds a document must fit inside to be parsed.
///
/// The defaults are sized for "a page an LLM wrote", with enough headroom that
/// a legitimate document never meets them and little enough that a hostile one
/// is cheap to reject. They are a field of the parse call rather than a
/// constant so an app that mounts this on an untrusted public endpoint can
/// tighten them without forking the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum size of the JSON source, in bytes. Default 512 KiB.
    pub max_bytes: usize,
    /// Maximum JSON nesting depth. Default 64.
    ///
    /// Checked on the [`Value`] tree, before the *typed* deserialization that
    /// recurses once per level through the derived `Expr`/`Node`
    /// deserializers — so a document engineered to overflow the stack there is
    /// rejected before it can. `serde_json`'s own 128-frame recursion limit
    /// backstops this while the `Value` is built, which is why even
    /// [`Limits::unbounded`] cannot be made to blow the stack here.
    pub max_depth: usize,
    /// Maximum number of JSON nodes (scalars, arrays, objects, and object
    /// entries) in the document. Default 20 000.
    pub max_nodes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024,
            max_depth: 64,
            max_nodes: 20_000,
        }
    }
}

impl Limits {
    /// Bounds with every limit raised as far as it goes.
    ///
    /// For tests and for trusted, locally-authored documents. Do not use this
    /// on anything a request body carried in.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_bytes: usize::MAX,
            max_depth: usize::MAX,
            max_nodes: usize::MAX,
        }
    }
}

/// Parse `source` into a [`Program`] without validating it.
///
/// Almost every caller wants [`parse`](super::parse) instead, which runs
/// [`validate`](super::validate) too and hands back a
/// [`Document`](super::Document) the renderer will accept. This entry point
/// exists for tooling that wants the syntax tree of a document it already
/// knows is invalid — a repair loop showing the author what was parsed, say.
///
/// # Errors
///
/// [`ConstelaError::Limit`] when `source` exceeds a bound in `limits`;
/// [`ConstelaError::Syntax`] when it is not valid JSON or does not match the
/// AST shape.
pub fn parse_program(source: &str, limits: &Limits) -> Result<Program, ConstelaError> {
    if source.len() > limits.max_bytes {
        return Err(ConstelaError::Limit(Diagnostic::new(
            "",
            codes::LIMIT,
            format!(
                "document is {} bytes, over the {}-byte limit",
                source.len(),
                limits.max_bytes
            ),
        )));
    }

    let value: Value = serde_json::from_str(source).map_err(|err| {
        ConstelaError::Syntax(Diagnostic::new(
            "",
            codes::SYNTAX,
            format!("not valid JSON: {err}"),
        ))
    })?;

    check_shape(&value, limits)?;

    Program::deserialize_value(value)
}

impl Program {
    /// Deserialize a pre-parsed, already-bounded [`Value`].
    ///
    /// Split out so [`parse_program`] can run [`check_shape`] between
    /// `serde_json`'s iterative parse and its recursive typed deserialization.
    fn deserialize_value(value: Value) -> Result<Self, ConstelaError> {
        serde_json::from_value(value).map_err(|err| {
            ConstelaError::Syntax(Diagnostic::new(
                "",
                codes::SYNTAX,
                format!("does not match the Constela AST: {err}"),
            ))
        })
    }
}

/// Walk `value` iteratively, enforcing the depth and node-count bounds.
///
/// Iterative on purpose: a recursive walk here would overflow the stack on
/// exactly the input it exists to reject.
fn check_shape(value: &Value, limits: &Limits) -> Result<(), ConstelaError> {
    let mut nodes = 0usize;
    // (value, depth of that value)
    let mut stack: Vec<(&Value, usize)> = vec![(value, 1)];

    while let Some((current, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > limits.max_nodes {
            return Err(ConstelaError::Limit(Diagnostic::new(
                "",
                codes::LIMIT,
                format!("document has more than {} JSON nodes", limits.max_nodes),
            )));
        }
        if depth > limits.max_depth {
            return Err(ConstelaError::Limit(Diagnostic::new(
                "",
                codes::LIMIT,
                format!("document nests deeper than {} levels", limits.max_depth),
            )));
        }

        match current {
            Value::Array(items) => {
                let next = depth.saturating_add(1);
                stack.extend(items.iter().map(|item| (item, next)));
            }
            Value::Object(entries) => {
                let next = depth.saturating_add(1);
                stack.extend(entries.iter().map(|(_, item)| (item, next)));
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_view() -> &'static str {
        r#"{"version":"1.0","view":{"kind":"text","value":{"expr":"lit","value":"hi"}}}"#
    }

    #[test]
    fn parses_a_minimal_program() {
        let program = parse_program(tiny_view(), &Limits::default()).expect("parses");
        assert_eq!(program.version, "1.0");
        assert_eq!(program.view.kind(), "text");
    }

    #[test]
    fn rejects_oversized_source_without_parsing_it() {
        let limits = Limits {
            max_bytes: 16,
            ..Limits::default()
        };
        let err = parse_program(tiny_view(), &limits).expect_err("over the byte limit");
        assert!(matches!(err, ConstelaError::Limit(_)));
        assert_eq!(err.diagnostics()[0].code, codes::LIMIT);
    }

    #[test]
    fn rejects_deep_nesting_before_typed_deserialization() {
        // 100 levels of `not` would recurse 100 frames deep inside serde's
        // derived `Expr` deserializer. The depth check must reject it first.
        //
        // 100 and not 5000 on purpose: `serde_json` enforces its own
        // 128-frame recursion limit while building the `Value`, so anything
        // deeper is rejected as a *syntax* error before `check_shape` is
        // reached. That is the backstop described on `Limits::max_depth` — it
        // is why `Limits::unbounded()` still cannot be made to overflow the
        // stack — but this test is about the bound this module owns, so it
        // stays inside the range where that bound is the one doing the work.
        let mut deep = String::from(r#"{"version":"1.0","view":{"kind":"text","value":"#);
        let depth = 100;
        for _ in 0..depth {
            deep.push_str(r#"{"expr":"not","operand":"#);
        }
        deep.push_str(r#"{"expr":"lit","value":true}"#);
        for _ in 0..depth {
            deep.push('}');
        }
        deep.push_str("}}");

        let err = parse_program(&deep, &Limits::default()).expect_err("over the depth limit");
        assert!(matches!(err, ConstelaError::Limit(_)));
        assert!(err.diagnostics()[0].message.contains("nests deeper"));
    }

    #[test]
    fn rejects_too_many_nodes() {
        let items: Vec<String> = (0..500)
            .map(|i| format!(r#"{{"kind":"text","value":{{"expr":"lit","value":{i}}}}}"#))
            .collect();
        let source = format!(
            r#"{{"version":"1.0","view":{{"kind":"element","tag":"div","children":[{}]}}}}"#,
            items.join(",")
        );
        let limits = Limits {
            max_nodes: 100,
            ..Limits::default()
        };
        let err = parse_program(&source, &limits).expect_err("over the node limit");
        assert!(err.diagnostics()[0].message.contains("JSON nodes"));
    }

    #[test]
    fn reports_bad_json_as_syntax() {
        let err = parse_program("{not json", &Limits::default()).expect_err("bad json");
        assert!(matches!(err, ConstelaError::Syntax(_)));
        assert_eq!(err.diagnostics()[0].code, codes::SYNTAX);
    }

    #[test]
    fn unknown_expression_variant_names_the_supported_ones() {
        let source =
            r#"{"version":"1.0","view":{"kind":"text","value":{"expr":"call","method":"x"}}}"#;
        let err = parse_program(source, &Limits::default()).expect_err("call is unsupported");
        let message = &err.diagnostics()[0].message;
        assert!(message.contains("call"), "{message}");
        assert!(message.contains("lit"), "{message}");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let source = r#"{"version":"1.0","theme":{"mode":"dark"},
            "view":{"kind":"element","tag":"div","transition":{"enter":"x"},"children":[]}}"#;
        let program =
            parse_program(source, &Limits::default()).expect("unknown fields are ignored");
        assert_eq!(program.view.kind(), "element");
    }
}
