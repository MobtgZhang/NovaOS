//! The kernel-side service layer: capabilities, IPC, filesystem, supervision.
//!
//! # Why this module exists
//!
//! The pure logic of M3 and M4 — the capability table, the IPC state machine,
//! the RAM filesystem, the protocol layouts — lives in the `nova-cap`,
//! `nova-ipc`, `nova-fs`, and `nova-protocols` crates, and is host-tested
//! there. What those crates deliberately do *not* contain is the kernel's
//! machinery: the pools the tables live in, the wiring between a handle and
//! an object, the syscall dispatch that consults them, and the services that
//! own them.
//!
//! This module is that machinery. It is deliberately placed *outside*
//! `kernel/src/syscall/`, because M2 owns that directory for the trap entry
//! and the `iretq` trampoline: two agents editing one `mod.rs` is a merge
//! conflict waiting to happen. The integration point into M2's entry path is
//! [`dispatch::syscall_dispatch`], which takes a decoded syscall and a
//! [`SyscallContext`](dispatch::SyscallContext) and returns a result — the
//! entry path's job is to build the context and put the result in `RAX`.
//!
//! # The shape of the kernel state
//!
//! One static [`KernelState`] holds everything:
//!
//! - the [`ObjectRegistry`](object::ObjectRegistry), which is what a capability
//!   handle resolves to;
//! - one [`CapabilityTable`] per task, in a bounded pool;
//! - one [`Channel`](nova_ipc::Channel) per IPC channel, in a bounded pool,
//!   together with the mapping from a channel's sides to the tasks holding
//!   them;
//! - the RAM filesystem and the per-task scope sets;
//! - the service supervisor's table;
//! - the per-thread slots that hold the message and the reply token a
//!   receive produced, so the syscall layer can hand them to the caller.
//!
//! # Single-CPU, interrupts-masked
//!
//! v0.1.0 is single-CPU and the test harness runs with interrupts masked, so
//! the state needs no lock. The type says so explicitly with a `Sync`
//! implementation that documents the assumption, rather than pretending the
//! question does not arise.

pub mod cap;
pub mod dispatch;
pub mod fs;
pub mod ipc;
pub mod object;
pub mod supervisor;

use core::cell::UnsafeCell;

use nova_cap::CapabilityTable;
use nova_ipc::endpoint::ReplyToken;
use nova_ipc::message::Message;

use crate::services::fs::FileServer;
use crate::services::ipc::IpcRegistry;
use crate::services::object::ObjectRegistry;
use crate::services::supervisor::Supervisor;

/// Maximum number of tasks the kernel tracks.
///
/// A task is a process: it owns one capability table and one scope set. The
/// bound is a compile-time constant because the tables are inline.
pub const MAX_TASKS: usize = 16;

/// Maximum number of threads the kernel tracks for the per-thread slots.
///
/// The slots hold the last message a receive produced and the reply token it
/// handed out. A thread identifier is an index into them.
pub const MAX_THREADS: usize = 64;

/// Maximum number of IPC channels the kernel tracks.
///
/// Each channel holds two bounded inboxes and a reply table, so this is the
/// bound on how much kernel memory the IPC machinery may use.
pub const MAX_CHANNELS: usize = 32;

/// One task's kernel state.
///
/// A task's *authority* is its capability table; a task's *filesystem view* is
/// its scope set. The two are separate on purpose: a capability names an
/// object, and the object named may be a path scope, but the table does not
/// know about paths and the scope set does not know about handles.
#[derive(Clone)]
pub struct TaskState {
    /// The task's identifier.
    pub id: u32,
    /// Whether the task is still alive.
    pub alive: bool,
    /// The task's capability table.
    pub caps: CapabilityTable,
    /// The path scopes the task holds.
    pub scopes: crate::services::fs::ScopePool,
    /// The task's root capability.
    ///
    /// The root is the authority to spawn objects and to grant. It is the
    /// only capability a fresh task has, and it is what `init` hands out. It
    /// is stored rather than derived from a constant, because the capability
    /// table assigns the slot and the generation, and the kernel must not
    /// guess them.
    pub root: nova_abi::cap::CapHandle,
    /// The IPC channels this task is a party to, and which side it holds.
    pub channels: [(nova_ipc::ChannelId, nova_ipc::Side); crate::services::ipc::MAX_TASK_CHANNELS],
    /// How many entries of `channels` are meaningful.
    pub channel_count: usize,
}

impl TaskState {
    /// A fresh task with an empty capability table and no scopes.
    ///
    /// The root capability is minted by the caller, because the root names an
    /// object in the registry and the registry is a separate structure.
    pub fn new(id: u32) -> Self {
        Self {
            id,
            alive: true,
            caps: CapabilityTable::new(),
            scopes: crate::services::fs::ScopePool::new(),
            root: nova_abi::cap::CapHandle::INVALID,
            channels: [(nova_ipc::ChannelId::new(0), nova_ipc::Side::First);
                crate::services::ipc::MAX_TASK_CHANNELS],
            channel_count: 0,
        }
    }

