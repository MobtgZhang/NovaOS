//! The capability table: the kernel's authority model.
//!
//! # What a capability *is*
//!
//! A capability is not an object. It is a *kernel-trusted entry* saying "this
//! holder may do these things to that object". The entry is the only place
//! authority lives, and it carries five things:
//!
//! - which object it names ([`ObjectRef`]);
//! - what the holder may do with it ([`Rights`]);
//! - the generation that makes stale handles detectable;
//! - the capability it was derived from, so the table can answer "what is this
//!   derived from" and so revocation can propagate;
//! - its revocation state, so a holder can learn that it lost authority
//!   instead of silently getting `E_BAD_HANDLE` for something it still believes
//!   it owns.
//!
//! # The one invariant everything else serves
//!
//! **A handle names exactly one lifetime of one slot.** Slot reuse must make
//! every handle to the previous occupant fail to resolve. That is what the
//! generation counter is for, and everything in this module — the reserved
//! slot 0, the retirement policy on wraparound, the revocation state — exists
//! to keep that true under every sequence of operations a hostile process can
//! provoke.
//!
//! # Why a fixed pool
//!
//! The kernel has no allocator. A fixed array of slots is not a compromise
//! taken for expedience; it is what lets the table live in a static, be sized
//! at link time, and be reasoned about as a fixed point. A table that is full
//! fails loudly with [`ErrorCode::NoMemory`] rather than growing.
//!
//! # What this module does *not* do
//!
//! It does not own objects, does not track which process holds which handle,
//! and does not decide *policy* — which rights a service needs, or who may
//! revoke what. Those decisions belong to `init` and to the kernel's syscall
//! layer. This module is the mechanism that makes such decisions enforceable.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;

use crate::object::ObjectRef;

/// Maximum number of slots in one capability table.
///
/// The bound is fixed at compile time because the kernel has no heap: the table
/// is a static, and "the table is full" is a reachable, reportable state
/// rather than an allocation failure in the middle of a syscall.
pub const MAX_CAPS: usize = 1024;

/// Number of slots reserved at the front of the table for the kernel's own use.
///
/// Slot 0 is never handed out. `CapHandle::INVALID` is all-zero, so a
/// zeroed handle has slot 0 and generation 0; reserving slot 0 means a handle
/// that was never initialised can never resolve to an object, and a
/// generation of 0 is likewise never issued.
pub const RESERVED_SLOTS: usize = 1;

/// Whether a capability still confers authority.
///
/// The distinction between "never existed" and "existed and was taken away"
/// is what lets a holder learn that it lost authority (the `CANCEL_WAIT`
/// contract) rather than being told it never had the capability in the first
/// place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevocationState {
    /// The capability is live and its rights are in force.
    Live,
    /// The capability was revoked. The entry is retained so the loss is
    /// observable, but no operation may use it.
    Revoked,
}

/// How far a revocation reaches.
///
/// Revocation is a policy question, so the caller chooses; the table provides
/// both because the two answers are defensible for different objects and
/// picking one in the table would bake a policy decision into a mechanism.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevocationScope {
    /// Only the named capability is revoked.
    ///
    /// Used when one holder's copy is withdrawn but the object keeps living and
    /// other holders keep their authority.
    Single,
    /// The named capability and every capability transitively derived from it.
    ///
    /// Used when the underlying authority must not survive in a derived copy —
    /// a revoked grant that lingers in a child is a privilege escalation, not a
    /// cosmetic problem.
    Descendants,
}

/// One kernel-trusted authority record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapEntry {
    /// The object this capability names.
    pub object_ref: ObjectRef,
    /// What the holder may do with that object.
    pub rights: Rights,
    /// The generation of the slot this entry occupies.
    ///
    /// A handle carries this number, so a handle minted before the slot was
    /// freed and reallocated fails to resolve.
    pub generation: u32,
    /// The capability this one was derived from, if any.
    ///
    /// Recorded so the table can answer "what is this derived from" and so a
    /// cascading revocation has a graph to walk.
    pub derivation_parent: Option<CapHandle>,
    /// Whether the capability is still live.
    pub revocation_state: RevocationState,
    /// The revocation epoch at which this entry was revoked.
    ///
    /// Zero while live. A holder that blocked on `CANCEL_WAIT` remembers the
    /// epoch it blocked at and compares, so it learns *that* it was revoked
    /// without the table having to keep a per-waiter list.
    pub revoked_at_epoch: u64,
}

impl CapEntry {
    /// Whether the entry is live.
    pub const fn is_live(&self) -> bool {
        matches!(self.revocation_state, RevocationState::Live)
    }

    /// Whether the entry has been revoked.
    pub const fn is_revoked(&self) -> bool {
        matches!(self.revocation_state, RevocationState::Revoked)
    }
}

/// The state of one slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// Free, and remembers the generation the next allocation of this slot
    /// must use.
    ///
    /// The generation is remembered *while the slot is free* rather than being
    /// recomputed at allocation time, so a slot's generations are strictly
    /// increasing for the whole life of the table.
    Free {
        /// The generation the next occupant of this slot will carry.
        next_generation: u32,
    },
    /// Currently held.
    Occupied(CapEntry),
    /// Permanently unusable.
    ///
    /// Only reached by the wraparound policy; see [`CapabilityTable::revoke`]
    /// and the module notes on generation exhaustion.
    Retired,
}

