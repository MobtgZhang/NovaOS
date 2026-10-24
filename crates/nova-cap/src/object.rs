//! Kernel object references.
//!
//! A capability does not point at an object; it points at a *reference* to one.
//! The split matters because the kernel has several kinds of object (endpoints,
//! tasks, memory objects) whose numbers live in separate spaces: an endpoint 3
//! and a memory object 3 are different things, and a capability that named
//! only "3" would be ambiguous.
//!
//! Keeping the type here rather than in `nova-abi` is deliberate: the ABI is
//! frozen and describes what two binaries agree on, while the object reference
//! is kernel-internal policy. The ABI's [`ObjectType`] enum is reused because
//! it is exactly the "which space" tag the reference needs.

use nova_abi::syscall::ObjectType;

/// A reference to one kernel object.
///
/// The pair is the whole identity: an object is named by its *kind* and its
/// *number*, and a capability carries the pair so the kernel can route a
/// handle to the right object table without any further lookup.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ObjectRef {
    /// Which kind of object this is.
    pub object_type: ObjectType,
    /// The object's number within that kind.
    pub object_id: u32,
}

impl ObjectRef {
    /// Build a reference to the object numbered `object_id` of that kind.
    pub const fn new(object_type: ObjectType, object_id: u32) -> Self {
        Self {
            object_type,
            object_id,
        }
    }

    /// The kind of object this names.
    pub const fn kind(self) -> ObjectType {
        self.object_type
    }

    /// The number of the object this names.
    pub const fn id(self) -> u32 {
        self.object_id
    }
}
