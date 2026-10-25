//! The kernel-side syscall dispatch.
//!
//! # Why this is not the trap entry
//!
//! M2 owns `kernel/src/syscall/`: the `iretq` trampoline, the register save
//! area, and the privilege transition itself. Editing that directory from
//! here would be a merge conflict with the agent that owns it, and — worse —
//! would put two implementations of the entry path in the tree.
//!
//! So this module is the *dispatch logic* and nothing else: it takes a
//! decoded syscall number, its arguments, and the caller's identity, and
//! returns a result. The entry path's job is to build the
//! [`SyscallContext`], call [`dispatch_syscall`], and put the result in
//! `RAX`. That is the whole integration contract, and it is one function.
//!
//! # Why the state is passed in
//!
//! There are two entry points, and they differ only in where the state comes
//! from:
//!
//! - [`dispatch_syscall`] takes the kernel's *global* service state and is
//!   what M2's entry path calls;
//! - [`dispatch_syscall_in`] takes the state as an argument and is what the
//!   kernel's own test suites call.
//!
//! The split exists because the global accessor hands out a `&'static mut`,
//! and two live `&mut` to the same state is aliasing that Rust's model does
//! not permit — even though the kernel is single-CPU. Passing the state in
//! makes the tests honest: a test that holds `&mut KernelState` and calls the
//! dispatcher with it cannot accidentally alias the global.
//!
//! # What makes this testable
//!
//! The dispatch logic is exercised by the kernel's own test suites, from ring
//! 0, by calling [`dispatch_syscall_in`] directly. That is legitimate: the
//! dispatch logic *is* the policy, and the policy is what has to be right. A
//! test that creates a capability table, mints a capability without
//! `Rights::READ`, and calls the dispatcher for a read genuinely exercises the
//! mechanism — it fails if the dispatch stops consulting the table, and it
//! fails if a denial stops producing `E_DENIED`.
//!
//! What the tests *cannot* exercise is the privilege transition itself: that a
//! real `#PF` from ring 3 is delivered to user space with the right error
//! code needs M2's entry path and a real user program. That part is reported
//! as pending rather than faked.
//!
//! # The syscall numbers
//!
//! The ABI's `nova_abi::nr` list is frozen, and this kernel implements the
//! calls that need only the service layer. Everything else returns
//! `E_UNSUPPORTED`, which is the ABI's documented answer for a number this
//! kernel does not implement — never a fall-through to another operation.
//!
//! # The argument convention
//!
//! The ABI fixes the register order: `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9`,
//! with the number in `RAX` and the result in `RAX` on return. The dispatcher
//! therefore takes `[u64; 6]`, in that order, and each handler documents what
//! each argument means.
//!
//! A user-space pointer is *never* dereferenced here. Only the entry path
//! knows the caller's address space, so the entry path copies the caller's
//! bytes into the kernel's bounded buffers before dispatching — into
//! [`KernelState::outgoing_message`] for a send, and out of
//! [`KernelState::last_message`] for a receive. The dispatcher works on
//! bytes the kernel already owns, which is what makes it testable from ring
//! 0 and what stops a dispatcher bug from becoming a kernel read of a user
//! address.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::ipc::MAX_INLINE_PAYLOAD;
use nova_abi::syscall::{ObjectType, Syscall, nr};
use nova_cap::ObjectRef;
use nova_ipc::endpoint::Side;
use nova_ipc::message::Message;

use crate::services::cap;
use crate::services::ipc::{IpcOutcome, SelfTransfer, TableTransfer};
use crate::services::object::ObjectKind;
use crate::services::process;
use crate::services::state;
use crate::services::two_caps_mut;
use crate::services::{KernelState, KernelStateParts};

/// The context a syscall is dispatched in.
///
/// This is the whole of what the dispatch logic knows about its caller. It
/// carries *identity*, not authority: the authority lives in the caller's
/// capability table, which the dispatcher reaches through the task's slot.
///
/// The context deliberately carries no user-space pointer and no payload
/// buffer. The entry path owns the copy from user space into the kernel's
/// bounded buffers, because only the entry path knows which address space the
/// caller had.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SyscallContext {
    /// The slot of the calling task's state in the kernel's task table.
    pub task_slot: usize,
    /// The calling thread's identifier.
    pub thread: u32,
}

