#![no_std]
#![no_main]

use nova_apps::{console, process};
use nova_fs::{DirEntry, FileKind, MAX_NAME_BYTES, RamFs};
use nova_protocols::shell::{ShellCommand, parse};

const HELP: &str = "help ls cat echo ps";

// SAFETY: the shell is single-threaded, and this static is only accessed by
// `shell_fs()` from that thread. A value on the stack would overflow the user
// stack because `RamFs` is a fixed-size pool.
static mut SHELL_FS: RamFs = RamFs::new();

fn shell_fs() -> &'static mut RamFs {
    // SAFETY: see the `SHELL_FS` invariant above.
    unsafe { &mut *core::ptr::addr_of_mut!(SHELL_FS) }
}

fn execute(command: ShellCommand<'_>) {
    match command {
        ShellCommand::Help => {
            let _ = console::write_str(HELP);
            let _ = console::write_str("\r\n");
        }
        ShellCommand::Ls { path } => {
            let path = path.unwrap_or("/");
            let mut entries = [DirEntry {
                name: [0; MAX_NAME_BYTES],
                name_len: 0,
                kind: FileKind::File,
                size: 0,
            }; nova_fs::ramfs::MAX_CHILDREN];
            match shell_fs().list_dir(path.as_bytes(), &mut entries) {
                Ok(count) => {
                    for entry in entries.iter().take(count) {
                        let _ = console::write_bytes(entry.name_bytes());
                        if entry.kind == FileKind::Directory {
                            let _ = console::write_str("/");
                        }
                        let _ = console::write_str("\r\n");
                    }
                }
                Err(_) => {
                    let _ = console::write_str("ls: not found\r\n");
                }
            }
        }
        ShellCommand::Cat { path } => {
            let filesystem = shell_fs();
            match filesystem.open(path.as_bytes()) {
                Ok(handle) => {
                    let mut buffer = [0u8; nova_fs::ramfs::MAX_FILE_BYTES];
                    match filesystem.read(handle, &mut buffer) {
                        Ok(count) => {
                            let _ = console::write_bytes(&buffer[..count]);
                            let _ = console::write_str("\r\n");
                        }
                        Err(_) => {
                            let _ = console::write_str("cat: read failed\r\n");
                        }
                    }
                    let _ = filesystem.close(handle);
                }
                Err(_) => {
                    let _ = console::write_str("cat: not found\r\n");
                }
            }
        }
        ShellCommand::Echo { text } => {
            let _ = console::write_bytes(text.as_bytes());
            let _ = console::write_str("\r\n");
        }
        ShellCommand::Ps => {
            let _ = console::write_fmt(format_args!("processes={}\r\n", process::count()));
        }
    }
}

fn run(command: &str) {
    match parse(command) {
        Some(command) => execute(command),
        None => {
            let _ = console::write_fmt(format_args!("unknown command: {command}\r\n"));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    let filesystem = shell_fs();
    let _ = filesystem.create_file(b"/hello.txt");
    let _ = filesystem.create_dir(b"/docs");
    let _ = filesystem.create_file(b"/docs/readme.txt");
    if let Ok(handle) = filesystem.open(b"/hello.txt") {
        let _ = filesystem.write(handle, b"NovaOS shell");
        let _ = filesystem.close(handle);
    }

    let _ = console::write_str("[NOVA] shell ready: ");
    let _ = console::write_str(HELP);
    let _ = console::write_str("\r\n");

    for command in ["help", "ls", "cat /hello.txt", "echo NovaOS", "ps"] {
        run(command);
    }

    loop {
        nova_apps::yield_now();
    }
}
