//! Builds the C tests and runs them. Ignored by default: it needs a C
//! compiler, a release build of the library, and a microphone.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate sits inside the workspace")
        .to_path_buf()
}

/// What the static library needs underneath it, which is the audio
/// stack and the C++ runtime the inference engine is written in.
fn system_libraries() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &[
            "-lpthread",
            "-lm",
            "-lc++",
            "-lobjc",
            "-liconv",
            "-framework",
            "AudioToolbox",
            "-framework",
            "CoreAudio",
            "-framework",
            "CoreFoundation",
            "-framework",
            "Foundation",
            "-framework",
            "CoreML",
        ]
    } else {
        &["-lasound", "-lpthread", "-ldl", "-lm", "-lstdc++"]
    }
}

/// Compile one C file, named from the workspace root, against the
/// built library.
fn build(source: &str) -> PathBuf {
    let root = root();
    let library = root.join("target/release/libedge_ear_capi.a");
    assert!(
        library.exists(),
        "build it first: cargo build -p edge-ear-capi --release"
    );

    let name = source.replace(['/', '.'], "_");
    let binary = std::env::temp_dir().join(format!("edge_ear_{name}"));
    let built = Command::new("cc")
        .current_dir(&root)
        .args(["-I", "capi/include", "-Wall", "-Wextra", "-Werror", "-O2"])
        .arg(source)
        .arg(&library)
        .args(system_libraries())
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("a C compiler must be installed");
    assert!(
        built.status.success(),
        "compiling {source} failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    binary
}

/// Compile one C file and run it.
fn build_and_run(source: &str, args: &[&str]) -> String {
    let binary = build(source);
    let ran = Command::new(&binary)
        .args(args)
        .output()
        .expect("the compiled test runs");
    let output = String::from_utf8_lossy(&ran.stdout).to_string();
    assert!(ran.status.success(), "{source} failed:\n{output}");
    output
}

/// The link line differs from one platform to the next, and nothing
/// else in the workspace would notice it going stale.
#[test]
#[ignore = "needs a C compiler and a release build"]
fn everything_written_in_c_still_links() {
    build("capi/tests/surface.c");
    build("capi/examples/listen.c");
}

#[test]
#[ignore = "needs a C compiler, a release build, and a microphone"]
fn every_entry_point_behaves_from_c() {
    let output = build_and_run("capi/tests/surface.c", &[]);
    assert!(
        output.contains("every entry point behaved"),
        "unexpected output:\n{output}"
    );
}