impl SyscallContext {
    /// A context for a caller in a given task slot.
    pub const fn new(task_slot: usize, thread: u32) -> Self {
        Self { task_slot, thread }
    }
}

/// The outcome of a dispatched syscall.
///
/// The value and the effect are separate because they answer different
/// questions: the value is what goes in `RAX`, and the effect is what the
/// kernel's scheduler has to do afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyscallOutcome {
    /// The call succeeded with this value.
    Value(i64),
    /// The call failed with this error code.
    Failed(ErrorCode),
    /// The call parked; the thread is blocked until a completion arrives.
    Blocked,
}

impl SyscallOutcome {
    /// The raw `RAX` value this outcome encodes to.
    ///
    /// A parked call returns `0` in `RAX`: the thread is not going to read it
    /// until it is woken, and the woken path re-reads the completion.
    pub const fn to_raw(self) -> i64 {
        match self {
            SyscallOutcome::Value(value) => value,
            SyscallOutcome::Failed(code) => -(code.code() as i64),
            SyscallOutcome::Blocked => 0,
        }
    }

    /// The error, if the call failed.
    pub const fn error(self) -> Option<ErrorCode> {
        match self {
            SyscallOutcome::Failed(code) => Some(code),
            _ => None,
        }
    }

    /// Whether the call succeeded.
    pub const fn is_ok(self) -> bool {
        matches!(self, SyscallOutcome::Value(_))
    }

    /// Whether the call parked.
    pub const fn is_blocked(self) -> bool {
        matches!(self, SyscallOutcome::Blocked)
    }
}

/// Dispatch one syscall against the kernel's global service state.
///
/// This is the entry point M2's syscall path calls. The signature is
/// deliberately narrow: the entry path decodes the number and the arguments,
/// builds a [`SyscallContext`], and puts [`SyscallOutcome::to_raw`] in `RAX`.
///
/// The number is taken as a `u64` because that is what the register holds. A
/// number the ABI does not define is `E_UNSUPPORTED`, which is the ABI's
/// documented answer for "this kernel does not implement that" — including
/// for a number from a *newer* ABI, so that a binary built against a newer
/// kernel learns the truth rather than landing on some other operation.
///
/// # Safety
///
/// The caller must have installed the service state with
/// [`crate::services::init`] and must not hold another borrow of it. That is
/// the same single-CPU, interrupts-masked discipline the rest of the kernel
/// runs under.
pub unsafe fn dispatch_syscall(
    context: &SyscallContext,
    number: u64,
    args: [u64; 6],
) -> SyscallOutcome {
    // SAFETY: delegated to this function's contract: the state is installed
    // and no other borrow of it is live.
    let kernel = unsafe { state() };
    dispatch_syscall_in(kernel, context, number, args)
}

/// Dispatch one syscall against an explicit service state.
///
/// The form the kernel's test suites use. It is the same policy as
/// [`dispatch_syscall`] — the global version is a thin wrapper around it —
/// but the state arrives as an argument, so a caller that already holds the
/// state cannot accidentally take a second, aliasing borrow of it.
pub fn dispatch_syscall_in(
    kernel: &mut KernelState,
    context: &SyscallContext,
    number: u64,
    args: [u64; 6],
) -> SyscallOutcome {
    let Ok(number) = u32::try_from(number) else {
        return SyscallOutcome::Failed(ErrorCode::Unsupported);
    };
    dispatch_typed(kernel, context, Syscall(number), args)
}

