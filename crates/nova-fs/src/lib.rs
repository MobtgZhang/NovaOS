//! NovaOS RAM filesystem.
//!
//! The bounded in-memory filesystem the FileServer serves. This crate holds
//! the path arithmetic and the file store; the service that exposes it is in
//! the kernel.
//!
//! # What lives here, and what does not
//!
//! - [`path`] canonicalises caller-supplied bytes into one canonical spelling
//!   per file. It is pure and never touches the filesystem.
//! - [`scope`] decides whether a granted path covers a requested path. It is
//!   a free function over two canonical paths, so it can be tested without a
//!   capability table.
//! - [`ramfs`] is the bounded store: a fixed-size inode pool, a fixed-size
//!   open-file table, and handles instead of repeated path resolution.
//!
//! What is deliberately *not* here: the capability *handle*. The filesystem
//! speaks in [`nova_abi::cap::Rights`]; the kernel owns the table that turns a
//! handle into a scope, and owns the decision of which scopes a caller holds.
//! Keeping that boundary is what lets this crate's rules be tested on the
//! host without a kernel.
//!
//! # Invariants
//!
//! 1. Every path that reaches the filesystem is canonical. The filesystem
//!    canonicalises internally on every public path-taking method, so
//!    "resolve `.` and `..` first" is a property of the filesystem rather than
//!    a discipline of its callers.
//! 2. Every length and offset is computed with checked arithmetic. There is
//!    no `wrapping_*` anywhere in the crate: an overflow in a size
//!    computation is a memory-safety bug, not a "file too large" condition.
//! 3. No `unsafe`, no allocator, no `Vec`, no `String`. Every structure is a
//!    fixed-size array with a compile-time bound.
//!
//! Rules 2 and 3 are not merely documented: the [source-discipline
//! tests](crate::tests) read this crate's own source and fail if a forbidden
//! construct appears in it.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod path;
pub mod ramfs;
pub mod scope;

pub use path::{CanonicalPath, MAX_NAME_BYTES, MAX_PATH_BYTES, MAX_PATH_COMPONENTS, PathError};
pub use ramfs::{
    DirEntry, FileHandle, FileKind, FsError, MAX_CHILDREN, MAX_FILE_BYTES, MAX_INODES,
    MAX_OPEN_FILES, MAX_TOTAL_BYTES, Metadata, RamFs,
};
pub use scope::{FileScope, ScopeSet};

#[cfg(test)]
mod tests {
    /// The crate's own source, read at test time.
    ///
    /// The rules this crate lives by — no `unsafe`, no `wrapping_*`, no
    /// `Vec`/`String`/`Box` — are structural rules about the source, and a
    /// structural rule is best enforced by a test that reads the source. A
    /// comment or a lint configuration cannot be forgotten, but a function
    /// body can.
    const SOURCES: [&str; 3] = [
        include_str!("path.rs"),
        include_str!("ramfs.rs"),
        include_str!("scope.rs"),
    ];

    /// Whether any *code* line of `source` contains `token`.
    ///
    /// Comments are stripped first, so the documentation may *mention* a
    /// forbidden construct — the module docs explain why `wrapping_add` is
    /// banned — without the test failing on the explanation. The test module
    /// at the end of each file is skipped for the same reason: it names the
    /// tokens in order to look for them.
    ///
    /// The scan is line-based rather than token-based, which is enough here: a
    /// forbidden construct in this crate is always written on one line.
    fn code_contains(source: &str, token: &str) -> bool {
        let mut in_test_module = false;
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed == "#[cfg(test)]" {
                in_test_module = true;
                continue;
            }
            if in_test_module {
                continue;
            }
            // Strip the comment part of the line. A `//` inside a string
            // literal would confuse this, but the crate has none.
            let code = line.split("//").next().unwrap_or("");
            if code.contains(token) {
                return true;
            }
        }
        false
    }

    /// Whether any of the crate's files contains `token` in code.
    fn crate_code_contains(token: &str) -> bool {
        SOURCES.iter().any(|source| code_contains(source, token))
            || code_contains(include_str!("lib.rs"), token)
    }

    #[test]
    fn the_crate_contains_no_unsafe_block() {
        for token in ["unsafe fn", "unsafe {", "unsafe impl", "unsafe trait"] {
            assert!(
                !crate_code_contains(token),
                "the filesystem must stay free of unsafe: it is a bounded pool of plain data"
            );
        }
    }

    #[test]
    fn the_crate_never_papers_over_an_overflow() {
        // `wrapping_add` on a length is how a filesystem hands out memory that
        // does not exist. The rule is that every length computation is
        // checked, so the wrapping forms must not appear at all.
        for token in [
            "wrapping_add",
            "wrapping_sub",
            "wrapping_mul",
            "overflowing_add",
            "overflowing_mul",
        ] {
            assert!(
                !crate_code_contains(token),
                "an unchecked length computation must not be hidden behind {token}"
            );
        }
    }

    #[test]
    fn the_crate_uses_no_heap_types() {
        for token in ["Vec<", "String", "Box<", "vec!", "format!"] {
            assert!(
                !crate_code_contains(token),
                "the filesystem has no allocator, so it may not use `{token}`"
            );
        }
    }
}
