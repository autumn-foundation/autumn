//! The pluggable embedding seam.
//!
//! `autumn-search` orchestrates embeddings; it does not ship a model, an
//! inference runtime, or a vendor SDK (explicitly out of scope in #1191). An
//! app implements [`Embedder`] — over an HTTP provider, a local ONNX runtime,
//! whatever — and installs it with `SearchPlugin::embedder(...)`.
//!
//! Two implementations ship here, and neither is a model:
//!
//! - [`NoEmbedder`] — the default. Refuses, loudly, so an app that never
//!   configured embeddings gets a typed error instead of meaningless vectors.
//! - [`HashingEmbedder`] — a deterministic, dependency-free hashing vectorizer
//!   (the classic "hashing trick"). It is a real lexical embedding, useful for
//!   development and for tests that must not reach the network, and it is
//!   honest about what it is: no semantics beyond token overlap.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

use crate::backend::BoxFuture;
use crate::error::{SearchError, SearchResult};
use crate::text::tokenize;

/// Turns text into vectors for semantic search.
///
/// Stored as `Arc<dyn Embedder>`, so the methods are object-safe.
pub trait Embedder: Send + Sync + 'static {
    /// Dimensionality of the vectors this embedder produces.
    ///
    /// **`0` means "no embeddings available".** The client treats a
    /// zero-dimension embedder as "skip embedding" while indexing (so a
    /// searchable model with an `embed` field still indexes for keyword search
    /// on an app that never configured a provider) and as
    /// [`SearchError::EmbedderUnavailable`] at query time (so a semantic query
    /// fails loudly rather than returning nonsense).
    fn dimensions(&self) -> usize;

    /// Embed a batch of texts, returning one vector per input in order.
    ///
    /// An empty batch must succeed with an empty result and do no work — the
    /// indexer calls this for every batch, including ones with nothing to
    /// embed.
    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, SearchResult<Vec<Vec<f32>>>>;
}

/// The default embedder: refuses to produce vectors.
///
/// Installed when an app has not configured a provider. Keyword search works
/// exactly as before; a semantic query returns
/// [`SearchError::EmbedderUnavailable`], whose message names the fix.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEmbedder;

impl Embedder for NoEmbedder {
    fn dimensions(&self) -> usize {
        0
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, SearchResult<Vec<Vec<f32>>>> {
        Box::pin(async move {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            Err(SearchError::EmbedderUnavailable)
        })
    }
}

/// A deterministic, dependency-free hashing vectorizer.
///
/// Each token is hashed into one of `dimensions` buckets (signed, to reduce
/// collision bias) and the resulting term-frequency vector is L2-normalized so
/// cosine similarity is a plain dot product. Two texts that share vocabulary
/// score higher than two that do not.
///
/// This is **not** a semantic model: "car" and "automobile" are unrelated to
/// it. Use it for development, for deterministic tests, and as a working
/// reference implementation of [`Embedder`] — not for production relevance.
#[derive(Debug, Clone, Copy)]
pub struct HashingEmbedder {
    dimensions: usize,
}

impl HashingEmbedder {
    /// Construct an embedder producing `dimensions`-wide vectors.
    ///
    /// # Panics
    ///
    /// Panics if `dimensions` is `0`. This is a *construction-time* argument
    /// check, not a request path — a 0-dimension embedder would map every text
    /// to the same vector, so failing at construction is the only honest
    /// outcome. Use [`HashingEmbedder::try_new`] for a runtime-supplied value.
    #[must_use]
    #[allow(
        clippy::expect_used,
        reason = "construction-time argument validation; no request path reaches this"
    )]
    pub const fn new(dimensions: usize) -> Self {
        Self::try_new(dimensions).expect(
            "HashingEmbedder requires at least one dimension; 0 would make every text identical",
        )
    }

    /// Fallible constructor. Returns `None` for `0` dimensions, which would
    /// map every text to the same (empty) vector.
    #[must_use]
    pub const fn try_new(dimensions: usize) -> Option<Self> {
        if dimensions == 0 {
            None
        } else {
            Some(Self { dimensions })
        }
    }

    fn embed_one(self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; self.dimensions];
        for token in tokenize(text) {
            let mut hasher = DefaultHasher::new();
            token.hash(&mut hasher);
            let hash = hasher.finish();
            // Low bits pick the bucket; one further bit picks the sign, which
            // keeps unrelated tokens from all pushing in the same direction.
            // `checked_rem` covers a zero-dimension config: the vector is
            // then empty and the `get_mut` below is a no-op either way.
            let bucket =
                usize::try_from(hash.checked_rem(self.dimensions as u64).unwrap_or_default())
                    .unwrap_or(0);
            let sign = if (hash >> 63) & 1 == 1 { -1.0 } else { 1.0 };
            if let Some(slot) = vector.get_mut(bucket) {
                *slot += sign;
            }
        }
        normalize(&mut vector);
        vector
    }
}

impl Embedder for HashingEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, SearchResult<Vec<Vec<f32>>>> {
        Box::pin(async move { Ok(texts.iter().map(|t| self.embed_one(t)).collect()) })
    }
}

/// Scale `vector` to unit length in place. A zero vector is left alone (it has
/// no direction), which keeps [`cosine_similarity`] free of `NaN`.
pub fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

/// Cosine similarity of two vectors, in `[-1.0, 1.0]`.
///
/// Returns `0.0` — never `NaN` — for a zero vector or a length mismatch, so a
/// malformed embedding degrades to "unrelated" instead of poisoning a sort.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denominator = norm_a.sqrt() * norm_b.sqrt();
    if denominator <= f32::EPSILON {
        return 0.0;
    }
    (dot / denominator).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_hashing_embedder_produces_unit_vectors_of_the_right_width() {
        let embedder = HashingEmbedder::new(16);
        let vectors = embedder
            .embed(&["autumn rust".to_owned()])
            .await
            .expect("embed");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].len(), 16);
        let norm: f32 = vectors[0].iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn empty_text_yields_a_zero_vector_rather_than_a_nan() {
        let embedder = HashingEmbedder::new(8);
        let vectors = embedder.embed(&[String::new()]).await.expect("embed");
        assert!(vectors[0].iter().all(|v| v.abs() < f32::EPSILON));
        assert!(cosine_similarity(&vectors[0], &vectors[0]).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_leaves_a_zero_vector_alone() {
        let mut zero = vec![0.0_f32; 4];
        normalize(&mut zero);
        assert!(zero.iter().all(|v| v.abs() < f32::EPSILON));
    }

    #[test]
    fn cosine_similarity_is_bounded_and_total() {
        assert!(cosine_similarity(&[], &[]).abs() < f32::EPSILON);
        assert!(cosine_similarity(&[1.0], &[1.0, 2.0]).abs() < f32::EPSILON);
        assert!(cosine_similarity(&[3.0, 4.0], &[3.0, 4.0]) <= 1.0);
    }

    #[test]
    fn try_new_rejects_zero_dimensions() {
        assert!(HashingEmbedder::try_new(0).is_none());
    }
}