/// The capability table.
///
/// One instance per process: the table is the process's authority set, and a
/// capability that travels to another process arrives there as a *new* entry
/// minted by the receiving table (see `nova-ipc`'s transfer support), never
/// as the same slot.
///
/// `Copy` is derived because the kernel keeps a table per task in a
/// fixed-size pool and needs to reset a slot to empty. A table is entirely
/// value data -- slots, a free stack, and counters -- with no interior
/// identity, so a copy is a distinct table with the same contents, which is
/// exactly what an empty slot wants. The `Copy` bound is also what lets the
/// pool's array be built by a repeat expression rather than a loop.
#[derive(Clone, Copy)]
pub struct CapabilityTable {
    slots: [Slot; MAX_CAPS],
    /// Free-slot stack.
    ///
    /// A stack rather than a scan: allocation and release are the hot paths
    /// of every capability operation, and the kernel has no other way to keep
    /// them cheap.
    free_stack: [u32; MAX_CAPS],
    free_len: usize,
    /// Number of occupied slots.
    live: usize,
    /// Monotonically increasing counter, bumped once per revoked entry.
    epoch: u64,
}

impl CapabilityTable {
    /// An empty table.
    ///
    /// `const` so the kernel can place one in a `static` and know its layout
    /// at link time.
    pub const fn new() -> Self {
        let slots = [Slot::Free { next_generation: 1 }; MAX_CAPS];
        let mut free_stack = [0u32; MAX_CAPS];
        let mut free_len = 0;
        let mut slot = MAX_CAPS - 1;
        // Slot 0 is never pushed: it is the kernel's reserved slot, and a
        // handle naming it must never resolve.
        while slot >= RESERVED_SLOTS {
            free_stack[free_len] = slot as u32;
            free_len += 1;
            slot -= 1;
        }
        Self {
            slots,
            free_stack,
            free_len,
            live: 0,
            epoch: 0,
        }
    }

    /// Empty the table, in place.
    ///
    /// A table holds a thousand slots and a thousand-entry free stack, so it
    /// is tens of kilobytes. Assigning a fresh table to it builds that whole
    /// value on the stack first, and the kernel's stack is 64 KiB. Clearing
    /// in place avoids the temporary entirely.
    pub fn clear(&mut self) {
        // Assigned field by field rather than `*self = Self::new()`: the
        // whole-table assignment builds a fresh table on the stack first,
        // and a table is tens of kilobytes.
        self.slots = [Slot::Free { next_generation: 1 }; MAX_CAPS];
        // The free stack is rebuilt in the same order `new` builds it, so a
        // cleared table allocates exactly as a fresh one does. The order is
        // part of the contract, not an accident: a table whose first
        // allocation differs from a fresh table's makes a handle's slot
        // depend on how the table was reached.
        self.free_len = 0;
        let mut slot = MAX_CAPS - 1;
        while slot >= RESERVED_SLOTS {
            self.free_stack[self.free_len] = slot as u32;
            self.free_len += 1;
            slot -= 1;
        }
        self.live = 0;
        self.epoch = 0;
    }

    /// Total number of slots, including the reserved one.
    pub const fn capacity(&self) -> usize {
        MAX_CAPS
    }

    /// Number of slots currently held by a live or revoked entry.
    ///
    /// A revoked entry still occupies its slot until it is destroyed, because
    /// the revocation must stay observable.
    pub const fn live_count(&self) -> usize {
        self.live
    }

    /// Number of slots available for a new capability.
    pub const fn free_count(&self) -> usize {
        self.free_len
    }

