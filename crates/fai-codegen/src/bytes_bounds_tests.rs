//! Byte access checks disappear only when the same buffer's bounds are proved.
use super::*;

const GUARDED: &str = "module M\npublic g : Int -> Bytes -> Int\nlet g i xs = if i >= 0 && i < Bytes.length xs then Bytes.unsafeGet i xs else 0\n";

#[test]
fn guarded_bytes_access_elides_its_check() {
    let ir = entry_ir(GUARDED, "g");
    assert!(ir.contains("uload8"), "{ir}");
    assert!(!ir.contains(" ult "), "{ir}");
}

#[test]
fn guarded_bytes_shadow_keeps_a_verification_check() {
    let ir = function_ir_options("M.fai", GUARDED, "g", fai_db::SourceOrigin::User, true, false)
        .remove(0);
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn guarded_bytes_access_elides_its_check_under_concurrency() {
    let ir = function_ir_options("M.fai", GUARDED, "g", fai_db::SourceOrigin::User, false, true)
        .remove(0);
    assert!(ir.contains("uload8"), "{ir}");
    assert!(!ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_array_length_guard_cannot_validate_a_byte_buffer() {
    let ir = entry_ir(
        "module M\npublic g : Int -> Array Int -> Bytes -> Int\nlet g i xs ys = if i >= 0 && i < Array.length xs then Bytes.unsafeGet i ys else 0\n",
        "g",
    );
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_upper_guard_alone_keeps_the_check() {
    let ir = entry_ir(&GUARDED.replace("i >= 0 && ", ""), "g");
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_disjunctive_rejection_proves_the_valid_branch() {
    let ir = entry_ir(
        "module M\npublic g : Int -> Bytes -> Int\nlet g i xs = if i < 0 || i >= Bytes.length xs then 0 else Bytes.unsafeGet i xs\n",
        "g",
    );
    assert!(!ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_disjunctive_acceptance_does_not_prove_both_bounds() {
    let ir = entry_ir(&GUARDED.replace("&&", "||"), "g");
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_false_conjunction_does_not_prove_both_complements() {
    let ir = entry_ir(
        "module M\npublic g : Int -> Bytes -> Int\nlet g i xs = if i < 0 && i >= Bytes.length xs then 0 else Bytes.unsafeGet i xs\n",
        "g",
    );
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_exclusive_endpoint_keeps_the_check() {
    let ir = entry_ir(&GUARDED.replace("i < Bytes.length", "i <= Bytes.length"), "g");
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_successful_access_proves_the_next_access() {
    let ir = entry_ir(
        "module M\npublic g : Int -> Bytes -> Int\nlet g i xs =\n  let a = Bytes.unsafeGet i xs\n  let b = Bytes.unsafeGet i xs\n  a + b\n",
        "g",
    );
    assert_eq!(ir.matches(" ult ").count(), 1, "{ir}");
}

#[test]
fn bytes_other_buffer_keeps_its_check() {
    let ir = entry_ir(
        "module M\npublic g : Int -> Bytes -> Bytes -> Int\nlet g i xs ys = if i >= 0 && i < Bytes.length xs then Bytes.unsafeGet i ys else 0\n",
        "g",
    );
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_loop_threads_length_and_nonnegative_entry_facts() {
    let ir = entry_ir(
        "module M\nlet scan i n acc xs = if i >= n then acc else scan (i + 1) n (acc + Bytes.unsafeGet i xs) xs\npublic run : Bytes -> Int\nlet run xs = scan 0 (Bytes.length xs) 0 xs\n",
        "scan",
    );
    assert!(ir.contains("uload8"), "{ir}");
    assert!(!ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_replacement_loop_keeps_the_new_buffers_check() {
    let ir = entry_ir(
        "module M\nlet scan i n acc xs = if i >= n then acc else scan (i + 1) n (acc + Bytes.unsafeGet i xs) (Bytes.slice 0 1 xs)\npublic run : Bytes -> Int\nlet run xs = scan 0 (Bytes.length xs) 0 xs\n",
        "scan",
    );
    assert!(ir.contains(" ult "), "{ir}");
}

#[test]
fn byte_length_return_facts_reach_callers() {
    let ir = entry_ir(
        "module M\npublic size : Int -> Bytes -> Int\nlet size n xs = if n <= 0 then Bytes.length xs else size (n - 1) xs\npublic g : Int -> Bytes -> Int\nlet g i xs =\n  let limit = size 1 xs\n  if i >= 0 && i < limit then Bytes.unsafeGet i xs else 0\n",
        "g",
    );
    assert!(!ir.contains(" ult "), "{ir}");
}

#[test]
fn bytes_return_preservation_reaches_callers() {
    let ir = entry_ir(
        "module M\npublic pass : Int -> Bytes -> Bytes\nlet pass n xs = if n <= 0 then xs else pass (n - 1) xs\npublic g : Int -> Bytes -> Int\nlet g i xs = if i >= 0 && i < Bytes.length xs then Bytes.unsafeGet i (pass 1 xs) else 0\n",
        "g",
    );
    assert!(!ir.contains(" ult "), "{ir}");
}
