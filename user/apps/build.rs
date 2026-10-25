use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("Cargo always sets CARGO_MANIFEST_DIR"),
    );

    let script = manifest
        .parent()
        .expect("the applications live in <root>/user/apps")
        .join("linker.ld");

    if !script.exists() {
        panic!(
            "the user-space linker script is missing at {}; applications cannot be linked without it",
            script.display()
        );
    }

    println!("cargo::rerun-if-changed={}", script.display());
    println!("cargo::rustc-link-arg=-T{}", script.display());
}
