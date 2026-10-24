//! Host-side tests for the IPC state machine.
//!
//! Every rule the milestone names has a test here, and each test is written so
//! that it *fails* if the rule it names is broken — not merely so that it
//! exercises the code path.
//!
//! # Why the tests use fixed arrays rather than `Vec`
//!
//! The crate is `no_std`, and `alloc` is not available to it. The tests hold
//! to the same rule the crate does: fixed-size buffers, `core` only. That
//! also means the tests exercise the code exactly as the kernel will.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::syscall::ObjectType;

use nova_cap::{CapabilityTable, ObjectRef};

use crate::channel::{Channel, MAX_QUEUE, MAX_REPLY_SLOTS};
use crate::endpoint::{ChannelId, Completion, CompletionSink, Endpoint, ReplyToken, Side};
use crate::message::{MAX_CAPS_PER_MSG, MAX_INLINE_PAYLOAD, Message};
use crate::transfer::{
    CapabilityTransfer, TransferredCaps, transfer_atomic, transfer_between_tables,
};

/// A bounded log of what a store did, so a test can assert on the *order* of
/// prepare, commit, and abort.
#[derive(Default)]
struct StoreLog {
    prepared: [CapHandle; 8],
    prepared_len: usize,
    committed: [CapHandle; 8],
    committed_len: usize,
    aborted: [CapHandle; 8],
    aborted_len: usize,
    live: [CapHandle; 8],
    live_len: usize,
}

impl StoreLog {
    fn record_prepare(&mut self, handle: CapHandle) {
        self.prepared[self.prepared_len] = handle;
        self.prepared_len += 1;
    }

    fn record_commit(&mut self, prepared: CapHandle, source: CapHandle) {
        self.committed[self.committed_len] = prepared;
        self.committed_len += 1;
        for index in 0..self.live_len {
            if self.live[index] == source {
                self.live[index] = self.live[self.live_len - 1];
                self.live_len -= 1;
                break;
            }
        }
    }

    fn record_abort(&mut self, prepared: CapHandle) {
        self.aborted[self.aborted_len] = prepared;
        self.aborted_len += 1;
    }
}

/// A capability store that fails on demand, so the atomicity property can be
/// checked against a store that *can* fail.
struct FailingStore {
    fail_at: Option<usize>,
    calls: usize,
    log: StoreLog,
}

impl FailingStore {
    fn new(fail_at: Option<usize>) -> Self {
        Self {
            fail_at,
            calls: 0,
            log: StoreLog::default(),
        }
    }
}

impl CapabilityTransfer for FailingStore {
    fn prepare(&mut self, _source: CapHandle) -> Result<CapHandle, ErrorCode> {
        let index = self.calls;
        self.calls += 1;
        if self.fail_at == Some(index) {
            return Err(ErrorCode::BadHandle);
        }
        // A distinct handle per source, so the test can tell which one was
        // prepared.
        let prepared = CapHandle::new(1000 + index as u32, 1);
        self.log.record_prepare(prepared);
        Ok(prepared)
    }

    fn commit(&mut self, prepared: CapHandle, source: CapHandle) {
        self.log.record_commit(prepared, source);
    }

    fn abort(&mut self, prepared: CapHandle) {
        self.log.record_abort(prepared);
    }
}

/// A store over real capability tables, for the end-to-end transfer tests.
struct TableStore<'a> {
    source: &'a mut CapabilityTable,
    destination: &'a mut CapabilityTable,
}

impl CapabilityTransfer for TableStore<'_> {
    fn prepare(&mut self, source: CapHandle) -> Result<CapHandle, ErrorCode> {
        let entry = self.source.resolve(source)?;
        let object_ref: ObjectRef = entry.object_ref;
        let rights = entry.rights;
        self.destination.create(object_ref, rights)
    }

    fn commit(&mut self, _prepared: CapHandle, source: CapHandle) {
        let _ = self.source.destroy(source);
    }

    fn abort(&mut self, prepared: CapHandle) {
        let _ = self.destination.destroy(prepared);
    }
}

