//! NovaOS's stable kernel/user ABI.
//!
//! This crate is the *single source of truth* for everything two independently
//! compiled binaries must agree on: error codes, syscall numbers, capability
//! handle layout, and the IPC message header. The kernel imports it, every
//! user-space crate imports it, and host-side tests import it — which is why it
//! is `no_std`, architecture-neutral, and has no dependencies.
//!
//! # What is deliberately *not* here
//!
//! - **Policy.** Which rights a service needs is a decision made by `init`, not by
//!   the ABI. The ABI defines `Rights`; it does not say a file server needs
//!   `READ | WRITE | MAP`.
//! - **Message schemas.** An opcode's meaning belongs to `nova-protocols`, which
//!   is versioned separately and can change without breaking the syscall layer.
//! - **Struct layouts for services.** Only the *fixed-layout value types* that
//!   cross a syscall boundary live here. Anything versioned by a protocol
//!   belongs in `nova-protocols`, so that a protocol can evolve without
//!   invalidating the syscall ABI.
//!
//! # Stability policy
//!
//! Every constant in this crate is **append-only**. Values are never renumbered
//! and never reused. A retired syscall or error code stays reserved and documented
//! so that an old binary calling it receives `E_UNSUPPORTED` rather than silently
//! reaching a different operation.
//!
//! See `docs/abi/` for the calling convention and the address layout this ABI
//! assumes.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod cap;
pub mod error;
pub mod ipc;
pub mod syscall;

pub use cap::{CapHandle, Rights};
pub use error::ErrorCode;
pub use ipc::{IpcHeader, MAX_CAPS_PER_MSG, MAX_INLINE_PAYLOAD};
pub use syscall::{ObjectType, Syscall, nr};

/// The ABI version this crate describes.
///
/// The kernel reports it from [`nr::GET_VERSION`], and a user-space process
/// compares it against the one it was built against. The comparison is
/// major/minor rather than a single integer so that a compatible extension (a new
/// object type, say) does not read as an incompatible kernel.
pub const ABI_VERSION: AbiVersion = AbiVersion { major: 0, minor: 1 };

/// A major/minor ABI version.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(C)]
pub struct AbiVersion {
    /// Incompatible change. Bumped only when an existing meaning changes.
    pub major: u16,
    /// Backwards-compatible addition. Bumped when something new is appended.
    pub minor: u16,
}

impl AbiVersion {
    /// Whether a process built against `self` can talk to a kernel reporting
    /// `kernel`.
    ///
    /// Major must match exactly. Minor may be lower in the kernel than in the
    /// binary: the binary simply cannot use the newer calls, and gets
    /// `E_UNSUPPORTED` for them, which is exactly what it was written to handle.
    /// Minor may *not* be higher in the kernel — that would mean the kernel is
    /// older than the binary, which the binary cannot detect by calling anything,
    /// so it is refused up front.
    pub const fn is_compatible_with(self, kernel: AbiVersion) -> bool {
        self.major == kernel.major && kernel.minor >= self.minor
    }
}

impl core::fmt::Display for AbiVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::{ABI_VERSION, AbiVersion};
    use core::fmt::Write as _;

    /// A fixed `core::fmt::Write` sink, so a `Display` assertion needs neither
    /// `String` nor `format!` — this crate is `no_std`, and pulling in `alloc`
    /// for one assertion would put an allocator behind a crate that is compiled
    /// into the kernel before one exists.
    struct Sink {
        bytes: [u8; 16],
        len: usize,
    }

    impl Sink {
        fn new() -> Self {
            Sink { bytes: [0; 16], len: 0 }
        }

        fn as_str(&self) -> &str {
            core::str::from_utf8(&self.bytes[..self.len]).expect("Display wrote valid UTF-8")
        }
    }

    impl core::fmt::Write for Sink {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let end = self.len + text.len();
            if end > self.bytes.len() {
                return Err(core::fmt::Error);
            }
            self.bytes[self.len..end].copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    #[test]
    fn the_version_is_reported_the_same_way_everywhere() {
        assert_eq!(ABI_VERSION.major, 0);
        assert_eq!(ABI_VERSION.minor, 1);

        let mut sink = Sink::new();
        write!(sink, "{ABI_VERSION}").expect("16 bytes is enough for a version");
        assert_eq!(sink.as_str(), "0.1");
    }

    #[test]
    fn the_version_is_two_words() {
        // It travels in a syscall result, so its size is a layout fact rather
        // than an implementation detail.
        assert_eq!(size_of::<AbiVersion>(), 4);
    }

    #[test]
    fn an_identical_version_is_compatible() {
        assert!(ABI_VERSION.is_compatible_with(ABI_VERSION));
    }

    #[test]
    fn a_newer_minor_kernel_is_compatible() {
        // Additive change: the binary asks for what it knows and gets
        // E_UNSUPPORTED for the rest.
        let kernel = AbiVersion { major: 0, minor: 9 };
        assert!(ABI_VERSION.is_compatible_with(kernel));
    }

    #[test]
    fn an_older_minor_kernel_is_refused() {
        // The binary cannot discover the kernel's age, so this is refused up
        // front rather than discovered by a failed call later.
        let kernel = AbiVersion { major: 0, minor: 0 };
        assert!(!ABI_VERSION.is_compatible_with(kernel));
    }

    #[test]
    fn any_major_mismatch_is_refused() {
        assert!(!ABI_VERSION.is_compatible_with(AbiVersion { major: 1, minor: 1 }));
        assert!(!ABI_VERSION.is_compatible_with(AbiVersion { major: 0, minor: 0 }));
    }
}