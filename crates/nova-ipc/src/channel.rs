//! The channel: the rendezvous state machine.
//!
//! # The shape of the model
//!
//! A channel has two sides. A message sent on one side is received on the
//! other and *only* on the other, which is structural here rather than a
//! check that could be forgotten: the two sides have separate inboxes, and a
//! send on side `S` writes into side `opp(S)`'s inbox.
//!
//! # The rendezvous
//!
//! `send` is synchronous. A message is not "sent" until a receiver has taken
//! it, so there are exactly three cases:
//!
//! - the peer is already parked in `recv`: the message is queued and the
//!   peer is woken to collect it. The sender's send completes at once,
//!   because a waiting receiver is committed to taking it.
//! - the peer is not waiting: the message is queued and the *sender* parks.
//!   When the peer eventually takes the message, the sender is woken.
//! - the peer's queue is full: the send fails with
//!   [`ErrorCode::WouldBlock`]. The kernel has no allocator, so a queue that
//!   cannot grow is the only honest answer.
//!
//! Parking is the interesting state, because a parked thread is a thread the
//! kernel must be able to wake. The channel never calls into the kernel; it
//! records a [`Completion`], and the kernel asks for the completions and
//! wakes the threads. That keeps this crate free of any scheduler dependency
//! and makes the whole machine testable on the host.
//!
//! # `call` and `reply`
//!
//! `call` is a `send` that also expects an answer. The difference is a reply
//! slot: a table entry that names the caller and the side that owes the reply.
//! When the receiver collects the message, the channel assigns the slot a
//! **fresh nonce** and hands the receiver a [`ReplyToken`] naming the slot
//! and the nonce.
//!
//! The nonce is assigned at delivery, not at call time, and is never reused.
//! That is what makes a token unforgeable: no token exists before the
//! receiver was handed one, so there is nothing to guess. Presenting it
//! again fails because the slot has been spent; presenting a token for a
//! different slot, a different channel, or a different side fails too.
//!
//! # Timeouts
//!
//! A parked caller may give up. Doing so invalidates the reply slot, so a
//! reply that arrives afterwards is discarded rather than delivered to
//! whoever holds the slot next. This is why the timeout is an explicit state
//! transition on the slot, and not "the waiter stopped looking".
//!
//! # Bounded queues
//!
//! Each inbox holds a fixed number of messages. When it is full, a send
//! reports [`ErrorCode::WouldBlock`] rather than growing: the kernel has no
//! allocator, and an unbounded queue is how a chatty peer turns into a hung
//! kernel.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;

use crate::endpoint::{
    ChannelId, Completion, CompletionSink, Endpoint, OpResult, Received, ReplyToken, Side,
};
use crate::message::Message;
use crate::transfer::{CapabilityTransfer, transfer_atomic};

/// Maximum messages waiting to be collected on one side of a channel.
///
/// Bounded because the kernel has no allocator and the queue must live in a
/// fixed-size slot. Eight is enough for the ping-pong protocols v0.1.0 needs
/// and small enough that a flood costs a bounded amount of kernel memory.
pub const MAX_QUEUE: usize = 8;

/// Maximum outstanding reply slots on one channel.
///
/// A reply slot exists for the whole lifetime of a `call`, so this bounds the
/// number of calls a channel can have in flight.
pub const MAX_REPLY_SLOTS: usize = 32;

/// The state of one reply slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReplyState {
    /// The slot holds no call.
    Vacant,
    /// A `call` was made and its message has not yet been collected.
    Pending,
    /// The message was collected, so the receiver holds the token.
    Delivered,
    /// The reply was made. The slot is spent and cannot be used again.
    Replied,
    /// The caller timed out. The slot is spent; a late reply is discarded.
    TimedOut,
    /// The call failed before anyone could reply.
    Cancelled,
}

