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
#[repr(u8)]
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
    /// CPU error code, when the vector pushes one.
    ///
    /// Kept separate from [`FaultInfo::detail`] because on x86-64 the two are
    /// different things that historically got conflated: `detail` is the
    /// architecture's *raw* code (which for `#PF` is the P/W/U/I decode), while
    /// this is the original code as pushed. A `#PF` wants both — the decode for
    /// the report and the original for a test that deliberately faults.
    pub error_code: u64,
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

// --- Recoverable faults -----------------------------------------------------
//
// A fault handler that always halts is correct for a kernel with no user space,
// but it makes two important things untestable: whether the handler reads the
// right registers, and whether the machine survives a fault at all. M2's first
// requirement is that a bad user access kills one *thread*, which needs the
// handler to have a way to say "I dealt with this" before it does so.
//
// So a handler may publish an outcome here instead of exiting, and the IDT
// dispatch path will resume. Two design points matter:
//
// 1. Resuming at the *same* instruction is unsound in general. A fault that has
//    been reported but not repaired re-faults immediately, so a handler that
//    chose to "continue" would spin the machine at fault speed with no way out.
//    The resume therefore takes an explicit target address, and the fault tests
//    point it at the instruction *after* the one that faulted.
//
// 2. That target must not be reachable from ordinary kernel code, or a bug in
//    the interlock would resume arbitrary code with an arbitrary stack. It is
//    therefore a per-test nonce written to a static, published only for the
//    duration of the test, and compared for equality before being used.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

/// What a fault handler decided to do about an exception.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultOutcome {
    /// Unrecoverable: report and halt with the panic status.
    Fatal,
    /// Reported, and the machine may resume at [`resume_target`].
    Continue,
}

/// Outcome of the most recent recoverable fault, for the test harness to read.
static LAST_OUTCOME: AtomicU8 = AtomicU8::new(0);

/// The most recent outcome, as its raw discriminant.
const OUTCOME_FATAL: u8 = 0;
const OUTCOME_CONTINUE: u8 = 1;

/// Address to resume at when a handler publishes [`FaultOutcome::Continue`].
///
/// Zero unless a test armed it. Read back through [`resume_target`] before use, so
/// a handler cannot resume an address it did not obtain from the fault harness.
static RESUME_TARGET: AtomicU64 = AtomicU64::new(0);

/// Publish the address to resume at once the next fault is handled.
///
/// Must be called inside [`with_recoverable_faults`] so the resume target can
/// never outlive the window that authorises it.
pub fn arm_resume_at(address: u64) {
    RESUME_TARGET.store(address, Ordering::Release);
}

/// The address a handler would resume at, or `0` if none is armed.
#[inline]
pub fn resume_target() -> u64 {
    RESUME_TARGET.load(Ordering::Acquire)
}

/// Clear the resume target, so a stale address cannot be used by a later fault.
pub fn disarm_resume() {
    RESUME_TARGET.store(0, Ordering::Release);
}

/// Whether a handler may currently choose [`FaultOutcome::Continue`].
///
/// Set only while the fault tests are deliberately provoking an exception, and
/// cleared immediately afterwards. This is the safety interlock that makes
/// resuming a faulting instruction sound: a `Continue` outside this window would
/// re-execute an instruction whose fault was never repaired, which turns a
/// single recoverable fault into an infinite fault loop.
static CONTINUE_ARMED: AtomicBool = AtomicBool::new(false);

/// Allow faults raised inside `body` to be resumed rather than halting the
/// machine.
///
/// The window is deliberately as narrow as it can be made: it is armed
/// immediately before the call and disarmed immediately after, so a fault
/// anywhere else in the kernel still halts. A test that panics or faults outside
/// `body` therefore cannot accidentally be resumed.
///
/// # Safety
///
/// `body` must not rely on the effects of a *failed* instruction, and must not
/// loop indefinitely without its own progress guarantee. Both hold for the
/// single faulting instruction the callers pass.
pub unsafe fn with_recoverable_faults<R>(body: impl FnOnce() -> R) -> R {
    CONTINUE_ARMED.store(true, Ordering::Release);
    let result = body();
    CONTINUE_ARMED.store(false, Ordering::Release);
    result
}

