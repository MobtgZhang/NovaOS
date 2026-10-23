//! Error codes shared by the kernel and every NovaOS SDK.
//!
//! # The encoding contract
//!
//! A syscall's result travels in one register. `RAX >= 0` means success and the
//! value itself is the result; `RAX < 0` means failure and the value is `-E_*`,
//! so `E_INVALID` (1) arrives as `0xFFFF_FFFF_FFFF_FFFF`. There is deliberately
//! no "success or error" flag: a flag and a value would need two registers, and
//! the single-register form is what makes a fast path free.
//!
//! # Why the numbers are frozen
//!
//! These values cross a process boundary. A user binary compiled against one
//! value keeps running against a kernel built with another, so the numbering is
//! append-only: **never renumber, never reuse**. A retired code stays reserved
//! and documented. This is the same discipline any C ABI needs, and it is why
//! the table below is the *only* definition in the tree — the kernel and the SDK
//! both import it rather than each writing their own list.

/// A NovaOS error code.
///
/// Returned negated across the syscall boundary; see the [module docs](self).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(i32)]
pub enum ErrorCode {
    /// Parameters are malformed, or a structure failed validation.
    Invalid = 1,
    /// A capability handle does not exist, or has been invalidated.
    BadHandle = 2,
    /// The handle exists but lacks the rights the operation requires.
    Denied = 3,
    /// Out of memory, or an object quota was hit.
    NoMemory = 4,
    /// The resource is in use.
    Busy = 5,
    /// The operation did not complete before its deadline.
    Timeout = 6,
    /// The endpoint or object was closed.
    Closed = 7,
    /// The peer thread or process exited.
    PeerDied = 8,
    /// The supplied buffer was too small.
    BufferSmall = 9,
    /// The version or operation is not supported.
    Unsupported = 10,
    /// An interruptible wait was interrupted.
    Interrupted = 11,
    /// An object-count or message quota was exceeded.
    Quota = 12,
    /// A non-blocking operation has no result yet.
    WouldBlock = 13,
    /// The grant was revoked.
    Revoked = 14,
}

impl ErrorCode {
    /// The numeric value on the wire.
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// The value as it appears in the result register: always negative.
    pub const fn negated(self) -> i64 {
        -(self.code() as i64)
    }

    /// Recover an error from a result register.
    ///
    /// Returns `None` for a non-negative result, which by the encoding contract
    /// means success — including success with a zero value.
    pub const fn from_result(result: i64) -> Option<ErrorCode> {
        if result >= 0 {
            return None;
        }
        Some(match -result as i32 {
            1 => ErrorCode::Invalid,
            2 => ErrorCode::BadHandle,
            3 => ErrorCode::Denied,
            4 => ErrorCode::NoMemory,
            5 => ErrorCode::Busy,
            6 => ErrorCode::Timeout,
            7 => ErrorCode::Closed,
            8 => ErrorCode::PeerDied,
            9 => ErrorCode::BufferSmall,
            10 => ErrorCode::Unsupported,
            11 => ErrorCode::Interrupted,
            12 => ErrorCode::Quota,
            13 => ErrorCode::WouldBlock,
            14 => ErrorCode::Revoked,
            // An unknown negative value is a version mismatch: a syscall made
            // against a newer ABI than the kernel implements. Reporting
            // `Unsupported` says exactly that, where `Invalid` would blame the
            // caller for a value the caller never chose.
            _ => ErrorCode::Unsupported,
        })
    }

