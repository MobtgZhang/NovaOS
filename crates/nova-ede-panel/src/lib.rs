//! Rust port of `ede-panel`, EDE's desktop panel — NovaOS edition.
//!
//! The panel is a single row of applets along the bottom of the screen.
//! Left-aligned applets run from the left edge; right-aligned applets from
//! the right edge. This port keeps EDE's applet set that fits the NovaOS
//! model — `start-menu`, `quick-launch`, `taskbar`, `systray` — and draws
//! everything with plain rectangles and the embedded vector typeface, with
//! no icon assets anywhere.
//!
//! # NovaOS's own character
//!
//! The port keeps EDE's workflow exactly — icons, start menu, applets,
//! `.desktop`-driven applications — but gives the chrome NovaOS's own
//! identity: the deep-sea backdrop and the cyan-on-slate palette instead of
//! EDE's grey-blue, and a comet emblem in place of EDE's swirl. The colours
//! below are the whole theme; there is no theme file to win or lose.
//!
//! # What this crate owns
//!
//! Pure drawing policy: where each applet sits, what it draws, and which
//! one a click lands on. It owns no window; the shell drives it with a
//! canvas and turns the clicks into `Action`s.
//!
//! # The applets
//!
//! - `start-menu` — a comet-emblem button with the `Nova` wordmark; clicking
//!   it asks the shell to open the launcher popup.
//! - `quick-launch` — three fixed launch glyphs (terminal, file manager,
//!   editor), each a single click away from an application.
//! - `taskbar` — one chip per pinned application; clicking one launches a
//!   fresh instance (NovaOS has no window-list IPC yet, so the chip is an
//!   honest launcher, not a fake task switcher).
//! - `systray` — one slot reserved for the notification daemon; a dot
//!   appears when there is an unread balloon, nothing otherwise.

#![no_std]
#![forbid(unsafe_code)]

use nova_ui::{Canvas, Rect};

/// The panel's height in pixels.
pub const PANEL_HEIGHT: u32 = 32;

/// The panel's canvas coordinates as an i32.
pub const PANEL_HEIGHT_I32: i32 = PANEL_HEIGHT as i32;

/// The bar's background.
pub const BAR_BG: u32 = 0x00131C28;
/// The bar's separator line.
pub const BAR_LINE: u32 = 0x001F3142;
/// Chip resting fill.
pub const CHIP_BG: u32 = 0x001B2836;
/// Chip outline.
pub const CHIP_EDGE: u32 = 0x00243442;
/// NovaOS accent colour.
pub const ACCENT: u32 = 0x0068AAC4;
/// Amber accent for attention-getting elements.
pub const AMBER: u32 = 0x00D8A24A;
/// Dim accent for the comet tail.
pub const ACCENT_DIM: u32 = 0x00406077;
/// Text colour on the bar.
pub const TEXT: u32 = 0x00E6ECF2;
/// Muted text colour on the bar.
pub const MUTED: u32 = 0x007E8896;

/// The action a click on the panel can produce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Open the start menu.
    OpenStartMenu,
    /// Launch an application by name.
    Launch(&'static str),
}

/// A launchable chip in the panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chip {
    /// The label drawn on the chip.
    pub label: &'static str,
    /// The command the chip launches.
    pub command: &'static str,
    /// Where the chip sits.
    pub bounds: Rect,
}

/// The quick-launch set: the three most-used applications as pure glyphs.
pub const QUICK_LAUNCH: &[Chip] = &[
    Chip {
        label: "Terminal",
        command: "terminal",
        bounds: Rect::new(0, 0, 0, 0),
    },
    Chip {
        label: "Files",
        command: "file-manager",
        bounds: Rect::new(0, 0, 0, 0),
    },
    Chip {
        label: "Editor",
        command: "editor",
        bounds: Rect::new(0, 0, 0, 0),
    },
];

/// The pinned taskbar entries.
pub const TASKBAR: &[Chip] = &[
    Chip {
        label: "Terminal",
        command: "terminal",
        bounds: Rect::new(0, 0, 0, 0),
    },
    Chip {
        label: "Files",
        command: "file-manager",
        bounds: Rect::new(0, 0, 0, 0),
    },
    Chip {
        label: "Editor",
        command: "editor",
        bounds: Rect::new(0, 0, 0, 0),
    },
    Chip {
        label: "Widgets",
        command: "widgets-demo",
        bounds: Rect::new(0, 0, 0, 0),
    },
];

/// The width of the start button, from the left edge.
pub const START_WIDTH: i32 = 80;

