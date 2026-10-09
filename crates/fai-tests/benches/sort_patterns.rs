//! Sorting distributions, with build/sort/checksum included and validation untimed.

use divan::Bencher;
use fai_db::Db;
use fai_tests::sorting::{CASES, Case};

fn main() {
    divan::main();
}

fn database(source: String) -> (fai_db::FaiDatabase, fai_db::SourceFile) {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("SortPatterns.fai".into(), source);
    let file = db.source_file(id).unwrap();
    (db, file)
}

mod jit {
    use super::*;

    #[divan::bench(args = CASES)]
    fn rust(bencher: Bencher, case: &Case) {
        bencher.bench(|| fai_tests::sorting::run(case.pattern, divan::black_box(case.n)));
    }

    #[divan::bench(args = CASES)]
    fn fai(bencher: Bencher, case: &Case) {
        let (db, file) = database(fai_tests::sorting::SOURCE.to_owned());
        let mut program = fai_driver::jit_compile(&db, file).unwrap_or_else(|d| panic!("{d:?}"));
        let closure = program.function(fai_syntax::Symbol::intern("run")).unwrap();
        let call = || {
            let result = fai_runtime::apply(
                closure,
                &[
                    fai_runtime::make_int(case.pattern as i64),
                    fai_runtime::make_int(divan::black_box(case.n) as i64),
                ],
            );
            let value = fai_runtime::read_int(result);
            fai_runtime::fai_drop(result);
            value
        };
        assert_eq!(call(), fai_tests::sorting::run(case.pattern, case.n));
        bencher.bench(call);
    }
}

#[cfg(not(windows))]
mod aot {
    use super::*;
    use fai_tests::sorting::PATTERNS;
    use std::process::Command;

    fn measure(bencher: Bencher, case: &Case, make: impl Fn() -> Command + Sync) {
        let expected = fai_tests::benchmark_process::ExpectedAnswer::Int(fai_tests::sorting::run(
            case.pattern,
            case.n,
        ));
        let first = fai_tests::benchmark_process::spawn_checked(&mut make()).unwrap();
        expected.verify(&case.to_string(), &first).unwrap();
        bencher.bench(|| fai_tests::benchmark_process::spawn_checked(&mut make()).unwrap());
    }

    #[divan::bench(args = CASES)]
    fn rust(bencher: Bencher, case: &Case) {
        measure(bencher, case, || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_algo-baseline"));
            command.args([PATTERNS[case.pattern], &case.n.to_string()]);
            command
        });
    }

    #[divan::bench(args = CASES)]
    fn ocaml(bencher: Bencher, case: &Case) {
        let Some(binary) = fai_tests::ocaml::baseline() else { return };
        measure(bencher, case, || {
            let mut command = Command::new(binary);
            command.args([PATTERNS[case.pattern], &case.n.to_string()]);
            command
        });
    }

    #[divan::bench(args = CASES)]
    fn fai(bencher: Bencher, case: &Case) {
        let directory = tempfile::tempdir().unwrap();
        let binary = camino::Utf8PathBuf::from_path_buf(directory.path().join("sort")).unwrap();
        let (db, file) = database(fai_tests::sorting::program(*case));
        let result = fai_driver::build_native(&db, file, &binary);
        assert!(result.artifact.is_some(), "{:?}", result.diagnostics);
        measure(bencher, case, || Command::new(&binary));
    }
}
