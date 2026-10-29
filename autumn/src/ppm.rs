//! Parts-per-million helpers shared by `slo` and `fault_injection`.

/// One million parts: 100%.
pub const PPM: u32 = 1_000_000;

/// Convert a percentage to ppm. Reject values outside `(0, 100)` and values
/// with more than four decimals.
pub fn objective_to_ppm(objective: f64) -> Result<u32, String> {
    if !objective.is_finite() || objective <= 0.0 || objective >= 100.0 {
        return Err(format!(
            "objective = {objective} must be greater than 0 and less than 100"
        ));
    }
    let scaled = objective * 10_000.0;
    let rounded = scaled.round();
    if (scaled - rounded).abs() > 1e-6 {
        return Err(format!(
            "objective = {objective} has more than four decimal places"
        ));
    }
    // The range check above keeps `rounded` in 1..=999_999.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ppm = rounded as u32;
    if ppm == 0 || ppm >= PPM {
        return Err(format!(
            "objective = {objective} must be greater than 0 and less than 100"
        ));
    }
    Ok(ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentage_becomes_ppm() {
        assert_eq!(objective_to_ppm(99.0), Ok(990_000));
        assert_eq!(objective_to_ppm(99.9), Ok(999_000));
        assert_eq!(objective_to_ppm(99.9999), Ok(999_999));
    }

    #[test]
    fn a_value_outside_zero_to_one_hundred_is_refused() {
        for bad in [0.0, -1.0, 100.0, 150.0, f64::NAN, f64::INFINITY] {
            assert!(objective_to_ppm(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn more_than_four_decimals_is_refused() {
        let error = objective_to_ppm(99.12345).expect_err("five decimals");
        assert!(error.contains("four decimal"), "{error}");
    }

    #[test]
    fn a_value_that_rounds_to_zero_is_refused() {
        assert!(objective_to_ppm(0.00001).is_err());
    }
}