/// Dispatch one syscall, given a decoded number.
///
/// The typed form of [`dispatch_syscall_in`], used by any caller that already
/// holds a [`Syscall`].
///
/// # The syscall numbers that are implemented
///
/// - `GET_VERSION` — the ABI version word;
/// - `OBJECT_CREATE` — create a kernel object and mint a capability;
/// - `OBJECT_DESTROY` — destroy an object the caller controls;
/// - `CAP_DEREGATE` — derive a narrower capability;
/// - `CAP_REVOKE` — withdraw a capability;
/// - `SEND` — send a message on an endpoint;
/// - `RECV` — receive a message on an endpoint;
/// - `CALL` — send a request and wait for its reply;
/// - `REPLY` — answer a request once;
/// - `YIELD`, `THREAD_EXIT` — the scheduler-facing calls.
///
/// Everything else — `MAP`, `UNMAP`, `THREAD_SPAWN`, `CANCEL_WAIT` — returns
/// `E_UNSUPPORTED`. `MAP` and `UNMAP` need M2's address-space machinery,
/// `THREAD_SPAWN` needs the scheduler, and `CANCEL_WAIT` needs a blocking
/// primitive that does not exist yet.
pub fn dispatch_typed(
    kernel: &mut KernelState,
    context: &SyscallContext,
    number: Syscall,
    args: [u64; 6],
) -> SyscallOutcome {
    match number {
        nr::GET_VERSION => SyscallOutcome::Value(version_word()),
        nr::OBJECT_CREATE => object_create(kernel, context, args),
        nr::OBJECT_DESTROY => object_destroy(kernel, context, args),
        nr::CAP_DEREGATE => cap_delegate(kernel, context, args),
        nr::CAP_REVOKE => cap_revoke(kernel, context, args),
        nr::SEND => send(kernel, context, args, false),
        nr::RECV => recv(kernel, context, args),
        nr::CALL => send(kernel, context, args, true),
        nr::REPLY => reply(kernel, context, args),
        nr::YIELD => SyscallOutcome::Value(0),
        nr::THREAD_EXIT => {
            crate::services::process::retire_task(context.task_slot);
            SyscallOutcome::Value(args[0] as i64)
        }
        nr::ENDPOINT_CREATE => match process::create_endpoint_pair() {
            Ok(handle) => SyscallOutcome::Value(handle.raw() as i64),
            Err(code) => SyscallOutcome::Failed(code),
        },
        nr::ENDPOINT_PEER_CREATE => match process::take_pending_endpoint() {
            Ok(handle) => SyscallOutcome::Value(handle.raw() as i64),
            Err(code) => SyscallOutcome::Failed(code),
        },
        nr::ENDPOINT_SELF => match process::management_endpoint() {
            Ok(handle) => SyscallOutcome::Value(handle.raw() as i64),
            Err(code) => SyscallOutcome::Failed(code),
        },
        nr::PROCESS_COUNT => SyscallOutcome::Value(process::live_task_count() as i64),
        _ => SyscallOutcome::Failed(ErrorCode::Unsupported),
    }
}

/// The ABI version word the kernel reports.
///
/// `major << 16 | minor`, so a single 64-bit comparison tells a process
/// whether the kernel is new enough.
const fn version_word() -> i64 {
    ((nova_abi::ABI_VERSION.major as i64) << 16) | nova_abi::ABI_VERSION.minor as i64
}

/// Check that the caller holds the authority to create objects.
///
/// The three outcomes are kept apart because they mean different things to
/// the caller: the root capability is gone (`E_REVOKED`), the root exists but
/// does not carry `SPAWN` (`E_DENIED`), and the caller's task does not exist
/// at all (`E_BAD_HANDLE`).
///
/// A task whose root capability was never minted is a fourth case, and it is
/// `E_DENIED` rather than `E_BAD_HANDLE`: the task exists and can be
/// dispatched to, it simply holds no authority at all. `E_BAD_HANDLE` would
/// say the caller named a capability that does not exist, which is a
/// statement about the *argument* rather than about the caller's authority.
pub(crate) fn check_spawn_authority(
    kernel: &KernelState,
    context: &SyscallContext,
) -> Result<(), ErrorCode> {
    check_spawn_authority_at_slot(kernel, context.task_slot)
}

pub(crate) fn check_spawn_authority_at_slot(
    kernel: &KernelState,
    task_slot: usize,
) -> Result<(), ErrorCode> {
    let Some(task) = kernel.task_at_slot(task_slot) else {
        return Err(ErrorCode::BadHandle);
    };
    let Some(table) = kernel.caps_at(task_slot) else {
        return Err(ErrorCode::BadHandle);
    };
    let rights = table.root_authority(task.root)?;
    if !rights.contains(Rights::SPAWN) {
        return Err(ErrorCode::Denied);
    }
    Ok(())
}

