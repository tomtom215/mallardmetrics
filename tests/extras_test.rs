//! The optional extras script (`tracking/extras.js`) run against a fake browser
//! alongside the real main tracker. See `tests/extras_harness.js`.

/// Run the harness, skipping where Node is not installed unless the
/// environment says it must be (CI, or `MALLARD_REQUIRE_NODE=1`).
#[test]
fn test_extras_script_behaviour() {
    let required =
        std::env::var_os("MALLARD_REQUIRE_NODE").is_some() || std::env::var_os("CI").is_some();
    let root = env!("CARGO_MANIFEST_DIR");
    let output = match std::process::Command::new("node")
        .arg(format!("{root}/tests/extras_harness.js"))
        .arg(format!("{root}/tracking/script.js"))
        .arg(format!("{root}/tracking/extras.js"))
        .output()
    {
        Ok(output) => output,
        Err(e) if !required => {
            eprintln!("skipping the extras harness: node is not available ({e})");
            return;
        }
        Err(e) => panic!("node is required for the extras harness: {e}"),
    };
    assert!(
        output.status.success(),
        "extras harness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