/// One outstanding call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ReplySlot {
    state: ReplyState,
    /// The nonce of the token handed out for this call.
    ///
    /// Assigned when the message is collected, and never reused for another
    /// call, so a spent token can never be confused with a live one.
    nonce: u32,
    caller_side: Side,
    replier_side: Side,
    caller: u32,
}

/// One queued message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct QueueEntry {
    used: bool,
    message: Message,
    /// The parked sender of a plain `send`, if any.
    ///
    /// A `call` has no waiter here: the caller is waiting on its reply slot,
    /// not on the delivery of its message.
    waiter: Option<u32>,
    /// The reply slot of a `call`, if any.
    reply_slot: Option<u16>,
}

/// The state of one side of a channel.
#[derive(Clone, Copy, Debug)]
struct SideState {
    alive: bool,
    /// Messages waiting to be collected *by this side*.
    inbox: [QueueEntry; MAX_QUEUE],
    inbox_len: usize,
    /// The thread parked in `recv` on this side, if any.
    receiver: Option<u32>,
}

impl SideState {
    const fn new() -> Self {
        Self {
            alive: true,
            inbox: [QueueEntry {
                used: false,
                message: Message::new(0),
                waiter: None,
                reply_slot: None,
            }; MAX_QUEUE],
            inbox_len: 0,
            receiver: None,
        }
    }

    fn has_space(&self) -> bool {
        self.inbox_len < MAX_QUEUE
    }

    fn push(&mut self, entry: QueueEntry) {
        let index = self.inbox_len;
        self.inbox[index] = entry;
        self.inbox[index].used = true;
        self.inbox_len += 1;
    }

    fn pop(&mut self) -> Option<QueueEntry> {
        if self.inbox_len == 0 {
            return None;
        }
        let entry = self.inbox[0];
        // Shift the remaining entries down. The queue is short and bounded,
        // so a shift is cheaper than a ring buffer's index arithmetic — and it
        // keeps the "the queue is FIFO" property obvious.
        for index in 1..self.inbox_len {
            self.inbox[index - 1] = self.inbox[index];
        }
        self.inbox_len -= 1;
        self.inbox[self.inbox_len].used = false;
        Some(entry)
    }

    fn clear(&mut self) {
        for index in 0..self.inbox_len {
            self.inbox[index].used = false;
            self.inbox[index].waiter = None;
            self.inbox[index].reply_slot = None;
        }
        self.inbox_len = 0;
    }
}

/// A channel: two sides, a rendezvous, and a table of outstanding calls.
///
/// `Copy` is derived because the channel is entirely value data: two side
/// states, a reply-slot table, a nonce counter, and a completion sink. None
/// of those has an identity that survives a copy, and the kernel keeps
/// channels in a fixed-size pool where expressing "no channel here" as
/// `None` requires the element type to be `Copy` for the repeat expression
/// that builds the array.
///
/// A copy is a distinct channel with the same contents, so two copies never
/// alias: there is no interior mutability to share.
#[derive(Clone, Copy)]
pub struct Channel {
    id: ChannelId,
    sides: [SideState; 2],
    replies: [ReplySlot; MAX_REPLY_SLOTS],
    /// Source of reply-slot nonces.
    ///
    /// Monotonic per channel and mixed with the slot index, so a nonce is
    /// never reused while the channel lives and a token for one slot can
    /// never be mistaken for a token for another.
    nonce_counter: u32,
    completions: CompletionSink,
}

impl Channel {
    /// A fresh channel with both sides alive.
    pub const fn new(id: ChannelId) -> Self {
        Self {
            id,
            sides: [SideState::new(), SideState::new()],
            replies: [ReplySlot {
                state: ReplyState::Vacant,
                nonce: 0,
                caller_side: Side::First,
                replier_side: Side::Second,
                caller: 0,
            }; MAX_REPLY_SLOTS],
            nonce_counter: 0,
            completions: CompletionSink::new(),
        }
    }

