//! Opaque aliases use identical native representations on either side of a file boundary.

use std::process::Command;

#[track_caller]
fn run_alias_sample(native: bool, tag: &str) {
    let dir = std::env::temp_dir().join(format!("fai-opaque-alias-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Library.fai"),
        include_str!("../../../samples/opaque_aliases/Library.fai"),
    )
    .unwrap();
    std::fs::write(
        dir.join("Facade.fai"),
        include_str!("../../../samples/opaque_aliases/Facade.fai"),
    )
    .unwrap();
    std::fs::write(dir.join("Main.fai"), include_str!("../../../samples/opaque_aliases/Main.fai"))
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
    command.args(["--no-daemon", "-C"]).arg(&dir);
    let output = if native {
        let exe = dir.join(format!("program{}", std::env::consts::EXE_SUFFIX));
        let build = command.args(["build", "Main.fai", "--out"]).arg(&exe).output().unwrap();
        assert!(
            build.status.success(),
            "{}{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
        Command::new(exe).output().unwrap()
    } else {
        command.args(["run", "Main.fai"]).output().unwrap()
    };
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "deferred\n42\n8\n11\n3.5\n");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn jit_preserves_opaque_scalar_aggregate_function_and_generic_abis() {
    run_alias_sample(false, "jit");
}

#[test]
fn native_preserves_opaque_scalar_aggregate_function_and_generic_abis() {
    run_alias_sample(true, "native");
}
