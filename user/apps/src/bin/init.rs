#![no_std]
#![no_main]

use nova_apps::{console, process, yield_now};

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] init ready\r\n");

    // The boot-time task pool is bounded at eight: the kernel's root task, this
    // `init` process, and six children. The six children are the services the
    // desktop needs. `echo-server` is packaged for the M3 IPC suites but is not
    // one of the six boot services, because a seventh child would hit the quota.
    const SERVICES: &[&str] = &[
        "file-server",
        "console-server",
        "shell",
        "terminal",
        "editor",
        "file-manager",
    ];

    for name in SERVICES {
        if process::exec(name).is_none() {
            let _ = console::write_str("[NOVA] init failed to start service\r\n");
            return -1;
        }
        yield_now();
    }

    loop {
        for name in SERVICES {
            if !process::status(name) {
                if process::exec(name).is_none() {
                    let _ = console::write_str("[NOVA] init failed to restart service\r\n");
                }
            }
        }
        yield_now();
    }
}
