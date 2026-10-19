//! Storage-independent text values, checked ranges, and reference ownership.
use super::*;
use crate::tests::lock;
use crate::*;
use std::cmp::Ordering;

fn list(fields: &[(String, String)]) -> Value {
    let pairs: Vec<_> = fields.iter().map(|(a, b)| data(0, &[make_str(a), make_str(b)])).collect();
    fai_text_pairs_from_list(cons_list(&pairs))
}

fn indexes(values: &[i64]) -> Value {
    values.iter().fold(fai_array_with_capacity(make_int(values.len() as i64)), |array, &n| {
        fai_array_push(array, make_int(n))
    })
}

fn view(fields: &[(String, String)], padding: &str) -> Value {
    let mut text = padding.to_owned();
    let mut offsets = Vec::new();
    for (first, second) in fields {
        for value in [first, second] {
            text.push('|');
            offsets.push(text.len() as i64);
            text.push_str(value);
            offsets.push(text.len() as i64);
        }
    }
    text.push_str(padding);
    let result = fai_text_pairs_from_ranges(make_str(&text), indexes(&offsets));
    assert_eq!(read_int(fai_data_tag(result)), 0);
    let table = fai_data_field(result, 0);
    fai_drop(result);
    table
}

#[track_caller]
fn balanced(f: impl FnOnce()) {
    let _guard = lock();
    let before = (live_count(), live_bytes());
    f();
    assert_eq!((live_count(), live_bytes()), before);
}

#[track_caller]
fn invalid(text: &str, offsets: &[i64]) {
    balanced(|| {
        let result = fai_text_pairs_from_ranges(make_str(text), indexes(offsets));
        assert_eq!(read_int(fai_data_tag(result)), 1);
        let message = fai_data_field(result, 0);
        assert_eq!(read_string(message), b"invalid text-pair ranges");
        fai_drop(message);
        fai_drop(result);
    });
}

#[test]
fn empty_representations_compare_and_hash_equally() {
    balanced(|| {
        let a = list(&[]);
        let b = view(&[], "unused");
        assert_eq!(fai_text_pairs_is_empty_borrowed(a), from_bool(true));
        assert_eq!(fai_text_pairs_is_empty_borrowed(b), from_bool(true));
        assert!(values_equal(a, b));
        assert_eq!(values_compare(a, b), Ordering::Equal);
        assert_eq!(values_hash(a), values_hash(b));
        fai_drop(a);
        fai_drop(b);
    });
}

#[test]
fn full_enumeration_transfers_an_existing_list() {
    balanced(|| {
        let original = cons_list(&[data(0, &[make_str("X"), make_str("v")])]);
        let table = fai_text_pairs_from_list(original);
        reset_allocations();
        let output = fai_text_pairs_to_list(table);
        assert_eq!(output, original);
        assert_eq!(allocations(), 0);
        fai_drop(output);
    });
}

#[test]
fn materialized_fields_do_not_retain_the_source_buffer() {
    balanced(|| {
        let before = live_bytes();
        let table = view(&[("X".into(), "é🙂".into())], &"ignored".repeat(10000));
        let output = fai_text_pairs_to_list(table);
        assert!(live_bytes() - before < 512);
        let expected = list(&[("X".into(), "é🙂".into())]);
        let expected = fai_text_pairs_to_list(expected);
        assert!(values_equal(output, expected));
        fai_drop(output);
        fai_drop(expected);
    });
}

#[test]
fn lookup_preserves_duplicate_order_and_unicode_case_rules() {
    balanced(|| {
        let fields = [
            ("K".into(), "first".into()),
            ("K".into(), "second".into()),
            ("k".into(), "third".into()),
        ];
        let table = view(&fields, "other content");
        let name = make_str("k");
        let first = fai_text_pairs_get_borrowed(table, name);
        let first_value = fai_data_field(first, 0);
        assert_eq!(read_string(first_value), b"first");
        fai_drop(first_value);
        fai_drop(first);
        let all = fai_text_pairs_get_all_borrowed(table, name);
        let expected = cons_list(&[make_str("first"), make_str("second"), make_str("third")]);
        assert!(values_equal(all, expected));
        // SAFETY: borrowed lookups retain both live operands and their sole owners.
        unsafe {
            assert_eq!(rc_load(as_obj(table)), 1);
            assert_eq!(rc_load(as_obj(name)), 1);
        }
        fai_drop(expected);
        fai_drop(all);
        fai_drop(name);
        fai_drop(table);
    });
}

#[test]
fn missing_lookup_returns_none_without_materializing_fields() {
    balanced(|| {
        let table = view(&[("X".into(), "v".into())], "padding");
        let name = make_str("missing");
        reset_allocations();
        assert_eq!(fai_text_pairs_get_borrowed(table, name), imm_int(0));
        assert_eq!(allocations(), 0);
        fai_drop(table);
        fai_drop(name);
    });
}

#[test]
fn retained_range_array_updates_cannot_change_the_table() {
    balanced(|| {
        let original = indexes(&[0, 1, 2, 3]);
        let result = fai_text_pairs_from_ranges(make_str("X:v"), fai_dup(original));
        let table = fai_data_field(result, 0);
        fai_drop(result);
        let changed = fai_array_set(original, make_int(2), make_int(0));
        let expected = list(&[("X".into(), "v".into())]);
        assert!(values_equal(table, expected));
        fai_drop(changed);
        fai_drop(expected);
        fai_drop(table);
    });
}

