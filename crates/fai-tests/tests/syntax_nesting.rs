//! Accepted syntax depths survive downstream consumers; excess nesting stays incremental.

use fai_db::{Db, Diag};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

#[track_caller]
fn downstream(case: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "downstream_worker", "--nocapture"])
        .env("FAI_DEPTH_CHECK", case)
        .env("RUST_MIN_STACK", "2097152")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(Duration::from_secs(30)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "{case}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn downstream_worker() {
    let Ok(case) = std::env::var("FAI_DEPTH_CHECK") else {
        return;
    };
    let body = match case.as_str() {
        "parens" => format!("let value = {}1{}", "(".repeat(126), ")".repeat(126)),
        "infix" => format!("let value = {}0", "1 + ".repeat(511)),
        "arrows" => format!("type Function = {}Int", "Int -> ".repeat(126)),
        "modules" => format!("{}let value = 0", "module Inner = ".repeat(120)),
        _ => panic!("unknown downstream case"),
    };
    let text = format!("module Main\n{body}\n");
    let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), &text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let formatted = fai_fmt::format(&parsed.module, &parsed.comments, &text);
    let reparsed = fai_syntax::parse_module(fai_span::SourceId::new(0), &formatted);
    assert!(reparsed.diagnostics.is_empty(), "{:?}", reparsed.diagnostics);
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), text);
    let diagnostics = fai_types::check_file::accumulated::<Diag>(&db, db.source_file(id).unwrap());
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn accepted_parentheses_survive_formatter_and_types() {
    downstream("parens");
}
#[test]
fn accepted_operator_trees_survive_formatter_and_types() {
    downstream("infix");
}
#[test]
fn accepted_arrow_types_survive_formatter_and_types() {
    downstream("arrows");
}
#[test]
fn accepted_nested_modules_survive_formatter_and_types() {
    downstream("modules");
}

#[test]
fn nesting_edits_match_clean_diagnostics() {
    let small = "module M\nlet value = 1\n";
    let deep = format!("module M\nlet value = {}1{}\n", "(".repeat(1_000), ")".repeat(1_000));
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", small)], &[("M.fai", &deep)], &[("M.fai", small)]],
        |db, ids| {
            fai_syntax::parse::accumulated::<Diag>(db, db.source_file(ids[0]).unwrap())
                .iter()
                .map(|d| (d.0.code, d.0.primary.range()))
                .collect::<Vec<_>>()
        },
    );
}

#[test]
fn an_edit_inside_rejected_nesting_preserves_the_item_tree_firewall() {
    let mut db = fai_db::FaiDatabase::new();
    let text = format!(
        "module M\nlet value = {}1{}\npublic answer : Int\nlet answer = 42\n",
        "(".repeat(1_000),
        ")".repeat(1_000)
    );
    let id = db.add_source("M.fai".into(), text.clone());
    let file = db.source_file(id).unwrap();
    assert_eq!(fai_syntax::public_item_count(&db, file), 1);
    db.enable_event_log();
    db.add_source("M.fai".into(), text.replace('1', "2"));
    assert_eq!(fai_syntax::public_item_count(&db, file), 1);
    let events = db.take_events();
    assert!(events.iter().any(|event| event.contains("parse")), "{events:?}");
    assert!(!events.iter().any(|event| event.contains("public_item_count")), "{events:?}");
}
