//! Untimed fixture validation in a separate database, preserving cold measurements.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};

use fai_db::{Db, FaiDatabase, SourceFile};
use fai_diagnostics::Severity;

type Key = (Vec<(String, String)>, Vec<String>);
static VALIDATED: OnceLock<Mutex<HashSet<Key>>> = OnceLock::new();

/// Supported successful-inference depths (below the parser's nesting budget).
pub const IF_DEPTHS: &[usize] = &[20, 60, 100];
/// Supported arithmetic-chain lengths, with two terms per generated item.
pub const ARITHMETIC_LENGTHS: &[usize] = &[20, 60, 120];

/// Small valid program shared by backend and daemon benchmarks.
pub const SMALL_PROGRAM: &str = "module M\n\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (1 + 2 * 3))\n";
/// A helper-chain program shared by backend and daemon benchmarks.
pub const MEDIUM_PROGRAM: &str = "module M\n\nlet inc x = x + 1\n\nlet double x = x + x\n\nlet apply f x = f x\n\nlet step x = double (inc x)\n\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (apply step (step 10)))\n";

/// An exported structural record and functions over its fields.
pub fn record_source(fields: usize) -> String {
    let labels: Vec<_> = (0..fields).map(|i| format!("f{i}")).collect();
    let decl = labels.iter().map(|l| format!("{l} : Int")).collect::<Vec<_>>().join(", ");
    let values = labels.iter().map(|l| format!("{l} = 0")).collect::<Vec<_>>().join(", ");
    let sum = labels.iter().map(|l| format!("r.{l}")).collect::<Vec<_>>().join(" + ");
    format!(
        "module M\npublic type R = {{ {decl} }}\npublic mk : R\nlet mk = {{ {values} }}\npublic total : R -> Int\nlet total r = {sum}\n"
    )
}

/// A public union and exhaustive matcher.
pub fn union_source(constructors: usize) -> String {
    let variants =
        (0..constructors).map(|i| format!("  | C{i} Int")).collect::<Vec<_>>().join("\n");
    let arms =
        (0..constructors).map(|i| format!("  | C{i} x -> x + {i}")).collect::<Vec<_>>().join("\n");
    format!(
        "module M\npublic type T =\n{variants}\npublic eval : T -> Int\nlet eval t =\n  match t with\n{arms}\n"
    )
}

/// An effect-annotated capability forwarded through a helper chain.
pub fn capability_source(depth: usize) -> String {
    assert!(depth > 0);
    let mut source = String::from("module M\n");
    for i in 0..depth {
        let body = if i == 0 {
            "env.console.writeLine \"deep\"".to_owned()
        } else {
            format!("helper{} env", i - 1)
        };
        source.push_str(&format!("helper{i} : {{ console : Console | 'r }} -> Unit / {{ Console }}\nlet helper{i} env = {body}\n"));
    }
    source.push_str(&format!(
        "public main : Runtime -> Unit / {{ Console }}\nlet main r = helper{} r\n",
        depth - 1
    ));
    source
}

/// Generates the decision-tree fixture shared by benchmarks and validation tests.
pub fn if_chain(n: usize) -> String {
    let mut body = String::new();
    for i in 0..n {
        body.push_str(&format!("if x = {i} then {i} else "));
    }
    format!("module M\nlet f x = {body}x\n")
}

/// Generates the arithmetic fixture shared by benchmarks and validation tests.
pub fn arithmetic_chain(n: usize) -> String {
    let terms = (0..n).map(|i| format!("x + {i}")).collect::<Vec<_>>().join(" + ");
    format!("module M\nlet f x = {terms}\n")
}

/// Validates a source set once per process, accepting exactly the named error
/// codes (empty for a valid fixture). This never queries the measured database.
#[track_caller]
pub fn validate_sources(sources: &[(String, String)], expected: &[&str]) {
    let expected: BTreeSet<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    let key = (sources.to_vec(), expected.iter().cloned().collect());
    let cache = VALIDATED.get_or_init(Mutex::default);
    if cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&key) {
        return;
    }
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let files: Vec<_> = sources
        .iter()
        .map(|(path, text)| {
            let id = db.add_source(path.clone().into(), text.clone());
            db.source_file(id).unwrap()
        })
        .collect();
    let diagnostics: Vec<_> = files
        .iter()
        .flat_map(|f| crate::check_source_diagnostics(&db, *f))
        .filter(|d| d.severity == Severity::Error)
        .collect();
    let actual: BTreeSet<_> = diagnostics.iter().map(|d| d.code.as_str().to_owned()).collect();
    assert_eq!(
        actual,
        expected,
        "benchmark fixture has unexpected diagnostics: {:?}",
        diagnostics.iter().take(10).collect::<Vec<_>>()
    );
    cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key);
}

/// Checks a measured database's user inputs in a separate, disposable database.
#[track_caller]
pub fn validate_db(db: &dyn Db, files: &[SourceFile], expected: &[&str]) {
    let sources: Vec<_> = files.iter().map(|f| (f.path(db).clone(), f.text(db).clone())).collect();
    validate_sources(&sources, expected);
}
