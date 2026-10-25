#![no_std]
#![no_main]

use nova_apps::console;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] echo-client ready\r\n");
    0
}
