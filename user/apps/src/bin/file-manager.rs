#![no_std]
#![no_main]

//! The file manager window: a path, a listing, and a click to walk down it.
//!
//! §20.8 asks for a "文件管理器初版" — a first version — and the honest
//! first version of a file manager is a *browser*, not an editor: it reads
//! directories through the file service and shows what is in them. Every
//! listing here is a `list_at` under the `/` read grant `init` handed this
//! process, so the window can only ever see paths its parent could see.
//!
//! # Why the listing is paged rather than fetched whole
//!
//! A directory can hold more children than one 64-byte inline reply can
//! carry, and the reply budget is the protocol's, not this program's. The
//! listing therefore arrives as pages of records — [`fsclient::list_at`]
//! with a moving offset — and the rows below fill up as the pages come in.
//! Walking with an offset rather than asking for "everything" is also what
//! makes the walk bounded: a directory that grew between two pages still
//! yields a coherent list, because each page names the index it starts at.
//!
//! # Why the path is a fixed buffer
//!
//! The path is `[u8; fsclient::MAX_PATH]` — 48 bytes, the longest path the
//! wire can carry — and descending into a child that would overrun it is
//! *refused* rather than truncated. A truncated path names a different
//! directory, and a file manager that lists the wrong one is worse than one
//! that says "too deep".

use nova_abi::error::ErrorCode;
use nova_apps::graphics::{InputEvent, Window};
use nova_apps::{console, fsclient, yield_now};
use nova_protocols::input::{INPUT_BUTTON_PRIMARY, INPUT_EVENT_KEY, INPUT_EVENT_POINTER};
use nova_ui::font::GLYPH_WIDTH;
use nova_ui::keyboard::{Decoder, Key};
use nova_ui::{Button, Canvas, Rect, colour, decimal, palette};

/// Window width in pixels.
/// The GUI face embedded in this program: Source Han Sans subset to ASCII,
/// CJK punctuation, and the GB2312 level-1 hanzi, so a Chinese file name in
/// the list is text rather than boxes.
static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

const WIDTH: u32 = 440;
/// Window height in pixels.
const HEIGHT: u32 = 380;

/// Height of the title bar and of a list row.
///
/// A row is exactly one glyph tall, so a name is drawn at the row's own `y`
/// and needs no vertical arithmetic to sit inside it.
const TITLE_HEIGHT: u32 = 22;
/// The top of the first list row.
const LIST_TOP: i32 = 50;
/// The height of one list row, in pixels.
const ROW_HEIGHT: i32 = 16;
/// The top of the status bar.
const STATUS_TOP: i32 = 348;
/// The height of the status bar, in pixels.
const STATUS_HEIGHT: u32 = HEIGHT - STATUS_TOP as u32;

/// Rows the window draws, and the most entries it holds.
///
/// `nova-fs`'s `MAX_CHILDREN` is 16, so this is not a display limit that
/// hides entries — it is the filesystem's own bound, matched so the window
/// never needs to scroll to see everything a directory can contain.
const MAX_ENTRIES: usize = 16;
/// Pages of records one reload may fetch.
///
/// Every page that returns records advances the offset by at least one, and a
/// page that returns none ends the walk, so 16 pages is a backstop against a
/// service that misreports rather than a limit the listing can reach.
const MAX_PAGES: usize = 16;

/// Bytes of a stored entry name.
///
/// Long enough for every name `nova-fs` can hold (its directory entries carry
/// up to 64 bytes), because a *display* truncation would also be a *path*
/// truncation: the name shown is the name descended into.
const NAME_SIZE: usize = 64;
/// Columns a name may occupy before the size column begins.
///
/// The name starts at x=26 and the size column is right-aligned at x=432, so
/// 46 columns end at x=394 — clear of the widest size label the window draws.
const NAME_COLUMNS: usize = 46;

/// Bytes the status line holds.
const STATUS_SIZE: usize = 64;