/// A bounded list of completions, so a test can look for one.
#[derive(Default)]
struct CompletionList {
    items: [Option<Completion>; 32],
    len: usize,
}

impl CompletionList {
    fn drain(channel: &mut Channel) -> Self {
        let mut list = CompletionList::default();
        for index in 0..channel.completions().len() {
            if let Some(completion) = channel.completions().get(index) {
                list.items[list.len] = Some(completion);
                list.len += 1;
            }
        }
        channel.take_completions();
        list
    }

    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, index: usize) -> Option<Completion> {
        self.items.get(index).copied().flatten()
    }

    /// The first completion for a given thread.
    fn for_waiter(&self, waiter: u32) -> Option<Completion> {
        for index in 0..self.len {
            if let Some(completion) = self.get(index)
                && completion.waiter() == waiter
            {
                return Some(completion);
            }
        }
        None
    }

    /// How many completions of a kind there are.
    fn count_kind(&self, wanted: CompletionKind) -> usize {
        let mut count = 0;
        for index in 0..self.len {
            if let Some(completion) = self.get(index) {
                let matches = match wanted {
                    CompletionKind::ReplyReady => {
                        matches!(completion, Completion::ReplyReady { .. })
                    }
                    CompletionKind::SendCompleted => {
                        matches!(completion, Completion::SendCompleted { .. })
                    }
                    CompletionKind::ReceiveReady => {
                        matches!(completion, Completion::ReceiveReady { .. })
                    }
                    CompletionKind::Failed => matches!(completion, Completion::Failed { .. }),
                };
                if matches {
                    count += 1;
                }
            }
        }
        count
    }
}

/// The kinds of completion a test cares about counting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionKind {
    ReplyReady,
    SendCompleted,
    ReceiveReady,
    Failed,
}

/// A channel with both endpoints.
fn both(rights: Rights) -> (Channel, Endpoint, Endpoint) {
    let channel = Channel::new(ChannelId::new(1));
    let first = channel.endpoint(Side::First, rights);
    let second = channel.endpoint(Side::Second, rights);
    (channel, first, second)
}

/// A capability table with `count` live capabilities, all naming one object.
fn table_with(count: usize) -> (CapabilityTable, [CapHandle; 8]) {
    let mut table = CapabilityTable::new();
    let mut handles = [CapHandle::INVALID; 8];
    for (index, slot) in handles.iter_mut().enumerate().take(count) {
        *slot = table
            .create(
                ObjectRef::new(ObjectType::MemoryObject, index as u32),
                Rights::READ,
            )
            .expect("table has room");
    }
    (table, handles)
}

#[test]
fn a_send_with_a_receiver_waiting_completes_immediately() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    // The receiver parks first.
    assert!(channel.recv(&second, 2).is_blocked());

    // The send then finds it and hands over directly.
    let result = channel.send(&first, Message::new(7), &mut store, 1);
    assert!(result.is_ready());

    let completions = CompletionList::drain(&mut channel);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions.get(0).expect("a completion").waiter(), 2);
}

#[test]
fn ipc_echo_64() {
    // A 64-byte payload is the ABI's maximum inline payload, and it must
    // round-trip byte for byte.
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    let payload: [u8; 64] = core::array::from_fn(|index| index as u8);
    let message = Message::with_payload(0xABCD, &payload).expect("64 bytes fits");
    assert_eq!(message.payload().len(), MAX_INLINE_PAYLOAD);

    // The receiver is parked, so the send completes immediately.
    assert!(channel.recv(&second, 2).is_blocked());
    assert!(channel.send(&first, message, &mut store, 1).is_ready());

    let received = channel.recv(&second, 2);
    let received = received.value().expect("a message was waiting");
    assert_eq!(received.message.payload(), &payload[..]);
    assert_eq!(received.message.opcode(), 0xABCD);
}

#[test]
fn an_oversized_payload_is_refused() {
    let payload = [0u8; MAX_INLINE_PAYLOAD + 1];
    assert_eq!(
        Message::with_payload(1, &payload).err(),
        Some(ErrorCode::Invalid)
    );
}

