//! Syscall numbers and the calling convention.
//!
//! # The convention
//!
//! On x86-64 a syscall is made with the `syscall` instruction, which loads the
//! instruction pointer and `RCX` from `RIP` and `R11` and leaves the *only* ABI
//! registers free. So:
//!
//! | Register | Meaning |
//! | --- | --- |
//! | `RAX` | syscall number in, result out |
//! | `RDI` | first argument |
//! | `RSI` | second argument |
//! | `RDX` | third argument |
//! | `R10` | fourth argument |
//! | `R8` | fifth argument |
//! | `R9` | sixth argument |
//!
//! `RCX` and `R11` are deliberately absent: they hold the return address and the
//! saved flags, which is the price of the instruction.
//!
//! # Why `iretq` first, not `sysretq`
//!
//! `sysretq` is faster but has a famously sharp edge: the value in `RCX` is
//! checked against the kernel's canonical addresses, and a non-canonical one
//! raises `#GP` *after* the stack switch is partially set up. A kernel that gets
//! this wrong faults in a way that loses all context. v0.1.0 returns with `iretq`,
//! which is one instruction slower and dramatically easier to reason about; see
//! `docs/abi/address-layout.md`.

/// A syscall number.
///
/// `#[repr(transparent)]` rather than `repr(u32)`: the number is a newtype over
/// its own value, and `repr(transparent)` guarantees it is layout-identical to
/// that `u32` — which is what lets it be passed in `RAX` without a wrapper.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(transparent)]
pub struct Syscall(pub u32);

impl Syscall {
    /// The number as passed in `RAX`.
    pub const fn number(self) -> u32 {
        self.0
    }

    /// The raw value, for logging.
    pub const fn raw(self) -> u64 {
        self.0 as u64
    }
}

/// Every syscall the kernel implements.
///
/// # Append-only
///
/// Numbers are a cross-binary contract. A retired number stays reserved rather
/// than being reused, so a stale binary calling it gets `E_UNSUPPORTED` instead of
/// silently landing on a different syscall.
pub mod nr {
    use super::Syscall;

    /// Report the kernel version. The only syscall every user process may make
    /// without any capability: a process that cannot identify its environment
    /// cannot even report an error usefully.
    pub const GET_VERSION: Syscall = Syscall(0);

    /// Create a kernel object.
    ///
    /// Arguments: `object_type`, `rights`. Returns a [`CapHandle`](crate::cap::CapHandle).
    pub const OBJECT_CREATE: Syscall = Syscall(1);

    /// Destroy an object this process holds a `CONTROL` right on.
    pub const OBJECT_DESTROY: Syscall = Syscall(2);

    /// Derive a narrower capability from one already held.
    ///
    /// Arguments: `handle`, `requested_rights`. The kernel intersects, so the
    /// child cannot gain a right the parent lacked.
    pub const CAP_DEREGATE: Syscall = Syscall(3);

    /// Revoke a capability held by another process.
    pub const CAP_REVOKE: Syscall = Syscall(4);

    /// Send a message on an endpoint.
    ///
    /// Arguments: `endpoint`, `header`, `payload_ptr`, `payload_len`.
    pub const SEND: Syscall = Syscall(5);

    /// Receive from an endpoint, blocking until a message arrives.
    pub const RECV: Syscall = Syscall(6);

    /// Send and wait for a reply on a one-shot reply token.
    pub const CALL: Syscall = Syscall(7);

    /// Reply once to a received request.
    ///
    /// The token is consumed by this call. A second reply with the same token
    /// fails rather than duplicating the answer — the invariant the report calls
    /// "a ReplyToken can succeed exactly once".
    pub const REPLY: Syscall = Syscall(8);

    /// Map a region of an address space.
    pub const MAP: Syscall = Syscall(9);

    /// Unmap a previously mapped region.
    pub const UNMAP: Syscall = Syscall(10);

    /// Start a thread in an existing address space.
    pub const THREAD_SPAWN: Syscall = Syscall(11);

