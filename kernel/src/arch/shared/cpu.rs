//! Arch-neutral CPU services: masking interrupts, idling, halting.
//!
//! Everything here is a *capability* the kernel needs regardless of which
//! machine it boots on, phrased so that the underlying mechanism can differ
//! freely. On x86-64 these map to `cli`/`sti` and `hlt`; on AArch64 they map to
//! `cpsid i`/`cpsie i` and `wfi`.
//!
//! # Naming
//!
//! The operations are named after their *intent* (`interrupts_masked`), not
//! after the instruction that implements them. A backend that has no
//! architecturally-visible equivalent of `sti` can still satisfy the contract by
//! tracking the state itself — which is exactly the situation on AArch64, where
//! guest and kernel share the exception mask.

/// Read and modify the architecture's interrupt mask.
///
/// # Concurrency
///
/// Callers must already hold whatever lock guards the CPU's mask state. These
/// helpers perform no locking, because on one architecture the mask is per-core
/// global state with no atomic read-modify-write — a lost update here would mean
/// interrupts enabled while a lock is held.
pub trait InterruptControl {
    /// Mask maskable interrupts and return the previous state.
    fn mask(&self) -> bool;

    /// Restore a previously observed state.
    fn restore(&self, was_masked: bool);

    /// Whether maskable interrupts are currently masked.
    fn is_masked(&self) -> bool;

    /// Execute [`InterruptControl::mask`] and return the previous state.
    fn mask_preserving(&self) -> MaskGuard<'_, Self>
    where
        Self: Sized,
    {
        MaskGuard {
            control: self,
            was_masked: self.mask(),
        }
    }
}

/// RAII guard that keeps interrupts masked until dropped.
///
/// This exists because the single most common kernel bug is restoring the wrong
/// interrupt state on an error path. Holding a guard makes the restore
/// unconditional: every exit from the scope — `return`, `?`, a panic — puts the
/// CPU back the way it was found.
///
/// Generic over the control type rather than holding a `&dyn` so that it works
/// for a backend value as well as for a trait object; the concrete type is known
/// at every call site in the kernel.
pub struct MaskGuard<'a, C: InterruptControl + ?Sized> {
    control: &'a C,
    was_masked: bool,
}

impl<C: InterruptControl> MaskGuard<'_, C> {
    /// Whether interrupts were already masked when the guard was taken.
    ///
    /// A `true` here means an outer scope owned the mask, so restoring it is a
    /// no-op rather than an unintended un-mask.
    pub const fn was_masked(&self) -> bool {
        self.was_masked
    }
}

impl<C: InterruptControl + ?Sized> Drop for MaskGuard<'_, C> {
    fn drop(&mut self) {
        self.control.restore(self.was_masked);
    }
}

/// An early-init function pointer that hands control to the kernel.
///
/// Bootloaders differ in *how* they call a kernel, and this is the seam where
/// that difference is absorbed. Each backend exports a `#[no_mangle]` symbol
/// with this signature; the architecture-specific entry stub is what the
/// bootloader actually jumps to.
///
/// # Safety
///
/// Must be called only by the bootloader, exactly once, before any other kernel
/// code has run. Everything the kernel relies on — a stack, a console, memory
/// the bootloader described — must already be in place.
pub type BootEntry = unsafe extern "C" fn() -> !;