#[test]
fn overlapping_and_reordered_ranges_keep_their_logical_order() {
    balanced(|| {
        let result =
            fai_text_pairs_from_ranges(make_str("abc"), indexes(&[1, 3, 0, 2, 0, 0, 3, 3]));
        let table = fai_data_field(result, 0);
        fai_drop(result);
        let expected = list(&[("bc".into(), "ab".into()), ("".into(), "".into())]);
        assert!(values_equal(table, expected));
        assert_eq!(values_hash(table), values_hash(expected));
        fai_drop(table);
        fai_drop(expected);
    });
}

#[test]
fn shared_tables_mark_both_children_before_cross_thread_use() {
    balanced(|| {
        let table = view(&[("X".into(), "retained".into())], "padding");
        fai_mark_shared(table);
        let (text, index) = children(table);
        // SAFETY: these are live boxed children reached by mark_shared's graph walk.
        unsafe {
            assert_ne!(rc_load(as_obj(text)) & MT_FLAG, 0);
            assert_ne!(rc_load(as_obj(index)) & MT_FLAG, 0);
        }
        let sent = fai_dup(table);
        std::thread::spawn(move || {
            let result = fai_text_pairs_get(sent, make_str("x"));
            let value = fai_data_field(result, 0);
            assert_eq!(read_string(value), b"retained");
            fai_drop(value);
            fai_drop(result);
        })
        .join()
        .unwrap();
        assert_eq!(pairs(&table).next().unwrap().1.text, "retained");
        fai_drop(table);
    });
}

#[test]
fn rejects_an_incomplete_pair() {
    invalid("abc", &[0, 1]);
}

#[test]
fn rejects_a_full_width_negative_start() {
    invalid("abc", &[i64::MIN, 1, 1, 2]);
}

#[test]
fn rejects_a_full_width_oversized_end() {
    invalid("abc", &[0, i64::MAX, 1, 2]);
}

#[test]
fn rejects_reversed_ranges() {
    invalid("abc", &[2, 1, 1, 2]);
}

#[test]
fn rejects_split_utf8_starts() {
    invalid("éx", &[1, 2, 2, 3]);
}

#[test]
fn rejects_split_utf8_ends() {
    invalid("éx", &[0, 1, 2, 3]);
}

#[test]
fn source_string_views_are_supported() {
    balanced(|| {
        let source = make_str("prefixX:ésuffix");
        // SAFETY: [6,10) covers X:é on UTF-8 boundaries in the retained source.
        let sliced = unsafe { make_string_slice(source, 6, 4) };
        fai_drop(source);
        let result = fai_text_pairs_from_ranges(sliced, indexes(&[0, 1, 2, 4]));
        let table = fai_data_field(result, 0);
        fai_drop(result);
        let expected = list(&[("X".into(), "é".into())]);
        assert!(values_equal(table, expected));
        fai_drop(expected);
        fai_drop(table);
    });
}

#[test]
fn resetting_text_storage_releases_children_before_cell_reuse() {
    balanced(|| {
        let table = view(&[("X".into(), "value".into())], "padding");
        let token = fai_drop_reuse(table);
        let fields = [make_int(1), make_int(2), make_int(3), make_int(4)];
        // SAFETY: token owns reset storage and all four immediate fields transfer in.
        let reused = unsafe { fai_reuse(token, 0, 4, fields.as_ptr()) };
        assert_eq!(reused, table);
        fai_drop(reused);
    });
}

#[test]
fn repeated_views_have_bounded_live_and_backing_storage() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes(), allocation_stats::current());
    let observe = |rounds| {
        let result = std::thread::spawn(move || {
            let fields = [("X".to_owned(), "é🙂".to_owned())];
            let padding = "unused".repeat(6000);
            reset_allocations();
            for _ in 0..rounds {
                let table = view(&fields, &padding);
                let alias = fai_dup(table);
                let values = fai_text_pairs_to_list(table);
                assert_eq!(pairs(&alias).next().unwrap().1.text, "é🙂");
                fai_drop(values);
                fai_drop(alias);
            }
            assert_eq!((live_count(), live_bytes()), (baseline.0, baseline.1));
            let retained = allocation_stats::current();
            assert_eq!(retained.mapped_bytes, baseline.2.mapped_bytes);
            assert_eq!(retained.system_bytes, baseline.2.system_bytes);
            (peak_live_bytes(), allocation_stats::peak_bytes(), retained)
        })
        .join()
        .unwrap();
        assert_eq!(allocation_stats::current(), baseline.2);
        result
    };
    assert_eq!(observe(12), observe(1200));
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn fields() -> impl Strategy<Value = Vec<(String, String)>> {
        prop::collection::vec((any::<String>(), any::<String>()), 0..24)
    }

    proptest! {
        #[test]
        fn storage_forms_preserve_values_and_hashes(fields in fields()) {
            balanced(|| {
                let a = list(&fields);
                let b = view(&fields, "prefix\0🙂");
                let c = view(&fields, "different source");
                assert!(values_equal(a, b) && values_equal(b, c));
                assert_eq!(values_hash(a), values_hash(b));
                assert_eq!(values_hash(b), values_hash(c));
                let materialized = fai_text_pairs_to_list(fai_dup(b));
                let original = fai_text_pairs_to_list(fai_dup(a));
                assert!(values_equal(materialized, original));
                fai_drop(materialized);
                fai_drop(original);
                fai_drop(a);
                fai_drop(b);
                fai_drop(c);
            });
        }

        #[test]
        fn ordering_matches_ordered_string_pairs(a in fields(), b in fields()) {
            balanced(|| {
                let av = view(&a, "not part of value");
                let bv = list(&b);
                assert_eq!(values_compare(av, bv), a.cmp(&b));
                assert_eq!(values_equal(av, bv), a == b);
                fai_drop(av);
                fai_drop(bv);
            });
        }
    }
}