    /// The channel's number.
    pub const fn id(&self) -> ChannelId {
        self.id
    }

    /// A view of one side, carrying a set of rights.
    ///
    /// The rights mirror the holder's capability; the IPC layer refuses
    /// operations the rights do not cover.
    pub const fn endpoint(&self, side: Side, rights: Rights) -> Endpoint {
        Endpoint::new(self.id, side, rights)
    }

    /// Whether a side's peer is still alive.
    pub fn peer_alive(&self, side: Side) -> bool {
        self.sides[side.opposite().index()].alive
    }

    /// Whether a side is still alive.
    pub fn side_alive(&self, side: Side) -> bool {
        self.sides[side.index()].alive
    }

    /// How many messages are waiting to be collected on a side.
    pub fn queued(&self, side: Side) -> usize {
        self.sides[side.index()].inbox_len
    }

    /// How many calls are outstanding.
    pub fn outstanding_calls(&self) -> usize {
        self.replies
            .iter()
            .filter(|slot| matches!(slot.state, ReplyState::Pending | ReplyState::Delivered))
            .count()
    }

    /// How many threads are parked on this channel.
    pub fn parked(&self) -> usize {
        let receivers = self
            .sides
            .iter()
            .filter(|side| side.receiver.is_some())
            .count();
        let senders = self
            .sides
            .iter()
            .map(|side| {
                side.inbox[..side.inbox_len]
                    .iter()
                    .filter(|entry| entry.used && entry.waiter.is_some())
                    .count()
            })
            .sum::<usize>();
        receivers + senders
    }

    /// The completions recorded since the kernel last drained them.
    pub const fn completions(&self) -> &CompletionSink {
        &self.completions
    }

    /// Take the recorded completions, leaving the sink empty.
    pub fn take_completions(&mut self) -> usize {
        self.completions.clear()
    }

    /// Send a message, parking until a receiver takes it.
    ///
    /// The message's capability list is transferred atomically: either every
    /// capability moves to the receiver's table, or the send fails and the
    /// sender's capability set is exactly what it was.
    ///
    /// Returns [`OpResult::Ready`] when a receiver was already waiting, and
    /// [`OpResult::Blocked`] when the sender must wait for the peer to take
    /// the message. The kernel parks the thread in the latter case and
    /// delivers the resulting [`Completion::SendCompleted`] when the message
    /// is finally collected.
    pub fn send(
        &mut self,
        endpoint: &Endpoint,
        message: Message,
        transfer: &mut dyn CapabilityTransfer,
        thread: u32,
    ) -> OpResult<()> {
        if let Err(error) = self.check_common(endpoint, Rights::SEND) {
            return OpResult::Error(error);
        }
        if !message.validate() {
            return OpResult::Error(ErrorCode::Invalid);
        }

        let peer = endpoint.side().opposite();

        // The queue-space check happens *before* the capability transfer, so
        // that a backpressure rejection cannot have moved anything. It is
        // unconditional: every send occupies a queue slot, even one that a
        // waiting receiver is about to take.
        if !self.sides[peer.index()].has_space() {
            return OpResult::Error(ErrorCode::WouldBlock);
        }

        let message = match self.transfer_caps(message, transfer) {
            Ok(message) => message,
            Err(error) => return OpResult::Error(error),
        };

        // The message always goes into the peer's queue, so that `recv`
        // returns the payload inline whichever path delivered it.
        let waiting = self.sides[peer.index()].receiver.take();
        self.sides[peer.index()].push(QueueEntry {
            used: true,
            message,
            // A sender that parks is remembered on the queued message, so
            // that collecting the message is what completes its send.
            waiter: if waiting.is_some() {
                None
            } else {
                Some(thread)
            },
            reply_slot: None,
        });

        if let Some(receiver) = waiting {
            // A receiver that was already waiting is committed to taking the
            // message, so the send completes at once and no sender has to be
            // woken later.
            self.completions
                .push(Completion::ReceiveReady { waiter: receiver });
            return OpResult::Ready(());
        }

        // Nobody is waiting, so the sender parks until the message is
        // actually collected.
        OpResult::Blocked
    }

