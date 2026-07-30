//! Typed failures for the search subsystem.

use autumn_web::AutumnError;

/// Result alias used throughout `autumn-search`.
pub type SearchResult<T> = Result<T, SearchError>;

/// Everything that can go wrong indexing or querying.
///
/// Deliberately typed rather than stringly: the difference between "this index
/// is not registered" (a wiring bug, a 500) and "this query has no embedder"
/// (a configuration gap, a 501) is the difference between two very different
/// operator responses. Nothing here ever degrades into an unfiltered read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// No index with this name is registered on the client.
    #[error(
        "search index {0:?} is not registered; add it with `SearchPlugin::index::<Model>()` \
         (the model must be `#[model]` + `#[searchable]` with an i64 primary key)"
    )]
    UnknownIndex(String),

    /// The index definition failed [`autumn_web::search::IndexDefinition::validate`].
    #[error("invalid search index definition: {0}")]
    InvalidIndex(String),

    /// A vector query hit an index with no `#[searchable(embed)]` field.
    #[error(
        "search index {index:?} has no embedded field; add `#[searchable(embed)]` to the field \
         you want embedded to enable vector search"
    )]
    VectorUnsupported {
        /// The index that was queried.
        index: String,
    },

    /// A vector operation was attempted with no usable embedder installed.
    #[error(
        "no embedding provider is configured; install one with `SearchPlugin::embedder(...)` \
         (the plugin ships no model or vendor by design)"
    )]
    EmbedderUnavailable,

    /// The embedder itself failed.
    #[error("embedding failed: {0}")]
    Embedding(String),

    /// A vector's length disagrees with what the index holds.
    #[error("embedding has {actual} dimensions, expected {expected}")]
    DimensionMismatch {
        /// Dimensionality the index was built with.
        expected: usize,
        /// Dimensionality of the offending vector.
        actual: usize,
    },

    /// An authorization-aware entry point was used with no visibility hook.
    #[error(
        "no `SearchVisibility` is registered; install one with `SearchPlugin::visibility(...)` \
         — an authorization-aware search must never fall back to an unfiltered one"
    )]
    VisibilityUnavailable,

    /// A reindex/backfill needs a [`crate::DocumentSource`] and none is installed.
    #[error(
        "no `DocumentSource` is installed; add one with `SearchPlugin::source(...)` \
         (the Postgres source is installed automatically by `SearchPlugin::postgres()`)"
    )]
    SourceUnavailable,

    /// The backend does not implement this operation.
    #[error("search backend {backend:?} does not support {operation}")]
    Unsupported {
        /// Backend name.
        backend: &'static str,
        /// The unsupported operation.
        operation: &'static str,
    },

    /// The backend failed (I/O, SQL, an engine error).
    #[error("search backend error: {0}")]
    Backend(String),
}

impl SearchError {
    /// Wrap an arbitrary backend failure.
    pub fn backend(error: impl std::fmt::Display) -> Self {
        Self::Backend(error.to_string())
    }

    /// Wrap an arbitrary embedding failure.
    pub fn embedding(error: impl std::fmt::Display) -> Self {
        Self::Embedding(error.to_string())
    }
}

impl SearchError {
    /// Convert into an [`AutumnError`] with a status that matches the cause.
    ///
    /// A blanket `impl<E: std::error::Error> From<E> for AutumnError` already
    /// exists in `autumn-web`, so `?` works without this — but that blanket
    /// impl cannot tell a bad *query* from a broken *deployment*. Use this at
    /// a handler boundary to get the distinction: a vector query against an
    /// index with no embedded field is the caller's mistake (400), while a
    /// missing embedder or an unreachable engine is the operator's (500).
    #[must_use]
    pub fn into_autumn_error(self) -> AutumnError {
        let message = self.to_string();
        match self {
            // Caller-fixable: the query asked for something this index cannot
            // answer.
            Self::VectorUnsupported { .. } | Self::DimensionMismatch { .. } => {
                AutumnError::bad_request_msg(message)
            }
            // Operator-fixable configuration gaps and genuine failures.
            Self::UnknownIndex(_)
            | Self::InvalidIndex(_)
            | Self::EmbedderUnavailable
            | Self::VisibilityUnavailable
            | Self::SourceUnavailable
            | Self::Unsupported { .. }
            | Self::Embedding(_)
            | Self::Backend(_) => AutumnError::internal_server_error_msg(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_query_maps_to_a_client_error_not_a_500() {
        let error = SearchError::VectorUnsupported {
            index: "articles".to_owned(),
        }
        .into_autumn_error();
        assert_eq!(error.status().as_u16(), 400);
    }

    #[test]
    fn a_wiring_gap_maps_to_a_server_error() {
        let error = SearchError::UnknownIndex("articles".to_owned()).into_autumn_error();
        assert_eq!(error.status().as_u16(), 500);
    }

    #[test]
    fn messages_name_the_fix() {
        assert!(
            SearchError::EmbedderUnavailable
                .to_string()
                .contains("SearchPlugin::embedder")
        );
        assert!(
            SearchError::UnknownIndex("a".to_owned())
                .to_string()
                .contains("SearchPlugin::index")
        );
    }
}
