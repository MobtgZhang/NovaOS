//! Endpoints, sides, reply tokens, and the results of operations.
//!
//! # Why an endpoint is a (channel, side) pair
//!
//! Two endpoints form a channel. A message sent on one side of a channel is
//! received on the other, and *only* on the other: a side never receives what
//! it sent. Modelling a channel as one object with two sides keeps that rule
//! structural instead of a check that could be forgotten.
//!
//! # Why an endpoint carries rights
//!
//! Rights live on the *capability* that names an endpoint, but the IPC layer
//! is where a right is finally enforced. The endpoint therefore carries a
//! mirror of the rights its holder has, so the IPC layer can refuse a `recv`
//! on a send-only capability without the kernel having to re-check the
//! capability table on the fast path. The mirror is authoritative for the
//! IPC layer only; the kernel must still obtain the real rights from
//! `nova-cap` when it resolves the capability.

use nova_abi::cap::Rights;
use nova_abi::error::ErrorCode;

pub use crate::message::Message;

/// Identifies one IPC channel.
///
/// The kernel allocates channel numbers; userspace never names one directly,
/// it names a capability that resolves to one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ChannelId(u32);

impl ChannelId {
    /// Build a channel number.
    pub const fn new(number: u32) -> Self {
        Self(number)
    }

    /// The channel number.
    pub const fn number(self) -> u32 {
        self.0
    }
}

/// One side of a channel.
///
/// The distinction is what keeps a side from receiving its own messages, and
/// what tells the kernel which peer to blame when one side dies.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Side {
    /// The first side of the channel.
    First,
    /// The second side of the channel.
    Second,
}

impl Side {
    /// The other side of the same channel.
    pub const fn opposite(self) -> Side {
        match self {
            Side::First => Side::Second,
            Side::Second => Side::First,
        }
    }

    /// The side as an array index.
    pub const fn index(self) -> usize {
        match self {
            Side::First => 0,
            Side::Second => 1,
        }
    }

    /// A stable tag for logs.
    pub const fn tag(self) -> &'static str {
        match self {
            Side::First => "first",
            Side::Second => "second",
        }
    }
}

/// A view of a channel from one side, carrying the rights of the capability
/// that named it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Endpoint {
    channel: ChannelId,
    side: Side,
    rights: Rights,
}

impl Endpoint {
    /// Bind an endpoint to a channel side with a set of rights.
    ///
    /// The rights mirror what the holder's capability carries; the IPC layer
    /// refuses operations the rights do not cover.
    pub const fn new(channel: ChannelId, side: Side, rights: Rights) -> Self {
        Self {
            channel,
            side,
            rights,
        }
    }

    /// The channel this endpoint belongs to.
    pub const fn channel(&self) -> ChannelId {
        self.channel
    }

    /// Which side of the channel this endpoint is.
    pub const fn side(&self) -> Side {
        self.side
    }

    /// The rights mirrored from the holder's capability.
    pub const fn rights(&self) -> Rights {
        self.rights
    }

    /// Whether this endpoint carries a right.
    pub const fn has(&self, right: Rights) -> bool {
        self.rights.contains(right)
    }
}

/// The result of one IPC operation.
///
/// The three cases are genuinely different, and the kernel must tell them
/// apart: `Ready` returns a value to the caller, `Blocked` parks the thread
/// until a completion arrives, and `Error` returns immediately with a code
/// the caller can act on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpResult<T> {
    /// The operation completed immediately.
    Ready(T),
    /// The operation is parked; a completion will arrive later.
    Blocked,
    /// The operation failed.
    Error(ErrorCode),
}

impl<T> OpResult<T> {
    /// Whether the operation completed immediately.
    pub const fn is_ready(&self) -> bool {
        matches!(self, OpResult::Ready(_))
    }

    /// Whether the operation parked.
    pub const fn is_blocked(&self) -> bool {
        matches!(self, OpResult::Blocked)
    }

    /// Whether the operation failed.
    pub const fn is_err(&self) -> bool {
        matches!(self, OpResult::Error(_))
    }

    /// The error, if the operation failed.
    pub const fn error(&self) -> Option<ErrorCode> {
        match self {
            OpResult::Error(code) => Some(*code),
            _ => None,
        }
    }

    /// The value, if the operation completed.
    pub fn value(self) -> Option<T> {
        match self {
            OpResult::Ready(value) => Some(value),
            _ => None,
        }
    }

    /// Map the value of a successful result.
    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> OpResult<U> {
        match self {
            OpResult::Ready(value) => OpResult::Ready(map(value)),
            OpResult::Blocked => OpResult::Blocked,
            OpResult::Error(code) => OpResult::Error(code),
        }
    }
}

/// A message together with the authorisation to reply to it.
///
/// The token is present only when the sender used `call` — a plain `send` is
/// fire-and-forget and expects no reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Received {
    /// The message that was taken off the queue.
    pub message: Message,
    /// The one-shot authorisation to reply, when the sender expects one.
    pub reply_token: Option<ReplyToken>,
}

