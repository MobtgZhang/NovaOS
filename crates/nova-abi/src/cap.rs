//! Capability handles: unforgeable, generational references to kernel objects.
//!
//! # Why a handle has a generation
//!
//! A bare slot index is not safe. The obvious design — "slot 7 is your endpoint"
//! — has an ABA problem: you close it, the kernel reuses slot 7 for a different
//! object, and a stale handle now silently refers to the new object. That is a
//! privilege escalation, not a bug, because the caller may hold rights to the new
//! object that it was never granted on the old one.
//!
//! NovaOS splits the handle into two fields for that reason:
//!
//! ```text
//!  63                    32 31                     0
//! ┌────────────────────────┬────────────────────────┐
//! │        generation      │          slot          │
//! │          32 bits        │         32 bits        │
//! └────────────────────────┴────────────────────────┘
//! ```
//!
//! Reusing a slot bumps its generation, so every handle naming the old slot stops
//! validating. The generation is kernel-owned: userspace cannot forge one, because
//! `slot` alone is not a valid handle and the kernel checks both halves.
//!
//! # Slot 0 is never valid
//!
//! A zeroed handle is what an uninitialised `CapHandle` looks like. Reserving
//! slot 0 means a forgotten initialisation is caught at the first syscall rather
//! than resolving to whatever the kernel put in slot 0.

/// An unforgeable reference to a kernel object.
///
/// `#[repr(transparent)]` so the two halves are exactly one 64-bit register, and
/// so a handle can be passed in a syscall argument without a wrapper struct.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Hash)]
#[repr(transparent)]
pub struct CapHandle(u64);

impl CapHandle {
    /// The all-zero handle. Never valid, by construction.
    pub const INVALID: CapHandle = CapHandle(0);

    /// Build a handle from its two halves.
    ///
    /// The `generation` is only ever nonzero in practice: wrapping to zero would
    /// make stale handles indistinguishable from the invalid handle.
    pub const fn new(slot: u32, generation: u32) -> CapHandle {
        CapHandle(((generation as u64) << 32) | (slot as u64))
    }

    /// The slot index, which is meaningless without a matching generation.
    pub const fn slot(self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32
    }

    /// The generation, which is what distinguishes a reused slot from a live one.
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// Whether this is the reserved invalid handle.
    pub const fn is_invalid(self) -> bool {
        self.0 == 0
    }

    /// The raw 64-bit value, for passing across the syscall boundary.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Recover a handle from its raw form.
    pub const fn from_raw(raw: u64) -> CapHandle {
        CapHandle(raw)
    }
}

impl core::fmt::Display for CapHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_invalid() {
            return f.write_str("cap(invalid)");
        }
        write!(f, "cap(slot={},gen={})", self.slot(), self.generation())
    }
}

/// Rights on a capability, as a bit set.
///
/// # Why not just a `u32` ordinal
///
/// An ordinal ("right 7 is SEND") means the kernel has to map it to a bit
/// somewhere, every caller has to agree on the mapping, and a bug in that mapping
/// silently grants the wrong access. Bits make the set directly comparable, which
/// is what the central invariant needs: `Rights(child) ⊆ Rights(parent)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Hash)]
#[repr(transparent)]
pub struct Rights(u32);

impl Rights {
    /// No rights at all. A handle with this grants nothing.
    pub const EMPTY: Rights = Rights(0);

    /// Inspect an object without acting on it.
    pub const INSPECT: Rights = Rights(1 << 0);
    /// Act on the object, in whatever way it defines.
    pub const CONTROL: Rights = Rights(1 << 1);
    /// Send a message to an endpoint.
    pub const SEND: Rights = Rights(1 << 2);
    /// Receive a message from an endpoint.
    pub const RECEIVE: Rights = Rights(1 << 3);
    /// Derive new, narrower capabilities from this one.
    pub const GRANT: Rights = Rights(1 << 4);
    /// Create further objects of the same type.
    pub const SPAWN: Rights = Rights(1 << 5);
    /// Read the object's contents.
    pub const READ: Rights = Rights(1 << 6);
    /// Modify the object's contents.
    pub const WRITE: Rights = Rights(1 << 7);
    /// Map the object into an address space.
    pub const MAP: Rights = Rights(1 << 8);
    /// Reply to a request exactly once.
    pub const REPLY: Rights = Rights(1 << 9);

    /// Every right the kernel knows about. Used to validate that a derived
    /// capability does not smuggle in an undefined bit.
    pub const ALL: Rights = Rights((1 << 10) - 1);

