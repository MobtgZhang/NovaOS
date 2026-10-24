//! Scoped file capabilities: a grant on a path, and the check against a request.
//!
//! # The rule
//!
//! A capability to a path grants access to that path **and to its descendants**.
//! It does not grant access to the path's parent, and it does not grant access
//! to its siblings. A capability to a *file* grants access to that file alone —
//! a file has no descendants, and granting its parent directory would be the
//! opposite of least privilege.
//!
//! The check is [`PathScope::allows`], and it is deliberately a free function
//! over two canonical paths: no capability table, no kernel state, no
//! allocator. That is what makes it testable on the host, and it is why this
//! crate does not depend on `nova-cap` — the *handle* is the kernel's business,
//! the *scope rule* is the filesystem's.
//!
//! # Why containment is component-wise
//!
//! The check compares components, not bytes. A byte-wise prefix test would
//! consider `/docsecret` to be inside `/docs`, because `"docsecret"` starts
//! with `"docs"`. Component-wise comparison means a grant on `/docs` covers
//! `/docs/notes.txt` and `/docs/sub/deep.txt`, and covers nothing else.
//!
//! # The root is special
//!
//! A grant on `/` is a grant on the whole filesystem. It is expressed as the
//! root scope, which contains every other path — including paths that are not
//! "below" the root in any useful sense.

use nova_abi::cap::Rights;

use crate::path::CanonicalPath;

/// The rights a file scope can confer.
///
/// A scope names *where*; the rights name *what*. Keeping them separate means
/// the same scope can be reused for a read-only and a read-write grant, and
/// the intersection rule ([`Rights::intersect`]) still applies when the kernel
/// derives a child capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileScope {
    /// The granted path, canonical.
    path: CanonicalPath,
    /// The rights the grant carries.
    rights: Rights,
}

impl FileScope {
    /// A scope over `path` carrying `rights`.
    pub const fn new(path: CanonicalPath, rights: Rights) -> Self {
        FileScope { path, rights }
    }

    /// The granted path.
    pub const fn path(&self) -> &CanonicalPath {
        &self.path
    }

    /// The rights this scope confers.
    pub const fn rights(&self) -> Rights {
        self.rights
    }

    /// Whether this scope permits `requested` at all, ignoring rights.
    ///
    /// This is the pure containment question: "does the granted subtree
    /// contain this path?" The rights question is asked separately, so that a
    /// denial can be attributed to the right failure rather than to the path.
    pub fn contains(&self, requested: &CanonicalPath) -> bool {
        contains(&self.path, requested)
    }

    /// Whether this scope permits the request, in both path and rights.
    ///
    /// The rights check is `scope.rights ∩ requested ⊆ scope.rights`, which is
    /// always true, so it reduces to "does the scope hold the right". The
    /// intersection is written out anyway so that the shape matches the
    /// kernel's derivation rule and does not accidentally diverge from it.
    pub fn allows(&self, requested: &CanonicalPath, required: Rights) -> bool {
        if !self.contains(requested) {
            return false;
        }
        let effective = self.rights.intersect(required);
        effective == required
    }

    /// Whether the scope denies `requested`.
    ///
    /// The negation of [`FileScope::allows`], spelled out so that call sites
    /// read as a decision rather than as a double negative.
    pub fn denies(&self, requested: &CanonicalPath, required: Rights) -> bool {
        !self.allows(requested, required)
    }
}

/// Whether `requested` lies inside the subtree rooted at `scope`.
///
/// The scope path itself is inside its own scope: a grant on `/docs` covers
/// `/docs`, because opening the granted directory is exactly what the grant
/// is for.
///
/// The root contains everything, including the root itself.
pub fn contains(scope: &CanonicalPath, requested: &CanonicalPath) -> bool {
    if scope.is_root() {
        return true;
    }
    requested.starts_with(scope)
}

/// A set of scopes, checked together.
///
/// A caller may hold several grants — a read-only grant on `/etc` and a
/// read-write grant on `/home`. The filesystem consults the set and allows a
/// request if *any* member permits it. The set is a fixed-size array: the
/// filesystem has no allocator, and the number of simultaneously live grants
/// is a kernel resource decision, not a policy one.
#[derive(Clone, Copy, Debug)]
pub struct ScopeSet<const N: usize> {
    scopes: [Option<FileScope>; N],
    len: usize,
}

