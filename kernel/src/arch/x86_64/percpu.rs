//! Per-processor data, addressed through `GS`.
//!
//! # Why `GS` base, and why now
//!
//! Before this module the kernel's per-CPU identity existed only as an
//! *index* — `apic::cpu_index()` — and every per-CPU table was indexed by it.
//! That works for Rust code, but two hot paths run in assembly *before* any
//! Rust code can name the CPU:
//!
//! 1. the `syscall` stub, which must stash the user's stack pointer and load
//!    the kernel stack of *the CPU that executed the instruction*;
//! 2. the syscall return path, which must read that stashed pointer back
//!    when it saves the thread's context.
//!
//! Both used a single global (`NOVA_KERNEL_USER_STACK_POINTER`) and a single
//! TSS (`nova_kernel_tss`), which is correct exactly as long as one processor
//! ever dispatches a syscall. A second processor entering the stub would
//! overwrite the first one's user stack pointer and land on the first one's
//! kernel stack. With `GS` pointing at this CPU's own data, the stub is
//! correct on every processor by construction.
//!
//! The base itself is written through `IA32_GS_BASE` (MSR `0xC000_0101`),
//! which ring 0 can always write. `swapgs` is never used: user mode never
//! touches `GS`, and the kernel keeps the same base at all times, so there is
//! no user/kernel base pair to swap.

use super::msr;
use crate::boot::info::MAX_CPUS;

/// `IA32_GS_BASE`. The kernel keeps this exact base at all times; the
/// "kernel" twin (`IA32_KERNEL_GS_BASE`) stays zero because nothing ever
/// executes `swapgs`.
pub const MSR_GS_BASE: u32 = 0xC000_0101;

/// Offsets inside [`PerCpuData`] the naked stub reads by hand. These are
/// load-bearing in `syscall_entry`'s assembly; the compile-time assertions in
/// [`PerCpuData`] keep them honest.
pub const OFFSET_USER_STACK_POINTER: usize = 0x00;
pub const OFFSET_KERNEL_STACK: usize = 0x08;
pub const OFFSET_CPU_INDEX: usize = 0x10;

/// One processor's kernel-owned data.
///
/// Cache-line aligned so two processors' fields never share a line: every
/// field here is written by its own processor and read by nobody else, and a
/// shared line would make the writes ping-pong.
#[repr(C, align(64))]
pub struct PerCpuData {
    /// The user stack pointer the `syscall` stub stashed. Read back by the
    /// syscall return path when it saves the thread's context.
    user_stack_pointer: u64,
    /// The kernel stack top this CPU's syscall entry switches to. Written by
    /// the scheduler when it resumes a thread on this CPU — the same value the
    /// TSS's `RSP0` carries for interrupt entries.
    kernel_stack: u64,
    /// This CPU's kernel slot index, so Rust code can answer "which CPU am I"
    /// without a serialising `CPUID`.
    cpu_index: u64,
    /// Zeroed padding out to a full line, so the next CPU's data does not
    /// share this one's.
    _pad: [u64; 5],
}

// The stub reads these offsets from assembly; a field move silently corrupts
// every processor's syscall path.
const _: () = {
    assert!(core::mem::offset_of!(PerCpuData, user_stack_pointer) == OFFSET_USER_STACK_POINTER);
    assert!(core::mem::offset_of!(PerCpuData, kernel_stack) == OFFSET_KERNEL_STACK);
    assert!(core::mem::offset_of!(PerCpuData, cpu_index) == OFFSET_CPU_INDEX);
};

/// One data block per kernel CPU slot. Indexing is by the same slot index the
/// boot snapshot and every per-CPU table use.
static mut PER_CPU_AREAS: [PerCpuData; MAX_CPUS] = [const {
    PerCpuData {
        user_stack_pointer: 0,
        kernel_stack: 0,
        cpu_index: 0,
        _pad: [0; 5],
    }
}; MAX_CPUS];

/// Point this CPU's `GS` base at its own data block.
///
/// # Safety
///
/// Must run on the CPU being initialised, before anything on it can execute a
/// `syscall` or read the per-CPU cells, and `cpu` must be the kernel slot of
/// *this* processor.
pub unsafe fn init_for_cpu(cpu: usize) {
    assert!(cpu < MAX_CPUS, "CPU index exceeds the per-CPU table");
    // SAFETY: the areas are static storage, always mapped; the base is valid
    // for this CPU's lifetime.
    let base = unsafe { &raw mut PER_CPU_AREAS[cpu] } as *mut u64;
    // Record the index once, so `cpu_index()` is a load rather than a `CPUID`.
    unsafe {
        base.add(OFFSET_CPU_INDEX / 8).write(cpu as u64);
        msr::write(MSR_GS_BASE, base as u64);
    }
}

/// Which CPU slot this processor occupies.
///
/// A plain `GS`-relative load: no serialising `CPUID`, no boot snapshot walk.
pub fn cpu_index() -> usize {
    let index: u64;
    // SAFETY: `GS` base always names this CPU's [`PerCpuData`], which is
    // mapped and aligned; the read never faults.
    unsafe {
        core::arch::asm!(
            "mov {index}, qword ptr gs:[{offset}]",
            index = out(reg) index,
            offset = const OFFSET_CPU_INDEX,
            options(nostack, preserves_flags),
        );
    }
    index as usize
}

/// Read one 64-bit cell out of this CPU's data block.
///
/// # Safety
///
/// The offset must name an existing field.
#[inline]
unsafe fn read_cell(offset: usize) -> u64 {
    let value: u64;
    // SAFETY: as `cpu_index`.
    unsafe {
        core::arch::asm!(
            "mov {value}, qword ptr gs:[{offset}]",
            value = out(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// Store one 64-bit cell into this CPU's data block.
///
/// # Safety
///
/// The offset must name an existing field.
#[inline]
unsafe fn write_cell(offset: usize, value: u64) {
    // SAFETY: `GS` base names this CPU's own data, mapped and aligned.
    unsafe {
        core::arch::asm!(
            "mov qword ptr gs:[{offset}], {value}",
            offset = in(reg) offset,
            value = in(reg) value,
            options(nostack, preserves_flags),
        );
    }
}

/// Set the kernel stack the next `syscall` entry on this CPU switches to.
///
/// Called by the scheduler when it resumes a thread on *this* CPU — the same
/// value it writes into the TSS's `RSP0` for interrupt entries.
///
/// # Safety
///
/// Must run on the CPU being updated, with interrupts masked, before user
/// code resumes.
pub unsafe fn set_kernel_stack(top: u64) {
    // SAFETY: this CPU's own cell; the caller holds the scheduler's
    // interrupts-masked discipline.
    unsafe { write_cell(OFFSET_KERNEL_STACK, top) };
}

/// Read the stashed user stack pointer.
///
/// # Safety
///
/// Only the syscall return path may read this, with interrupts masked.
pub unsafe fn user_stack_pointer() -> u64 {
    // SAFETY: the stub wrote it on this same CPU.
    unsafe { read_cell(OFFSET_USER_STACK_POINTER) }
}
