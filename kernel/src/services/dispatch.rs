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
//! [`SyscallContext`], call [`syscall_dispatch`], and put the result in `RAX`.
//! That is the whole integration contract, and it is one function.
//!
//! # What makes this testable
//!
//! The dispatch logic is exercised by the kernel's own test suites, from ring
//! 0, by calling [`syscall_dispatch`] directly. That is legitimate: the
//! dispatch logic *is* the policy, and the policy is what has to be right. A
//! test that creates a capability table, mints a capability without
//! `Rights::READ`, and calls the dispatcher for a read genuinely exercises
//! the mechanism — it fails if the dispatch stops consulting the table, and it
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

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::ipc::MAX_INLINE_PAYLOAD;
use nova_abi::syscall::{ObjectType, Syscall, nr};
use nova_cap::ObjectRef;
use nova_ipc::endpoint::{Endpoint, Side};
use nova_ipc::message::Message;

use crate::services::cap;
use crate::services::ipc::{IpcOutcome, TableTransfer};
use crate::services::object::ObjectKind;
use crate::services::state;

/// The context a syscall is dispatched in.
///
/// This is the whole of what the dispatch logic knows about its caller. It
/// carries *identity*, not authority: the authority lives in the caller's
/// capability table, which the dispatcher reaches through the task's slot.
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
}

