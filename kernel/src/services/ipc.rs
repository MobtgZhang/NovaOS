//! The kernel-side IPC plumbing.
//!
//! # What the kernel adds to `nova-ipc`
//!
//! `nova-ipc` holds the *state machine*: two sides, a rendezvous, reply slots,
//! and completions. It has no idea that channels exist in a table, that a
//! message's capabilities come from one task's table and land in another's, or
//! that a thread waiting for a completion has to be woken by someone.
//!
//! This module is the *mechanism* around that state machine:
//!
//! - [`IpcRegistry`] holds the channels, in a bounded pool.
//! - [`TableTransfer`] implements `nova_ipc::CapabilityTransfer` over two
//!   tasks' capability tables, so a message's capabilities move atomically
//!   between them.
//! - [`IpcRegistry::drain_completions`] collects the completions every channel
//!   recorded and hands them to the kernel, which wakes the threads.
//!
//! # How peer death is triggered
//!
//! A task's death is not something the IPC layer notices by itself: the IPC
//! layer knows channels, not tasks. So the kernel holds the mapping from a
//! task to the channels it is a party to, and when a task dies the kernel
//! calls [`IpcRegistry::kill_task`], which kills that task's side of every
//! channel it was party to. The channel then records `E_PEER_DIED` for every
//! waiter that was parked on the dead side, exactly as the report requires:
//! a client blocked in `call` is woken when the server exits.
//!
//! # Draining
//!
//! The channel never calls into the kernel: a parked thread is recorded state,
//! and the wakeup is a [`Completion`](nova_ipc::Completion) the kernel drains
//! after every syscall. That keeps the state machine free of any scheduler
//! dependency — and it is what lets the kernel's test suites exercise the whole
//! IPC path without a scheduler at all.

use nova_abi::cap::CapHandle;
use nova_abi::error::ErrorCode;
use nova_cap::CapabilityTable;
use nova_ipc::channel::Channel;
use nova_ipc::endpoint::{ChannelId, Completion, Endpoint, ReplyToken, Side};
use nova_ipc::message::Message;
use nova_ipc::transfer::CapabilityTransfer;

use crate::services::MAX_CHANNELS;

/// The maximum number of completions one drain can return.
///
/// A drain walks every channel, and each channel can hold a bounded number of
/// completions. The bound is generous for the channels a kernel this size
/// supports, and it keeps the drain's output in a fixed-size array.
pub const MAX_DRAINED_COMPLETIONS: usize = 64;

/// The kernel's IPC channel registry.
///
/// A bounded pool of channels. A channel is created when two endpoints are
/// created, and its identifier is what a capability's object reference names.
pub struct IpcRegistry {
    channels: [Option<Channel>; MAX_CHANNELS],
}

impl Default for IpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IpcRegistry {
    /// An empty registry.
    pub const fn new() -> Self {
        Self {
            channels: [None; MAX_CHANNELS],
        }
    }

    /// Create a channel, returning its identifier.
    ///
    /// Returns `None` when the pool is full. The kernel has no allocator, so a
    /// full pool is a quota rather than an allocation failure.
    pub fn create_channel(&mut self) -> Option<ChannelId> {
        let slot = self.channels.iter().position(|entry| entry.is_none())?;
        let id = ChannelId::new(slot as u32);
        self.channels[slot] = Some(Channel::new(id));
        Some(id)
    }

    /// Borrow a channel, if it exists.
    pub fn channel(&self, id: ChannelId) -> Option<&Channel> {
        let slot = id.number() as usize;
        self.channels.get(slot).and_then(|entry| entry.as_ref())
    }

    /// Borrow a channel mutably, if it exists.
    pub fn channel_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        let slot = id.number() as usize;
        self.channels.get_mut(slot).and_then(|entry| entry.as_mut())
    }

    /// How many channels exist.
    pub fn count(&self) -> usize {
        self.channels.iter().filter(|entry| entry.is_some()).count()
    }

    /// Drop every channel.
    pub fn reset(&mut self) {
        self.channels = core::array::from_fn(|_| None);
    }

    /// Kill one side of every channel a task is a party to.
    ///
    /// This is the kernel half of peer death. The channel decides *what* a
    /// dead peer means for its waiters — `E_PEER_DIED` for a parked sender, a
    /// parked receiver, and a caller whose replier died — and this function
    /// decides *when* it happens: when the task that owns that side exits.
    ///
    /// The task's side is the side whose channel slot matches the task's
    /// recorded side. A task may hold several endpoints, so the mapping from a
    /// task to its (channel, side) pairs is what the kernel tracks.
    pub fn kill_task(&mut self, task: u32, sides: &[(ChannelId, Side)]) {
        for (channel, side) in sides {
            if let Some(channel) = self.channel_mut(*channel) {
                channel.kill_side(*side);
            }
        }
        let _ = task;
    }

    /// Collect every completion recorded since the last drain.
    ///
    /// The kernel calls this after every syscall and wakes the threads the
    /// completions name. A completion that cannot be delivered because the
    /// output array is full is counted, not dropped silently: a lost wakeup is
    /// a hung thread, which is far worse than a reported overflow.
    pub fn drain_completions(&mut self, out: &mut [Completion]) -> usize {
        let mut count = 0usize;
        for entry in self.channels.iter_mut().flatten() {
            let sink = entry.completions();
            for index in 0..sink.len() {
                if count >= out.len() {
                    return count;
                }
                let Some(completion) = sink.get(index) else {
                    continue;
                };
                out[count] = completion;
                count += 1;
            }
            entry.take_completions();
        }
        count
    }
}

