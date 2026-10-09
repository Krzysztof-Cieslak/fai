//! Table growth retains existing entry cells and immutable snapshots.

use std::collections::BTreeMap;
use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module M
public dict : Unit -> HashDict Int Int
let dict u = HashDict.fromList (List.map (fun k -> (k, k * 10)) (List.range 0 6))
public set : Unit -> HashSet Int
let set u = HashSet.fromList (List.range 0 6)
public addDict : HashDict Int Int -> HashDict Int Int
let addDict d = HashDict.insert 6 60 d
public addSet : HashSet Int -> HashSet Int
let addSet s = HashSet.insert 6 s
public main : Runtime -> Unit
let main r = ()
"#;

fn entries(table: rt::Value) -> BTreeMap<i64, rt::Value> {
    let slots = rt::fai_data_field(table, 1);
    let length = rt::fai_array_length_borrowed(slots);
    let count = rt::read_int(length);
    rt::fai_drop(length);
    let mut entries = BTreeMap::new();
    for i in 0..count {
        let entry = rt::fai_array_get_borrowed(slots, rt::make_int(i));
        if rt::read_int(rt::fai_data_tag(entry)) == 1 {
            let key = rt::fai_data_field(entry, 0);
            entries.insert(rt::read_int(key), entry);
            rt::fai_drop(key);
        } else {
            rt::fai_drop(entry);
        }
    }
    rt::fai_drop(slots);
    entries
}

#[track_caller]
fn grows_in_place(builder: &str, insert: &str, shared: bool) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), SOURCE.into());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
    let baseline = rt::live_count();
    let build = program.function(Symbol::intern(builder)).unwrap();
    let insert = program.function(Symbol::intern(insert)).unwrap();
    let table = rt::apply(build, &[rt::FAI_UNIT]);
    let original = shared.then(|| rt::fai_dup(table));
    let old_entries = entries(table);
    assert_eq!(old_entries.len(), 6);
    rt::reset_allocations();
    let grown = rt::apply(insert, &[table]);
    assert!(rt::allocations() <= 4, "growth allocated {} cells", rt::allocations());
    let new_entries = entries(grown);
    assert_eq!(new_entries.len(), 7);
    let retained: BTreeMap<_, _> = new_entries
        .iter()
        .filter(|(key, _)| **key < 6)
        .map(|(&key, &value)| (key, value))
        .collect();
    assert_eq!(retained, old_entries, "rehashing only moves the bucket reference");
    if let Some(original) = original {
        let snapshot = entries(original);
        assert_eq!(snapshot, old_entries);
        snapshot.values().for_each(|&entry| rt::fai_drop(entry));
        rt::fai_drop(original);
    }
    old_entries.values().chain(new_entries.values()).for_each(|&entry| rt::fai_drop(entry));
    rt::fai_drop(grown);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn unique_dictionary_growth_reuses_entry_cells() {
    grows_in_place("dict", "addDict", false);
}

#[test]
fn shared_dictionary_growth_retains_snapshot_entry_cells() {
    grows_in_place("dict", "addDict", true);
}

#[test]
fn unique_set_growth_reuses_entry_cells() {
    grows_in_place("set", "addSet", false);
}

#[test]
fn shared_set_growth_retains_snapshot_entry_cells() {
    grows_in_place("set", "addSet", true);
}
