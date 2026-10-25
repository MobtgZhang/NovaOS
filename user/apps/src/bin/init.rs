#![no_std]
#![no_main]

use nova_apps::{console, process, yield_now};

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] init ready\r\n");

    for name in ["echo-server", "file-server", "console-server", "shell"] {
        if process::exec(name).is_none() {
            let _ = console::write_str("[NOVA] init failed to start service\r\n");
            return -1;
        }
        yield_now();
    }

    0
}
