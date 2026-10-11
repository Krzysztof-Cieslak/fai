//! Source-identical controls must change allocation behavior without changing answers.

#![cfg(any(debug_assertions, feature = "allocation-stats"))]

use std::sync::Mutex;

use fai_db::{AllocationControl, Db};
use fai_runtime as rt;
use fai_syntax::Symbol;
use fai_tests::allocation_design::{CASES, Case};

static LOCK: Mutex<()> = Mutex::new(());

fn measure(
    db: &fai_db::FaiDatabase,
    file: fai_db::SourceFile,
    case: &Case,
    input: i64,
) -> (i64, i64) {
    let mut program = fai_driver::jit_compile(db, file).unwrap();
    let function = program.function(Symbol::intern("run")).unwrap();
    let baseline = rt::live_bytes();
    rt::reset_allocations();
    let answer = rt::apply(function, &[rt::make_int(input)]);
    assert_eq!(rt::read_int(answer), (case.expected)(input));
    rt::fai_drop(answer);
    assert_eq!(rt::live_bytes(), baseline, "controlled compilation must release all values");
    (rt::allocations(), rt::peak_live_bytes() - baseline)
}

fn compare(name: &str) -> ((i64, i64), (i64, i64)) {
    let _guard = LOCK.lock().unwrap();
    let case = CASES.iter().find(|case| case.name == name).unwrap();
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = format!("{}\npublic main : Runtime -> Unit\nlet main runtime = ()\n", case.source);
    let id = db.add_source("Controlled.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let before = measure(&db, file, case, 8);
    db.set_allocation_control(case.control);
    let disabled = measure(&db, file, case, 8);
    db.set_allocation_control(AllocationControl::Enabled);
    let restored = measure(&db, file, case, 8);
    assert_eq!(before, restored, "changing controls must invalidate and restore cached lowering");
    (before, disabled)
}

#[test]
fn disabling_fusion_materializes_the_same_source_pipeline() {
    let (enabled, disabled) = compare("Fusion");
    assert_eq!(enabled.0, 0);
    assert!(disabled.0 > enabled.0 && disabled.1 > enabled.1);
}

#[test]
fn disabling_reuse_allocates_for_each_unique_list_rebuild() {
    let (enabled, disabled) = compare("UniqueList");
    assert!(disabled.0 > enabled.0 * 4, "{enabled:?} versus {disabled:?}");
}

#[test]
fn shared_list_control_preserves_the_retained_snapshot() {
    let (enabled, disabled) = compare("SharedList");
    assert!(disabled.0 > enabled.0, "{enabled:?} versus {disabled:?}");
}

#[test]
fn disabling_scalar_replacement_restores_aggregate_storage() {
    let (enabled, disabled) = compare("Scalars");
    assert_eq!(enabled.0, 0);
    assert!(disabled.0 > 0 && disabled.1 > 0);
}
