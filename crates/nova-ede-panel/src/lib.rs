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
//! # The edeneu icon theme
//!
//! The desktop launches four applications whose icons the NovaOS tree
//! used to draw from rectangles. The real EDE icons are embedded in
//! [`icon_theme`]: `terminal` → `utilities-terminal`, `file-manager` →
//! `system-file-manager`, `editor` → `accessories-text-editor`, and the
//! widgets showcase → `utilities-system-monitor` — the actual
//! `edeneu` theme, not a re-drawn lookalike.
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

use nova_ui::palette;
use nova_ui::{Canvas, Rect};

pub mod icon_theme;

use icon_theme::{
    APPLICATION_RGBA, FILE_MANAGER_RGBA, FOLDER_RGBA, SYSTEM_MONITOR_RGBA, TERMINAL_RGBA,
    TEXT_EDITOR_RGBA,
};

pub mod settings;
pub mod taskbar;

pub use settings::{PanelSettings, Position};
pub use taskbar::{TaskClick, TaskEntry, TaskbarModel};

/// The panel's height in pixels.
pub const PANEL_HEIGHT: u32 = 32;

/// The panel's canvas coordinates as an i32.
pub const PANEL_HEIGHT_I32: i32 = PANEL_HEIGHT as i32;

/// The bar's background: EDE's warm window gray.
pub const BAR_BG: u32 = palette::SURFACE;
/// The bar's separator line.
pub const BAR_LINE: u32 = palette::BORDER;
/// Chip resting fill.
pub const CHIP_BG: u32 = palette::SURFACE;
/// Chip outline.
pub const CHIP_EDGE: u32 = palette::BORDER;
/// Accent colour: EDE's title-bar blue.
pub const ACCENT: u32 = palette::CHROME;
/// Amber accent for attention-getting elements (the tray dot).
pub const AMBER: u32 = 0x00D8A24A;
/// Dim accent for the comet tail.
pub const ACCENT_DIM: u32 = palette::FACE_DARK;
/// Text colour on the bar.
pub const TEXT: u32 = palette::TEXT;
/// Muted text colour on the bar.
pub const MUTED: u32 = palette::TEXT_DIM;
/// The bevel's light edge.
pub const FACE_LIGHT: u32 = palette::FACE_LIGHT;
/// The bevel's dark edge.
pub const FACE_DARK: u32 = palette::FACE_DARK;

/// The action a click on the panel can produce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Open the start menu.
    OpenStartMenu,
    /// Launch an application by name.
    Launch(&'static str),
    /// Activate the window with this surface id.
    Activate(u32),
    /// Minimize the active window with this surface id.
    Minimize(u32),
    /// Show the panel after it was hidden by the hider.
    ShowPanel,
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
    /// The hider's grip.
    Hider,
    /// Nothing was hit.
    Nothing,
}