    /// Remember that this task holds `side` of `channel`.
    pub fn remember_channel(&mut self, channel: nova_ipc::ChannelId, side: nova_ipc::Side) -> bool {
        if self.channel_count >= self.channels.len() {
            return false;
        }
        self.channels[self.channel_count] = (channel, side);
        self.channel_count += 1;
        true
    }
}

/// Everything the kernel's service layer owns.
///
/// One instance, in a static. Every field is a fixed-size value, so the whole
/// structure is a link-time constant plus mutable state — no allocator is
/// involved anywhere.
pub struct KernelState {
    /// The object registry: what a capability handle resolves to.
    pub objects: ObjectRegistry,
    /// The tasks, each with its own capability table and scope set.
    pub tasks: [Option<TaskState>; MAX_TASKS],
    /// The IPC channels.
    pub ipc: IpcRegistry,
    /// The filesystem the file server serves.
    pub files: FileServer,
    /// The service supervisor.
    pub supervisor: Supervisor,
    /// The message the last receive on each thread produced.
    pub last_message: [Option<Message>; MAX_THREADS],
    /// The reply token the last receive on each thread handed out.
    pub last_token: [Option<ReplyToken>; MAX_THREADS],
    /// The identifier the next task will receive.
    next_task_id: u32,
}

impl KernelState {
    /// A state with nothing in it.
    pub const fn new() -> Self {
        Self {
            objects: ObjectRegistry::new(),
            tasks: [None; MAX_TASKS],
            ipc: IpcRegistry::new(),
            files: FileServer::new(),
            supervisor: Supervisor::new(),
            last_message: [None; MAX_THREADS],
            last_token: [None; MAX_THREADS],
            next_task_id: 1,
        }
    }

    /// Create a task, returning its identifier.
    ///
    /// The task is created with a root capability: the authority to spawn
    /// objects and to grant. A task with no root could do nothing at all,
    /// which would make the "no capability, no privilege" rule untestable —
    /// everything would be denied for the trivial reason that nothing exists.
    ///
    /// Returns `None` when the task pool is full. The bound is a compile-time
    /// constant, so this is a reportable quota rather than an allocation
    /// failure.
    pub fn create_task(&mut self) -> Option<u32> {
        let slot = self.tasks.iter().position(|entry| entry.is_none())?;
        let id = self.next_task_id;
        self.next_task_id = self.next_task_id.wrapping_add(1);

        let mut task = TaskState::new(id);
        let root_object = self
            .objects
            .create(ObjectKind::TaskFactory { task: id })?;
        task.root = task
            .caps
            .create(root_object, crate::services::cap::ROOT_RIGHTS)
            .ok()?;
        self.tasks[slot] = Some(task);
        Some(id)
    }

    /// The state of a task, if it exists.
    pub fn task(&self, id: u32) -> Option<&TaskState> {
        self.tasks.iter().flatten().find(|task| task.id == id)
    }

    /// The state of a task, mutably, if it exists.
    pub fn task_mut(&mut self, id: u32) -> Option<&mut TaskState> {
        self.tasks.iter_mut().flatten().find(|task| task.id == id)
    }

    /// The state of a task at a slot, if it exists.
    ///
    /// The syscall path names a task by its *slot* rather than by its
    /// identifier, because the identifier is what a task learns about itself
    /// and the slot is what the kernel's tables are indexed by. The two are
    /// kept distinct so a stale identifier cannot name a different task after
    /// the slot is reused.
    pub fn task_at_slot(&self, slot: usize) -> Option<&TaskState> {
        self.tasks.get(slot).and_then(|entry| entry.as_ref())
    }

    /// The state of a task at a slot, mutably.
    pub fn task_at_slot_mut(&mut self, slot: usize) -> Option<&mut TaskState> {
        self.tasks.get_mut(slot).and_then(|entry| entry.as_mut())
    }

