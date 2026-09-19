//! The pluggable embedding seam.
//!
//! AC: "Embedding generation is pluggable (a trait the app implements or a
//! provider adapter); the search plugin does not hardcode an embedding model
//! or vendor." So the crate ships exactly two implementations: one that
//! refuses (the honest default) and one that is deterministic and
//! dependency-free (dev + tests). Anything vendor-shaped is the app's.

use autumn_search::{Embedder as _, HashingEmbedder, NoEmbedder, SearchError, cosine_similarity};

#[tokio::test]
async fn the_default_embedder_refuses_rather_than_inventing_vectors() {
    let err = NoEmbedder
        .embed(&["hello".to_owned()])
        .await
        .expect_err("NoEmbedder must not produce vectors");
    assert!(matches!(err, SearchError::EmbedderUnavailable), "{err:?}");
    assert_eq!(NoEmbedder.dimensions(), 0);
}

#[tokio::test]
async fn the_hashing_embedder_is_deterministic_and_normalized() {
    let embedder = HashingEmbedder::new(64);
    let first = embedder
        .embed(&["autumn rust web framework".to_owned()])
        .await
        .expect("embed");
    let second = embedder
        .embed(&["autumn rust web framework".to_owned()])
        .await
        .expect("embed");

    assert_eq!(first, second, "embeddings must be reproducible");
    assert_eq!(first[0].len(), 64);

    let norm: f32 = first[0].iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-5,
        "expected unit vector, got {norm}"
    );
}

#[tokio::test]
async fn the_hashing_embedder_puts_related_text_closer_than_unrelated_text() {
    let embedder = HashingEmbedder::new(256);
    let vectors = embedder
        .embed(&[
            "rust web framework".to_owned(),
            "rust web framework for apis".to_owned(),
            "tomato basil gardening".to_owned(),
        ])
        .await
        .expect("embed");

    let related = cosine_similarity(&vectors[0], &vectors[1]);
    let unrelated = cosine_similarity(&vectors[0], &vectors[2]);
    assert!(
        related > unrelated,
        "related={related} should exceed unrelated={unrelated}"
    );
}

#[tokio::test]
async fn embedding_an_empty_batch_makes_no_work() {
    let embedder = HashingEmbedder::new(8);
    assert!(embedder.embed(&[]).await.expect("embed").is_empty());
    // Even the refusing embedder short-circuits an empty batch, so a record
    // with nothing to embed never trips the "no embedder" error.
    assert!(NoEmbedder.embed(&[]).await.expect("embed").is_empty());
}

#[test]
fn cosine_similarity_is_defined_for_degenerate_inputs() {
    assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    // A zero vector has no direction: similarity is 0, never NaN.
    assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]).abs() < f32::EPSILON);
    // Mismatched lengths are 0 rather than a panic or a partial dot product.
    assert!(cosine_similarity(&[1.0, 0.0], &[1.0]).abs() < f32::EPSILON);
}

#[tokio::test]
async fn dimensions_zero_is_rejected_at_construction() {
    // A 0-dimension embedder would make every document identical.
    assert!(HashingEmbedder::try_new(0).is_none());
    assert!(HashingEmbedder::try_new(1).is_some());
}
