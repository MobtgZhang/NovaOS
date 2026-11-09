#![no_std]
#![no_main]

//! The nova-graphic showcase window.
//!
//! # What this proves
//!
//! `nova-graphic` ships eight controls and a layout engine. A toolkit that
//! is only unit-tested is a pile of type signatures; this window exercises
//! every control the way a person does — clicks land on widgets, typing
//! reaches the line edit, Tab walks focus, the radios are mutually
//! exclusive, the slider drives the progress bar, and the list selection is
//! reported in the status bar.
//!
//! # Layout
//!
//! The whole form is one vertical box: [`VBox`] packs the rows from the
//! window's top margin to the button row, and the VBox-computed rectangle of
//! each row positions the widgets inside it. The buttons and radios split
//! their row's width themselves.

use nova_apps::graphics::{InputEvent, Window};
use nova_apps::{console, yield_now};
use nova_graphic::{
    EchoMode, Event, EventKind, MouseButton, MouseState, QCheckBox, QLabel, QLineEdit, QListWidget,
    QProgressBar, QPushButton, QRadioButton, QSlider, Rect, VBox, Widget,
};
use nova_protocols::input::{INPUT_BUTTON_PRIMARY, INPUT_EVENT_KEY, INPUT_EVENT_POINTER};
use nova_ui::keyboard::{Decoder, Key};
use nova_ui::{Canvas, Rect as UiRect, decimal, typeface};

/// The GUI face: Source Han Sans, the face the desktop apps use.
static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

const WIDTH: u32 = 460;
const HEIGHT: u32 = 380;
const MAX_EVENTS: usize = 8;
const NAME_CAP: usize = 24;

/// Which widget currently receives the keyboard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    LineEdit,
    ShowPassword,
    Light,
    Dark,
    Volume,
    List,
    Ok,
    Cancel,
}

impl Focus {
    const ORDER: [Focus; 8] = [
        Focus::LineEdit,
        Focus::ShowPassword,
        Focus::Light,
        Focus::Dark,
        Focus::Volume,
        Focus::List,
        Focus::Ok,
        Focus::Cancel,
    ];

    fn next(self) -> Focus {
        let index = Focus::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Focus::ORDER[(index + 1) % Focus::ORDER.len()]
    }
}

struct Demo {
    name: [u8; NAME_CAP],
    name_len: usize,
    title: QLabel,
    line_edit: QLineEdit,
    show_password: QCheckBox,
    light: QRadioButton,
    dark: QRadioButton,
    volume: QSlider,
    progress: QProgressBar,
    list: QListWidget,
    ok: QPushButton,
    cancel: QPushButton,
    focus: Focus,
    prev_primary: bool,
    light_theme: bool,
    submits: u32,
    light_was_checked: bool,
    dark_was_checked: bool,
}

impl Demo {
    fn new() -> Self {
        let mut demo = Self {
            name: [0; NAME_CAP],
            name_len: 0,
            title: QLabel::new(Rect::new(0, 0, 0, 0), "nova-graphic showcase"),
            line_edit: QLineEdit::new(Rect::new(0, 0, 0, 0)),
            show_password: QCheckBox::new(Rect::new(0, 0, 440, 20), "Show password as text"),
            light: QRadioButton::new(Rect::new(0, 0, 0, 0), "Light theme"),
            dark: QRadioButton::new(Rect::new(0, 0, 0, 0), "Dark theme"),
            volume: QSlider::new(Rect::new(0, 0, 0, 0)),
            progress: QProgressBar::new(Rect::new(0, 0, 0, 0)),
            list: QListWidget::new(Rect::new(0, 0, 0, 0)),
            ok: QPushButton::new(Rect::new(0, 0, 0, 0), "OK"),
            cancel: QPushButton::new(Rect::new(0, 0, 0, 0), "Cancel"),
            focus: Focus::LineEdit,
            prev_primary: false,
            light_theme: true,
            submits: 0,
            light_was_checked: true,
            dark_was_checked: false,
        };
        demo.line_edit.echo_mode = EchoMode::Password;
        demo.line_edit.focused = true;
        demo.list.items = ["Apple", "Banana", "Cherry", "Durian", "", "", "", ""];
        demo.list.item_count = 4;
        demo.list.selected = Some(0);
        demo.light.checked = true;
        demo.layout();
        demo
    }

