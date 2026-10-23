//! Print experimental root-locality diagnostics for an emitted program bundle.
//!
//! Usage: `cargo run -p fai-tests --example locality_report -- ROOT ENTRY`.
//! No code is executed or optimized. Package preparation is locked and offline.

use std::error::Error;
use std::io::{self, Write};

use camino::{Utf8Path, Utf8PathBuf};
use fai_core::wire::from_wire;
use fai_driver::{ProjectOptions, Session};
use fai_rc::{analyze_locality, analyze_program_locality};
use serde_json::json;

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [root, entry] = args.as_slice() else {
        return Err("usage: locality_report ROOT ENTRY".into());
    };
    let session = Session::open_with_options(
        Utf8PathBuf::from(root),
        ProjectOptions { locked: true, offline: true, ..ProjectOptions::default() },
    )?;
    let file = session.entry_file(Utf8Path::new(entry))?;
    let result = fai_driver::build_run_bundle(session.db(), file);
    let bundle =
        result.bundle.ok_or_else(|| format!("cannot lower program: {:?}", result.diagnostics))?;
    let bytes = serde_json::to_vec(&bundle)?;
    let fingerprint = blake3::hash(&bytes).to_hex().to_string();
    let rebuilt = from_wire(&bundle);
    let program = analyze_program_locality(&rebuilt.defs);
    let mut definitions = Vec::new();
    for (index, def) in rebuilt.defs.iter().enumerate() {
        let before = def.clone();
        let report = &program.definitions[index];
        let baseline = analyze_locality(def);
        assert_eq!(*def, before, "diagnostic analysis changed its input");
        let functions: Vec<_> = report.functions.iter().map(|function| {
            let sites: Vec<_> = function.sites.iter().map(|site| json!({
                "node": site.node,
                "operation": format!("{:?}", site.operation),
                "local": site.local.map(|local| local.index()),
                "fields": site.fields,
                "reason": format!("{:?}", site.reason),
                "baselineReason": baseline.functions.iter().find(|f| f.function == function.function)
                    .and_then(|f| f.sites.iter().find(|s| s.node == site.node)).map(|s| format!("{:?}", s.reason)),
            })).collect();
            json!({"function":function.function,"nodes":function.nodes,"limited":function.limited,"sites":sites})
        }).collect();
        definitions.push(json!({
            "module": rebuilt.module_labels.get(&def.def.file),
            "name": def.def.name.as_str(),
            "functions": functions,
            "summaries": program.summaries.iter().filter(|s| s.definition == index).map(|s| json!({
                "function": s.function, "captures": s.captures, "mayPublish": s.may_publish,
                "result": format!("{:?}", s.result), "limited": s.limited,
            })).collect::<Vec<_>>(),
        }));
    }
    assert_eq!(bytes, serde_json::to_vec(&bundle)?, "analysis changed the wire input");
    let output = json!({
        "schemaVersion": 2,
        "kind": "experimental-locality-diagnostic",
        "compiler": fai_driver::build_info(),
        "concurrent": bundle.concurrent,
        "bundleFingerprint": fingerprint,
        "inputUnchanged": true,
        "definitions": definitions,
        "summaryRounds": program.rounds,
        "summariesConverged": program.converged,
        "limitations": [
            "Root locality is not uniqueness or descendant locality.",
            "Explicit ownership IR sites are not counts of runtime mutations.",
            "Unknown calls, primitives, representation-changing projections and loop inputs remain conservative.",
            "No native optimization consumes this diagnostic report."
        ],
    });
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    serde_json::to_writer_pretty(&mut stdout, &output)?;
    writeln!(stdout)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("locality_report: {error}");
        std::process::exit(1);
    }
}