    /// Receive a message, parking until one arrives.
    ///
    /// Returns [`OpResult::Blocked`] when nothing was waiting; the kernel
    /// parks the thread and delivers the resulting
    /// [`Completion::ReceiveReady`] when a message shows up.
    ///
    /// The returned [`Received`] carries a [`ReplyToken`] when the sender
    /// used `call` — that token is the only authorisation to reply.
    pub fn recv(&mut self, endpoint: &Endpoint, thread: u32) -> OpResult<Received> {
        if let Err(error) = self.check_common(endpoint, Rights::RECEIVE) {
            return OpResult::Error(error);
        }

        if let Some(entry) = self.sides[endpoint.side().index()].pop() {
            // The reply token, if the sender expects an answer. The nonce is
            // assigned here — at delivery — so that no token exists before the
            // receiver was handed one, and therefore none can be guessed.
            let reply_token = match entry.reply_slot {
                Some(index) => self.deliver_reply_token(index),
                None => None,
            };

            // A plain send completes now that its message has been taken.
            if let Some(sender) = entry.waiter {
                self.completions
                    .push(Completion::SendCompleted { waiter: sender });
            }

            return OpResult::Ready(Received {
                message: entry.message,
                reply_token,
            });
        }

        self.sides[endpoint.side().index()].receiver = Some(thread);
        OpResult::Blocked
    }

    /// Send a message and wait for its reply.
    ///
    /// This is a `send` plus an outstanding reply slot. The reply slot is
    /// allocated *before* the message is sent, so the receiver is always
    /// handed a token that names a real slot.
    ///
    /// The call always ends up blocked: either because the message is still
    /// waiting to be collected, or because the reply has not arrived yet.
    /// Either way the kernel parks the thread and delivers
    /// [`Completion::ReplyReady`] or [`Completion::Failed`] later.
    pub fn call(
        &mut self,
        endpoint: &Endpoint,
        message: Message,
        transfer: &mut dyn CapabilityTransfer,
        thread: u32,
    ) -> OpResult<()> {
        if let Err(error) = self.check_common(endpoint, Rights::SEND) {
            return OpResult::Error(error);
        }
        if !message.validate() {
            return OpResult::Error(ErrorCode::Invalid);
        }

        let peer = endpoint.side().opposite();

        if !self.sides[peer.index()].has_space() {
            return OpResult::Error(ErrorCode::WouldBlock);
        }

        let slot_index = match self.allocate_reply_slot(endpoint.side(), thread) {
            Ok(index) => index,
            Err(error) => return OpResult::Error(error),
        };

        let message = match self.transfer_caps(message, transfer) {
            Ok(message) => message,
            Err(error) => {
                // The call never happened, so its slot is dead.
                self.replies[slot_index as usize].state = ReplyState::Cancelled;
                return OpResult::Error(error);
            }
        };

        let waiting = self.sides[peer.index()].receiver.take();
        self.sides[peer.index()].push(QueueEntry {
            used: true,
            message,
            // A `call` does not wait for delivery; it waits for its reply.
            waiter: None,
            reply_slot: Some(slot_index),
        });

        if let Some(receiver) = waiting {
            self.completions
                .push(Completion::ReceiveReady { waiter: receiver });
        }

        OpResult::Blocked
    }

