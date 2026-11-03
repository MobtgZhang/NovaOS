#![no_std]
#![no_main]

use nova_abi::cap::Rights;
use nova_apps::{console, process, yield_now};

/// One boot service and the path scope `init` passes down to it.
///
/// A service that needs no files passes `None`, and that is the default: §16.1
/// says a process starts with no kernel-level file authority, and authority
/// arrives only by being handed a slice of its parent's. `init` holds `/` with
/// `GRANT` from the boot, so it is the one place in the system that can cut
/// these slices — and it cuts only the ones listed here.
type Service = (&'static str, Option<(&'static str, Rights)>);

const HOME: Rights = Rights::READ.union(Rights::WRITE);

/// The boot-time task pool is bounded at eight: the kernel's root task, this
/// `init` process, and six children. The six children are the services the
/// desktop needs. `echo-server` is packaged for the M3 IPC suites but is not
/// one of the six boot services, because a seventh child would hit the quota.
const SERVICES: &[Service] = &[
    ("file-server", None),
    ("console-server", None),
    ("shell", None),
    ("terminal", None),
    // The editor reads and writes the user's own files, and nothing else.
    ("editor", Some(("/home", HOME))),
    // The file manager browses; it never writes.
    ("file-manager", Some(("/", Rights::READ))),
];

fn start(name: &str, scope: Option<(&str, Rights)>) -> Option<u64> {
    match scope {
        None => process::exec(name),
        Some((path, rights)) => process::exec_with_scope(name, path, rights),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let _ = console::write_str("[NOVA] init ready\r\n");

    for (name, scope) in SERVICES {
        if start(name, *scope).is_none() {
            let _ = console::write_str("[NOVA] init failed to start service\r\n");
            return -1;
        }
        yield_now();
    }

    loop {
        for (name, scope) in SERVICES {
            if !process::status(name) {
                if start(name, *scope).is_none() {
                    let _ = console::write_str("[NOVA] init failed to restart service\r\n");
                }
            }
        }
        yield_now();
    }
}