/// How many input events one drain may bring back.
///
/// Bounded because the service's own queue is bounded: a drain that looped
/// until empty would let a flood of events turn one turn of the event loop
/// into an unbounded amount of work.
const MAX_EVENTS: usize = 8;

/// One row of the listing: what it is called, and what it is.
#[derive(Clone, Copy)]
#[repr(C)]
struct Entry {
    /// The child's name, as the directory holds it; `len` bytes are live.
    name: [u8; NAME_SIZE],
    /// How many of `name` are live.
    len: usize,
    /// Whether the entry is a directory — whether clicking it descends.
    directory: bool,
    /// Its size in bytes; zero for a directory.
    size: u64,
}

impl Entry {
    /// An empty row, for filling the array before any listing arrives.
    const EMPTY: Entry = Entry {
        name: [0; NAME_SIZE],
        len: 0,
        directory: false,
        size: 0,
    };
}

/// The file manager window.
struct FileManager {
    /// The directory being shown, always absolute and always rooted at `/`.
    path: [u8; fsclient::MAX_PATH],
    /// How many bytes of `path` are live.
    path_len: usize,
    /// The rows, filled left to right by [`FileManager::reload`].
    entries: [Entry; MAX_ENTRIES],
    /// How many of `entries` are live.
    count: usize,
    /// How many children the directory has in total, however many fitted.
    total: usize,
    /// The row the keyboard would act on, if any.
    selected: Option<usize>,
    /// The button that walks to the parent directory.
    up: Button,
    /// The button that asks for the listing again.
    refresh: Button,
    /// The status line's live bytes.
    status: [u8; STATUS_SIZE],
    /// How many of `status` are live.
    status_len: usize,
    /// Whether the primary button was down for the previous pointer event.
    ///
    /// Tracked here because a *row* activates on the press while a *button*
    /// activates on the release, and only the edge between two events tells
    /// them apart: a held button that merely moves is not a new click.
    primary_down: bool,
}

impl FileManager {
    /// A file manager over `/`, already listed.
    fn new() -> Self {
        let mut manager = Self {
            path: [0; fsclient::MAX_PATH],
            path_len: 1,
            entries: [Entry::EMPTY; MAX_ENTRIES],
            count: 0,
            total: 0,
            selected: None,
            up: Button::new(Rect::new(8, 312, 60, 24), "Up"),
            refresh: Button::new(Rect::new(76, 312, 76, 24), "Refresh"),
            status: [0; STATUS_SIZE],
            status_len: 0,
            primary_down: false,
        };
        manager.path[0] = b'/';
        manager.reload();
        manager
    }

    /// Rebuild the status line from a prefix, some numbers, and a suffix.
    ///
    /// The numbers are joined with `" of "` when there is more than one, which
    /// is the shape every count here wants: `"showing 16 of 16 entries"`. The
    /// whole line is bytes rather than a `&str` so a name read out of a
    /// directory record — which is bytes until someone says otherwise — can be
    /// appended without first being claimed as UTF-8.
    fn set_status(&mut self, prefix: &str, numbers: &[usize], suffix: &str) {
        let mut len = 0usize;
        let mut digits = [0u8; 20];
        append(&mut self.status, &mut len, prefix.as_bytes());
        for (index, value) in numbers.iter().enumerate() {
            if index > 0 {
                append(&mut self.status, &mut len, b" of ");
            }
            let text = decimal(*value, &mut digits);
            append(&mut self.status, &mut len, text.as_bytes());
        }
        append(&mut self.status, &mut len, suffix.as_bytes());
        self.status_len = len;
    }

