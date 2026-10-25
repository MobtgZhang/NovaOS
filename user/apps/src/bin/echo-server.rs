#![no_std]
#![no_main]

use nova_apps::{console, ipc};

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let Some(endpoint) = ipc::endpoint_self() else {
        return -1;
    };
    let _ = console::write_str("[NOVA] echo-server ready\r\n");

    loop {
        let Some((_opcode, _payload, _caps)) = ipc::recv(endpoint) else {
            continue;
        };
        let _ = ipc::reply(endpoint, 0, b"echo");
    }
}
