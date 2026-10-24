//! The kernel-side capability plumbing.
//!
//! # What the kernel adds to `nova-cap`
//!
//! `nova-cap` holds the *table* and the rules that govern it: narrowing
//! derivation, generational handles, revocation with a scope. It has no idea
//! that a table belongs to a task, that the objects a capability names live in
//! the kernel's object registry, or that a path grant additionally has to be
//! remembered as a scope.
//!
//! This module is that machinery:
//!
//! - [`resolve`] — turn a handle into the object reference and rights it
//!   names, checking both halves;
//! - [`grant_file_scope`] — mint a path-scoped grant, which is a capability
//!   *and* a scope entry;
//! - [`revoke`] — withdraw authority, cascading to descendants when the
//!   underlying authority must not survive in a derived copy.
//!
//! # The object registry and the table are two halves of one lookup
//!
//! A capability names an object. The *table* says what the holder may do with
//! it; the *registry* says what the object is. Resolving a handle therefore
//! takes two steps — look the handle up in the table to get the object
//! reference, then look the reference up in the registry to get the object —
//! and both must succeed. A handle whose object has been destroyed resolves to
//! nothing even though the table still holds the entry, which is what makes
//! `E_BAD_HANDLE` rather than a use-after-free.
//!
//! # Revocation
//!
//! Revocation is the operation whose *scope* matters. Two scopes are
//! supported, and the kernel's choice between them is a policy decision that
//! the code below makes explicit:
//!
//! - withdrawing one holder's copy uses `Single`, because the object keeps
//!   living and other holders keep their authority;
//! - killing an object uses `Descendants`, because a derived copy of a dead
//!   object's authority must not outlive the object.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_cap::{CapabilityTable, ObjectRef, RevocationScope};
use nova_fs::path::{CanonicalPath, canonicalize};
use nova_fs::scope::FileScope;

use crate::services::fs::ScopePool;
use crate::services::object::{ObjectKind, ObjectRegistry};

/// The rights a task's root capability carries.
///
/// The root is the authority to create objects and to grant them to others.
/// It is deliberately narrow: it is the authority `init` delegates *from*, and
/// a root that carried every right would be the "superuser = bypass all"
/// back door the report forbids.
pub const ROOT_RIGHTS: Rights = Rights::SPAWN.union(Rights::GRANT);

/// A resolved capability: what the handle names and what the holder may do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resolved {
    /// The object the capability names.
    pub object_ref: ObjectRef,
    /// What the holder may do with it.
    pub rights: Rights,
    /// The kernel's kind of the object.
    pub kind: ObjectKind,
}

/// Resolve a handle to the object and rights it names.
///
/// The three checks are kept apart because they mean different things:
///
/// - the table lookup fails with `E_BAD_HANDLE` when the handle names no
///   capability — an unknown slot, a stale generation, a revoked entry;
/// - the registry lookup fails with `E_BAD_HANDLE` when the object the
///   capability names has been destroyed — the capability outlived its
///   object, which is a bug in the destructor path, not a policy outcome;
/// - the rights check fails with `E_DENIED` when the capability is fine but
///   the authority is not.
pub fn resolve(
    table: &CapabilityTable,
    objects: &ObjectRegistry,
    handle: CapHandle,
    needed: Rights,
) -> Result<Resolved, ErrorCode> {
    let entry = table.check_rights(handle, needed)?;
    let object_ref = entry.object_ref;
    let rights = entry.rights;
    let kind = *objects.get(object_ref).ok_or(ErrorCode::BadHandle)?;
    Ok(Resolved {
        object_ref,
        rights,
        kind,
    })
}

/// Resolve a handle to the object it names, without asking for any right.
///
/// Used by the operations that act on the *object* rather than on the
/// authority to act on it — the object's own state is checked separately.
pub fn resolve_any(
    table: &CapabilityTable,
    objects: &ObjectRegistry,
    handle: CapHandle,
) -> Result<Resolved, ErrorCode> {
    resolve(table, objects, handle, Rights::EMPTY)
}

/// Mint a capability for an object the kernel just created.
///
/// The kernel's own authority to create the object is the reason the
/// capability exists; the caller decides which rights it carries.
pub fn create(
    table: &mut CapabilityTable,
    objects: &mut ObjectRegistry,
    kind: ObjectKind,
    rights: Rights,
) -> Result<(CapHandle, ObjectRef), ErrorCode> {
    let object_ref = objects.create(kind).ok_or(ErrorCode::NoMemory)?;
    let handle = table.create(object_ref, rights)?;
    Ok((handle, object_ref))
}

/// Mint a path-scoped grant for a task.
///
/// A file grant is two things at once, and both must happen or neither does:
///
/// - a capability in the task's table, which is what the task will pass
///   around; and
/// - a scope entry in the task's scope pool, which is what lets the kernel
///   turn the handle back into a path.
///
/// The object the capability names is created here too, so that the two
/// halves — the capability and the scope — cannot drift apart. If the scope
/// pool is full the capability and the object are undone again, so a failed
/// grant leaves the task with nothing rather than with a handle that names a
/// scope the kernel cannot find.
pub fn grant_file_scope(
    table: &mut CapabilityTable,
    scopes: &mut ScopePool,
    objects: &mut ObjectRegistry,
    path: &[u8],
    rights: Rights,
) -> Result<CapHandle, ErrorCode> {
    let canonical: CanonicalPath = canonicalize(path).map_err(|_| ErrorCode::Invalid)?;
    let object_ref = objects
        .create(ObjectKind::FileScope { path: canonical })
        .ok_or(ErrorCode::NoMemory)?;
    let handle = table.create(object_ref, rights)?;
    let scope = FileScope::new(canonical, rights);
    if !scopes.insert(handle, scope) {
        // The pool is full: undo the capability and the object, so the task is
        // not left holding an authority the kernel cannot enforce.
        let _ = table.destroy(handle);
        objects.destroy(object_ref);
        return Err(ErrorCode::Quota);
    }
    Ok(handle)
}

/// Withdraw authority from a capability.
///
/// `scope` says how far the withdrawal reaches. The kernel uses
/// [`RevocationScope::Descendants`] when the underlying authority must not
/// survive in a derived copy, and [`RevocationScope::Single`] when only one
/// holder's copy is withdrawn.
pub fn revoke(
    table: &mut CapabilityTable,
    handle: CapHandle,
    scope: RevocationScope,
) -> Result<usize, ErrorCode> {
    table.revoke(handle, scope)
}

/// Withdraw every capability naming an object.
///
/// Called when the object is destroyed: every holder of a capability to it
/// must lose that authority, and the table is the only place that set is
/// written down.
pub fn revoke_object(
    table: &mut CapabilityTable,
    objects: &mut ObjectRegistry,
    object_ref: ObjectRef,
) -> usize {
    let revoked = table.revoke_object(object_ref, RevocationScope::Descendants);
    objects.destroy(object_ref);
    revoked
}

/// Withdraw every capability a table holds.
///
/// Used when a task dies: the task's authority must not survive it.
///
/// The revocation is driven by the *objects* rather than by the table's
/// internal slots, because every capability the kernel mints names an object
/// in the registry — that is what makes the table and the registry two halves
/// of one lookup. Revoking each object therefore withdraws every capability,
/// including the ones derived from them, and the table is left empty.
///
/// The count is the number of capabilities whose state actually changed.
pub fn revoke_all(table: &mut CapabilityTable, objects: &ObjectRegistry) -> usize {
    let mut revoked = 0usize;
    for object_ref in objects.objects() {
        revoked += table.revoke_object(object_ref, RevocationScope::Descendants);
    }
    revoked
}