/// The panel's state.
#[derive(Clone, Copy)]
pub struct Panel {
    width: u32,
    /// The panel's height in pixels — `panel.conf`'s `height` key.
    height: u32,
    quick_launch: [Chip; 3],
    taskbar: [Chip; 6],
    start_bounds: Rect,
    tray_bounds: Rect,
    chips_placed: bool,
    /// The compositor's window list, when the shell has published one.
    windows: TaskbarModel,
    /// Whether the panel is expanded (true) or collapsed to its grip.
    hider_open: bool,
    /// Where the dynamic taskbar chips sit once laid out.
    window_chips: [Rect; taskbar::MAX_WINDOWS],
    window_chip_width: i32,
    /// Where the clock sits, between quick launch and the taskbar.
    clock_bounds: Rect,
    /// Which applets `panel.conf` asked for. Every applet defaults on;
    /// an empty list means "whatever the config did not say".
    show_start: bool,
    show_quick: bool,
    show_clock: bool,
    show_taskbar: bool,
    show_tray: bool,
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
            height: PANEL_HEIGHT,
            quick_launch: [EMPTY; 3],
            taskbar: [EMPTY; 6],
            start_bounds: Rect::new(0, 0, START_WIDTH as u32, PANEL_HEIGHT),
            tray_bounds: Rect::new(0, 0, TRAY_WIDTH as u32, PANEL_HEIGHT),
            chips_placed: false,
            windows: TaskbarModel::new(),
            hider_open: true,
            window_chips: [Rect::new(0, 0, 0, 0); taskbar::MAX_WINDOWS],
            window_chip_width: 0,
            clock_bounds: Rect::new(0, 0, 0, 0),
            show_start: true,
            show_quick: true,
            show_clock: true,
            show_taskbar: true,
            show_tray: true,
        }
    }

    /// The panel's height in pixels — what the shell subtracts from
    /// the surface height to find the top of the bar.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Apply `panel.conf`: the bar's height and which applets it
    /// carries. An empty applet list keeps every applet — a config
    /// that says nothing must not blank the panel.
    pub fn apply_settings(&mut self, settings: &PanelSettings) {
        self.height = settings.height.clamp(16, 64);
        if settings.applet_count() > 0 {
            self.show_start = settings.has_applet("start-menu");
            self.show_quick = settings.has_applet("quick-launch");
            self.show_clock = settings.has_applet("clock");
            self.show_taskbar = settings.has_applet("taskbar");
            self.show_tray = settings.has_applet("systray");
        }
        self.chips_placed = false;
    }

    /// Publish the compositor's window list. An empty list falls the
    /// taskbar back to its pinned launcher chips.
    pub fn set_windows(&mut self, windows: &TaskbarModel) {
        self.windows = *windows;
        self.chips_placed = false;
    }

    /// Expand or collapse the panel. Collapsed, only the hider's grip
    /// remains; clicking it expands the panel again.
    pub fn set_hider_open(&mut self, open: bool) {
        self.hider_open = open;
        self.chips_placed = false;
    }

    /// Whether the panel is expanded.
    pub const fn hider_open(&self) -> bool {
        self.hider_open
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
            chip.bounds = Rect::new(x, 0, 40, self.height);
            x += 46;
            self.quick_launch[index] = chip;
        }
        // The clock sits after quick launch, in EDE's applet order
        // (start_menu, quick_launch, ..., clock, taskbar, ...).
        if self.show_clock {
            self.clock_bounds = Rect::new(x + 8, 0, 52, self.height);
            x += 68;
        } else {
            self.clock_bounds = Rect::new(0, 0, 0, 0);
        }
        // The taskbar region starts after a gap; the tray pins the right
        // edge. Dynamic window chips and the pinned fallback share it.
        let taskbar_x = x + 12;
        self.tray_bounds = Rect::new(
            self.width as i32 - TRAY_WIDTH,
            0,
            TRAY_WIDTH as u32,
            self.height,
        );
        let region_width = self.tray_bounds.x - 8 - taskbar_x;
        let count = self.windows.count();
        if count > 0 && region_width > 0 {
            // One chip per window, squeezed into the region, capped so a
            // long title cannot push neighbours off the panel.
            let width = (region_width / count as i32).clamp(24, 120);
            self.window_chip_width = width;
            for index in 0..count {
                self.window_chips[index] = Rect::new(
                    taskbar_x + index as i32 * width,
                    0,
                    width as u32 - 2,
                    self.height,
                );
            }
        } else {
            self.window_chip_width = 0;
            let mut x = taskbar_x;
            for (index, source) in TASKBAR.iter().enumerate() {
                let mut chip = *source;
                chip.bounds = Rect::new(x, 0, 96, self.height);
                x += 104;
                self.taskbar[index] = chip;
            }
        }
        self.chips_placed = true;
    }

    /// Draw the panel into the canvas at `top`.
    ///
    /// `clock` is the time string the shell read from the wall clock
    /// (`HH:MM`, empty when the RTC could not be read) — the panel
    /// draws it but never reads hardware itself.
    pub fn draw(&mut self, canvas: &mut Canvas, top: i32, tray_dot: bool, clock: &str) {
        self.place();
        let height = self.height;
        // Bar background + separator.
        canvas.fill_rect(Rect::new(0, top, self.width, height), BAR_BG);
        canvas.fill_rect(Rect::new(0, top, self.width, 1), BAR_LINE);

        // The EDE button: the menu's entry point, as upstream labels it.
        if self.show_start {
            bevel(
                canvas,
                Rect::new(2, top + 2, (START_WIDTH - 4) as u32, height - 4),
            );
            canvas.draw_text(
                START_WIDTH / 2 - 10,
                top + (height as i32 - 16) / 2,
                "EDE",
                TEXT,
                Some(CHIP_BG),
            );
        }

        // Collapsed: only the hider's grip shows.
        if !self.hider_open {
            bevel(canvas, Rect::new(0, top + 8, 12, height - 16));
            canvas.fill_rect(Rect::new(4, top + height as i32 / 2 - 2, 4, 4), ACCENT);
            return;
        }

        // Quick launch: square chips with glyphs.
        if self.show_quick {
            for chip in self.quick_launch.iter() {
                bevel(
                    canvas,
                    Rect::new(chip.bounds.x + 2, top + 4, 36, height - 8),
                );
                glyph(canvas, chip.command, chip.bounds.x + 20, top + 16);
            }
        }

        // The clock applet, in EDE's applet order before the taskbar.
        if self.show_clock && !clock.is_empty() {
            let width = Canvas::text_width(clock) as i32;
            let x = self.clock_bounds.x + (self.clock_bounds.width as i32 - width) / 2;
            canvas.draw_text(
                x,
                top + (height as i32 - 16) / 2,
                clock,
                TEXT,
                Some(CHIP_BG),
            );
        }

        // Taskbar: dynamic window chips when the compositor has a list,
        // the pinned launcher otherwise. The active window wears EDE's
        // title blue; the rest sit on the panel's own gray.
        if self.show_taskbar {
            if self.window_chip_width > 0 {
                for (index, bounds) in self
                    .window_chips
                    .iter()
                    .enumerate()
                    .take(self.windows.count())
                {
                    let Some(entry) = self.windows.entry(index) else {
                        continue;
                    };
                    // The chips are laid out in panel-local x, but the
                    // y is the panel's own: the rects `place()` stores
                    // carry y=0, so they must be re-based to `top`
                    // here — otherwise every chip paints itself at the
                    // top of the screen, on top of the wallpaper.
                    let bounds = Rect::new(bounds.x, top, bounds.width, height);
                    if entry.active {
                        canvas.fill_rect(bounds, ACCENT);
                        canvas.fill_rect(Rect::new(bounds.x, top, 1, height), palette::CHROME_SUNK);
                        canvas.draw_text(
                            bounds.x + 14,
                            top + 9,
                            entry.title(),
                            palette::TEXT_ON_CHROME,
                            Some(ACCENT),
                        );
                        canvas.fill_rect(
                            Rect::new(bounds.x + 6, top + 12, 4, 8),
                            if entry.minimized {
                                palette::TEXT_DIM
                            } else {
                                palette::TEXT_ON_CHROME
                            },
                        );
                    } else {
                        bevel(canvas, bounds);
                        canvas.fill_rect(
                            Rect::new(bounds.x + 6, top + 12, 4, 8),
                            if entry.minimized { MUTED } else { ACCENT },
                        );
                        canvas.draw_text(
                            bounds.x + 14,
                            top + 9,
                            entry.title(),
                            TEXT,
                            Some(CHIP_BG),
                        );
                    }
                }
            } else {
                // Pinned fallback: chips with an accent dot and label.
                for chip in self.taskbar.iter() {
                    if chip.command.is_empty() {
                        continue;
                    }
                    bevel(
                        canvas,
                        Rect::new(chip.bounds.x + 2, top + 4, 92, height - 8),
                    );
                    canvas.fill_rect(Rect::new(chip.bounds.x + 8, top + 12, 4, 8), ACCENT);
                    canvas.draw_text(chip.bounds.x + 16, top + 9, chip.label, TEXT, Some(CHIP_BG));
                }
            }
        }

        // Right segment: the system tray slot.
        if self.show_tray {
            bevel(
                canvas,
                Rect::new(
                    self.tray_bounds.x + 6,
                    top + 4,
                    (TRAY_WIDTH - 12) as u32,
                    height - 8,
                ),
            );
            draw_comet(canvas, self.tray_bounds.x + 10, top + 9, ACCENT);
            canvas.draw_text(self.tray_bounds.x + 28, top + 9, "EDE", TEXT, Some(CHIP_BG));
            if tray_dot {
                canvas.fill_rect(
                    Rect::new(self.tray_bounds.x + TRAY_WIDTH - 14, top + 4, 8, 8),
                    AMBER,
                );
            }
        }
    }

    /// Hit-test a panel-local point.
    pub fn hit(&mut self, x: i32, y: i32) -> Hit {
        self.place();
        let height = self.height as i32;
        if !(0..height).contains(&y) {
            return Hit::Nothing;
        }
        // Collapsed: only the grip answers.
        if !self.hider_open {
            return if x < 14 { Hit::Hider } else { Hit::Nothing };
        }
        let start_right = self.start_bounds.x + self.start_bounds.width as i32;
        if self.show_start && x >= self.start_bounds.x && x < start_right {
            return Hit::StartMenu;
        }
        if self.show_quick {
            for (index, chip) in self.quick_launch.iter().enumerate() {
                let right = chip.bounds.x + chip.bounds.width as i32;
                if chip.bounds.width > 0 && x >= chip.bounds.x && x < right {
                    return Hit::QuickLaunch(index);
                }
            }
        }
        if !self.show_taskbar {
            return Hit::Nothing;
        }
        if self.window_chip_width > 0 {
            for (index, bounds) in self
                .window_chips
                .iter()
                .enumerate()
                .take(self.windows.count())
            {
                let right = bounds.x + bounds.width as i32;
                if x >= bounds.x && x < right {
                    return Hit::Taskbar(index);
                }
            }
            return Hit::Nothing;
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
            Hit::Taskbar(index) => {
                if self.window_chip_width > 0 {
                    // A window chip: active windows minimize on click,
                    // everything else activates — upstream's taskbar.
                    let entry = self.windows.entry(index)?;
                    if entry.active && !entry.minimized {
                        Some(Action::Minimize(entry.id))
                    } else {
                        Some(Action::Activate(entry.id))
                    }
                } else {
                    self.taskbar
                        .get(index)
                        .map(|chip| Action::Launch(chip.command))
                }
            }
            Hit::Hider => Some(Action::ShowPanel),
            Hit::Nothing => None,
        }
    }
}