/// Whether a handler may resume a faulting instruction right now.
///
/// Read by the IDT dispatch path after the handler has already reported; the
/// architecture code then decides whether resuming is safe.
#[inline]
pub fn may_continue() -> bool {
    CONTINUE_ARMED.load(Ordering::Acquire)
}

/// Record an outcome and return it, so the caller can branch on it.
pub fn publish_outcome(outcome: FaultOutcome) -> FaultOutcome {
    LAST_OUTCOME.store(
        match outcome {
            FaultOutcome::Fatal => OUTCOME_FATAL,
            FaultOutcome::Continue => OUTCOME_CONTINUE,
        },
        Ordering::Release,
    );
    outcome
}

/// Reset the recorded outcome to [`FaultOutcome::Fatal`].
///
/// Called at the start of each fault test so that a handler which never ran is
/// distinguishable from one that ran and chose to continue. Without this, a test
/// that stops raising faults would keep passing on the strength of an earlier
/// one.
pub fn reset_outcome() {
    LAST_OUTCOME.store(OUTCOME_FATAL, Ordering::Release);
}

/// The outcome recorded by the most recent fault.
pub fn last_outcome() -> FaultOutcome {
    match LAST_OUTCOME.load(Ordering::Acquire) {
        OUTCOME_CONTINUE => FaultOutcome::Continue,
        _ => FaultOutcome::Fatal,
    }
}

/// How many faults have been reported since the last [`reset_fault_count`].
static FAULT_COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Count one reported fault.
pub fn count_fault() {
    FAULT_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Number of faults reported since the last reset.
pub fn fault_count() -> usize {
    FAULT_COUNT.load(Ordering::Relaxed)
}

/// Zero the fault counter.
pub fn reset_fault_count() {
    FAULT_COUNT.store(0, Ordering::Relaxed);
}

// --- Fault recording for the test harness ------------------------------------
//
// A handler that resumes control to its caller has, by then, restored the stack
// and registers — so a test cannot read them afterwards. The only way to assert
// on what the handler actually saw is for the handler to record it first, which
// is what this does.

/// A fault, in the shape the tests need to assert on.
#[derive(Clone, Copy)]
pub struct FaultRecord {
    /// The normalised fault kind.
    pub kind: FaultKind,
    /// The CPU vector.
    pub vector: u64,
    /// The saved instruction pointer.
    pub ip: u64,
    /// The saved stack pointer, or `0` when the CPU pushed none.
    pub sp: u64,
    /// The faulting address, when the architecture supplied one.
    pub address: Option<u64>,
    /// The raw CPU error code.
    pub error_code: u64,
    /// Whether the fault was taken from an unprivileged context.
    pub from_user: bool,
    /// Whether the CPU pushed the privilege-level-change words.
    pub privilege_change: bool,
    /// The raw `CS` word as read from the frame.
    pub cs_value: u64,
    /// The raw `RFLAGS` word as read from the frame.
    pub rflags_value: u64,
}

impl FaultRecord {
    /// A record that reads as "no fault has been seen".
    const fn empty() -> Self {
        Self {
            kind: FaultKind::SynchronousGeneric,
            // The sentinel is an impossible vector: it makes "handler never ran"
            // distinguishable from "handler ran and reported address 0", which a
            // zero-filled record cannot do.
            vector: u64::MAX,
            ip: 0,
            sp: 0,
            address: None,
            error_code: 0,
            from_user: false,
            privilege_change: false,
            cs_value: 0,
            rflags_value: 0,
        }
    }

    /// Whether a fault has been recorded since the last [`reset_fault`].
    pub const fn is_set(&self) -> bool {
        self.vector != u64::MAX
    }
}

/// The most recent recorded fault.
static LAST_FAULT: AtomicFaultRecord = AtomicFaultRecord::new();

/// Copy-on-write storage for a [`FaultRecord`].
///
/// A lock-free `Copy` of 12 fields would need an `AtomicU128`, which does not
/// exist. Two `AtomicU64` slots would tear, so the record is published under a
/// simple test-and-set: only one writer is possible at a time anyway, because the
/// interlock admits exactly one deliberately-provoked fault, and the reader is
/// always the same thread that armed it.
struct AtomicFaultRecord {
    /// Guard; non-zero while a writer holds the slot.
    locked: AtomicBool,
    /// The record itself.
    slot: core::cell::UnsafeCell<FaultRecord>,
}

// SAFETY: `slot` is only ever read or written while `locked` is held, and the
// only possible writer is the fault handler invoked by the single-CPU kernel with
// interrupts masked. No SMP code exists to race with.
unsafe impl Sync for AtomicFaultRecord {}

impl AtomicFaultRecord {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            slot: core::cell::UnsafeCell::new(FaultRecord::empty()),
        }
    }

    fn store(&self, value: FaultRecord) {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the compare-exchange above granted exclusive access, and no
        // other thread can hold it until this store releases it below.
        unsafe { *self.slot.get() = value };
        self.locked.store(false, Ordering::Release);
    }

    fn load(&self) -> FaultRecord {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the compare-exchange above granted exclusive access.
        let copy = unsafe { *self.slot.get() };
        self.locked.store(false, Ordering::Release);
        copy
    }
}

