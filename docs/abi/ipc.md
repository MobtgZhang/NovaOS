# NovaOS IPC semantics

This document records the IPC contract: what a message is, how a capability
travels with it, how a reply is authorised, and what happens when a peer
dies or a caller gives up.

**Status:** frozen for M3.

The state machine lives in `crates/nova-ipc`; the capability table it draws
authority from lives in `crates/nova-cap`; the fixed message layout and the
error codes live in `crates/nova-abi`. This document names the owning symbol
for each value and rule, rather than repeating them, because a rule copied
into prose is a rule that can drift from the code that enforces it.

## The model

Two endpoints form a channel. A message sent on one side of a channel is
received on the other, and *only* on the other.

That "only on the other" is structural rather than a check that could be
forgotten: the two sides have separate inboxes, and a send on side `S` writes
into side `opp(S)`'s inbox. `nova_ipc::Side` is the two-valued enumeration
that names the sides, and `nova_ipc::Endpoint` is a *view* of one side of a
channel carrying a mirror of the rights the holder's capability grants.

The endpoint mirrors the rights rather than looking them up, because the IPC
layer is where a right is finally enforced and the fast path should not have
to re-resolve the capability table on every operation. The mirror is
authoritative for the IPC layer only; the kernel still obtains the real rights
from `nova-cap` when it resolves the capability, and it is the kernel's
resolved rights that are handed to the endpoint.

### The four operations

| Operation | Behaviour | Typical use |
| --- | --- | --- |
| `send` | one-way message; the sender parks until a receiver takes it | logs, events |
| `recv` | wait for a message on the endpoint | a service's main loop |
| `call` | send a request and wait for a one-shot reply | file/device RPC |
| `reply` | answer a received request, exactly once | the service side of `call` |

`send` and `call` are *synchronous*. A message is not "sent" until a receiver
has taken it, so there are exactly three cases:

* the peer is already parked in `recv`: the message is queued and the peer is
  woken to collect it. The sender's send completes at once, because a waiting
  receiver is committed to taking it.
* the peer is not waiting: the message is queued and the *sender* parks. When
  the peer eventually takes the message, the sender is woken with
  `nova_ipc::Completion::SendCompleted`.
* the peer's queue is full: the send fails with `E_WOULD_BLOCK`.

A plain `send` is fire-and-forget in the sense that it expects no *answer* —
but it is not asynchronous in the sense that it does not return until the
message has been taken. `call` is the one that expects an answer, and it is
the one that carries a reply token.

### Bounded queues

Each inbox holds a fixed number of messages: `nova_ipc::channel::MAX_QUEUE`.
When it is full, a send reports `E_WOULD_BLOCK` rather than growing. The
kernel has no allocator, so a queue that cannot grow is the only honest
answer — and an unbounded queue is how a chatty peer turns into a hung kernel.

The same reason bounds the reply-slot table (`nova_ipc::MAX_REPLY_SLOTS`) and
the completion sink (`nova_ipc::endpoint::MAX_COMPLETIONS`).

## The message

### The header

Every message begins with `nova_abi::ipc::IpcHeader`: a `#[repr(C)]` structure
of exactly `nova_abi::ipc::IPC_HEADER_SIZE` bytes.

| Field | Meaning |
| --- | --- |
| `payload_len` | meaningful bytes in the inline payload |
| `cap_count` | valid handles in the capability array |
| `opcode` | caller-defined; its meaning belongs to `nova-protocols` |
| `reserved` | must be zero |

The layout is the contract, so it is pinned at compile time: the size and
each field's offset are asserted in `crates/nova-abi/src/ipc.rs`. A field
that moved, grew, or changed type would not be a compile error — the two sides
are separate binaries — so the assertion is what turns it into one.

`reserved` exists so a future revision can add a field without moving the
existing ones. A non-zero value is a protocol violation, not an extension
point: the kernel rejects it, because silently ignoring a byte the sender
thought was meaningful is how two sides end up disagreeing about a message.
`IpcHeader::validate` is the check, and the kernel runs it on a message that
arrived from user space, where the header may have been forged.

