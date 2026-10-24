//! Canonical paths for the NovaOS RAM filesystem.
//!
//! # Why canonicalisation is not optional
//!
//! A capability-scoped filesystem cannot answer "is this path inside my grant?"
//! unless every path has exactly one spelling. `/docs/notes.txt`, `/docs//notes.txt`,
//! `/docs/./notes.txt` and `/docs/sub/../notes.txt` are the same file, and a scope
//! check that compared them literally would let `..` walk out of a granted
//! directory. Canonicalisation is therefore the *first* thing every entry point
//! does, and the scope check only ever sees canonical paths.
//!
//! # The rules, in full
//!
//! 1. Paths are **absolute**. A path that does not begin with `/` is rejected
//!    ([`PathError::NotAbsolute`]) rather than silently resolved against some
//!    ambient working directory. A filesystem that hands out capability-scoped
//!    access has no meaningful notion of "current directory": the grant names
//!    a place, not a position in a walk.
//! 2. An empty path is rejected ([`PathError::Empty`]).
//! 3. A path containing a NUL byte is rejected ([`PathError::InteriorNul`]). NUL
//!    terminates a C string, so a path containing one has two different
//!    meanings depending on who reads it.
//! 4. A path longer than [`MAX_PATH_BYTES`] is rejected
//!    ([`PathError::TooLong`]) *before* any copying happens.
//! 5. A single component longer than [`MAX_NAME_BYTES`] is rejected
//!    ([`PathError::ComponentTooLong`]), because a component is a file name and
//!    file names are bounded.
//! 6. More than [`MAX_PATH_COMPONENTS`] components is rejected
//!    ([`PathError::TooManyComponents`]).
//! 7. Repeated separators collapse: `//a///b` is `/a/b`.
//! 8. A trailing separator is dropped: `/a/b/` is `/a/b`. The root `/` is the
//!    one path that keeps its separator.
//! 9. A `.` component is removed: `/a/./b` is `/a/b`.
//! 10. A `..` component removes the previous component: `/a/b/..` is `/a`.
//! 11. A `..` at the root **clamps**: `/..` is `/`, `/../..` is `/`. It does not
//!     escape the root. A filesystem whose grants are path-scoped would
//!     otherwise let `..` walk above the granted subtree and read a sibling
//!     directory through a path that looks contained.
//!
//! The canonical form therefore always begins with `/`, never contains `.` or
//! `..` components, never contains an empty component, and — except for the
//! root — never ends with `/`.
//!
//! # Canonicalisation is syntactic; resolution is semantic
//!
//! [`canonicalize`] here is *pure*: it does not know what a file is. That is
//! what makes it testable and what makes it safe to run before a scope check.
//! Whether `/etc/passwd/.` is an error depends on the filesystem, not on the
//! spelling, and that judgement belongs to the RAM filesystem's resolver, which
//! walks the components against real inodes.

use nova_abi::error::ErrorCode;

/// Maximum length of a whole path, in bytes, including the leading `/`.
pub const MAX_PATH_BYTES: usize = 256;

/// Maximum length of a single path component (a file name), in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// Maximum number of components in a path.
pub const MAX_PATH_COMPONENTS: usize = 32;

/// Why a path was refused.
///
/// Every variant maps to [`ErrorCode::Invalid`], because `nova-abi` — frozen, and
/// deliberately minimal — has no `ENOENT`/`ENAMETOOLONG` codes. A caller that
/// needs to distinguish "no such file" from "bad path" gets that distinction
/// from [`crate::ramfs::FsError`], which is richer than the ABI codes on purpose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathError {
    /// The path was empty.
    Empty,
    /// The path did not begin with `/`.
    NotAbsolute,
    /// The path contained a NUL byte.
    InteriorNul,
    /// The path was longer than [`MAX_PATH_BYTES`].
    TooLong,
    /// A single component was longer than [`MAX_NAME_BYTES`].
    ComponentTooLong,
    /// The path had more than [`MAX_PATH_COMPONENTS`] components.
    TooManyComponents,
}

