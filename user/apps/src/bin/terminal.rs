#![no_std]
#![no_main]

//! The terminal window: a `nova>` shell, a scrollback, and a keyboard.
//!
//! The M6 desktop needs a terminal as one of its three first applications
//! (§20.8). This one speaks to the system's services directly through their
//! protocols: a handful of built-in commands reach the file service (`ls`,
//! `cat`, `stat`), the process table (`ps`), and the kernel's own ABI
//! (`uname`) — the working core of a shell, without a PTY or a separate
//! shell process behind it.
//!
//! # Text
//!
//! The terminal renders with Sarasa Gothic Mono SC, embedded below: ASCII
//! at a half-em advance, GB2312 level-1 hanzi at full width, all vector
//! outlines through `nova-type` — any size, anti-aliased. A command's
//! Chinese output is text, not boxes.
//!
//! # Why the scrollback and the command line are two text boxes
//!
//! The scrollback is never edited, so it is a text box with `focused` left
//! off: it gets the same clipping, wrapping and scrolling the editor's field
//! gets, for free, instead of a second implementation of line layout. The
//! command line is the focused one, and it is the only buffer keystrokes are
//! written into — so a keystroke can never end up in the wrong place by
//! accident, because there is exactly one place for it to go.

use nova_apps::graphics;
use nova_apps::graphics::{InputEvent, Window};
use nova_apps::{console, fsclient, process, yield_now};
use nova_protocols::input::{INPUT_BUTTON_PRIMARY, INPUT_EVENT_KEY, INPUT_EVENT_POINTER};
use nova_ui::keyboard::{Decoder, Key};
use nova_ui::{Button, Canvas, Rect, TextBox, TitlebarHit, draw_titlebar, palette, titlebar_hit};

/// The terminal face embedded in this program: Sarasa Gothic Mono SC,
/// subset to ASCII, CJK punctuation, and the GB2312 level-1 hanzi — the
/// user-designated terminal face.
static SARASA: &[u8] = include_bytes!("../../../../assets/fonts/sarasa-terminal.ttf");

/// Window width in pixels.
const WIDTH: u32 = 440;
/// Window height in pixels.
const HEIGHT: u32 = 320;

/// The boot services `ps` reports on, by their initramfs names.
///
/// A fixed list rather than a process-table walk: the kernel's process
/// service answers for a *named* process, and the desktop's roster is a
/// boot-time fact this file already documents.
const SERVICES: &[(&str, &str)] = &[
    ("file-server", "file-server"),
    ("console-server", "console-server"),
    ("shell", "shell"),
    ("terminal", "terminal"),
    ("editor", "editor"),
    ("file-manager", "file-manager"),
];

/// Bytes held in the scrollback.
///
/// Roughly two screens' worth at the width below. It is a stack array: a ring
/// 3 stack is 64 KiB, and two kilobytes of history is enough for a session
/// that is demonstrating input rather than logging one.
const LOG_SIZE: usize = 2048;

/// Bytes held in the command line.
const INPUT_SIZE: usize = 64;

/// How many input events one drain may bring back.
///
/// Bounded because the service's own queue is bounded: a drain that looped
/// until empty would let a flood of keystrokes turn one turn of the event
/// loop into an unbounded amount of work.
const MAX_EVENTS: usize = 8;

/// The terminal window.
struct Terminal {
    /// The scrollback text, with `\n` between lines.
    log: [u8; LOG_SIZE],
    /// The scrollback's view onto `log`.
    log_box: TextBox,
    /// The command line being typed.
    input: [u8; INPUT_SIZE],
    /// The command line's view onto `input`.
    input_box: TextBox,
    /// The button that empties the scrollback.
    clear: Button,
    /// What the status line says.
    status: &'static str,
    /// The primary button's state on the previous event — the edge
    /// the title-bar buttons fire on.
    title_down: bool,
    /// The surface this window owns, for the iconify button.
    surface: u32,
    /// Set by the close button; the main loop exits on it.
    quit: bool,
}