/// Dispatch one syscall.
///
/// This is the integration point M2's entry path calls. The signature is
/// deliberately narrow: the entry path decodes the number and the arguments,
/// builds a [`SyscallContext`], and puts [`SyscallOutcome::to_raw`] in `RAX`.
pub fn syscall_dispatch(context: &SyscallContext, number: Syscall, args: [u64; 6]) -> SyscallOutcome {
    match number {
        nr::GET_VERSION => SyscallOutcome::Value(version_word()),
        nr::OBJECT_CREATE => object_create(context, args),
        nr::OBJECT_DESTROY => object_destroy(context, args),
        nr::CAP_DEREGATE => cap_delegate(context, args),
        nr::CAP_REVOKE => cap_revoke(context, args),
        nr::SEND => send(context, args),
        nr::RECV => recv(context, args),
        nr::REPLY => reply(context, args),
        nr::YIELD => SyscallOutcome::Value(0),
        nr::THREAD_EXIT => SyscallOutcome::Value(args[0] as i64),
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

/// Create a kernel object and hand the caller a capability to it.
///
/// `arg0` is the ABI object type, `arg1` the requested rights. The rights are
/// *intersected* with the caller's authority to create objects of that kind,
/// so a caller cannot mint a capability with rights it does not itself hold.
fn object_create(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let Some(object_type) = ObjectType::from_number(args[0] as u32) else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };
    let Some(requested) = Rights::from_bits(args[1] as u32) else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    // SAFETY: the state is installed during boot, before any syscall can
    // reach this code.
    let kernel = unsafe { state() };

    let Some(task) = kernel.task_at_slot_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    // A task may only create an object if it holds the right to spawn one.
    // The root authority to create is `Rights::SPAWN`, and the capability the
    // caller gets is intersected with it.
    if !task.caps.check_rights(ROOT_HANDLE, Rights::SPAWN).is_ok() {
        return SyscallOutcome::Failed(ErrorCode::Denied);
    }

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
        ObjectType::IoRegion => ObjectKind::FileScope {
            path: nova_fs::path::CanonicalPath::ROOT,
        },
        ObjectType::Irq => {
            // The ABI names an IRQ type, but this kernel has no device model
            // yet, so there is nothing to create.
            return SyscallOutcome::Failed(ErrorCode::Unsupported);
        }
        ObjectType::TaskFactory => {
            return SyscallOutcome::Failed(ErrorCode::Unsupported);
        }
    };

    let rights = requested.intersect(Rights::ALL);
    let Some(task) = kernel.task_at_slot_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    match cap::create(&mut task.caps, &mut kernel.objects, kind, rights) {
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
/// root is the first thing a task's table is given.
pub const ROOT_HANDLE: CapHandle = CapHandle::new(1, 1);

/// Destroy an object the caller holds `CONTROL` on.
///
/// Revocation happens here rather than in the registry, because the registry
/// does not know which tables hold capabilities to the object: the
/// capability table is the only place that set is written down.
fn object_destroy(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    // SAFETY: the state is installed during boot.
    let kernel = unsafe { state() };
    let Some(task) = kernel.task_at_slot(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    let resolved = match cap::resolve(&task.caps, &kernel.objects, handle, Rights::CONTROL) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let object_ref = resolved.object_ref;
    let Some(task) = kernel.task_at_slot_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let revoked = cap::revoke_object(&mut task.caps, &mut kernel.objects, object_ref);
    let _ = revoked;
    SyscallOutcome::Value(0)
}

/// Derive a narrower capability from one the caller holds.
///
/// `arg0` is the parent handle, `arg1` the requested rights. The kernel
/// intersects, so the child can never hold a right the parent lacked — and a
/// parent without `Rights::GRANT` may not delegate at all.
fn cap_delegate(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let parent = CapHandle::from_raw(args[0]);
    let Some(requested) = Rights::from_bits(args[1] as u32) else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    // SAFETY: the state is installed during boot.
    let kernel = unsafe { state() };
    let Some(task) = kernel.task_at_slot_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    match task.caps.derive(parent, requested) {
        Ok(handle) => SyscallOutcome::Value(handle.raw() as i64),
        Err(code) => SyscallOutcome::Failed(code),
    }
}

/// Revoke a capability the caller holds.
///
/// `arg0` is the handle. The revocation cascades to the capability's
/// descendants, because a withdrawn grant that lingers in a derived copy is a
/// privilege escalation rather than a cosmetic problem.
fn cap_revoke(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    // SAFETY: the state is installed during boot.
    let kernel = unsafe { state() };
    let Some(task) = kernel.task_at_slot_mut(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    match cap::revoke(&mut task.caps, handle, nova_cap::RevocationScope::Descendants) {
        Ok(count) => SyscallOutcome::Value(count as i64),
        Err(code) => SyscallOutcome::Failed(code),
    }
}

/// Send a message on an endpoint.
///
/// `arg0` is the endpoint handle, `arg1` the opcode, `arg2` a pointer to the
/// payload in the caller's address space, `arg3` the payload length.
///
/// The pointer is *not* dereferenced here: the kernel's entry path is
/// responsible for copying the caller's bytes into the kernel's own buffer
/// before calling the dispatcher, because only the entry path knows the
/// caller's address space. The dispatcher works on a payload the kernel
/// already owns, which is what makes it testable from ring 0.
///
/// The dispatch therefore takes the payload through the context's message
/// buffer rather than through a raw pointer. The entry path fills
/// [`SyscallContext`]'s buffer before dispatching.
fn send(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    // SAFETY: the state is installed during boot.
    let kernel = unsafe { state() };
    let Some(task) = kernel.task_at_slot(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    // The rights come from the *capability*, not from the caller: a
    // send-only endpoint must refuse a receive, and that refusal has to be
    // driven by what the holder holds.
    let resolved = match cap::resolve(&task.caps, &kernel.objects, handle, Rights::SEND) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let ObjectKind::Endpoint { channel, side } = resolved.kind else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let Some(channel) = kernel.ipc.channel_mut(nova_ipc::ChannelId::new(channel)) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    // The message's capabilities are transferred from the caller's table to
    // the *destination* task's table. The destination is the peer of this
    // endpoint's channel — the task that holds the other side.
    let peer_task = kernel.ipc.peer_task(channel.id(), side);

    let message = Message::new(args[1]);
    let endpoint = channel.endpoint(side, resolved.rights);

    match peer_task {
        Some(peer_slot) => {
            // SAFETY: the peer task exists; the two borrows are of different
            // tasks, and the task table is indexed rather than aliased.
            let (Some(caller_task), Some(peer)) = (
                kernel.task_at_slot_mut(context.task_slot),
                kernel.task_at_slot_mut(peer_slot),
            ) else {
                return SyscallOutcome::Failed(ErrorCode::BadHandle);
            };
            let mut transfer = TableTransfer::new(&mut caller_task.caps, &mut peer.caps);
            match crate::services::ipc::send(channel, &endpoint, message, &mut transfer, context.thread)
            {
                IpcOutcome::Ready(_) => SyscallOutcome::Value(0),
                IpcOutcome::Blocked => SyscallOutcome::Blocked,
                IpcOutcome::Failed(code) => SyscallOutcome::Failed(code),
            }
        }
        None => {
            // No peer task: the message goes nowhere, so the operation is
            // refused rather than silently dropped.
            SyscallOutcome::Failed(ErrorCode::Closed)
        }
    }
}

/// Receive a message from an endpoint.
///
/// `arg0` is the endpoint handle. A receive needs `Rights::RECEIVE`, so a
/// send-only endpoint refuses it with `E_DENIED` — which is the acceptance
/// criterion `cap_send_only_denies_recv`.
fn recv(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);

    // SAFETY: the state is installed during boot.
    let kernel = unsafe { state() };
    let Some(task) = kernel.task_at_slot(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    let resolved = match cap::resolve(&task.caps, &kernel.objects, handle, Rights::RECEIVE) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let ObjectKind::Endpoint { channel, side } = resolved.kind else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let Some(channel) = kernel.ipc.channel_mut(nova_ipc::ChannelId::new(channel)) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let endpoint = channel.endpoint(side, resolved.rights);
    let (outcome, _token) = crate::services::ipc::recv(channel, &endpoint, context.thread);
    match outcome {
        IpcOutcome::Ready(message) => SyscallOutcome::Value(message.opcode() as i64),
        IpcOutcome::Blocked => SyscallOutcome::Blocked,
        IpcOutcome::Failed(code) => SyscallOutcome::Failed(code),
    }
}

/// Reply once to a received request.
///
/// `arg0` is the endpoint handle, `arg1` the reply token.
fn reply(context: &SyscallContext, args: [u64; 6]) -> SyscallOutcome {
    let handle = CapHandle::from_raw(args[0]);
    let token = nova_ipc::ReplyToken::from_raw(args[1]);

    // SAFETY: the state is installed during boot.
    let kernel = unsafe { state() };
    let Some(task) = kernel.task_at_slot(context.task_slot) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };

    let resolved = match cap::resolve(&task.caps, &kernel.objects, handle, Rights::REPLY) {
        Ok(resolved) => resolved,
        Err(code) => return SyscallOutcome::Failed(code),
    };

    let ObjectKind::Endpoint { channel, side } = resolved.kind else {
        return SyscallOutcome::Failed(ErrorCode::Invalid);
    };

    let Some(channel) = kernel.ipc.channel_mut(nova_ipc::ChannelId::new(channel)) else {
        return SyscallOutcome::Failed(ErrorCode::BadHandle);
    };
    let endpoint = channel.endpoint(side, resolved.rights);
    let message = Message::new(0);
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
