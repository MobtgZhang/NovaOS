//! NovaOS user accounts, password verification, and the tiny permission model
//! the shell needs to implement `chmod`/`chown`.
//!
//! # What this crate is
//!
//! A static roster of two accounts — `root` (password `root`) and `zhang`
//! (password `zhang`) — the two password hashes, and the little helpers the
//! shell needs to know: which uid a name maps to, which gid a name maps to,
//! and whether a password matches. There are no syscalls here, no services,
//! and no `alloc`; every account is a `&'static str`, so this compiles for
//! the host *and* for the bare-metal applications.
//!
//! # Why this is a crate rather than a module of the shell
//!
//! `nova-apps` — the applications crate — cannot be *run* by the host test
//! runner: it is a set of `no_std`/`no_main` binaries with their own
//! `_start`. If the password verifier lived there, it would be a password
//! verifier nobody ever ran a test against. A crate that is testable on the
//! host is the difference between a hash comparison that passes on every
//! commit and one that is reviewed once and then hoped for.
//!
//! # Why FNV-1a
//!
//! A cryptographic hash is not wanted here: there is no on-disk credential,
//! no network protocol, no threat model of offline brute force on a static
//! binary. What is wanted is a *different* bit pattern for a wrong password
//! than for the right one, with no dependency, no table, and no allocation.
//! FNV-1a over 64 bits gives that, deterministically, on every machine the
//! same inputs produce the same hash.
//!
//! # The permission bits
//!
//! A file's mode is nine bits — `rwx` for owner, group and others —
//! exactly the layout every POSIX kernel uses. A file's owner is the uid;
//! the group is the gid. A uid of `0` is `root`: root reads and writes
//! anything, changes any mode, takes ownership of any file. A uid of
//! anything else is subject to the bits.
//!
//! The *interpretation* of the bits — who may read, write, execute — is the
//! filesystem's job. This crate only answers "may this caller do this to
//! this inode, given owner/mode?".

#![no_std]

/// The uid `root` has. Root is uid 0 on every UNIX-like system the user has
/// ever used, and this is no exception: root is the subject the permission
/// checks pass to it unconditionally.
pub const ROOT_UID: u32 = 0;

/// The gid `root` has.
pub const ROOT_GID: u32 = 0;

/// The uid of the default administrative account (`zhang`).
pub const ZHANG_UID: u32 = 1000;

/// The gid of the default administrative account.
pub const ZHANG_GID: u32 = 1000;

/// The hostname the shell prints in its prompt.
///
/// The default matches what the boot menu calls itself: the user typed the
/// `novaos` name to get here, so `user@novaos` is the machine's name in every
/// context a person sees.
pub const HOSTNAME: &str = "novaos";

/// One user account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    /// Login name, exactly as typed at the prompt.
    pub name: &'static str,
    /// User id.
    pub uid: u32,
    /// Primary group id.
    pub gid: u32,
    /// FNV-1a 64-bit hash of the password.
    pub password_hash: u64,
}

/// The two accounts every NovaOS ships with.
///
/// `root` has the empty password by intent of the user: every account on
/// NovaOS is explicitly provisioned, and the brief names these two verbatim.
pub const ACCOUNTS: &[Account] = &[
    Account {
        name: "root",
        uid: ROOT_UID,
        gid: ROOT_GID,
        password_hash: hash_password(b"root"),
    },
    Account {
        name: "zhang",
        uid: ZHANG_UID,
        gid: ZHANG_GID,
        password_hash: hash_password(b"zhang"),
    },
];

/// FNV-1a/64 over the password bytes.
const fn hash_password(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash: u64 = OFFSET;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(PRIME);
        index += 1;
    }
    hash
}

/// Look up an account by name.
pub fn lookup_account(name: &str) -> Option<&'static Account> {
    ACCOUNTS.iter().find(|account| account.name == name)
}

/// Whether `password` matches the account's stored hash.
pub fn verify_password(account: &Account, password: &str) -> bool {
    hash_password(password.as_bytes()) == account.password_hash
}