/// The one-shot authorisation to reply to a `call`.
///
/// # Why it cannot be replayed
///
/// The token names a *slot* in the channel's reply table, and the channel
/// checks four things before accepting a reply: that the slot is still
/// `Pending`, that the nonce matches the one the channel generated for that
/// request, that the channel matches, and that the replier is on the side that
/// received the request.
///
///
/// The nonce is derived from a per-channel secret the kernel supplies and
/// userspace never sees, so a holder of a token for one request cannot mint a
/// token for another — including one for a request it made itself, which
/// would let it answer its own question. Consuming the reply moves the slot
/// out of `Pending`, so the second use of the same token fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReplyToken {
    channel: ChannelId,
    index: u16,
    nonce: u32,
}

impl ReplyToken {
    /// Build a token from its parts.
    ///
    /// The kernel uses this to reconstruct a token from the wire; the nonce
    /// is still checked against the channel's table, so a forged value only
    /// produces a rejected reply.
    pub const fn from_parts(channel: ChannelId, index: u16, nonce: u32) -> Self {
        Self {
            channel,
            index,
            nonce,
        }
    }

    /// The channel the token belongs to.
    ///
    /// A token presented to the wrong channel is refused, so a token cannot be
    /// moved between channels.
    pub const fn channel(&self) -> ChannelId {
        self.channel
    }

    /// The reply-table slot the token names.
    pub const fn index(&self) -> u16 {
        self.index
    }

    /// The nonce the token carries.
    pub const fn nonce(&self) -> u32 {
        self.nonce
    }

    /// The token as one 64-bit word, for the syscall boundary.
    pub const fn to_raw(self) -> u64 {
        ((self.channel.number() as u64 & 0xFFFF) << 48)
            | ((self.index as u64 & 0xFFFF) << 32)
            | self.nonce as u64
    }

    /// Recover a token from the wire.
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            channel: ChannelId(((raw >> 48) & 0xFFFF) as u32),
            index: ((raw >> 32) & 0xFFFF) as u16,
            nonce: (raw & 0xFFFF_FFFF) as u32,
        }
    }
}

/// What a parked thread is woken with.
///
/// The channel does not call into the kernel: it records the fact, and the
/// kernel asks for the completions and wakes the threads itself. That split
/// keeps this crate free of any scheduler dependency.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Completion {
    /// A parked sender's message was taken by a receiver.
    SendCompleted {
        /// The parked sender's thread identifier.
        waiter: u32,
    },
    /// A message is waiting for a parked receiver.
    ///
    /// The payload is *not* included: the receiver collects it by calling
    /// `recv` again, so the payload always arrives inline with the receive
    /// and never in a wakeup notification. That keeps one rule for where a
    /// message comes from, whichever path delivered it.
    ReceiveReady {
        /// The parked receiver's thread identifier.
        waiter: u32,
    },
    /// A parked caller was handed its reply.
    ReplyReady {
        /// The parked caller's thread identifier.
        waiter: u32,
        /// The reply message.
        message: Message,
    },
    /// A parked operation failed.
    Failed {
        /// The parked thread's identifier.
        waiter: u32,
        /// Why it failed.
        error: ErrorCode,
    },
}

impl Completion {
    /// The thread identifier this completion is for.
    pub const fn waiter(&self) -> u32 {
        match self {
            Completion::SendCompleted { waiter } => *waiter,
            Completion::ReceiveReady { waiter, .. } => *waiter,
            Completion::ReplyReady { waiter, .. } => *waiter,
            Completion::Failed { waiter, .. } => *waiter,
        }
    }

    /// The error, if the completion is a failure.
    pub const fn error(&self) -> Option<ErrorCode> {
        match self {
            Completion::Failed { error, .. } => Some(*error),
            _ => None,
        }
    }
}

/// A bounded sink for completions.
///
/// The kernel owns the sink and drains it after every syscall. It is bounded
/// because the kernel has no allocator, and it is explicit — rather than
/// buffered inside the channel — because a completion that overflowed a
/// hidden buffer would mean a thread that never wakes, which is far worse
/// than a dropped wakeup the kernel can notice.
#[derive(Clone, Copy, Debug)]
pub struct CompletionSink {
    items: [Option<Completion>; MAX_COMPLETIONS],
    len: usize,
    /// How many completions were dropped because the sink was full.
    dropped: usize,
}

impl CompletionSink {
    /// An empty sink.
    pub const fn new() -> Self {
        Self {
            items: [None; MAX_COMPLETIONS],
            len: 0,
            dropped: 0,
        }
    }

    /// Record a completion.
    ///
    /// Returns `false` when the sink was full and the completion was
    /// dropped. A kernel that treats a `false` as fatal will never lose a
    /// wakeup silently.
    pub fn push(&mut self, completion: Completion) -> bool {
        if self.len == MAX_COMPLETIONS {
            self.dropped += 1;
            return false;
        }
        self.items[self.len] = Some(completion);
        self.len += 1;
        true
    }

    /// How many completions are held.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the sink holds nothing.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many completions were dropped.
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    /// The completion at an index.
    pub fn get(&self, index: usize) -> Option<Completion> {
        self.items.get(index).copied().flatten()
    }

    /// Empty the sink, returning how many completions it held.
    pub fn clear(&mut self) -> usize {
        let count = self.len;
        self.items = [None; MAX_COMPLETIONS];
        self.len = 0;
        count
    }
}

impl Default for CompletionSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum completions one sink can hold.
///
/// A single operation generates at most two completions, and a peer death at
/// most one per parked waiter, so this is generous for a sink the kernel
/// drains after every syscall.
pub const MAX_COMPLETIONS: usize = 32;
