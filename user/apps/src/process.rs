use nova_abi::cap::Rights;
use nova_abi::nr;

use crate::syscall;

pub fn exec(name: &str) -> Option<u64> {
    syscall::call(
        nr::PROCESS_EXEC.raw(),
        [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0],
    )
    .ok()
    .map(|raw| raw as u64)
}

/// Start `name` and hand the child the path scope `path`.
///
/// This is the parent-driven grant of §16.1: the child starts with no file
/// authority and receives exactly this slice of ours. The kernel refuses the
/// call unless our own scope covers `path` with `rights` *and* carries
/// `GRANT`, so a program can only pass on what it was itself given.
pub fn exec_with_scope(name: &str, path: &str, rights: Rights) -> Option<u64> {
    syscall::call(
        nr::PROCESS_EXEC.raw(),
        [
            name.as_ptr() as u64,
            name.len() as u64,
            path.as_ptr() as u64,
            path.len() as u64,
            rights.bits() as u64,
            0,
        ],
    )
    .ok()
    .map(|raw| raw as u64)
}

pub fn status(name: &str) -> bool {
    syscall::call(
        nr::PROCESS_STATUS.raw(),
        [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0],
    )
    .ok()
    .map(|value| value != 0)
    .unwrap_or(false)
}

pub fn count() -> u64 {
    crate::process_count().unwrap_or(0)
}
