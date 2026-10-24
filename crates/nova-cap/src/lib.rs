//! NovaOS capability table.
//!
//! The kernel's authority model. Every object a user process can act on is
//! reached through a capability, and a capability is a handle into a
//! kernel-trusted table entry that says *what* the holder may do with it.
//!
//! This crate holds the table and the rules that govern it. It is `no_std`
//! and free of `unsafe`, so the host test suite exercises exactly the code
//! the kernel runs.
//!
//! # The model in one paragraph
//!
//! A capability names an object, a set of rights, and nothing else. Rights
//! are handed out by *derivation*, and derivation intersects: a child
//! capability can never hold a right its parent did not hold. Capabilities
//! live in a fixed pool of slots, and each slot carries a generation counter
//! that advances every time the slot is freed, so a handle to a freed slot
//! stops resolving the moment it is freed. Nothing here allocates: the kernel
//! has no heap, and a table that cannot grow fails loudly instead.
//!
//! # What this crate does *not* decide
//!
//! *Which* rights an object needs, and *who* may revoke what, is policy, and
//! policy belongs to `init` and to the kernel's syscall layer. This crate
//! provides the mechanism that makes such policy enforceable — including the
//! choice of revocation scope, which it leaves to the caller.
//!
//! See [`table::CapabilityTable`] for the table itself and [`object::ObjectRef`]
//! for what a capability points at.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod object;
pub mod table;

pub use object::ObjectRef;
pub use table::{
    CapEntry, CapabilityTable, MAX_CAPS, RESERVED_SLOTS, RevocationScope, RevocationState,
};