    /// Yield the remainder of this thread's quantum.
    pub const YIELD: Syscall = Syscall(12);

    /// Exit the calling thread.
    pub const THREAD_EXIT: Syscall = Syscall(13);

    /// Wait for a capability to become revoked.
    ///
    /// This is how a process learns it lost authority, without polling: it blocks
    /// and the kernel wakes it on revocation.
    pub const CANCEL_WAIT: Syscall = Syscall(14);

    /// Write to a file descriptor.
    ///
    /// Arguments: `fd`, `buffer`, `length`. The buffer is a *user* address; the
    /// kernel copies out of it. Returns the number of bytes written, or a
    /// negative error code.
    ///
    /// File descriptor `1` is standard output and `2` is standard error. Both
    /// go to the kernel's serial console, which is the only sink a user process
    /// has before the console service exists in M4.
    ///
    /// The write is *unbuffered* from the caller's point of view: the bytes are
    /// in the console by the time the call returns. That matters for a
    /// program that is about to exit -- the last line must not be lost in a
    /// buffer that is never flushed.
    pub const WRITE: Syscall = Syscall(15);

    /// Execute an image from the initramfs as a new process.
    ///
    /// Arguments: `name_ptr`, `name_len`. The caller must hold `SPAWN`
    /// authority. On success the parent receives the management endpoint for
    /// the child; the child retrieves its side with `ENDPOINT_SELF`.
    pub const PROCESS_EXEC: Syscall = Syscall(16);

    /// Create the first side of an endpoint pair.
    pub const ENDPOINT_CREATE: Syscall = Syscall(17);

    /// Create the opposite side of an endpoint pair already held by this task.
    pub const ENDPOINT_PEER_CREATE: Syscall = Syscall(18);

    /// Return the management endpoint the kernel gave this process at spawn.
    pub const ENDPOINT_SELF: Syscall = Syscall(19);

    /// Return the number of live kernel tasks, for `ps`.
    pub const PROCESS_COUNT: Syscall = Syscall(20);

    /// Every number currently assigned.
    pub const ALL: &[Syscall] = &[
        GET_VERSION,
        OBJECT_CREATE,
        OBJECT_DESTROY,
        CAP_DEREGATE,
        CAP_REVOKE,
        SEND,
        RECV,
        CALL,
        REPLY,
        MAP,
        UNMAP,
        THREAD_SPAWN,
        YIELD,
        THREAD_EXIT,
        CANCEL_WAIT,
        WRITE,
        PROCESS_EXEC,
        ENDPOINT_CREATE,
        ENDPOINT_PEER_CREATE,
        ENDPOINT_SELF,
        PROCESS_COUNT,
    ];

    /// A name for each number, for logs and backtraces.
    pub fn name(number: Syscall) -> &'static str {
        match number {
            GET_VERSION => "get_version",
            OBJECT_CREATE => "object_create",
            OBJECT_DESTROY => "object_destroy",
            CAP_DEREGATE => "cap_delegate",
            CAP_REVOKE => "cap_revoke",
            SEND => "send",
            RECV => "recv",
            CALL => "call",
            REPLY => "reply",
            MAP => "map",
            UNMAP => "unmap",
            THREAD_SPAWN => "thread_spawn",
            YIELD => "yield",
            THREAD_EXIT => "thread_exit",
            CANCEL_WAIT => "cancel_wait",
            WRITE => "write",
            PROCESS_EXEC => "process_exec",
            ENDPOINT_CREATE => "endpoint_create",
            ENDPOINT_PEER_CREATE => "endpoint_peer_create",
            ENDPOINT_SELF => "endpoint_self",
            PROCESS_COUNT => "process_count",
            // Numbers deliberately not implemented yet are named so a binary
            // that calls one produces a readable log rather than "unknown
            // syscall 15". The list is empty in v0.1.0 but the arm exists so
            // adding a reserved name later cannot change this function's shape.
            _ => "reserved",
        }
    }
}