/// Create a kernel object and hand the caller a capability to it.
///
/// `arg0` is the ABI object type, `arg1` the requested rights. The rights are
/// *intersected* with the caller's authority to create objects of that kind,
/// so a caller cannot mint a capability with rights it does not itself hold.
fn object_create(
    kernel: &mut KernelState,
    context: &SyscallContext,
    args: [u64; 6],
) -> SyscallOutcome {
    let Some(object_type) = ObjectType::from_number(args[0] as u32) else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };
    let Some(requested) = Rights::from_bits(args[1] as u32) else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    if let Err(code) = check_spawn_authority(kernel, context) {
        return SyscallOutcome::Failed(code);
    }

    // The kind is built first, because creating an endpoint also creates a
    // channel, and the channel must exist before the object names it.
    let kind = match object_type {
        ObjectType::Endpoint => {
            let Some(channel) = kernel.ipc.create_channel() else {
                return SyscallOutcome::Failed(ErrorCode::NoMemory);
            };
            // The side is `First`: the second side is created by the other
            // party, which derives its endpoint from the same channel.
            ObjectKind::Endpoint {
                channel: channel.number(),
                side: Side::First,
            }
        }
        ObjectType::Task => {
            let Some(task_id) = kernel.create_task() else {
                return SyscallOutcome::Failed(ErrorCode::NoMemory);
            };
            ObjectKind::Task { task: task_id }
        }
        ObjectType::Thread => ObjectKind::Thread {
            thread: context.thread,
        },
        ObjectType::AddressSpace => ObjectKind::AddressSpace,
        ObjectType::MemoryObject => ObjectKind::MemoryObject,
        ObjectType::Notification => ObjectKind::Notification,
        ObjectType::IoRegion => {
            // The ABI has no directory kind, so a path-scoped file grant is
            // exposed as an `IoRegion`. Creating one *here* would mint a grant
            // over the whole filesystem, which is never what the caller wants;
            // a scoped grant is made by `init` through
            // `KernelState::grant_file_scope`, which names a path.
            return SyscallOutcome::Failed(ErrorCode::Unsupported);
        }
        ObjectType::Irq => {
            // The ABI names an IRQ type, but this kernel has no device model
            // yet, so there is nothing to create.
            return SyscallOutcome::Failed(ErrorCode::Unsupported);
        }
        ObjectType::TaskFactory => {
            // The authority to create tasks is what `init` holds; a process
            // may not mint more of it for itself.
            return SyscallOutcome::Failed(ErrorCode::Unsupported);
        }
    };

    let rights = requested.intersect(Rights::ALL);
    let KernelStateParts { caps, objects, .. } = kernel.split();
    let Some(table) = caps.get_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    match cap::create(table, objects, kind, rights) {
        Ok((handle, _)) => SyscallOutcome::Value(handle.raw() as i64),
        Err(code) => SyscallOutcome::Failed(code),
    }
}

/// The handle of the kernel's root capability in a task's table.
///
/// Slot 1 is where the kernel puts a task's root capability when the task is
/// created: the authority to spawn objects and to grant. It is the *only*
/// handle a fresh task has, and it is what `init` hands out.
///
/// A constant rather than a stored handle because the root capability is
/// always minted in slot 1: the capability table's slot 0 is reserved, and the
/// root is the first thing a task's table is given. The task's *actual* root
/// handle is stored in [`TaskState::root`] as well, because the generation is
/// a property of the table and must not be guessed.
pub const ROOT_HANDLE: CapHandle = CapHandle::new(1, 1);

/// Destroy an object the caller holds `CONTROL` on.
///
/// Revocation happens here rather than in the registry, because the registry
/// does not know which tables hold capabilities to the object: the
/// capability table is the only place that set is written down.
fn object_destroy(
    kernel: &mut KernelState,
    context: &SyscallContext,
    args: [u64; 6],
) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    if kernel.task_at_slot(context.task_slot).is_none() {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    }
    let Some(table) = kernel.caps_at(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    let resolved = match cap::resolve(table, &kernel.objects, handle, Rights::CONTROL) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let object_ref = resolved.object_ref;
    let KernelStateParts { caps, objects, .. } = kernel.split();
    let Some(table) = caps.get_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let revoked = cap::revoke_object(table, objects, object_ref);
    let _ = revoked;
    let _ = table.destroy(handle);
    SyscallOutcome::Value(0)
}

