//! Strict validation and deterministic Markdown generation for tracking issues.

use std::path::Path;

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
    let [directory, output, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: performance-report catalog | SNAPSHOT OUTPUT [EVIDENCE_HTTPS_URL]".into()
        );
    };
    if rest.len() > 1 {
        return Err("too many arguments".into());
    }
    let snapshot = Snapshot::load(Path::new(directory))?;
    let reports = snapshot.render(rest.first().map_or("", String::as_str))?;
    let output = Path::new(output);
    std::fs::create_dir_all(output)?;
    for (name, text) in reports {
        std::fs::write(output.join(name), text)?;
    }
    std::fs::write(output.join("results.json"), serde_json::to_vec_pretty(&snapshot.to_json()?)?)?;
    println!("Validated and rendered snapshot {}", snapshot.manifest().run_id);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("performance-report: {error}");
        std::process::exit(1);
    }
}