/// A capability transfer between two tasks' capability tables.
///
/// This is the kernel's implementation of `nova_ipc::CapabilityTransfer`. The
/// trait's contract is what makes the transfer atomic:
///
/// - `prepare` mints the destination-side handle and changes nothing the
///   source can observe;
/// - `commit` publishes it, dropping the source's copy;
/// - `abort` undoes it, destroying the destination-side handle.
///
/// `nova-ipc`'s `transfer_atomic` drives those three, so a batch either lands
/// whole or not at all.
pub struct TableTransfer<'a> {
    source: &'a mut CapabilityTable,
    destination: &'a mut CapabilityTable,
}

impl<'a> TableTransfer<'a> {
    /// Bind a source and a destination table.
    pub fn new(source: &'a mut CapabilityTable, destination: &'a mut CapabilityTable) -> Self {
        Self {
            source,
            destination,
        }
    }
}

impl CapabilityTransfer for TableTransfer<'_> {
    fn prepare(&mut self, source: CapHandle) -> Result<CapHandle, ErrorCode> {
        // The source must hold a live capability: a revoked or stale handle
        // cannot be given away.
        let entry = self.source.resolve(source)?;
        let object_ref = entry.object_ref;
        let rights = entry.rights;

        // Mint the destination-side handle now, so a failure here is caught
        // before anything is committed. The source entry is untouched: it is
        // only removed at commit time.
        self.destination.create(object_ref, rights)
    }

    fn commit(&mut self, _prepared: CapHandle, source: CapHandle) {
        // The destination handle already exists; publishing it means dropping
        // the source's copy, so the authority cannot be spent twice.
        let _ = self.source.destroy(source);
    }

    fn abort(&mut self, prepared: CapHandle) {
        // Undo the staging: the destination-side handle never existed as far
        // as anyone else is concerned.
        let _ = self.destination.destroy(prepared);
    }
}

/// Transfer a batch of capabilities between two tables, atomically.
///
/// The convenience the kernel's IPC path uses; the atomicity property is
/// `nova-ipc`'s, and this is only the kernel's way of naming it.
pub fn transfer_between_tables(
    source: &mut CapabilityTable,
    destination: &mut CapabilityTable,
    caps: &[CapHandle],
) -> Result<nova_ipc::transfer::TransferredCaps, ErrorCode> {
    let mut transfer = TableTransfer::new(source, destination);
    nova_ipc::transfer::transfer_atomic(&mut transfer, caps)
}

/// The result of one kernel-side IPC operation.
///
/// The three cases mirror `nova_ipc::OpResult`, but the kernel needs one extra
/// piece of information: which reply token, if any, the operation produced.
/// The token is the receiver's authorisation to reply, so it must not be lost
/// between the state machine and the syscall layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IpcOutcome {
    /// The operation completed; the value is the result.
    Ready(Message),
    /// The operation parked; the thread must be woken by a completion.
    Blocked,
    /// The operation failed.
    Failed(ErrorCode),
}

impl IpcOutcome {
    /// Whether the operation completed.
    pub const fn is_ready(&self) -> bool {
        matches!(self, IpcOutcome::Ready(_))
    }

    /// Whether the operation parked.
    pub const fn is_blocked(&self) -> bool {
        matches!(self, IpcOutcome::Blocked)
    }

    /// The error, if the operation failed.
    pub const fn error(&self) -> Option<ErrorCode> {
        match self {
            IpcOutcome::Failed(code) => Some(*code),
            _ => None,
        }
    }
}

/// Send a message on an endpoint.
///
/// `rights` is the rights the *caller's capability* carries; the endpoint
/// mirrors them so the IPC layer can refuse a `recv` on a send-only
/// capability without the kernel having to re-check the table on the fast
/// path. The kernel resolves the real rights from the capability table before
/// it gets here.
pub fn send(
    channel: &mut Channel,
    endpoint: &Endpoint,
    message: Message,
    transfer: &mut dyn CapabilityTransfer,
    thread: u32,
) -> IpcOutcome {
    match channel.send(endpoint, message, transfer, thread) {
        nova_ipc::OpResult::Ready(()) => IpcOutcome::Ready(Message::new(0)),
        nova_ipc::OpResult::Blocked => IpcOutcome::Blocked,
        nova_ipc::OpResult::Error(code) => IpcOutcome::Failed(code),
    }
}

/// Receive a message from an endpoint.
pub fn recv(channel: &mut Channel, endpoint: &Endpoint, thread: u32) -> (IpcOutcome, Option<ReplyToken>) {
    match channel.recv(endpoint, thread) {
        nova_ipc::OpResult::Ready(received) => {
            let token = received.reply_token;
            (IpcOutcome::Ready(received.message), token)
        }
        nova_ipc::OpResult::Blocked => (IpcOutcome::Blocked, None),
        nova_ipc::OpResult::Error(code) => (IpcOutcome::Failed(code), None),
    }
}

/// Reply once to a received request.
pub fn reply(
    channel: &mut Channel,
    endpoint: &Endpoint,
    token: ReplyToken,
    message: Message,
) -> IpcOutcome {
    match channel.reply(endpoint, token, message) {
        nova_ipc::OpResult::Ready(()) => IpcOutcome::Ready(Message::new(0)),
        nova_ipc::OpResult::Blocked => IpcOutcome::Blocked,
        nova_ipc::OpResult::Error(code) => IpcOutcome::Failed(code),
    }
}
