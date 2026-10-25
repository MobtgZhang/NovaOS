//! Model-Specific Registers.
//!
//! # Why the kernel needs three of them
//!
//! The `syscall`/`sysretq` mechanism is controlled by three MSRs, and a
//! kernel that gets any of them wrong fails in a way that is hard to read:
//!
//! - `EFER.SCE` — the *System Call Extension* bit. Without it the `syscall`
//!   instruction does not exist, and executing it raises `#UD` — the same
//!   symptom as a missing entry point, which makes the two mistakes hard to
//!   tell apart.
//! - `STAR` — where the kernel and user segment selectors live. The CPU reads
//!   the user selectors out of it when a `syscall` drops the privilege level.
//! - `LSTAR` — the entry point the `syscall` instruction jumps to.
//! - `SFMASK` — the `RFLAGS` bits the CPU clears on entry.
//!
//! # Why the access is inlined
//!
//! `RDMSR` and `WRMSR` are privileged: at ring 3 they raise `#GP`. The kernel
//! writes them once at boot and the user program never touches them, so the
//! operations live here as small `unsafe` helpers rather than as a general
//! MSR API.

use core::arch::asm;

/// `IA32_EFER` — the Extended Feature Enable register.
pub const MSR_EFER: u32 = 0xC000_0080;

/// `IA32_STAR` — the System Test and Architecture register.
pub const MSR_STAR: u32 = 0xC000_0081;

/// `IA32_LSTAR` — the Long System Call Target address.
pub const MSR_LSTAR: u32 = 0xC000_0082;

/// `IA32_SFMASK` — the System Call Flag Mask.
pub const MSR_SFMASK: u32 = 0xC000_0084;

/// `EFER.SCE` — the System Call Extension bit.
pub const EFER_SCE: u64 = 1 << 0;

/// Read an MSR.
///
/// # Safety
///
/// The MSR must exist on this processor. Reading an MSR that does not exist
/// raises `#GP`.
#[inline]
pub unsafe fn read(msr: u32) -> u64 {
    let (low, high): (u32, u32);

    // SAFETY: delegated to this function's contract. `RDMSR` is privileged and
    // the kernel runs at ring 0. The two halves are read into separate
    // registers and combined afterwards, because `RDMSR` writes a 64-bit value
    // across `EDX:EAX` and the assembler cannot express that as one operand.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nostack, preserves_flags),
        );
    }

    ((high as u64) << 32) | (low as u64)
}

/// Write an MSR.
///
/// # Safety
///
/// The MSR must exist on this processor, and the value must be one the
/// processor accepts.
#[inline]
pub unsafe fn write(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;

    // SAFETY: delegated to this function's contract. `WRMSR` is privileged
    // and the kernel runs at ring 0. The value is split across `EAX:EDX`,
    // which is the register pair the instruction reads.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags),
        );
    }
}

/// Set the `EFER.SCE` bit, which is what makes the `syscall` instruction
/// exist.
///
/// The other bits of `EFER` are read first and written back unchanged. Writing
/// a bare `1` would clear every other feature the firmware enabled — and
/// `EFER` bits include things like `LMA` (long mode active) and `NXE` (no-execute
/// support), whose clearing would change how the machine behaves in ways that
/// have nothing to do with the syscall path.
///
/// # Safety
///
/// Must be called at ring 0. The `EFER` MSR must exist, which it does on every
/// processor that has long mode.
pub unsafe fn set_efer(bit: u64) {
    // SAFETY: reading `EFER` at ring 0 is always safe.
    let current = unsafe { read(MSR_EFER) };

    // SAFETY: delegated to this function's contract. The value written is the
    // current one with one bit added, so no other feature is disturbed.
    unsafe {
        write(MSR_EFER, current | bit);
    }
}
