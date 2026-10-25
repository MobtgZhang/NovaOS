//! NovaOS service protocols.
//!
//! The versioned wire formats the kernel's services speak, and the manifest
//! that declares what a service expects. A manifest is a declaration of
//! intent: it never confers authority.
//!
//! # What lives here
//!
//! - [`ProtocolVersion`] is the version type every protocol shares, together
//!   with the [negotiation rule](negotiate) that decides whether a version
//!   may be interpreted at all.
//! - [`console`] is the console service protocol: chunked writes and a
//!   blocking read-line, plus the bounded console buffer both are defined
//!   against.
//! - [`file`] is the file service protocol: open, read, write, close, list,
//!   and stat, each with a `#[repr(C)]` request and reply layout.
//! - [`manifest`] is the service manifest: dependencies, the READY handshake,
//!   timeouts, and the restart window — with no way to express authority.
//!
//! # Layout is a cross-binary contract
//!
//! Every request and every reply is `#[repr(C)]` with a compile-time size and
//! offset assertion. A protocol whose layout drifts is not a compile error in
//! either party — they are separate binaries — it is silent corruption. The
//! assertions make the drift a *build* failure instead, which is the only
//! place it can still be cheap to fix.
//!
//! # The version rule
//!
//! A message carrying a version the service does not implement is **rejected**,
//! never interpreted under a nearby version. Two independently compiled
//! binaries that disagree about a field's meaning produce plausible garbage,
//! not an error, so the only safe response to an unknown version is to refuse
//! it. [`negotiate`] is the single place that decision is made.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod console;
pub mod file;
pub mod manifest;
pub mod shell;

pub use console::{
    CONSOLE_CAPACITY, CONSOLE_OP_READ_LINE, CONSOLE_OP_WRITE, CONSOLE_VERSION, ConsoleBuffer,
    ConsoleOpcode, ConsoleReadLineReply, ConsoleReadLineRequest, ConsoleWriteReply,
    ConsoleWriteRequest, LineOutcome, LineStatus,
};
pub use file::{
    FILE_OP_CLOSE, FILE_OP_LIST, FILE_OP_OPEN, FILE_OP_READ, FILE_OP_STAT, FILE_OP_WRITE,
    FILE_VERSION, FileCloseReply, FileCloseRequest, FileDirEntry, FileKind, FileListReply,
    FileListRequest, FileOpenReply, FileOpenRequest, FileReadReply, FileReadRequest, FileStatReply,
    FileStatRequest, FileWriteReply, FileWriteRequest, WireHandle,
};
pub use manifest::{
    Clock, Dependency, DependencyStatus, MAX_DEPENDENCIES, ManualClock, ReadyMessage,
    RestartDecision, RestartTracker, ServiceManifest, ServiceState, check_dependencies,
};
pub use shell::ShellCommand;

/// The version of a NovaOS protocol.
///
/// Major is an incompatible change: bumping it means a peer built against the
/// old version must be refused. Minor is an additive change: a peer built
/// against an older minor can still be served, because it only asks for what
/// it knows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(C)]
pub struct ProtocolVersion {
    /// The incompatible-change counter.
    pub major: u16,
    /// The additive-change counter.
    pub minor: u16,
}

impl ProtocolVersion {
    /// A version.
    pub const fn new(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    /// Whether a peer offering `self` may be served by an implementation of
    /// `implemented`.
    ///
    /// The rule is: the major versions must be equal, and the offered minor
    /// must not be *newer* than the implemented one. A newer minor means the
    /// peer expects behaviour this implementation does not have, and serving
    /// it anyway would mean interpreting a message whose fields the
    /// implementation has never seen.
    pub const fn is_compatible_with(self, implemented: ProtocolVersion) -> bool {
        self.major == implemented.major && self.minor <= implemented.minor
    }
}

impl core::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Why a version was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VersionError {
    /// The major versions differ: the protocols are not the same protocol.
    MajorMismatch,
    /// The peer offered a newer minor version than the service implements.
    MinorTooNew,
}

impl VersionError {
    /// The ABI error code a refusal maps to.
    ///
    /// [`nova_abi::error::ErrorCode::Unsupported`] is exactly the right code:
    /// it says "the version or operation is not supported", which is what an
    /// unknown version means.
    pub const fn code(self) -> nova_abi::error::ErrorCode {
        nova_abi::error::ErrorCode::Unsupported
    }

    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            VersionError::MajorMismatch => "E_VERSION_MAJOR_MISMATCH",
            VersionError::MinorTooNew => "E_VERSION_MINOR_TOO_NEW",
        }
    }
}

