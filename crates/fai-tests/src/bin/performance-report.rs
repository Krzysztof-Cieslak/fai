//! Strict evidence validation and presentation-independent report-data export.

use std::path::Path;

use sha2::{Digest, Sha256};

use fai_tests::bench_summary::tracking::{Snapshot, catalog, fingerprint};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() == ["catalog"] {
        println!("{}", serde_json::to_string_pretty(&catalog())?);
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "fingerprint") {
        let [_, path, rest @ ..] = args.as_slice() else {
            return Err("fingerprint needs a file".into());
        };
        if rest.len() > 1 || rest.first().is_some_and(|flag| flag != "--gzip") {
            return Err("usage: performance-report fingerprint FILE [--gzip]".into());
        }
        println!("{}", fingerprint(Path::new(path), !rest.is_empty())?);
        return Ok(());
    }
    let [command, directory, output] = args.as_slice() else {
        return Err("usage: performance-report catalog | export SNAPSHOT OUTPUT".into());
    };
    if command != "export" {
        return Err("expected export SNAPSHOT OUTPUT".into());
    }
    let snapshot = Snapshot::load(Path::new(directory))?;
    let mut data = snapshot.report_data();
    data["snapshotSha256"] =
        format!("{:x}", Sha256::digest(std::fs::read(Path::new(directory).join("snapshot.json"))?))
            .into();
    let output = Path::new(output);
    std::fs::create_dir_all(output)?;
    std::fs::write(output.join("report-data.json"), serde_json::to_vec_pretty(&data)?)?;
    std::fs::write(output.join("results.json"), serde_json::to_vec_pretty(&snapshot.to_json()?)?)?;
    println!("Validated and exported snapshot {}", snapshot.manifest().run_id);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("performance-report: {error}");
        std::process::exit(1);
    }
}