/// The width of the tray slot, at the right edge.
pub const TRAY_WIDTH: i32 = 88;

/// A click target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    /// The start-menu button.
    StartMenu,
    /// A quick-launch chip at this index.
    QuickLaunch(usize),
    /// A taskbar chip at this index.
    Taskbar(usize),
    /// Nothing was hit.
    Nothing,
}

/// The panel's state.
#[derive(Clone, Copy)]
pub struct Panel {
    width: u32,
    quick_launch: [Chip; 3],
    taskbar: [Chip; 6],
    start_bounds: Rect,
    tray_bounds: Rect,
    chips_placed: bool,
}

impl Panel {
    /// A panel serving a surface `width` pixels wide.
    pub const fn new(width: u32) -> Self {
        const EMPTY: Chip = Chip {
            label: "",
            command: "",
            bounds: Rect::new(0, 0, 0, 0),
        };
        Self {
            width,
            quick_launch: [EMPTY; 3],
            taskbar: [EMPTY; 6],
            start_bounds: Rect::new(0, 0, START_WIDTH as u32, PANEL_HEIGHT),
            tray_bounds: Rect::new(0, 0, TRAY_WIDTH as u32, PANEL_HEIGHT),
            chips_placed: false,
        }
    }

    /// Lay the chips out for the current width. Called once per draw.
    fn place(&mut self) {
        if self.chips_placed {
            return;
        }
        let mut x = START_WIDTH + 8;
        // Quick launch: icon-only chips, 40 px wide.
        for (index, source) in QUICK_LAUNCH.iter().enumerate() {
            let mut chip = *source;
            chip.bounds = Rect::new(x, 0, 40, PANEL_HEIGHT);
            x += 46;
            self.quick_launch[index] = chip;
        }
        // Taskbar: text chips, ~96 px wide, after a gap.
        x += 12;
        for (index, source) in TASKBAR.iter().enumerate() {
            let mut chip = *source;
            chip.bounds = Rect::new(x, 0, 96, PANEL_HEIGHT);
            x += 104;
            self.taskbar[index] = chip;
        }
        // Tray: pinned to the right edge.
        self.tray_bounds = Rect::new(
            self.width as i32 - TRAY_WIDTH,
            0,
            TRAY_WIDTH as u32,
            PANEL_HEIGHT,
        );
        self.chips_placed = true;
    }

    /// Draw the panel into the canvas at `top`.
    pub fn draw(&mut self, canvas: &mut Canvas, top: i32, tray_dot: bool) {
        self.place();
        // Bar background + separator.
        canvas.fill_rect(Rect::new(0, top, self.width, PANEL_HEIGHT), BAR_BG);
        canvas.fill_rect(Rect::new(0, top, self.width, 1), BAR_LINE);

        // Start button: a comet emblem and the "Nova" wordmark.
        canvas.fill_rect(
            Rect::new(2, top + 2, (START_WIDTH - 4) as u32, PANEL_HEIGHT - 4),
            CHIP_BG,
        );
        canvas.stroke_rect(
            Rect::new(2, top + 2, (START_WIDTH - 4) as u32, PANEL_HEIGHT - 4),
            CHIP_EDGE,
        );
        draw_comet(canvas, 8, top + 6, ACCENT);
        canvas.draw_text(28, top + 8, "Nova", TEXT, Some(CHIP_BG));

        // Quick launch: square chips with glyphs.
        for chip in self.quick_launch.iter() {
            canvas.fill_rect(
                Rect::new(chip.bounds.x + 2, top + 4, 36, PANEL_HEIGHT - 8),
                CHIP_BG,
            );
            glyph(canvas, chip.command, chip.bounds.x + 20, top + 16);
        }

        // Taskbar: chips with an accent dot and label.
        for chip in self.taskbar.iter() {
            if chip.command.is_empty() {
                continue;
            }
            canvas.fill_rect(
                Rect::new(chip.bounds.x + 2, top + 4, 92, PANEL_HEIGHT - 8),
                CHIP_BG,
            );
            canvas.stroke_rect(
                Rect::new(chip.bounds.x + 2, top + 4, 92, PANEL_HEIGHT - 8),
                CHIP_EDGE,
            );
            canvas.fill_rect(Rect::new(chip.bounds.x + 8, top + 12, 4, 8), ACCENT);
            canvas.draw_text(chip.bounds.x + 16, top + 9, chip.label, TEXT, Some(CHIP_BG));
        }

        // Right segment: a comet mark + the edition tag.
        canvas.fill_rect(
            Rect::new(
                self.tray_bounds.x + 6,
                top + 4,
                (TRAY_WIDTH - 12) as u32,
                PANEL_HEIGHT - 8,
            ),
            CHIP_BG,
        );
        draw_comet(canvas, self.tray_bounds.x + 10, top + 9, AMBER);
        canvas.draw_text(
            self.tray_bounds.x + 28,
            top + 9,
            "EDE",
            AMBER,
            Some(CHIP_BG),
        );
        if tray_dot {
            canvas.fill_rect(
                Rect::new(self.tray_bounds.x + TRAY_WIDTH - 14, top + 4, 8, 8),
                AMBER,
            );
        }
    }

