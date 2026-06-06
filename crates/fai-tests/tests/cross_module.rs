//! Multi-file (cross-module) type-checking scenarios, positive and negative.

use fai_tests::check_named;
use indoc::indoc;

/// The codes reported for the primary file.
fn codes(primary: &str, files: &[(&str, &str)]) -> Vec<String> {
    check_named(primary, files).codes()
}

#[test]
fn qualified_public_reference_typechecks() {
    let outcome = check_named(
        "B.fai",
        &[
            (
                "A.fai",
                indoc! {r#"
                    module A

                    public inc : Int -> Int
                    let inc x = x + 1
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    public two : Int
                    let two = A.inc 1
                "#},
            ),
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
            (
                "A.fai",
                indoc! {r#"
                    module A

                    let secret x = x
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    public use : Int -> Int
                    let use n = A.secret n
                "#},
            ),
        ],
    );
    assert!(cs.contains(&"FAI2003".to_owned()), "got {cs:?}");
}

#[test]
fn qualified_reference_to_unknown_module_is_error() {
    let cs = codes(
        "B.fai",
        &[(
            "B.fai",
            indoc! {r#"
                module B

                let use = Nope.thing 1
            "#},
        )],
    );
    assert!(cs.contains(&"FAI2008".to_owned()), "got {cs:?}");
}

#[test]
fn qualified_reference_to_missing_member_is_error() {
    let cs = codes(
        "B.fai",
        &[
            (
                "A.fai",
                indoc! {r#"
                    module A

                    public inc : Int -> Int
                    let inc x = x + 1
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    let use = A.nope 1
                "#},
            ),
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
            (
                "A.fai",
                indoc! {r#"
                    module A

                    public toStr : Int -> String
                    let toStr n = Int.toString n
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    public label : Int -> String
                    let label n = A.toStr n
                "#},
            ),
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
            (
                "A.fai",
                indoc! {r#"
                    module A

                    public inc : Int -> Int
                    let inc x = x + 1
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    let bad = A.inc "x"
                "#},
            ),
        ],
    );
    assert!(cs.contains(&"FAI3001".to_owned()), "got {cs:?}");
}

#[test]
fn duplicate_module_name_errors_on_each_file() {
    let a = codes(
        "A.fai",
        &[
            (
                "A.fai",
                indoc! {r#"
                    module Same

                    let a = 1
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module Same

                    let b = 2
                "#},
            ),
        ],
    );
    assert!(a.contains(&"FAI2007".to_owned()), "file A: {a:?}");
}

#[test]
fn polymorphic_export_instantiates_per_use() {
    // A.identity : 'a -> 'a used at Int and Bool in B.
    let outcome = check_named(
        "B.fai",
        &[
            (
                "A.fai",
                indoc! {r#"
                    module A

                    public id : 'a -> 'a
                    let id x = x
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    public f : Int -> Bool -> Int
                    let f n b = if A.id b then A.id n else n
                "#},
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
                indoc! {r#"
                    module A

                    public inc : Int -> Int
                    let inc x = helper x + 1

                    let helper y = y * 2
                "#},
            ),
            (
                "B.fai",
                indoc! {r#"
                    module B

                    let two = A.inc 1
                "#},
            ),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
}

// ── Poker module chain ────────────────────────────────────────────────────────
// Three modules: Card → Eval → Score. Tests qualified refs across two hops.

const CARD_SRC: &str = indoc! {r#"
    module Card

    public makeCard : Int -> Int -> Int * Int
    let makeCard rank suit = (rank, suit)

    public rank : Int * Int -> Int
    let rank card =
      let (r, s) = card
      r

    public suit : Int * Int -> Int
    let suit card =
      let (r, s) = card
      s

    public ace : Int
    let ace = 14

    public validCard : Int * Int -> Bool
    let validCard card =
      let r = rank card
      let s = suit card
      r >= 2 && r <= 14 && s >= 0 && s <= 3
"#};

const EVAL_SRC: &str = indoc! {r#"
    module Eval

    public isFlush5 : Int -> Int -> Int -> Int -> Int -> Bool
    let isFlush5 s1 s2 s3 s4 s5 = s1 = s2 && s2 = s3 && s3 = s4 && s4 = s5

    public hasPairInRanks : Int -> Int -> Int -> Int -> Int -> Bool
    let hasPairInRanks r1 r2 r3 r4 r5 = r1 = r2 || r1 = r3 || r1 = r4 || r1 = r5 || r2 = r3 || r2 = r4 || r2 = r5 || r3 = r4 || r3 = r5 || r4 = r5

    public handCategory : Bool -> Bool -> Int
    let handCategory isF isPair = if isF then 5 else if isPair then 1 else 0
"#};

const SCORE_SRC: &str = indoc! {r#"
    module Score

    public scoreHand : Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int
    let scoreHand c1 c2 c3 c4 c5 =
      let r1 = Card.rank c1
      let r2 = Card.rank c2
      let r3 = Card.rank c3
      let r4 = Card.rank c4
      let r5 = Card.rank c5
      let s1 = Card.suit c1
      let s2 = Card.suit c2
      let s3 = Card.suit c3
      let s4 = Card.suit c4
      let s5 = Card.suit c5
      let flush = Eval.isFlush5 s1 s2 s3 s4 s5
      let pair = Eval.hasPairInRanks r1 r2 r3 r4 r5
      Eval.handCategory flush pair

    public isAce : Int * Int -> Bool
    let isAce card = Card.rank card = Card.ace

    public validHand : Int * Int -> Int * Int -> Int * Int -> Int * Int -> Int * Int -> Bool
    let validHand c1 c2 c3 c4 c5 = Card.validCard c1 && Card.validCard c2 && Card.validCard c3 && Card.validCard c4 && Card.validCard c5
"#};

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
    let bad = indoc! {r#"
        module Bad

        public bad : String -> Int
        let bad s = Card.rank s
    "#};
    let cs = codes("Bad.fai", &[("Card.fai", CARD_SRC), ("Eval.fai", EVAL_SRC), ("Bad.fai", bad)]);
    assert!(cs.iter().any(|c| c.starts_with("FAI3")), "expected a type error, got {cs:?}");
}
