//! The pure algorithms behind NovaOS's virtual memory, scheduling, and syscall
//! layers.
//!
//! # Why a crate of its own
//!
//! Three of the things most likely to go wrong in a microkernel are the ones
//! that are hardest to observe when they go wrong:
//!
//! - a page-table walk that leaks an intermediate table;
//! - a scheduler that puts a thread in the runnable queue twice;
//! - a syscall number that falls through to the wrong operation.
//!
//! Each of those is a *policy* problem, and each policy is a piece of pure
//! logic that can be expressed without a CPU, a page table, or a privilege
//! transition. Putting them here means the host test suite exercises the exact
//! code the kernel runs — not a re-implementation that might differ from it in
//! the one place that matters.
//!
//! # What is deliberately *not* here
//!
//! Nothing that touches hardware:
//!
//! - the page tables themselves, and the CR3 reload, live in the kernel's
//!   `vmm` module;
//! - the context switch and the PIT live in the kernel's `sched` module;
//! - the `iretq` trampoline lives in the kernel's `syscall` module.
//!
//! The split is by *ownership of the mechanism*, not by convenience. The
//! policy is here because it can be tested; the mechanism is in the kernel
//! because it cannot.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod address_space;
pub mod paging;
pub mod scheduler;
pub mod syscall;

#[cfg(test)]
mod tests;

pub use address_space::{
    AddressSpace, GuardPage, MAX_MAPPINGS, Mapping, Protection, UserWindow, VmmError,
};
pub use paging::{
    AddressParts, ENTRIES_PER_TABLE, INDEX_MASK, Level, PAGE_SIZE, PageFlags, VIRTUAL_ADDRESS_BITS,
    WalkIndices, decode_entry, encode_entry, entry_flags, entry_is_huge, entry_is_present,
    is_canonical, is_canonical_in, page_base, page_offset, split_address,
};
pub use scheduler::{MAX_THREADS, SchedError, Scheduler, ThreadRecord, ThreadState};
pub use syscall::{Dispatch, SyscallArgs, SyscallResult, ThreadEffect};
