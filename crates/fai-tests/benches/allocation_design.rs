//! Optimized AOT allocation attribution using source-identical compiler controls.

fn main() {
    #[cfg(not(windows))]
    run();
}

#[cfg(not(windows))]
fn run() {
    use fai_db::{AllocationControl, Db};
    use fai_tests::allocation_design::{CASES, counters};
    use fai_tests::bench_summary::AllocationSample;
    use fai_tests::benchmark_aot::{Entry, checksum, fai_program_from};
    use fai_tests::benchmark_process::{ExpectedAnswer, spawn_checked};
    use fai_tests::native_peer::hash;
    use std::path::PathBuf;
    use std::process::Command;

    let mut selected = Vec::new();
    let mut inputs = Vec::new();
    let mut test = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bench" => {}
            "--test" => test = true,
            "--case" => selected.push(args.next().expect("case name")),
            "--input" => {
                inputs.push(args.next().expect("input").parse::<i64>().expect("integer input"))
            }
            "--list" => {
                for case in CASES {
                    println!("{} / {} / {:?}", case.name, case.control_name, case.inputs);
                }
                return;
            }
            "--help" | "-h" => {
                println!("allocation_design [--case NAME] [--input N] [--test] [--list]");
                return;
            }
            _ => panic!("unknown allocation-design argument: {arg}"),
        }
    }
    assert!(
        !cfg!(debug_assertions) || test,
        "allocation attribution uses optimized release builds"
    );
    assert!(
        selected.iter().all(|name| CASES.iter().any(|case| case.name == name)),
        "unknown allocation case"
    );
    assert!(
        inputs.iter().all(|&n| (0..=100000).contains(&n)),
        "allocation fixture input out of range"
    );
    let temporary = tempfile::Builder::new().prefix("fai-allocation-design-").tempdir().unwrap();
    let directory = std::env::var_os("FAI_BENCH_ARTIFACT_DIR")
        .map_or_else(|| temporary.path().to_path_buf(), PathBuf::from);
    let directory = std::env::current_dir().unwrap().join(directory);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("compiler-info.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "compiler": fai_driver::build_info(), "hashAlgorithm": "blake3", "allocationStats": true,
        "methodology": "allocation-ablation-v1",
    })).unwrap()).unwrap();
    for case in CASES
        .iter()
        .filter(|case| selected.is_empty() || selected.iter().any(|name| name == case.name))
    {
        let folder = directory.join(case.name);
        std::fs::create_dir_all(&folder).unwrap();
        let source = fai_program_from(case.source, "run", false, Entry::Memory);
        std::fs::write(folder.join("workload.fai"), &source).unwrap();
        let source_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        for (name, control) in
            [("enabled", AllocationControl::Enabled), (case.control_name, case.control)]
        {
            let mut db = fai_db::FaiDatabase::new();
            db.set_allocation_control(control);
            fai_types::std_lib::load_std(&mut db);
            let id = db.add_source(format!("{}.fai", case.name).into(), source.clone());
            let file = db.source_file(id).unwrap();
            let mut ir = String::new();
            for function in case
                .source
                .lines()
                .filter_map(|line| line.strip_prefix("let "))
                .filter_map(|line| line.split_whitespace().next())
            {
                let symbol = fai_syntax::Symbol::intern(function);
                ir.push_str(&format!("\n{name}: {function}\n"));
                ir.push_str(&fai_core::pretty_def(&fai_rc::rc_emit(&db, file, symbol)));
                for generated in &fai_core::fuse_def(&db, file, symbol).loops {
                    ir.push_str(&fai_core::pretty_def(&generated.lowered));
                }
            }
            std::fs::write(folder.join(format!("{name}.ir")), ir).unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(folder.join(name)).unwrap();
            let built = fai_driver::build_native(&db, file, &path);
            assert!(built.ok, "{} / {name}: {:?}", case.name, built.diagnostics);
            let executable_hash = hash(path.as_std_path());
            for &size in if inputs.is_empty() { case.inputs } else { &inputs } {
                let output = spawn_checked(
                    Command::new(&path)
                        .args([size.to_string(), "1".into(), "run".into()])
                        .env("FAI_REPORT_ALLOCATIONS", "1")
                        .env_remove("FAI_REPORT_RSS"),
                )
                .unwrap();
                checksum(&[ExpectedAnswer::Int((case.expected)(size))], 1)
                    .verify(case.name, &output)
                    .unwrap();
                let row = AllocationSample {
                    methodology: "allocation-ablation-v1".into(),
                    algorithm: case.name.into(),
                    control: name.into(),
                    source_hash: source_hash.clone(),
                    compiler_id: fai_driver::TOOL_BUILD_ID.into(),
                    executable_hash: executable_hash.clone(),
                    size,
                    iterations: 1,
                    counters: counters(&output.stderr).unwrap(),
                };
                if test {
                    println!("validated {} / {name} / n={size}", case.name);
                } else {
                    println!("{}", row.to_line());
                }
            }
        }
    }
}
