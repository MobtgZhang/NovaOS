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
//! # Why the capability tables live apart from the task metadata
//!
//! A capability table is large — a thousand slots, each carrying an entry —
//! while a task's *metadata* is small: its identifier, whether it is alive,
//! the root handle, its path scopes, and the channels it is party to. Keeping
//! the two in separate pools means the metadata can be `Copy` and cheap to
//! move, while the tables stay in place and are addressed by slot. It also
//! means the borrow checker can see the two as disjoint: a syscall that needs
//! both a task's metadata and another task's table can hold one and borrow the
//! other, which it could not do if both lived inside one struct.
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
pub mod process;
pub mod supervisor;

use core::cell::UnsafeCell;

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_cap::CapabilityTable;
use nova_fs::scope::{FileScope, ScopeSet};
use nova_ipc::endpoint::{ChannelId, Completion, ReplyToken, Side};
use nova_ipc::message::Message;

use crate::services::fs::{FileServer, MAX_SCOPES_PER_TASK};
use crate::services::ipc::IpcRegistry;
use crate::services::object::{ObjectKind, ObjectRegistry};
use crate::services::supervisor::Supervisor;

/// Maximum number of tasks the kernel tracks.
///
/// A task is a process: it owns one capability table and one scope set. The
/// bound is a compile-time constant because the tables are inline, and every
/// task's table costs the kernel about sixty kilobytes of static memory.
pub const MAX_TASKS: usize = 8;

/// Maximum number of threads the kernel tracks for the per-thread slots.
///
/// The slots hold the last message a receive produced, the reply token it
/// handed out, and the error a failed operation produced. A thread identifier
/// is an index into them.
pub const MAX_THREADS: usize = 64;

/// Maximum number of IPC channels the kernel tracks.
///
/// Each channel holds two bounded inboxes and a reply table, so this is the
/// bound on how much kernel memory the IPC machinery may use.
pub const MAX_CHANNELS: usize = 16;

/// Maximum bytes in the initramfs service name recorded for a task.
pub const MAX_SERVICE_NAME: usize = 32;

/// One task's kernel metadata.
///
/// The *authority* of a task is its capability table, which lives in
/// [`KernelState::caps`] and is addressed by slot; what is kept here is the
/// small bookkeeping the syscall path needs on every call.
///
/// A task's *filesystem view* is its scope set. The two are separate on
/// purpose: a capability names an object, and the object named may be a path
/// scope, but the table does not know about paths and the scope set does not
/// know about handles.
#[derive(Clone, Copy)]
pub struct TaskState {
    /// The task's identifier.
    pub id: u32,
    /// Whether the task is still alive.
    pub alive: bool,
    /// The task's root capability.
    ///
    /// The root is the authority to spawn objects and to grant. It is the
    /// only capability a fresh task has, and it is what `init` hands out. It
    /// is stored rather than derived from a constant, because the capability
    /// table assigns the slot and the generation, and the kernel must not
    /// guess them.
    pub root: CapHandle,
    /// The endpoint that connects this child process to its parent.
    pub management_endpoint: CapHandle,
    /// The second side of a pair created by `ENDPOINT_CREATE`.
    pub pending_endpoint: CapHandle,
    /// The path scopes the task holds.
    pub scopes: crate::services::fs::ScopePool,
    /// The IPC channels this task is a party to, and which side it holds.
    pub channels: [(ChannelId, Side); MAX_CHANNELS],
    /// How many entries of `channels` are meaningful.
    pub channel_count: usize,
    /// The initramfs image name that started this task, if any.
    ///
    /// A supervisor needs to ask "is this service alive?" without holding a
    /// capability into every child. The kernel records the launch name as
    /// plain data; it does not confer authority or make a process privileged.
    pub service_name: [u8; MAX_SERVICE_NAME],
    /// How many bytes of `service_name` are meaningful.
    pub service_name_len: usize,
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
            root: CapHandle::INVALID,
            management_endpoint: CapHandle::INVALID,
            pending_endpoint: CapHandle::INVALID,
            scopes: crate::services::fs::ScopePool::new(),
            channels: [(ChannelId::new(0), Side::First); MAX_CHANNELS],
            channel_count: 0,
            service_name: [0; MAX_SERVICE_NAME],
            service_name_len: 0,
        }
    }

    /// Remember that this task holds `side` of `channel`.
    ///
    /// Returns `false` when the task is already a party to as many channels as
    /// the pool allows. A task that holds more channels than that has a
    /// design problem — the channels should be behind a server — so the
    /// refusal is a quota, not an eviction.
    pub fn remember_channel(&mut self, channel: ChannelId, side: Side) -> bool {
        if self.channel_count >= self.channels.len() {
            return false;
        }
        self.channels[self.channel_count] = (channel, side);
        self.channel_count += 1;
        true
    }
}

