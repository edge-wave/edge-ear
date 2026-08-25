//! The committed header must match what the source produces. Ignored
//! by default because it needs cbindgen installed.

use std::process::Command;

#[test]
#[ignore = "needs cbindgen"]
fn the_committed_header_matches_the_source() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate sits inside the workspace");

    let made = Command::new("cbindgen")
        .current_dir(root)
        .args(["--config", "capi/cbindgen.toml", "--crate", "edge-ear-capi"])
        .output()
        .expect("cbindgen must be installed: cargo install cbindgen");
    assert!(
        made.status.success(),
        "cbindgen failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    let fresh = String::from_utf8_lossy(&made.stdout);
    let committed = std::fs::read_to_string(root.join("capi/include/edge_ear.h"))
        .expect("the header is committed");

    if fresh.trim() != committed.trim() {
        let mut first = None;
        for (n, (a, b)) in fresh.lines().zip(committed.lines()).enumerate() {
            if a != b {
                first = Some((n + 1, a.to_string(), b.to_string()));
                break;
            }
        }
        panic!(
            "the header no longer matches the source. Regenerate it:\n  \
             cbindgen --config capi/cbindgen.toml --crate edge-ear-capi \\\n    \
             --output capi/include/edge_ear.h\n\nfirst difference: {first:?}"
        );
    }
}
