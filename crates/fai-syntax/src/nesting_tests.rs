//! Deep syntax is rejected in the parser without overflowing the host stack.

use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

fn input(case: &str, depth: usize) -> String {
    let body = match case {
        "parentheses" | "truncated" => format!(
            "let value = {}1{}",
            "(".repeat(depth),
            if case == "truncated" { String::new() } else { ")".repeat(depth) }
        ),
        "prefix" => format!("let value = {}1", "- ".repeat(depth)),
        "if" => format!("let value = {}0", "if true then 0 else ".repeat(depth)),
        "lambda" => format!("let value = {}0", "fun x -> ".repeat(depth)),
        "pattern" => format!("let f {}x{} = x", "(".repeat(depth), ")".repeat(depth)),
        "pattern-list" => format!("let f {}x{} = x", "[".repeat(depth), "]".repeat(depth)),
        "cons" => format!("let f ({}[]) = 0", "x :: ".repeat(depth)),
        "arrow" => format!("value : {}Int", "Int -> ".repeat(depth)),
        "type" => format!("value : {}Int{}", "(".repeat(depth), ")".repeat(depth)),
        "module" => format!("{}let value = 0", "module Inner = ".repeat(depth)),
        "infix" => format!("let value = {}0", "1 + ".repeat(depth)),
        "application" => format!("let value = {}0", "f ".repeat(depth)),
        "field" => format!("let value = {}x", "x.".repeat(depth)),
        "type-application" => format!("value : {}Int", "T ".repeat(depth)),
        _ => panic!("unknown nesting case"),
    };
    format!("module Main\n// é🌍\n{body}\nlet recovered = 1\n")
}

#[track_caller]
fn rejects(case: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "nesting_tests::nesting_worker", "--nocapture"])
        .env("FAI_NESTING_CASE", case)
        .env("RUST_MIN_STACK", "2097152")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(Duration::from_secs(20)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "{case}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn nesting_worker() {
    let Ok(case) = std::env::var("FAI_NESTING_CASE") else {
        return;
    };
    let text = input(&case, 10_000);
    let parsed = crate::parse_module(fai_span::SourceId::new(0), &text);
    let errors: Vec<_> =
        parsed.diagnostics.iter().filter(|d| d.code.as_str() == "FAI1023").collect();
    assert_eq!(errors.len(), 1, "expected one nesting diagnostic");
    let span = errors[0].primary.range();
    assert!(span.end().to_usize() <= text.len());
    assert!(text.is_char_boundary(span.start().to_usize()));
    assert!(text.is_char_boundary(span.end().to_usize()));
    if case != "truncated" {
        assert!(parsed.module.roots.iter().any(|id| matches!(parsed.module.items[id.index()].kind, crate::ast::ItemKind::Binding { name, .. } if name.as_str() == "recovered")));
    }
}

#[test]
fn recursive_nesting_boundary_is_accepted() {
    let text = input("parentheses", 126);
    assert!(crate::parse_module(fai_span::SourceId::new(0), &text).diagnostics.is_empty());
}

#[test]
fn the_first_excessive_layer_has_an_exact_utf8_safe_location() {
    let text = input("parentheses", 127);
    let parsed = crate::parse_module(fai_span::SourceId::new(0), &text);
    assert_eq!(parsed.diagnostics.len(), 1);
    let diagnostic = &parsed.diagnostics[0];
    assert_eq!(diagnostic.code, crate::NESTING_LIMIT);
    assert_eq!(diagnostic.message, "syntax nesting exceeds 128 recursive grammar levels");
    let offset = text.find('1').unwrap();
    assert_eq!(diagnostic.primary.start().to_usize(), offset);
    assert_eq!(diagnostic.primary.end().to_usize(), offset + 1);
}

#[test]
fn iterative_tree_depth_boundary_is_accepted() {
    let text = input("infix", 255);
    assert!(crate::parse_module(fai_span::SourceId::new(0), &text).diagnostics.is_empty());
}

#[test]
fn iterative_tree_depth_boundary_reports_one_error() {
    let text = input("infix", 256);
    let parsed = crate::parse_module(fai_span::SourceId::new(0), &text);
    assert_eq!(parsed.diagnostics.len(), 1);
    assert_eq!(parsed.diagnostics[0].code, crate::NESTING_LIMIT);
}

mod proptests {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn mixed_nesting_recovers_at_the_next_declaration(wrappers in proptest::collection::vec(0u8..4, 0..256)) {
            let mut expression = "1".to_owned();
            for wrapper in &wrappers {
                expression = match wrapper {
                    0 => format!("({expression})"),
                    1 => format!("[{expression}]"),
                    2 => format!("{{ x = {expression} }}"),
                    _ => format!("if true then {expression} else 0"),
                };
            }
            let text = format!("module M\n// é🌍\nlet value = {expression}\nlet recovered = 1\n");
            let parsed = crate::parse_module(fai_span::SourceId::new(0), &text);
            prop_assert_eq!(parsed.diagnostics.len(), usize::from(wrappers.len() > 126));
            prop_assert!(parsed.module.roots.iter().any(|id| matches!(parsed.module.items[id.index()].kind, crate::ast::ItemKind::Binding { name, .. } if name.as_str() == "recovered")), "the following declaration must survive recovery");
        }
    }
}

#[test]
fn deeply_nested_parentheses_are_rejected() {
    rejects("parentheses");
}
#[test]
fn truncated_deep_syntax_is_rejected() {
    rejects("truncated");
}
#[test]
fn deeply_nested_prefixes_are_rejected() {
    rejects("prefix");
}
#[test]
fn deeply_nested_if_branches_are_rejected() {
    rejects("if");
}
#[test]
fn deeply_nested_lambdas_are_rejected() {
    rejects("lambda");
}
#[test]
fn deeply_nested_patterns_are_rejected() {
    rejects("pattern");
}
#[test]
fn deeply_nested_list_patterns_are_rejected() {
    rejects("pattern-list");
}
#[test]
fn deeply_nested_cons_patterns_are_rejected() {
    rejects("cons");
}
#[test]
fn deeply_nested_arrows_are_rejected() {
    rejects("arrow");
}
#[test]
fn deeply_nested_types_are_rejected() {
    rejects("type");
}
#[test]
fn deeply_nested_modules_are_rejected() {
    rejects("module");
}
#[test]
fn deep_left_associated_infix_trees_are_rejected() {
    rejects("infix");
}
#[test]
fn deep_application_trees_are_rejected() {
    rejects("application");
}
#[test]
fn deep_field_trees_are_rejected() {
    rejects("field");
}
#[test]
fn deep_type_application_trees_are_rejected() {
    rejects("type-application");
}