    /// Answer a call.
    ///
    /// The token is consumed by this call: the slot moves out of
    /// [`ReplyState::Delivered`], so a second reply with the same token finds
    /// nothing to consume and fails.
    ///
    /// The reply must come from the side that received the request. A token
    /// presented by the wrong side is refused, which is what stops a caller
    /// from answering its own question.
    pub fn reply(
        &mut self,
        endpoint: &Endpoint,
        token: ReplyToken,
        message: Message,
    ) -> OpResult<()> {
        if let Err(error) = self.check_common(endpoint, Rights::REPLY) {
            return OpResult::Error(error);
        }

        if token.channel() != self.id {
            return OpResult::Error(ErrorCode::Invalid);
        }
        if !message.validate() {
            return OpResult::Error(ErrorCode::Invalid);
        }

        let index = token.index() as usize;
        if index >= MAX_REPLY_SLOTS {
            return OpResult::Error(ErrorCode::Invalid);
        }
        let slot = self.replies[index];
        if slot.nonce != token.nonce() {
            return OpResult::Error(ErrorCode::Invalid);
        }
        if slot.replier_side != endpoint.side() {
            return OpResult::Error(ErrorCode::Denied);
        }

        match slot.state {
            ReplyState::Delivered => {}
            ReplyState::Pending => {
                // The request has not been collected yet, so no token was
                // ever handed out and the nonce cannot have been known.
                return OpResult::Error(ErrorCode::Invalid);
            }
            ReplyState::TimedOut => {
                // The caller gave up. The reply is discarded rather than
                // delivered to whoever holds the slot next.
                return OpResult::Error(ErrorCode::Timeout);
            }
            ReplyState::Replied => {
                // The token was already spent. This is the one-shot rule.
                return OpResult::Error(ErrorCode::Invalid);
            }
            ReplyState::Vacant | ReplyState::Cancelled => {
                return OpResult::Error(ErrorCode::Invalid);
            }
        }

        self.replies[index].state = ReplyState::Replied;
        self.completions.push(Completion::ReplyReady {
            waiter: slot.caller,
            message,
        });
        OpResult::Ready(())
    }

    /// Give up on an outstanding call.
    ///
    /// The reply slot is invalidated, so a reply that arrives afterwards is
    /// discarded. Without this, a late reply could be delivered to an
    /// unrelated waiter that happens to hold the reused slot.
    ///
    /// Returns [`ErrorCode::Timeout`] if the reply had already arrived — the
    /// caller cannot un-receive an answer.
    pub fn expire_call(&mut self, token: ReplyToken) -> OpResult<()> {
        if token.channel() != self.id {
            return OpResult::Error(ErrorCode::Invalid);
        }
        let index = token.index() as usize;
        if index >= MAX_REPLY_SLOTS {
            return OpResult::Error(ErrorCode::Invalid);
        }
        let slot = self.replies[index];
        if slot.nonce != token.nonce() {
            return OpResult::Error(ErrorCode::Invalid);
        }
        match slot.state {
            ReplyState::Pending | ReplyState::Delivered => {}
            ReplyState::Replied => return OpResult::Error(ErrorCode::Invalid),
            ReplyState::TimedOut => return OpResult::Error(ErrorCode::Timeout),
            ReplyState::Vacant | ReplyState::Cancelled => {
                return OpResult::Error(ErrorCode::Invalid);
            }
        }
        self.replies[index].state = ReplyState::TimedOut;
        self.completions.push(Completion::Failed {
            waiter: slot.caller,
            error: ErrorCode::Timeout,
        });
        OpResult::Ready(())
    }

    /// The token naming a thread's outstanding call, if it has one.
    ///
    /// This is what the kernel uses when a caller's deadline expires: it
    /// looks the caller up, takes the token, and hands it to
    /// [`Channel::expire_call`]. Exposing the lookup rather than a bare
    /// "expire whatever this thread is waiting for" keeps the kernel from
    /// having to track the token itself.
    pub fn call_token(&self, caller_side: Side, thread: u32) -> Option<ReplyToken> {
        for index in 0..MAX_REPLY_SLOTS {
            let slot = self.replies[index];
            if slot.caller == thread
                && slot.caller_side == caller_side
                && matches!(slot.state, ReplyState::Pending | ReplyState::Delivered)
            {
                return Some(ReplyToken::from_parts(self.id, index as u16, slot.nonce));
            }
        }
        None
    }

