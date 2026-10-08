//! URL component decoding over Unicode and malformed percent escapes.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::{CompiledProgram, jit_compile};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn compile() -> CompiledProgram {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), "module Main\npublic encode : String -> String\nlet encode s = Url.encodeComponent s\npublic decode : String -> String\nlet decode s = Url.decodeComponent s\npublic main : Runtime -> Unit\nlet main r = ()\n".into());
    let file = db.source_file(id).unwrap();
    jit_compile(&db, file).unwrap_or_else(|diagnostics| panic!("{diagnostics:?}"))
}

fn call(program: &mut CompiledProgram, name: &str, text: &str) -> String {
    let baseline = rt::live_count();
    let function = program.function(Symbol::intern(name)).unwrap();
    let result = rt::apply(rt::fai_dup(function), &[rt::make_str(text)]);
    let text = String::from_utf8(rt::read_string(result)).unwrap();
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
    text
}

#[track_caller]
fn decoded(input: &str, expected: &str) {
    let _guard = lock();
    assert_eq!(call(&mut compile(), "decode", input), expected);
}

#[test]
fn unescaped_unicode_is_preserved() {
    decoded("café 😀", "café 😀");
}

#[test]
fn invalid_hex_is_preserved_literally() {
    decoded("%GG%2G", "%GG%2G");
}

#[test]
fn a_lone_percent_is_literal() {
    decoded("%", "%");
}
#[test]
fn a_truncated_escape_is_literal() {
    decoded("%A", "%A");
}
#[test]
fn escaped_unicode_decodes_all_bytes() {
    decoded("%C3%A9%F0%9F%98%80", "é😀");
}
#[test]
fn escaped_and_literal_unicode_mix() {
    decoded("café%20😀", "café 😀");
}
#[test]
fn invalid_decoded_utf8_preserves_the_complete_component() {
    decoded("%41%FF", "%41%FF");
}
#[test]
fn plus_is_not_reinterpreted_as_form_space() {
    decoded("a+b", "a+b");
}
#[test]
fn a_percent_encoded_nul_is_valid() {
    decoded("a%00b", "a\0b");
}
#[test]
fn lowercase_hex_is_accepted() {
    decoded("%c3%a9", "é");
}

mod proptests {
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    use super::*;

    #[test]
    fn arbitrary_unicode_round_trips_through_percent_encoding() {
        let _guard = lock();
        let mut program = compile();
        let program = std::cell::RefCell::new(&mut program);
        let strings = proptest::collection::vec(any::<char>(), 0..64)
            .prop_map(|chars| chars.into_iter().collect::<String>());
        TestRunner::default()
            .run(&strings, |text| {
                let mut program = program.borrow_mut();
                let encoded = call(&mut program, "encode", &text);
                let decoded = call(&mut program, "decode", &encoded);
                prop_assert_eq!(decoded, text);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn unescaped_unicode_is_an_identity() {
        let _guard = lock();
        let mut program = compile();
        let program = std::cell::RefCell::new(&mut program);
        let strings = proptest::collection::vec(any::<char>(), 0..64)
            .prop_map(|chars| chars.into_iter().filter(|c| *c != '%').collect::<String>());
        TestRunner::default()
            .run(&strings, |text| {
                prop_assert_eq!(call(&mut program.borrow_mut(), "decode", &text), text);
                Ok(())
            })
            .unwrap();
    }
}
