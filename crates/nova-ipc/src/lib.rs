//! NovaOS IPC.
//!
//! The rendezvous that user processes use to talk to each other and to the
//! kernel's services. This crate holds the endpoint state machine; the
//! privilege transition that reaches it is in the kernel.
//!
//! # What is here
//!
//! - [`channel::Channel`] — the rendezvous: `send`, `recv`, `call`, `reply`,
//!   peer death, timeouts, and a bounded queue.
//! - [`transfer`] — atomic capability transfer: a message carrying
//!   capabilities moves all of them or none of them.
//! - [`endpoint`] — endpoints, sides, reply tokens, and the completions the
//!   kernel drains to wake parked threads.
//! - [`message::Message`] — the ABI header plus its bounded inline payload
//!   and capability list.
//!
//! # The shape of the model
//!
//! Two endpoints form a channel; a message sent on one side is received on
//! the other and only on the other. `send` and `call` are *synchronous*: they
//! do not return until the peer has taken the message, so a sender that finds
//! nobody waiting is parked rather than silently buffered.
//!
//! There are no threads and no locks in this crate. A parked thread is
//! recorded state, and the wakeup is a [`Completion`](endpoint::Completion)
//! the kernel drains after every syscall. That is what makes the whole
//! machine testable on the host, and it keeps the kernel's scheduling
//! dependency out of the logic that has to be right.
//!
//! # Why the pieces are split this way
//!
//! The capability transfer lives in its own module rather than inside the
//! channel because atomicity is a property of the *transfer*, and it has to
//! be testable against a store that fails on demand. The channel calls the
//! transfer through a small trait, so the atomicity property is checked
//! directly rather than inferred from the channel's behaviour.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod channel;
pub mod endpoint;
pub mod message;
pub mod transfer;

pub use channel::{Channel, MAX_QUEUE, MAX_REPLY_SLOTS};
pub use endpoint::{
    ChannelId, Completion, CompletionSink, Endpoint, OpResult, Received, ReplyToken, Side,
};
pub use message::{MAX_CAPS_PER_MSG, MAX_INLINE_PAYLOAD, Message};
pub use transfer::{
    CapabilityTransfer, TableTransfer, TransferredCaps, transfer_atomic, transfer_between_tables,
};

pub use nova_abi::error::ErrorCode;

#[cfg(test)]
mod tests;
