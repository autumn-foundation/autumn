use autumn_web::auth::{hash_password, verify_password};
use std::time::Instant;

#[tokio::test]
async fn eris_auth_timing_test() {
    let valid_hash = hash_password("secret123").await.unwrap();

    // 0. Warm up the bcrypt library to avoid lazy initialization overhead in timing
    let _ = verify_password("secret123", &valid_hash).await;
    let _ = verify_password("secret123", "invalid_hash_format").await;

    // 1. Time the verification of a valid hash
    let start_valid = Instant::now();
    let _ = verify_password("secret123", &valid_hash).await;
    let elapsed_valid = start_valid.elapsed();

    // 2. Time the verification of an invalid hash
    let start_invalid = Instant::now();
    let _ = verify_password("secret123", "invalid_hash_format").await;
    let elapsed_invalid = start_invalid.elapsed();

    // 3. Compare the times. If invalid is significantly faster, we have a timing vulnerability.
    println!("Elapsed valid: {elapsed_valid:?}");
    println!("Elapsed invalid: {elapsed_invalid:?}");

    // To make this a regression test that fails now and passes later, we assert:
    // In CI (especially Windows Debug), timing can be highly variable. We just
    // want to make sure it's not a fast-path return (which takes < 5ms).
    // The dummy hash should take at least 50ms (often > 1000ms), and shouldn't
    // be less than a tenth of the valid check's time.
    assert!(
        elapsed_invalid.as_millis() > 50,
        "Timing vulnerability detected: Invalid hash returned instantly ({}ms)",
        elapsed_invalid.as_millis()
    );

    assert!(
        elapsed_invalid.as_millis() >= elapsed_valid.as_millis() / 10,
        "Timing vulnerability detected: Invalid hash check is significantly faster ({}ms vs {}ms)",
        elapsed_invalid.as_millis(),
        elapsed_valid.as_millis()
    );
}