/// Draw the NovaOS comet emblem — a square head with a trailing tail of
/// fading pixels to the lower-left.
/// Draw an FLTK-style up-box bevel inside `rect`: the panel's fill
/// with a light edge on the top and left and a dark edge on the bottom
/// and right — the shape every EDE control is drawn with.
fn bevel(canvas: &mut Canvas, rect: Rect) {
    canvas.fill_rect(rect, CHIP_BG);
    let (x, y) = (rect.x, rect.y);
    let (w, h) = (rect.width, rect.height as i32);
    canvas.fill_rect(Rect::new(x, y, w, 1), FACE_LIGHT);
    canvas.fill_rect(Rect::new(x, y, 1, h as u32), FACE_LIGHT);
    canvas.fill_rect(Rect::new(x, y + h - 1, w, 1), FACE_DARK);
    canvas.fill_rect(Rect::new(x + w as i32 - 1, y, 1, h as u32), FACE_DARK);
}

pub fn draw_comet(canvas: &mut Canvas, x: i32, y: i32, color: u32) {
    canvas.fill_rect(Rect::new(x, y, 10, 10), color);
    canvas.fill_rect(Rect::new(x + 2, y + 2, 3, 3), 0x00C8F0FF);
    canvas.fill_rect(Rect::new(x - 5, y + 8, 5, 2), ACCENT_DIM);
    canvas.fill_rect(Rect::new(x - 3, y + 11, 4, 2), ACCENT_DIM);
    canvas.fill_rect(Rect::new(x - 1, y + 13, 3, 2), ACCENT_DIM);
}