    /// The slot a task's identifier occupies.
    pub fn slot_of_task(&self, id: u32) -> Option<usize> {
        self.tasks
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|task| task.id == id))
    }

    /// Kill a task: mark it dead, revoke its authority, and break its links.
    ///
    /// Revocation is the point: a dead task's authority must not survive it.
    /// The channel side is killed too, which is what wakes a client blocked on
    /// the dead task with `E_PEER_DIED`.
    pub fn kill_task(&mut self, id: u32) -> bool {
        let Some(slot) = self.slot_of_task(id) else {
            return false;
        };
        let Some(task) = self.tasks[slot].as_mut() else {
            return false;
        };
        task.alive = false;

        // Revoke every capability the task holds. The table has no
        // "revoke everything" entry point, so the capabilities are revoked one
        // by one. The handles are collected first, because revoking mutates
        // the table.
        let mut handles = [nova_abi::cap::CapHandle::INVALID; 64];
        let mut count = 0usize;
        for slot_index in 1..nova_cap::MAX_CAPS {
            if count >= handles.len() {
                break;
            }
            // The table's slot numbering is the handle's slot, but the
            // generation is the table's private state, so the table is asked
            // for its entries rather than guessed at.
            let _ = slot_index;
        }
        // The capability table exposes its entries only through a handle, so
        // the revocation is driven by the handles the table reports. `live`
        // is the number of occupied slots; the handles themselves are found
        // by probing each slot through the public lookup, which is the only
        // sound way to learn a slot's generation.
        let live = self.tasks[slot]
            .as_ref()
            .map(|task| task.caps.live_count())
            .unwrap_or(0);
        let mut probe = 1u32;
        while count < handles.len() && count < live {
            // Every generation a slot can have is tried, because the table
            // never reuses a generation for a slot.
            let mut generation = 1u32;
            let mut found = false;
            while generation <= u32::MAX && !found {
                let candidate = nova_abi::cap::CapHandle::new(probe, generation);
                if self.tasks[slot]
                    .as_ref()
                    .is_some_and(|task| task.caps.entry(candidate).is_some())
                {
                    handles[count] = candidate;
                    count += 1;
                    found = true;
                }
                generation += 1;
                if generation == 0 {
                    break;
                }
            }
            probe += 1;
            if probe as usize >= nova_cap::MAX_CAPS {
                break;
            }
        }

        let Some(task) = self.tasks[slot].as_mut() else {
            return false;
        };
        for handle in handles.iter().take(count) {
            let _ = task
                .caps
                .revoke(*handle, nova_cap::RevocationScope::Descendants);
        }
        task.scopes.clear();

        // Kill the task's side of every channel it was party to, which is
        // what wakes a blocked peer with `E_PEER_DIED`.
        let sides: [(nova_ipc::ChannelId, nova_ipc::Side); crate::services::ipc::MAX_TASK_CHANNELS] =
            self.tasks[slot]
                .as_ref()
                .map(|task| task.channels)
                .unwrap_or([(nova_ipc::ChannelId::new(0), nova_ipc::Side::First);
                    crate::services::ipc::MAX_TASK_CHANNELS]);
        let side_count = self.tasks[slot]
            .as_ref()
            .map(|task| task.channel_count)
            .unwrap_or(0);
        self.ipc.kill_task(id, &sides[..side_count]);

        true
    }

    /// Drop every task's capabilities, object, channel, and file state.
    ///
    /// The kernel test harness runs every suite in one kernel instance, and a
    /// suite that left state behind would make the next suite's counts
    /// meaningless. This is the reset between them.
    pub fn reset(&mut self) {
        self.objects.reset();
        self.tasks = [None; MAX_TASKS];
        self.ipc.reset();
        self.files.reset();
        self.supervisor.reset();
        self.last_message = [None; MAX_THREADS];
        self.last_token = [None; MAX_THREADS];
    }
}

impl Default for KernelState {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds the kernel's service state.
///
/// The state is behind an `UnsafeCell` because it is mutated in place; the
/// `Sync` implementation documents the single-CPU, interrupts-masked
/// assumption that makes that sound.
struct SingleState {
    inner: UnsafeCell<Option<KernelState>>,
}

// SAFETY: the state is written exactly once, by `init`, before any reader
// exists, and is thereafter mutated only from kernel code that runs with
// interrupts masked on a single CPU. That is the same assumption the frame
// allocator makes, and it is replaced by a real lock when M2 introduces
// preemption.
unsafe impl Sync for SingleState {}

impl SingleState {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(None),
        }
    }
}

static STATE: SingleState = SingleState::new();

/// Install the kernel's service state.
///
/// # Safety
///
/// Must be called exactly once, before any reader exists, with interrupts
/// masked.
pub unsafe fn init() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        *STATE.inner.get() = Some(KernelState::new());
    }
}

/// The kernel's service state.
///
/// # Safety
///
/// `init` must already have run. The returned reference is `&'static` because
/// the state lives in a static and is never moved.
#[inline]
pub unsafe fn state() -> &'static mut KernelState {
    // SAFETY: delegated to this function's contract. The exclusive borrow is
    // sound under the same single-CPU, interrupts-masked assumption that makes
    // the `Sync` implementation above sound.
    unsafe {
        (&mut *STATE.inner.get())
            .as_mut()
            .expect("kernel service state used before services::init")
    }
}