    /// Ask the service for the listing of the current path, from scratch.
    ///
    /// Every failure path leaves the window with fewer rows than it started
    /// with and a status line that says why, rather than keeping a stale
    /// listing on screen under a path bar that no longer matches it.
    fn reload(&mut self) {
        self.count = 0;
        self.total = 0;
        self.selected = None;

        let mut page = [0u8; fsclient::PAGE_BYTES];
        let mut offset = 0u32;
        for _ in 0..MAX_PAGES {
            // The path borrow ends with this block: what comes back is an
            // owned `Result`, so the `Err` arm below can take `&mut self`.
            let outcome = {
                let path = &self.path[..self.path_len];
                fsclient::list_at(path, offset, &mut page)
            };
            let info = match outcome {
                Ok(info) => info,
                Err(error) => {
                    self.set_status(error_status(error), &[], "");
                    return;
                }
            };

            self.total = info.total as usize;
            if info.count == 0 {
                // Either the directory is spent, or its next record is too
                // long for one page. Both mean: stop walking, and let the
                // `count`/`total` pair below say how much arrived.
                break;
            }

            let mut records = fsclient::Records::new(&page[..info.bytes]);
            for _ in 0..info.count as usize {
                if self.count >= MAX_ENTRIES {
                    break;
                }
                let Some(record) = records.next() else {
                    break;
                };
                let len = record.name.len().min(NAME_SIZE);
                let mut entry = Entry {
                    name: [0; NAME_SIZE],
                    len,
                    directory: record.directory,
                    size: record.size,
                };
                // Truncated only if the service sent a name longer than its
                // own directory entries can hold, which would be a service
                // bug — and one that shows up as a missing row, not a
                // descended-into-wrong-directory.
                entry.name[..len].copy_from_slice(&record.name[..len]);
                self.entries[self.count] = entry;
                self.count += 1;
            }

            offset = offset.saturating_add(info.count);
            if self.count >= MAX_ENTRIES {
                break;
            }
        }

        self.set_status("showing ", &[self.count, self.total], " entries");
    }

    /// Walk to the parent directory.
    fn go_up(&mut self) {
        if self.path_len <= 1 {
            self.set_status("already at the root", &[], "");
            return;
        }
        // Cut just before the last separator: `/home/nova` becomes `/home`,
        // and `/home` becomes `/` — the one case where the cut lands on the
        // leading slash and the path must keep that byte.
        let position = self.path[..self.path_len]
            .iter()
            .rposition(|&byte| byte == b'/');
        let cut = match position {
            Some(0) | None => 1,
            Some(index) => index,
        };
        self.path_len = cut;
        self.reload();
    }

    /// Walk into `entry`, refusing a path that would not fit the wire.
    fn descend(&mut self, entry: &Entry) {
        let mut next = [0u8; fsclient::MAX_PATH];
        let Some(len) = join(
            &self.path[..self.path_len],
            &entry.name[..entry.len],
            &mut next,
        ) else {
            self.set_status("too deep to follow", &[], "");
            return;
        };
        self.path[..len].copy_from_slice(&next[..len]);
        self.path_len = len;
        self.reload();
    }

    /// Act on a row: descend if it is a directory, otherwise select it.
    fn activate(&mut self, index: usize) {
        if index >= self.count {
            return;
        }
        let entry = self.entries[index];
        if entry.directory {
            self.descend(&entry);
        } else {
            self.select(Some(index));
        }
    }

    /// Change the selection, and report whether anything changed.
    fn select(&mut self, index: Option<usize>) -> bool {
        if self.selected == index {
            return false;
        }
        self.selected = index;
        match index {
            Some(i) if i < self.count => {
                let entry = self.entries[i];
                let name = core::str::from_utf8(&entry.name[..entry.len]).unwrap_or("?");
                self.set_status("selected ", &[], name);
            }
            _ => self.set_status("no selection", &[], ""),
        }
        true
    }

    /// Move the selection one row, clamped to the listing.
    fn move_selection(&mut self, delta: i32) -> bool {
        if self.count == 0 {
            return self.select(None);
        }
        let last = (self.count - 1) as i32;
        let current = match self.selected {
            Some(index) => index as i32,
            // No selection yet: the first arrow picks an end, and which end
            // depends on which way it was pressed.
            None if delta < 0 => last,
            None => 0,
        };
        let next = (current + delta).clamp(0, last) as usize;
        self.select(Some(next))
    }

