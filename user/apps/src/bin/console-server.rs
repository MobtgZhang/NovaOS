#![no_std]
#![no_main]

use nova_apps::{console, ipc, syscall};

/// In safe-terminal mode the boot chain stays silent; printing "ready"
/// here would pollute the black screen the user expects.
fn is_quiet() -> bool {
    matches!(syscall::boot_mode(), Ok(2))
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let Some(endpoint) = ipc::endpoint_self() else {
        return -1;
    };
    if !is_quiet() {
        let _ = console::write_str("[NOVA] console-server ready\r\n");
    }

    loop {
        let Some((_opcode, payload, _caps)) = ipc::recv(endpoint) else {
            continue;
        };
        let text = payload
            .iter()
            .copied()
            .take_while(|byte| *byte != 0)
            .count();
        let _ = console::write_bytes(&payload[..text]);
        let _ = ipc::reply(endpoint, 0, b"ok");
    }
}