    /// Time out a thread's outstanding call.
    ///
    /// The convenience form of [`Channel::call_token`] plus
    /// [`Channel::expire_call`], for the kernel's timeout path.
    pub fn cancel_call(&mut self, caller_side: Side, thread: u32) -> OpResult<()> {
        let Some(token) = self.call_token(caller_side, thread) else {
            return OpResult::Error(ErrorCode::Invalid);
        };
        self.expire_call(token)
    }

    /// Kill one side of the channel.
    ///
    /// Everything waiting on the dead peer is woken with
    /// [`ErrorCode::PeerDied`]: parked senders, parked receivers, and callers
    /// whose replier died. This is the mechanism behind "a client blocked in
    /// `call` is woken when the server exits".
    pub fn kill_side(&mut self, side: Side) {
        if !self.sides[side.index()].alive {
            return;
        }
        self.sides[side.index()].alive = false;
        let peer = side.opposite();

        // Messages the *surviving* side sent that nobody collected: their
        // senders are parked and their peer is gone, so they are woken.
        for index in 0..self.sides[side.index()].inbox_len {
            let entry = self.sides[side.index()].inbox[index];
            if let Some(sender) = entry.waiter {
                self.completions.push(Completion::Failed {
                    waiter: sender,
                    error: ErrorCode::PeerDied,
                });
            }
        }
        self.sides[side.index()].clear();

        // Messages the *dead* side sent, waiting to be collected by the
        // survivor: the sender is gone, so nothing may be completed on its
        // behalf and no reply may be made to them.
        for index in 0..self.sides[peer.index()].inbox_len {
            let entry = self.sides[peer.index()].inbox[index];
            if let Some(sender) = entry.waiter {
                self.completions.push(Completion::Failed {
                    waiter: sender,
                    error: ErrorCode::PeerDied,
                });
                self.sides[peer.index()].inbox[index].waiter = None;
            }
        }

        // The surviving side's parked receiver: its peer is gone.
        if let Some(receiver) = self.sides[peer.index()].receiver.take() {
            self.completions.push(Completion::Failed {
                waiter: receiver,
                error: ErrorCode::PeerDied,
            });
        }

        // The dead side's own parked receiver: it was waiting on a side that
        // will never produce a message again, so it must be woken rather
        // than left parked — a parked thread nobody will ever wake is a hung
        // thread, which is the worst outcome the peer-death path exists to
        // prevent.
        //
        // The waiter is not necessarily the dead task's own thread: a task
        // may hold a receive view of its peer's endpoint, and that waiter
        // belongs to a task that is still alive and must be told why its
        // wait ended.
        if let Some(receiver) = self.sides[side.index()].receiver.take() {
            self.completions.push(Completion::Failed {
                waiter: receiver,
                error: ErrorCode::PeerDied,
            });
        }

        // Outstanding calls: a caller whose replier died is woken, and a
        // caller that died itself has its slot cleaned up.
        for index in 0..MAX_REPLY_SLOTS {
            let slot = self.replies[index];
            if !matches!(slot.state, ReplyState::Pending | ReplyState::Delivered) {
                continue;
            }
            if slot.replier_side == side {
                self.completions.push(Completion::Failed {
                    waiter: slot.caller,
                    error: ErrorCode::PeerDied,
                });
                self.replies[index].state = ReplyState::Cancelled;
            } else if slot.caller_side == side {
                self.replies[index].state = ReplyState::Cancelled;
            }
        }
    }