    /// Handle one decoded keystroke. Returns whether the window must be
    /// redrawn.
    ///
    /// The keyboard drives the *selection* and the walk, not a text field:
    /// this window has nothing to type into, so a character keystroke is
    /// ignored rather than echoed somewhere it does not belong.
    fn on_key(&mut self, key: Key) -> bool {
        match key {
            Key::Up => self.move_selection(-1),
            Key::Down => self.move_selection(1),
            Key::Enter => match self.selected {
                Some(index) => {
                    self.activate(index);
                    true
                }
                None => false,
            },
            Key::Backspace => {
                self.go_up();
                true
            }
            Key::Escape => self.select(None),
            Key::Char(_)
            | Key::Left
            | Key::Right
            | Key::Delete
            | Key::Home
            | Key::End
            | Key::Tab => false,
        }
    }

    /// Handle one pointer event. Returns whether the window must be redrawn.
    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        let primary = event.buttons & INPUT_BUTTON_PRIMARY != 0;
        let pressed = primary && !self.primary_down;
        self.primary_down = primary;

        let before = (self.up.is_down(), self.refresh.is_down());

        let mut fired = false;
        if self.up.on_pointer(event.x, event.y, primary) {
            self.go_up();
            fired = true;
        }
        if self.refresh.on_pointer(event.x, event.y, primary) {
            self.reload();
            fired = true;
        }

        // Rows activate on the press, so that a click which ends outside the
        // window still did what the user pointed at — and so that a row can
        // be distinguished from a button, which deliberately waits for the
        // release to give a chance to change one's mind.
        if pressed {
            if let Some(row) = self.row_at(event.y) {
                self.activate(row);
                fired = true;
            }
        }

        // A press that only arms a button still changes what it looks like,
        // so it counts as a redraw — otherwise the button would not appear
        // to go down until it fired.
        fired || before != (self.up.is_down(), self.refresh.is_down())
    }

    /// The row a pointer at `y` lands in, if it lands on one that exists.
    fn row_at(&self, y: i32) -> Option<usize> {
        if y < LIST_TOP {
            return None;
        }
        let row = ((y - LIST_TOP) / ROW_HEIGHT) as usize;
        if row >= self.count {
            return None;
        }
        Some(row)
    }

    /// Draw the whole window.
    fn draw(&self, canvas: &mut Canvas) {
        canvas.clear(palette::SURFACE);

        canvas.fill_rect(Rect::new(0, 0, WIDTH, TITLE_HEIGHT), palette::CHROME);
        canvas.draw_text(
            8,
            3,
            "File Manager",
            palette::TEXT_ON_CHROME,
            Some(palette::CHROME),
        );

        let bar = Rect::new(8, 26, WIDTH - 16, 20);
        canvas.fill_rect(bar, palette::FIELD);
        canvas.stroke_rect(bar, palette::BORDER);
        canvas.draw_text(
            12,
            28,
            core::str::from_utf8(&self.path[..self.path_len]).unwrap_or("/"),
            palette::TEXT,
            Some(palette::FIELD),
        );

        for index in 0..self.count {
            let entry = self.entries[index];
            let y = LIST_TOP + (index as i32) * ROW_HEIGHT;
            let background = if self.selected == Some(index) {
                palette::SELECTED
            } else {
                palette::SURFACE
            };
            canvas.fill_rect(Rect::new(8, y, WIDTH - 16, ROW_HEIGHT as u32), background);

            // A folder and a file are told apart by the little block at the
            // left of the row: colour alone would say nothing to anyone who
            // cannot separate these two hues.
            let glyph = if entry.directory {
                colour(226, 188, 96)
            } else {
                colour(148, 164, 190)
            };
            canvas.fill_rect(Rect::new(12, y + 2, 8, ROW_HEIGHT as u32 - 4), glyph);

            draw_name(
                canvas,
                26,
                y,
                &entry.name[..entry.len],
                NAME_COLUMNS,
                palette::TEXT,
                Some(background),
            );

            let mut digits = [0u8; 16];
            let label: &str = if entry.directory {
                "DIR"
            } else {
                decimal(entry.size as usize, &mut digits)
            };
            let width = Canvas::text_width(label) as i32;
            canvas.draw_text(432 - width, y, label, palette::TEXT, Some(background));
        }

        self.up.draw(canvas);
        self.refresh.draw(canvas);

        canvas.fill_rect(
            Rect::new(0, STATUS_TOP, WIDTH, STATUS_HEIGHT),
            palette::STATUS,
        );
        canvas.draw_text(
            8,
            STATUS_TOP + 8,
            core::str::from_utf8(&self.status[..self.status_len]).unwrap_or(""),
            palette::TEXT_ON_CHROME,
            Some(palette::STATUS),
        );
    }
}