### The payload

The inline payload is bounded by `nova_abi::ipc::MAX_INLINE_PAYLOAD`. That
limit is small on purpose: a single IPC operation must have a bounded,
predictable copy cost, because the kernel has no allocator and every
rendezvous slot has to be a fixed size.

Larger data travels in a shared memory object whose handle goes in the
capability array. That is the whole reason the capability array exists as well
as the payload: the two limits together say "small data inline, large data by
capability".

`nova_ipc::message::Message` is the kernel's fixed-size carrier: the header
plus a `MAX_INLINE_PAYLOAD`-byte array plus a `MAX_CAPS_PER_MSG`-element
handle array. It has no allocator requirement and no variable-length anything.

### Capabilities

The capability list is bounded by `nova_abi::ipc::MAX_CAPS_PER_MSG`. Four is
chosen so that a message stays inside two cache lines and so that transferring
a capability — which must be atomic — cannot require an unbounded amount of
kernel state to stage.

## Atomic capability transfer

> A message that carries capabilities transfers **all of them or none of
> them**.

There is no state in which a receiver holds some of a batch and the sender
still holds the rest. A half-transferred batch is a batch in which the kernel
has lost track of who owns what, and capability ownership is exactly the thing
this kernel cannot afford to lose track of.

### How the atomicity is achieved

`nova_ipc::transfer::transfer_atomic` stages the batch:

1. **Prepare** every handle. Preparing mints the destination-side handle but
   leaves the source untouched, so a failure halfway through has changed
   nothing the sender can observe.
2. If any prepare failed, **abort** the ones already prepared, in reverse
   order. The sender's capability set is exactly what it was.
3. Otherwise **commit** them all. Committing is the point of no return and is
   infallible by contract, so the batch either lands whole or not at all.

The three operations are the `nova_ipc::CapabilityTransfer` trait:
`prepare`, `commit`, `abort`. The trait exists so the atomicity property can
be tested against a store that fails on demand, rather than inferred from the
channel's behaviour.

The kernel's implementations are `kernel/src/services/ipc.rs::TableTransfer`
(between two tasks' tables) and `::SelfTransfer` (within one table, for the
case where a task talks to itself through its own endpoint).

### Why the source is destroyed rather than copied

A transfer *moves* authority. If the sender kept its handle as well, a
capability could be spent twice: the sender could send the same handle to two
receivers, each of which would believe it held the only copy of an authority
that was meant to be exclusive. `Rights::CONTROL` on an object is exactly the
sort of authority that must not be duplicable.

The rights themselves are carried over unchanged. A transfer moves authority;
it does not widen or narrow it. Narrowing is `CapabilityTable::derive`'s job,
and the receiver can always derive a narrower capability from the one it was
given.

### A duplicated handle is refused outright

A batch that names the same handle twice is rejected before anything is
staged. A duplicated handle has no single destination, so "all or none" cannot
be defined for it.

### Backpressure happens before the transfer

The queue-space check in `Channel::send` and `Channel::call` happens *before*
the capability transfer, and it is unconditional — every send occupies a queue
slot, even one a waiting receiver is about to take. So a `E_WOULD_BLOCK`
rejection cannot have moved anything.

## Rights

Rights live on the *capability* that names an endpoint. They are a bit set
(`nova_abi::cap::Rights`), not an ordinal, because the central invariant needs
set comparison: `Rights(child) ⊆ Rights(parent)`.

The rights that matter to IPC are `SEND`, `RECEIVE` and `REPLY`. A send-only
capability cannot receive: the kernel resolves the capability with the right
the operation needs, and the endpoint refuses an operation its mirror does not
cover. That refusal is `E_DENIED`, and it is the acceptance criterion
`cap_send_only_denies_recv`.

`nova_ipc`'s channel keeps those failures apart, because they mean different
things to a caller: the endpoint is not this channel's (a programming error,
`E_INVALID`), the caller lacks the right (a policy outcome, `E_DENIED`), the
endpoint's own side is closed (`E_CLOSED`), and the peer is gone (a fact about
the world, `E_PEER_DIED`).

