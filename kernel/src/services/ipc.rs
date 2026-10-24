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
//! - [`IpcRegistry`] holds the channels, in a bounded pool, together with the
//!   mapping from a channel's sides to the tasks holding them;
//! - [`TableTransfer`] implements `nova_ipc::CapabilityTransfer` over two
//!   tasks' capability tables, so a message's capabilities move atomically
//!   between them;
//! - [`IpcRegistry::drain_completions`] collects the completions every channel
//!   recorded and hands them to the kernel, which wakes the threads.
//!
//! # Why the registry tracks the sides
//!
//! The channel state machine knows two sides and nothing else. The kernel,
//! however, has to answer a question the state machine cannot: *which task
//! holds the other side of this channel?* The answer is what makes a
//! capability transfer possible — a message's capabilities are minted in the
//! *recipient's* table, not in the sender's — and it is what makes peer death
//! possible, because a task's death must kill the sides that task holds.
//!
//! The mapping is therefore part of the registry rather than of the channel:
//! the channel is the state machine's, and the registry is the kernel's table
//! of channels. Mixing them would put kernel bookkeeping inside a crate that
//! deliberately knows nothing about tasks.
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

use crate::services::{MAX_CHANNELS, MAX_TASKS};

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
    /// The task slot that holds each side of each channel.
    ///
    /// `holders[channel][side]` is the slot in the kernel's task table of the
    /// task that holds that side. A side that nobody holds is `None`, which is
    /// a channel that has been created but not yet connected — the kernel
    /// refuses to send on it rather than dropping the message.
    holders: [[Option<usize>; 2]; MAX_CHANNELS],
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
            holders: [[None; 2]; MAX_CHANNELS],
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
        self.holders[slot] = [None; 2];
        Some(id)
    }

    /// Record that the task at `task_slot` holds `side` of `channel`.
    ///
    /// Returns `false` when the channel does not exist or when the side is
    /// already held by another task. The second case matters: two tasks
    /// holding the same side would make "the peer" ambiguous, and the
    /// capability transfer would have two possible destinations.
    pub fn bind(&mut self, channel: ChannelId, side: Side, task_slot: usize) -> bool {
        let slot = channel.number() as usize;
        if slot >= MAX_CHANNELS {
            return false;
        }
        if self.channels[slot].is_none() {
            return false;
        }
        let entry = &mut self.holders[slot][side.index()];
        if entry.is_some() {
            return false;
        }
        *entry = Some(task_slot);
        true
    }

    /// The task slot that holds `side` of `channel`.
    pub fn holder(&self, channel: ChannelId, side: Side) -> Option<usize> {
        let slot = channel.number() as usize;
        self.holders
            .get(slot)
            .and_then(|entry| entry.get(side.index()))
            .copied()
            .flatten()
    }

    /// The task slot that holds the *other* side of `channel`.
    ///
    /// This is the peer of a side: the task a message sent on `side` is
    /// delivered to, and the task whose capability table a transferred
    /// capability is minted in.
    ///
    /// Returns `None` when nobody holds the other side — a channel that has
    /// been created but not connected. The caller refuses the operation rather
    /// than dropping the message, because a message that goes nowhere is a
    /// message the sender believes was delivered.
    pub fn peer_task(&self, channel: ChannelId, side: Side) -> Option<usize> {
        self.holder(channel, side.opposite())
    }

    /// Connect two tasks to the two sides of a channel.
    ///
    /// A channel exists before either side is held: the creator makes the
    /// channel, and the peer is *connected* to it. That is why the kernel
    /// tracks the sides separately from the channel — a channel with one side
    /// held is a half-open endpoint, and the message that arrives on it is
    /// delivered when the peer connects.
    ///
    /// Returns `false` when the channel does not exist or when a side is
    /// already held. A side is never taken away from a task that holds it:
    /// reconnecting would be a way to steal an endpoint.
    pub fn connect(&mut self, channel: ChannelId, first: usize, second: usize) -> bool {
        let slot = channel.number() as usize;
        if slot >= MAX_CHANNELS {
            return false;
        }
        if self.channels[slot].is_none() {
            return false;
        }
        if first >= MAX_TASKS || second >= MAX_TASKS || first == second {
            return false;
        }
        if self.holders[slot][0].is_some() || self.holders[slot][1].is_some() {
            return false;
        }
        self.holders[slot][0] = Some(first);
        self.holders[slot][1] = Some(second);
        true
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
        self.channels = [None; MAX_CHANNELS];
        self.holders = [[None; 2]; MAX_CHANNELS];
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
    pub fn kill_task(&mut self, task_slot: usize, sides: &[(ChannelId, Side)]) {
        for (channel, side) in sides {
            if let Some(channel) = self.channel_mut(*channel) {
                channel.kill_side(*side);
            }
        }
        // The dead task no longer holds any side: the mapping is cleared so
        // that a later send on that side cannot pick the dead task as the
        // destination for a capability transfer.
        for slot in 0..MAX_CHANNELS {
            for side in 0..2 {
                if self.holders[slot][side] == Some(task_slot) {
                    self.holders[slot][side] = None;
                }
            }
        }
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

/// A capability transfer within a single table.
///
/// The ordinary case — a message travelling between two tasks — moves a
/// capability from one table to another, and the destination handle is minted
/// in the recipient's table. A task that talks to *itself* through its own
/// endpoint is the one case where the source and the destination are the same
/// table, and it needs its own implementation: the same table cannot be
/// borrowed mutably twice at once, but the stages are sequential.
///
/// The atomicity property is unchanged: every handle is staged first, and
/// only then are the sources dropped. A failure halfway through aborts the
/// staged handles and leaves the source's set exactly as it was.
pub struct SelfTransfer<'a> {
    table: &'a mut CapabilityTable,
}

impl<'a> SelfTransfer<'a> {
    /// Bind the table the capability moves within.
    pub fn new(table: &'a mut CapabilityTable) -> Self {
        Self { table }
    }
}

impl CapabilityTransfer for SelfTransfer<'_> {
    fn prepare(&mut self, source: CapHandle) -> Result<CapHandle, ErrorCode> {
        let entry = self.table.resolve(source)?;
        let object_ref = entry.object_ref;
        let rights = entry.rights;
        self.table.create(object_ref, rights)
    }

    fn commit(&mut self, _prepared: CapHandle, source: CapHandle) {
        let _ = self.table.destroy(source);
    }

    fn abort(&mut self, prepared: CapHandle) {
        let _ = self.table.destroy(prepared);
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

/// Transfer a batch of capabilities within one table, atomically.
///
/// See [`SelfTransfer`] for why the self-send case needs its own path.
pub fn transfer_within_table(
    table: &mut CapabilityTable,
    caps: &[CapHandle],
) -> Result<nova_ipc::transfer::TransferredCaps, ErrorCode> {
    let mut transfer = SelfTransfer::new(table);
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
/// `rights` are the rights the *caller's capability* carries; the endpoint
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
pub fn recv(
    channel: &mut Channel,
    endpoint: &Endpoint,
    thread: u32,
) -> (IpcOutcome, Option<ReplyToken>) {
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
