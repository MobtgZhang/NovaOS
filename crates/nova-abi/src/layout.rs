//! The frozen user-space address layout.
//!
//! # Why these numbers live in the ABI crate
//!
//! `docs/abi/address-layout.md` classifies every address in the system into
//! one of three kinds, and this module holds the *third* kind — the **ABI**
//! constants, the ones a user binary and the kernel must agree on exactly:
//!
//! | Class | Where it comes from | Can it move? |
//! | --- | --- | --- |
//! | Derived | the address-space geometry or the bootloader's answers | yes, with no code change |
//! | ABI | agreed with `user/linker.ld` | only by changing both sides in one commit |
//! | Physical | where the firmware put the kernel image | no |
//!
//! The **derived** constants are deliberately *not* here: the user window's
//! bounds come from the target's virtual address width, and the HHDM offset
//! comes from the bootloader. Neither is a property of the ABI, and a copy of
//! either in this crate would be a second source of truth that could disagree
//! with the first — which is the exact failure the layout document exists to
//! prevent.
//!
//! # The linker script is the other half
//!
//! `user/linker.ld` spells the same numbers in its own syntax. It cannot
//! import them from here — a linker script has no way to read a Rust
//! constant — so the agreement is maintained by *kind*: both sides name the
//! same value for the same thing, and the kernel checks each of these against
//! the derived window at boot, so a disagreement shows up as a refusal to
//! start rather than as a program that runs somewhere unexpected.

/// The base of the user program image.
///
/// The first page of a user address space. `0x0` is never mapped, so a null
/// pointer faults rather than reading the program's own first page.
pub const USER_IMAGE_BASE: u64 = 0x0000_0000_0000_1000;

/// The initial user stack pointer: the top of the user stack.
///
/// The stack grows *down* from here, so this is one past the last byte the
/// stack may occupy.
pub const USER_STACK_TOP: u64 = 0x0000_0000_7FFF_F000;

/// The size of the user stack, in bytes.
///
/// Sixteen pages. The stack is mapped by the kernel, not by the linker, so
/// this is the size of the mapping the kernel creates — and the size the
/// guard page is placed below.
pub const USER_STACK_SIZE: u64 = 64 * 1024;

/// The base of the reserved user heap region.
///
/// The region is `NOLOAD` in the linker script: it occupies address space so
/// that two programs' heaps cannot overlap, but no page of it is mapped until
/// a program asks for one.
pub const USER_HEAP_BASE: u64 = 0x0000_0000_4000_0000;

/// The size of the reserved user heap region, in bytes.
pub const USER_HEAP_SIZE: u64 = 1024 * 1024;

/// The base of the user stack's lowest page.
///
/// One below the stack's top, less its size. The page immediately below
/// *this* is the guard page.
pub const USER_STACK_BASE: u64 = USER_STACK_TOP - USER_STACK_SIZE;

/// The guard page immediately below the user stack.
///
/// The page is deliberately left unmapped, so a stack overflow raises a page
/// fault at the guard rather than silently corrupting whatever lies below the
/// stack.
///
/// The guard's range is *not* the stack's: it is the page below
/// [`USER_STACK_BASE`], so it starts at `USER_STACK_BASE - PAGE_SIZE` and ends
/// where the stack begins.
pub const USER_GUARD_BASE: u64 = USER_STACK_BASE - 0x1000;

/// The page size the layout is expressed in.
///
/// Duplicated rather than imported because `nova-core` — where the canonical
/// page size lives — is a dependency of the *kernel*, not of the ABI, and the
/// ABI must not depend on the kernel's internals. x86-64's page size is
/// architectural and cannot change.
pub const PAGE_SIZE: u64 = 0x1000;

/// Whether `address` lies in the reserved heap region.
pub const fn in_heap(address: u64) -> bool {
    address >= USER_HEAP_BASE && address < USER_HEAP_BASE + USER_HEAP_SIZE
}

/// Whether `address` lies in the user stack's mapped pages.
pub const fn in_stack(address: u64) -> bool {
    address >= USER_STACK_BASE && address < USER_STACK_TOP
}

/// Whether `address` lies in the guard page below the stack.
pub const fn in_guard(address: u64) -> bool {
    address >= USER_GUARD_BASE && address < USER_STACK_BASE
}
