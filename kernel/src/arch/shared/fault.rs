//! Arch-neutral fault reporting.
//!
//! Each architecture decodes *its own* fault into a [`FaultInfo`] and then calls
//! [`report`]. Everything after decoding — formatting, line shape, the panic
//! decision — is identical on both platforms, and duplicating it per-architecture
//! is exactly how the two logs drifted apart in the first draft of this kernel.
//!
//! # Why decode-then-report
//!
//! The register sets differ substantially: x86-64 pushes an error code, `CR2`, and
//! a possibly-16-byte frame depending on the privilege change, while AArch64
//! pushes an ESR/ECR pair and a frame whose size depends on the cause. Only the
//! *interpretation* is architecture-specific. Normalising to [`FaultInfo`] means
//! the log format is a property of NovaOS, not of the machine it booted on,
//! which in turn lets `scripts/qemu.sh` assert on one format for both targets.

use crate::arch::shared::console::{ByteSink, Console};
use crate::logger::Level;

/// Why the CPU stopped executing the interrupted code.
///
/// The variants are the subset of fault causes the kernel handles the same way
/// on every architecture. Anything more specific is folded into
/// [`FaultInfo::detail`], which carries the architecture's raw encoding for
/// anyone debugging at the register level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultKind {
    /// The processor could not decode or execute the instruction.
    IllegalInstruction,
    /// An instruction or data reference violated a protection rule.
    ProtectionViolation,
    /// A translation failed: no mapping, or a mapping that was not permitted.
    AddressTranslation,
    /// The access was not aligned to what the instruction required.
    MisalignedAccess,
    /// An integer divide by zero, or a quotient that does not fit.
    DivideByZero,
    /// A synchronous exception the kernel has no specific handling for.
    SynchronousGeneric,
    /// A trap raised deliberately by kernel code (breakpoint, syscall, `brk`).
    SoftwareTrap,
    /// An interrupt arrived with no handler bound to that vector.
    UnexpectedInterrupt,
    /// The CPU reported a hardware failure.
    MachineCheck,
}

impl FaultKind {
    /// Stable lowercase tag used in the serial log.
    ///
    /// These strings are part of the kernel's log format contract: the test
    /// runner greps for them, so they must not be renamed for style.
    pub const fn tag(self) -> &'static str {
        match self {
            FaultKind::IllegalInstruction => "illegal-instruction",
            FaultKind::ProtectionViolation => "protection",
            FaultKind::AddressTranslation => "address-translation",
            FaultKind::MisalignedAccess => "misaligned",
            FaultKind::DivideByZero => "divide-by-zero",
            FaultKind::SynchronousGeneric => "sync",
            FaultKind::SoftwareTrap => "software-trap",
            FaultKind::UnexpectedInterrupt => "unexpected-interrupt",
            FaultKind::MachineCheck => "machine-check",
        }
    }

    /// Longer human-readable description.
    pub const fn describe(self) -> &'static str {
        match self {
            FaultKind::IllegalInstruction => "could not execute instruction",
            FaultKind::ProtectionViolation => "violated a protection rule",
            FaultKind::AddressTranslation => "address translation failed",
            FaultKind::MisalignedAccess => "access was misaligned",
            FaultKind::DivideByZero => "divide by zero or inexact quotient",
            FaultKind::SynchronousGeneric => "synchronous exception",
            FaultKind::SoftwareTrap => "software trap",
            FaultKind::UnexpectedInterrupt => "interrupt with no handler",
            FaultKind::MachineCheck => "hardware fault reported by the machine",
        }
    }
}

/// One architecture's normalised description of a fault.
#[derive(Clone, Copy)]
pub struct FaultInfo {
    /// What the CPU was trying to do.
    pub kind: FaultKind,
    /// Architecture-defined vector or exception number. On x86-64 this is the IDT
    /// vector; on AArch64 it is the syndrome-reduced exception index.
    pub vector: u64,
    /// Instruction pointer at the fault.
    pub ip: u64,
    /// Stack pointer at the fault.
    pub sp: u64,
    /// Faulting address, when the architecture supplies one.
    pub address: Option<u64>,
    /// Whether the fault happened in an unprivileged context.
    pub from_user: bool,
    /// Architecture-specific extra bits (x86-64 error code, AArch64 `ESR`).
    pub detail: u64,
}

/// Build a `[NOVA-TEST]`-style single-line fault report into `sink`.
///
/// Separate from [`report`] so that both the reporter and the panic handler can
/// use it without duplicating the formatting.
///
/// `level` replaces the hard-coded `ERR` tag. A recovered fault is still a
/// fault worth seeing, but calling it a warning keeps the log honest about
/// whether the machine kept running.
pub fn format_into<const N: usize>(sink: &mut ByteSink<N>, info: &FaultInfo, level: Option<Level>) {
    let tag = level.unwrap_or(Level::Error).tag();

    sink.push_str("[NOVA] ");
    sink.push_str(tag);
    sink.push_str("   + ");
    sink.push_str(info.kind.tag());
    sink.push_str(": ");
    sink.push_str(info.kind.describe());
    sink.push_str(" (vector ");
    sink.push_hex(info.vector);
    sink.push_str(")\r\n");

    sink.push_str("[NOVA] ");
    sink.push_str(tag);
    sink.push_str("   | ip=");
    sink.push_hex(info.ip);
    sink.push_str(" sp=");
    sink.push_hex(info.sp);

    if let Some(address) = info.address {
        sink.push_str(" addr=");
        sink.push_hex(address);
    }

    sink.push_str(" detail=");
    sink.push_hex(info.detail);
    sink.push_str("\r\n");

    if info.from_user {
        // Spelling out what will happen next matters: without this line, a ring-3
        // fault looks identical to a kernel bug in the log.
        sink.push_str("[NOVA] ");
        sink.push_str(tag);
        sink.push_str("   | fault originated in unprivileged mode; the thread is killed\r\n");
    }
}

/// Report a fault on the serial console.
///
/// Bypasses `klog!` on purpose. The fault may have been raised by the formatter
/// itself, by the logger's re-entrancy guard, or by anything the logger touches;
/// routing the report through the same machinery invites recursion straight back
/// into the fault. This writes bytes and nothing else.
///
/// # Safety
///
/// Must be called from a context where writing to the console cannot itself
/// fault. Both backends implement `send_bytes` without locks or allocation, so
/// this holds in the fault path.
pub unsafe fn report<C: Console>(console: &C, info: &FaultInfo) {
    let mut sink = ByteSink::<320>::new();
    format_into(&mut sink, info, None);

    // `send_bytes` is safe on both backends — bounded spins, no locks, no
    // allocation — so this function's own `unsafe` is what authorises the call
    // from a fault context where the usual preconditions cannot be checked.
    console.send_bytes(sink.as_bytes());
}

/// Log a non-fatal fault-class event at `level`.
///
/// Used for the cases where the kernel recovers — an unhandled device interrupt
/// that can be ignored, for instance — so those do not all look like halts.
///
/// # Note
///
/// This path deliberately does *not* re-enter the level-filtered logger: the
/// fault it is reporting may be exactly what broke it. The severity is
/// substituted into the formatted line instead, so the log still records it.
pub fn note<C: Console>(console: &C, level: Level, info: &FaultInfo) {
    let mut sink = ByteSink::<320>::new();
    format_into(&mut sink, info, Some(level));

    console.send_bytes(sink.as_bytes());
}