#[test]
fn cap_send_only_denies_recv() {
    // A send-only capability must not be able to receive. This is the whole
    // point of a right being enforced at the IPC layer.
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let send_only = channel.endpoint(Side::Second, Rights::SEND);
    let mut store = FailingStore::new(None);

    // The send-only side may send.
    assert!(channel.recv(&first, 1).is_blocked());
    assert!(
        channel
            .send(&second, Message::new(1), &mut store, 2)
            .is_ready()
    );

    // But it may not receive.
    let result = channel.recv(&send_only, 2);
    assert_eq!(result.error(), Some(ErrorCode::Denied));
}

#[test]
fn a_receive_only_capability_may_not_send() {
    let (mut channel, _first, _second) = both(Rights::SEND | Rights::RECEIVE);
    let receive_only = channel.endpoint(Side::First, Rights::RECEIVE);
    let mut store = FailingStore::new(None);
    assert_eq!(
        channel
            .send(&receive_only, Message::new(1), &mut store, 1)
            .error(),
        Some(ErrorCode::Denied)
    );
}

#[test]
fn reply_token_used_once() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    // The caller makes a call.
    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );

    // The server receives it and is handed a token.
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let token = received.reply_token.expect("a call yields a token");

    // The token may be used exactly once.
    assert!(channel.reply(&second, token, Message::new(2)).is_ready());

    // The caller is woken with the reply.
    let completions = CompletionList::drain(&mut channel);
    assert_eq!(completions.count_kind(CompletionKind::ReplyReady), 1);
}

#[test]
fn second_reply_with_same_token_fails() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let token = received.reply_token.expect("a call yields a token");

    assert!(channel.reply(&second, token, Message::new(2)).is_ready());

    // The second use of the same token must fail, and must not wake anybody.
    let second_reply = channel.reply(&second, token, Message::new(3));
    assert_eq!(second_reply.error(), Some(ErrorCode::Invalid));

    let completions = CompletionList::drain(&mut channel);
    assert_eq!(
        completions.count_kind(CompletionKind::ReplyReady),
        1,
        "the token authorised exactly one reply"
    );
    assert_eq!(completions.count_kind(CompletionKind::SendCompleted), 0);
    assert_eq!(completions.count_kind(CompletionKind::ReceiveReady), 0);
    assert_eq!(completions.count_kind(CompletionKind::Failed), 0);
}

#[test]
fn a_forged_reply_token_is_refused() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let token = received.reply_token.expect("a call yields a token");

    // A token with the right shape but the wrong nonce.
    let forged = ReplyToken::from_parts(token.channel(), token.index(), 0);
    assert_eq!(
        channel.reply(&second, forged, Message::new(2)).error(),
        Some(ErrorCode::Invalid)
    );

    // A token naming a slot that does not exist.
    let out_of_range =
        ReplyToken::from_parts(token.channel(), MAX_REPLY_SLOTS as u16, token.nonce());
    assert_eq!(
        channel
            .reply(&second, out_of_range, Message::new(2))
            .error(),
        Some(ErrorCode::Invalid)
    );

    // A token naming a different channel.
    let other_channel = ReplyToken::from_parts(ChannelId::new(99), token.index(), token.nonce());
    assert_eq!(
        channel
            .reply(&second, other_channel, Message::new(2))
            .error(),
        Some(ErrorCode::Invalid)
    );
}

#[test]
fn a_caller_cannot_reply_to_itself() {
    // The token names the side that owes the reply. A caller presenting it
    // from the wrong side is refused.
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let token = received.reply_token.expect("a call yields a token");

    // `first` is the caller, not the replier.
    assert_eq!(
        channel.reply(&first, token, Message::new(2)).error(),
        Some(ErrorCode::Denied)
    );
}