    /// The checks every operation shares.
    ///
    /// The failures are kept apart because they mean different things to a
    /// caller: the endpoint is not this channel's (a programming error), the
    /// caller lacks the right (a policy outcome), and the peer is gone (a
    /// fact about the world).
    fn check_common(&self, endpoint: &Endpoint, needed: Rights) -> Result<(), ErrorCode> {
        if endpoint.channel() != self.id {
            return Err(ErrorCode::Invalid);
        }
        if !endpoint.has(needed) {
            return Err(ErrorCode::Denied);
        }
        if !self.sides[endpoint.side().index()].alive {
            return Err(ErrorCode::Closed);
        }
        if !self.peer_alive(endpoint.side()) {
            return Err(ErrorCode::PeerDied);
        }
        Ok(())
    }

    /// Transfer the message's capabilities and rewrite it with the
    /// destination-side handles.
    ///
    /// The transfer is atomic, so a failure leaves the sender's capability
    /// set untouched and the message carries no capabilities.
    fn transfer_caps(
        &mut self,
        mut message: Message,
        transfer: &mut dyn CapabilityTransfer,
    ) -> Result<Message, ErrorCode> {
        if !message.has_caps() {
            return Ok(message);
        }
        let mut source = [CapHandle::INVALID; crate::message::MAX_CAPS_PER_MSG];
        let count = message.cap_count();
        source[..count].copy_from_slice(&message.caps()[..count]);

        let transferred = transfer_atomic(transfer, &source[..count])?;
        message.set_caps(transferred.handles());
        Ok(message)
    }

    /// Take a reply slot for a new call.
    fn allocate_reply_slot(&mut self, caller_side: Side, caller: u32) -> Result<u16, ErrorCode> {
        // Prefer a slot that holds nothing; a spent slot is reclaimed only
        // when there is no free one, so a stale token keeps referring to a
        // spent slot for as long as possible.
        let mut fallback = None;
        for index in 0..MAX_REPLY_SLOTS {
            match self.replies[index].state {
                ReplyState::Vacant => {
                    self.replies[index] = ReplySlot {
                        state: ReplyState::Pending,
                        nonce: 0,
                        caller_side,
                        replier_side: caller_side.opposite(),
                        caller,
                    };
                    return Ok(index as u16);
                }
                ReplyState::Replied | ReplyState::Cancelled | ReplyState::TimedOut => {
                    if fallback.is_none() {
                        fallback = Some(index);
                    }
                }
                ReplyState::Pending | ReplyState::Delivered => {}
            }
        }
        let index = fallback.ok_or(ErrorCode::Quota)?;
        self.replies[index] = ReplySlot {
            state: ReplyState::Pending,
            nonce: 0,
            caller_side,
            replier_side: caller_side.opposite(),
            caller,
        };
        Ok(index as u16)
    }

    /// Hand out the token for a delivered call.
    ///
    /// The nonce is assigned here — at delivery — so that no token exists
    /// before the receiver was handed one, and therefore none can be guessed
    /// or replayed against a different slot.
    ///
    /// Returns `None` when the call is no longer live: the caller gave up or
    /// died before the message was collected, and there is nobody to reply
    /// to.
    fn deliver_reply_token(&mut self, index: u16) -> Option<ReplyToken> {
        let slot = self.replies[index as usize];
        match slot.state {
            ReplyState::Pending => {
                let nonce = self.next_nonce(index);
                self.replies[index as usize].state = ReplyState::Delivered;
                self.replies[index as usize].nonce = nonce;
                Some(ReplyToken::from_parts(self.id, index, nonce))
            }
            _ => {
                self.replies[index as usize].state = ReplyState::Cancelled;
                None
            }
        }
    }

    /// The next nonce for a reply token.
    ///
    /// The slot index is mixed in so that two slots handed out tokens at the
    /// same time cannot share a nonce, and the counter never repeats within a
    /// channel's life.
    fn next_nonce(&mut self, index: u16) -> u32 {
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        self.nonce_counter ^ ((index as u32) << 24)
    }
}

impl Default for Channel {
    fn default() -> Self {
        Self::new(ChannelId::new(0))
    }
}
