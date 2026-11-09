#![no_std]
#![no_main]

use nova_abi::cap::Rights;
use nova_apps::{console, process, syscall, yield_now};

/// One boot service and the path scope `init` passes down to it.
///
/// A service that needs no files passes `None`, and that is the default: §16.1
/// says a process starts with no kernel-level file authority, and authority
/// arrives only by being handed a slice of its parent's. `init` holds `/` with
/// `GRANT` from the boot, so it is the one place in the system that can cut
/// these slices — and it cuts only the ones listed here.
type Service = (&'static str, Option<(&'static str, Rights)>);

const HOME: Rights = Rights::READ.union(Rights::WRITE);

/// The desktop boot mode: every boot service starts.
///
/// `echo-server` is packaged for the M3 IPC suites but is not one of the boot
/// services: the widgets demo takes its slot, and the thread pool is sized for
/// exactly the services in this list.
const DESKTOP_SERVICES: &[Service] = &[
    ("file-server", None),
    ("console-server", None),
    ("shell", None),
    ("terminal", None),
    // The editor reads and writes the user's own files, and nothing else.
    ("editor", Some(("/home", HOME))),
    // The file manager browses; it never writes.
    ("file-manager", Some(("/", Rights::READ))),
    ("widgets-demo", None),
];

/// The safe-terminal boot mode: only the services a terminal session needs.
const SAFE_TERMINAL_SERVICES: &[Service] = &[
    ("file-server", None),
    ("console-server", None),
    ("shell", None),
];

/// Select the services the boot mode asks for.
fn services() -> &'static [Service] {
    match syscall::boot_mode() {
        Ok(2) => SAFE_TERMINAL_SERVICES,
        _ => DESKTOP_SERVICES,
    }
}

fn start(name: &str, scope: Option<(&str, Rights)>) -> Option<u64> {
    match scope {
        None => process::exec(name),
        Some((path, rights)) => process::exec_with_scope(name, path, rights),
    }
}

/// Whether this boot mode is the quiet one. The safe terminal stays silent;
/// desktop stays verbose so the screen shows the boot list as each service
/// reports in.
fn is_quiet() -> bool {
    matches!(syscall::boot_mode(), Ok(2))
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let quiet = is_quiet();

    if !quiet {
        let mode_name = match syscall::boot_mode().unwrap_or(0) {
            1 => "desktop",
            2 => "safe-terminal",
            _ => "unknown",
        };
        let _ = console::write_str("[NOVA] init ready (");
        let _ = console::write_str(mode_name);
        let _ = console::write_str(")\r\n");
    }

    for (name, scope) in services() {
        if start(name, *scope).is_none() {
            // A spawn failure is always a bug worth being loud about — even
            // in safe-terminal mode, where the user otherwise sees nothing.
            let _ = console::write_str("[NOVA] init failed to start service\r\n");
            return -1;
        }
        if !quiet {
            let _ = console::write_fmt(format_args!("[NOVA] init: started {}\r\n", name));
        }
        yield_now();
    }

    loop {
        for (name, scope) in services() {
            if !process::status(name) {
                if start(name, *scope).is_none() {
                    let _ = console::write_str("[NOVA] init failed to restart service\r\n");
                }
            }
        }
        yield_now();
    }
}
