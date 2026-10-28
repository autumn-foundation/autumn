//! Parts-per-million helpers shared by `slo` and `fault_injection`.

/// One million parts: 100%.
pub const PPM: u32 = 1_000_000;

/// Convert a percentage to ppm. Reject values outside `(0, 100)` and values
/// with more than four decimals.
pub(crate) fn objective_to_ppm(objective: f64) -> Result<u32, String> {
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