impl<const N: usize> Default for ScopeSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ScopeSet<N> {
    /// An empty set, which grants nothing.
    pub const fn new() -> Self {
        ScopeSet {
            scopes: [None; N],
            len: 0,
        }
    }

    /// The number of live scopes.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no scope is held.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add a scope.
    ///
    /// Returns `false` if the set is full or the scope is a duplicate. A full
    /// set is a refusal, not an eviction: silently dropping an older grant
    /// would change the meaning of the caller's authority without telling it.
    pub fn insert(&mut self, scope: FileScope) -> bool {
        if self.len >= N {
            return false;
        }
        if self.scopes[..self.len]
            .iter()
            .flatten()
            .any(|existing| *existing == scope)
        {
            return false;
        }
        self.scopes[self.len] = Some(scope);
        self.len = self.len.saturating_add(1);
        true
    }

    /// Remove a scope, by exact path.
    pub fn remove_path(&mut self, path: &CanonicalPath) -> bool {
        for index in 0..self.len {
            if let Some(scope) = self.scopes[index]
                && scope.path() == path
            {
                let last = self.len.saturating_sub(1);
                self.scopes[index] = self.scopes[last];
                self.scopes[last] = None;
                self.len = last;
                return true;
            }
        }
        false
    }

    /// The scopes currently held, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &FileScope> {
        self.scopes[..self.len].iter().flatten()
    }

    /// Whether any held scope permits the request.
    pub fn allows(&self, requested: &CanonicalPath, required: Rights) -> bool {
        self.iter().any(|scope| scope.allows(requested, required))
    }

    /// Whether the request is denied by every held scope.
    pub fn denies(&self, requested: &CanonicalPath, required: Rights) -> bool {
        !self.allows(requested, required)
    }
}

#[cfg(test)]
mod tests {
    use nova_abi::cap::Rights;

    use super::{FileScope, ScopeSet, contains};
    use crate::path::canonicalize;

    fn path(text: &str) -> crate::path::CanonicalPath {
        canonicalize(text.as_bytes()).expect("test path canonicalises")
    }

    fn scope(text: &str, rights: Rights) -> FileScope {
        FileScope::new(path(text), rights)
    }

    #[test]
    fn file_cap_scope_allows_descendant() {
        let granted = scope("/docs", Rights::READ | Rights::WRITE);

        assert!(
            granted.allows(&path("/docs/notes.txt"), Rights::READ),
            "a grant on a directory covers its files"
        );
        assert!(
            granted.allows(&path("/docs/sub/deep.txt"), Rights::READ),
            "a grant on a directory covers arbitrarily deep descendants"
        );
        assert!(
            granted.allows(&path("/docs"), Rights::READ),
            "a grant covers the directory it names"
        );
        assert!(
            granted.allows(&path("/docs/notes.txt"), Rights::READ | Rights::WRITE),
            "the scope holds both rights"
        );
    }

    #[test]
    fn file_cap_scope_denies_sibling() {
        let granted = scope("/docs", Rights::READ | Rights::WRITE);

        assert!(
            granted.denies(&path("/etc/passwd"), Rights::READ),
            "a sibling directory is outside the grant"
        );
        assert!(
            granted.denies(&path("/docs2/notes.txt"), Rights::READ),
            "a name that merely shares a byte prefix is outside the grant"
        );
        assert!(
            granted.denies(&path("/"), Rights::READ),
            "the parent of the grant is outside the grant"
        );
        assert!(
            granted.denies(&path("/docs/../etc/passwd"), Rights::READ),
            "`..` must not widen the grant; canonicalisation removes it first"
        );
    }

    #[test]
    fn file_read_without_cap_denied() {
        // The acceptance criterion: a caller with no scope at all is refused.
        let grants = ScopeSet::<4>::new();
        assert!(grants.is_empty());
        assert!(
            grants.denies(&path("/docs/notes.txt"), Rights::READ),
            "with no grant, every read is denied"
        );

        // A grant on an unrelated path does not help either.
        let mut grants = ScopeSet::<4>::new();
        assert!(grants.insert(scope("/etc", Rights::READ)));
        assert!(grants.denies(&path("/docs/notes.txt"), Rights::READ));
        assert!(grants.allows(&path("/etc/passwd"), Rights::READ));
    }

