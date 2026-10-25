//! Build script for the kernel.
//!
//! # Why the linker script is named here
//!
//! The kernel's image layout is frozen in `kernel/linker.ld`: a higher-half
//! virtual address with a low physical load address, so the kernel can build
//! recursive page tables and switch CR3 without asking the bootloader where it
//! put the image.
//!
//! `rustc` cannot learn that from a manifest, so the script has to be named to
//! the linker somehow. The two ways to do that are a `-C link-arg` rustflag or
//! a link-arg emitted by a build script, and the build script is used here for
//! two reasons.
//!
//! The first is the same reason `user/nova-rt/build.rs` uses one: a rustflag
//! would have to be repeated by every caller, and a caller who forgot it would
//! get a binary linked at the linker's default entry layout — which *links*,
//! and runs, and is wrong. A build script cannot be forgotten: it runs as part
//! of every build of the package.
//!
//! The second is scope. The bare-metal rustflags in `.cargo/config.toml` are
//! keyed on the *target*, so they apply to every crate built for
//! `x86_64-unknown-none` — including the user-space runtime, which needs a
//! completely different layout (the low half of the address space, entry at
//! `0x1000`). A `-C link-arg=-T...` in the config would hand the kernel's
//! script to the user runtime as well, and the user image would come out
//! linked at the kernel's higher-half addresses, where the loader's user-window
//! check rejects it. Naming the script per *package* keeps the two apart:
//! this file names the kernel's, `user/nova-rt/build.rs` names the user's.
//!
//! # Why the path is absolute
//!
//! The linker resolves `-T` relative to its *working directory*, which is the
//! workspace root when Cargo drives it but the package directory when
//! something else does. An absolute path is the only spelling that works in
//! both cases, and the manifest directory is the one thing a build script
//! always knows.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("Cargo always sets CARGO_MANIFEST_DIR"),
    );

    let script = manifest.join("linker.ld");

    if !script.exists() {
        panic!(
            "the kernel's linker script is missing at {}; the kernel cannot be linked without it",
            script.display()
        );
    }

    println!("cargo::rerun-if-changed={}", script.display());

    // The linker's own argument, not a rustc flag: the script has to be read
    // by `rust-lld`, and `-C link-arg` is how a package tells the linker
    // about something the manifest cannot express.
    println!("cargo::rustc-link-arg=-T{}", script.display());
}