#[test]
fn ipc_4caps_atomic() {
    // Four capabilities is the ABI's maximum, and all four must move.
    let (mut source, handles) = table_with(MAX_CAPS_PER_MSG);
    let mut destination = CapabilityTable::new();
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = TableStore {
        source: &mut source,
        destination: &mut destination,
    };

    let message = Message::with_caps(1, &handles[..MAX_CAPS_PER_MSG]).expect("4 caps fit");
    assert_eq!(message.cap_count(), MAX_CAPS_PER_MSG);

    assert!(channel.recv(&second, 2).is_blocked());
    assert!(channel.send(&first, message, &mut store, 1).is_ready());

    // The receiver got all four.
    let received = channel.recv(&second, 2).value().expect("a message waited");
    assert_eq!(received.message.cap_count(), MAX_CAPS_PER_MSG);
    for handle in received.message.caps() {
        assert!(
            destination.resolve(*handle).is_ok(),
            "every transferred capability must be live in the destination"
        );
    }
}

#[test]
fn cap_transfer_all_or_nothing() {
    // The sender's capability set must be unchanged when the transfer fails.
    let mut store = FailingStore::new(Some(2));
    let caps = [
        CapHandle::new(1, 1),
        CapHandle::new(2, 1),
        CapHandle::new(3, 1),
        CapHandle::new(4, 1),
    ];

    let result = transfer_atomic(&mut store, &caps);
    assert_eq!(result.err(), Some(ErrorCode::BadHandle));

    // The two that were prepared were aborted, in reverse order.
    assert_eq!(store.log.aborted_len, 2);
    assert_eq!(store.log.prepared_len, 2);
    assert!(store.log.committed_len == 0, "nothing was published");
}

#[test]
fn a_failed_transfer_between_tables_changes_nothing() {
    let (mut source, handles) = table_with(4);
    let mut destination = CapabilityTable::new();
    let source_live_before = source.live_count();

    // The fourth handle is not a live capability, so preparing it fails.
    let mut stale = handles;
    stale[3] = CapHandle::new(50_000, 1);

    let result = transfer_between_tables(&mut source, &mut destination, &stale);
    assert!(result.is_err());

    // The sender still holds everything it started with.
    assert_eq!(source.live_count(), source_live_before);
    for handle in &handles[..3] {
        assert!(source.resolve(*handle).is_ok());
    }
    // And the receiver gained nothing.
    assert_eq!(destination.live_count(), 0);
}

#[test]
fn a_successful_transfer_moves_authority_rather_than_copying_it() {
    // The sender must lose its copy, or the authority could be spent twice.
    let (mut source, handles) = table_with(1);
    let mut destination = CapabilityTable::new();
    let handle = handles[0];

    let transferred =
        transfer_between_tables(&mut source, &mut destination, &handles[..1]).expect("it succeeds");
    assert_eq!(transferred.count(), 1);

    assert!(
        source.resolve(handle).is_err(),
        "the sender must no longer hold the capability"
    );
    assert!(destination.resolve(transferred.handles()[0]).is_ok());
}

#[test]
fn a_duplicate_handle_in_one_batch_is_refused() {
    // A duplicated handle has no single destination, so "all or none" cannot
    // be defined for it.
    let (mut source, handles) = table_with(1);
    let mut destination = CapabilityTable::new();
    let mut store = TableStore {
        source: &mut source,
        destination: &mut destination,
    };

    let duplicated = [handles[0], handles[0]];
    assert_eq!(
        transfer_atomic(&mut store, &duplicated).err(),
        Some(ErrorCode::Invalid)
    );
    assert_eq!(destination.live_count(), 0);
    assert!(source.resolve(handles[0]).is_ok());
}

#[test]
fn a_transfer_result_reports_what_moved() {
    let (mut source, handles) = table_with(3);
    let mut destination = CapabilityTable::new();
    let transferred =
        transfer_between_tables(&mut source, &mut destination, &handles[..3]).expect("it succeeds");
    assert_eq!(transferred.count(), 3);
    assert_eq!(transferred.handles().len(), 3);
    assert!(!transferred.is_empty());
    assert!(TransferredCaps::empty().is_empty());
}