impl Terminal {
    /// A terminal with a greeting in its scrollback and an empty command
    /// line.
    fn new() -> Self {
        let mut terminal = Self {
            log: [0; LOG_SIZE],
            log_box: TextBox::new(Rect::new(8, 26, 424, 240)),
            input: [0; INPUT_SIZE],
            input_box: TextBox::new(Rect::new(26, 270, 322, 24)),
            clear: Button::new(Rect::new(356, 270, 76, 24), "Clear"),
            status: "ready",
            title_down: false,
            surface: 0,
            quit: false,
        };
        // The command line takes keys from the moment the window opens: a
        // terminal the user has to click into first feels broken.
        terminal.input_box.focused = true;
        terminal.append(b"NovaOS terminal (Sarasa Gothic)\n");
        // The second line is in Chinese on purpose: it exercises the CJK
        // half of the embedded face on the very first screen, so a subset
        // regression is visible before any command runs.
        terminal.append("输入 help 查看命令\n".as_bytes());
        terminal
    }

    /// Append text to the scrollback, dropping its oldest line when full.
    fn append(&mut self, text: &[u8]) {
        for &byte in text {
            if self.log_box.insert(&mut self.log, byte) {
                continue;
            }
            // Full. Drop the oldest *line*, not the oldest byte: a
            // scrollback cut in half mid-word still reads, while one cut
            // mid-line reads as two broken ones.
            self.drop_first_line();
            // The dropped line always leaves room for one byte, unless the
            // scrollback is a single line longer than the whole buffer —
            // which the command line's own bound makes impossible.
            let _ = self.log_box.insert(&mut self.log, byte);
        }
    }

    /// Remove the scrollback's first line, including its newline.
    fn drop_first_line(&mut self) {
        let len = self.log_box.len().min(LOG_SIZE);
        if len == 0 {
            return;
        }
        let cut = self.log[..len]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(len, |index| index + 1);
        if cut >= len {
            // One long line: there is nothing older to keep, so it goes.
            self.log_box.clear();
            return;
        }
        self.log.copy_within(cut..len, 0);
        // The length, not a `set_text`: the bytes are already in place and
        // copying them onto themselves would be the only thing this call
        // achieved. The cursor moves to the end, which is where the next
        // append belongs.
        self.log_box.set_len(len - cut);
    }

    /// Handle one decoded keystroke. Returns whether the window must be
    /// redrawn.
    fn on_key(&mut self, key: Key) -> bool {
        match key {
            Key::Char(byte) => {
                self.input_box.insert(&mut self.input, byte);
            }
            Key::Backspace => {
                self.input_box.backspace(&mut self.input);
            }
            Key::Left => self.input_box.cursor_left(&self.input),
            Key::Right => self.input_box.cursor_right(&self.input),
            Key::Escape => {
                // Escape abandons the line, as it does in every shell.
                self.input_box.clear();
            }
            Key::Enter => {
                let mut line = [0u8; INPUT_SIZE];
                let len = self.input_box.len().min(INPUT_SIZE);
                line[..len].copy_from_slice(&self.input[..len]);
                self.input_box.clear();
                self.run(&line[..len]);
            }
            // Arrows that belong to a multi-line editor, and Tab: this line
            // is one line long and has no completion to offer.
            Key::Up | Key::Down | Key::Delete | Key::Home | Key::End | Key::Tab => {}
        }
        true
    }