    /// Pack every row with the VBox, then hand each computed rectangle to
    /// its widget. Widths that must be split (radios, buttons) are divided
    /// here rather than in `VBox`, which has one child per cell.
    fn layout(&mut self) {
        // Column starts just below the chrome, ends above the status bar.
        let column = VBox::new(Rect::new(8, 30, WIDTH - 16, 0), 8);
        let heights: [u32; 8] = [20, 26, 20, 22, 26, 18, 96, 28];
        let mut rects = [Rect::new(0, 0, 0, 0); 8];
        let _ = column.layout(&heights, &mut rects);

        // Row 0: the title label.
        self.title.set_rect(rects[0]);
        // Row 1: caption "Name:" then the line edit filling the rest.
        let caption = "Name:";
        let caption_w = Canvas::text_width(caption) + 8;
        self.line_edit.set_rect(Rect::new(
            rects[1].x + caption_w as i32,
            rects[1].y,
            rects[1].w - caption_w,
            rects[1].h,
        ));
        // Row 2: checkbox.
        self.show_password.set_rect(rects[2]);
        // Row 3: radios, half each.
        let half = rects[3].w / 2;
        self.light
            .set_rect(Rect::new(rects[3].x, rects[3].y, half, rects[3].h));
        self.dark.set_rect(Rect::new(
            rects[3].x + half as i32,
            rects[3].y,
            half,
            rects[3].h,
        ));
        // Row 4: slider after a "Volume" caption.
        let vol_w = Canvas::text_width("Volume") + 8;
        self.volume.set_rect(Rect::new(
            rects[4].x + vol_w as i32,
            rects[4].y,
            rects[4].w - vol_w - 48,
            rects[4].h,
        ));
        // Row 5: progress bar.
        self.progress.set_rect(rects[5]);
        // Row 6: list.
        self.list.set_rect(rects[6]);
        // Row 7: buttons.
        self.ok
            .set_rect(Rect::new(rects[7].x, rects[7].y, 96, rects[7].h));
        self.cancel
            .set_rect(Rect::new(rects[7].x + 108, rects[7].y, 96, rects[7].h));
    }

    fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    fn insert_byte(&mut self, byte: u8) {
        if self.name_len < NAME_CAP && (byte.is_ascii_graphic() || byte == b' ') {
            self.name[self.name_len] = byte;
            self.name_len += 1;
        }
    }

    fn backspace(&mut self) {
        if self.name_len > 0 {
            self.name_len -= 1;
        }
    }

    fn enforce_radio_exclusivity(&mut self) {
        // Rising edges decide which button won the toggle fight, so clicking
        // the dark radio can actually take the light one's checked state
        // away — Qt's mutual-exclusivity semantics, done with a one-shot
        // edge detector rather than a priority rule.
        let light_edge = self.light.checked && !self.light_was_checked;
        let dark_edge = self.dark.checked && !self.dark_was_checked;
        match (light_edge, dark_edge) {
            (true, false) => {
                self.dark.checked = false;
                self.light_theme = true;
            }
            (false, true) => {
                self.light.checked = false;
                self.light_theme = false;
            }
            (true, true) => {
                // Programmatic set of both in one frame: dark wins.
                self.light.checked = false;
                self.light_theme = false;
            }
            (false, false) => {}
        }
        self.light_was_checked = self.light.checked;
        self.dark_was_checked = self.dark.checked;
    }

    /// Handle one pointer event. Returns whether the window must redraw.
    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        let primary = event.buttons & INPUT_BUTTON_PRIMARY != 0;
        let released = self.prev_primary && !primary;
        let pressed = primary && !self.prev_primary;
        self.prev_primary = primary;

        let mut dirty = false;
        let send = |widget: &mut dyn Widget, kind: EventKind| widget.on_event(&Event { kind });

        if pressed {
            let ev = EventKind::MouseButton {
                x: event.x,
                y: event.y,
                button: MouseButton::Left,
                state: MouseState::Pressed,
            };
            dirty |= send(&mut self.volume, ev);
            dirty |= send(&mut self.line_edit, ev);
        }
        if released {
            let ev = EventKind::MouseButton {
                x: event.x,
                y: event.y,
                button: MouseButton::Left,
                state: MouseState::Released,
            };
            dirty |= send(&mut self.ok, ev);
            dirty |= send(&mut self.cancel, ev);
            dirty |= send(&mut self.show_password, ev);
            dirty |= send(&mut self.light, ev);
            dirty |= send(&mut self.dark, ev);
            dirty |= send(&mut self.list, ev);
        }