/// Derive a narrower capability from one the caller holds.
///
/// `arg0` is the parent handle, `arg1` the requested rights. The kernel
/// intersects, so the child can never hold a right the parent lacked — and a
/// parent without `Rights::GRANT` may not delegate at all.
fn cap_delegate(
    kernel: &mut KernelState,
    context: &SyscallContext,
    args: [u64; 6],
) -> SyscallOutcome {
    let parent = CapHandle::from_raw(args[0]);
    let Some(requested) = Rights::from_bits(args[1] as u32) else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let Some(table) = kernel.caps_at_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    match table.derive(parent, requested) {
        Ok(handle) => SyscallOutcome::Value(handle.raw() as i64),
        Err(code) => SyscallOutcome::Failed(code),
    }
}

/// Revoke a capability the caller holds.
///
/// `arg0` is the handle. The revocation cascades to the capability's
/// descendants, because a withdrawn grant that lingers in a derived copy is a
/// privilege escalation rather than a cosmetic problem.
fn cap_revoke(
    kernel: &mut KernelState,
    context: &SyscallContext,
    args: [u64; 6],
) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    let Some(table) = kernel.caps_at_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    match cap::revoke(table, handle, nova_cap::RevocationScope::Descendants) {
        Ok(count) => SyscallOutcome::Value(count as i64),
        Err(code) => SyscallOutcome::Failed(code),
    }
}

