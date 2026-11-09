#![no_std]
#![no_main]

use nova_apps::fsclient;
use nova_apps::{console, process};
use nova_fs::MAX_PATH_BYTES;
use nova_fs::path::canonicalize;
use nova_user;

const HELP: &str = "commands: help ls cat echo ps pwd cd whoami id su chmod chown exit clear";

const ASCII_LOGO: &[&str] = &[
    r"  _   _  ____   __  ___  __  ____",
    r" | \ | |/ __ \ /  \/ _ \|  \/  _ \",
    r" |  \| |  ___ | | | | | | .  . | | | | | | (_) |",
    r" |_|\\  | |_| ||_| |_| |_||_||_/ ||_|",
    "",
];

/// The per-session state: who is logged in and where the prompt currently is.
struct Session {
    uid: u32,
    gid: u32,
    cwd: [u8; MAX_PATH_BYTES],
    cwd_len: usize,
}

impl Session {
    fn new(uid: u32, gid: u32) -> Self {
        let mut session = Session {
            uid,
            gid,
            cwd: [0; MAX_PATH_BYTES],
            cwd_len: 1,
        };
        session.cwd[0] = b'/';
        session
    }

    fn cwd(&self) -> &[u8] {
        &self.cwd[..self.cwd_len]
    }

    fn set_cwd(&mut self, path: &[u8]) {
        let len = path.len().min(MAX_PATH_BYTES);
        self.cwd[..len].copy_from_slice(&path[..len]);
        self.cwd_len = len;
    }
}

fn write_cwd_prompt(session: &Session) {
    let user = nova_user::lookup_uid(session.uid)
        .map(|account| account.name)
        .unwrap_or("?");
    let _ = console::write_str(user);
    let _ = console::write_str("@");
    let _ = console::write_str(nova_user::HOSTNAME);
    let _ = console::write_str(":");
    let _ = console::write_bytes(session.cwd());
    let _ = console::write_str("> ");
}