    /// A stable lowercase name, for logs and test assertions.
    pub const fn name(self) -> &'static str {
        match self {
            ErrorCode::Invalid => "E_INVALID",
            ErrorCode::BadHandle => "E_BAD_HANDLE",
            ErrorCode::Denied => "E_DENIED",
            ErrorCode::NoMemory => "E_NO_MEMORY",
            ErrorCode::Busy => "E_BUSY",
            ErrorCode::Timeout => "E_TIMEOUT",
            ErrorCode::Closed => "E_CLOSED",
            ErrorCode::PeerDied => "E_PEER_DIED",
            ErrorCode::BufferSmall => "E_BUFFER_SMALL",
            ErrorCode::Unsupported => "E_UNSUPPORTED",
            ErrorCode::Interrupted => "E_INTERRUPTED",
            ErrorCode::Quota => "E_QUOTA",
            ErrorCode::WouldBlock => "E_WOULD_BLOCK",
            ErrorCode::Revoked => "E_REVOKED",
        }
    }

    /// Whether retrying the identical call could plausibly succeed.
    ///
    /// Used by the SDK's backoff policy and by the test harness to tell a
    /// transient condition from a permanent one. Being wrong here costs latency,
    /// not correctness, so it errs toward "retryable".
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::Busy | ErrorCode::Timeout | ErrorCode::WouldBlock | ErrorCode::Interrupted
        )
    }
}

impl core::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    /// Every code in the table, so a new variant cannot skip the invariants below.
    const ALL: [ErrorCode; 14] = [
        ErrorCode::Invalid,
        ErrorCode::BadHandle,
        ErrorCode::Denied,
        ErrorCode::NoMemory,
        ErrorCode::Busy,
        ErrorCode::Timeout,
        ErrorCode::Closed,
        ErrorCode::PeerDied,
        ErrorCode::BufferSmall,
        ErrorCode::Unsupported,
        ErrorCode::Interrupted,
        ErrorCode::Quota,
        ErrorCode::WouldBlock,
        ErrorCode::Revoked,
    ];

    #[test]
    fn codes_are_the_frozen_documented_values() {
        // These numbers are an external contract with already-built binaries.
        // Changing one is a breaking ABI change, so the table is pinned here.
        let expected = [
            ErrorCode::Invalid.code(),
            ErrorCode::BadHandle.code(),
            ErrorCode::Denied.code(),
            ErrorCode::NoMemory.code(),
            ErrorCode::Busy.code(),
            ErrorCode::Timeout.code(),
            ErrorCode::Closed.code(),
            ErrorCode::PeerDied.code(),
            ErrorCode::BufferSmall.code(),
            ErrorCode::Unsupported.code(),
            ErrorCode::Interrupted.code(),
            ErrorCode::Quota.code(),
            ErrorCode::WouldBlock.code(),
            ErrorCode::Revoked.code(),
        ];
        assert_eq!(expected, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
    }

    #[test]
    fn codes_are_dense_and_start_at_one() {
        // `OK` is 0, so no error may be 0: a zero result register must mean
        // success unambiguously.
        for (index, error) in ALL.iter().enumerate() {
            assert_eq!(error.code(), index as i32 + 1);
        }
    }

    #[test]
    fn round_trips_through_the_result_register() {
        for error in ALL {
            assert_eq!(ErrorCode::from_result(error.negated()), Some(error));
        }
    }

    #[test]
    fn a_non_negative_result_is_success() {
        // Including zero: a successful call returning 0 must not decode as
        // "no error code 0".
        assert_eq!(ErrorCode::from_result(0), None);
        assert_eq!(ErrorCode::from_result(1), None);
        assert_eq!(ErrorCode::from_result(i64::MAX), None);
    }

    #[test]
    fn an_unknown_code_reports_a_version_mismatch_not_bad_input() {
        // A syscall made against a newer ABI must not be reported as the
        // caller's fault.
        assert_eq!(ErrorCode::from_result(-9999), Some(ErrorCode::Unsupported));
    }

    #[test]
    fn every_error_has_a_distinct_name() {
        let mut names = ALL.map(|error| error.name());
        names.sort_unstable();
        for pair in names.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate error name");
        }
    }

    #[test]
    fn transient_errors_are_retryable_and_the_rest_are_not() {
        for error in ALL {
            let expected = matches!(
                error,
                ErrorCode::Busy
                    | ErrorCode::Timeout
                    | ErrorCode::WouldBlock
                    | ErrorCode::Interrupted
            );
            assert_eq!(error.is_retryable(), expected, "{}", error.name());
        }
    }
}