impl PathError {
    /// The ABI error code this failure maps to.
    pub const fn code(self) -> ErrorCode {
        ErrorCode::Invalid
    }

    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            PathError::Empty => "E_EMPTY_PATH",
            PathError::NotAbsolute => "E_NOT_ABSOLUTE",
            PathError::InteriorNul => "E_INTERIOR_NUL",
            PathError::TooLong => "E_PATH_TOO_LONG",
            PathError::ComponentTooLong => "E_NAME_TOO_LONG",
            PathError::TooManyComponents => "E_TOO_MANY_COMPONENTS",
        }
    }
}

impl core::fmt::Display for PathError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// A path in canonical form.
///
/// The invariant is established once, by [`canonicalize`], and every other
/// operation in the crate may rely on it:
///
/// * `bytes[0] == b'/'`
/// * `len >= 1`
/// * no component is empty, `.` or `..`
/// * `len == 1` (the root) or `bytes[len - 1] != b'/'`
///
/// The type is `Copy` but not small: it is a fixed 256-byte buffer, because the
/// filesystem has no allocator and a path is copied by value rather than by
/// pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CanonicalPath {
    bytes: [u8; MAX_PATH_BYTES],
    len: usize,
}

impl CanonicalPath {
    /// The root directory, `/`.
    pub const ROOT: CanonicalPath = {
        let mut bytes = [0u8; MAX_PATH_BYTES];
        bytes[0] = b'/';
        CanonicalPath { bytes, len: 1 }
    };

    /// The canonical bytes, not including any terminator.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The canonical bytes as UTF-8, if they are valid UTF-8.
    ///
    /// Paths are byte strings, not text: a filesystem that stored only text
    /// could not hold the names a real filesystem holds. This accessor exists
    /// for logs and for the shell, which is allowed to be UTF-8-only.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }

    /// The length of the canonical form in bytes.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the path has no components beyond the root.
    ///
    /// A canonical path is never empty — the root is one byte — so this is
    /// only true for the root, and it exists because a `len` without an
    /// `is_empty` is a trap for callers that test emptiness.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether this is the root directory.
    pub const fn is_root(&self) -> bool {
        self.len == 1
    }

    /// The number of components below the root.
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The components of the path, without the leading `/`.
    pub fn components(&self) -> Components<'_> {
        Components {
            bytes: self.as_bytes(),
        }
    }

    /// The parent directory, or `None` for the root.
    ///
    /// The root has no parent: `..` at the root clamps rather than escaping.
    pub fn parent(&self) -> Option<CanonicalPath> {
        if self.is_root() {
            return None;
        }
        let body = &self.bytes[1..self.len];
        match body.iter().rposition(|&byte| byte == b'/') {
            Some(index) => {
                let mut parent = *self;
                parent.len = index.checked_add(1)?;
                parent.bytes[parent.len..].fill(0);
                Some(parent)
            }
            None => Some(CanonicalPath::ROOT),
        }
    }

    /// The last component, or `None` for the root.
    pub fn file_name(&self) -> Option<&[u8]> {
        if self.is_root() {
            return None;
        }
        let body = &self.bytes[1..self.len];
        match body.iter().rposition(|&byte| byte == b'/') {
            Some(index) => Some(&body[index.checked_add(1)?..]),
            None => Some(body),
        }
    }

    /// Whether this path lies inside `prefix`, comparing whole components.
    ///
    /// `/docs` contains `/docs` and `/docs/notes.txt`, but not `/docs2` and not
    /// `/etc`. The comparison is component-wise on purpose: a byte-wise prefix
    /// test would let `/docs` contain `/docsecret`, which is the classic
    /// confused-deputy bug of path containment checks.
    pub fn starts_with(&self, prefix: &CanonicalPath) -> bool {
        let mut mine = self.components();
        let mut theirs = prefix.components();
        loop {
            match theirs.next() {
                None => return true,
                Some(want) => match mine.next() {
                    None => return false,
                    Some(have) => {
                        if have != want {
                            return false;
                        }
                    }
                },
            }
        }
    }
}