/// Read a line from input with optional echo of typed characters.
fn read_line(echo: bool, line: &mut [u8]) -> usize {
    let mut len = 0usize;
    loop {
        if len >= line.len() {
            let _ = console::write_str("\r\n");
            return len;
        }
        let mut chunk = [0u8; 1];
        match nova_apps::syscall::read(0, &mut chunk) {
            Ok(n) if n > 0 => {
                let byte = chunk[0];
                match byte {
                    b'\r' | b'\n' => {
                        let _ = console::write_str("\r\n");
                        return len;
                    }
                    b'\x08' | b'\x7f' => {
                        if len > 0 {
                            len -= 1;
                            if echo {
                                let _ = console::write_str("\x08 \x08");
                            }
                        }
                    }
                    byte if byte >= 0x20 && byte < 0x7f => {
                        line[len] = byte;
                        len += 1;
                        if echo {
                            let _ = console::write_bytes(&[byte]);
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                nova_apps::yield_now();
            }
        }
    }
}

fn login() -> Session {
    let _ = console::write_str("\r\n");
    let _ = console::write_str("NovaOS Safe Terminal\r\n");
    loop {
        let _ = console::write_str("novaos login: ");
        let mut name = [0u8; 32];
        let name_len = read_line(true, &mut name);
        let trimmed = &name[..name_len];
        let name = core::str::from_utf8(trimmed).unwrap_or("");
        let account = match nova_user::lookup_account(name) {
            Some(account) => account,
            None => {
                let _ = console::write_str("Login incorrect\r\n");
                continue;
            }
        };

        let _ = console::write_str("Password: ");
        let mut password = [0u8; 32];
        let password_len = read_line(false, &mut password);
        let password = core::str::from_utf8(&password[..password_len]).unwrap_or("");
        if !nova_user::verify_password(account, password) {
            let _ = console::write_str("Login incorrect\r\n");
            continue;
        }

        let _ = console::write_fmt(format_args!("Welcome, {}!\r\n", account.name));
        return Session::new(account.uid, account.gid);
    }
}

/// Split a line into command name and remaining argument text.
fn split_command(line: &str) -> (&str, &str) {
    match line.split_once(' ') {
        Some((name, rest)) => (name, rest.trim()),
        None => (line.trim(), ""),
    }
}

/// Resolve `path` against cwd, writes the canonical form into `out`,
/// returns its length.
fn resolve_path(session: &Session, path: &str, out: &mut [u8; MAX_PATH_BYTES]) -> usize {
    let src = path.as_bytes();
    let mut joined = [0u8; MAX_PATH_BYTES];
    let mut joined_len;
    if src.first().copied() == Some(b'/') {
        let take = src.len().min(MAX_PATH_BYTES);
        joined[..take].copy_from_slice(&src[..take]);
        joined_len = take;
    } else {
        let cwd_len = session.cwd_len;
        joined[..cwd_len].copy_from_slice(session.cwd());
        joined_len = cwd_len;
        if session.cwd() != b"/" {
            joined[joined_len] = b'/';
            joined_len += 1;
        }
        let take = src.len().min(MAX_PATH_BYTES - joined_len);
        joined[joined_len..joined_len + take].copy_from_slice(&src[..take]);
        joined_len += take;
    }
    match canonicalize(&joined[..joined_len]) {
        Ok(canonical) => {
            let bytes = canonical.as_bytes();
            let take = bytes.len().min(MAX_PATH_BYTES);
            out[..take].copy_from_slice(&bytes[..take]);
            take
        }
        Err(_) => {
            out[0] = b'/';
            1
        }
    }
}

fn long_line(dir_path: &[u8], name: &[u8], directory: bool) {
    let mut full = [0u8; MAX_PATH_BYTES];
    let mut len = dir_path.len();
    full[..len].copy_from_slice(dir_path);
    if dir_path != b"/" && len > 0 && full[len - 1] != b'/' {
        full[len] = b'/';
        len += 1;
    } else if dir_path == b"/" {
        full[0] = b'/';
        len = 1;
    }
    let take = name.len().min(MAX_PATH_BYTES - len);
    full[len..len + take].copy_from_slice(name);
    len += take;
    let target = &full[..len];

    let meta = match fsclient::stat(target) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    let mut mode_str = [0u8; 9];
    let _ = nova_user::format_mode(meta.mode as u16, &mut mode_str);
    let kind = if directory { 'd' } else { '-' };
    let _ = console::write_bytes(&[kind as u8]);
    let _ = console::write_bytes(&mode_str);
    let _ = console::write_fmt(format_args!(
        " {:>4} {:>4} {:>5} ",
        meta.owner, meta.group, meta.size
    ));
    let _ = console::write_bytes(name);
    if directory {
        let _ = console::write_str("/");
    }
    let _ = console::write_str("\r\n");
}

/// Walk every record of a directory, page by page. `visit` is called once
/// per entry. Returns false when the directory cannot be listed at all.
fn for_each_entry(dir_path: &[u8], mut visit: impl FnMut(fsclient::Record<'_>)) -> bool {
    let mut offset: u32 = 0;
    let mut visited = false;
    loop {
        let mut page_bytes = [0u8; nova_apps::fsclient::PAGE_BYTES];
        let page = match fsclient::list_at(dir_path, offset, &mut page_bytes) {
            Ok(page) => page,
            Err(_) => return false,
        };
        let mut records = fsclient::Records::new(&page_bytes[..page.bytes]);
        while let Some(record) = records.next() {
            visit(record);
            visited = true;
        }
        offset = offset.saturating_add(page.count);
        if page.count == 0 || offset >= page.total {
            return visited || page.total > 0;
        }
    }
}

/// The short listing: name per line, directories suffixed with a slash.
fn print_short_listing(dir_path: &[u8]) -> bool {
    for_each_entry(dir_path, |record| {
        let _ = console::write_bytes(record.name);
        if record.directory {
            let _ = console::write_str("/");
        }
        let _ = console::write_str("\r\n");
    })
}

/// The long listing: `d|- mode owner group size name[/]`, one stat per
/// entry. Directories on this system hold a handful of entries, not
/// thousands, so the per-entry round trip is affordable.
fn print_long_listing(dir_path: &[u8]) -> bool {
    for_each_entry(dir_path, |record| {
        long_line(dir_path, record.name, record.directory);
    })
}

fn list_long(session: &Session, path: &str) {
    let mut buf = [0u8; MAX_PATH_BYTES];
    let len = resolve_path(session, path, &mut buf);
    let dir_path = &buf[..len];
    if !print_long_listing(dir_path) {
        let _ = console::write_str("ls: not found\r\n");
    }
}

fn execute(line: &str, session: &mut Session) {
    let (name, args) = split_command(line);
    match name {
        "help" => {
            let _ = console::write_str(HELP);
            let _ = console::write_str("\r\n");
        }
        "ls" => {
            let arg = args.trim();
            if arg == "-l" || arg.starts_with("-l ") {
                let tail = arg[2..].trim();
                let dir = if tail.is_empty() { "." } else { tail };
                list_long(session, dir);
                return;
            }
            let path = if arg.is_empty() { "." } else { arg };
            let mut buf = [0u8; MAX_PATH_BYTES];
            let len = resolve_path(session, path, &mut buf);
            let dir_path = &buf[..len];
            if !print_short_listing(dir_path) {
                let _ = console::write_str("ls: not found\r\n");
            }
        }
        "cat" => {
            if args.is_empty() {
                let _ = console::write_str("usage: cat <file>\r\n");
                return;
            }
            let mut buf = [0u8; MAX_PATH_BYTES];
            let len = resolve_path(session, args, &mut buf);
            let file_path = &buf[..len];
            let mut buffer = [0u8; nova_fs::MAX_FILE_BYTES];
            match fsclient::open(file_path, 0) {
                Ok(file) => {
                    let mut total = 0usize;
                    loop {
                        match fsclient::read(file.handle, &mut buffer[total..]) {
                            Ok(0) => break,
                            Ok(count) => total += count,
                            Err(error) => {
                                let _ = console::write_fmt(format_args!(
                                    "cat: read: {}\r\n",
                                    error.name()
                                ));
                                break;
                            }
                        }
                        if total >= buffer.len() {
                            break;
                        }
                    }
                    let _ = console::write_bytes(&buffer[..total]);
                    if total == 0 || buffer[total - 1] != b'\n' {
                        let _ = console::write_str("\r\n");
                    }
                    let _ = fsclient::close(file.handle);
                }
                Err(error) => {
                    let _ = console::write_fmt(format_args!("cat: open: {}\r\n", error.name()));
                }
            }
        }
        "echo" => {
            let _ = console::write_str(args);
            let _ = console::write_str("\r\n");
        }
        "ps" => {
            let _ = console::write_fmt(format_args!("processes={}\r\n", process::count()));
        }
        "pwd" => {
            let _ = console::write_bytes(session.cwd());
            let _ = console::write_str("\r\n");
        }
        "cd" => {
            let target = if args.is_empty() { "/" } else { args };
            let mut buf = [0u8; MAX_PATH_BYTES];
            let len = resolve_path(session, target, &mut buf);
            let dir_path = &buf[..len];
            match fsclient::stat(dir_path) {
                Ok(meta) if meta.directory => {
                    let canonical = match canonicalize(dir_path) {
                        Ok(c) => c,
                        Err(_) => {
                            let _ = console::write_str("cd: invalid path\r\n");
                            return;
                        }
                    };
                    session.set_cwd(canonical.as_bytes());
                }
                Ok(_) => {
                    let _ = console::write_str("cd: not a directory\r\n");
                }
                Err(_) => {
                    let _ = console::write_str("cd: no such directory\r\n");
                }
            }
        }
        "whoami" => {
            let account = nova_user::lookup_uid(session.uid)
                .map(|a| a.name)
                .unwrap_or("?");
            let _ = console::write_str(account);
            let _ = console::write_str("\r\n");
        }
        "id" => {
            let _ = console::write_fmt(format_args!(
                "uid={}({}) gid={}({})\r\n",
                session.uid,
                nova_user::lookup_uid(session.uid)
                    .map(|a| a.name)
                    .unwrap_or("?"),
                session.gid,
                nova_user::lookup_uid(session.gid)
                    .map(|a| a.name)
                    .unwrap_or("?"),
            ));
        }
        "chmod" => {
            let mut parts = args.splitn(2, ' ');
            let (mode_text, path_text) = match (parts.next(), parts.next()) {
                (Some(m), Some(p)) => (m, p.trim()),
                _ => {
                    let _ = console::write_str("usage: chmod <mode> <path>\r\n");
                    return;
                }
            };
            let mode = match nova_user::parse_mode(mode_text) {
                Some(mode) => mode,
                None => {
                    let _ = console::write_str("chmod: mode must be three octal digits\r\n");
                    return;
                }
            };
            let mut buf = [0u8; MAX_PATH_BYTES];
            let len = resolve_path(session, path_text, &mut buf);
            let file_path = &buf[..len];
            // The gate for the permission model lives here in the shell,
            // where the session's identity is known; the file service only
            // knows scopes, so it cannot make this decision for us.
            let meta = match fsclient::stat(file_path) {
                Ok(meta) => meta,
                Err(_) => {
                    let _ = console::write_str("chmod: not found\r\n");
                    return;
                }
            };
            if session.uid != 0 && session.uid != meta.owner {
                let _ = console::write_str("chmod: operation not permitted\r\n");
                return;
            }
            match fsclient::chmod(file_path, mode, session.uid) {
                Ok(()) => {}
                Err(_) => {
                    let _ = console::write_str("chmod: operation not permitted\r\n");
                }
            }
        }
        "chown" => {
            let mut parts = args.splitn(2, ' ');
            let (uid_text, path_text) = match (parts.next(), parts.next()) {
                (Some(u), Some(p)) => (u, p.trim()),
                _ => {
                    let _ = console::write_str("usage: chown <uid> <path>\r\n");
                    return;
                }
            };
            let uid: u32 = match uid_text.parse() {
                Ok(uid) => uid,
                Err(_) => {
                    let _ = console::write_str("chown: usage: chown <uid> <path>\r\n");
                    return;
                }
            };
            if session.uid != 0 {
                let _ = console::write_str("chown: operation not permitted\r\n");
                return;
            }
            let mut buf = [0u8; MAX_PATH_BYTES];
            let len = resolve_path(session, path_text, &mut buf);
            let file_path = &buf[..len];
            match fsclient::chown(file_path, uid, uid, session.uid) {
                Ok(()) => {}
                Err(_) => {
                    let _ = console::write_str("chown: operation not permitted\r\n");
                }
            }
        }
        "su" => {
            if args.is_empty() {
                let _ = console::write_str("Password for root: ");
                let mut password = [0u8; 32];
                let password_len = read_line(false, &mut password);
                let password = core::str::from_utf8(&password[..password_len]).unwrap_or("");
                let root = nova_user::lookup_account("root").unwrap();
                if !nova_user::verify_password(root, password) {
                    let _ = console::write_str("su: incorrect password\r\n");
                    return;
                }
                session.uid = root.uid;
                session.gid = root.gid;
                let _ = console::write_str("Switched to root\r\n");
                return;
            }
            let target = args.split_once(' ').map(|(name, _)| name).unwrap_or(args);
            let account = match nova_user::lookup_account(target) {
                Some(account) => account,
                None => {
                    let _ = console::write_str("su: unknown user\r\n");
                    return;
                }
            };
            let _ = console::write_fmt(format_args!("Password for {}: ", account.name));
            let mut password = [0u8; 32];
            let password_len = read_line(false, &mut password);
            let password = core::str::from_utf8(&password[..password_len]).unwrap_or("");
            if !nova_user::verify_password(account, password) {
                let _ = console::write_str("su: incorrect password\r\n");
                return;
            }
            session.uid = account.uid;
            session.gid = account.gid;
            let _ = console::write_fmt(format_args!("Switched to {}\r\n", account.name));
        }
        "exit" => {
            let _ = console::write_str("Goodbye!\r\n");
            *session = login();
        }
        "clear" => {
            let _ = nova_apps::syscall::console_clear();
        }
        _ => {
            let _ = console::write_fmt(format_args!("unknown command: {name}\r\n"));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    for line in ASCII_LOGO {
        let _ = console::write_str(line);
        let _ = console::write_str("\r\n");
    }
    let _ = console::write_str("NovaOS x86_64 (Safe Terminal)\r\n");
    let _ = console::write_str("Type 'help' for a list of commands.\r\n\r\n");

    loop {
        let mut session = login();
        loop {
            write_cwd_prompt(&session);
            let mut line = [0u8; 256];
            let len = read_line(true, &mut line);
            if len == 0 {
                continue;
            }
            let line = core::str::from_utf8(&line[..len]).unwrap_or("");
            execute(line, &mut session);
        }
    }
}