    /// Hit-test a panel-local point.
    pub fn hit(&mut self, x: i32, y: i32) -> Hit {
        self.place();
        if !(0..PANEL_HEIGHT_I32).contains(&y) {
            return Hit::Nothing;
        }
        let start_right = self.start_bounds.x + self.start_bounds.width as i32;
        if x >= self.start_bounds.x && x < start_right {
            return Hit::StartMenu;
        }
        for (index, chip) in self.quick_launch.iter().enumerate() {
            let right = chip.bounds.x + chip.bounds.width as i32;
            if chip.bounds.width > 0 && x >= chip.bounds.x && x < right {
                return Hit::QuickLaunch(index);
            }
        }
        for (index, chip) in self.taskbar.iter().enumerate() {
            let right = chip.bounds.x + chip.bounds.width as i32;
            if chip.bounds.width > 0 && x >= chip.bounds.x && x < right {
                return Hit::Taskbar(index);
            }
        }
        Hit::Nothing
    }

    /// Resolve a hit to an action.
    pub fn action_for(&mut self, hit: Hit) -> Option<Action> {
        match hit {
            Hit::StartMenu => Some(Action::OpenStartMenu),
            Hit::QuickLaunch(index) => self
                .quick_launch
                .get(index)
                .map(|chip| Action::Launch(chip.command)),
            Hit::Taskbar(index) => self
                .taskbar
                .get(index)
                .map(|chip| Action::Launch(chip.command)),
            Hit::Nothing => None,
        }
    }
}

/// Draw the NovaOS comet emblem — a square head with a trailing tail of
/// fading pixels to the lower-left.
pub fn draw_comet(canvas: &mut Canvas, x: i32, y: i32, color: u32) {
    canvas.fill_rect(Rect::new(x, y, 10, 10), color);
    canvas.fill_rect(Rect::new(x + 2, y + 2, 3, 3), 0x00C8F0FF);
    canvas.fill_rect(Rect::new(x - 5, y + 8, 5, 2), ACCENT_DIM);
    canvas.fill_rect(Rect::new(x - 3, y + 11, 4, 2), ACCENT_DIM);
    canvas.fill_rect(Rect::new(x - 1, y + 13, 3, 2), ACCENT_DIM);
}

/// Draw the glyph for an application's `Icon=`/exec name.
pub fn glyph(canvas: &mut Canvas, command: &str, x: i32, y: i32) {
    match command {
        "terminal" => {
            // A screen with a prompt.
            canvas.fill_rect(Rect::new(x - 9, y - 8, 18, 13), 0x000A1218);
            canvas.fill_rect(Rect::new(x - 6, y - 4, 4, 2), ACCENT);
            canvas.fill_rect(Rect::new(x - 6, y, 7, 2), ACCENT);
        }
        "file-manager" => {
            // A folder.
            canvas.fill_rect(Rect::new(x - 9, y - 4, 18, 11), AMBER);
            canvas.fill_rect(Rect::new(x - 9, y - 8, 7, 4), AMBER);
        }
        "editor" => {
            // A pencil.
            for step in 0..4 {
                canvas.fill_rect(
                    Rect::new(x + step * 3 - 8, y + 4 - step * 3, 4, 4),
                    0x007BC88B,
                );
            }
            canvas.fill_rect(Rect::new(x + 6, y - 10, 5, 5), 0x00F2E4C8);
        }
        "widgets-demo" => {
            // A 2x2 grid.
            canvas.fill_rect(Rect::new(x - 8, y - 7, 7, 7), 0x00C88BD0);
            canvas.fill_rect(Rect::new(x + 1, y - 7, 7, 7), 0x00C88BD0);
            canvas.fill_rect(Rect::new(x - 8, y + 2, 7, 7), 0x00C88BD0);
            canvas.fill_rect(Rect::new(x + 1, y + 2, 7, 7), ACCENT);
        }
        _ => {
            canvas.fill_rect(Rect::new(x - 4, y - 4, 8, 8), TEXT);
        }
    }
}
