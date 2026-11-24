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
        if let Some((_opcode, _payload, Some(token))) = ipc::recv(endpoint) {
            let _ = ipc::reply(endpoint, token, b"echo");
        }
    }
}
