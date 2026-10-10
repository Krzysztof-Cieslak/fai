//! Matched supplemental workloads, kept separate from the main algorithm registry.

use crate::algorithms::{Algorithm, Oracle};

/// Version of the supplemental workload definitions, independent of sampling.
pub const VERSION: &str = "tail-components-v1";

/// Construction, scalar-length and retained-prefix cases plus matched quicksort.
pub const COMPONENTS: &[Algorithm] = &[
    Algorithm {
        module: "TailBuildAscii",
        entry: "buildAscii",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(build_ascii),
    },
    Algorithm {
        module: "TailBuildUnicode",
        entry: "buildUnicode",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(build_unicode),
    },
    Algorithm {
        module: "TailLengthAscii",
        entry: "lengthAscii",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(length_ascii),
    },
    Algorithm {
        module: "TailLengthUnicode",
        entry: "lengthUnicode",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(length_unicode),
    },
    Algorithm {
        module: "TailViewsAscii",
        entry: "viewsAscii",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(views_ascii),
    },
    Algorithm {
        module: "TailViewsUnicode",
        entry: "viewsUnicode",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(views_unicode),
    },
    Algorithm {
        module: "TailQuickSort",
        entry: "run",
        jit_size: 2000,
        aot_size: 20000,
        oracle: Oracle::Int(quicksort),
    },
];

/// A supplemental case by its distinct measurement name.
#[must_use]
pub fn by_module(name: &str) -> Option<&'static Algorithm> {
    COMPONENTS.iter().find(|case| case.module == name)
}

/// Fai source of a supplemental case. Every case retains the normal worker ABI.
#[must_use]
pub fn source(name: &str) -> Option<&'static str> {
    by_module(name).map(|_| {
        if name == "TailQuickSort" {
            include_str!("../../../samples/algorithms/QuickSort.fai")
        } else {
            include_str!("tail_components/strings.fai")
        }
    })
}

fn ascii(n: i64) -> String {
    let mut text = String::new();
    for _ in 0..n {
        text.push('a');
    }
    text
}

fn unicode(n: i64) -> String {
    let mut text = String::new();
    for _ in 0..n {
        text.push_str("aéλ😀");
    }
    text
}

/// Incremental ASCII construction followed by Unicode-scalar length.
#[must_use]
pub fn build_ascii(n: i64) -> i64 {
    ascii(n).chars().count() as i64
}

/// Incremental mixed-width UTF-8 construction followed by scalar length.
#[must_use]
pub fn build_unicode(n: i64) -> i64 {
    unicode(n).chars().count() as i64
}

fn lengths(text: String) -> i64 {
    let mut total = 0;
    for _ in 0..200 {
        total += text.chars().count() as i64;
    }
    total
}

/// Construct an ASCII base, then observe its scalar length 200 times.
#[must_use]
pub fn length_ascii(n: i64) -> i64 {
    lengths(ascii(n))
}

/// Construct a UTF-8 base, then observe its scalar length 200 times.
#[must_use]
pub fn length_unicode(n: i64) -> i64 {
    lengths(unicode(n))
}

fn views(text: String) -> i64 {
    let half = text.chars().count() / 2;
    let prefixes: Vec<_> = (0..200)
        .map(|i| {
            let end =
                text.char_indices().nth(half + i % 3).map_or(text.len(), |(offset, _)| offset);
            &text[..end]
        })
        .collect();
    prefixes.iter().map(|prefix| prefix.chars().count() as i64).sum()
}

/// Construct and retain 200 ASCII prefixes before consuming their scalar lengths.
#[must_use]
pub fn views_ascii(n: i64) -> i64 {
    views(ascii(n))
}

/// Construct and retain 200 UTF-8 prefixes, with all indices measured in scalars.
#[must_use]
pub fn views_unicode(n: i64) -> i64 {
    views(unicode(n))
}

fn sort(values: &mut [i64]) {
    let (mut low, mut high) = (0, values.len());
    while high - low > 1 {
        let mut store = low;
        for index in low..high - 1 {
            if values[index] < values[high - 1] {
                values.swap(store, index);
                store += 1;
            }
        }
        values.swap(store, high - 1);
        if store - low < high - store - 1 {
            sort(&mut values[low..store]);
            low = store + 1;
        } else {
            sort(&mut values[store + 1..high]);
            high = store;
        }
    }
}

/// The sample's Lomuto partition and smaller-side-first recursion on every peer.
#[must_use]
pub fn quicksort(n: i64) -> i64 {
    let mut values: Vec<_> =
        (0..n).map(|k| k.wrapping_mul(2_654_435_761).wrapping_add(12345) % n).collect();
    sort(&mut values);
    values.iter().enumerate().fold(0, |total, (i, value)| total + i as i64 * value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_counts_scalars_not_bytes() {
        assert_eq!((build_unicode(7), length_unicode(7), views_unicode(7)), (28, 5600, 2999));
    }

    #[test]
    fn short_prefixes_clamp_at_scalar_boundaries() {
        assert_eq!((views_ascii(1), views_unicode(1)), (133, 599));
    }

    #[test]
    fn empty_prefixes_remain_empty() {
        assert_eq!((views_ascii(0), views_unicode(0)), (0, 0));
    }

    #[test]
    fn lomuto_sort_orders_duplicate_and_negative_keys() {
        let mut values = [7, -2, 7, 0, -2, 1];
        sort(&mut values);
        assert_eq!(values, [-2, -2, 0, 1, 7, 7]);
    }

    #[test]
    fn lomuto_result_matches_the_registered_sort_checksum() {
        assert_eq!(quicksort(257), crate::algorithms::quicksort_sum(257));
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn lomuto_matches_stable_sort(mut values in prop::collection::vec(any::<i64>(), 0..128)) {
            let mut expected = values.clone();
            expected.sort();
            super::sort(&mut values);
            prop_assert_eq!(values, expected);
        }
    }
}
