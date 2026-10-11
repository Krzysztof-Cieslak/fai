//! Conditional loop-state selection preserves values and immutable array owners.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

const SOURCE: &str = r#"module Main
let count i stop values total =
  if i >= stop then total
  else if Array.unsafeGet i values then count (i + 1) stop values (total + 1)
  else count (i + 1) stop values (total - 1)
public run : Int -> Array Bool -> Int
let run seed values = count 0 (Array.length values) values seed
public main : Runtime -> Unit
let main _ = ()
"#;

mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn selected_loop_states_match_wrapping_fold_and_preserve_shared_arrays() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Main.fai".into(), SOURCE.into());
        fai_driver::set_bce_shadow(true);
        let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        fai_driver::set_bce_shadow(false);
        let function = program.function(Symbol::intern("run")).unwrap();
        proptest::test_runner::TestRunner::default()
            .run(
                &(any::<i64>(), proptest::collection::vec(any::<bool>(), 0..256), any::<bool>()),
                |(seed, flags, shared)| {
                    let baseline = (rt::live_count(), rt::live_bytes());
                    let mut values = rt::fai_array_with_capacity(rt::make_int(flags.len() as i64));
                    for flag in &flags {
                        values = rt::fai_array_push(values, rt::make_int(i64::from(*flag)));
                    }
                    let original = shared.then(|| rt::fai_dup(values));
                    let result = rt::apply(rt::fai_dup(function), &[rt::make_int(seed), values]);
                    let expected = flags
                        .iter()
                        .fold(seed, |total, flag| total.wrapping_add(if *flag { 1 } else { -1 }));
                    prop_assert_eq!(rt::read_int(result), expected);
                    rt::fai_drop(result);
                    if let Some(original) = original {
                        for (index, expected) in flags.iter().enumerate() {
                            let value =
                                rt::fai_array_get_borrowed(original, rt::make_int(index as i64));
                            prop_assert_eq!(rt::read_int(value), i64::from(*expected));
                            rt::fai_drop(value);
                        }
                        rt::fai_drop(original);
                    }
                    prop_assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
                    Ok(())
                },
            )
            .unwrap();
        rt::fai_drop(function);
    }
}
