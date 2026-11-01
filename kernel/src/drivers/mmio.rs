//! Volatile access to memory-mapped device registers.
//!
//! # Why this exists
//!
//! A device register is not memory. It may read as a different value on every
//! read, a write may start a transfer the very next instruction, and reordering
//! one of those around the other changes what the device does. Plain reads and
//! writes are therefore both *incorrect* (the compiler may invent, drop, or
//! coalesce them) and insufficient (the CPU may reorder them past each other).
//!
//! This module is the only place the kernel dereferences a register pointer.
//! Every accessor is `read_volatile`/`write_volatile`, and every door that a
//! device walks through — descriptor rings being the one VirtIO uses — is
//! fenced with [`fence`].
//!
//! # Why the pointers are `*mut`
//!
//! Every register the kernel touches is writable at some point, including the
//! ones it currently only reads: a device that is reset by writing a status
//! register needs a *mutable* pointer to that register, and taking a `*const`
//! here and casting at the call site would move the `unsafe` to where it cannot
//! be reviewed. `*mut` with an explicit read is the smaller promise.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::Ordering;

/// Order every pending register access against the accesses either side of it.
///
/// A memory barrier is what separates "the kernel wrote the descriptor ring"
/// from "the device may now be told to look at it". Without it the notify can
/// reach the device first, the device reads a ring the kernel has not finished
/// writing, and the failure is a request the device silently drops — which on
/// a real machine is indistinguishable from a device that never answered.
#[inline]
pub fn fence() {
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// Read a byte-sized register.
///
/// # Safety
///
/// `base` must name a device register of at least one byte, and no other code
/// may be racing this read in a way the device cannot tolerate.
#[inline]
pub unsafe fn read_u8(base: *mut u8, offset: usize) -> u8 {
    // SAFETY: delegated to this function's contract.
    unsafe { read_volatile(base.add(offset)) }
}

/// Read a 16-bit register, little-endian.
///
/// # Safety
/// See [`read_u8`]; the register must be two bytes wide.
#[inline]
pub unsafe fn read_u16(base: *mut u8, offset: usize) -> u16 {
    // SAFETY: delegated to this function's contract.
    unsafe { read_volatile(base.add(offset).cast::<u16>()) }
}

/// Read a 32-bit register, little-endian.
///
/// # Safety
/// See [`read_u8`]; the register must be four bytes wide.
#[inline]
pub unsafe fn read_u32(base: *mut u8, offset: usize) -> u32 {
    // SAFETY: delegated to this function's contract.
    unsafe { read_volatile(base.add(offset).cast::<u32>()) }
}

/// Read a 64-bit register, little-endian.
///
/// # Safety
/// See [`read_u8`]; the register must be eight bytes wide.
#[inline]
pub unsafe fn read_u64(base: *mut u8, offset: usize) -> u64 {
    // SAFETY: delegated to this function's contract.
    unsafe { read_volatile(base.add(offset).cast::<u64>()) }
}

/// Write a byte-sized register.
///
/// # Safety
/// See [`read_u8`]. A write may have an irreversible effect — starting a DMA,
/// resetting a device — so the value is the caller's to have validated.
#[inline]
pub unsafe fn write_u8(base: *mut u8, offset: usize, value: u8) {
    // SAFETY: delegated to this function's contract.
    unsafe { write_volatile(base.add(offset), value) }
}

/// Write a 16-bit register, little-endian.
///
/// # Safety
/// See [`write_u8`].
#[inline]
pub unsafe fn write_u16(base: *mut u8, offset: usize, value: u16) {
    // SAFETY: delegated to this function's contract.
    unsafe { write_volatile(base.add(offset).cast::<u16>(), value) }
}

/// Write a 32-bit register, little-endian.
///
/// # Safety
/// See [`write_u8`].
#[inline]
pub unsafe fn write_u32(base: *mut u8, offset: usize, value: u32) {
    // SAFETY: delegated to this function's contract.
    unsafe { write_volatile(base.add(offset).cast::<u32>(), value) }
}

/// Write a 64-bit register, little-endian.
///
/// # Safety
/// See [`write_u8`].
#[inline]
pub unsafe fn write_u64(base: *mut u8, offset: usize, value: u64) {
    // SAFETY: delegated to this function's contract.
    unsafe { write_volatile(base.add(offset).cast::<u64>(), value) }
}

// No `#[cfg(test)]` module: `kernel/Cargo.toml` marks the binary target
// `test = false`, so one here would never be compiled and would read as
// coverage that does not exist. These accessors are exercised for real by the
// M5 suites in `crate::test::m5`, inside the machine they are written for.