## `call` and `reply`: the one-shot reply token

`call` is a `send` that also expects an answer. The difference is a *reply
slot*: a table entry naming the caller and the side that owes the reply.

When the receiver collects the message, the channel assigns the slot a fresh
nonce and hands the receiver a `nova_ipc::ReplyToken` naming the slot and the
nonce. `Received` carries the message and, when the sender used `call`, the
token.

### Why a token cannot be replayed

The channel checks four things before accepting a reply:

* the slot is still `Delivered` — that is, the request was collected and the
  token was handed out;
* the nonce matches the one the channel generated for that request;
* the channel matches the token's channel;
* the replier is on the side that *received* the request.

The nonce is assigned **at delivery**, not at call time, and is never reused
within a channel's life: `Channel::next_nonce` advances a per-channel counter
and mixes in the slot index, so two slots handed out tokens at the same moment
cannot share a nonce. That is what makes a token unforgeable in practice: no
token exists before the receiver was handed one, so there is nothing to guess,
and a holder of a token for one request cannot mint a token for another —
including one for a request it made itself, which would let it answer its own
question. That last case is refused by the side check, and it is the
acceptance criterion `a_caller_cannot_reply_to_itself`.

Consuming the reply moves the slot out of `Delivered`, so the second use of
the same token fails. A second reply with the same token is `E_INVALID`: the
one-shot rule, and the acceptance criteria `reply_token_used_once` and
`second_reply_with_same_token_fails`.

### The token on the wire

`ReplyToken::to_raw` packs the channel, the slot index and the nonce into one
64-bit word, and `ReplyToken::from_raw` unpacks it. A forged value only
produces a rejected reply, because the nonce is still checked against the
channel's table.

### A plain `send` carries no token

`Received::reply_token` is `None` for a message that arrived by `send`. There
is nothing to answer, and no authority to answer it with.

## Peer death

> A client blocked in `call` is woken when the server exits.

The channel decides *what* a dead peer means for its waiters; the kernel
decides *when* it happens.

`nova_ipc::Channel::kill_side` wakes everything waiting on the dead side with
`E_PEER_DIED`:

* a parked sender whose message the dead side was going to collect;
* a parked receiver on the surviving side;
* a caller whose *replier* died — its outstanding call is woken with
  `E_PEER_DIED` and the slot is cancelled.

Messages the dead side sent, waiting to be collected by the survivor, are
handled with care: the sender is gone, so nothing may be completed on its
behalf and no reply may be made to them.

A caller's *own* outstanding call is cancelled rather than failed, because
there is nobody left to receive the result.

### How the kernel triggers it

The IPC layer knows channels, not tasks, so the kernel holds the mapping from
a task to the channel sides it is party to. When a task exits, the kernel
calls `kernel/src/services/ipc.rs::IpcRegistry::kill_task`, which kills that
task's side of every channel it held. The registry also clears the
task-to-side mapping, so a later send on that side cannot pick the dead task
as the destination for a capability transfer.

`kernel/src/services/mod.rs::KernelState::kill_task` is the entry point that
combines the task's recorded sides with the registry.

### After the peer dies

An operation on a channel whose peer is dead fails immediately with
`E_PEER_DIED` rather than parking — a caller that knows the peer is gone
should not have to wait for a wakeup that will never come.

## Timeouts and late replies

A parked caller may give up. Doing so *invalidates the reply slot*, so a
reply that arrives afterwards is discarded rather than delivered to whoever
holds the slot next. That is why the timeout is an explicit state transition
on the slot, and not "the waiter stopped looking".

The state machine's `TimedOut` state is what makes this work: a reply
presented against a timed-out slot is refused with `E_TIMEOUT` and is not
delivered to anyone. A timed-out slot is also not reused for a later call while
it can still be reclaimed — `Channel::allocate_reply_slot` prefers a vacant
slot and only reclaims a spent one when there is no free one — so a stale
token keeps referring to a spent slot for as long as possible.