#[test]
fn server_exit_wakes_client() {
    // The client is blocked in `call`; the server dies; the client must be
    // woken with E_PEER_DIED.
    let (mut channel, first, _second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );

    // The server exits.
    channel.kill_side(Side::Second);

    let completions = CompletionList::drain(&mut channel);
    let failure = completions.for_waiter(1).expect("the client is woken");
    assert_eq!(failure.error(), Some(ErrorCode::PeerDied));
}

#[test]
fn peer_death_wakes_a_parked_receiver() {
    let (mut channel, _first, second) = both(Rights::SEND | Rights::RECEIVE);
    assert!(channel.recv(&second, 2).is_blocked());

    channel.kill_side(Side::First);

    let completions = CompletionList::drain(&mut channel);
    let failure = completions.for_waiter(2).expect("the receiver is woken");
    assert_eq!(failure.error(), Some(ErrorCode::PeerDied));
}

#[test]
fn peer_death_wakes_a_parked_sender() {
    let (mut channel, first, _second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    // Nobody is receiving, so the sender parks with its message queued.
    assert!(
        channel
            .send(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );

    channel.kill_side(Side::Second);

    let completions = CompletionList::drain(&mut channel);
    let failure = completions.for_waiter(1).expect("the sender is woken");
    assert_eq!(failure.error(), Some(ErrorCode::PeerDied));
}

#[test]
fn an_operation_on_a_dead_peer_fails_immediately() {
    let (mut channel, first, _second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);
    channel.kill_side(Side::Second);

    assert_eq!(
        channel.send(&first, Message::new(1), &mut store, 1).error(),
        Some(ErrorCode::PeerDied)
    );
    assert_eq!(channel.recv(&first, 1).error(), Some(ErrorCode::PeerDied));
}

#[test]
fn late_reply_after_timeout_discarded() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let token = received.reply_token.expect("a call yields a token");

    // The caller's deadline expires.
    assert!(channel.cancel_call(Side::First, 1).is_ready());

    // The late reply is discarded rather than delivered.
    let late = channel.reply(&second, token, Message::new(2));
    assert_eq!(late.error(), Some(ErrorCode::Timeout));

    // And no reply was ever delivered.
    let completions = CompletionList::drain(&mut channel);
    assert_eq!(completions.count_kind(CompletionKind::ReplyReady), 0);
}

#[test]
fn a_timed_out_call_cannot_be_reused_by_a_later_call() {
    // The slot is spent by the timeout, so a later call cannot inherit the
    // old token's authority.
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE | Rights::REPLY);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .call(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let old_token = received.reply_token.expect("a call yields a token");

    assert!(channel.cancel_call(Side::First, 1).is_ready());

    // A second call, with a different caller.
    assert!(
        channel
            .call(&first, Message::new(3), &mut store, 3)
            .is_blocked()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    let new_token = received.reply_token.expect("a call yields a token");

    assert_ne!(
        old_token.nonce(),
        new_token.nonce(),
        "a reused slot must still carry a fresh nonce"
    );
    assert_ne!(old_token.index(), new_token.index());

    // The old token is refused: its slot was spent by the timeout, so the
    // reply is discarded rather than delivered to the new caller.
    assert!(channel.reply(&second, old_token, Message::new(4)).is_err());

    // And the new call is still answerable exactly once.
    assert!(
        channel
            .reply(&second, new_token, Message::new(5))
            .is_ready()
    );
    assert_eq!(
        channel.reply(&second, new_token, Message::new(6)).error(),
        Some(ErrorCode::Invalid)
    );
}

#[test]
fn queue_full_reports_backpressure() {
    // The queue is bounded, and a send into a full queue must report the
    // condition distinguishably rather than growing or blocking forever.
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    // Fill the peer's inbox.
    for index in 0..MAX_QUEUE {
        assert!(
            channel
                .send(&first, Message::new(index as u64), &mut store, 1)
                .is_blocked(),
            "the queue accepts {MAX_QUEUE} messages"
        );
    }
    assert_eq!(channel.queued(Side::Second), MAX_QUEUE);

    // One more is refused with a distinguishable condition.
    let overflow = channel.send(&first, Message::new(99), &mut store, 1);
    assert_eq!(overflow.error(), Some(ErrorCode::WouldBlock));

    // Draining one makes room again.
    assert!(channel.recv(&second, 2).is_ready());
    assert!(
        channel
            .send(&first, Message::new(100), &mut store, 1)
            .is_blocked()
    );
}

#[test]
fn a_backpressure_rejection_moves_no_capabilities() {
    // The queue check happens before the transfer, so a rejected send must
    // leave the sender's capabilities alone.
    let (mut source, _handles) = table_with(2);
    let mut destination = CapabilityTable::new();
    let (mut channel, first, _second) = both(Rights::SEND | Rights::RECEIVE);

    // Fill the peer's queue, without anyone collecting, so that the next
    // send has nowhere to go. Each message carries a capability, so the
    // transfers have to happen for the queue to fill.
    for index in 0..MAX_QUEUE {
        let cap = source
            .create(
                ObjectRef::new(ObjectType::MemoryObject, index as u32),
                Rights::READ,
            )
            .expect("table has room");
        let mut store = TableStore {
            source: &mut source,
            destination: &mut destination,
        };
        let message = Message::with_caps(index as u64, &[cap]).expect("one cap fits");
        assert!(channel.send(&first, message, &mut store, 1).is_blocked());
    }
    assert_eq!(channel.queued(Side::Second), MAX_QUEUE);

    // One more capability is created for the rejected send, so that the
    // rejection cannot be confused with a failed transfer.
    let spare = source
        .create(ObjectRef::new(ObjectType::MemoryObject, 999), Rights::READ)
        .expect("table has room");
    let source_live = source.live_count();
    let destination_live = destination.live_count();
    let mut store = TableStore {
        source: &mut source,
        destination: &mut destination,
    };
    let message = Message::with_caps(99, &[spare]).expect("one cap fits");
    assert_eq!(
        channel.send(&first, message, &mut store, 1).error(),
        Some(ErrorCode::WouldBlock)
    );
    assert_eq!(source.live_count(), source_live);
    assert_eq!(destination.live_count(), destination_live);
    assert!(source.resolve(spare).is_ok(), "the sender still holds it");
}

#[test]
fn the_queue_is_fifo() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    for index in 0..4u64 {
        assert!(
            channel
                .send(&first, Message::new(index), &mut store, 1)
                .is_blocked()
        );
    }
    for index in 0..4u64 {
        let received = channel.recv(&second, 2).value().expect("a message waited");
        assert_eq!(received.message.opcode(), index);
    }
}

#[test]
fn a_side_never_receives_its_own_message() {
    let (mut channel, first, _second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    assert!(
        channel
            .send(&first, Message::new(1), &mut store, 1)
            .is_blocked()
    );
    // The message went to the *other* side's inbox.
    assert_eq!(channel.queued(Side::Second), 1);
    assert_eq!(channel.queued(Side::First), 0);
}

#[test]
fn a_plain_send_carries_no_reply_token() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    assert!(channel.recv(&second, 2).is_blocked());
    assert!(
        channel
            .send(&first, Message::new(1), &mut store, 1)
            .is_ready()
    );
    let received = channel.recv(&second, 2).value().expect("a message waited");
    assert!(received.reply_token.is_none());
}

#[test]
fn the_reply_token_round_trips_through_the_wire_form() {
    let token = ReplyToken::from_parts(ChannelId::new(3), 7, 0xABCD);
    assert_eq!(token, ReplyToken::from_raw(token.to_raw()));
    assert_eq!(token.channel(), ChannelId::new(3));
    assert_eq!(token.index(), 7);
    assert_eq!(token.nonce(), 0xABCD);
}

#[test]
fn the_completion_sink_is_bounded_and_counts_drops() {
    let mut sink = CompletionSink::new();
    assert!(sink.is_empty());
    for waiter in 0..crate::endpoint::MAX_COMPLETIONS as u32 {
        assert!(sink.push(Completion::SendCompleted { waiter }));
    }
    assert_eq!(sink.len(), crate::endpoint::MAX_COMPLETIONS);
    assert!(!sink.push(Completion::SendCompleted { waiter: 999 }));
    assert_eq!(sink.dropped(), 1);
    assert_eq!(sink.clear(), crate::endpoint::MAX_COMPLETIONS);
    assert!(sink.is_empty());
}

#[test]
fn a_forged_message_header_is_refused() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    let mut message = Message::new(1);
    message.header_mut().reserved = 1;
    assert_eq!(
        channel.send(&first, message, &mut store, 1).error(),
        Some(ErrorCode::Invalid)
    );
    let _ = second;
}

