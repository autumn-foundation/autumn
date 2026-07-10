//! Tests for the deterministic fake-data generators in `autumn_web::fake`.

use autumn_web::fake;

/// Draw a mixed sequence of values, exercising most generators, so determinism
/// covers the full API surface (each draw advances the shared RNG).
fn sample_sequence() -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..25 {
        out.push(fake::name());
        out.push(fake::email());
        out.push(fake::username());
        out.push(fake::sentence());
        out.push(fake::paragraph());
        out.push(fake::url());
        out.push(fake::word());
        out.push(fake::boolean().to_string());
        out.push(fake::int_range(1, 10).to_string());
        out.push(fake::decimal().to_string());
        out.push(fake::decimal_f64().to_string());
        out.push(fake::recent_datetime().to_rfc3339());
        out.push(fake::uuid().to_string());
    }
    out
}

#[test]
fn reseed_makes_output_deterministic() {
    fake::reseed(42);
    let first = sample_sequence();
    fake::reseed(42);
    let second = sample_sequence();
    assert_eq!(
        first, second,
        "same seed must reproduce identical sequences"
    );
}

#[test]
fn different_seeds_diverge() {
    fake::reseed(1);
    let a = sample_sequence();
    fake::reseed(2);
    let b = sample_sequence();
    assert_ne!(a, b, "different seeds should produce different sequences");
}

#[test]
fn name_has_high_cardinality() {
    fake::reseed(7);
    let mut names = std::collections::HashSet::new();
    for _ in 0..200 {
        names.insert(fake::name());
    }
    assert!(
        names.len() > 100,
        "expected many distinct names, got {}",
        names.len()
    );
}

#[test]
fn sentence_has_high_cardinality() {
    fake::reseed(7);
    let mut sentences = std::collections::HashSet::new();
    for _ in 0..200 {
        sentences.insert(fake::sentence());
    }
    assert!(
        sentences.len() > 150,
        "expected many distinct sentences, got {}",
        sentences.len()
    );
}

#[test]
fn int_range_stays_within_bounds() {
    fake::reseed(7);
    for _ in 0..1000 {
        let v = fake::int_range(1, 10);
        assert!((1..=10).contains(&v), "int_range out of bounds: {v}");
    }
    // Degenerate range collapses to lo.
    assert_eq!(fake::int_range(5, 5), 5);
    assert_eq!(fake::int_range(9, 3), 9);
}

#[test]
fn email_contains_exactly_one_at() {
    fake::reseed(7);
    for _ in 0..100 {
        let e = fake::email();
        assert_eq!(e.matches('@').count(), 1, "bad email: {e}");
    }
}

#[test]
fn url_is_https() {
    fake::reseed(7);
    for _ in 0..100 {
        let u = fake::url();
        assert!(u.starts_with("https://"), "bad url: {u}");
    }
}

#[test]
fn username_is_lowercase_and_spaceless() {
    fake::reseed(7);
    for _ in 0..100 {
        let u = fake::username();
        assert!(!u.contains(' '), "username has space: {u}");
        assert_eq!(u, u.to_lowercase(), "username not lowercase: {u}");
    }
}

#[test]
fn uuids_are_distinct_and_v4() {
    fake::reseed(7);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..500 {
        let id = fake::uuid();
        assert_eq!(id.get_version_num(), 4, "uuid not v4: {id}");
        assert!(seen.insert(id), "duplicate uuid: {id}");
    }
}

#[test]
fn words_zero_is_empty() {
    fake::reseed(7);
    assert_eq!(fake::words(0), "");
    assert!(!fake::words(3).is_empty());
}