    /// Run one command line against the built-ins and the system services.
    fn run(&mut self, line: &[u8]) {
        self.append(line);
        self.append(b"\n");

        let command = trim(line);
        if command.is_empty() {
            return;
        }

        if command == b"help" {
            self.append(
                b"help  about  clear  echo <text>\n\
                 ls [path]  cat <path>  stat <path>\n\
                 ps  uname\n",
            );
        } else if command == b"about" {
            self.append(b"NovaOS desktop, milestone M6\n");
        } else if command == b"clear" {
            self.log_box.clear();
            self.status = "scrollback cleared";
            return;
        } else if let Some(rest) = command.strip_prefix(b"echo ") {
            self.append(rest);
            self.append(b"\n");
        } else if command == b"ls" {
            self.list(b"/");
        } else if let Some(rest) = command.strip_prefix(b"ls ") {
            self.list(trim(rest));
        } else if let Some(rest) = command.strip_prefix(b"cat ") {
            self.cat(trim(rest));
        } else if let Some(rest) = command.strip_prefix(b"stat ") {
            self.stat(trim(rest));
        } else if command == b"ps" {
            self.ps();
        } else if command == b"uname" {
            self.uname();
        } else {
            self.append(b"unknown command: ");
            self.append(command);
            self.append(b"\n");
            self.status = "unknown command";
            return;
        }
        self.status = "ready";
    }

    /// `ls [path]`: list a directory through the file service.
    fn list(&mut self, path: &[u8]) {
        let mut page_buffer = [0u8; fsclient::PAGE_BYTES];
        let mut offset = 0u32;
        loop {
            let page = match fsclient::list_at(path, offset, &mut page_buffer) {
                Ok(page) => page,
                Err(error) => {
                    self.append(b"ls: ");
                    self.append(error.name().as_bytes());
                    self.append(b"\n");
                    return;
                }
            };
            if page.count == 0 {
                break;
            }
            let mut records = fsclient::Records::new(&page_buffer[..page.bytes]);
            while let Some(record) = records.next() {
                self.append(record.name);
                if record.directory {
                    self.append(b"/");
                } else {
                    self.append(b"  ");
                    self.append_decimal(record.size);
                }
                self.append(b"\n");
            }
            offset += page.count;
            if offset >= page.total {
                break;
            }
        }
    }

    /// `cat <path>`: read a file and put it in the scrollback.
    fn cat(&mut self, path: &[u8]) {
        let mut contents = [0u8; 1024];
        match fsclient::read_all(path, &mut contents) {
            Ok(count) => {
                self.append(&contents[..count]);
                if count == 0 || contents[count - 1] != b'\n' {
                    self.append(b"\n");
                }
            }
            Err(error) => {
                self.append(b"cat: ");
                self.append(error.name().as_bytes());
                self.append(b"\n");
            }
        }
    }

    /// `stat <path>`: what the file service knows about one path.
    fn stat(&mut self, path: &[u8]) {
        match fsclient::stat(path) {
            Ok(info) => {
                self.append(if info.directory {
                    b"directory  "
                } else {
                    b"file  "
                });
                self.append_decimal(info.size);
                self.append(b" bytes\n");
            }
            Err(error) => {
                self.append(b"stat: ");
                self.append(error.name().as_bytes());
                self.append(b"\n");
            }
        }
    }

    /// `ps`: the kernel's process count and each boot service's state.
    fn ps(&mut self) {
        self.append(b"processes: ");
        self.append_decimal(process::count());
        self.append(b"\n");
        for (name, label) in SERVICES {
            self.append(label.as_bytes());
            self.append(if process::status(name) {
                b"  up\n"
            } else {
                b"  down\n"
            });
        }
    }

    /// `uname`: the kernel and ABI the session runs on.
    fn uname(&mut self) {
        self.append(b"NovaOS x86_64 (ABI ");
        self.append_decimal(u64::from(nova_abi::ABI_VERSION.major));
        self.append(b".");
        self.append_decimal(u64::from(nova_abi::ABI_VERSION.minor));
        self.append(b")\n");
    }

    /// Append a number in decimal, bounded to a scratch buffer.
    fn append_decimal(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut length = 0usize;
        // Fixed-buffer decimal, shortest last: the scrollback's own append
        // does the bounds work.
        if value == 0 {
            self.append(b"0");
            return;
        }
        let mut rest = value;
        while rest > 0 && length < digits.len() {
            digits[digits.len() - 1 - length] = b'0' + (rest % 10) as u8;
            rest /= 10;
            length += 1;
        }
        self.append(&digits[digits.len() - length..]);
    }