/// Draw the glyph for an application's `Icon=`/exec name.
/// The edeneu icon for an application, as 64x64 RGBA pixels — the
/// theme `edeproject/ede` ships in `data/icon-themes/edeneu`, embedded
/// in [`icon_theme`]. `None` for a service this port does not carry an
/// edeneu icon for; callers fall back to their own drawing. The map
/// takes both the `.desktop` `Icon=` name and the service's executable
/// name, because the desktop layer hands out whichever it has.
pub fn icon_rgba(command: &str) -> Option<&'static [u8]> {
    match command {
        "utilities-terminal" | "terminal" => Some(&TERMINAL_RGBA),
        "system-file-manager" | "file-manager" => Some(&FILE_MANAGER_RGBA),
        "accessories-text-editor" | "editor" => Some(&TEXT_EDITOR_RGBA),
        // The showcase gets edeneu's system monitor icon: the closest
        // thing the theme ships to a "widgets" noun.
        "utilities-system-monitor" | "widgets-demo" => Some(&SYSTEM_MONITOR_RGBA),
        // The generic application icon for launchers the user creates.
        "application-x-executable" => Some(&APPLICATION_RGBA),
        // The folder icon for folder launchers.
        "folder" => Some(&FOLDER_RGBA),
        _ => None,
    }
}

