//! Errors and diagnostics for the Constela pipeline.
//!
//! The shape here is chosen for one job: **telling a language model what to
//! fix.** A generator that gets back "invalid document" has to guess; a
//! generator that gets back a list of JSON paths, stable codes, and one
//! sentence each can repair every fault in a single round trip. So
//! [`validate`](super::validate) collects *all* violations rather than
//! returning at the first, and every diagnostic carries a
//! [`path`](Diagnostic::path) that points into the submitted document.

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

use std::fmt;

/// A stable, machine-readable code identifying a class of violation.
///
/// Codes are part of the public surface: a repair prompt, a metric label, or a
/// test may match on them, so they change only with the same care as a
/// function signature.
pub mod codes {
    /// The document exceeded a [`Limits`](super::super::Limits) bound.
    pub const LIMIT: &str = "constela.limit";
    /// The document is not valid JSON, or does not match the AST shape.
    pub const SYNTAX: &str = "constela.syntax";
    /// `version` is not a supported DSL version.
    pub const VERSION: &str = "constela.version";
    /// A state field's `initial` does not inhabit its declared `type`.
    pub const STATE_TYPE: &str = "constela.state.type";
    /// A reference names something the document does not declare.
    pub const UNKNOWN_REF: &str = "constela.unknown_ref";
    /// A name is declared more than once.
    pub const DUPLICATE: &str = "constela.duplicate";
    /// An HTML tag is not on the allowlist.
    pub const TAG_NOT_ALLOWED: &str = "constela.tag_not_allowed";
    /// An attribute is not on the allowlist.
    pub const ATTR_NOT_ALLOWED: &str = "constela.attr_not_allowed";
    /// A URL-bearing attribute used a scheme that is not on the allowlist.
    pub const URL_SCHEME: &str = "constela.url_scheme";
    /// A node appears somewhere it is not legal.
    pub const MISPLACED: &str = "constela.misplaced";
    /// Components reference each other in a cycle.
    pub const CYCLE: &str = "constela.cycle";
    /// A step's operands do not fit its operation.
    pub const STEP_SHAPE: &str = "constela.step_shape";
    /// The node requires a Cargo feature this build does not have.
    pub const FEATURE_REQUIRED: &str = "constela.feature_required";
    /// An expression could not be evaluated at render time.
    pub const EVAL: &str = "constela.eval";
    /// Rendering exceeded a [`RenderLimits`](super::super::RenderLimits) bound.
    pub const RENDER_LIMIT: &str = "constela.render_limit";
}

/// One thing wrong with a document, located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Where the fault is, as a dotted/bracketed path into the submitted
    /// document — e.g. `view.children[2].props.href`. Rooted at the document,
    /// so it can be read against the exact JSON that was sent.
    pub path: String,
    /// A stable code from [`codes`].
    pub code: &'static str,
    /// One sentence saying what is wrong. Where a fix is unambiguous it says
    /// what to write instead.
    pub message: String,
}

impl Diagnostic {
    /// Build a diagnostic.
    pub fn new(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} [{}]", self.path, self.message, self.code)
    }
}

/// Everything that can go wrong turning JSON into rendered markup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstelaError {
    /// The input was rejected before parsing, or the AST exceeded a structural
    /// bound. Carries a single diagnostic because the check that tripped
    /// stopped the walk.
    Limit(Diagnostic),
    /// The input is not valid JSON, or does not match the AST shape.
    Syntax(Diagnostic),
    /// The document parsed but is not valid. Never empty.
    Invalid(Vec<Diagnostic>),
    /// Rendering failed. Carries a single diagnostic: a render aborts at the
    /// first fault rather than producing half a page.
    Render(Diagnostic),
}

impl ConstelaError {
    /// Every diagnostic this error carries, in report order.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Limit(d) | Self::Syntax(d) | Self::Render(d) => std::slice::from_ref(d),
            Self::Invalid(ds) => ds,
        }
    }

    /// The diagnostics rendered as a JSON array, ready to hand back to a
    /// generator as the repair prompt.
    ///
    /// ```
    /// # use autumn_web::constela::{parse, Limits};
    /// let err = parse(r#"{"version":"1.0","view":{"kind":"element","tag":"script"}}"#, &Limits::default())
    ///     .expect_err("script is not on the tag allowlist");
    /// let report = err.to_json();
    /// assert_eq!(report[0]["code"], "constela.tag_not_allowed");
    /// assert_eq!(report[0]["path"], "view");
    /// ```
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.diagnostics()
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "path": d.path,
                        "code": d.code,
                        "message": d.message,
                    })
                })
                .collect(),
        )
    }
}

impl fmt::Display for ConstelaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diagnostics = self.diagnostics();
        let label = match self {
            Self::Limit(_) => "constela document rejected",
            Self::Syntax(_) => "constela document could not be parsed",
            Self::Invalid(_) => "constela document is invalid",
            Self::Render(_) => "constela document could not be rendered",
        };
        write!(f, "{label} ({} problem", diagnostics.len())?;
        if diagnostics.len() != 1 {
            f.write_str("s")?;
        }
        f.write_str(")")?;
        for diagnostic in diagnostics {
            write!(f, "\n  - {diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConstelaError {}

impl ConstelaError {
    /// The HTTP status a handler returning this error should produce:
    /// `422 Unprocessable Entity`.
    ///
    /// A Constela document is a *body* that parsed as JSON but does not
    /// describe a renderable UI — the textbook 422, and the same status
    /// Autumn's own validation failures use. Consulted by
    /// [`AutumnError`](crate::error::AutumnError)'s blanket `From` impl, so
    /// `?` on a `ConstelaError` in a handler produces a 422 rather than the
    /// default 500.
    #[must_use]
    pub const fn http_status(&self) -> http::StatusCode {
        http::StatusCode::UNPROCESSABLE_ENTITY
    }
}
