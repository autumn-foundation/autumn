//! A timing check for tests that does not depend on the speed of the machine.

use std::time::{Duration, Instant};

/// Below this time on the large input, `assert_linear` passes. At the sizes
/// the tests use, a quadratic path takes seconds.
const FAST_ENOUGH: Duration = Duration::from_millis(50);

/// The large input is this many times the small one.
const SCALE: usize = 8;

/// The highest ratio `assert_linear` accepts. Linear is about 8, quadratic
/// about 64. The wide gap absorbs the noise of a loaded CI runner.
const MAX_RATIO: f64 = 24.0;

/// Runs per input. The fastest run counts.
const RUNS: usize = 3;

/// Assert that `run` takes about linear time in the size of its input.
///
/// `make(k)` builds an input of size `k`. The check times `run` on
/// `make(n / SCALE)` and on `make(n)`, the best of [`RUNS`] runs each. The
/// runs alternate, so load on the machine slows both sizes alike. A linear
/// `run` takes about 8 times as long on the large input, a quadratic one
/// about 64 times. The check fails above [`MAX_RATIO`]. A ratio does not
/// change with the speed of the machine. Returns the output of `run` on
/// `make(n)`.
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
    let time = |input: &T| {
        let start = Instant::now();
        let output = run(input);
        (start.elapsed(), output)
    };
    let small_input = make(n / SCALE);
    let large_input = make(n);
    let (mut small, mut large) = (Duration::MAX, Duration::MAX);
    let mut output = None;
    for _ in 0..RUNS {
        small = small.min(time(&small_input).0);
        let (elapsed, result) = time(&large_input);
        large = large.min(elapsed);
        output = Some(result);
    }
    let output = output.expect("at least one run");
    if let Some(message) = verdict(what, n, small, large) {
        panic!("{message}");
    }
    output
}

/// The failure message when `large` (at `n`) is more than [`MAX_RATIO`]
/// times `small` (at `n / SCALE`), or `None`. A `large` under [`FAST_ENOUGH`]
/// passes.
fn verdict(what: &str, n: usize, small: Duration, large: Duration) -> Option<String> {
    if large < FAST_ENOUGH {
        return None;
    }
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    (ratio > MAX_RATIO).then(|| {
        format!(
            "{what} took {large:?} at {n} and {small:?} at {}: {ratio:.1} times for {SCALE} \
             times the input. That looks quadratic again.",
            n / SCALE
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
        // 8 times is linear. 20 times is linear on a loaded runner.
        assert_eq!(verdict("parse", 800, ms(100), ms(800)), None);
        assert_eq!(verdict("parse", 800, ms(100), ms(2000)), None);
    }

    #[test]
    fn a_quadratic_ratio_fails() {
        let message = verdict("parse", 800, ms(100), ms(6400)).expect("a failure");
        assert!(message.contains("64.0 times for 8 times"), "{message}");
        assert!(message.contains("looks quadratic again"), "{message}");
    }

    #[test]
    fn a_fast_large_run_passes_at_any_ratio() {
        assert_eq!(verdict("parse", 800, ms(1), ms(40)), None);
    }

    #[test]
    fn a_zero_small_run_does_not_divide_by_zero() {
        assert!(verdict("parse", 800, Duration::ZERO, ms(100)).is_some());
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