/// Append `text` to `out` at `len`, stopping at the end of `out`.
fn append(out: &mut [u8], len: &mut usize, text: &[u8]) {
    for &byte in text {
        if *len >= out.len() {
            break;
        }
        out[*len] = byte;
        *len += 1;
    }
}

/// Build `current/name` into `out`, or `None` if it would not fit.
///
/// A `current` that already ends in `/` — the root does — does not get a
/// second one: `//home` is a path no canonicaliser should have to repair.
fn join(current: &[u8], name: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut len = current.len();
    if out.len() < len.saturating_add(1).saturating_add(name.len()) {
        return None;
    }
    out[..len].copy_from_slice(current);
    if len == 0 || out[len - 1] != b'/' {
        out[len] = b'/';
        len += 1;
    }
    out[len..len + name.len()].copy_from_slice(name);
    Some(len + name.len())
}

/// Draw `name` as at most `columns` glyphs, starting at `x`.
///
/// Clipped here rather than by [`Canvas::draw_text`] because a name has to
/// stop *before* the size column, not at the edge of the window — and a name
/// that ran into the sizes would make both unreadable.
fn draw_name(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    name: &[u8],
    columns: usize,
    foreground: u32,
    background: Option<u32>,
) {
    let mut column = 0usize;
    for &byte in name {
        if column >= columns {
            break;
        }
        canvas.draw_glyph(
            x + (column as i32) * GLYPH_WIDTH as i32,
            y,
            byte,
            foreground,
            background,
        );
        column += 1;
    }
}

/// What the status line says about a file-service failure.
fn error_status(error: ErrorCode) -> &'static str {
    match error {
        ErrorCode::Denied => "denied: outside this window's grant",
        ErrorCode::Invalid => "no such path",
        ErrorCode::Quota => "the filesystem is full",
        ErrorCode::Unsupported => "this filesystem cannot do that",
        ErrorCode::Busy => "the filesystem is busy",
        ErrorCode::Closed => "the file service is gone",
        _ => "listing failed",
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    // The GUI face, as the editor uses it: one selection, and every later
    // draw renders from the outlines.
    nova_ui::typeface::set_face(HAN_SANS);

    let _ = console::write_str("[NOVA] file-manager ready\r\n");

    let mut window = match Window::open(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!(
                "file-manager: window failed {}\r\n",
                error.name()
            ));
            return -1;
        }
    };
    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);

    let mut manager = FileManager::new();
    let mut decoder = Decoder::new();
    let mut events = [InputEvent::EMPTY; MAX_EVENTS];

    manager.draw(&mut canvas);
    if let Err(error) = window.present() {
        let _ = console::write_fmt(format_args!(
            "file-manager: present failed {}\r\n",
            error.name()
        ));
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
                        dirty |= manager.on_key(key);
                    }
                }
                INPUT_EVENT_POINTER => dirty |= manager.on_pointer(event),
                _ => {}
            }
        }

        if dirty {
            manager.draw(&mut canvas);
            if let Err(error) = window.present() {
                let _ = console::write_fmt(format_args!(
                    "file-manager: present failed {}\r\n",
                    error.name()
                ));
                return -1;
            }
        }

        // Yield rather than spin: the desktop's idle CPU is one of the
        // milestone's acceptance suites, and a window that polls in a tight
        // loop is exactly what it is there to catch.
        yield_now();
    }
}