impl core::fmt::Display for VersionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// Decide whether a message carrying `offered` may be interpreted by an
/// implementation of `implemented`.
///
/// This is the *only* place the decision is made, so the sender, the
/// receiver, and the tests all apply identical validation. A refusal is a
/// refusal: there is no "best effort" path that interprets the message under
/// a nearby version, because a nearby version may have moved a field.
pub const fn negotiate(
    implemented: ProtocolVersion,
    offered: ProtocolVersion,
) -> Result<(), VersionError> {
    if implemented.major != offered.major {
        return Err(VersionError::MajorMismatch);
    }
    if offered.minor > implemented.minor {
        return Err(VersionError::MinorTooNew);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ProtocolVersion, VersionError, negotiate};

    #[test]
    fn the_version_is_two_words() {
        // The version travels in the payload of a message, so its size is a
        // layout fact rather than an implementation detail.
        assert_eq!(size_of::<ProtocolVersion>(), 4);
        assert_eq!(core::mem::offset_of!(ProtocolVersion, major), 0);
        assert_eq!(core::mem::offset_of!(ProtocolVersion, minor), 2);
    }

    #[test]
    fn protocol_version_rejects_unknown() {
        // A peer built against a different major is a different protocol.
        assert_eq!(
            negotiate(ProtocolVersion::new(1, 0), ProtocolVersion::new(2, 0)),
            Err(VersionError::MajorMismatch)
        );
        assert_eq!(
            negotiate(ProtocolVersion::new(1, 4), ProtocolVersion::new(2, 0)),
            Err(VersionError::MajorMismatch)
        );

        // A peer built against a newer minor expects fields this
        // implementation has never seen. It is refused rather than
        // interpreted under the nearby minor.
        assert_eq!(
            negotiate(ProtocolVersion::new(1, 2), ProtocolVersion::new(1, 3)),
            Err(VersionError::MinorTooNew)
        );

        // The same version, and an older minor, are accepted.
        assert_eq!(
            negotiate(ProtocolVersion::new(1, 2), ProtocolVersion::new(1, 2)),
            Ok(())
        );
        assert_eq!(
            negotiate(ProtocolVersion::new(1, 2), ProtocolVersion::new(1, 0)),
            Ok(())
        );
        assert_eq!(
            negotiate(ProtocolVersion::new(1, 2), ProtocolVersion::new(1, 1)),
            Ok(())
        );

        // The refusal maps onto the ABI's "not supported" code, so a service
        // can pass it straight out.
        assert_eq!(
            VersionError::MajorMismatch.code(),
            nova_abi::error::ErrorCode::Unsupported
        );
        assert_eq!(
            VersionError::MinorTooNew.code(),
            nova_abi::error::ErrorCode::Unsupported
        );
    }

    #[test]
    fn compatibility_is_the_same_rule_as_negotiation() {
        let implemented = ProtocolVersion::new(1, 2);
        for offered in [
            ProtocolVersion::new(1, 0),
            ProtocolVersion::new(1, 1),
            ProtocolVersion::new(1, 2),
        ] {
            assert!(offered.is_compatible_with(implemented));
            assert_eq!(negotiate(implemented, offered), Ok(()));
        }
        for offered in [
            ProtocolVersion::new(1, 3),
            ProtocolVersion::new(0, 0),
            ProtocolVersion::new(2, 0),
        ] {
            assert!(!offered.is_compatible_with(implemented));
            assert!(negotiate(implemented, offered).is_err());
        }
    }

    #[test]
    fn the_version_renders_readably() {
        // A `core::fmt::Write` sink, because the crate is `no_std` and has no
        // `format!`.
        struct Sink {
            bytes: [u8; 16],
            len: usize,
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
        let mut sink = Sink {
            bytes: [0; 16],
            len: 0,
        };
        core::fmt::Write::write_fmt(&mut sink, format_args!("{}", ProtocolVersion::new(1, 2)))
            .expect("16 bytes is enough for a version");
        assert_eq!(core::str::from_utf8(&sink.bytes[..sink.len]), Ok("1.2"));
    }
}

#[cfg(test)]
mod discipline {
    /// The crate's own source, read at test time.
    ///
    /// The rules this crate lives by — no `unsafe`, no `wrapping_*`, no
    /// `Vec`/`String`/`Box` — are structural rules about the source, and a
    /// structural rule is best enforced by a test that reads the source. A
    /// comment or a lint configuration cannot be forgotten, but a function
    /// body can.
    const SOURCES: [&str; 3] = [
        include_str!("console.rs"),
        include_str!("file.rs"),
        include_str!("manifest.rs"),
    ];

    /// Whether any *code* line of `source` contains `token`.
    ///
    /// Comments are stripped first, so the documentation may *mention* a
    /// forbidden construct without the test failing on the explanation. The
    /// test module at the end of each file is skipped for the same reason: it
    /// names the tokens in order to look for them.
    fn code_contains(source: &str, token: &str) -> bool {
        let mut in_test_module = false;
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed == "#[cfg(test)]" {
                in_test_module = true;
                continue;
            }
            if in_test_module {
                continue;
            }
            let code = line.split("//").next().unwrap_or("");
            if code.contains(token) {
                return true;
            }
        }
        false
    }

    /// Whether any of the crate's files contains `token` in code.
    fn crate_code_contains(token: &str) -> bool {
        SOURCES.iter().any(|source| code_contains(source, token))
            || code_contains(include_str!("lib.rs"), token)
    }

    #[test]
    fn the_crate_contains_no_unsafe_block() {
        for token in ["unsafe fn", "unsafe {", "unsafe impl", "unsafe trait"] {
            assert!(
                !crate_code_contains(token),
                "the protocol crate must stay free of unsafe: its layouts are plain data"
            );
        }
    }

    #[test]
    fn the_crate_never_papers_over_an_overflow() {
        for token in [
            "wrapping_add",
            "wrapping_sub",
            "wrapping_mul",
            "overflowing_add",
            "overflowing_mul",
        ] {
            assert!(
                !crate_code_contains(token),
                "an unchecked length computation must not be hidden behind {token}"
            );
        }
    }

    #[test]
    fn the_crate_uses_no_heap_types() {
        for token in ["Vec<", "String", "Box<", "vec!", "format!"] {
            assert!(
                !crate_code_contains(token),
                "the protocol crate has no allocator, so it may not use `{token}`"
            );
        }
    }
}
