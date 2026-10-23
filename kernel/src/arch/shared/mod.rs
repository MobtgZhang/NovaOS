//! Code that is genuinely independent of the target architecture.
//!
//! Nothing here may contain inline asm, reference a control register, or assume a
//! particular address width. If a piece of logic needs any of those, it belongs
//! in a per-architecture backend instead — that rule is what keeps the split
//! honest, because the temptation to "just add an `#[cfg]`" is exactly what turns
//! a clean facade into a pile of `cfg_if`s.

pub mod console;
pub mod cpu;
pub mod exit;
pub mod fault;
pub mod hw;