`nova_ipc::Channel::cancel_call` is the convenience form the kernel's timeout
path uses: it finds the token naming a thread's outstanding call and expires
it, and the caller is woken with `E_TIMEOUT`.

Two things the timeout does *not* mean, and which the service layer rather than
the kernel must honour:

* **A timeout does not mean the remote operation did not happen.** A write may
  have completed and only its reply been lost. Services therefore need a
  request identifier or an explicit idempotence rule so that a client's retry
  does not duplicate a mutation.
* **A timeout does not cancel the work.** It cancels the *wait*.

## Completions and waking parked threads

The channel never calls into the kernel. A parked thread is recorded state,
and the wakeup is a `nova_ipc::Completion` the kernel asks for and acts on.
That split keeps `nova-ipc` free of any scheduler dependency and makes the
whole machine testable on the host.

| Completion | Meaning |
| --- | --- |
| `SendCompleted` | a parked sender's message was taken by a receiver |
| `ReceiveReady` | a message is waiting for a parked receiver |
| `ReplyReady` | a parked caller was handed its reply |
| `Failed` | a parked operation failed, with an error code |

`ReceiveReady` deliberately does **not** carry the payload: the receiver
collects it by calling `recv` again, so the payload always arrives inline with
the receive and never in a wakeup notification. That keeps one rule for where
a message comes from, whichever path delivered it.

The kernel drains the completions after every syscall
(`kernel/src/services/mod.rs::KernelState::drain_completions`, which delegates
to `kernel/src/services/ipc.rs::IpcRegistry::drain_completions`) and wakes the
threads they name. A completion that cannot be delivered because the output
array is full is *counted*, not dropped silently: a lost wakeup is a hung
thread, which is far worse than a reported overflow.

A `ReplyReady` completion carries the reply message with it, so the kernel
writes it into the waiting thread's message slot and clears its token slot; a
`Failed` completion carries the error code instead, and that goes into the
thread's error slot. That is the point at which the *kernel* owns the
message, and it is the same slot a `recv` fills — so a thread that is woken
by a reply finds its answer where it would have found a received message.

## The capability model this rests on

An endpoint is named by a capability, and the capability is the authority. The
handle is `nova_abi::cap::CapHandle`, one 64-bit word split into a
generation in the high half and a slot in the low half.

* Reusing a slot bumps its generation, so every handle naming the old slot
  stops validating. The generation is kernel-owned: userspace cannot forge
  one, because a slot index alone is not a valid handle and the kernel checks
  both halves.
* Slot 0 is reserved and never valid, so a forgotten initialisation is caught
  at the first syscall rather than resolving to whatever the kernel put in
  slot 0.
* Deriving a capability intersects the requested rights with the parent's, so a
  child can never hold a right the parent lacked.
* Revoking keeps the entry rather than freeing the slot, so the loss is
  *observable*: the handle still resolves and its state says it was revoked. A
  holder blocked on `CANCEL_WAIT` can therefore learn that it lost authority
  rather than being told the capability never existed.

The rights a holder has are the *only* authority it has. There is no global
namespace, no name lookup and no ambient permission: an operation that needs a
right asks for it through a capability, and the kernel's capability table is
the only thing that answers.

## See also

- `docs/abi/address-layout.md` — the address layout the IPC layer's copies
  happen inside.
- `docs/abi/syscalls-x86_64.md` — the calls that reach this layer.
- `crates/nova-abi/src/ipc.rs` — the header, and the payload and capability
  limits.
- `crates/nova-abi/src/cap.rs` — the handle layout and the rights set.
- `crates/nova-ipc/src/channel.rs` — the rendezvous state machine.
- `crates/nova-ipc/src/transfer.rs` — the atomic transfer.
- `crates/nova-cap/src/table.rs` — the capability table and its invariants.
- `kernel/src/services/ipc.rs` — the kernel's registry and transfer
  implementations.
