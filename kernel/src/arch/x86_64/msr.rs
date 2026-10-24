//! Model-specific registers.
//!
//! # Why a separate module
//!
//! The `syscall` mechanism is configured entirely through MSRs, and `wrmsr`/
//! `rdmsr` are privileged instructions that fault outside ring 0. Wrapping them
//! in one module means the kernel has exactly one place where "read a control
//! register" happens, and the set of MSRs the kernel touches is a short list
//! rather than a scattering of magic numbers across the syscall and paging
//! code.
//!
//! # The four that matter here
//!
//! - `EFER` — enables the `syscall`/`sysret` instructions (`SCE`) and the
//!   no-execute bit (`NXE`).
//! - `STAR` — the segment selectors `syscall`/`sysret` use.
//! - `LSTAR` — the kernel entry point's address.
//! - `SFMASK` — the `RFLAGS` bits `syscall` clears on entry.

use core::arch::asm;

/// `IA32_EFER`: the extended feature-control register.
pub const MSR_EFER: u64 = 0xC000_0080;
/// `IA32_STAR`: the segment selectors used by `syscall` and `sysret`.
pub const MSR_STAR: u64 = 0xC000_0081;
/// `IA32_LSTAR`: the address of the kernel's `syscall` entry point.
pub const MSR_LSTAR: u64 = 0xC000_0082;
/// `IA32_SFMASK`: the `RFLAGS` bits cleared on entry to `syscall`.
pub const MSR_SFMASK: u64 = 0xC000_0084;
/// `IA32_FMASK`: an alias of `SFMASK` used by `sysret` on some parts.
pub const MSR_FMASK: u64 = 0xC000_0084;

/// `EFER.SCE`: the system-call extension is enabled.
///
/// Without it, executing `syscall` raises `#UD`, which is the same symptom as
/// a missing entry point and makes the two mistakes hard to tell apart.
pub const EFER_SCE: u64 = 1 << 0;
/// `EFER.NXE`: the no-execute bit in page-table entries is honoured.
///
/// Required by the W^X rule: x86-64 has no architectural "execute" bit, so a
/// page that is not writable is still executable until `NXE` is set. The
/// kernel's user mappings need "readable but not executable" to exist at all.
pub const EFER_NXE: u64 = 1 << 11;

/// Read a model-specific register.
pub fn read(msr: u64) -> u64 {
    let value: u64;
    // SAFETY: `rdmsr` is privileged but does not fault for any MSR the kernel
    // reads here; the read/write pair is atomic with respect to the CPU.
    unsafe {
        asm!(
            "rdmsr",
            out("rax") value,
            out("rdx") _,
            in("ecx") msr as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// Write a model-specific register.
pub fn write(msr: u64, value: u64) {
    // SAFETY: `wrmsr` is privileged but does not fault for any MSR the kernel
    // writes here; the low 32 bits of the value go in `eax` and the high 32 in
    // `edx`.
    unsafe {
        asm!(
            "wrmsr",
            in("rax") value,
            in("rdx") 0u32,
            in("ecx") msr as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// The current contents of `EFER`.
pub fn efer() -> u64 {
    read(MSR_EFER)
}

/// Set the given `EFER` bits, leaving the others alone.
///
/// Read-modify-write rather than a plain store: `EFER` holds the no-execute
/// enable alongside the system-call enable and several bits the kernel has no
/// business clearing, and a whole-register store would silently reset them.
pub fn set_efer(bits: u64) {
    write(MSR_EFER, efer() | bits);
}

/// Whether `EFER.SCE` is set.
pub fn syscall_extension_enabled() -> bool {
    efer() & EFER_SCE != 0
}

/// Whether `EFER.NXE` is set.
pub fn no_execute_enabled() -> bool {
    efer() & EFER_NXE != 0
}