/// Draw the application's edeneu icon at `(x, y)` (top-left), alpha-
/// blended on whatever is underneath — no plate, the way the real
/// desktop renders its icons on the wallpaper.
pub fn draw_app_icon(canvas: &mut Canvas, command: &str, x: i32, y: i32) {
    if let Some(rgba) = icon_rgba(command) {
        canvas.blit_rgba(x, y, 64, 64, rgba);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> Panel {
        Panel::new(1024)
    }

    fn windows() -> TaskbarModel {
        let mut model = TaskbarModel::new();
        model.sync(&[
            TaskEntry::new(11, "Terminal", false, false),
            TaskEntry::new(12, "Editor", true, false),
        ]);
        model
    }

    #[test]
    fn the_start_button_hits_where_it_draws() {
        let mut panel = panel();
        let hit = panel.hit(40, 16);
        assert_eq!(hit, Hit::StartMenu);
        assert_eq!(panel.action_for(hit), Some(Action::OpenStartMenu));
    }

    #[test]
    fn quick_launch_chips_launch_their_commands() {
        let mut panel = panel();
        let hit = panel.hit(START_WIDTH + 20, 16);
        match panel.action_for(hit) {
            Some(Action::Launch(command)) => assert!(!command.is_empty()),
            other => panic!("expected a launch, got {other:?}"),
        }
    }

    #[test]
    fn no_chip_paints_above_the_panel() {
        // Regression for the chips painting themselves at the top of
        // the display: `place()` stores the chip rects with y=0 (panel-
        // local x only), and `draw()` used them directly, so every chip
        // landed at y=0..height on top of the wallpaper.
        let mut panel = panel();
        panel.set_windows(&windows());
        const WIDTH: usize = 400;
        const HEIGHT: usize = 160;
        const TOP: usize = 128;
        let mut buffer = [0xAB_u8; WIDTH * HEIGHT * 4];
        let mut canvas = Canvas::new(buffer.as_mut_ptr(), WIDTH as u32, HEIGHT as u32);
        panel.draw(&mut canvas, TOP as i32, false, "12:00");
        let stride = WIDTH * 4;
        for y in 0..TOP {
            for byte in &buffer[y * stride..(y + 1) * stride] {
                assert_eq!(byte, &0xAB, "row {y} was painted above the bar");
            }
        }
    }

    #[test]
    fn pinned_chips_fall_out_when_windows_arrive() {
        let mut panel = panel();
        panel.set_windows(&windows());
        panel.place(); // lay the dynamic chips out
        // The first dynamic chip lands in the taskbar region.
        let hit = panel.hit(panel.window_chips[0].x + 4, 16);
        assert_eq!(
            panel.action_for(hit),
            Some(Action::Activate(11)),
            "a non-active window chip activates"
        );
    }

    #[test]
    fn clicking_the_active_window_minimizes() {
        let mut panel = panel();
        panel.set_windows(&windows());
        panel.place();
        let hit = panel.hit(panel.window_chips[1].x + 4, 16);
        assert_eq!(panel.action_for(hit), Some(Action::Minimize(12)));
    }

    #[test]
    fn an_empty_window_list_falls_back_to_pinned_chips() {
        let mut panel = panel();
        panel.set_windows(&TaskbarModel::new());
        panel.place();
        let hit = panel.hit(panel.taskbar[0].bounds.x + 4, 16);
        assert!(matches!(panel.action_for(hit), Some(Action::Launch(_))));
    }

    #[test]
    fn the_hider_collapses_the_panel_to_its_grip() {
        let mut panel = panel();
        panel.set_hider_open(false);
        assert_eq!(panel.hit(6, 16), Hit::Hider);
        assert_eq!(panel.action_for(Hit::Hider), Some(Action::ShowPanel));
        assert_eq!(panel.hit(200, 16), Hit::Nothing, "nothing else answers");
        panel.set_hider_open(true);
        assert_eq!(panel.hit(40, 16), Hit::StartMenu, "expanded again");
    }

    #[test]
    fn chips_stay_inside_the_panel() {
        let mut panel = panel();
        panel.set_windows(&windows());
        panel.place();
        for index in 0..windows().count() {
            let bounds = panel.window_chips[index];
            assert!(bounds.x >= 0);
            assert!(
                bounds.x + bounds.width as i32 <= panel.width as i32,
                "chip {index} runs off the panel"
            );
        }
    }

    #[test]
    fn panel_settings_flow_through() {
        let config = nova_edelib::EdeConfig::parse(
            b"[Panel]\nposition = 1\nheight = 36\napplets = start-menu,clock\n",
        );
        let settings = PanelSettings::from_config(&config);
        assert_eq!(settings.height, 36);
        assert_eq!(settings.position, Position::Bottom);
        assert!(settings.has_applet("start-menu"));
        assert!(settings.has_applet("clock"));
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn apply_settings_sets_the_height() {
        let mut panel = Panel::new(1024);
        assert_eq!(panel.height(), 32);
        let config = nova_edelib::EdeConfig::parse(b"[Panel]\nheight = 48\n");
        panel.apply_settings(&PanelSettings::from_config(&config));
        assert_eq!(panel.height(), 48);
        // The height bounds the hit test too: below the bar answers.
        assert_eq!(panel.hit(40, 47), Hit::StartMenu);
        assert_eq!(panel.hit(40, 48), Hit::Nothing, "above the bar");
    }

    #[test]
    fn apply_settings_gates_the_applets() {
        let mut panel = Panel::new(1024);
        let config = nova_edelib::EdeConfig::parse(b"[Panel]\napplets = start-menu,taskbar\n");
        panel.apply_settings(&PanelSettings::from_config(&config));
        // The start button still answers…
        assert_eq!(panel.hit(40, 16), Hit::StartMenu);
        // …the quick-launch chips do not.
        let quick_x = START_WIDTH + 20;
        assert_eq!(panel.hit(quick_x, 16), Hit::Nothing);
    }

    #[test]
    fn an_empty_applet_list_keeps_everything() {
        let mut panel = Panel::new(1024);
        panel.apply_settings(&PanelSettings::from_config(&nova_edelib::EdeConfig::parse(
            b"[Panel]\n",
        )));
        assert_eq!(panel.hit(40, 16), Hit::StartMenu);
        assert_eq!(panel.hit(START_WIDTH + 20, 16), Hit::QuickLaunch(0));
    }
}