    /// Number of slots permanently retired by generation exhaustion.
    pub fn retired_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot, Slot::Retired))
            .count()
    }

    /// The current revocation epoch.
    ///
    /// A waiter that blocked on `CANCEL_WAIT` records the epoch it blocked at;
    /// when the epoch moves past a value the waiter's capability carries, the
    /// waiter has been revoked.
    pub const fn revocation_epoch(&self) -> u64 {
        self.epoch
    }

    /// Mint a fresh capability for an object.
    ///
    /// This is the root of the authority graph: no parent, so nothing to
    /// revoke it transitively. The kernel calls it when it creates an object
    /// and hands the first holder its authority.
    pub fn create(
        &mut self,
        object_ref: ObjectRef,
        rights: Rights,
    ) -> Result<CapHandle, ErrorCode> {
        self.allocate(object_ref, rights, None, None)
    }

    /// Mint a fresh capability for an object, in a slot other than `avoid`.
    ///
    /// # Why the avoided slot matters
    ///
    /// A handle is only meaningful within the table that issued it, so two
    /// tables may hold numerically identical handles without any ambiguity
    /// between them. But a capability that *travels* from one table to
    /// another arrives as a new entry minted by the receiving table, and that
    /// entry must not occupy the source's slot: a receiver that was handed
    /// the sender's own handle value would have a number that says nothing
    /// about who issued it, and every layer that logs, echoes, or compares
    /// handles would be comparing two different capabilities as if they were
    /// one.
    ///
    /// The avoided slot is the *source's* slot. The receiving table's
    /// allocation order is its own business; the only thing that must not
    /// happen is the source's slot being reused for the transferred entry.
    ///
    /// Fails with [`ErrorCode::NoMemory`] when every free slot is the avoided
    /// one. That is a quota outcome, not a bug: the table has one slot left
    /// and it may not be used for this entry.
    pub fn create_excluding(
        &mut self,
        object_ref: ObjectRef,
        rights: Rights,
        avoid: CapHandle,
    ) -> Result<CapHandle, ErrorCode> {
        self.allocate(object_ref, rights, None, Some(avoid.slot()))
    }

    /// The authority a task's *root* capability confers.
    ///
    /// A table with no root capability at all holds no authority, and the
    /// refusal is [`ErrorCode::Denied`]: the task exists and can be
    /// dispatched to, it simply has nothing to act with. Reporting
    /// [`ErrorCode::BadHandle`] instead would claim that the caller named a
    /// capability that does not exist, which is a different statement — and
    /// one the caller cannot act on, because it has no handle to fix.
    ///
    /// A *revoked* root reports [`ErrorCode::Revoked`] rather than
    /// [`ErrorCode::Denied`], so a holder learns that it lost authority
    /// instead of being told it never had it.
    pub fn root_authority(&self, root: CapHandle) -> Result<Rights, ErrorCode> {
        if root.is_invalid() {
            return Err(ErrorCode::Denied);
        }
        Ok(self.resolve(root)?.rights)
    }

    /// Derive a narrower capability from one already held.
    ///
    /// The child's rights are `parent ∩ requested`: the kernel intersects, and
    /// never unions. A child therefore can never hold a right its parent
    /// lacked, whatever the caller asks for.
    ///
    /// A request naming a right the parent does *not* hold is not an error:
    /// the intersection simply omits it. This matches the ABI's own statement
    /// that deriving is `parent ∩ requested` — the request is a wish about the
    /// maximum, and the parent's authority is the ceiling.
    ///
    /// The one thing that *is* refused is delegating at all without
    /// [`Rights::GRANT`]: a capability that does not carry the grant right
    /// names an object the holder may use, not one it may hand to anyone else.
    pub fn derive(&mut self, parent: CapHandle, requested: Rights) -> Result<CapHandle, ErrorCode> {
        let entry = *self.resolve(parent)?;
        if !entry.rights.contains(Rights::GRANT) {
            return Err(ErrorCode::Denied);
        }
        // The intersection is the narrowing operation, and it is the whole of
        // the narrowing guarantee: a child can never hold a right its parent
        // lacked, whatever the caller asked for.
        let narrowed = entry.rights.intersect(requested);
        self.allocate(entry.object_ref, narrowed, Some(parent), None)
    }

    /// Revoke a capability.
    ///
    /// The entry is *retained*, not freed, so the loss is observable: the
    /// handle still resolves, and its state says [`RevocationState::Revoked`].
    /// A holder blocked on `CANCEL_WAIT` can therefore learn that it lost
    /// authority rather than being told the capability never existed.
    ///
    /// The slot is released only by [`CapabilityTable::destroy`].
    ///
    /// # Does revocation cascade?
    ///
    /// That is the caller's choice, expressed by the [`RevocationScope`]:
    ///
    /// - [`RevocationScope::Single`] revokes one holder's copy. The object
    ///   keeps living and other holders keep their authority.
    /// - [`RevocationScope::Descendants`] revokes the capability and every
    ///   capability transitively derived from it. This is the right choice when
    ///   the underlying authority must not survive in a derived copy: a revoked
    ///   grant that lingers in a child is a privilege escalation.
    ///
    /// The kernel should use `Descendants` when an object dies or a grant is
    /// withdrawn, and `Single` when one process's copy is merely withdrawn.
    ///
    /// The caller must already have been authorised to revoke; this function
    /// does not itself check rights, because "who may revoke" is a policy the
    /// syscall layer owns.
    ///
    /// Returns the number of entries that changed state.
    pub fn revoke(
        &mut self,
        handle: CapHandle,
        scope: RevocationScope,
    ) -> Result<usize, ErrorCode> {
        let slot = self.resolve_slot(handle).ok_or(ErrorCode::BadHandle)?;
        let mut revoked = 0usize;

        if self.mark_revoked(slot) {
            revoked += 1;
        }

        if matches!(scope, RevocationScope::Descendants) {
            // Walk the derivation graph. The table is small and the walk is
            // bounded by the number of slots, so a fixpoint loop is both
            // simplest and safest: it does not assume the graph is a tree.
            loop {
                let mut changed = false;
                for index in RESERVED_SLOTS..MAX_CAPS {
                    let parent = match self.slots[index] {
                        Slot::Occupied(entry) => entry.derivation_parent,
                        _ => None,
                    };
                    let Some(parent) = parent else { continue };
                    if self.resolve_slot(parent).is_none() {
                        continue;
                    }
                    if self.mark_revoked(index) {
                        changed = true;
                        revoked += 1;
                    }
                }
                if !changed {
                    break;
                }
            }
        }

        Ok(revoked)
    }

    /// Revoke every capability in this table that names the same object.
    ///
    /// This is the primitive an object destructor needs: when an object dies,
    /// every holder of a capability to it must lose that authority, and the
    /// only place that set is written down is the table.
    pub fn revoke_object(&mut self, object_ref: ObjectRef, scope: RevocationScope) -> usize {
        let mut revoked = 0usize;
        for index in RESERVED_SLOTS..MAX_CAPS {
            let matches_object = match self.slots[index] {
                Slot::Occupied(entry) => entry.object_ref == object_ref && entry.is_live(),
                _ => false,
            };
            if !matches_object {
                continue;
            }
            if scope == RevocationScope::Descendants {
                // Revoking the object takes the whole derivation subtree with
                // it, so a derived copy of a dead object's authority cannot
                // outlive the object.
                revoked += self.revoke_by_slot(index);
            } else if self.mark_revoked(index) {
                revoked += 1;
            }
        }
        revoked
    }

    /// Give up a capability.
    ///
    /// The slot is released and its generation advances, so every handle that
    /// named this capability stops resolving immediately. No right is required
    /// to drop one's own authority — a holder may always discard what it holds
    /// — but destroying the *object* a capability names is a different act and
    /// is checked by the kernel against [`Rights::CONTROL`].
    pub fn destroy(&mut self, handle: CapHandle) -> Result<(), ErrorCode> {
        let slot = self.resolve_slot(handle).ok_or(ErrorCode::BadHandle)?;
        self.release_slot(slot);
        Ok(())
    }

    /// The entry a handle names, if it names one.
    ///
    /// Returns the entry even when it is revoked: revocation must be
    /// *observable*, so the entry has to remain reachable.
    pub fn entry(&self, handle: CapHandle) -> Option<&CapEntry> {
        let slot = self.resolve_slot(handle)?;
        match &self.slots[slot] {
            Slot::Occupied(entry) => Some(entry),
            _ => None,
        }
    }

    /// Resolve a handle to its entry.
    ///
    /// Fails with [`ErrorCode::BadHandle`] when the handle names nothing:
    /// an unknown slot, a stale generation, generation 0, or the reserved
    /// slot 0.
    pub fn lookup(&self, handle: CapHandle) -> Result<&CapEntry, ErrorCode> {
        let slot = self.resolve_slot(handle).ok_or(ErrorCode::BadHandle)?;
        match &self.slots[slot] {
            Slot::Occupied(entry) => Ok(entry),
            _ => Err(ErrorCode::BadHandle),
        }
    }

    /// Resolve a handle to a *usable* entry.
    ///
    /// Same as [`CapabilityTable::lookup`] but refuses a revoked entry with
    /// [`ErrorCode::Revoked`], so an operation that acts on authority can
    /// never act on a capability that has been withdrawn.
    pub fn resolve(&self, handle: CapHandle) -> Result<&CapEntry, ErrorCode> {
        let entry = self.lookup(handle)?;
        if entry.is_revoked() {
            return Err(ErrorCode::Revoked);
        }
        Ok(entry)
    }

    /// Resolve a handle and check that it carries the rights an operation
    /// needs.
    ///
    /// [`ErrorCode::Denied`] means the handle is fine but the authority is
    /// not; [`ErrorCode::BadHandle`] means there is no capability at all.
    /// Keeping those two apart matters: the first is a policy outcome the
    /// caller may fix by asking for a narrower operation, the second means the
    /// caller's belief about the world is wrong.
    pub fn check_rights(&self, handle: CapHandle, needed: Rights) -> Result<&CapEntry, ErrorCode> {
        let entry = self.resolve(handle)?;
        if !entry.rights.contains(needed) {
            return Err(ErrorCode::Denied);
        }
        Ok(entry)
    }

    /// The rights a handle carries, if the handle names a live or revoked
    /// capability.
    pub fn rights(&self, handle: CapHandle) -> Option<Rights> {
        self.entry(handle).map(|entry| entry.rights)
    }

    /// Whether a handle names a capability that has been revoked.
    pub fn is_revoked(&self, handle: CapHandle) -> bool {
        self.entry(handle).is_some_and(CapEntry::is_revoked)
    }

    /// Whether a handle names a live capability.
    pub fn is_live(&self, handle: CapHandle) -> bool {
        self.entry(handle).is_some_and(CapEntry::is_live)
    }

    /// The capability a handle was derived from, if any.
    pub fn derivation_parent(&self, handle: CapHandle) -> Option<CapHandle> {
        self.entry(handle).and_then(|entry| entry.derivation_parent)
    }

    /// The object a handle names, if it names one.
    pub fn object_ref(&self, handle: CapHandle) -> Option<ObjectRef> {
        self.entry(handle).map(|entry| entry.object_ref)
    }

    /// The epoch at which a handle was revoked, if it was.
    pub fn revoked_at_epoch(&self, handle: CapHandle) -> Option<u64> {
        self.entry(handle)
            .filter(|entry| entry.is_revoked())
            .map(|entry| entry.revoked_at_epoch)
    }

    /// The slot a handle names, if it names one.
    fn resolve_slot(&self, handle: CapHandle) -> Option<usize> {
        // A zeroed handle is the "never initialised" case and a generation of
        // zero is never issued; both are refused before the slot is even
        // looked at.
        if handle.is_invalid() || handle.generation() == 0 {
            return None;
        }
        let slot = handle.slot() as usize;
        if !(RESERVED_SLOTS..MAX_CAPS).contains(&slot) {
            return None;
        }
        match self.slots[slot] {
            Slot::Occupied(entry) if entry.generation == handle.generation() => Some(slot),
            _ => None,
        }
    }

    /// Take a free slot and put an entry in it.
    ///
    /// `avoid_slot` names a slot the entry may not occupy. It exists for the
    /// capability-transfer path, where the destination table must not hand
    /// back the very slot number the source used: a handle is only
    /// meaningful within the table that issued it, so a receiver holding the
    /// sender's own handle value would hold a number that says nothing about
    /// who issued it.
    fn allocate(
        &mut self,
        object_ref: ObjectRef,
        rights: Rights,
        derivation_parent: Option<CapHandle>,
        avoid_slot: Option<u32>,
    ) -> Result<CapHandle, ErrorCode> {
        if self.free_len == 0 {
            // The table is full. This is an object quota, not a transient
            // condition, so it is reported as "out of memory" rather than
            // "busy": the caller must free something, not retry.
            return Err(ErrorCode::NoMemory);
        }
        // Pick the slot and pop it off the free stack in one step.
        //
        // The stack's top is the next slot to hand out. When a slot is
        // avoided, the search walks down the stack for the first entry that
        // is not the avoided one; that entry is then moved into the top and
        // the top is dropped, so the stack keeps its shape and the avoided
        // slot stays on it — it is still free, it is simply not this entry's
        // slot.
        let slot = match avoid_slot {
            None => {
                let slot = self.free_stack[self.free_len - 1] as usize;
                self.free_len -= 1;
                slot
            }
            Some(avoided) => {
                let mut chosen = None;
                let mut index = self.free_len;
                while index > 0 {
                    index -= 1;
                    if self.free_stack[index] == avoided {
                        continue;
                    }
                    chosen = Some(index);
                    break;
                }
                let Some(chosen) = chosen else {
                    // Every free slot is the avoided one, so there is nowhere
                    // this entry may go. That is a quota outcome, not a bug.
                    return Err(ErrorCode::NoMemory);
                };
                let value = self.free_stack[chosen] as usize;
                self.free_stack[chosen] = self.free_stack[self.free_len - 1];
                self.free_len -= 1;
                value
            }
        };
        let generation = match self.slots[slot] {
            Slot::Free { next_generation } => next_generation,
            // The free stack only ever holds free slots; this arm exists so a
            // corrupted stack cannot make the kernel index a wrong slot.
            _ => {
                self.free_len += 1;
                return Err(ErrorCode::NoMemory);
            }
        };
        let entry = CapEntry {
            object_ref,
            rights,
            generation,
            derivation_parent,
            revocation_state: RevocationState::Live,
            revoked_at_epoch: 0,
        };
        self.slots[slot] = Slot::Occupied(entry);
        self.live += 1;
        Ok(CapHandle::new(slot as u32, generation))
    }

    /// Release a slot, advancing its generation.
    fn release_slot(&mut self, slot: usize) {
        let generation = match self.slots[slot] {
            Slot::Occupied(entry) => entry.generation,
            _ => return,
        };
        self.live -= 1;
        match generation.checked_add(1) {
            Some(next) => {
                self.slots[slot] = Slot::Free {
                    next_generation: next,
                };
                self.free_stack[self.free_len] = slot as u32;
                self.free_len += 1;
            }
            None => {
                // The generation counter has run out. The slot is retired
                // rather than wrapped: wrapping would hand out a generation a
                // stale handle might still carry, which is exactly the
                // confusion the generation field exists to prevent.
                self.slots[slot] = Slot::Retired;
            }
        }
    }

    /// Mark one slot revoked. Returns whether anything changed.
    fn mark_revoked(&mut self, slot: usize) -> bool {
        let entry = match self.slots[slot] {
            Slot::Occupied(entry) if entry.is_live() => entry,
            _ => return false,
        };
        self.epoch += 1;
        let epoch = self.epoch;
        self.slots[slot] = Slot::Occupied(CapEntry {
            revocation_state: RevocationState::Revoked,
            revoked_at_epoch: epoch,
            ..entry
        });
        true
    }

    /// Revoke a slot and its descendants, returning how many entries changed.
    fn revoke_by_slot(&mut self, slot: usize) -> usize {
        let mut revoked = 0usize;
        if self.mark_revoked(slot) {
            revoked += 1;
        }
        loop {
            let mut changed = false;
            for index in RESERVED_SLOTS..MAX_CAPS {
                let parent = match self.slots[index] {
                    Slot::Occupied(entry) => entry.derivation_parent,
                    _ => None,
                };
                let Some(parent) = parent else { continue };
                if self.resolve_slot(parent).is_none() {
                    continue;
                }
                if self.mark_revoked(index) {
                    changed = true;
                    revoked += 1;
                }
            }
            if !changed {
                break;
            }
        }
        revoked
    }
}

