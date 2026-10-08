//! Rename validation preserves every reference's original binding.

use fai_db::{Db, DbSpanResolver, FaiDatabase, SourceFile};
use fai_ide::{RenameError, checked_rename_at};

fn workspace(sources: &[(&str, &str)]) -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let files = sources
        .iter()
        .map(|(path, text)| {
            let id = db.add_source((*path).into(), (*text).into());
            db.source_file(id).unwrap()
        })
        .collect();
    (db, files)
}

#[track_caller]
fn rejected(source: &str, selected: &str, new_name: &str, expected: RenameError) {
    let (db, files) = workspace(&[("Main.fai", source)]);
    let result = checked_rename_at(
        &db,
        &files,
        files[0],
        source.find(selected).unwrap() as u32,
        new_name,
        &DbSpanResolver::new(&db),
    );
    assert_eq!(result.err(), Some(expected));
    assert_eq!(files[0].text(&db), source, "validation must not edit live inputs");
}

#[track_caller]
fn invalid_name(name: &str) {
    rejected("module Main\nlet value = 1\n", "value", name, RenameError::InvalidName);
}

#[test]
fn rejects_let_keyword() {
    invalid_name("let");
}
#[test]
fn rejects_match_keyword() {
    invalid_name("match");
}
#[test]
fn rejects_foreign_keyword() {
    invalid_name("foreign");
}
#[test]
fn rejects_wildcard() {
    invalid_name("_");
}
#[test]
fn rejects_true_literal() {
    invalid_name("true");
}
#[test]
fn rejects_false_literal() {
    invalid_name("false");
}
#[test]
fn rejects_identifier_with_comment() {
    invalid_name("name(*comment*)");
}

#[test]
fn rejects_capture_by_an_inner_let() {
    rejected(
        "module Main\nlet f x =\n  let y = 1\n  x\n",
        "x =",
        "y",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_capture_by_a_lambda_parameter() {
    rejected(
        "module Main\nlet f x = (fun y -> x + y) 1\n",
        "x =",
        "y",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_capture_by_a_match_binder() {
    rejected(
        "module Main\nlet f x =\n  match Some 1 with\n  | Some y -> x + y\n  | None -> x\n",
        "x =",
        "y",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_capturing_an_unedited_global_reference() {
    rejected(
        "module Main\nlet outer = 41\nlet f x = x + outer\n",
        "x =",
        "outer",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_capture_of_a_renamed_definition_at_its_caller() {
    rejected(
        "module Main\nlet original x = x\nlet caller replacement = original 1\n",
        "original",
        "replacement",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_duplicate_top_level_definitions() {
    rejected(
        "module Main\nlet first = 1\nlet second = 2\n",
        "first",
        "second",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_duplicate_nested_definitions() {
    rejected(
        "module Main\nmodule Inner =\n  let first = 1\n  let second = 2\n",
        "first",
        "second",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_duplicate_constructors() {
    rejected(
        "module Main\ntype Choice = | First | Second\nlet value = First\n",
        "First\n",
        "Second",
        RenameError::ResolutionChanged,
    );
}

#[test]
fn rejects_changing_an_operator_to_an_identifier() {
    rejected(
        "module Main\nlet (%%) x y = x + y\nlet value = 1 %% 2\n",
        "%%",
        "combine",
        RenameError::NotRenameable,
    );
}

#[test]
fn rejects_renaming_a_punned_record_field_as_a_local() {
    rejected(
        "module Main\nlet f { value } = value\n",
        "value\n",
        "renamed",
        RenameError::NotRenameable,
    );
}

#[test]
fn permits_names_used_by_an_unrelated_function() {
    let source = "module Main\nlet f x = x + 1\nlet g value = value + 2\n";
    let (mut db, files) = workspace(&[("Main.fai", source)]);
    let edits = checked_rename_at(
        &db,
        &files,
        files[0],
        source.find("x =").unwrap() as u32,
        "value",
        &DbSpanResolver::new(&db),
    )
    .unwrap();
    assert_eq!(edits.len(), 2);
    assert_eq!(files[0].text(&db), source);
    let mut renamed = source.to_owned();
    for edit in edits.iter().rev() {
        renamed.replace_range(edit.span.byte_start as usize..edit.span.byte_end as usize, "value");
    }
    assert_eq!(renamed, "module Main\nlet f value = value + 1\nlet g value = value + 2\n");
    db.add_source("Main.fai".into(), renamed);
    let errors = fai_types::check_file::accumulated::<fai_db::Diag>(&db, files[0]);
    assert!(!errors.iter().any(|d| d.0.severity == fai_diagnostics::Severity::Error));
}

#[test]
fn permits_the_same_member_name_in_another_module() {
    let a = "module A\npublic first : Int\nlet first = 1\n";
    let b = "module B\nlet second = 2\nlet value = A.first + second\n";
    let (db, files) = workspace(&[("A.fai", a), ("B.fai", b)]);
    let edits = checked_rename_at(
        &db,
        &files,
        files[0],
        a.find("first").unwrap() as u32,
        "second",
        &DbSpanResolver::new(&db),
    )
    .unwrap();
    assert_eq!(edits.len(), 3);
}

#[test]
fn rejects_an_incomplete_cross_module_edit_set() {
    let a = "module A\npublic first : Int\nlet first = 1\n";
    let b = "module B\nlet value = A.first\n";
    let (db, files) = workspace(&[("A.fai", a), ("B.fai", b)]);
    let result = checked_rename_at(
        &db,
        &files[..1],
        files[0],
        a.find("first").unwrap() as u32,
        "renamed",
        &DbSpanResolver::new(&db),
    );
    assert_eq!(result.err(), Some(RenameError::ResolutionChanged));
}

#[test]
fn preserves_resolution_when_source_ids_have_tombstones() {
    let (mut db, _) = workspace(&[("Gone.fai", "module Gone\nlet gone = 0\n")]);
    let gone = db.all_source_files().into_iter().find(|file| file.path(&db) == "Gone.fai").unwrap();
    let source = "module Main\nlet f x = x + 1\nlet result = f 2\n";
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    db.remove_source(gone.source(&db));
    let edits = checked_rename_at(
        &db,
        &[file],
        file,
        source.find("f x").unwrap() as u32,
        "renamed",
        &DbSpanResolver::new(&db),
    )
    .unwrap();
    assert_eq!(edits.len(), 2);
}

#[test]
fn permits_an_explicit_record_field_binder() {
    let source = "module Main\nlet f { field = value } = value\n";
    let (db, files) = workspace(&[("Main.fai", source)]);
    let edits = checked_rename_at(
        &db,
        &files,
        files[0],
        source.find("value").unwrap() as u32,
        "renamed",
        &DbSpanResolver::new(&db),
    )
    .unwrap();
    assert_eq!(edits.len(), 2);
}

#[test]
fn rejects_as_pattern_ranges_that_would_delete_the_pattern() {
    rejected(
        "module Main\nlet f ((x, y) as pair) = pair\n",
        "pair\n",
        "renamed",
        RenameError::NotRenameable,
    );
}