/// The kernel state's fields, borrowed disjointly.
///
/// The service layer needs to touch several fields of [`KernelState`] at once —
/// the task table, the capability tables, the IPC registry — and the borrow
/// checker cannot see through a method call that those fields are distinct. A
/// method call on `&mut self` borrows the *whole* struct, so
/// `kernel.task_at_slot_mut(..)` and `&mut kernel.objects` are two overlapping
/// borrows even though they touch different fields.
///
/// This struct is the explicit split: it takes the fields apart so that the
/// borrows are visibly disjoint. It is the mechanism that lets the syscall
/// path hold one task's capability table and another's *at the same time*,
/// which is exactly what a capability transfer needs.
pub struct KernelStateParts<'a> {
    /// The tasks' metadata.
    pub tasks: &'a mut [Option<TaskState>; MAX_TASKS],
    /// The tasks' capability tables.
    pub caps: &'a mut [CapabilityTable; MAX_TASKS],
    /// The IPC channels.
    pub ipc: &'a mut IpcRegistry,
    /// The object registry.
    pub objects: &'a mut ObjectRegistry,
    /// The file server.
    pub files: &'a mut FileServer,
    /// The service supervisor.
    pub supervisor: &'a mut Supervisor,
    /// The message the last receive on each thread produced.
    pub last_message: &'a mut [Option<Message>; MAX_THREADS],
    /// The reply token the last receive on each thread handed out.
    pub last_token: &'a mut [Option<ReplyToken>; MAX_THREADS],
    /// The error the last failed operation on each thread produced.
    pub last_error: &'a mut [Option<ErrorCode>; MAX_THREADS],
    /// The message each thread has prepared for its next send.
    pub outgoing_message: &'a mut [Option<Message>; MAX_THREADS],
}

impl KernelState {
    /// Borrow the kernel state's fields disjointly.
    ///
    /// The explicit split is what makes the simultaneous borrows sound; see
    /// [`KernelStateParts`] for why the borrow checker needs the help.
    pub fn split(&mut self) -> KernelStateParts<'_> {
        KernelStateParts {
            tasks: &mut self.tasks,
            caps: &mut self.caps,
            ipc: &mut self.ipc,
            objects: &mut self.objects,
            files: &mut self.files,
            supervisor: &mut self.supervisor,
            last_message: &mut self.last_message,
            last_token: &mut self.last_token,
            last_error: &mut self.last_error,
            outgoing_message: &mut self.outgoing_message,
        }
    }
}