    /// Handle one pointer event. Returns whether the window must be
    /// redrawn.
    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        let primary = event.buttons & INPUT_BUTTON_PRIMARY != 0;
        let pressed = primary && !self.title_down;
        self.title_down = primary;

        // The title bar's buttons fire on the press edge: close quits,
        // iconify minimizes to the taskbar (the taskbar's chip brings
        // the window back through ACTIVATE).
        if pressed {
            match titlebar_hit(event.x, event.y, WIDTH as i32) {
                TitlebarHit::Close => {
                    self.quit = true;
                    return true;
                }
                TitlebarHit::Iconify => {
                    let _ = graphics::compositor_minimize(self.surface, true);
                    return false;
                }
                _ => {}
            }
        }

        let was_down = self.clear.is_down();
        let fired = self.clear.on_pointer(event.x, event.y, primary);
        if fired {
            self.log_box.clear();
            self.status = "scrollback cleared";
        }
        // A press that only arms the button still changes what it looks
        // like, so it counts as a redraw — otherwise the button would not
        // appear to go down until it fired.
        fired || was_down != self.clear.is_down()
    }

    /// Draw the whole window.
    fn draw(&self, canvas: &mut Canvas) {
        canvas.clear(palette::SURFACE);

        draw_titlebar(canvas, WIDTH as i32, "Terminal", true);

        self.log_box.draw(canvas, &self.log);

        canvas.draw_text(8, 274, ">", palette::TEXT, None);
        self.input_box.draw(canvas, &self.input);
        self.clear.draw(canvas);

        canvas.fill_rect(Rect::new(0, 296, WIDTH, HEIGHT - 296), palette::STATUS);
        canvas.draw_text(8, 300, self.status, palette::TEXT, Some(palette::STATUS));
    }
}

/// Drop leading spaces, so `  help` and `help` are the same command.
fn trim(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|&byte| byte != b' ')
        .unwrap_or(line.len());
    &line[start..]
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    // The terminal face: Sarasa Gothic, ASCII plus the GB2312 level-1
    // hanzi. Every draw from here on renders from its outlines; a parse
    // failure degrades to the bitmap font inside nova-ui.
    nova_ui::typeface::set_face(SARASA);

    let _ = console::write_str("[NOVA] terminal ready\r\n");

    let mut window = match Window::open(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ =
                console::write_fmt(format_args!("terminal: window failed {}\r\n", error.name()));
            return -1;
        }
    };
    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);

    let mut terminal = Terminal::new();
    terminal.surface = window.surface;
    let mut decoder = Decoder::new();
    let mut events = [InputEvent::EMPTY; MAX_EVENTS];

    terminal.draw(&mut canvas);
    if let Err(error) = window.present() {
        let _ = console::write_fmt(format_args!(
            "terminal: present failed {}\r\n",
            error.name()
        ));
        return -1;
    }

    loop {
        let count = window.drain(&mut events).unwrap_or(0);
        let mut dirty = false;

        for event in &events[..count] {
            match event.kind {
                // Every key event is fed to the decoder, press or release:
                // the release is what puts Shift back down. Only the keys it
                // decodes are acted on.
                INPUT_EVENT_KEY => {
                    if let Some(key) = decoder.on_input(event.kind, event.state, event.scancode) {
                        dirty |= terminal.on_key(key);
                    }
                }
                INPUT_EVENT_POINTER => dirty |= terminal.on_pointer(event),
                _ => {}
            }
        }

        if dirty {
            terminal.draw(&mut canvas);
            if let Err(error) = window.present() {
                let _ = console::write_fmt(format_args!(
                    "terminal: present failed {}\r\n",
                    error.name()
                ));
                return -1;
            }
        }

        // Yield rather than spin: the desktop's idle CPU is one of the
        // milestone's acceptance suites, and a window that polls in a tight
        // loop is exactly what it is there to catch.
        if terminal.quit {
            break;
        }

        yield_now();
    }

    0
}