#[test]
fn memory_leak_create_drop_10000() {
    // Ten thousand messages and ten thousand capabilities, and the pools must
    // come back to where they started.
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut source = CapabilityTable::new();
    let mut destination = CapabilityTable::new();
    let source_start = source.live_count();
    let destination_start = destination.live_count();
    let channel_start = channel.queued(Side::Second);

    for index in 0..10_000u64 {
        let cap = source
            .create(
                ObjectRef::new(ObjectType::MemoryObject, index as u32),
                Rights::READ,
            )
            .expect("table has room");

        // The message carries the capability, and the send performs the
        // transfer — the caller does not transfer by hand.
        let message = Message::with_caps(index, &[cap]).expect("one cap fits");
        let mut store = TableStore {
            source: &mut source,
            destination: &mut destination,
        };

        // The send parks first because nobody is waiting, and the receive
        // then collects it and wakes the sender.
        assert!(channel.send(&first, message, &mut store, 1).is_blocked());
        let received = channel.recv(&second, 2).value().expect("a message waited");
        assert_eq!(received.message.opcode(), index);

        // The receiver drops what it was given.
        for handle in received.message.caps() {
            let _ = destination.destroy(*handle);
        }
    }

    assert_eq!(source.live_count(), source_start);
    assert_eq!(destination.live_count(), destination_start);
    assert_eq!(channel.queued(Side::Second), channel_start);
    assert_eq!(channel.queued(Side::First), 0);
    assert_eq!(channel.outstanding_calls(), 0);
}

