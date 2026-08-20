//! Proves the `count-allocations` gating mechanically rather than trusting
//! it (SPEC §9): `allocation-counter` registers `#[global_allocator]`
//! unconditionally once compiled, and `optional = true` doesn't apply to
//! `[dev-dependencies]` -- it has to be a real, feature-gated
//! `[dependencies]` entry (`crates/risk/Cargo.toml`), and this is what
//! actually checks that gating holds rather than just asserting it in a
//! comment. Runs in the plain (feature-less) test suite, so a normal
//! `cargo test` catches a regression here without anyone remembering to
//! pass `--features count-allocations`.

use std::process::Command;

#[test]
fn allocation_counter_is_absent_from_the_default_dependency_tree() {
    let output = Command::new("cargo")
        .args(["tree", "-p", "risk", "-e", "normal"])
        .output()
        .expect("failed to run `cargo tree` -- is cargo on PATH?");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        !tree.contains("allocation-counter"),
        "allocation-counter must be absent from risk's default dependency tree, but cargo tree showed:\n{tree}"
    );
}
