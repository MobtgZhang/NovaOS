//! Port IO helpers for x86-64.
//!
//! NovaOS talks to legacy devices — the 16550 UART above all — exclusively
//! through `in`/`out`. This module is the single place where that is allowed,
//! so the rest of the kernel never issues a bare inline-asm port access.

use core::arch::asm;

/// Write a byte to an IO port.
///
/// # Safety
///
/// `port` must be a port the caller is allowed to write to, and the port must
/// accept byte-sized writes. Writing some ports has side effects that persist
/// beyond the instruction, so callers must know what they are driving.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nostack, preserves_flags));
    }
}

/// Write a 32-bit word to an IO port.
///
/// # Safety
///
/// `port` must be writable and accept dword-sized writes; see [`outb`].
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack, preserves_flags));
    }
}

/// Read a byte from an IO port.
///
/// # Safety
///
/// `port` must be readable, and some ports clear state or acknowledge
/// interrupts when read, so callers must know what they are polling. A port
/// with no read side effect — the UART's line status register, for instance —
/// still needs this `unsafe` because the *caller*, not this function, is the one
/// that knows which ports those are.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nostack, preserves_flags));
    }
    value
}

/// Read a 32-bit word from an IO port.
///
/// # Safety
///
/// `port` must be readable and accept dword-sized reads; see [`inb`].
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("in eax, dx", out("eax") value, in("dx") port, options(nostack, preserves_flags));
    }
    value
}

/// Read the 16-bit CR2 (page-fault linear address) register.
///
/// # Safety
///
/// This register is always safe to read; the value is only meaningful while
/// servicing a `#PF`.
#[inline]
pub unsafe fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: `mov rax, cr2` is an unprivileged read of a debug register.
    unsafe {
        asm!("mov {}, cr2", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Read the 64-bit CR0 register.
#[inline]
pub unsafe fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: `mov rax, cr0` is an unprivileged read.
    unsafe {
        asm!("mov {}, cr0", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Read the 64-bit CR3 (page directory base) register.
#[inline]
pub unsafe fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: `mov rax, cr3` is an unprivileged read.
    unsafe {
        asm!("mov {}, cr3", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Read the 64-bit CR4 register.
#[inline]
pub unsafe fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: `mov rax, cr4` is an unprivileged read.
    unsafe {
        asm!("mov {}, cr4", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Load CR0.
///
/// # Safety
///
/// Changing CR0 can enable paging, protection, or disable writes to
/// read-only pages system-wide. Callers must set exactly the bits they intend.
#[inline]
pub unsafe fn write_cr0(value: u64) {
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags));
    }
}
///
/// # Safety
///
/// `value` must be the physical base of a valid page table whose top-level
/// entries cover every address the kernel and current stack touch. Loading a
/// bad CR3 faults immediately and, with an IDT that depends on the stack, may
/// triple fault.
#[inline]
pub unsafe fn write_cr3(value: u64) {
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// Load CR4.
///
/// # Safety
///
/// Must not clear `CR4.PAE` while paging is active.
#[inline]
pub unsafe fn write_cr4(value: u64) {
    // SAFETY: delegated to this function's own contract.
    unsafe {
        asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// `cli`: disable maskable interrupts.
#[inline]
pub fn cli() {
    // SAFETY: `cli` is unprivileged; the caller restores interrupt state.
    unsafe {
        asm!("cli", options(nostack, preserves_flags));
    }
}

/// `sti`: enable maskable interrupts.
#[inline]
pub fn sti() {
    // SAFETY: `sti` is unprivileged; callers must have an IDT installed first.
    unsafe {
        asm!("sti", options(nostack, preserves_flags));
    }
}

/// `hlt`: halt the CPU until the next interrupt arrives.
#[inline]
/// Halt the CPU until an interrupt arrives.
///
/// `hlt` with interrupts masked never returns, so reaching this with the mask
/// still set permanently stops the machine — which is exactly what an exit path
/// wants, and a bug everywhere else.
pub fn hlt() {
    // SAFETY: `hlt` is unprivileged.
    unsafe {
        asm!("hlt", options(nostack, preserves_flags));
    }
}