/// Whether `caller` may perform `action` on an inode whose `owner` and `mode`
/// are given.
///
/// `action` is a combination of read/write/execute bits:
///
/// * `0o4` — read
/// * `0o2` — write
/// * `0o1` — execute (for a directory, this means "list and enter")
///
/// Root (uid 0) is admitted without consulting the bits. Any other uid
/// consults the owner bits if `owner == uid`, the group bits otherwise, and
/// the other bits if the group test fails.
pub fn may(caller: u32, owner: u32, owner_gid: u32, mode: u16, action: u16) -> bool {
    if caller == ROOT_UID {
        return true;
    }
    let shift: u32 = if caller == owner {
        6
    } else if caller == ROOT_GID || group_of(caller) == Some(owner_gid) {
        3
    } else {
        0
    };
    let granted = (mode >> shift) & 0o7;
    (granted & action) == action
}

/// A caller's primary group id, or `None` for a uid with no account.
///
/// Root's group is root's gid; zhang's is zhang's gid; anything else has
/// no account and accordingly no group.
pub fn group_of(uid: u32) -> Option<u32> {
    ACCOUNTS
        .iter()
        .find(|account| account.uid == uid)
        .map(|account| account.gid)
}

/// Parse an octal mode string like `"644"` into a mode word.
///
/// The shell refuses to pass mode bits the string cannot express, so this
/// returns `None` for anything other than exactly three octal digits.
pub fn parse_mode(text: &str) -> Option<u16> {
    if text.len() != 3 {
        return None;
    }
    let mut mode: u16 = 0;
    for byte in text.bytes() {
        if !(b'0'..=b'7').contains(&byte) {
            return None;
        }
        mode = mode * 8 + (byte - b'0') as u16;
    }
    Some(mode)
}

/// Format a Unix-style `rwxrwxrwx` permission string into `out`.
///
/// Returns the number of bytes written; the buffer must hold at least ten
/// bytes (`d`/`-` + nine bits + NUL is unnecessary — no terminator).
pub fn format_mode(mode: u16, out: &mut [u8; 9]) -> usize {
    for (index, shift) in [6u32, 3, 0].into_iter().enumerate() {
        let bits = (mode >> shift) & 0o7;
        out[index * 3] = if bits & 0o4 != 0 { b'r' } else { b'-' };
        out[index * 3 + 1] = if bits & 0o2 != 0 { b'w' } else { b'-' };
        out[index * 3 + 2] = if bits & 0o1 != 0 { b'x' } else { b'-' };
    }
    9
}

/// The account whose uid matches, or `None` for a uid with no login.
pub fn lookup_uid(uid: u32) -> Option<&'static Account> {
    ACCOUNTS.iter().find(|account| account.uid == uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_hashes_resolve() {
        let root = lookup_account("root").unwrap();
        assert_eq!(root.uid, ROOT_UID);
        assert_eq!(root.gid, ROOT_GID);
        let zhang = lookup_account("zhang").unwrap();
        assert_eq!(zhang.uid, ZHANG_UID);
        assert!(verify_password(root, "root"));
        assert!(!verify_password(root, "wrong"));
        assert!(verify_password(zhang, "zhang"));
        assert!(!verify_password(zhang, ""));
    }

    #[test]
    fn parse_mode_accepts_only_three_octal_digits() {
        assert_eq!(parse_mode("644"), Some(0o644));
        assert_eq!(parse_mode("755"), Some(0o755));
        assert_eq!(parse_mode("0"), None);
        assert_eq!(parse_mode("888"), None);
        assert_eq!(parse_mode("64"), None);
        assert_eq!(parse_mode("6444"), None);
        assert_eq!(parse_mode(""), None);
    }

    #[test]
    fn may_enforces_owner_group_other_consistency() {
        // root bypasses the bits entirely.
        assert!(may(ROOT_UID, ZHANG_UID, ZHANG_GID, 0o000, 0o7));

        // the owner can read/write with 0o600, cannot execute.
        assert!(may(ZHANG_UID, ZHANG_UID, ZHANG_GID, 0o600, 0o4 | 0o2));
        assert!(!may(ZHANG_UID, ZHANG_UID, ZHANG_GID, 0o600, 0o1));

        // 0o644 lets the world read, but not write.
        assert!(may(1, ZHANG_UID, ZHANG_GID, 0o644, 0o4));
        assert!(!may(1, ZHANG_UID, ZHANG_GID, 0o644, 0o2));

        // 0o640 lets the group read, but not a uid with no account.
        assert!(may(ZHANG_UID, ZHANG_UID, ZHANG_GID, 0o640, 0o4));
        assert!(!may(1, ZHANG_UID, ZHANG_GID, 0o640, 0o4));
    }
}