    /// The raw bit pattern.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build from raw bits, rejecting anything outside [`Rights::ALL`].
    ///
    /// Returning `None` for unknown bits matters: a capability table entry that
    /// arrived with a bit the kernel does not understand must be refused rather
    /// than masked, because masking it would silently drop a right the holder
    /// believes it has.
    pub const fn from_bits(bits: u32) -> Option<Rights> {
        if bits & !Self::ALL.0 == 0 {
            Some(Rights(bits))
        } else {
            None
        }
    }

    /// Whether every right in `other` is held.
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// The intersection: the rights valid for a child derived from this handle.
    ///
    /// This is *the* narrowing operation. Deriving is `parent ∩ requested`, never
    /// `requested`, which is what makes "rights can only shrink" a property of
    /// the code rather than of every call site's care.
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    /// The union.
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    /// Remove `other`'s rights.
    pub const fn without(self, other: Rights) -> Rights {
        Rights(self.0 & !other.0)
    }

    /// Whether no rights are held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// A stable comma-separated list, for logs and test failures.
    pub fn names(self) -> alloc_free::Names {
        const ENTRIES: [(Rights, &str); 10] = [
            (Rights::INSPECT, "INSPECT"),
            (Rights::CONTROL, "CONTROL"),
            (Rights::SEND, "SEND"),
            (Rights::RECEIVE, "RECEIVE"),
            (Rights::GRANT, "GRANT"),
            (Rights::SPAWN, "SPAWN"),
            (Rights::READ, "READ"),
            (Rights::WRITE, "WRITE"),
            (Rights::MAP, "MAP"),
            (Rights::REPLY, "REPLY"),
        ];
        alloc_free::Names::new(self, &ENTRIES)
    }
}

/// Right names without an allocator.
///
/// `Rights::names()` is called from `no_std` kernel logging and from user-space
/// diagnostics alike, so it cannot use `alloc::string::String`. This collects the
/// matching names into a fixed 64-byte buffer instead: at most ten names of at
/// most eight characters each, plus separators, fits comfortably.
pub mod alloc_free {
    use super::Rights;

    /// A comma-separated right list in a bounded buffer.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Names {
        buffer: [u8; 96],
        len: usize,
    }

    impl Names {
        /// Collect the names of the rights present in `rights`.
        pub fn new(rights: Rights, entries: &[(Rights, &'static str)]) -> Self {
            let mut buffer = [0u8; 96];
            let mut len = 0;

            for (bit, name) in entries {
                if !rights.contains(*bit) {
                    continue;
                }
                if len > 0 && len + 1 < buffer.len() {
                    buffer[len] = b',';
                    len += 1;
                }
                for &byte in name.as_bytes() {
                    if len >= buffer.len() {
                        break;
                    }
                    buffer[len] = byte;
                    len += 1;
                }
            }

            Names { buffer, len }
        }

        /// The rendered list.
        pub fn as_str(&self) -> &str {
            core::str::from_utf8(&self.buffer[..self.len]).unwrap_or("")
        }

        /// Whether the list is empty, which is what an `EMPTY` rights set
        /// renders as.
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
    }

    impl core::fmt::Display for Names {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str(self.as_str())
        }
    }
}

/// Union of two right sets.
impl core::ops::BitOr for Rights {
    type Output = Rights;

    fn bitor(self, rhs: Rights) -> Rights {
        self.union(rhs)
    }
}

/// Union, assigning.
impl core::ops::BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Rights) {
        self.0 |= rhs.0;
    }
}

/// Intersection.
impl core::ops::BitAnd for Rights {
    type Output = Rights;

    fn bitand(self, rhs: Rights) -> Rights {
        self.intersect(rhs)
    }
}

/// `A | B` reads naturally for a rights set, so it is the *only* operator that
/// makes `Rights` usable without a method call — `SEND | RECEIVE` is how the
/// kernel's own tables spell a grant. The set operations that can *lose* rights
/// (`intersect`, `without`) are deliberately method-only, so narrowing always
/// reads as narrowing.
#[cfg(test)]
mod tests {
    use super::{CapHandle, Rights};

    #[test]
    fn a_handle_is_exactly_one_word() {
        // A handle travels in a syscall argument register. Anything larger would
        // force a two-register convention that no architecture guarantees.
        assert_eq!(size_of::<CapHandle>(), 8);
    }

    #[test]
    fn the_invalid_handle_is_zero_and_reports_itself() {
        assert!(CapHandle::INVALID.is_invalid());
        assert_eq!(CapHandle::default(), CapHandle::INVALID);
        assert_eq!(CapHandle::default().slot(), 0);
        assert_eq!(CapHandle::default().generation(), 0);
    }