/// Record the most recent fault for the test harness.
pub fn record_fault(info: &FaultInfo, cs_value: u64, rflags_value: u64, privilege_change: bool) {
    LAST_FAULT.store(FaultRecord {
        kind: info.kind,
        vector: info.vector,
        ip: info.ip,
        sp: info.sp,
        address: info.address,
        error_code: info.error_code,
        from_user: info.from_user,
        privilege_change,
        cs_value,
        rflags_value,
    });
}

/// The most recent recorded fault.
pub fn last_fault() -> FaultRecord {
    LAST_FAULT.load()
}

/// Clear the recorded fault, so a stale one cannot make a handler that never ran
/// look correct.
pub fn reset_fault() {
    LAST_FAULT.store(FaultRecord::empty());
}

#[cfg(test)]
mod tests {
    use super::{FaultOutcome, last_outcome, reset_outcome};
    use core::sync::atomic::{AtomicU32, Ordering};

    // The fault-path tests mutate process-wide state, so they run under one lock
    // to keep their ordering from becoming load-bearing. `cargo test` runs tests
    // on threads in parallel, and these five touch the same statics.
    static LOCK: AtomicU32 = AtomicU32::new(0);

    fn serialise() -> u32 {
        let mut spins = 0;
        while LOCK
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            assert!(spins < 10_000_000, "fault-path test lock never released");
            core::hint::spin_loop();
        }
        spins
    }

    fn release() {
        LOCK.store(0, Ordering::Release);
    }

    #[test]
    fn a_fresh_outcome_is_fatal() {
        serialise();
        reset_outcome();
        assert_eq!(last_outcome(), FaultOutcome::Fatal);
        release();
    }

    #[test]
    fn the_continue_interlock_is_closed_outside_the_window() {
        // This is the invariant that makes resuming a faulting instruction
        // sound. If it were armed outside a `with_recoverable_faults` window,
        // any fault anywhere in the kernel would be resumed and immediately
        // re-raised, turning one fault into an unbounded loop.
        serialise();
        assert!(
            !super::may_continue(),
            "the continue interlock must default to closed"
        );
        release();
    }

    #[test]
    fn the_window_opens_around_the_body_and_closes_after() {
        serialise();
        let inside = unsafe {
            super::with_recoverable_faults(|| {
                assert!(super::may_continue(), "window must be open inside");
                7u32
            })
        };
        assert_eq!(inside, 7, "the body must run exactly once and return");
        assert!(
            !super::may_continue(),
            "the window must close even after the body returns"
        );
        release();
    }
}
