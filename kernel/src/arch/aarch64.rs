// Rustfmt and rust-analyzer resolve every `mod` declaration without evaluating
// its `#[cfg]`. A backend that is declared in `arch/mod.rs` but not yet
// implemented -- `aarch64` -- therefore makes `cargo fmt` abort with
// "failed to resolve mod `aarch64`" and exit non-zero, so the whole tree stops
// being format-able the moment a second architecture is *planned* rather than
// built.
//
// The stub below exists purely to give rustfmt something real to open. It is
// never compiled for x86-64, and when the aarch64 backend lands this file is
// deleted along with the `mod` line it backs.
//
// Keep this file free of code: anything added here is unreachable code that
// still has to be maintained and reviewed.

/// Placeholder backing `arch::aarch64` until that backend exists.
///
/// See the file comment above for why it is here at all.
#[cfg(target_arch = "aarch64")]
pub type Unimplemented = ();