/// The kind of object [`nr::OBJECT_CREATE`] can make.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u32)]
pub enum ObjectType {
    /// A one-to-many message channel.
    Endpoint = 1,
    /// A task (process).
    Task = 2,
    /// A thread within a task.
    Thread = 3,
    /// An address space.
    AddressSpace = 4,
    /// Shareable physical memory.
    MemoryObject = 5,
    /// A one-shot wakeup.
    Notification = 6,
    /// A device interrupt line.
    Irq = 7,
    /// A physical device register range.
    IoRegion = 8,
    /// The authority to create tasks.
    TaskFactory = 9,
}

impl ObjectType {
    /// Recover a type from its numeric value.
    ///
    /// Returns `None` for an unknown value rather than a catch-all, so a newer
    /// caller cannot make the kernel act on a type it does not understand.
    pub const fn from_number(value: u32) -> Option<ObjectType> {
        Some(match value {
            1 => ObjectType::Endpoint,
            2 => ObjectType::Task,
            3 => ObjectType::Thread,
            4 => ObjectType::AddressSpace,
            5 => ObjectType::MemoryObject,
            6 => ObjectType::Notification,
            7 => ObjectType::Irq,
            8 => ObjectType::IoRegion,
            9 => ObjectType::TaskFactory,
            _ => return None,
        })
    }

    /// A stable name for logs.
    pub const fn name(self) -> &'static str {
        match self {
            ObjectType::Endpoint => "endpoint",
            ObjectType::Task => "task",
            ObjectType::Thread => "thread",
            ObjectType::AddressSpace => "address-space",
            ObjectType::MemoryObject => "memory-object",
            ObjectType::Notification => "notification",
            ObjectType::Irq => "irq",
            ObjectType::IoRegion => "io-region",
            ObjectType::TaskFactory => "task-factory",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ObjectType, Syscall, nr};

    #[test]
    fn syscall_numbers_are_dense_and_unique() {
        // Dense numbering is not required by the ABI, but a duplicate would be a
        // silent, catastrophic bug: two different syscalls sharing a number.
        let mut seen = [false; 64];
        for syscall in nr::ALL {
            let index = syscall.number() as usize;
            assert!(
                index < seen.len(),
                "syscall number out of the table: {index}"
            );
            assert!(!seen[index], "duplicate syscall number {index}");
            seen[index] = true;
        }
    }

    #[test]
    fn every_syscall_has_a_name() {
        for syscall in nr::ALL {
            assert_ne!(
                nr::name(*syscall),
                "reserved",
                "syscall {syscall:?} has no name"
            );
            assert!(!nr::name(*syscall).is_empty());
        }
    }

    #[test]
    fn an_unassigned_number_reads_as_reserved() {
        // A binary calling a syscall this kernel does not implement must log
        // something a reader can act on.
        assert_eq!(nr::name(Syscall(9999)), "reserved");
    }

    #[test]
    fn syscall_zero_is_the_version_query() {
        // Number 0 is what an uninitialised `RAX` looks like, so it must be
        // harmless: a process that jumps to a null function pointer gets a clean
        // version string rather than an arbitrary operation.
        assert_eq!(nr::GET_VERSION.number(), 0);
    }

    #[test]
    fn object_types_round_trip() {
        const ALL: [ObjectType; 9] = [
            ObjectType::Endpoint,
            ObjectType::Task,
            ObjectType::Thread,
            ObjectType::AddressSpace,
            ObjectType::MemoryObject,
            ObjectType::Notification,
            ObjectType::Irq,
            ObjectType::IoRegion,
            ObjectType::TaskFactory,
        ];

        for object in ALL {
            assert_eq!(ObjectType::from_number(object as u32), Some(object));
            assert!(!object.name().is_empty());
        }
    }

    #[test]
    fn an_unknown_object_type_is_refused_not_defaulted() {
        // Defaulting to `Endpoint` on an unrecognised value would let a newer
        // caller's intent be served by an older kernel doing the wrong thing.
        assert_eq!(ObjectType::from_number(0), None);
        assert_eq!(ObjectType::from_number(4242), None);
    }
}
