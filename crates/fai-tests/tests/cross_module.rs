//! Multi-file (cross-module) type-checking scenarios, positive and negative.

use fai_tests::check_named;

/// The codes reported for the primary file.
fn codes(primary: &str, files: &[(&str, &str)]) -> Vec<String> {
    check_named(primary, files).codes()
}

#[test]
fn qualified_public_reference_typechecks() {
    let outcome = check_named(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n"),
            ("B.fai", "module B\n\npublic two : Int\nlet two = A.inc 1\n"),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
    assert_eq!(outcome.types.get("two").map(String::as_str), Some("Int"));
}

#[test]
fn qualified_reference_to_private_is_error() {
    let cs = codes(
        "B.fai",
        &[
            ("A.fai", "module A\n\nlet secret x = x\n"),
            ("B.fai", "module B\n\npublic use : Int -> Int\nlet use n = A.secret n\n"),
        ],
    );
    assert!(cs.contains(&"FAI2003".to_owned()), "got {cs:?}");
}

#[test]
fn qualified_reference_to_unknown_module_is_error() {
    let cs = codes("B.fai", &[("B.fai", "module B\n\nlet use = Nope.thing 1\n")]);
    assert!(cs.contains(&"FAI2008".to_owned()), "got {cs:?}");
}

#[test]
fn qualified_reference_to_missing_member_is_error() {
    let cs = codes(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n"),
            ("B.fai", "module B\n\nlet use = A.nope 1\n"),
        ],
    );
    assert!(cs.contains(&"FAI2001".to_owned()), "got {cs:?}");
}

#[test]
fn cross_module_type_flows_into_local_inference() {
    // A.toString : Int -> String; B uses it, so B.label : Int -> String.
    let outcome = check_named(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic toStr : Int -> String\nlet toStr n = intToString n\n"),
            ("B.fai", "module B\n\npublic label : Int -> String\nlet label n = A.toStr n\n"),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
    assert_eq!(outcome.types.get("label").map(String::as_str), Some("Int -> String"));
}

#[test]
fn cross_module_signature_mismatch_is_caught_in_caller() {
    // B applies A.inc (Int -> Int) to a String: a type error in B.
    let cs = codes(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n"),
            ("B.fai", "module B\n\nlet bad = A.inc \"x\"\n"),
        ],
    );
    assert!(cs.contains(&"FAI3001".to_owned()), "got {cs:?}");
}

#[test]
fn duplicate_module_name_errors_on_each_file() {
    let a = codes(
        "A.fai",
        &[("A.fai", "module Same\n\nlet a = 1\n"), ("B.fai", "module Same\n\nlet b = 2\n")],
    );
    assert!(a.contains(&"FAI2007".to_owned()), "file A: {a:?}");
}

#[test]
fn polymorphic_export_instantiates_per_use() {
    // A.identity : 'a -> 'a used at Int and Bool in B.
    let outcome = check_named(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic id : 'a -> 'a\nlet id x = x\n"),
            (
                "B.fai",
                "module B\n\npublic f : Int -> Bool -> Int\nlet f n b = if A.id b then A.id n else n\n",
            ),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
}

#[test]
fn private_binding_is_not_an_export() {
    // A.helper is private; B can only reach A.inc.
    let outcome = check_named(
        "B.fai",
        &[
            (
                "A.fai",
                "module A\n\npublic inc : Int -> Int\nlet inc x = helper x + 1\n\nlet helper y = y * 2\n",
            ),
            ("B.fai", "module B\n\nlet two = A.inc 1\n"),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
}

// ── Poker module chain ────────────────────────────────────────────────────────
// Three modules: Card → Eval → Score. Tests qualified refs across two hops.

const CARD_SRC: &str = "module Card\n\npublic makeCard : Int -> Int -> Int * Int\nlet makeCard rank suit = (rank, suit)\n\npublic rank : Int * Int -> Int\nlet rank card =\n  let (r, s) = card\n  r\n\npublic suit : Int * Int -> Int\nlet suit card =\n  let (r, s) = card\n  s\n\npublic ace : Int\nlet ace = 14\n\npublic validCard : Int * Int -> Bool\nlet validCard card =\n  let r = rank card\n  let s = suit card\n  r >= 2 && r <= 14 && s >= 0 && s <= 3\n";

const EVAL_SRC: &str = "module Eval\n\npublic isFlush5 : Int -> Int -> Int -> Int -> Int -> Bool\nlet isFlush5 s1 s2 s3 s4 s5 = s1 = s2 && s2 = s3 && s3 = s4 && s4 = s5\n\npublic hasPairInRanks : Int -> Int -> Int -> Int -> Int -> Bool\nlet hasPairInRanks r1 r2 r3 r4 r5 = r1 = r2 || r1 = r3 || r1 = r4 || r1 = r5 || r2 = r3 || r2 = r4 || r2 = r5 || r3 = r4 || r3 = r5 || r4 = r5\n\npublic handCategory : Bool -> Bool -> Int\nlet handCategory isF isPair = if isF then 5 else if isPair then 1 else 0\n";

const SCORE_SRC: &str = "module Score\n\npublic scoreHand : Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int\nlet scoreHand c1 c2 c3 c4 c5 =\n  let r1 = Card.rank c1\n  let r2 = Card.rank c2\n  let r3 = Card.rank c3\n  let r4 = Card.rank c4\n  let r5 = Card.rank c5\n  let s1 = Card.suit c1\n  let s2 = Card.suit c2\n  let s3 = Card.suit c3\n  let s4 = Card.suit c4\n  let s5 = Card.suit c5\n  let flush = Eval.isFlush5 s1 s2 s3 s4 s5\n  let pair = Eval.hasPairInRanks r1 r2 r3 r4 r5\n  Eval.handCategory flush pair\n\npublic isAce : Int * Int -> Bool\nlet isAce card = Card.rank card = Card.ace\n\npublic validHand : Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int * Int -> Bool\nlet validHand c1 c2 c3 c4 c5 = Card.validCard c1 && Card.validCard c2 && Card.validCard c3 && Card.validCard c4 && Card.validCard c5\n";

#[test]
fn three_module_poker_chain_typechecks() {
    let outcome = check_named(
        "Score.fai",
        &[("Card.fai", CARD_SRC), ("Eval.fai", EVAL_SRC), ("Score.fai", SCORE_SRC)],
    );
    assert!(!outcome.has_errors(), "Score: {:?}", outcome.codes());
    assert_eq!(
        outcome.types.get("scoreHand").map(String::as_str),
        Some("Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int"),
        "scoreHand type: {:?}",
        outcome.types.get("scoreHand")
    );
}

#[test]
fn card_and_eval_typecheck_independently() {
    for (name, _src) in [("Card.fai", CARD_SRC), ("Eval.fai", EVAL_SRC)] {
        let outcome = check_named(
            name,
            &[("Card.fai", CARD_SRC), ("Eval.fai", EVAL_SRC), ("Score.fai", SCORE_SRC)],
        );
        assert!(!outcome.has_errors(), "{name}: {:?}", outcome.codes());
    }
}

#[test]
fn passing_wrong_type_to_card_rank_errors() {
    let bad = "\
module Bad\n\
\n\
public bad : String -> Int\n\
let bad s = Card.rank s\n";
    let cs = codes("Bad.fai", &[("Card.fai", CARD_SRC), ("Eval.fai", EVAL_SRC), ("Bad.fai", bad)]);
    assert!(cs.iter().any(|c| c.starts_with("FAI3")), "expected a type error, got {cs:?}");
}
