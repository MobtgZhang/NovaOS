//! Atomic capability transfer.
//!
//! # The rule
//!
//! A message that carries capabilities transfers **all of them or none of
//! them**. There is no state in which a receiver holds some of a batch and
//! the sender still holds the rest: a half-transferred batch is a batch in
//! which the kernel has lost track of who owns what, and capability ownership
//! is exactly the thing this kernel cannot afford to lose track of.
//!
//! # How atomicity is achieved
//!
//! The transfer is staged:
//!
//! 1. **Prepare** every handle. Preparing mints the destination-side handle
//!    but leaves the source untouched, so a failure halfway through has
//!    changed nothing the sender can observe.
//! 2. If any prepare failed, **abort** the ones already prepared. The
//!    sender's capability set is exactly as it was.
//! 3. Otherwise **commit** them all. Committing is the point of no return and
//!    is infallible by contract, so the batch either lands whole or not at
//!    all.
//!
//! # Why the source is destroyed rather than copied
//!
//! A transfer *moves* authority. If the sender kept its handle as well, a
//! capability could be spent twice: the sender could send the same handle to
//! two receivers, each of which would believe it held the only copy of an
//! authority that was meant to be exclusive. `Rights::CONTROL` on an object
//! is exactly the sort of authority that must not be duplicable.

use nova_abi::cap::CapHandle;
use nova_abi::error::ErrorCode;

use nova_cap::{CapabilityTable, ObjectRef};

use crate::message::MAX_CAPS_PER_MSG;

/// The capability-store operations an atomic transfer needs.
///
/// The IPC layer does not know what a capability table is — it knows that
/// authority can be staged, published, and undone. The kernel implements this
/// trait over `nova-cap`'s [`CapabilityTable`]; the trait exists so the
/// atomicity property can be tested against a store that fails on demand.
pub trait CapabilityTransfer {
    /// Stage one transfer, returning the handle that will exist on the
    /// destination side.
    ///
    /// Must not change anything the source can observe. Returning `Err`
    /// means this handle cannot be transferred at all.
    fn prepare(&mut self, source: CapHandle) -> Result<CapHandle, ErrorCode>;

    /// Publish a prepared transfer.
    ///
    /// Infallible by contract: the whole atomicity guarantee rests on this,
    /// because a commit that could fail would leave the batch half-landed.
    fn commit(&mut self, prepared: CapHandle, source: CapHandle);

    /// Undo a prepared transfer.
    fn abort(&mut self, prepared: CapHandle);
}

/// The result of a successful transfer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TransferredCaps {
    handles: [CapHandle; MAX_CAPS_PER_MSG],
    count: usize,
}

impl TransferredCaps {
    /// An empty transfer result.
    pub const fn empty() -> Self {
        Self {
            handles: [CapHandle::INVALID; MAX_CAPS_PER_MSG],
            count: 0,
        }
    }

    /// The destination-side handles, in the order they were staged.
    pub fn handles(&self) -> &[CapHandle] {
        &self.handles[..self.count]
    }

    /// How many capabilities were transferred.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Whether nothing was transferred.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Transfer a batch of capabilities atomically.
///
/// Returns the destination-side handles on success. On failure, **nothing**
/// has moved: every already-prepared handle has been aborted, and the
/// source's capability set is byte-for-byte what it was before the call.
///
/// The batch is rejected outright if it names the same handle twice. A
/// duplicated handle has no single destination, so "all or none" cannot be
/// defined for it.
pub fn transfer_atomic(
    transfer: &mut dyn CapabilityTransfer,
    caps: &[CapHandle],
) -> Result<TransferredCaps, ErrorCode> {
    if caps.len() > MAX_CAPS_PER_MSG {
        return Err(ErrorCode::Invalid);
    }

    // A duplicate has no single destination, so the batch is refused before
    // anything is staged.
    for (index, handle) in caps.iter().enumerate() {
        if caps[index + 1..].contains(handle) {
            return Err(ErrorCode::Invalid);
        }
    }

    let mut result = TransferredCaps::empty();
    let mut sources = [CapHandle::INVALID; MAX_CAPS_PER_MSG];

    for (index, source) in caps.iter().enumerate() {
        match transfer.prepare(*source) {
            Ok(prepared) => {
                sources[index] = *source;
                result.handles[index] = prepared;
                result.count += 1;
            }
            Err(error) => {
                // Undo everything staged so far, in reverse order, so the
                // source is exactly as it was before the call.
                for staged in (0..index).rev() {
                    transfer.abort(result.handles[staged]);
                }
                return Err(error);
            }
        }
    }

    // Everything is staged; publishing cannot fail.
    for (index, source) in sources.iter().enumerate().take(result.count) {
        transfer.commit(result.handles[index], *source);
    }

    Ok(result)
}

/// A transfer between two capability tables.
///
/// The source table loses the handles; the destination table gains new ones
/// with the same object and the same rights. The rights are carried over
/// unchanged: a transfer moves authority, it does not widen or narrow it.
/// Narrowing is [`CapabilityTable::derive`]'s job, and the receiver can
/// always derive a narrower capability from the one it was given.
pub struct TableTransfer<'a> {
    source: &'a mut CapabilityTable,
    destination: &'a mut CapabilityTable,
}

impl<'a> TableTransfer<'a> {
    /// Bind a source and a destination table.
    pub fn new(source: &'a mut CapabilityTable, destination: &'a mut CapabilityTable) -> Self {
        Self {
            source,
            destination,
        }
    }
}

impl CapabilityTransfer for TableTransfer<'_> {
    fn prepare(&mut self, source: CapHandle) -> Result<CapHandle, ErrorCode> {
        // The source must hold a live capability: a revoked or stale handle
        // cannot be given away.
        let entry = self.source.resolve(source)?;
        let object_ref: ObjectRef = entry.object_ref;
        let rights = entry.rights;

        // Mint the destination-side handle now, so a failure here is caught
        // before anything is committed. The source entry is untouched: it is
        // only removed at commit time.
        //
        // The destination's handle must not carry the source's slot number. A
        // handle is only meaningful within the table that issued it, so a
        // receiver holding a numerically identical handle would hold a number
        // that says nothing about which table minted it.
        self.destination
            .create_excluding(object_ref, rights, source)
    }

    fn commit(&mut self, prepared: CapHandle, source: CapHandle) {
        // The destination handle already exists; publishing it means dropping
        // the source's copy so the authority cannot be spent twice.
        let _ = self.source.destroy(source);
        let _ = prepared;
    }

    fn abort(&mut self, prepared: CapHandle) {
        // Undo the staging: the destination-side handle never existed as far
        // as anyone else is concerned.
        let _ = self.destination.destroy(prepared);
    }
}

/// Transfer a batch of capabilities between two tables.
///
/// A convenience over [`transfer_atomic`] for the common case, where the
/// kernel has the sender's and the receiver's tables to hand.
pub fn transfer_between_tables(
    source: &mut CapabilityTable,
    destination: &mut CapabilityTable,
    caps: &[CapHandle],
) -> Result<TransferredCaps, ErrorCode> {
    let mut transfer = TableTransfer::new(source, destination);
    transfer_atomic(&mut transfer, caps)
}
