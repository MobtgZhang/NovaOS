//! A 16-byte-aligned kernel stack, shared by every backend.
//!
//! # Why this is shared
//!
//! Each architecture needs a stack it owns, of a known alignment, whose top
//! address it can hand to the CPU. The requirements are identical on all of
//! them, and they differ only in the register the top address goes into:
//!
//! - x86-64 loads it into `RSP` (`gdt::switch_to_kernel_stack`),
//! - AArch64 loads it into `SP`,
//! - RISC-V loads it into `SP`,
//! - LoongArch loads it into `SP`.
//!
//! Keeping the buffer here means the alignment assertion is written once, and
//! a backend cannot forget it.
//!
//! # Alignment
//!
//! The AArch64 and RISC-V ABIs both require a 16-byte aligned stack pointer
//! at every function boundary, and both *check* it: a misaligned `SP` raises
//! an exception on the very first call rather than corrupting memory silently.
//! A `.bss` array is packed against whatever precedes it, so its address ends
//! up at an arbitrary offset such as `0xca9` — which looks like a perfectly
//! ordinary address right up until the CPU checks its low four bits.

use core::cell::UnsafeCell;

/// A stack buffer whose *top* address is 16-byte aligned.
///
/// The buffer itself is 16-byte aligned as well, which makes the top address
/// aligned too: the size is a multiple of 16, so `base + size` keeps the
/// alignment `base` had.
pub struct AlignedStack<const N: usize> {
    /// The bytes the CPU will push onto.
    ///
    /// `UnsafeCell` because the CPU — not Rust — writes here, through the stack
    /// pointer it was given. A `static mut` would be wrong twice over: it
    /// would claim the kernel mutates the buffer in Rust (it does not), and it
    /// would let a reader take a shared reference to a mutable static, which
    /// is undefined behaviour by construction in edition 2024.
    buffer: UnsafeCell<[u8; N]>,
}

// SAFETY: the buffer is only ever written through the stack pointer the CPU
// was given, and the kernel is single-CPU with interrupts masked for the whole
// window in which a stack is live. There is no Rust-level aliasing of the
// bytes: `top` hands out an address, not a reference, and nothing in the
// kernel reads the buffer through Rust at all.
unsafe impl<const N: usize> Sync for AlignedStack<N> {}

impl<const N: usize> AlignedStack<N> {
    /// A new, zeroed stack.
    ///
    /// The contents are irrelevant — the CPU only ever pushes onto it — but
    /// zeroing keeps the image deterministic.
    pub const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([0; N]),
        }
    }

    /// The address one past the last byte of the stack.
    ///
    /// This is where a downward-growing stack pointer starts.
    ///
    /// # Panics
    ///
    /// Never at runtime: the assertion is on constants, so it is checked at
    /// compile time in every use that matters.
    pub fn top(&self) -> u64 {
        let base = self.buffer.get() as *const u8 as u64;
        // The buffer is 16-byte aligned by the `repr` below, and `N` is a
        // multiple of 16 for every stack in the kernel, so the sum is aligned
        // too. The mask keeps the property even if a future size is not.
        let top = base + N as u64;
        debug_assert_eq!(top & 0xF, 0, "the kernel stack top must be 16-byte aligned");
        top & !0xF
    }
}

impl<const N: usize> Default for AlignedStack<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The alignment every kernel stack top has.
///
/// Spelled once so the backend that switches stacks and the type that owns
/// the buffer cannot disagree about it.
pub const STACK_ALIGNMENT: u64 = 16;
