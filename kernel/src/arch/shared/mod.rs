//! Code that is genuinely independent of the target architecture.
//!
//! Nothing here may contain inline asm, reference a control register, or assume a
//! particular address width. If a piece of logic needs any of those, it belongs
//! in a per-architecture backend instead — that rule is what keeps the split
//! honest, because the temptation to "just add an `#[cfg]`" is exactly what turns
//! a clean facade into a pile of `cfg_if`s.
//!
//! # What is *not* here
//!
//! A few helpers in this directory are used by only some of the backends — the
//! 16550 driver is used by the two machines that put one on the memory bus, and
//! the aligned-stack helper is used by every backend except x86-64, which keeps
//! its own copy next to its GDT code. Those modules are declared conditionally
//! below, so a backend that does not use them does not pay for them and does not
//! accumulate dead code.

pub mod boot;
pub mod console;
pub mod cpu;
pub mod exit;
pub mod fault;
pub mod hw;

/// The kernel's own 16-byte-aligned stack.
///
/// Used by every backend except x86-64, whose GDT code carries its own copy
/// because the two stacks it needs (kernel stack and fault stack) are wired
/// into the TSS rather than into a boot-time symbol.
#[cfg(not(target_arch = "x86_64"))]
pub mod stack;

/// The memory-mapped 16550 UART.
///
/// RISC-V and LoongArch both put a 16550 on the memory bus; x86-64 reaches its
/// UART through port I/O instead and cannot use this driver at all.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub mod uart16550;
