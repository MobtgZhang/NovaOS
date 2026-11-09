#![no_std]
#![no_main]

//! The text editor window: a wrapped field, two buttons, and a file behind
//! them.
//!
//! This is the application the M6 exit gate is really about (§20.8): *input
//! text, save to RAMFS*. Everything else on the desktop can be a picture of
//! a window; this one has to take keystrokes, hold them in a buffer, and get
//! those bytes onto the filesystem through the file service — under the path
//! grant `init` handed it, which is why the file it writes is
//! `/home/nova/notes.txt` and not anywhere else.
//!
//! # Why saving truncates
//!
//! A save opens the file with `FILE_FLAG_CREATE | FILE_FLAG_TRUNCATE`. The
//! create makes it exist; the truncate empties whatever was there before the
//! first byte is written. Without it, typing a short note over a long one
//! would leave the old tail in place and the file would read back as the new
//! text with the old text still glued to the end — a save that silently
//! returns more than you wrote.
//!
//! # Why the buffer is a fixed array
//!
//! The editor's text is `[u8; 1024]` on the stack, which is exactly the
//! filesystem's maximum file size: a buffer any larger could hold text that
//! can never be written, and a smaller one could not hold a file it can read.

use nova_abi::error::ErrorCode;
use nova_apps::graphics::{InputEvent, Window};
use nova_apps::{console, fsclient, yield_now};
use nova_protocols::input::{INPUT_BUTTON_PRIMARY, INPUT_EVENT_KEY, INPUT_EVENT_POINTER};
use nova_ui::keyboard::{Decoder, Key};
use nova_ui::{Button, Canvas, Rect, TextBox, decimal, palette};

/// Window width in pixels.
/// The GUI face embedded in this program: Source Han Sans subset to ASCII,
/// CJK punctuation, and the GB2312 level-1 hanzi — 1 MiB of `.rodata`, the
/// price of showing Chinese as text.
static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

const WIDTH: u32 = 440;
/// Window height in pixels.
const HEIGHT: u32 = 340;

/// The editor's text buffer, and the filesystem's largest file.
///
/// `MAX_FILE_BYTES` in `nova-fs` is 1024. A buffer of the same size means a
/// file this editor can read, it can also write back.
const TEXT_SIZE: usize = 1024;

/// How many input events one drain may bring back.
const MAX_EVENTS: usize = 8;

/// The one file this editor reads and writes.
///
/// Under `/home`, which is the scope `init` granted it: §16.1 says a process
/// starts with no file authority, and this is the slice of `init`'s that the
/// editor was given.
const PATH: &[u8] = b"/home/nova/notes.txt";

/// The editor window.
struct Editor {
    /// The text being edited.
    text: [u8; TEXT_SIZE],
    /// The field's view onto `text`.
    field: TextBox,
    /// The button that writes the text out.
    save: Button,
    /// The button that reads the file back in.
    load: Button,
    /// What the status line says.
    status: &'static str,
    /// A count to print after `status`, when there is one.
    detail: Option<usize>,
}

impl Editor {
    /// An empty editor, which tries to load the file it will save to.
    fn new() -> Self {
        let mut editor = Self {
            text: [0; TEXT_SIZE],
            field: TextBox::new(Rect::new(8, 26, 424, 250)),
            save: Button::new(Rect::new(8, 284, 76, 24), "Save"),
            load: Button::new(Rect::new(92, 284, 76, 24), "Load"),
            status: "ready",
            detail: None,
        };
        // One editable control, so it holds the keys from the moment the
        // window opens rather than waiting to be clicked.
        editor.field.focused = true;
        editor.load_file();
        editor
    }

    /// Handle one decoded keystroke. Returns whether the window must be
    /// redrawn.
    fn on_key(&mut self, key: Key) -> bool {
        match key {
            Key::Char(byte) => {
                self.field.insert(&mut self.text, byte);
            }
            Key::Backspace => {
                self.field.backspace(&mut self.text);
            }
            Key::Left => self.field.cursor_left(&self.text),
            Key::Right => self.field.cursor_right(&self.text),
            // Enter is the one editing key that is also a character: the
            // field lays a newline out as the start of a new row.
            Key::Enter => {
                self.field.insert(&mut self.text, b'\n');
            }
            // Rows, columns and forward-delete all need the field's own
            // layout — where a line wraps depends on the window's width —
            // and the field keeps that to itself rather than exposing a
            // second copy of it for the application to disagree with.
            Key::Up | Key::Down | Key::Delete | Key::Home | Key::End | Key::Escape | Key::Tab => {}
        }
        true
    }

    /// Handle one pointer event. Returns whether the window must be redrawn.
    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        let primary = event.buttons & INPUT_BUTTON_PRIMARY != 0;
        let before = (self.save.is_down(), self.load.is_down());

        let mut fired = false;
        if self.save.on_pointer(event.x, event.y, primary) {
            self.save_file();
            fired = true;
        }
        if self.load.on_pointer(event.x, event.y, primary) {
            self.load_file();
            fired = true;
        }

