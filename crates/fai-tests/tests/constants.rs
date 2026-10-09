//! Literal globals retain exact values and ownership across native boundaries.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

#[track_caller]
fn run(source: &str, expected: &str) -> i64 {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Box.fai".into(), "module Box\npublic keep : 'a -> 'a\nlet keep x = x\n".into());
    let id = db.add_source("M.fai".into(), source.into());
    rt::capture_start();
    rt::reset_allocations();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let allocations = rt::allocations();
    assert_eq!(result.exit_code, 0, "{source}");
    assert_eq!(rt::capture_take().trim(), expected);
    allocations
}

#[test]
fn scalar_constants_keep_full_width_and_exact_float_bits() {
    run(
        "module M\nlet wide = 0x8000000000000001\nlet zero = -0.0\nlet nan = Float.fromBits 0x7ff8000000000012\nlet flag = true\nlet letter = 'λ'\nlet text = \"héllo\"\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let good = Box.keep wide = 0x8000000000000001 && Float.toBits (Box.keep zero) = 0x8000000000000000 && Float.toBits (Box.keep nan) = 0x7ff8000000000012 && Box.keep flag && Box.keep letter = 'λ' && Box.keep text = \"héllo\"\n  r.console.writeLine (if good then \"ok\" else \"wrong\")\n",
        "ok",
    );
}

#[test]
fn nested_constants_survive_generic_and_first_class_use() {
    run(
        "module M\ntype Point = { x : Float, y : Float }\npoint : Point\nlet point = { x = 1.5, y = 2.5 }\nlet pair = (point, \"kept\")\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let keep = Box.keep\n  let (p, name) = keep pair\n  r.console.writeLine (name ++ Float.toString (p.x + p.y))\n",
        "kept4.0",
    );
}

#[test]
fn relocated_dictionary_lambdas_survive_generic_use() {
    run(
        "module M\ninterface F = apply : Int -> Int\nlet inst = { F with apply x = x + 1 }\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let f = Box.keep inst\n  r.console.writeLine (Int.toString (f.apply 41))\n",
        "42",
    );
}

#[test]
fn effectful_initializers_still_run_once_per_read() {
    run(
        "module M\nlet value =\n  let ignored = stdConsole.writeLine \"forced\"\n  42\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (value + value))\n",
        "forced\nforced\n84",
    );
}

#[test]
fn an_untaken_branch_does_not_force_a_trapping_initializer() {
    run(
        "module M\nlet value = 1 / 0\nlet choose use = if use then value else 42\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (choose false))\n",
        "42",
    );
}

#[test]
fn repeated_float_global_reads_allocate_like_literals() {
    let source = "module M\nlet dt = 0.5\nlet loop n acc = if n <= 0 then acc else loop (n - 1) (acc + dt)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Float.toString (loop 100 0.0))\n";
    let constant = run(source, "50.0");
    let literal = run(&source.replace("acc + dt", "acc + 0.5"), "50.0");
    assert_eq!(constant, literal, "constant reads add no boxes or dispatch allocations");
}

#[test]
fn constant_edits_match_clean_native_output() {
    let source = "module M\nlet point = (1.0, 2.0)\npublic run : Int -> Float\nlet run x =\n  let (a, b) = point\n  a + b + Int.toFloat x\n";
    let edited = source.replace("1.0", "3.0");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)], &[("M.fai", source)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("run");
            (
                fai_core::pretty_def(&fai_core::simplified(db, file, name)),
                fai_core::pretty_def(&fai_rc::rc(db, file, name)),
                (*fai_driver::object_code(db, file, name, false)).clone(),
            )
        },
    );
}