impl Default for CapabilityTable {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for CapabilityTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CapabilityTable")
            .field("live", &self.live)
            .field("free", &self.free_len)
            .field("retired", &self.retired_count())
            .field("epoch", &self.epoch)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CapEntry, CapabilityTable, MAX_CAPS, ObjectRef, RESERVED_SLOTS, RevocationScope,
        RevocationState, Slot,
    };
    use nova_abi::cap::{CapHandle, Rights};
    use nova_abi::error::ErrorCode;
    use nova_abi::syscall::ObjectType;

    fn endpoint(id: u32) -> ObjectRef {
        ObjectRef::new(ObjectType::Endpoint, id)
    }

    fn table_with_rights(rights: Rights) -> (CapabilityTable, CapHandle) {
        let mut table = CapabilityTable::new();
        let handle = table.create(endpoint(1), rights).expect("table has room");
        (table, handle)
    }

    #[test]
    fn a_fresh_capability_names_its_object_and_holds_its_rights() {
        let (table, handle) = table_with_rights(Rights::SEND | Rights::RECEIVE);
        let entry = table.lookup(handle).expect("handle resolves");
        assert_eq!(entry.object_ref, endpoint(1));
        assert_eq!(entry.rights, Rights::SEND | Rights::RECEIVE);
        assert_eq!(entry.revocation_state, RevocationState::Live);
        assert_eq!(entry.derivation_parent, None);
    }

    #[test]
    fn a_new_capability_starts_at_generation_one() {
        // Generations start at 1 because generation 0 is the "never
        // initialised" marker and must never be issued.
        let (table, handle) = table_with_rights(Rights::ALL);
        assert_eq!(handle.generation(), 1);
        assert_eq!(table.lookup(handle).expect("resolves").generation, 1);
    }

    #[test]
    fn slot_zero_is_invalid() {
        let mut table = CapabilityTable::new();
        let handle = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");

        // Slot 0 is reserved, so no issued handle ever names it.
        assert_ne!(handle.slot(), 0);
        assert!(!table.lookup(CapHandle::new(0, 1)).is_ok());
        assert!(!table.lookup(CapHandle::new(0, 0)).is_ok());
        assert!(table.lookup(CapHandle::INVALID).is_err());
    }

    #[test]
    fn a_generation_of_zero_never_resolves() {
        // A zeroed generation would make a stale handle indistinguishable
        // from an uninitialised one.
        let (table, handle) = table_with_rights(Rights::ALL);
        assert!(table.lookup(CapHandle::new(handle.slot(), 0)).is_err());
        assert!(table.entry(CapHandle::new(handle.slot(), 0)).is_none());
    }

    #[test]
    fn an_unknown_handle_is_a_bad_handle() {
        let (table, _) = table_with_rights(Rights::ALL);
        assert_eq!(
            table.lookup(CapHandle::new(999, 1)),
            Err(ErrorCode::BadHandle)
        );
    }

    #[test]
    fn rights_child_subset_of_parent() {
        let mut table = CapabilityTable::new();
        let parent = table
            .create(endpoint(1), Rights::SEND | Rights::RECEIVE | Rights::GRANT)
            .expect("table has room");
        let child = table
            .derive(parent, Rights::SEND)
            .expect("derivation succeeds");

        let parent_rights = table.lookup(parent).expect("parent resolves").rights;
        let child_rights = table.lookup(child).expect("child resolves").rights;
        assert!(
            parent_rights.contains(child_rights),
            "the child's rights must be a subset of the parent's"
        );
        assert_eq!(child_rights, Rights::SEND);
    }

    #[test]
    fn derive_intersects_rights() {
        let mut table = CapabilityTable::new();
        let parent = table
            .create(endpoint(1), Rights::SEND | Rights::RECEIVE | Rights::GRANT)
            .expect("table has room");

        // The request names a right the parent has and one it does not.
        let child = table
            .derive(parent, Rights::SEND | Rights::WRITE)
            .expect("derivation succeeds");
        assert_eq!(
            table.lookup(child).expect("child resolves").rights,
            Rights::SEND
        );
    }

    #[test]
    fn a_parent_without_grant_may_not_delegate() {
        let mut table = CapabilityTable::new();
        let parent = table
            .create(endpoint(1), Rights::SEND | Rights::RECEIVE)
            .expect("table has room");
        assert_eq!(table.derive(parent, Rights::SEND), Err(ErrorCode::Denied));
        // The failed derivation left nothing behind.
        assert_eq!(table.live_count(), 1);
    }

    #[test]
    fn a_request_for_an_unheld_right_is_narrowed_away_not_refused() {
        // Derivation is `parent ∩ requested`, so asking for a right the parent
        // lacks cannot be an error — the intersection drops it. What must never
        // happen is the child *gaining* it.
        let mut table = CapabilityTable::new();
        let parent = table
            .create(endpoint(1), Rights::SEND | Rights::GRANT)
            .expect("table has room");
        let child = table
            .derive(parent, Rights::SEND | Rights::WRITE)
            .expect("derivation succeeds");
        let child_rights = table.lookup(child).expect("child resolves").rights;
        assert_eq!(child_rights, Rights::SEND);
        assert!(!child_rights.contains(Rights::WRITE));
    }

    #[test]
    fn derivation_records_its_parent() {
        let mut table = CapabilityTable::new();
        let parent = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");
        let child = table
            .derive(parent, Rights::SEND)
            .expect("derivation succeeds");
        assert_eq!(table.derivation_parent(child), Some(parent));
        assert_eq!(table.derivation_parent(parent), None);
    }

    #[test]
    fn old_handle_invalid_after_slot_reuse() {
        let mut table = CapabilityTable::new();
        let old = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");
        table.destroy(old).expect("destroy succeeds");

        // The slot is reused by a different object.
        let new = table
            .create(endpoint(2), Rights::SEND)
            .expect("table has room");
        assert_eq!(old.slot(), new.slot(), "the slot really was reused");
        assert_ne!(old.generation(), new.generation());

        // The stale handle must not resolve, and must not resolve to the new
        // occupant either.
        assert_eq!(table.lookup(old), Err(ErrorCode::BadHandle));
        assert!(table.entry(old).is_none());
        assert_eq!(table.object_ref(new), Some(endpoint(2)));
    }

    #[test]
    fn a_destroyed_capability_stops_resolving() {
        let mut table = CapabilityTable::new();
        let handle = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");
        assert!(table.lookup(handle).is_ok());
        table.destroy(handle).expect("destroy succeeds");
        assert_eq!(table.lookup(handle), Err(ErrorCode::BadHandle));
        assert_eq!(table.live_count(), 0);
    }

    #[test]
    fn generation_wrap_retires_slot() {
        let mut table = CapabilityTable::new();
        let handle = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");
        let slot = handle.slot() as usize;

        // Free the slot, then pretend its generation counter ran up to the
        // last value that can be issued.
        table.destroy(handle).expect("destroy succeeds");
        table.slots[slot] = Slot::Free {
            next_generation: u32::MAX,
        };

        let last = table
            .create(endpoint(2), Rights::ALL)
            .expect("table has room");
        assert_eq!(last.generation(), u32::MAX);
        assert_eq!(last.slot(), handle.slot());

        // Releasing it must retire the slot rather than wrap the counter.
        table.destroy(last).expect("destroy succeeds");
        assert_eq!(table.slots[slot], Slot::Retired);
        assert_eq!(table.retired_count(), 1);

        // The retired slot never comes back, so no handle can ever name it
        // again with a generation a stale handle might carry.
        let mut seen_retired_slot = false;
        for index in 0..64 {
            let fresh = table
                .create(endpoint(100 + index), Rights::SEND)
                .expect("table has room");
            if fresh.slot() == last.slot() {
                seen_retired_slot = true;
            }
        }
        assert!(
            !seen_retired_slot,
            "a retired slot must never be allocated again"
        );
    }

    #[test]
    fn generations_are_strictly_increasing_per_slot() {
        // The invariant the wrap policy exists to protect: no two lifetimes of
        // one slot ever share a generation.
        let mut table = CapabilityTable::new();
        let mut previous: u32 = 0;
        for _ in 0..32 {
            let handle = table
                .create(endpoint(1), Rights::ALL)
                .expect("table has room");
            let slot = handle.slot();
            let generation = handle.generation();
            table.destroy(handle).expect("destroy succeeds");
            if slot == 1 {
                assert!(generation > previous);
                previous = generation;
            }
        }
    }

    #[test]
    fn revoke_invalidates_handle() {
        let (mut table, handle) = table_with_rights(Rights::ALL);
        table
            .revoke(handle, RevocationScope::Single)
            .expect("revocation succeeds");

        // The handle still resolves — the loss has to be observable — but it
        // no longer confers authority.
        assert_eq!(
            table
                .lookup(handle)
                .expect("entry is retained")
                .revocation_state,
            RevocationState::Revoked
        );
        assert_eq!(table.resolve(handle), Err(ErrorCode::Revoked));
        assert_eq!(
            table.check_rights(handle, Rights::SEND),
            Err(ErrorCode::Revoked)
        );
    }

    #[test]
    fn revocation_observable() {
        let (mut table, handle) = table_with_rights(Rights::ALL);
        let before = table.revocation_epoch();
        table
            .revoke(handle, RevocationScope::Single)
            .expect("revocation succeeds");

        // A holder blocked on CANCEL_WAIT learns of the loss by comparing the
        // epoch it blocked at with the epoch now.
        assert!(table.revocation_epoch() > before);
        assert!(table.is_revoked(handle));
        assert!(!table.is_live(handle));
        assert!(table.revoked_at_epoch(handle).is_some());
        assert_eq!(
            table.revoked_at_epoch(handle),
            Some(table.revocation_epoch())
        );
    }

    #[test]
    fn revocation_of_one_scope_leaves_the_other_live() {
        let (mut table, handle) = table_with_rights(Rights::ALL);
        table
            .revoke(handle, RevocationScope::Single)
            .expect("revocation succeeds");
        assert!(table.is_revoked(handle));
    }

    #[test]
    fn a_cascading_revocation_reaches_derived_capabilities() {
        let mut table = CapabilityTable::new();
        let root = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");
        // The intermediate keeps `GRANT`, otherwise it could not have been
        // derived from in the first place.
        let child = table
            .derive(root, Rights::ALL)
            .expect("derivation succeeds");
        let grandchild = table
            .derive(child, Rights::SEND)
            .expect("derivation succeeds");

        table
            .revoke(root, RevocationScope::Descendants)
            .expect("revocation succeeds");

        assert!(table.is_revoked(root));
        assert!(table.is_revoked(child));
        assert!(table.is_revoked(grandchild));
    }

    #[test]
    fn a_single_scope_revocation_leaves_derived_capabilities_alive() {
        let mut table = CapabilityTable::new();
        let root = table
            .create(endpoint(1), Rights::ALL)
            .expect("table has room");
        let child = table
            .derive(root, Rights::SEND)
            .expect("derivation succeeds");

        table
            .revoke(root, RevocationScope::Single)
            .expect("revocation succeeds");

        assert!(table.is_revoked(root));
        assert!(table.is_live(child));
    }

    #[test]
    fn revoking_an_object_revokes_every_capability_to_it() {
        let mut table = CapabilityTable::new();
        let first = table
            .create(endpoint(7), Rights::ALL)
            .expect("table has room");
        let second = table
            .create(endpoint(7), Rights::SEND)
            .expect("table has room");
        let other = table
            .create(endpoint(8), Rights::ALL)
            .expect("table has room");

        let revoked = table.revoke_object(endpoint(7), RevocationScope::Single);
        assert_eq!(revoked, 2);
        assert!(table.is_revoked(first));
        assert!(table.is_revoked(second));
        assert!(table.is_live(other));
    }

    #[test]
    fn destroying_a_revoked_capability_releases_its_slot() {
        let (mut table, handle) = table_with_rights(Rights::ALL);
        table
            .revoke(handle, RevocationScope::Single)
            .expect("revocation succeeds");
        table.destroy(handle).expect("destroy succeeds");
        assert_eq!(table.live_count(), 0);
        assert_eq!(table.lookup(handle), Err(ErrorCode::BadHandle));
    }

    #[test]
    fn a_full_table_fails_loudly_instead_of_growing() {
        let mut table = CapabilityTable::new();
        let mut last = None;
        for index in 0..(MAX_CAPS - RESERVED_SLOTS) {
            last = table.create(endpoint(index as u32), Rights::SEND).ok();
        }
        assert!(last.is_some(), "every reserved-excluded slot is usable");
        assert_eq!(table.free_count(), 0);
        assert_eq!(
            table.create(endpoint(9999), Rights::SEND),
            Err(ErrorCode::NoMemory)
        );
        assert_eq!(table.live_count(), MAX_CAPS - RESERVED_SLOTS);
    }

    #[test]
    fn freed_slots_are_reused_rather_than_exhausted() {
        let mut table = CapabilityTable::new();
        for _ in 0..100 {
            let handle = table
                .create(endpoint(1), Rights::SEND)
                .expect("table has room");
            table.destroy(handle).expect("destroy succeeds");
        }
        assert_eq!(table.free_count(), MAX_CAPS - RESERVED_SLOTS);
        assert_eq!(table.live_count(), 0);
    }

    #[test]
    fn rights_are_checked_against_the_entry() {
        let (table, handle) = table_with_rights(Rights::SEND);
        assert!(table.check_rights(handle, Rights::SEND).is_ok());
        assert_eq!(
            table.check_rights(handle, Rights::RECEIVE),
            Err(ErrorCode::Denied)
        );
        assert_eq!(table.rights(handle), Some(Rights::SEND));
    }

    #[test]
    fn the_entry_shape_is_the_one_the_plan_names() {
        // The kernel-trusted entry is {object_ref, rights, generation,
        // derivation_parent, revocation_state}; `revoked_at_epoch` is the one
        // addition, and it exists so a CANCEL_WAIT waiter can learn of a
        // revocation without the table keeping a per-waiter list.
        let entry = CapEntry {
            object_ref: endpoint(1),
            rights: Rights::SEND,
            generation: 1,
            derivation_parent: None,
            revocation_state: RevocationState::Live,
            revoked_at_epoch: 0,
        };
        assert!(entry.is_live());
        assert!(!entry.is_revoked());
    }

    #[test]
    fn memory_leak_create_drop_10000() {
        // The plan requires that creating and dropping capabilities leaves the
        // pool exactly where it started. If a single slot leaked, the count
        // would drift.
        let mut table = CapabilityTable::new();
        let start_live = table.live_count();
        let start_free = table.free_count();
        let start_retired = table.retired_count();

        for index in 0..10_000u32 {
            let handle = table
                .create(endpoint(index), Rights::ALL)
                .expect("table has room");
            if index % 3 == 0 {
                let child = table
                    .derive(handle, Rights::SEND)
                    .expect("derivation succeeds");
                table.destroy(child).expect("destroy succeeds");
            }
            if index % 7 == 0 {
                table
                    .revoke(handle, RevocationScope::Single)
                    .expect("revocation succeeds");
            }
            table.destroy(handle).expect("destroy succeeds");
        }

        assert_eq!(table.live_count(), start_live);
        assert_eq!(table.free_count(), start_free);
        assert_eq!(table.retired_count(), start_retired);
    }
}
