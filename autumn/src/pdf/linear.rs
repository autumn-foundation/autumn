//! A timing check for tests that does not depend on the speed of the machine.

use std::time::{Duration, Instant};

/// Below this time on the large input, `assert_linear` passes. At the sizes
/// the tests use, a quadratic path takes seconds.
const FAST_ENOUGH: Duration = Duration::from_millis(50);

/// The highest ratio `assert_linear` accepts. Linear is about 4, quadratic
/// about 16.
const MAX_RATIO: f64 = 8.0;

/// Assert that `run` takes about linear time in the size of its input.
///
/// `make(k)` builds an input of size `k`. The check times `run` on
/// `make(n / 4)` and on `make(n)`, the best of two runs each. A linear `run`
/// takes about 4 times as long on the large input, a quadratic one about 16
/// times. The check fails above [`MAX_RATIO`]. A ratio does not change with
/// the speed of the machine. Returns the output of `run` on `make(n)`.
///
/// # Panics
///
/// If [`verdict`] finds the ratio too high.
pub fn assert_linear<T, R>(
    what: &str,
    n: usize,
    make: impl Fn(usize) -> T,
    run: impl Fn(&T) -> R,
) -> R {
    let best = |input: &T| {
        let mut fastest = Duration::MAX;
        let mut output = None;
        for _ in 0..2 {
            let start = Instant::now();
            let result = run(input);
            fastest = fastest.min(start.elapsed());
            output = Some(result);
        }
        (fastest, output.expect("two runs"))
    };
    let (small, _) = best(&make(n / 4));
    let (large, output) = best(&make(n));
    if let Some(message) = verdict(what, n, small, large) {
        panic!("{message}");
    }
    output
}

/// The failure message when `large` (at `n`) is more than [`MAX_RATIO`]
/// times `small` (at `n / 4`), or `None`. A `large` under [`FAST_ENOUGH`]
/// passes.
fn verdict(what: &str, n: usize, small: Duration, large: Duration) -> Option<String> {
    if large < FAST_ENOUGH {
        return None;
    }
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    (ratio > MAX_RATIO).then(|| {
        format!(
            "{what} took {large:?} at {n} and {small:?} at {}: {ratio:.1} times for 4 times \
             the input. That looks quadratic again.",
            n / 4
        )
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{assert_linear, verdict};

    const fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn a_linear_ratio_passes() {
        assert_eq!(verdict("parse", 400, ms(100), ms(400)), None);
        assert_eq!(verdict("parse", 400, ms(100), ms(800)), None);
    }

    #[test]
    fn a_quadratic_ratio_fails() {
        let message = verdict("parse", 400, ms(100), ms(1600)).expect("a failure");
        assert!(message.contains("16.0 times"), "{message}");
        assert!(message.contains("looks quadratic again"), "{message}");
    }

    #[test]
    fn a_fast_large_run_passes_at_any_ratio() {
        assert_eq!(verdict("parse", 400, ms(1), ms(40)), None);
    }

    #[test]
    fn a_zero_small_run_does_not_divide_by_zero() {
        assert!(verdict("parse", 400, Duration::ZERO, ms(100)).is_some());
    }

    #[test]
    fn assert_linear_returns_the_output_of_the_large_run() {
        let sum = assert_linear(
            "sum",
            4_000_000,
            |k| vec![1_u64; k],
            |v| v.iter().sum::<u64>(),
        );
        assert_eq!(sum, 4_000_000);
    }
}