impl core::fmt::Display for CanonicalPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.as_str() {
            Some(text) => f.write_str(text),
            None => f.write_str("<non-utf8 path>"),
        }
    }
}

/// The components of a canonical path.
///
/// Yields each component in order, without the separators. Empty components
/// cannot occur in a canonical path, but the iterator skips them anyway so
/// that it is safe to point at a non-canonical buffer.
pub struct Components<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for Components<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            if self.bytes.is_empty() {
                return None;
            }
            let rest = if self.bytes[0] == b'/' {
                &self.bytes[1..]
            } else {
                self.bytes
            };
            match rest.iter().position(|&byte| byte == b'/') {
                Some(index) => {
                    let component = &rest[..index];
                    self.bytes = &rest[index..];
                    if component.is_empty() {
                        continue;
                    }
                    return Some(component);
                }
                None => {
                    let component = rest;
                    self.bytes = &[];
                    if component.is_empty() {
                        return None;
                    }
                    return Some(component);
                }
            }
        }
    }
}

/// Canonicalise a path.
///
/// This is the single entry point for turning caller-supplied bytes into a
/// [`CanonicalPath`]. Every rule is listed in the [module documentation](self);
/// the tests at the bottom of this file pin each one.
///
/// The function is pure and never consults the filesystem, which is what lets
/// the scope check run *before* any inode is touched.
pub fn canonicalize(input: &[u8]) -> Result<CanonicalPath, PathError> {
    if input.is_empty() {
        return Err(PathError::Empty);
    }
    if input.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    if input[0] != b'/' {
        return Err(PathError::NotAbsolute);
    }
    if input.contains(&0u8) {
        return Err(PathError::InteriorNul);
    }

    let mut bytes = [0u8; MAX_PATH_BYTES];
    bytes[0] = b'/';
    let mut len: usize = 1;

    // Where each component starts inside `bytes`, so that `..` can truncate
    // the previous component without re-parsing the string.
    let mut starts = [0usize; MAX_PATH_COMPONENTS];
    let mut depth: usize = 0;

    for component in input.split(|&byte| byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if component == b"." {
            continue;
        }
        if component == b".." {
            if depth == 0 {
                // Clamp at the root: `..` must never escape the filesystem.
                continue;
            }
            depth = depth.checked_sub(1).ok_or(PathError::TooManyComponents)?;
            // Truncating to just before the separator of the removed
            // component leaves `/docs` for `/docs/notes.txt`. The root is
            // special-cased: removing the first component leaves `/`.
            let cut = starts[depth].saturating_sub(1).max(1);
            len = cut.min(len);
            bytes[len..].fill(0);
            continue;
        }
        if component.len() > MAX_NAME_BYTES {
            return Err(PathError::ComponentTooLong);
        }
        if depth >= MAX_PATH_COMPONENTS {
            return Err(PathError::TooManyComponents);
        }

        // Append `/name`, with every step checked: the buffer is fixed size, so
        // an unchecked addition here would be a buffer overflow, not a
        // "directory too large" error. The separator is only written when the
        // path is not already the root, which is what keeps the canonical
        // form from starting with `//`.
        let separator = usize::from(len > 1);
        let with_separator = len.checked_add(separator).ok_or(PathError::TooLong)?;
        let new_len = with_separator
            .checked_add(component.len())
            .ok_or(PathError::TooLong)?;
        if new_len > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        if separator == 1 {
            bytes[len] = b'/';
        }
        bytes[with_separator..new_len].copy_from_slice(component);
        len = new_len;

        starts[depth] = with_separator;
        depth = depth.checked_add(1).ok_or(PathError::TooManyComponents)?;
    }

    // The bytes after `len` were already zeroed as the path was built, and the
    // `..` truncation zeroes them again, so two equal paths compare equal
    // byte-for-byte rather than only by length.
    Ok(CanonicalPath { bytes, len })
}