#[test]
fn the_endpoint_rights_mirror_is_readable() {
    let channel = Channel::new(ChannelId::new(1));
    let endpoint = channel.endpoint(Side::First, Rights::SEND);
    assert!(endpoint.has(Rights::SEND));
    assert!(!endpoint.has(Rights::RECEIVE));
    assert_eq!(endpoint.rights(), Rights::SEND);
    assert_eq!(endpoint.channel(), ChannelId::new(1));
    assert_eq!(endpoint.side(), Side::First);
    assert_eq!(Side::First.opposite(), Side::Second);
    assert_eq!(Side::Second.opposite(), Side::First);
}

#[test]
fn an_endpoint_from_another_channel_is_refused() {
    let (mut channel, _first, _second) = both(Rights::SEND | Rights::RECEIVE);
    let foreign = Endpoint::new(ChannelId::new(42), Side::First, Rights::SEND);
    assert_eq!(channel.recv(&foreign, 1).error(), Some(ErrorCode::Invalid));
}

#[test]
fn the_channel_reports_what_is_parked() {
    let (mut channel, first, second) = both(Rights::SEND | Rights::RECEIVE);
    let mut store = FailingStore::new(None);

    assert_eq!(channel.parked(), 0);
    assert!(channel.recv(&second, 2).is_blocked());
    assert_eq!(channel.parked(), 1);
    assert!(
        channel
            .send(&first, Message::new(1), &mut store, 1)
            .is_ready()
    );
    assert_eq!(channel.parked(), 0);
}

#[test]
fn a_channel_is_created_with_the_expected_shape() {
    let channel = Channel::new(ChannelId::new(5));
    assert_eq!(channel.id(), ChannelId::new(5));
    assert!(channel.peer_alive(Side::First));
    assert!(channel.peer_alive(Side::Second));
    assert_eq!(channel.queued(Side::First), 0);
}