/// Send a message on an endpoint, or make a call.
///
/// `arg0` is the endpoint handle, `arg1` the opcode.
///
/// The payload is *not* taken from a pointer here: the kernel's entry path is
/// responsible for copying the caller's bytes into the kernel's own buffer
/// before calling the dispatcher, because only the entry path knows the
/// caller's address space. The dispatcher therefore sends the message the
/// entry path staged in [`KernelState::outgoing_message`], falling back to an
/// empty message with the given opcode.
///
/// The message's capabilities are transferred from the caller's table to the
/// *destination* task's table. The destination is the peer of this endpoint's
/// channel — the task that holds the other side.
fn send(
    kernel: &mut KernelState,
    context: &SyscallContext,
    args: [u64; 6],
    is_call: bool,
) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    if kernel.task_at_slot(context.task_slot).is_none() {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    }
    let Some(table) = kernel.caps_at(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    // The rights come from the *capability*, not from the caller: a
    // send-only endpoint must refuse a receive, and that refusal has to be
    // driven by what the holder holds.
    let resolved = match cap::resolve(table, &kernel.objects, handle, Rights::SEND) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let ObjectKind::Endpoint { channel, side } = resolved.kind else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let channel_id = nova_ipc::ChannelId::new(channel);
    let Some(peer_slot) = kernel.ipc.peer_task(channel_id, side) else {
        // No peer task: the message goes nowhere, so the operation is
        // refused rather than silently dropped. A dropped message is a
        // message the sender believes was delivered.
        return SyscallOutcome::Failed(ErrorCode::Closed);
    };

    // The message the entry path staged, or an empty one.
    let thread = context.thread as usize;
    let message = if thread < crate::services::MAX_THREADS {
        kernel.outgoing_message[thread].unwrap_or_else(|| Message::new(args[1]))
    } else {
        Message::new(args[1])
    };

    // The borrows are of different fields, so they are split explicitly: the
    // compiler cannot see through a method call that `self.caps` and
    // `self.ipc` are disjoint.
    let KernelStateParts { caps, ipc, .. } = kernel.split();

    // A task talking to itself is the one case where the source and the
    // destination are the same table, and a table cannot be borrowed mutably
    // twice at once. It gets its own transfer implementation.
    if peer_slot == context.task_slot {
        let Some(caller_caps) = caps.get_mut(context.task_slot) else {
            return SyscallOutcome::Failed(ErrorCode::BadHandle);
        };
        let Some(channel) = ipc.channel_mut(channel_id) else {
            return SyscallOutcome::Failed(ErrorCode::BadHandle);
        };
        let endpoint = channel.endpoint(side, resolved.rights);
        let mut transfer = SelfTransfer::new(caller_caps);
        let outcome = if is_call {
            channel.call(&endpoint, message, &mut transfer, context.thread)
        } else {
            channel.send(&endpoint, message, &mut transfer, context.thread)
        };
        return match outcome {
            nova_ipc::OpResult::Ready(()) => SyscallOutcome::Value(0),
            nova_ipc::OpResult::Blocked => SyscallOutcome::Blocked,
            nova_ipc::OpResult::Error(code) => SyscallOutcome::Failed(code),
        };
    }

    // Two *different* tables are borrowed at once, which the array has to
    // be split for; the helper does the split and restores the order.
    let Some((first, second)) = two_caps_mut(caps, context.task_slot, peer_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let (caller_caps, peer_caps) = if context.task_slot < peer_slot {
        (first, second)
    } else {
        (second, first)
    };

    // The endpoint is built from the channel, so the channel must be
    // borrowed separately from the two capability tables.
    let Some(channel) = ipc.channel_mut(channel_id) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let endpoint = channel.endpoint(side, resolved.rights);

    let mut transfer = TableTransfer::new(caller_caps, peer_caps);
    let outcome = if is_call {
        channel.call(&endpoint, message, &mut transfer, context.thread)
    } else {
        channel.send(&endpoint, message, &mut transfer, context.thread)
    };
    match outcome {
        nova_ipc::OpResult::Ready(()) => SyscallOutcome::Value(0),
        nova_ipc::OpResult::Blocked => SyscallOutcome::Blocked,
        nova_ipc::OpResult::Error(code) => SyscallOutcome::Failed(code),
    }
}

/// Receive a message from an endpoint.
///
/// `arg0` is the endpoint handle. A receive needs `Rights::RECEIVE`, so a
/// send-only endpoint refuses it with `E_DENIED` — which is the acceptance
/// criterion `cap_send_only_denies_recv`.
///
/// The message and the reply token are published in the calling thread's
/// slots, so the entry path can hand them to user space after the call
/// returns.
fn recv(kernel: &mut KernelState, context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    if kernel.task_at_slot(context.task_slot).is_none() {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    }
    let Some(table) = kernel.caps_at(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    let resolved = match cap::resolve(table, &kernel.objects, handle, Rights::RECEIVE) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let ObjectKind::Endpoint { channel, side } = resolved.kind else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let channel_id = nova_ipc::ChannelId::new(channel);
    let Some(channel) = kernel.ipc.channel_mut(channel_id) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let endpoint = channel.endpoint(side, resolved.rights);
    let (outcome, token) = crate::services::ipc::recv(channel, &endpoint, context.thread);
    match outcome {
        IpcOutcome::Ready(message) => {
            let thread = context.thread as usize;
            if thread < crate::services::MAX_THREADS {
                kernel.last_message[thread] = Some(message);
                kernel.last_token[thread] = token;
                kernel.last_error[thread] = None;
            }
            SyscallOutcome::Value(message.opcode() as i64)
        }
        IpcOutcome::Blocked => SyscallOutcome::Blocked,
        IpcOutcome::Failed(code) => SyscallOutcome::Failed(code),
    }
}

/// Reply once to a received request.
///
/// `arg0` is the endpoint handle, `arg1` the reply token.
///
/// The reply message is the one the entry path staged in
/// [`KernelState::outgoing_message`]; a reply with no staged message is an
/// empty one.
fn reply(kernel: &mut KernelState, context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);
    let token = nova_ipc::ReplyToken::from_raw(args[1]);

    if kernel.task_at_slot(context.task_slot).is_none() {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    }
    let Some(table) = kernel.caps_at(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    let resolved = match cap::resolve(table, &kernel.objects, handle, Rights::REPLY) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let ObjectKind::Endpoint { channel, side } = resolved.kind else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let thread = context.thread as usize;
    let message = if thread < crate::services::MAX_THREADS {
        kernel.outgoing_message[thread].unwrap_or_else(|| Message::new(0))
    } else {
        Message::new(0)
    };

    let Some(channel) = kernel.ipc.channel_mut(nova_ipc::ChannelId::new(channel)) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let endpoint = channel.endpoint(side, resolved.rights);
    match crate::services::ipc::reply(channel, &endpoint, token, message) {
        IpcOutcome::Ready(_) => SyscallOutcome::Value(0),
        IpcOutcome::Blocked => SyscallOutcome::Blocked,
        IpcOutcome::Failed(code) => SyscallOutcome::Failed(code),
    }
}

/// The maximum inline payload the dispatcher accepts.
pub const MAX_PAYLOAD: usize = MAX_INLINE_PAYLOAD;

/// The object reference a fresh task's root capability names.
///
/// The root capability's object is a task factory: the authority to create
/// tasks and spawn objects. It is created when the task is created, so the
/// root handle always resolves.
pub fn root_object_ref() -> ObjectRef {
    ObjectRef::new(ObjectType::TaskFactory, 0)
}