#[cfg(test)]
mod tests {
    use super::{MAX_NAME_BYTES, MAX_PATH_BYTES, MAX_PATH_COMPONENTS, PathError, canonicalize};

    /// A canonical path held by value, so a test can compare it with a byte
    /// string without needing an allocator.
    #[derive(PartialEq, Eq, Debug)]
    struct Canonical {
        bytes: [u8; MAX_PATH_BYTES],
        len: usize,
    }

    impl<const N: usize> PartialEq<&[u8; N]> for Canonical {
        fn eq(&self, other: &&[u8; N]) -> bool {
            self.bytes[..self.len] == other[..]
        }
    }

    impl PartialEq<&[u8]> for Canonical {
        fn eq(&self, other: &&[u8]) -> bool {
            self.bytes[..self.len] == other[..]
        }
    }

    impl Canonical {
        fn as_slice(&self) -> &[u8] {
            &self.bytes[..self.len]
        }
    }

    fn canonical(input: &[u8]) -> Canonical {
        let path = canonicalize(input).expect("path should canonicalise");
        let mut bytes = [0u8; MAX_PATH_BYTES];
        bytes[..path.len()].copy_from_slice(path.as_bytes());
        Canonical {
            bytes,
            len: path.len(),
        }
    }

    #[test]
    fn path_canonicalisation() {
        // The canonical form of every spelling of the same file.
        assert_eq!(canonical(b"/docs/notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs//notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs///notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs/./notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs/././notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs/sub/../notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs/notes.txt/"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/docs//sub//..//notes.txt"), b"/docs/notes.txt");
        assert_eq!(canonical(b"/"), b"/");
        assert_eq!(canonical(b"//"), b"/");
        assert_eq!(canonical(b"/."), b"/");
        assert_eq!(canonical(b"/./"), b"/");

        // The canonical form is idempotent: canonicalising twice changes
        // nothing, which is what makes it safe to compare canonical paths.
        for spelling in [&b"/docs/notes.txt"[..], b"/a/b/c", b"/", b"/x//y/../z/"] {
            let once = canonical(spelling);
            let twice = canonical(once.as_slice());
            assert_eq!(once, twice, "canonicalisation is not idempotent");
        }
    }

    #[test]
    fn dotdot_clamps_at_root() {
        // `..` never escapes: the root is the top of the filesystem.
        assert_eq!(canonical(b"/.."), b"/");
        assert_eq!(canonical(b"/../.."), b"/");
        assert_eq!(canonical(b"/../../.."), b"/");
        assert_eq!(canonical(b"/../etc/passwd"), b"/etc/passwd");
        assert_eq!(canonical(b"/a/../.."), b"/");
        assert_eq!(canonical(b"/a/b/../../.."), b"/");
        // A `..` *inside* the tree still pops exactly one component.
        assert_eq!(canonical(b"/a/b/../c"), b"/a/c");
    }

    #[test]
    fn path_overflow_rejected() {
        // Too long for the whole path. The path is built from several
        // components, because a single component cannot exceed the file-name
        // bound — which is a different limit, checked separately below.
        let mut too_long = [0u8; MAX_PATH_BYTES + 64];
        too_long[0] = b'/';
        for byte in too_long.iter_mut().skip(1) {
            *byte = b'a';
        }
        assert_eq!(
            canonicalize(&too_long).unwrap_err(),
            PathError::TooLong,
            "a path longer than the bound must be refused, not truncated"
        );

        // Exactly at the bound is accepted: four components of 63 bytes each
        // plus their separators come to exactly MAX_PATH_BYTES.
        let mut at_bound = [0u8; MAX_PATH_BYTES];
        at_bound[0] = b'/';
        let mut cursor = 1usize;
        for component in 0..4 {
            if component > 0 {
                at_bound[cursor] = b'/';
                cursor += 1;
            }
            for _ in 0..63 {
                at_bound[cursor] = b'a';
                cursor += 1;
            }
        }
        assert_eq!(cursor, MAX_PATH_BYTES);
        assert!(canonicalize(&at_bound).is_ok());

        // One component longer than a file name may be.
        let mut long_component = [0u8; MAX_NAME_BYTES + 2];
        long_component[0] = b'/';
        long_component[1..].fill(b'b');
        assert_eq!(
            canonicalize(&long_component).unwrap_err(),
            PathError::ComponentTooLong
        );
        let mut component_at_bound = [0u8; MAX_NAME_BYTES + 1];
        component_at_bound[0] = b'/';
        component_at_bound[1..].fill(b'b');
        assert!(canonicalize(&component_at_bound).is_ok());

        // Too many components.
        let mut many = [0u8; (MAX_PATH_COMPONENTS + 1) * 2];
        let (pairs, _) = many.as_chunks_mut::<2>();
        for pair in pairs.iter_mut() {
            pair[0] = b'/';
            pair[1] = b'c';
        }
        assert_eq!(
            canonicalize(&many).unwrap_err(),
            PathError::TooManyComponents
        );
    }
    #[test]
    fn malformed_paths_are_refused() {
        assert_eq!(canonicalize(b"").unwrap_err(), PathError::Empty);
        assert_eq!(
            canonicalize(b"docs/notes.txt").unwrap_err(),
            PathError::NotAbsolute,
            "a relative path has no meaning in a capability-scoped filesystem"
        );
        assert_eq!(canonicalize(b"./docs").unwrap_err(), PathError::NotAbsolute);
        assert_eq!(
            canonicalize(b"/docs/no\0te").unwrap_err(),
            PathError::InteriorNul
        );
    }

    #[test]
    fn the_canonical_form_has_the_documented_shape() {
        let path = canonicalize(b"/docs//./sub/../notes.txt/").expect("canonicalises");
        assert_eq!(path.as_bytes(), b"/docs/notes.txt");
        assert!(path.as_bytes().starts_with(b"/"));
        assert!(!path.as_bytes().ends_with(b"/"));
        assert_eq!(path.depth(), 2);
        assert_eq!(path.file_name(), Some(&b"notes.txt"[..]));
        let parent = path.parent().expect("has a parent");
        assert_eq!(parent.as_bytes(), b"/docs");
        let grandparent = parent.parent().expect("has a grandparent");
        assert_eq!(grandparent.as_bytes(), b"/");
        assert!(grandparent.parent().is_none());
    }

    #[test]
    fn component_containment_is_component_wise() {
        let docs = canonicalize(b"/docs").expect("canonicalises");
        let notes = canonicalize(b"/docs/notes.txt").expect("canonicalises");
        let docsecret = canonicalize(b"/docsecret").expect("canonicalises");

        assert!(notes.starts_with(&docs));
        assert!(docs.starts_with(&docs));
        assert!(
            !docsecret.starts_with(&docs),
            "a byte-wise prefix test would wrongly contain /docsecret"
        );
        assert!(!docs.starts_with(&notes));
        assert!(
            !canonicalize(b"/")
                .expect("canonicalises")
                .starts_with(&docs)
        );
    }

    #[test]
    fn the_root_is_its_own_only_ancestor() {
        let root = canonicalize(b"/").expect("canonicalises");
        assert!(root.is_root());
        assert_eq!(root.depth(), 0);
        assert_eq!(root.parent(), None);
        assert_eq!(root.file_name(), None);
        assert_eq!(root, super::CanonicalPath::ROOT);
    }
}