/// Borrow two distinct tasks' capability tables at once.
///
/// The capability transfer needs the caller's table and the recipient's
/// table simultaneously, and a table cannot be borrowed mutably twice — so
/// the array is split at the higher index and the two halves are borrowed
/// from each side. The returned pair is in the order the arguments were
/// given, so the caller does not have to remember which half is which.
///
/// Returns `None` when the indices are equal or out of range.
pub fn two_caps_mut(
    caps: &mut [CapabilityTable; MAX_TASKS],
    first: usize,
    second: usize,
) -> Option<(&mut CapabilityTable, &mut CapabilityTable)> {
    if first == second || first >= MAX_TASKS || second >= MAX_TASKS {
        return None;
    }
    let (low, high) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    let (left, right) = caps.split_at_mut(high);
    let low_table = left.get_mut(low)?;
    let high_table = right.get_mut(0)?;
    if first < second {
        Some((low_table, high_table))
    } else {
        Some((high_table, low_table))
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
    /// The tasks' metadata, indexed by slot.
    pub tasks: [Option<TaskState>; MAX_TASKS],
    /// The tasks' capability tables, indexed by the same slot.
    ///
    /// Kept apart from `tasks` for the reasons in the module documentation:
    /// a table is large and immovable, and the two pools must be borrowable
    /// independently.
    pub caps: [CapabilityTable; MAX_TASKS],
    /// The IPC channels, and which task holds which side of each.
    pub ipc: IpcRegistry,
    /// The filesystem the file server serves.
    pub files: FileServer,
    /// The service supervisor.
    pub supervisor: Supervisor,
    /// The message the last receive on each thread produced.
    pub last_message: [Option<Message>; MAX_THREADS],
    /// The reply token the last receive on each thread handed out.
    pub last_token: [Option<ReplyToken>; MAX_THREADS],
    /// The error the last failed operation on each thread produced.
    ///
    /// A woken thread learns *why* it was woken from here, which is what
    /// turns a peer death into `E_PEER_DIED` rather than a silent wakeup.
    pub last_error: [Option<ErrorCode>; MAX_THREADS],
    /// The message each thread has prepared for its next send.
    ///
    /// The entry path copies the caller's message out of user space into
    /// this slot before dispatching a send, because only the entry path
    /// knows the caller's address space. The dispatcher then sends *this*
    /// message, which is what lets a capability list travel: the handles in
    /// it are the caller's own, and the transfer mints the recipient's
    /// handles in the recipient's table.
    pub outgoing_message: [Option<Message>; MAX_THREADS],
    /// The identifier the next task will receive.
    next_task_id: u32,
}

impl KernelState {
    /// A state with nothing in it.
    ///
    /// A `const fn` so the whole state can be a static initialiser rather
    /// than a value built on the stack. The state holds a file server, and a
    /// file server holds a whole RAM filesystem -- about 71 KiB of inode
    /// data. The kernel's stack is 64 KiB, so constructing the state on the
    /// stack overflows it. The overflow is silent and its symptom appears
    /// later, as a fault whose reported instruction pointer points at
    /// innocent code, which is why this is built as a constant rather than
    /// merely sized carefully.
    pub const fn new() -> Self {
        Self {
            objects: ObjectRegistry::new(),
            tasks: [None; MAX_TASKS],
            caps: [CapabilityTable::new(); MAX_TASKS],
            ipc: IpcRegistry::new(),
            files: FileServer::new(),
            supervisor: Supervisor::new(),
            last_message: [None; MAX_THREADS],
            last_token: [None; MAX_THREADS],
            last_error: [None; MAX_THREADS],
            outgoing_message: [None; MAX_THREADS],
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
        let root_object = self.objects.create(ObjectKind::TaskFactory { task: id })?;
        task.root = self.caps[slot]
            .create(root_object, crate::services::cap::ROOT_RIGHTS)
            .ok()?;
        self.tasks[slot] = Some(task);
        Some(id)
    }

    /// Create a task *without* a root capability.
    ///
    /// This is the "no capability, no privilege" case made concrete: the task
    /// exists, it can be dispatched to, and every operation it attempts is
    /// denied because its capability table is empty. A task that did not exist
    /// would be denied too, but for the wrong reason — `E_BAD_HANDLE` rather
    /// than `E_DENIED` — and a test could not tell the two apart.
    ///
    /// It is also what a *revoked* task looks like from the outside, which is
    /// why the kernel needs both.
    pub fn create_bare_task(&mut self) -> Option<u32> {
        let slot = self.tasks.iter().position(|entry| entry.is_none())?;
        let id = self.next_task_id;
        self.next_task_id = self.next_task_id.wrapping_add(1);
        self.tasks[slot] = Some(TaskState::new(id));
        Some(id)
    }

    /// The slot a freshly created task occupies.
    ///
    /// The identifier and the slot are different things, and a caller that
    /// wants to dispatch *to* a task needs the slot. Returning it here saves
    /// every caller a second lookup.
    pub fn slot_of_newest_task(&self, id: u32) -> Option<usize> {
        self.slot_of_task(id)
    }

    /// Create a connected pair of endpoints between two tasks.
    ///
    /// The kernel's `init` path: it creates a channel, connects the two
    /// tasks to its two sides, and mints a capability in each task's table
    /// naming that side.
    ///
    /// The two sides are given *different* rights by default, because the
    /// report's rule is that a client gets a send-only endpoint and the
    /// server a receive-only one. The caller may ask for anything, but the
    /// rights are the caller's decision, not the kernel's.
    ///
    /// Returns the two handles, or `None` if a pool is exhausted.
    pub fn create_endpoint_pair(
        &mut self,
        first_slot: usize,
        second_slot: usize,
        first_rights: Rights,
        second_rights: Rights,
    ) -> Option<(CapHandle, CapHandle)> {
        if first_slot >= MAX_TASKS || second_slot >= MAX_TASKS {
            return None;
        }
        let channel = self.ipc.create_channel()?;
        if !self.ipc.connect(channel, first_slot, second_slot) {
            return None;
        }

        let first_object = self.objects.create(ObjectKind::Endpoint {
            channel: channel.number(),
            side: Side::First,
        })?;
        let second_object = self.objects.create(ObjectKind::Endpoint {
            channel: channel.number(),
            side: Side::Second,
        })?;

        let first = self.caps[first_slot]
            .create(first_object, first_rights)
            .ok()?;
        let second = self.caps[second_slot]
            .create(second_object, second_rights)
            .ok()?;

        // The tasks' channel lists are what peer death uses, so they are
        // updated here rather than left to the caller: a task that holds a
        // side the kernel does not know about would not be woken when its
        // peer dies.
        if let Some(task) = self.tasks[first_slot].as_mut() {
            task.remember_channel(channel, Side::First);
        }
        if let Some(task) = self.tasks[second_slot].as_mut() {
            task.remember_channel(channel, Side::Second);
        }
        Some((first, second))
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

    /// The capability table of the task at a slot.
    pub fn caps_at(&self, slot: usize) -> Option<&CapabilityTable> {
        self.caps.get(slot)
    }

    /// The capability table of the task at a slot, mutably.
    pub fn caps_at_mut(&mut self, slot: usize) -> Option<&mut CapabilityTable> {
        self.caps.get_mut(slot)
    }

    /// The slot a task's identifier occupies.
    pub fn slot_of_task(&self, id: u32) -> Option<usize> {
        self.tasks
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|task| task.id == id))
    }

    /// Mint a path-scoped grant for the task at a slot.
    ///
    /// The grant is two things at once: a capability in the task's table, and
    /// a scope entry in the task's scope pool. Both are created here, so a
    /// handle can always be turned back into a path, and neither can exist
    /// without the other.
    ///
    /// This is the kernel's `init` path — the place where authority is
    /// delegated — and it is the only place a scope is created.
    pub fn grant_file_scope(
        &mut self,
        slot: usize,
        path: &[u8],
        rights: Rights,
    ) -> Result<CapHandle, ErrorCode> {
        // The three borrows are of three different fields, so they are split
        // explicitly: a borrow of `self.tasks` and a borrow of `self.caps`
        // cannot both come from `&mut self` through a method call, because the
        // compiler cannot see that they touch different fields.
        let KernelState {
            tasks,
            caps,
            objects,
            ..
        } = self;
        let Some(Some(task)) = tasks.get_mut(slot) else {
            return Err(ErrorCode::BadHandle);
        };
        crate::services::cap::grant_file_scope(
            &mut caps[slot],
            &mut task.scopes,
            objects,
            path,
            rights,
        )
    }

    /// The path scopes the task at a slot currently holds.
    ///
    /// The scope set is rebuilt from the capability table every time, and the
    /// *rights* come from the table rather than from the pool. That is the
    /// whole point: a revoked or narrowed capability stops granting the
    /// moment it is revoked, rather than when the pool is next rewritten.
    ///
    /// A scope whose capability is gone is dropped, so the set can never grant
    /// more than the table does.
    pub fn scope_set_for(&self, slot: usize) -> ScopeSet<MAX_SCOPES_PER_TASK> {
        let mut scopes = ScopeSet::new();
        let Some(task) = self.tasks.get(slot).and_then(|entry| entry.as_ref()) else {
            return scopes;
        };
        let Some(table) = self.caps.get(slot) else {
            return scopes;
        };
        for entry in task.scopes.iter() {
            let Some(capability) = table.entry(entry.handle) else {
                continue;
            };
            if !capability.is_live() {
                continue;
            }
            let scope = FileScope::new(*entry.scope.path(), capability.rights);
            if !scopes.insert(scope) {
                // The set is full. The grant is simply not represented, which
                // means it does not grant — a conservative outcome, and the
                // only safe one.
                break;
            }
        }
        scopes
    }

    /// Kill a task: mark it dead, revoke its authority, and break its links.
    ///
    /// Revocation is the point: a dead task's authority must not survive it.
    /// The channel sides are killed too, which is what wakes a client blocked
    /// on the dead task with `E_PEER_DIED`.
    pub fn kill_task(&mut self, id: u32) -> bool {
        let Some(slot) = self.slot_of_task(id) else {
            return false;
        };

        let (sides, side_count) = {
            let Some(task) = self.tasks[slot].as_ref() else {
                return false;
            };
            (task.channels, task.channel_count)
        };

        {
            let Some(task) = self.tasks[slot].as_mut() else {
                return false;
            };
            task.alive = false;
            task.scopes.clear();
        }

        // Every capability the task holds is withdrawn. The revocation is
        // driven by the *objects* rather than by the table's internal slots,
        // because every capability the kernel mints names an object in the
        // registry — that is what makes the table and the registry two halves
        // of one lookup.
        let revoked = crate::services::cap::revoke_all(&mut self.caps[slot], &self.objects);
        let _ = revoked;

        // Kill the task's side of every channel it was party to, which is what
        // wakes a blocked peer with `E_PEER_DIED`.
        self.ipc.kill_task(slot, &sides[..side_count]);
        true
    }

    /// Remove one dead task and release its slot.
    ///
    /// Authority is withdrawn first, because a caller might observe the slot
    /// disappearing while a stale capability still resolves. Only after
    /// revocation and peer wake-up does the metadata leave the table.
    pub fn retire_task(&mut self, slot: usize) -> bool {
        let Some(id) = self.task_at_slot(slot).map(|task| task.id) else {
            return false;
        };
        if !self.kill_task(id) {
            return false;
        }
        self.tasks[slot] = None;
        true
    }

    /// Whether a live task was started from the named initramfs image.
    pub fn service_is_alive(&self, name: &[u8]) -> bool {
        self.tasks.iter().flatten().any(|task| {
            task.alive
                && task.service_name_len == name.len()
                && task.service_name[..task.service_name_len] == *name
        })
    }

    /// Collect the completions every channel recorded, and publish what they
    /// carry to the threads they name.
    ///
    /// A reply completion carries the reply message, so it is written into
    /// the waiting thread's message slot; a failure completion carries the
    /// error, so it is written into the waiting thread's error slot. The
    /// scheduler (M2) then observes the thread's slots and makes it runnable.
    ///
    /// Returns the number of completions collected.
    pub fn drain_completions(&mut self, out: &mut [Completion]) -> usize {
        let count = self.ipc.drain_completions(out);
        for completion in out.iter().take(count) {
            let waiter = completion.waiter() as usize;
            if waiter >= MAX_THREADS {
                continue;
            }
            match completion {
                Completion::ReplyReady { message, .. } => {
                    self.last_message[waiter] = Some(*message);
                    self.last_token[waiter] = None;
                    self.last_error[waiter] = None;
                }
                Completion::Failed { error, .. } => {
                    self.last_error[waiter] = Some(*error);
                }
                Completion::ReceiveReady { .. } | Completion::SendCompleted { .. } => {}
            }
        }
        count
    }

    /// Drop every task's capabilities, object, channel, and file state.
    ///
    /// The kernel test harness runs every suite in one kernel instance, and a
    /// suite that left state behind would make the next suite's counts
    /// meaningless. This is the reset between them.
    pub fn reset(&mut self) {
        self.objects.reset();
        self.tasks = [None; MAX_TASKS];
        // `clear` rather than a fresh array: a capability table is tens of
        // kilobytes and there is one per task, so assigning a new array
        // builds all of them on the stack first.
        for table in &mut self.caps {
            table.clear();
        }
        self.ipc.reset();
        self.files.reset();
        self.supervisor.reset();
        self.last_message = [None; MAX_THREADS];
        self.last_token = [None; MAX_THREADS];
        self.last_error = [None; MAX_THREADS];
        self.outgoing_message = [None; MAX_THREADS];
        self.next_task_id = 1;
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
    inner: UnsafeCell<KernelState>,
}

// SAFETY: the state is mutated only from kernel code that runs with
// interrupts masked on a single CPU. That is the same assumption the frame
// allocator makes, and it is replaced by a real lock when M2 introduces
// preemption.
unsafe impl Sync for SingleState {}

impl SingleState {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(KernelState::new()),
        }
    }
}

