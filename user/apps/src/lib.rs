#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![feature(linkage)]

pub mod allocator;
pub mod console;
pub mod ipc;
pub mod process;
pub mod syscall;

use nova_abi::nr;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();

    let status = main();
    syscall::exit_thread(status)
}

#[unsafe(no_mangle)]
#[linkage = "weak"]
extern "C" fn main() -> i64 {
    0
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = console::write_fmt(format_args!("user panic: {info}"));
    syscall::exit_thread(-1)
}

pub fn process_count() -> Option<u64> {
    syscall::call(nr::PROCESS_COUNT.raw(), [0; 6])
        .ok()
        .map(|value| value as u64)
}

pub fn yield_now() -> bool {
    syscall::call(nr::YIELD.raw(), [0; 6]).is_ok()
}
