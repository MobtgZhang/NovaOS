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