/// The kernel's service state.
///
/// A static rather than a value installed at boot. The state is large -- the
/// file server alone accounts for about 71 KiB -- so it is a link-time
/// constant in the kernel's image, and the kernel's 64 KiB stack never has
/// to hold it. An `Option` around it would have forced the value to be
/// constructed at runtime, on the stack, which is exactly the overflow this
/// arrangement avoids.
static STATE: SingleState = SingleState::new();

/// A second instance, used by the kernel's test suites.
///
/// The suites need a state to dispatch against, and they need it to be
/// *theirs*: the kernel's own state is reachable through
/// [`state()`], and a suite that held a borrow of it while the dispatcher
/// reached for it again would be holding two mutable borrows of one object.
/// The optimiser is entitled to assume that cannot happen, and it acts on
/// that assumption in ways that look like a hang.
///
/// A separate instance avoids the question entirely. It is a second link-time
/// constant, so it costs nothing at runtime and nothing on the stack.
static TEST_STATE: SingleState = SingleState::new();

/// The test suites' own service state.
///
/// # Safety
///
/// The same discipline as [`state()`]: the suites run one at a time, in ring
/// 0, with interrupts masked, and no two callers hold this borrow at once.
/// A caller that keeps the returned reference across another call to this
/// function has aliased it, which is the same mistake the aliasing rule
/// forbids everywhere else.
pub unsafe fn test_state() -> &'static mut KernelState {
    // SAFETY: delegated to this function's contract.
    unsafe { &mut *TEST_STATE.inner.get() }
}

/// Install the kernel's service state.
///
/// # Safety
///
/// Must be called exactly once, before any reader exists, with interrupts
/// masked.
pub unsafe fn init() {
    // The state is a static initialiser, so there is nothing to install at
    // runtime. This function exists to keep the boot sequence's shape
    // explicit: the service layer's state is live before anything can
    // dispatch a syscall, and a reader that runs earlier is a bug rather
    // than a race.
}

/// The kernel's service state.
///
/// # Safety
///
/// `init` must already have run. The returned reference is `&'static` because
/// the state lives in a static and is never moved.
///
/// Only one borrow may be live at a time: the state is behind a raw pointer
/// for exactly this reason, so that a caller who has finished with a borrow
/// can take another without the two aliasing in the compiler's model. The
/// service layer never holds two at once — every helper takes `&KernelState`
/// or `&mut KernelState` explicitly rather than reaching for the global.
#[inline]
pub unsafe fn state() -> &'static mut KernelState {
    // SAFETY: delegated to this function's contract. The exclusive borrow is
    // sound under the same single-CPU, interrupts-masked assumption that makes
    // the `Sync` implementation above sound.
    unsafe { &mut *STATE.inner.get() }
}