    #[test]
    fn a_grant_on_a_file_covers_only_that_file() {
        // A file has no descendants, so the grant is exactly the file. In
        // particular it must not cover the directory the file lives in.
        let granted = scope("/docs/notes.txt", Rights::READ);
        assert!(granted.allows(&path("/docs/notes.txt"), Rights::READ));
        assert!(granted.denies(&path("/docs/other.txt"), Rights::READ));
        assert!(granted.denies(&path("/docs"), Rights::READ));
        assert!(granted.denies(&path("/"), Rights::READ));
    }

    #[test]
    fn rights_are_checked_separately_from_the_path() {
        let granted = scope("/docs", Rights::READ);

        assert!(granted.allows(&path("/docs/notes.txt"), Rights::READ));
        assert!(
            granted.denies(&path("/docs/notes.txt"), Rights::WRITE),
            "the scope holds READ but not WRITE"
        );
        assert!(
            granted.denies(&path("/docs/notes.txt"), Rights::READ | Rights::WRITE),
            "a partial rights set is a denial"
        );
        assert!(
            granted.allows(&path("/docs/notes.txt"), Rights::EMPTY),
            "requiring no rights is trivially permitted"
        );
    }

    #[test]
    fn a_root_grant_covers_the_whole_filesystem() {
        let root = scope("/", Rights::READ);
        assert!(root.allows(&path("/etc/passwd"), Rights::READ));
        assert!(root.allows(&path("/"), Rights::READ));
        assert!(root.denies(&path("/etc/passwd"), Rights::WRITE));
    }

    #[test]
    fn containment_is_pure_and_symmetric_in_its_own_way() {
        // The function is the same one the scope uses, so it is worth testing
        // directly: it has no state and cannot drift.
        assert!(contains(&path("/docs"), &path("/docs")));
        assert!(contains(&path("/docs"), &path("/docs/a/b")));
        assert!(!contains(&path("/docs"), &path("/docsecret")));
        assert!(!contains(&path("/docs/a"), &path("/docs")));
        assert!(contains(&path("/"), &path("/anything/at/all")));
    }

    #[test]
    fn a_scope_set_permits_if_any_member_does() {
        let mut grants = ScopeSet::<4>::new();
        assert!(grants.insert(scope("/etc", Rights::READ)));
        assert!(grants.insert(scope("/home", Rights::READ | Rights::WRITE)));

        assert!(grants.allows(&path("/etc/passwd"), Rights::READ));
        assert!(grants.allows(&path("/home/user/notes"), Rights::WRITE));
        assert!(grants.denies(&path("/etc/passwd"), Rights::WRITE));
        assert!(grants.denies(&path("/var/log"), Rights::READ));
        assert_eq!(grants.len(), 2);
    }

    #[test]
    fn a_scope_set_is_bounded_and_refuses_to_overflow() {
        let mut grants = ScopeSet::<2>::new();
        assert!(grants.insert(scope("/a", Rights::READ)));
        assert!(grants.insert(scope("/b", Rights::READ)));
        assert!(
            !grants.insert(scope("/c", Rights::READ)),
            "the third grant must be refused, not silently replace an earlier one"
        );
        assert_eq!(grants.len(), 2);
        assert!(grants.allows(&path("/a"), Rights::READ));
        assert!(grants.allows(&path("/b"), Rights::READ));
        assert!(grants.denies(&path("/c"), Rights::READ));
    }

    #[test]
    fn removing_a_scope_revokes_it() {
        let mut grants = ScopeSet::<4>::new();
        assert!(grants.insert(scope("/docs", Rights::READ)));
        assert!(grants.remove_path(&path("/docs")));
        assert!(grants.is_empty());
        assert!(grants.denies(&path("/docs/notes.txt"), Rights::READ));
        assert!(!grants.remove_path(&path("/docs")));
    }
}
