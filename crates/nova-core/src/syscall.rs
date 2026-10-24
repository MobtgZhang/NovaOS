//! The syscall dispatch table, as pure logic.
//!
//! # What is here and what is not
//!
//! The *policy* of a syscall — which number means what, what its arguments
//! are, what it returns, and what it does to the calling thread's state — is
//! here. The *mechanism* — the `iretq` trampoline, the register save area, the
//! privilege transition — is in the kernel's `syscall` module.
//!
//! Keeping the policy here means the rules that are easy to get subtly wrong
//! are testable without a privilege transition:
//!
//! - a number the kernel does not implement must return `E_UNSUPPORTED`,
//!   never fall through to another operation;
//! - `THREAD_YIELD` must put the caller at the back of the runnable queue;
//! - `THREAD_EXIT` must mark the caller exited rather than merely returning;
//! - the version syscall must report the ABI's version, not a kernel-local
//!   one, so the two cannot disagree.
//!
//! # The error convention
//!
//! A syscall returns a non-negative value on success and a *negative* error
//! code on failure. The kernel's user-space stubs turn the negative value into
//! an `Err`; the raw convention is kept here because that is what crosses the
//! privilege boundary.

use nova_abi::{ABI_VERSION, ErrorCode, Syscall, nr};

/// The value a successful syscall returns.
pub const SUCCESS: i64 = 0;

/// The result of a syscall, as it crosses the privilege boundary.
///
/// The two halves are deliberately distinct types rather than one `i64` with a
/// sign convention: the kernel builds a `SyscallResult`, and only the final
/// encoding into `RAX` knows about the sign. A caller that forgets to check
/// the sign gets a type error rather than a silent misreading of an error code
/// as a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyscallResult {
    /// The call succeeded with this value.
    Value(i64),
    /// The call failed with this error code.
    Failed(ErrorCode),
}

impl SyscallResult {
    /// The raw `RAX` value this result encodes to.
    pub const fn to_raw(self) -> i64 {
        match self {
            SyscallResult::Value(value) => value,
            SyscallResult::Failed(code) => -(code.code() as i64),
        }
    }

    /// Whether the call succeeded.
    pub const fn is_ok(self) -> bool {
        matches!(self, SyscallResult::Value(_))
    }
}

/// The arguments a syscall was called with.
///
/// The registers follow the calling convention documented in
/// `nova_abi::syscall`: `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9`. `RAX` carries
/// the number on entry and the result on exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SyscallArgs {
    /// First argument (`RDI`).
    pub arg0: u64,
    /// Second argument (`RSI`).
    pub arg1: u64,
    /// Third argument (`RDX`).
    pub arg2: u64,
    /// Fourth argument (`R10`).
    pub arg3: u64,
    /// Fifth argument (`R8`).
    pub arg4: u64,
    /// Sixth argument (`R9`).
    pub arg5: u64,
}

impl SyscallArgs {
    /// The arguments, in order.
    pub const fn all(&self) -> [u64; 6] {
        [
            self.arg0, self.arg1, self.arg2, self.arg3, self.arg4, self.arg5,
        ]
    }
}

/// What a syscall did that affects the calling thread.
///
/// Most syscalls return a value and leave the thread running. Three do not:
/// yielding, exiting, and blocking. The dispatcher reports *which* of those
/// happened, so the kernel's entry path can perform the state change and
/// schedule something else — without the dispatcher having to know about the
/// scheduler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadEffect {
    /// The thread continues.
    Continue,
    /// The thread yielded; schedule another one.
    Yield,
    /// The thread exited; it must be reaped.
    Exit,
    /// The thread blocked; wake it when the awaited event happens.
    Block,
}

/// The outcome of dispatching one syscall.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dispatch {
    /// The value to return to the caller.
    pub result: SyscallResult,
    /// What the call did to the calling thread.
    pub effect: ThreadEffect,
}

impl Dispatch {
    /// A dispatch that returns `value` and lets the thread continue.
    pub const fn value(value: i64) -> Self {
        Self {
            result: SyscallResult::Value(value),
            effect: ThreadEffect::Continue,
        }
    }

    /// A dispatch that fails with `code`.
    pub const fn failed(code: ErrorCode) -> Self {
        Self {
            result: SyscallResult::Failed(code),
            effect: ThreadEffect::Continue,
        }
    }

    /// A dispatch that fails with `code` and ends the thread.
    pub const fn failed_and_exit(code: ErrorCode) -> Self {
        Self {
            result: SyscallResult::Failed(code),
            effect: ThreadEffect::Exit,
        }
    }
}

/// The ABI version the kernel reports.
pub const fn abi_version() -> nova_abi::AbiVersion {
    ABI_VERSION
}

/// The version value a user process receives from `GET_VERSION`.
///
/// Encoded as `major << 16 | minor`, so a single 64-bit comparison tells a
/// process whether the kernel is new enough. The kernel's own
/// [`nova_abi::AbiVersion::is_compatible_with`] does the careful comparison;
/// this is the cheap one a process can do without linking the ABI crate's
/// policy.
pub const fn version_word() -> i64 {
    ((ABI_VERSION.major as i64) << 16) | ABI_VERSION.minor as i64
}

/// Whether the kernel implements `number`.
///
/// The M2 kernel implements the calls that a user process needs to exist at
/// all — query the kernel, yield, exit — and refuses the rest with
/// `E_UNSUPPORTED` rather than guessing. Guessing is how a number reserved for
/// M3's IPC ends up meaning something else.
pub const fn is_implemented(number: Syscall) -> bool {
    matches!(
        number,
        nr::GET_VERSION | nr::YIELD | nr::THREAD_EXIT | nr::THREAD_SPAWN
    )
}

/// Dispatch a syscall whose implementation needs no kernel state.
///
/// The calls that *do* need kernel state — mapping memory, spawning a thread
/// into an address space, sending a message — are handled by the kernel's
/// `syscall` module, which owns the pools those calls draw from. This
/// function covers the stateless ones, which is enough to make the convention
/// testable end to end.
///
/// # Errors
///
/// Returns `E_UNSUPPORTED` for a number this kernel does not implement.
pub fn dispatch_stateless(number: Syscall, args: &SyscallArgs) -> Dispatch {
    match number {
        nr::GET_VERSION => Dispatch::value(version_word()),

        nr::YIELD => Dispatch {
            result: SyscallResult::Value(SUCCESS),
            effect: ThreadEffect::Yield,
        },

        nr::THREAD_EXIT => {
            // The exit status is the first argument. A process that passes
            // something absurd still exits — refusing to exit would strand it.
            let status = args.arg0 as i64;
            Dispatch {
                result: SyscallResult::Value(status),
                effect: ThreadEffect::Exit,
            }
        }

        _ => Dispatch::failed(ErrorCode::Unsupported),
    }
}

/// The name of a syscall, for the serial log.
pub fn name(number: Syscall) -> &'static str {
    nr::name(number)
}