        // The password echo follows the checkbox, as in any real form.
        self.line_edit.echo_mode = if self.show_password.checked {
            EchoMode::Normal
        } else {
            EchoMode::Password
        };
        self.enforce_radio_exclusivity();
        self.progress.value = self.volume.value;

        // Clicks hand the keyboard to what they land on.
        if pressed || released {
            for (target, hit) in [
                (Focus::LineEdit, self.line_edit.rect()),
                (Focus::ShowPassword, self.show_password.rect()),
                (Focus::Light, self.light.rect()),
                (Focus::Dark, self.dark.rect()),
                (Focus::Volume, self.volume.rect()),
                (Focus::List, self.list.rect()),
                (Focus::Ok, self.ok.rect()),
                (Focus::Cancel, self.cancel.rect()),
            ] {
                if hit.contains(event.x, event.y) {
                    self.focus = target;
                    dirty = true;
                    break;
                }
            }
        }

        // Fire the button actions on release.
        if released && self.ok.rect().contains(event.x, event.y) {
            self.submits = self.submits.wrapping_add(1);
            dirty = true;
        } else if released && self.cancel.rect().contains(event.x, event.y) {
            self.name_len = 0;
            dirty = true;
        }

        self.sync_focus();
        dirty
    }

    fn sync_focus(&mut self) {
        self.line_edit.focused = self.focus == Focus::LineEdit;
        self.ok.focused = self.focus == Focus::Ok;
        self.cancel.focused = self.focus == Focus::Cancel;
    }

    /// Handle one decoded keystroke.
    fn on_key(&mut self, key: Key) -> bool {
        // Tab always cycles focus, before any per-widget handling.
        if let Key::Tab = key {
            self.focus = self.focus.next();
            self.sync_focus();
            return true;
        }
        match self.focus {
            Focus::LineEdit => match key {
                Key::Char(byte) => {
                    self.insert_byte(byte);
                    true
                }
                Key::Backspace => {
                    self.backspace();
                    true
                }
                Key::Enter => {
                    self.submits = self.submits.wrapping_add(1);
                    true
                }
                Key::Escape => {
                    self.focus = Focus::Ok;
                    self.sync_focus();
                    true
                }
                _ => false,
            },
            Focus::ShowPassword => match key {
                Key::Enter | Key::Char(b' ') => {
                    self.show_password.checked = !self.show_password.checked;
                    self.line_edit.echo_mode = if self.show_password.checked {
                        EchoMode::Normal
                    } else {
                        EchoMode::Password
                    };
                    true
                }
                _ => false,
            },
            Focus::Light => match key {
                Key::Enter | Key::Char(b' ') => {
                    self.light.checked = true;
                    self.enforce_radio_exclusivity();
                    true
                }
                _ => false,
            },
            Focus::Dark => match key {
                Key::Enter | Key::Char(b' ') => {
                    self.dark.checked = true;
                    self.enforce_radio_exclusivity();
                    true
                }
                _ => false,
            },
            Focus::Volume => match key {
                Key::Left | Key::Down => {
                    self.volume.value = self.volume.value.saturating_sub(5);
                    self.progress.value = self.volume.value;
                    true
                }
                Key::Right | Key::Up => {
                    self.volume.value = (self.volume.value + 5).min(100);
                    self.progress.value = self.volume.value;
                    true
                }
                _ => false,
            },
            Focus::List => match key {
                Key::Up => {
                    let current = self.list.selected.unwrap_or(0);
                    self.list.selected = Some(current.saturating_sub(1));
                    true
                }
                Key::Down => {
                    let current = self.list.selected.unwrap_or(0);
                    self.list.selected = Some((current + 1).min(self.list.item_count - 1));
                    true
                }
                _ => false,
            },
            Focus::Ok => match key {
                Key::Enter | Key::Char(b' ') => {
                    self.submits = self.submits.wrapping_add(1);
                    true
                }
                _ => false,
            },
            Focus::Cancel => match key {
                Key::Enter | Key::Char(b' ') => {
                    self.name_len = 0;
                    true
                }
                _ => false,
            },
        }
    }

    fn draw(&self, canvas: &mut Canvas) {
        let (background, chrome, text, muted, status, status_text) = if self.light_theme {
            (
                0x00EDEFF2, 0x00D7DBE0, 0x002A2E35, 0x005A6270, 0x00D7DBE0, 0x002A2E35,
            )
        } else {
            (
                0x00232A32, 0x00182020, 0x00D9DEE8, 0x008A93A4, 0x00182020, 0x00D9DEE8,
            )
        };
        let card = 0x002E3440;

        canvas.clear(background);

        // Window chrome.
        canvas.fill_rect(UiRect::new(0, 0, WIDTH, 22), chrome);
        canvas.draw_text(8, 3, "widgets-demo", text, Some(chrome));

        // The card the controls sit on, so the form reads as one piece.
        canvas.fill_rect(UiRect::new(8, 26, WIDTH - 16, 322), card);

        // Captions, in muted text.
        canvas.draw_text(16, self.line_edit.rect().y + 4, "Name:", muted, Some(card));
        canvas.draw_text(
            self.volume.rect().x - Canvas::text_width("Volume ") as i32,
            self.volume.rect().y + 4,
            "Volume",
            muted,
            Some(card),
        );

        // The widgets, each in its VBox cell.
        self.title.draw(canvas);
        self.line_edit.draw_with_text(canvas, self.name_bytes());
        self.show_password.draw(canvas);
        self.light.draw(canvas);
        self.dark.draw(canvas);
        self.volume.draw(canvas);
        let mut vol = [0u8; 8];
        let vol_text = decimal(self.volume.value as usize, &mut vol);
        canvas.draw_text(
            self.volume.rect().x + self.volume.rect().w as i32 + 8,
            self.volume.rect().y + 4,
            vol_text,
            text,
            Some(card),
        );
        self.progress.draw(canvas);
        self.list.draw(canvas);
        self.ok.draw(canvas);
        self.cancel.draw(canvas);

        // Status bar: the live form state, always visible.
        canvas.fill_rect(UiRect::new(0, (HEIGHT - 24) as i32, WIDTH, 24), status);
        let mut line = [0u8; 160];
        let mut cursor = 0usize;
        let prefix = b"volume=";
        line[..prefix.len()].copy_from_slice(prefix);
        cursor += prefix.len();
        let mut digits = [0u8; 8];
        let d = decimal(self.volume.value as usize, &mut digits);
        line[cursor..cursor + d.len()].copy_from_slice(d.as_bytes());
        cursor += d.len();
        let mid = b"% item=";
        line[cursor..cursor + mid.len()].copy_from_slice(mid);
        cursor += mid.len();
        if let Some(index) = self.list.selected {
            let item = self.list.items[index].as_bytes();
            line[cursor..cursor + item.len()].copy_from_slice(item);
            cursor += item.len();
        }
        let theme = if self.light_theme {
            b" light".as_slice()
        } else {
            b" dark".as_slice()
        };
        line[cursor..cursor + theme.len()].copy_from_slice(theme);
        cursor += theme.len();
        let submits = b" submits=";
        line[cursor..cursor + submits.len()].copy_from_slice(submits);
        cursor += submits.len();
        let mut digits = [0u8; 8];
        let d = decimal(self.submits as usize, &mut digits);
        line[cursor..cursor + d.len()].copy_from_slice(d.as_bytes());
        cursor += d.len();
        let text = core::str::from_utf8(&line[..cursor]).unwrap_or("");
        canvas.draw_text(8, HEIGHT as i32 - 18, text, status_text, Some(status));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    typeface::set_face(HAN_SANS);

    let _ = console::write_str("[NOVA] widgets-demo ready\r\n");

    let mut window = match Window::open(WIDTH, HEIGHT) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!(
                "widgets-demo: window failed {}\r\n",
                error.name()
            ));
            return -1;
        }
    };
    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);

    let mut demo = Demo::new();
    let mut decoder = Decoder::new();
    let mut events = [InputEvent::EMPTY; MAX_EVENTS];

    demo.draw(&mut canvas);
    if let Err(error) = window.present() {
        let _ = console::write_fmt(format_args!(
            "widgets-demo: present failed {}\r\n",
            error.name()
        ));
        return -1;
    }

    loop {
        let count = window.drain(&mut events).unwrap_or(0);
        let mut dirty = false;

        for event in &events[..count] {
            match event.kind {
                INPUT_EVENT_KEY => {
                    if let Some(key) = decoder.on_input(event.kind, event.state, event.scancode) {
                        dirty |= demo.on_key(key);
                    }
                }
                INPUT_EVENT_POINTER => dirty |= demo.on_pointer(event),
                _ => {}
            }
        }

        if dirty {
            demo.draw(&mut canvas);
            if let Err(error) = window.present() {
                let _ = console::write_fmt(format_args!(
                    "widgets-demo: present failed {}\r\n",
                    error.name()
                ));
                return -1;
            }
        }

        yield_now();
    }
}
