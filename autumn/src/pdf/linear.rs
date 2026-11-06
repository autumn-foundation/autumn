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
/// If the ratio is above [`MAX_RATIO`].
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
    if large > FAST_ENOUGH {
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
        assert!(
            ratio <= MAX_RATIO,
            "{what} took {large:?} at {n} and {small:?} at {}: {ratio:.1} times for 4 times \
             the input. That looks quadratic again.",
            n / 4
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::assert_linear;

    #[test]
    fn a_linear_run_passes() {
        let sum = assert_linear(
            "sum",
            4_000_000,
            |k| vec![1_u64; k],
            |v| v.iter().sum::<u64>(),
        );
        assert_eq!(sum, 4_000_000);
    }

    #[test]
    #[should_panic(expected = "looks quadratic again")]
    fn a_quadratic_run_fails() {
        assert_linear(
            "pairs",
            8_000,
            |k| vec![1_u64; k],
            |v| {
                let mut total = 0_u64;
                for a in v {
                    for b in v {
                        total = total.wrapping_add(std::hint::black_box(a ^ b));
                    }
                }
                total
            },
        );
    }
}
