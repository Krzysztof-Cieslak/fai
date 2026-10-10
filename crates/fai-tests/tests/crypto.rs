//! Native crypto primitives cross the generated uniform ABI with their real arities.

use fai_db::Db;

#[test]
fn crypto_primitives_and_entropy_execute_through_the_jit() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("CryptoCheck.fai".into(), r#"module CryptoCheck
public main : Runtime -> Unit / { Console, Crypto.Entropy }
let main r =
  let digest = Crypto.sha256 (Bytes.fromString "abc")
  let mac = Crypto.hmacSha256 digest Bytes.empty
  let derived = Crypto.pbkdf2Sha256 (Bytes.fromString "password") (Bytes.fromString "salt") 1
  let entropy = Crypto.systemEntropy.bytes 32
  let valid = Bytes.length digest = 32 && Bytes.length mac = 32 && Crypto.equal digest digest && Result.map Bytes.length derived = Ok 32 && Result.map Bytes.length entropy = Ok 32 && Crypto.scramPassword "I\u{AD}X" = "IX"
  r.console.writeLine (if valid then "ok" else "bad")
"#.into());
    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(outcome.exit_code, 0, "{:?}", outcome.diagnostics);
    assert_eq!(output, "ok\n");
}
