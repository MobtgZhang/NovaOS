//! The kernel's object registry.
//!
//! # What an object is here
//!
//! A capability names an object. The registry is where those objects live: a
//! bounded table of slots, each holding one kernel object's identity and
//! whatever the kernel needs in order to *act* on it. A capability's
//! [`ObjectRef`](nova_cap::ObjectRef) is the pair (ABI object type, slot index),
//! and resolving it means finding the slot and checking that the slot's kind
//! is the one the reference claims.
//!
//! # Why the type tag is checked on lookup
//!
//! `ObjectRef` is (type, id). If the registry ignored the type half, a
//! reference minted for an endpoint could resolve to whatever now occupies that
//! slot — for example a file scope, whose rights would then be interpreted
//! with the endpoint's rules. The type half is therefore part of the identity,
//! and a mismatch is a miss, not a reinterpretation. That is the same ABA
//! defence the capability table's generation counter provides, one level down.
//!
//! # Why a file scope is exposed as `IoRegion`
//!
//! The frozen ABI's [`ObjectType`] list has no directory kind, and the ABI may
//! not be edited. A path-scoped file grant is nevertheless *not* an endpoint,
//! not a task, and not shareable memory, so it needs a type of its own. It is
//! exposed as [`ObjectType::IoRegion`], which is the ABI's name for "a bounded
//! region of a resource", and the registry's *internal* kind keeps it apart
//! from a genuine device register range. The mapping from internal kind to ABI
//! type is injective, so `(type, id)` still names exactly one slot.
//!
//! # Bounds
//!
//! [`MAX_OBJECTS`] slots. The registry is a fixed-size array: the kernel has no
//! allocator, and "the registry is full" is a reportable state rather than a
//! failure in the middle of a syscall.

use nova_abi::syscall::ObjectType;
use nova_cap::ObjectRef;
use nova_fs::path::CanonicalPath;
use nova_ipc::Side;

/// Maximum number of objects the registry holds.
///
/// A slot holds a canonical path in the worst case, so the table is a few tens
/// of kilobytes of kernel memory — cheap, and bounded by construction.
pub const MAX_OBJECTS: usize = 128;

/// What the kernel knows about an object.
///
/// The variants the kernel can *act* on carry the state the action needs. The
/// remaining variants are placeholders that the registry can already name, so
/// that a capability to them resolves rather than failing as a bad handle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    /// An IPC endpoint: a channel, and one side of it.
    ///
    /// The pair is the identity: an endpoint is not "a channel", it is a
    /// *side* of a channel, which is what stops a side from receiving what it
    /// sent.
    Endpoint {
        /// The channel this endpoint belongs to.
        channel: u32,
        /// Which side of the channel it is.
        side: Side,
    },
    /// A path-scoped filesystem grant.
    ///
    /// The path is canonical: it was canonicalised when the grant was made and
    /// is never re-parsed, so a scope can never be widened by a later
    /// spelling.
    FileScope {
        /// The granted subtree, canonical.
        path: CanonicalPath,
    },
    /// A task (process).
    Task {
        /// The task's identifier.
        task: u32,
    },
    /// A thread within a task.
    Thread {
        /// The thread's identifier.
        thread: u32,
    },
    /// An address space.
    AddressSpace,
    /// A shareable memory region.
    MemoryObject,
    /// A one-shot notification.
    Notification,
    /// The authority to create tasks and spawn objects.
    ///
    /// A fresh task holds exactly one of these: its *root*. The root is what
    /// `init` delegates from, and the authority to spawn is what it carries.
    TaskFactory {
        /// The task the authority belongs to.
        task: u32,
    },
}

impl ObjectKind {
    /// The ABI object type this kind is exposed as.
    ///
    /// The mapping is injective: two internal kinds never share an ABI type,
    /// which is what makes `(type, id)` name exactly one registry slot.
    pub const fn abi_type(&self) -> ObjectType {
        match self {
            ObjectKind::Endpoint { .. } => ObjectType::Endpoint,
            ObjectKind::FileScope { .. } => ObjectType::IoRegion,
            ObjectKind::Task { .. } => ObjectType::Task,
            ObjectKind::Thread { .. } => ObjectType::Thread,
            ObjectKind::AddressSpace => ObjectType::AddressSpace,
            ObjectKind::MemoryObject => ObjectType::MemoryObject,
            ObjectKind::Notification => ObjectType::Notification,
            ObjectKind::TaskFactory { .. } => ObjectType::TaskFactory,
        }
    }
}

/// The kernel's object table.
///
/// Every entry is inline: the struct is a fixed-size value the kernel can place
/// in a static, and it needs no allocator.
#[derive(Clone)]
pub struct ObjectRegistry {
    slots: [Option<ObjectKind>; MAX_OBJECTS],
}

impl ObjectRegistry {
    /// An empty registry.
    pub const fn new() -> Self {
        Self { slots: [None; MAX_OBJECTS] }
    }

    /// Create an object, returning the reference that names it.
    ///
    /// Returns `None` when the registry is full: the kernel has no allocator,
    /// so a full table is a quota rather than an allocation failure.
    pub fn create(&mut self, kind: ObjectKind) -> Option<ObjectRef> {
        let slot = self.slots.iter().position(|entry| entry.is_none())?;
        let object_type = kind.abi_type();
        self.slots[slot] = Some(kind);
        Some(ObjectRef::new(object_type, slot as u32))
    }

    /// The kind an object reference names, if it names one.
    ///
    /// The type half of the reference is checked as well as the slot: a
    /// reference minted for one kind must not resolve to another kind that
    /// happens to occupy the same slot.
    pub fn get(&self, object_ref: ObjectRef) -> Option<&ObjectKind> {
        let slot = object_ref.id() as usize;
        if slot >= MAX_OBJECTS {
            return None;
        }
        let kind = self.slots[slot].as_ref()?;
        if kind.abi_type() != object_ref.kind() {
            return None;
        }
        Some(kind)
    }

    /// Remove an object.
    ///
    /// Returns whether the object was there. Removing an object does not by
    /// itself invalidate the capabilities that name it — the capability layer
    /// does that, because only it knows which tables hold them.
    pub fn destroy(&mut self, object_ref: ObjectRef) -> bool {
        let slot = object_ref.id() as usize;
        if slot >= MAX_OBJECTS {
            return false;
        }
        match self.slots[slot] {
            Some(kind) if kind.abi_type() == object_ref.kind() => {
                self.slots[slot] = None;
                true
            }
            _ => false,
        }
    }

    /// Every object that exists.
    ///
    /// Used by the revocation path: when a task dies, every capability it
    /// holds must be withdrawn, and the capabilities are found by the objects
    /// they name.
    pub fn objects(&self) -> impl Iterator<Item = ObjectRef> + '_ {
        self.slots.iter().enumerate().filter_map(|(slot, entry)| {
            entry.as_ref().map(|kind| ObjectRef::new(kind.abi_type(), slot as u32))
        })
    }

    /// How many objects exist.
    pub fn count(&self) -> usize {
        self.slots.iter().filter(|entry| entry.is_some()).count()
    }

    /// How many slots are free.
    pub fn free_count(&self) -> usize {
        MAX_OBJECTS - self.count()
    }

    /// Drop every object.
    ///
    /// Used by the kernel test harness between suites: the suites share one
    /// kernel instance, and a suite that left objects behind would make the
    /// next suite's counts meaningless.
    pub fn reset(&mut self) {
        self.slots = [None; MAX_OBJECTS];
    }
}

impl Default for ObjectRegistry {
    fn default() -> Self {
        Self::new()
    }
}