    #[test]
    fn the_two_halves_round_trip() {
        let handle = CapHandle::new(7, 3);
        assert_eq!(handle.slot(), 7);
        assert_eq!(handle.generation(), 3);
        assert_eq!(CapHandle::from_raw(handle.raw()), handle);
    }

    #[test]
    fn a_reused_slot_is_distinguishable_by_its_generation() {
        // The whole point of the generation field: two handles naming the same
        // slot are different capabilities once the slot is recycled.
        let old = CapHandle::new(7, 1);
        let new = CapHandle::new(7, 2);
        assert_eq!(old.slot(), new.slot());
        assert_ne!(old, new);
        assert_ne!(old.generation(), new.generation());
    }

    #[test]
    fn slot_zero_is_never_a_valid_handle() {
        // Generation 0 with slot 0 is the reserved invalid handle, so the kernel
        // can tell "forgot to initialise" from "resolved a real capability".
        assert!(CapHandle::new(0, 0).is_invalid());
        assert!(!CapHandle::new(0, 1).is_invalid());
        assert!(!CapHandle::new(1, 0).is_invalid());
    }

    #[test]
    fn rights_round_trip_through_bits() {
        let rights = Rights::SEND | Rights::RECEIVE;
        assert_eq!(Rights::from_bits(rights.bits()), Some(rights));
    }

    #[test]
    fn unknown_right_bits_are_refused_rather_than_masked() {
        // Masking would silently drop a right the holder believes it has, which
        // is the same class of bug as an ABA handle.
        assert_eq!(Rights::from_bits(Rights::ALL.bits() | (1 << 31)), None);
    }

    #[test]
    fn derivation_can_only_narrow() {
        // THE central invariant: a child never gains a right the parent lacked.
        let parent = Rights::SEND | Rights::RECEIVE | Rights::GRANT;
        let requested = Rights::SEND | Rights::MAP;

        let child = parent.intersect(requested);
        assert_eq!(child, Rights::SEND);
        assert!(child.contains(Rights::SEND));
        assert!(!child.contains(Rights::RECEIVE));
        assert!(!child.contains(Rights::MAP), "MAP was never held");
    }

    #[test]
    fn the_intersection_is_commutative_and_idempotent() {
        let a = Rights::SEND | Rights::READ;
        let b = Rights::READ | Rights::WRITE;
        assert_eq!(a.intersect(b), b.intersect(a));
        assert_eq!(a.intersect(a), a);
        assert_eq!(a.intersect(Rights::EMPTY), Rights::EMPTY);
    }

    #[test]
    fn requesting_a_right_the_parent_lacks_yields_nothing() {
        let parent = Rights::SEND;
        assert_eq!(parent.intersect(Rights::WRITE), Rights::EMPTY);
        assert!(parent.intersect(Rights::WRITE).is_empty());
    }

    #[test]
    fn without_removes_exactly_the_named_rights() {
        let all = Rights::SEND | Rights::RECEIVE;
        assert_eq!(all.without(Rights::RECEIVE), Rights::SEND);
        assert_eq!(all.without(Rights::SEND | Rights::RECEIVE), Rights::EMPTY);
        // Removing a right not held is a no-op, not an underflow.
        assert_eq!(all.without(Rights::WRITE), all);
    }

    #[test]
    fn rights_is_a_single_word() {
        assert_eq!(size_of::<Rights>(), 4);
    }

    #[test]
    fn names_render_the_set_members() {
        let names = (Rights::SEND | Rights::RECEIVE).names();
        assert_eq!(names.as_str(), "SEND,RECEIVE");

        assert!(Rights::EMPTY.names().is_empty());
        assert_eq!(Rights::GRANT.names().as_str(), "GRANT");
    }

    #[test]
    fn names_stay_within_their_bound() {
        // Every right at once is the worst case; the buffer must not truncate
        // mid-name into invalid UTF-8.
        let all = Rights::ALL;
        let rendered = all.names();
        let text = rendered.as_str();
        for name in [
            "INSPECT", "CONTROL", "SEND", "RECEIVE", "GRANT", "SPAWN", "READ", "WRITE", "MAP",
            "REPLY",
        ] {
            assert!(text.contains(name), "{name} missing from {text}");
        }
    }

    #[test]
    fn all_covers_every_defined_right() {
        // A new right that forgets to join `ALL` would fail `from_bits`
        // validation for any capability carrying it.
        for rights in [
            Rights::INSPECT,
            Rights::CONTROL,
            Rights::SEND,
            Rights::RECEIVE,
            Rights::GRANT,
            Rights::SPAWN,
            Rights::READ,
            Rights::WRITE,
            Rights::MAP,
            Rights::REPLY,
        ] {
            assert!(Rights::ALL.contains(rights), "{} is outside ALL", rights.names());
        }
    }
}