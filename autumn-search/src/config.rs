//! The plugin-owned `[search]` section of `autumn.toml`.

use serde::{Deserialize, Serialize};

/// Configuration for the search subsystem.
///
/// ```toml
/// [search]
/// queue = "search"            # the #[job] queue reindex/backfill run on
/// batch_size = 500            # rows per backfill batch
/// enabled = true              # false ⇒ index writes become no-ops
/// embedding_dimensions = 768  # declared width; enables the pgvector fast path
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SearchConfig {
    /// Named `#[job]` queue the reindex and backfill jobs are routed to.
    ///
    /// A dedicated queue by default: indexing is bulk, off-request work that
    /// should never head-of-line-block a user-visible job.
    #[serde(default = "default_queue")]
    pub queue: String,

    /// Rows read (and documents written) per backfill batch.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// When `false`, index writes become no-ops and queries return empty
    /// pages. The kill switch for an incident: turning search off must not
    /// require a deploy, and must not start failing writes to the model.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Declared embedding width.
    ///
    /// Optional: the in-memory backend infers it from the vectors it is given.
    /// The Postgres backend needs it to declare a `pgvector` column, so
    /// setting it is what enables the accelerated k-NN path; without it,
    /// vectors fall back to a portable `double precision[]` column.
    #[serde(default)]
    pub embedding_dimensions: Option<usize>,
}

fn default_queue() -> String {
    "search".to_owned()
}

const fn default_batch_size() -> usize {
    500
}

const fn default_enabled() -> bool {
    true
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            queue: default_queue(),
            batch_size: default_batch_size(),
            enabled: default_enabled(),
            embedding_dimensions: None,
        }
    }
}

/// Why a `[search]` section was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchConfigError {
    /// The TOML did not parse, or `[search]` had an unknown/ill-typed key.
    #[error("invalid [search] configuration: {0}")]
    Invalid(String),
}

impl SearchConfig {
    /// Read `[search]` from the `autumn.toml` at `path`.
    ///
    /// Matches `autumn-media-plugin`'s `MediaConfig::from_autumn_toml`, which
    /// also takes a **path** — the string-taking form here is
    /// [`SearchConfig::from_toml_str`], so a caller who passes `"autumn.toml"`
    /// to the wrong one gets a compile error rather than a TOML parse error
    /// about the filename.
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] when the file cannot be read or
    /// its `[search]` table is malformed.
    pub fn from_autumn_toml(path: impl AsRef<std::path::Path>) -> Result<Self, SearchConfigError> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|e| {
            SearchConfigError::Invalid(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::from_toml_str(&contents)
    }

    /// Parse the `[search]` table out of an `autumn.toml` document.
    ///
    /// A missing section yields the defaults. An unknown key is an **error**,
    /// not a warning: a typo'd `queu = "indexing"` would otherwise silently
    /// leave indexing on the default queue with no signal at all.
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] when the document does not
    /// parse, when `[search]` has an unknown or ill-typed key, or when a value
    /// is out of range.
    pub fn from_toml_str(contents: &str) -> Result<Self, SearchConfigError> {
        #[derive(Deserialize)]
        struct Document {
            #[serde(default)]
            search: Option<SearchConfig>,
        }

        let document: Document =
            toml_from_str(contents).map_err(|e| SearchConfigError::Invalid(e.to_string()))?;
        let config = document.search.unwrap_or_default();
        config.validate()?;
        Ok(config)
    }

    /// Reject values that would misbehave at runtime.
    ///
    /// # Errors
    ///
    /// Returns [`SearchConfigError::Invalid`] for a zero batch size, an empty
    /// queue name, or a zero embedding width.
    pub fn validate(&self) -> Result<(), SearchConfigError> {
        if self.batch_size == 0 {
            return Err(SearchConfigError::Invalid(
                "search.batch_size must be at least 1".to_owned(),
            ));
        }
        if self.queue.trim().is_empty() {
            return Err(SearchConfigError::Invalid(
                "search.queue must not be empty".to_owned(),
            ));
        }
        if self.embedding_dimensions == Some(0) {
            return Err(SearchConfigError::Invalid(
                "search.embedding_dimensions must be at least 1".to_owned(),
            ));
        }
        Ok(())
    }
}

/// `toml::from_str` behind a helper so the dependency edge is in one place.
fn toml_from_str<T: serde::de::DeserializeOwned>(contents: &str) -> Result<T, toml::de::Error> {
    toml::from_str(contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_route_indexing_to_its_own_queue() {
        let config = SearchConfig::default();
        assert_eq!(config.queue, "search");
        assert_eq!(config.batch_size, 500);
        assert!(config.enabled);
        assert_eq!(config.embedding_dimensions, None);
    }

    #[test]
    fn a_partial_section_keeps_the_other_defaults() {
        let config =
            SearchConfig::from_toml_str("[search]\nqueue = \"indexing\"\n").expect("parse");
        assert_eq!(config.queue, "indexing");
        assert_eq!(config.batch_size, 500);
        assert!(config.enabled);
    }

    #[test]
    fn an_unrelated_document_yields_the_defaults() {
        assert_eq!(
            SearchConfig::from_toml_str("[server]\nport = 3000\n").expect("parse"),
            SearchConfig::default()
        );
        assert_eq!(
            SearchConfig::from_toml_str("").expect("parse"),
            SearchConfig::default()
        );
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silent_default() {
        assert!(SearchConfig::from_toml_str("[search]\nqueu = \"x\"\n").is_err());
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        assert!(SearchConfig::from_toml_str("[search]\nbatch_size = 0\n").is_err());
        assert!(SearchConfig::from_toml_str("[search]\nqueue = \"\"\n").is_err());
        assert!(SearchConfig::from_toml_str("[search]\nembedding_dimensions = 0\n").is_err());
    }

    #[test]
    fn an_ill_typed_value_is_rejected() {
        assert!(SearchConfig::from_toml_str("[search]\nbatch_size = \"lots\"\n").is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let config = SearchConfig {
            queue: "indexing".to_owned(),
            batch_size: 42,
            enabled: false,
            embedding_dimensions: Some(768),
        };
        let document = format!("[search]\n{}", toml::to_string(&config).expect("serialize"));
        assert_eq!(
            SearchConfig::from_toml_str(&document).expect("parse"),
            config
        );
    }
}