        // A press that only arms a button still changes what it looks like,
        // so it counts as a redraw — otherwise the button would not appear
        // to go down until it fired.
        fired || before != (self.save.is_down(), self.load.is_down())
    }

    /// Write the buffer out, replacing the file's previous contents.
    fn save_file(&mut self) {
        let len = self.field.len().min(TEXT_SIZE);
        let result = fsclient::save(PATH, &self.text[..len]);
        self.status = "saved ";
        self.detail = None;
        match result {
            Ok(count) => self.detail = Some(count),
            Err(ErrorCode::Denied) => self.status = "save denied: no write grant ",
            Err(ErrorCode::Quota) => self.status = "save failed: the filesystem is full ",
            Err(ErrorCode::Unsupported) => {
                self.status = "save failed: this filesystem cannot truncate "
            }
            Err(_) => self.status = "save failed ",
        }
    }

    /// Read the file into the buffer, replacing whatever was there.
    fn load_file(&mut self) {
        let result = fsclient::read_all(PATH, &mut self.text);
        self.status = "loaded ";
        self.detail = None;
        match result {
            Ok(count) => {
                // The field's length, not a `set_text`: the bytes are
                // already in `text`, and copying them onto themselves would
                // be the only thing the copy achieved.
                self.field.set_len(count);
                self.detail = Some(count);
            }
            Err(ErrorCode::Denied) => self.status = "load denied: no read grant ",
            // A file that is not there yet is not an error the user needs
            // to see: it is what a new document starts as.
            Err(ErrorCode::Invalid) => self.status = "new file ",
            Err(_) => self.status = "load failed ",
        }
    }

    /// Draw the whole window.
    fn draw(&self, canvas: &mut Canvas) {
        canvas.clear(palette::SURFACE);

        canvas.fill_rect(Rect::new(0, 0, WIDTH, 22), palette::CHROME);
        canvas.draw_text(
            8,
            3,
            "Editor  /home/nova/notes.txt",
            palette::TEXT_ON_CHROME,
            Some(palette::CHROME),
        );

        self.field.draw(canvas, &self.text);
        self.save.draw(canvas);
        self.load.draw(canvas);

        canvas.fill_rect(Rect::new(0, 312, WIDTH, HEIGHT - 312), palette::STATUS);
        canvas.draw_text(
            8,
            318,
            self.status,
            palette::TEXT_ON_CHROME,
            Some(palette::STATUS),
        );
        if let Some(value) = self.detail {
            let mut digits = [0u8; 20];
            let count = decimal(value, &mut digits);
            let after_status = 8 + Canvas::text_width(self.status) as i32;
            canvas.draw_text(
                after_status,
                318,
                count,
                palette::TEXT_ON_CHROME,
                Some(palette::STATUS),
            );
            canvas.draw_text(
                after_status + Canvas::text_width(count) as i32,
                318,
                " bytes",
                palette::TEXT_ON_CHROME,
                Some(palette::STATUS),
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    // The GUI face: Source Han Sans, ASCII plus the GB2312 level-1 hanzi, so
    // a file's Chinese text is text on screen rather than boxes. Every later
    // draw goes through the face; a failure to parse degrades to the bitmap
    // font inside nova-ui, so this call cannot fail the startup.
    nova_ui::typeface::set_face(HAN_SANS);

    let _ = console::write_str("[NOVA] editor ready\r\n");

    let mut window = match Window::open(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!("editor: window failed {}\r\n", error.name()));
            return -1;
        }
    };
    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);

    let mut editor = Editor::new();
    let mut decoder = Decoder::new();
    let mut events = [InputEvent::EMPTY; MAX_EVENTS];

    editor.draw(&mut canvas);
    if let Err(error) = window.present() {
        let _ = console::write_fmt(format_args!("editor: present failed {}\r\n", error.name()));
        return -1;
    }

    loop {
        let count = window.drain(&mut events).unwrap_or(0);
        let mut dirty = false;

        for event in &events[..count] {
            match event.kind {
                // Every key event reaches the decoder, press or release:
                // the release is what puts Shift back down. Only the keys it
                // decodes are acted on.
                INPUT_EVENT_KEY => {
                    if let Some(key) = decoder.on_input(event.kind, event.state, event.scancode) {
                        dirty |= editor.on_key(key);
                    }
                }
                INPUT_EVENT_POINTER => dirty |= editor.on_pointer(event),
                _ => {}
            }
        }

        if dirty {
            editor.draw(&mut canvas);
            if let Err(error) = window.present() {
                let _ =
                    console::write_fmt(format_args!("editor: present failed {}\r\n", error.name()));
                return -1;
            }
        }

        // Yield rather than spin: the desktop's idle CPU is one of the
        // milestone's acceptance suites, and a window that polls in a tight
        // loop is exactly what it is there to catch.
        yield_now();
    }
}
