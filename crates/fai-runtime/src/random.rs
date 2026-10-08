//! The process-global xorshift64* stream used by the Random capability.

use std::sync::atomic::{AtomicU64, Ordering};

const SEED: u64 = 0x2545_f491_4f6c_dd1d;
static STATE: AtomicU64 = AtomicU64::new(SEED);

pub(crate) fn next() -> u64 {
    next_from(&STATE, || {})
}

fn advance(mut state: u64) -> u64 {
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    state
}

/// The observation point lets tests force concurrent readers of one state;
/// production uses an empty closure, inlined out of the hot path.
fn next_from(state: &AtomicU64, after_read: impl FnOnce()) -> u64 {
    let mut previous = state.load(Ordering::Relaxed);
    after_read();
    loop {
        let next = advance(previous);
        // The state has no associated memory to publish; its atomic modification
        // order is sufficient to serialize draws. A failed contender retries
        // from the committed state instead of overwriting another draw.
        match state.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next.wrapping_mul(SEED),
            Err(actual) => previous = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUTS: [u64; 8] = [
        0xad5db60b4c5d45d4,
        0xbedaf65d8707906e,
        0x0a1fe02d6c6dd3d0,
        0x9706f44d6f0b43b4,
        0xfdf3c3d8a31fa376,
        0x481e64cc0cd21571,
        0x20b83dcc23713dfa,
        0x785134315b1edc82,
    ];
    const FINAL_STATE: u64 = 0xcee3fa071b33c0ea;

    #[test]
    fn sequential_stream_preserves_the_existing_algorithm() {
        let state = AtomicU64::new(SEED);
        let outputs: Vec<_> = (0..8).map(|_| next_from(&state, || {})).collect();
        assert_eq!(outputs, OUTPUTS);
        assert_eq!(state.load(Ordering::Relaxed), FINAL_STATE);
    }

    #[test]
    fn concurrent_advances_equal_a_serialized_stream() {
        let state = AtomicU64::new(SEED);
        let read = std::sync::Barrier::new(8);
        let mut outputs = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        next_from(&state, || {
                            read.wait();
                        })
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
        });
        outputs.sort_unstable();
        let mut expected = OUTPUTS;
        expected.sort_unstable();
        assert_eq!(outputs, expected, "each caller must commit a distinct stream transition");
        assert_eq!(state.load(Ordering::Relaxed), FINAL_STATE);
    }

    #[test]
    fn boxed_bounds_and_results_leave_no_live_objects() {
        let _guard = crate::tests::lock();
        let baseline = crate::live_count();
        let result = crate::fai_random_next_int(crate::fai_box_int(i64::MAX));
        assert!((0..i64::MAX).contains(&crate::read_int(result)));
        crate::fai_drop(result);
        assert_eq!(crate::live_count(), baseline);
    }

    #[test]
    fn nonpositive_boxed_bound_does_not_advance_the_stream() {
        let _guard = crate::tests::lock();
        let baseline = crate::live_count();
        let before = STATE.load(Ordering::Relaxed);
        let result = crate::fai_random_next_int(crate::fai_box_int(i64::MIN));
        assert_eq!(crate::read_int(result), 0);
        crate::fai_drop(result);
        assert_eq!(STATE.load(Ordering::Relaxed), before);
        assert_eq!(crate::live_count(), baseline);
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn a_nonzero_seed_never_advances_to_zero(seed in 1u64..=u64::MAX) {
            let state = AtomicU64::new(seed);
            let _ = next_from(&state, || {});
            prop_assert_ne!(state.load(Ordering::Relaxed), 0);
        }
    }
}
