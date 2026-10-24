//! The boot-time capability the architecture facade delegates to.
//!
//! # Why this is a capability and not three free functions
//!
//! The facade exposes `dump_tables`, `switch_to_kernel_stack` and
//! `switch_to_kernel_stack_and_continue`. Each of them means "do the thing
//! *this architecture* does", and each architecture does it differently: x86-64
//! moves `RSP` and jumps, AArch64 moves `SP` and branches, RISC-V moves `SP`
//! and jumps through a register.
//!
//! When the facade called the x86-64 backend directly, the facade knew the
//! architecture — which is precisely the coupling this module exists to
//! prevent. A named capability says the same thing without naming the
//! machine: the facade asks for "a boot handoff", and the backend that was
//! compiled in answers.
//!
//! The three backends therefore each provide a `BootImpl` type implementing
//! this trait, and `arch::boot` re-exports whichever one the target selects.

use crate::arch::shared::cpu::BootEntry;

/// The boot-time operations every architecture must provide.
///
/// Implementations are unit structs: the capability has no state, because the
/// operations it describes are one-shot boot actions.
pub trait BootHandoff {
    /// Print the architecture's boot-time tables to the console.
    ///
    /// Diagnostics only; safe to call more than once. The console must already
    /// exist, or the output is silently dropped.
    fn dump_tables(&self);

    /// Move the CPU onto the kernel's own stack.
    ///
    /// # Safety
    ///
    /// Interrupts must be masked. The caller's frame is abandoned, so this must
    /// be called only from a frame that will never be unwound.
    unsafe fn switch_to_kernel_stack(&self);

    /// Move the CPU onto the kernel's own stack and continue boot from there.
    ///
    /// This is the form the boot entry point uses. It is `-> !` deliberately:
    /// the entry point has already built a frame on the bootloader's stack, and
    /// abandoning that frame while still unwinding through it later corrupts
    /// the return path in a way that surfaces as a fault in an unrelated
    /// callee. Returning `!` tells LLVM the frame has no epilogue, so nothing
    /// ever reads the abandoned stack back.
    ///
    /// # Safety
    ///
    /// Interrupts must be masked. Called exactly once, from the boot entry
    /// point. Does not return.
    unsafe fn switch_to_kernel_stack_and_continue(&self) -> !;

    /// The function boot continues at once the kernel stack is in place.
    ///
    /// Named rather than hard-coded inside each adapter so that the place boot
    /// resumes is a property of the *kernel*, not of the architecture's
    /// assembly. A backend that implements the stack switch itself jumps to
    /// this address; the x86-64 backend, which already had a hand-written
    /// trampoline before this capability existed, reports the same address so
    /// the two cannot drift apart silently.
    fn continuation() -> BootEntry;
}
