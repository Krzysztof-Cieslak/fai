//! Matched binary nodes and explicit BTreeMap/Map alternatives, by measured phase.

use divan::Bencher;
use fai_db::Db;
use fai_runtime as rt;
use fai_syntax::Symbol;
use fai_tests::tree_lookup::{self as tree, BinaryTree, KEYS, QUERIES};

#[path = "support/tree_fixture.rs"]
mod fixture;

fn main() {
    fixture::validate(KEYS);
    divan::main();
}

fn database(source: String) -> (fai_db::FaiDatabase, fai_db::SourceFile) {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("OptionTreeFind.fai".into(), source);
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn fai(bencher: Bencher, n: i64, build: bool) {
    let (db, file) = database(tree::fai_source());
    let mut program = fai_driver::jit_compile(&db, file).unwrap_or_else(|d| panic!("{d:?}"));
    let make = program.function(Symbol::intern("make")).unwrap();
    let held = (!build).then(|| rt::apply(make, &[rt::make_int(KEYS)]));
    let f = program.function(Symbol::intern(if build { "buildProbe" } else { "probe" })).unwrap();
    let call = || {
        let n = rt::make_int(divan::black_box(n));
        let result = match held {
            Some(held) => rt::apply(f, &[rt::fai_dup(held), n]),
            None => rt::apply(f, &[n]),
        };
        let value = rt::read_int(result);
        rt::fai_drop(result);
        value
    };
    assert_eq!(call(), tree::expected(n));
    bencher.bench(call);
    if let Some(held) = held {
        rt::fai_drop(held);
    }
}

mod in_process_build_lookup {
    use super::*;
    #[divan::bench(args = QUERIES)]
    fn fai_binary(bencher: Bencher, n: i64) {
        fai(bencher, n, true);
    }
    #[divan::bench(args = QUERIES)]
    fn rust_binary(bencher: Bencher, n: i64) {
        bencher.bench(|| BinaryTree::build(KEYS).checksum(divan::black_box(n)));
    }
    #[divan::bench(args = QUERIES)]
    fn rust_btree_map(bencher: Bencher, n: i64) {
        bencher.bench(|| {
            let map = tree::btree();
            tree::checksum(divan::black_box(n), |key| map.get(&key).copied())
        });
    }
}

mod in_process_lookup_only {
    use super::*;
    #[divan::bench(args = QUERIES)]
    fn fai_binary(bencher: Bencher, n: i64) {
        fai(bencher, n, false);
    }
    #[divan::bench(args = QUERIES)]
    fn rust_binary(bencher: Bencher, n: i64) {
        let tree = BinaryTree::build(KEYS);
        bencher.bench(|| tree.checksum(divan::black_box(n)));
    }
    #[divan::bench(args = QUERIES)]
    fn rust_btree_map(bencher: Bencher, n: i64) {
        let map = tree::btree();
        bencher.bench(|| tree::checksum(divan::black_box(n), |key| map.get(&key).copied()));
    }
}

#[cfg(not(windows))]
mod native {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;
    use std::time::Duration;
    use wait_timeout::ChildExt;

    struct Worker {
        child: Child,
        input: Option<ChildStdin>,
        lines: Receiver<String>,
        reader: Option<JoinHandle<()>>,
    }

    impl Worker {
        fn start(mut command: Command) -> Self {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let input = child.stdin.take();
            let output = child.stdout.take().unwrap();
            let (send, lines) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    if send.send(line).is_err() {
                        break;
                    }
                }
            });
            let worker = Self { child, input, lines, reader: Some(reader) };
            assert_eq!(worker.lines.recv_timeout(Duration::from_secs(30)).unwrap(), "ready");
            worker
        }

        fn query(&mut self, n: i64) -> i64 {
            let input = self.input.as_mut().unwrap();
            writeln!(input, "{n}").unwrap();
            input.flush().unwrap();
            self.lines.recv_timeout(Duration::from_secs(30)).unwrap().parse().unwrap()
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            self.input.take();
            if self.child.wait_timeout(Duration::from_secs(2)).ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn measure(bencher: Bencher, n: i64, backend: &str, build: bool) {
        let directory = tempfile::tempdir().unwrap();
        let mode = if build { "build" } else { "lookup" };
        let command = match backend {
            "fai" => {
                let path =
                    camino::Utf8PathBuf::from_path_buf(directory.path().join("tree")).unwrap();
                let (db, file) = database(tree::fai_worker(build));
                let built = fai_driver::build_native(&db, file, &path);
                assert!(built.ok, "{:?}", built.diagnostics);
                Command::new(built.artifact.unwrap())
            }
            "rust_binary" | "rust_btree" => {
                let mut command = Command::new(env!("CARGO_BIN_EXE_tree-baseline"));
                command.args([if backend == "rust_binary" { "binary" } else { "btree" }, mode]);
                command
            }
            _ => {
                let Some(path) = fai_tests::ocaml::tree_baseline() else { return };
                let mut command = Command::new(path);
                command.args([if backend == "ocaml_binary" { "binary" } else { "map" }, mode]);
                command
            }
        };
        let mut worker = Worker::start(command);
        let expected = tree::expected(n);
        assert_eq!(worker.query(n), expected);
        bencher.bench_local(|| assert_eq!(worker.query(divan::black_box(n)), expected));
    }

    macro_rules! cases {
        ($module:ident, $build:literal) => {
            mod $module {
                use super::*;
                #[divan::bench(args = QUERIES)]
                fn fai_binary(b: Bencher, n: i64) {
                    measure(b, n, "fai", $build);
                }
                #[divan::bench(args = QUERIES)]
                fn rust_binary(b: Bencher, n: i64) {
                    measure(b, n, "rust_binary", $build);
                }
                #[divan::bench(args = QUERIES)]
                fn rust_btree_map(b: Bencher, n: i64) {
                    measure(b, n, "rust_btree", $build);
                }
                #[divan::bench(args = QUERIES)]
                fn ocaml_binary(b: Bencher, n: i64) {
                    measure(b, n, "ocaml_binary", $build);
                }
                #[divan::bench(args = QUERIES)]
                fn ocaml_map(b: Bencher, n: i64) {
                    measure(b, n, "ocaml_map", $build);
                }
            }
        };
    }
    cases!(build_lookup, true);
    cases!(lookup_only, false);
}
