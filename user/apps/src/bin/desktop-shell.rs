#![no_std]
#![no_main]

//! The NovaOS desktop shell — a faithful Rust port of EDE's desktop stack.
//!
//! # What this is
//!
//! The background desktop as one fullscreen compositor client at the back
//! of the z-order. It paints the wallpaper, shows one icon per `.desktop`
//! file in `/home/nova/Desktop`, draws the panel along the bottom with the
//! EDE applets (`start-menu`, `quick-launch`, `taskbar`, `systray`), and
//! turns clicks into process spawns.
//!
//! The crate layout mirrors EDE's own:
//!
//! - `nova-edelib` — edelib's logging, configuration and `.desktop` parsing
//! - `nova-ede-desktop` — the desktop's icon layer model
//! - `nova-ede-panel` — the panel's applets and hit-testing
//! - `nova-ede-launch` — the start-menu launcher model
//! - `nova-ede-notifyd` — the notification daemon model (balloon)
//! - `nova-ede-about` — the About dialog content
//! - `nova-ede-tip` — tooltip text handling
//! - `nova-ede-desktop-conf` — `desktop.conf`: wallpaper and icon labels
//!
//! # Widgets are data, not code
//!
//! Which icons appear, what the panel shows, what the start menu offers,
//! all come from `.desktop` files in the VFS — the same freedesktop
//! convention EDE used. The shell hardcodes nothing about applications.
//!
//! # Clicks are edges, not levels
//!
//! The input service reports the button *state* on every pointer event, so
//! a press must be recognised as the transition from up to down: a hover
//! while the button is held is the same state as the press that started
//! it. Every activation here — selecting an icon, opening the menu,
//! dismissing a balloon — runs on that edge exactly once, and an icon
//! drag starts on the press and resolves on the release.

use nova_apps::graphics::{self, InputEvent, Window};
use nova_apps::syscall::wall_time;
use nova_apps::{console, fsclient, process, yield_now};
use nova_desktop::wallpaper;
use nova_ede_about::AboutContent;
use nova_ede_desktop::DesktopLayer;
use nova_ede_desktop_conf::DesktopConf;
use nova_ede_launch::Launchpad;
use nova_ede_notifyd::NotifyBus;
use nova_ede_panel::settings::PanelSettings;
use nova_ede_panel::taskbar::{self, TaskEntry, TaskbarModel};
use nova_ede_panel::{self, Action as PanelAction, Panel};
use nova_ede_tip::Tooltip;
use nova_edelib::EdeConfig;
use nova_protocols::input::{
    INPUT_BUTTON_PRIMARY, INPUT_BUTTON_SECONDARY, INPUT_EVENT_KEY, INPUT_EVENT_POINTER,
};
use nova_ui::keyboard::{Decoder, Key};
use nova_ui::{Canvas, Rect, palette, typeface};

static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 768;
const MAX_EVENTS: usize = 8;

/// The EDE menu's geometry, in surface coordinates.
///
/// The menu sizes itself to its page: a header bar, up to
/// [`MAX_MENU_ROWS`] rows of [`MENU_ROW_PITCH`], and a 4px floor gap.
const MENU_WIDTH: i32 = 240;
const MENU_HEADER_H: i32 = 24;
const MENU_ROW_PITCH: i32 = 24;
const MENU_FLOOR: i32 = 4;
const MENU_X: i32 = 8;
/// The most rows any menu page shows.
const MAX_MENU_ROWS: usize = 8;

/// The Run dialog's size, and the offsets of its two buttons.
const RUN_DIALOG_W: i32 = 360;
const RUN_DIALOG_H: i32 = 116;
const RUN_OK_INSET: i32 = 184;
const RUN_CANCEL_INSET: i32 = 96;
const RUN_BUTTON_INSET_Y: i32 = 84;

/// How many `icons=` names `desktop.conf` may filter by.
const MAX_ICON_FILTER: usize = 16;
/// The longest basename kept in the filter.
const FILTER_NAME_MAX: usize = 31;

/// A pointer drag in progress: the press landed on an already-selected
/// icon, so it may become a move — or, if the pointer never travelled,
/// an activation on release.
#[derive(Clone, Copy)]
struct Drag {
    index: usize,
    grab_dx: i32,
    grab_dy: i32,
    origin_x: i32,
    origin_y: i32,
    moved: bool,
}

/// The desktop shell's mutable state.
struct Shell {
    /// The icon column, built from `.desktop` files at boot.
    layer: DesktopLayer,
    /// Whether the start menu is open.
    start_menu: bool,
    /// Whether the About dialog is open.
    about: bool,
    /// The launcher model behind the start menu.
    launchpad: Launchpad,
    /// The notification daemon model.
    notify: NotifyBus,
    /// The panel's state and hit model.
    panel: Panel,
    /// Which icon the pointer is hovering, and how long it has been there.
    hover: Option<usize>,
    hover_ticks: u32,
    /// The tooltip for the hovered icon.
    tip: Tooltip,
    /// Ticks since boot, used purely for the balloon lifetime.
    ticks: u32,
    /// The primary button's state on the previous event: the edge the
    /// click logic runs on.
    prev_buttons: u32,
    /// An icon drag started by the last press.
    drag: Option<Drag>,
    /// The keyboard decoder: shift, caps lock, the extended prefix.
    decoder: Decoder,
    /// `panel.conf`, applied to the panel at boot.
    panel_settings: PanelSettings,
    /// `desktop.conf`: the wallpaper and the icon labels.
    desktop_conf: DesktopConf,
    /// Which desktop files `desktop.conf`'s `icons` key keeps. An empty
    /// filter keeps every file.
    icon_filter: [[u8; FILTER_NAME_MAX]; MAX_ICON_FILTER],
    icon_filter_lens: [usize; MAX_ICON_FILTER],
    icon_filter_count: usize,
    /// This shell's own surface: never a taskbar chip of its own.
    own_surface: u32,
    /// The last pointer position, for the auto-hider.
    pointer_x: i32,
    pointer_y: i32,
    /// The scanout size this shell lays itself out for.
    ///
    /// Read from the display service at startup, not assumed: the
    /// desktop must fill whatever mode the GPU reports, and a shell
    /// that hard-coded 1024x768 would leave a band of untouched
    /// framebuffer wherever the device chose something else.
    width: u32,
    height: u32,
    /// Ticks the pointer has been away from an auto-hiding panel.
    hider_ticks: u32,
    /// The window list's fingerprint as last published: membership and
    /// state, not the round trip, is what warrants a redraw.
    windows_fingerprint: Option<u64>,
    /// The clock text the panel draws (`HH:MM`) and the minute it was
    /// read — the wall clock is an RTC poll, so it is fetched when the
    /// minute turns, not per frame. Empty until the first successful
    /// read.
    clock_text: [u8; 6],
    clock_len: usize,
    clock_minute: i64,
    /// The window list as last published, for the EDE menu's Window
    /// List page.
    window_list: [TaskEntry; taskbar::MAX_WINDOWS],
    window_count: usize,
    /// Which page of the EDE menu is showing.
    menu_page: MenuPage,
    /// The highlighted row on the current menu page.
    menu_selection: usize,
    /// The Run dialog: open flag, typed command, caret, and buttons.
    run_open: bool,
    run_input: [u8; RUN_INPUT_MAX],
    run_len: usize,
    run_cursor: usize,
    run_ok: nova_ui::Button,
    run_cancel: nova_ui::Button,
    /// The name of a notification file that resisted deletion: its
    /// content is posted once, not again on every drain.
    stuck_notify: [u8; 32],
    stuck_notify_len: usize,
}

/// The maximum command the Run dialog keeps.
const RUN_INPUT_MAX: usize = 48;

/// Which page of the EDE menu is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuPage {
    /// The top level: Programs, Window List, Run…, About EDE.
    Root,
    /// The Programs page: one row per category.
    Programs,
    /// One category's applications; the index is into the category
    /// list [`Shell::menu_categories`] builds — stable for a given set
    /// of `.desktop` files, which is what makes it safe to draw the
    /// page and to hit-test it separately.
    Category(usize),
    /// The window list.
    WindowList,
}

impl Shell {
    fn new(width: u32, height: u32) -> Self {
        let mut shell = Self {
            layer: DesktopLayer::new(),
            start_menu: false,
            about: false,
            launchpad: Launchpad::new(),
            notify: NotifyBus::new(),
            panel: Panel::new(width),
            hover: None,
            hover_ticks: 0,
            tip: Tooltip::new(12),
            ticks: 0,
            prev_buttons: 0,
            drag: None,
            decoder: Decoder::new(),
            panel_settings: PanelSettings::new(),
            desktop_conf: DesktopConf::default(),
            icon_filter: [[0; FILTER_NAME_MAX]; MAX_ICON_FILTER],
            icon_filter_lens: [0; MAX_ICON_FILTER],
            icon_filter_count: 0,
            own_surface: 0,
            pointer_x: -1,
            pointer_y: -1,
            width,
            height,
            hider_ticks: 0,
            windows_fingerprint: None,
            clock_text: [0; 6],
            clock_len: 0,
            clock_minute: -1,
            window_list: [TaskEntry::new(0, "", false, false); taskbar::MAX_WINDOWS],
            window_count: 0,
            menu_page: MenuPage::Root,
            menu_selection: 0,
            run_open: false,
            run_input: [0; RUN_INPUT_MAX],
            run_len: 0,
            run_cursor: 0,
            run_ok: nova_ui::Button::new(
                Rect::new(
                    (width as i32 - RUN_DIALOG_W) / 2 + RUN_DIALOG_W - RUN_OK_INSET,
                    (height as i32 - RUN_DIALOG_H) / 2 + RUN_BUTTON_INSET_Y,
                    80,
                    24,
                ),
                "OK",
            ),
            run_cancel: nova_ui::Button::new(
                Rect::new(
                    (width as i32 - RUN_DIALOG_W) / 2 + RUN_DIALOG_W - RUN_CANCEL_INSET,
                    (height as i32 - RUN_DIALOG_H) / 2 + RUN_BUTTON_INSET_Y,
                    80,
                    24,
                ),
                "Cancel",
            ),
            stuck_notify: [0; 32],
            stuck_notify_len: 0,
        };
        shell.load_configs();
        shell.scan_desktop();
        shell
    }

    /// Read `~/.ede/panel.conf` and `~/.ede/desktop.conf` — the two
    /// files the kernel seeds and the settings tools rewrite.
    fn load_configs(&mut self) {
        let mut buffer = [0u8; 1024];
        if let Ok(count) = fsclient::read_all(b"/home/nova/.ede/panel.conf", &mut buffer) {
            let config = EdeConfig::parse(&buffer[..count]);
            self.panel_settings = PanelSettings::from_config(&config);
            self.panel.apply_settings(&self.panel_settings);
        }
        if let Ok(count) = fsclient::read_all(b"/home/nova/.ede/desktop.conf", &mut buffer) {
            let config = EdeConfig::parse(&buffer[..count]);
            self.desktop_conf = DesktopConf::from_config(&config);
            if let Some(list) = config.get("Desktop", "icons") {
                let mut count = 0usize;
                for name in list.split(',') {
                    let name = name.trim();
                    if name.is_empty() || count >= MAX_ICON_FILTER {
                        continue;
                    }
                    let bytes = name.as_bytes();
                    let take = bytes.len().min(FILTER_NAME_MAX);
                    self.icon_filter[count][..take].copy_from_slice(&bytes[..take]);
                    self.icon_filter_lens[count] = take;
                    count += 1;
                }
                self.icon_filter_count = count;
            }
        }
    }

    /// Whether `desktop.conf`'s `icons` list excludes this desktop
    /// file. The list names stems (`terminal`, not `terminal.desktop`).
    fn icon_filtered_out(&self, basename: &[u8]) -> bool {
        if self.icon_filter_count == 0 {
            return false;
        }
        let stem = basename.strip_suffix(b".desktop").unwrap_or(basename);
        !(0..self.icon_filter_count)
            .any(|index| &self.icon_filter[index][..self.icon_filter_lens[index]] == stem)
    }

    /// Read `/home/nova/Desktop/*.desktop` into the icon column.
    fn scan_desktop(&mut self) {
        let mut offset: u32 = 0;
        loop {
            let mut page = [0u8; fsclient::PAGE_BYTES];
            let page_info = match fsclient::list_at(b"/home/nova/Desktop", offset, &mut page) {
                Ok(page) => page,
                Err(_) => break,
            };
            let mut records = fsclient::Records::new(&page[..page_info.bytes]);
            while let Some(record) = records.next() {
                if record.directory || !record.name.ends_with(b".desktop") {
                    continue;
                }
                if self.icon_filtered_out(record.name) {
                    continue;
                }
                let mut path = [0u8; nova_fs::MAX_PATH_BYTES];
                let base = b"/home/nova/Desktop/";
                path[..base.len()].copy_from_slice(base);
                path[base.len()..base.len() + record.name.len()].copy_from_slice(record.name);
                let full = &path[..base.len() + record.name.len()];
                let mut contents = [0u8; 1024];
                let Ok(count) = fsclient::read_all(full, &mut contents) else {
                    continue;
                };
                let index = self.layer.len();
                let _ = self.layer.add_from_desktop(&contents[..count], index);
                let _ = self.launchpad.add_desktop(&contents[..count]);
            }
            offset = offset.saturating_add(page_info.count);
            if page_info.count == 0 || offset >= page_info.total {
                break;
            }
        }
    }

    /// Drain `/var/run/notify` for pending notifications.
    ///
    /// A file the VFS refused to delete stays on disk; without a memory
    /// of it, every drain would post the same message again. The file's
    /// *name* is that memory — it is the posting application's identity,
    /// so one stuck file is skipped until a delete finally succeeds.
    ///
    /// Returns whether a notification was posted: that is a screen
    /// change the shell must repaint for.
    fn drain_notifications(&mut self) -> bool {
        let mut changed = false;
        let mut page = [0u8; fsclient::PAGE_BYTES];
        let Ok(info) = fsclient::list_at(b"/var/run/notify", 0, &mut page) else {
            return false;
        };
        let mut records = fsclient::Records::new(&page[..info.bytes]);
        while let Some(record) = records.next() {
            if record.directory {
                continue;
            }
            let mut path = [0u8; nova_fs::MAX_PATH_BYTES];
            let base = b"/var/run/notify/";
            path[..base.len()].copy_from_slice(base);
            path[base.len()..base.len() + record.name.len()].copy_from_slice(record.name);
            let full = &path[..base.len() + record.name.len()];
            let mut contents = [0u8; fsclient::PAGE_BYTES];
            let Ok(count) = fsclient::read_all(full, &mut contents) else {
                continue;
            };
            let stuck = self.stuck_notify_len == record.name.len()
                && &self.stuck_notify[..self.stuck_notify_len] == record.name;
            if !stuck {
                if let Ok(text) = core::str::from_utf8(&contents[..count]) {
                    if self.notify.post_wire(text) {
                        changed = true;
                    }
                }
            }
            if fsclient::delete(full).is_ok() {
                if stuck {
                    self.stuck_notify_len = 0;
                }
            } else {
                let take = record.name.len().min(self.stuck_notify.len());
                self.stuck_notify[..take].copy_from_slice(&record.name[..take]);
                self.stuck_notify_len = take;
            }
        }
        changed
    }

    /// Read the wall clock when the minute turns.
    ///
    /// The wall clock is an RTC read through the kernel — not a cheap
    /// call — so the shell polls it once a minute of *wall time* and
    /// keeps the formatted `HH:MM` between polls. Returns whether the
    /// displayed text changed.
    fn refresh_clock(&mut self) -> bool {
        let Ok(seconds) = wall_time() else {
            return false;
        };
        if seconds < 0 {
            return false;
        }
        let minute = seconds / 60;
        if minute == self.clock_minute {
            return false;
        }
        self.clock_minute = minute;
        let day_seconds = seconds.rem_euclid(86_400);
        let hour = (day_seconds / 3600) as u8;
        let mins = ((day_seconds % 3600) / 60) as u8;
        let text = [
            b'0' + hour / 10,
            b'0' + hour % 10,
            b':',
            b'0' + mins / 10,
            b'0' + mins % 10,
        ];
        self.clock_text[..5].copy_from_slice(&text);
        self.clock_len = 5;
        true
    }

    /// Page through the compositor's window list and republish it to
    /// the panel's taskbar. The shell's own surface is skipped — a
    /// window does not list itself as one of its own chips.
    ///
    /// Returns whether the list *changed*: the round trip runs on a
    /// timer, but a redraw runs only when membership or state moved, or
    /// the desktop would repaint continuously for a list that is
    /// standing still.
    fn refresh_windows(&mut self) -> bool {
        let mut collected = [TaskEntry::new(0, "", false, false); taskbar::MAX_WINDOWS];
        let mut count = 0usize;
        let mut offset = 0u32;
        loop {
            let Ok(page) = graphics::compositor_list(offset) else {
                break;
            };
            for index in 0..page.count {
                let info = page.entries[index];
                if info.id == self.own_surface || count >= taskbar::MAX_WINDOWS {
                    continue;
                }
                let mut name = [0u8; 48];
                let title = match graphics::compositor_name(info.id, &mut name) {
                    Ok(len) if len > 0 => core::str::from_utf8(&name[..len]).unwrap_or("Window"),
                    _ => "Window",
                };
                collected[count] = TaskEntry::new(info.id, title, info.focused(), info.minimized());
                count += 1;
            }
            let next = offset.saturating_add(page.count as u32);
            if page.count == 0 || next as usize >= page.total {
                break;
            }
            offset = next;
        }
        // Fingerprint membership and state; titles are derived from the
        // same ids and cannot drift alone.
        let mut fingerprint = count as u64;
        for entry in collected.iter().take(count) {
            fingerprint = fingerprint.rotate_left(13)
                ^ entry.id as u64
                ^ ((entry.active as u64) << 8)
                ^ ((entry.minimized as u64) << 16);
        }
        if self.windows_fingerprint == Some(fingerprint) {
            return false;
        }
        self.windows_fingerprint = Some(fingerprint);
        // Keep a copy for the EDE menu's Window List page.
        self.window_list = collected;
        self.window_count = count;
        let mut model = TaskbarModel::new();
        model.sync(&collected[..count]);
        self.panel.set_windows(&model);
        true
    }

    /// The auto-hider: with `hider = true` in `panel.conf`, the panel
    /// folds to its grip after the pointer has stayed away, and unfolds
    /// the moment the pointer returns to the strip. Returns whether the
    /// panel's visibility changed.
    fn tick_hider(&mut self) -> bool {
        if !self.panel_settings.hider {
            return false;
        }
        let panel_top = self.height as i32 - self.panel.height() as i32;
        if self.pointer_y >= panel_top {
            self.hider_ticks = 0;
            if !self.panel.hider_open() {
                self.panel.set_hider_open(true);
                return true;
            }
        } else if self.panel.hider_open() {
            self.hider_ticks = self.hider_ticks.saturating_add(1);
            if self.hider_ticks > 240 {
                self.panel.set_hider_open(false);
                self.hider_ticks = 0;
                return true;
            }
        }
        false
    }

    /// The rows the current menu page shows, capped at the page budget.
    fn menu_row_count(&self) -> usize {
        let rows = match self.menu_page {
            MenuPage::Root => 4,
            MenuPage::Programs => self.launchpad.category_count(),
            MenuPage::Category(category) => self
                .launchpad
                .category_at(category)
                .map(|name| self.launchpad.matching_in_category(name).count())
                .unwrap_or(0),
            MenuPage::WindowList => self.window_count,
        };
        rows.min(MAX_MENU_ROWS)
    }

    /// The menu's height for its current page: header, rows, floor.
    fn menu_height(&self) -> i32 {
        MENU_HEADER_H + (self.menu_row_count() as i32) * MENU_ROW_PITCH + MENU_FLOOR
    }

    /// The menu's top edge: it hangs directly above the panel.
    fn menu_top(&self) -> i32 {
        self.height as i32 - self.panel.height() as i32 - 4 - self.menu_height()
    }

    /// The Run dialog's left edge, centred on the screen.
    fn run_dialog_x(&self) -> i32 {
        (self.width as i32 - RUN_DIALOG_W) / 2
    }

    /// The Run dialog's top edge, centred on the screen.
    fn run_dialog_y(&self) -> i32 {
        (self.height as i32 - RUN_DIALOG_H) / 2
    }

    /// The header's title for the current page.
    fn menu_title(&self) -> &str {
        match self.menu_page {
            MenuPage::Root => "EDE",
            MenuPage::Programs => "Programs",
            MenuPage::Category(index) => self.launchpad.category_at(index).unwrap_or("Programs"),
            MenuPage::WindowList => "Window List",
        }
    }

    /// The label of one row of the current page, and whether it opens
    /// a submenu (the row then gets a ▸ arrow).
    fn menu_row_text(&self, row: usize) -> (&str, bool) {
        match self.menu_page {
            MenuPage::Root => match row {
                0 => ("Programs", true),
                1 => ("Window List", true),
                2 => ("Run...", false),
                _ => ("About EDE", false),
            },
            MenuPage::Programs => (self.launchpad.category_at(row).unwrap_or(""), true),
            MenuPage::Category(category) => {
                let name = self.launchpad.category_at(category).unwrap_or("");
                let label = self
                    .launchpad
                    .matching_in_category(name)
                    .nth(row)
                    .and_then(|index| self.launchpad.entry_ref(index))
                    .map(|entry| entry.name())
                    .unwrap_or("");
                (label, false)
            }
            MenuPage::WindowList => (
                self.window_list
                    .get(row)
                    .map(|entry| entry.title())
                    .unwrap_or(""),
                false,
            ),
        }
    }

    /// Move the menu's highlight, wrapping at both ends.
    fn menu_move(&mut self, delta: i32) -> bool {
        let rows = self.menu_row_count();
        if rows == 0 {
            return false;
        }
        let mut next = self.menu_selection as i32 + delta;
        while next < 0 {
            next += rows as i32;
        }
        let next = (next as usize) % rows;
        if next != self.menu_selection {
            self.menu_selection = next;
            true
        } else {
            false
        }
    }

    /// Go up one menu page; at the root there is nowhere to go.
    fn menu_back(&mut self) -> bool {
        let next = match self.menu_page {
            MenuPage::Category(_) => Some(MenuPage::Programs),
            MenuPage::Programs | MenuPage::WindowList => Some(MenuPage::Root),
            MenuPage::Root => None,
        };
        match next {
            Some(page) => {
                self.menu_page = page;
                self.menu_selection = 0;
                true
            }
            None => false,
        }
    }

    /// Close the menu and forget its page and filter.
    fn close_menu(&mut self) {
        self.start_menu = false;
        self.launchpad.clear();
        self.menu_page = MenuPage::Root;
        self.menu_selection = 0;
    }

    /// Open the Run dialog with an empty field.
    fn open_run_dialog(&mut self) {
        self.close_menu();
        self.run_open = true;
        self.run_len = 0;
        self.run_cursor = 0;
    }

    fn draw(&mut self, canvas: &mut Canvas) {
        // The wallpaper: the named image when `desktop.conf` asks for
        // it, the configured background colour otherwise.
        if self.desktop_conf.wallpaper_use
            && matches!(self.desktop_conf.wallpaper_path(), Some("nova-deep-sea"))
        {
            wallpaper::paint(canvas);
        } else {
            let color = self.desktop_conf.color & 0x00FF_FFFF;
            canvas.fill_rect(Rect::new(0, 0, self.width, self.height), color);
        }

        // Desktop icons, with the selection highlight.
        let icon_count = self.layer.len();
        for index in 0..icon_count {
            let Some(icon) = self.layer.icon(index) else {
                continue;
            };
            let cell = nova_ui::Rect::new(
                icon.cell.x,
                icon.cell.y,
                icon.cell.width as u32,
                icon.cell.height as u32,
            );
            if icon.selected {
                canvas.fill_rect(cell, palette::SELECTED);
                canvas.fill_rect(cell.inset(2), palette::SURFACE);
            }
            // Glyph plate and a simple glyph per app: EDE's light
            // window gray with the theme's border.
            let gx = cell.x + 28;
            let gy = cell.y + 8;
            canvas.fill_rect(Rect::new(gx, gy, 48, 40), palette::SURFACE);
            canvas.stroke_rect(Rect::new(gx, gy, 48, 40), palette::BORDER);
            nova_ede_panel::glyph(canvas, icon.exec(), gx + 24, gy + 20);
            // Label with shadow.
            canvas.draw_text(cell.x + 6, cell.y + 56, icon.label(), 0x00000000, None);
            canvas.draw_text(cell.x + 5, cell.y + 55, icon.label(), 0x00E6ECF2, None);
        }

        // The hover tooltip, near the bottom of the hovered icon.
        if self.tip.visible {
            if let Some(index) = self.hover {
                if let Some(icon) = self.layer.icon(index) {
                    let text = icon.label();
                    let w = nova_ui::Canvas::text_width(text) as i32 + 20;
                    let bx = (icon.cell.x + 60).min(self.width as i32 - w - 8);
                    let by = icon.cell.y + 84;
                    canvas.fill_rect(Rect::new(bx, by, w as u32, 26), 0x00FFF7B0);
                    canvas.draw_text(bx + 10, by + 5, text, 0x00201A08, Some(0x00FFF7B0));
                }
            }
        }

        // The panel, with the clock read from the wall clock. The text
        // is copied first: `panel.draw` needs `&mut self.panel`, which
        // cannot coexist with a borrow of `self` for the string.
        let panel_top = self.height as i32 - self.panel.height() as i32;
        let mut clock_buf = [0u8; 6];
        let clock_len = self.clock_len;
        clock_buf[..clock_len].copy_from_slice(&self.clock_text[..clock_len]);
        let clock = core::str::from_utf8(&clock_buf[..clock_len]).unwrap_or("");
        self.panel
            .draw(canvas, panel_top, self.notify.pending() > 0, clock);

        // The EDE menu, if open.
        if self.start_menu {
            self.draw_ede_menu(canvas);
        }

        // The notification balloon, if any.
        if let Some(note) = self.notify.current() {
            let bx = self.width as i32 - 280;
            let by = panel_top - 96;
            canvas.fill_rect(Rect::new(bx, by, 256, 88), 0x00172028);
            canvas.stroke_rect(Rect::new(bx, by, 256, 88), 0x0068AAC4);
            canvas.draw_text(bx + 10, by + 8, note.app(), 0x0068AAC4, Some(0x00172028));
            canvas.draw_text(
                bx + 10,
                by + 30,
                note.summary(),
                0x00E6ECF2,
                Some(0x00172028),
            );
            canvas.draw_text(bx + 10, by + 52, note.body(), 0x008A93A4, Some(0x00172028));
        }

        // The About dialog, if open.
        if self.about {
            self.draw_about(canvas);
        }

        // The Run dialog, if open — topmost of all.
        if self.run_open {
            self.draw_run_dialog(canvas);
        }
    }

    fn draw_about(&self, canvas: &mut Canvas) {
        let about = AboutContent::novaos();
        let bx = (self.width as i32 - 360) / 2;
        let by = (self.height as i32 - 280) / 2;
        let rect = Rect::new(bx, by, 360, 280);
        canvas.fill_rect(rect, palette::SURFACE);
        canvas.stroke_rect(rect, palette::BORDER);
        nova_ui::draw_titlebar_at(canvas, bx, by, 360, "About EDE", true);
        canvas.draw_text(
            bx + 20,
            by + 32,
            about.product,
            palette::TEXT,
            Some(palette::SURFACE),
        );
        canvas.draw_text(
            bx + 20,
            by + 54,
            about.version,
            palette::CHROME,
            Some(palette::SURFACE),
        );
        canvas.draw_text(
            bx + 20,
            by + 76,
            about.tagline,
            palette::TEXT_DIM,
            Some(palette::SURFACE),
        );
        for (index, line) in about.credits.iter().enumerate() {
            canvas.draw_text(
                bx + 20,
                by + 104 + index as i32 * 18,
                line,
                palette::TEXT,
                Some(palette::SURFACE),
            );
        }
        canvas.draw_text(
            bx + 20,
            by + 254,
            "click anywhere to close",
            palette::TEXT_DIM,
            Some(palette::SURFACE),
        );
    }

    /// Draw the EDE menu: a header bar over a page of rows, in the
    /// theme's near-white body with the title blue as the selection.
    fn draw_ede_menu(&mut self, canvas: &mut Canvas) {
        let rows = self.menu_row_count();
        let height = self.menu_height();
        let by = self.menu_top();
        let bx = MENU_X;
        let width = MENU_WIDTH as u32;

        // Body and border.
        canvas.fill_rect(Rect::new(bx, by, width, height as u32), palette::MENU_FACE);
        canvas.stroke_rect(Rect::new(bx, by, width, height as u32), palette::BORDER);

        // Header: the page title, and a "<" back affordance on subpages.
        canvas.fill_rect(
            Rect::new(bx, by, width, MENU_HEADER_H as u32),
            palette::MENU_HEADER,
        );
        canvas.draw_text(
            bx + 8,
            by + 4,
            self.menu_title(),
            palette::TEXT_ON_CHROME,
            Some(palette::MENU_HEADER),
        );
        if self.menu_page != MenuPage::Root {
            canvas.draw_text(
                bx + MENU_WIDTH - 16,
                by + 4,
                "<",
                palette::TEXT_ON_CHROME,
                Some(palette::MENU_HEADER),
            );
        }

        // Rows: the highlighted one wears the title blue.
        for row in 0..rows {
            let y = by + MENU_HEADER_H + (row as i32) * MENU_ROW_PITCH;
            let selected = row == self.menu_selection;
            let (label, submenu) = self.menu_row_text(row);
            if selected {
                canvas.fill_rect(
                    Rect::new(bx + 1, y, width - 2, MENU_ROW_PITCH as u32),
                    palette::CHROME,
                );
            }
            let (text, bg) = if selected {
                (palette::TEXT_ON_CHROME, Some(palette::CHROME))
            } else {
                (palette::TEXT, Some(palette::MENU_FACE))
            };
            canvas.draw_text(bx + 10, y + 5, label, text, bg);
            if submenu {
                // A small right-pointing triangle at the row's end.
                let ax = bx + MENU_WIDTH - 18;
                let ay = y + 7;
                for step in 0..5 {
                    canvas.fill_rect(
                        Rect::new(ax + step, ay + step, 1, (10 - step * 2) as u32),
                        text,
                    );
                }
            }
        }
    }

    /// Draw the Run dialog: an EDE window with one text field and
    /// OK/Cancel buttons.
    fn draw_run_dialog(&mut self, canvas: &mut Canvas) {
        let rect = Rect::new(
            self.run_dialog_x(),
            self.run_dialog_y(),
            RUN_DIALOG_W as u32,
            RUN_DIALOG_H as u32,
        );
        canvas.fill_rect(rect, palette::SURFACE);
        canvas.stroke_rect(rect, palette::BORDER);
        nova_ui::draw_titlebar_at(
            canvas,
            self.run_dialog_x(),
            self.run_dialog_y(),
            RUN_DIALOG_W,
            "Run...",
            true,
        );
        canvas.draw_text(
            self.run_dialog_x() + 16,
            self.run_dialog_y() + 34,
            "Command:",
            palette::TEXT,
            Some(palette::SURFACE),
        );
        // The field: white with the theme's border, caret at the end
        // of what has been typed.
        let field = Rect::new(
            self.run_dialog_x() + 16,
            self.run_dialog_y() + 54,
            (RUN_DIALOG_W - 32) as u32,
            22,
        );
        canvas.fill_rect(field, palette::FIELD);
        canvas.stroke_rect(field, palette::BORDER);
        let typed = core::str::from_utf8(&self.run_input[..self.run_len]).unwrap_or("");
        canvas.draw_text(
            field.x + 6,
            field.y + 3,
            typed,
            palette::TEXT,
            Some(palette::FIELD),
        );
        let caret_x = field.x + 6 + Canvas::text_width(typed) as i32 + 1;
        canvas.fill_rect(Rect::new(caret_x, field.y + 3, 1, 16), palette::TEXT);
        self.run_ok.draw(canvas);
        self.run_cancel.draw(canvas);
    }

    /// Launch a service by name, reporting a failure honestly: a
    /// missing service is an error the console sees, not silence.
    fn launch(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        match process::exec(name) {
            Some(_) => {
                let _ = console::write_fmt(format_args!("ede-shell: launch {name}\r\n"));
            }
            None => {
                let _ = console::write_fmt(format_args!(
                    "ede-shell: launch {name} failed: no such service\r\n"
                ));
            }
        }
    }

    /// Resolve a start-menu row to the entry it shows.
    /// A key while the start menu is open: type to filter, arrows to
    /// move the selection, Enter to launch, Escape to close.
    fn on_key(&mut self, event: &InputEvent) -> bool {
        let Some(key) = self
            .decoder
            .on_input(event.kind, event.state, event.scancode)
        else {
            return false;
        };

        // The Run dialog takes the keyboard while it is open.
        if self.run_open {
            return self.run_key(key);
        }

        if !self.start_menu {
            if key == Key::Escape && self.about {
                self.about = false;
                return true;
            }
            return false;
        }

        match key {
            Key::Escape => {
                // Escape climbs one page; at the root it closes.
                if !self.menu_back() {
                    self.close_menu();
                }
                true
            }
            Key::Char(byte) => {
                // Typing filters the applications on a category page;
                // the other pages have nothing to filter.
                if matches!(self.menu_page, MenuPage::Category(_)) {
                    self.launchpad.type_char(byte);
                    self.menu_selection = 0;
                    true
                } else {
                    false
                }
            }
            Key::Backspace => {
                if matches!(self.menu_page, MenuPage::Category(_)) {
                    let removed = self.launchpad.backspace();
                    if removed {
                        self.menu_selection = 0;
                    }
                    removed
                } else {
                    false
                }
            }
            Key::Up => self.menu_move(-1),
            Key::Down => self.menu_move(1),
            Key::Left => self.menu_back(),
            Key::Right | Key::Enter => {
                let row = self.menu_selection;
                self.activate_menu_row(row)
            }
            _ => false,
        }
    }

    /// A key while the Run dialog is open: type the command, Enter
    /// runs it, Escape cancels.
    fn run_key(&mut self, key: Key) -> bool {
        match key {
            Key::Escape => {
                self.run_open = false;
                true
            }
            Key::Char(byte) => {
                if (0x20..0x7f).contains(&byte) && self.run_len < RUN_INPUT_MAX {
                    self.run_input[self.run_len] = byte;
                    self.run_len += 1;
                    true
                } else {
                    false
                }
            }
            Key::Backspace => {
                if self.run_len > 0 {
                    self.run_len -= 1;
                    true
                } else {
                    false
                }
            }
            Key::Enter => {
                self.run_submit();
                true
            }
            _ => false,
        }
    }

    /// Run the Run dialog's command: the typed text is an executable
    /// name, exactly what `ede-launch`'s Run line accepts.
    fn run_submit(&mut self) {
        let mut buffer = [0u8; RUN_INPUT_MAX];
        let len = self.run_len;
        buffer[..len].copy_from_slice(&self.run_input[..len]);
        self.run_open = false;
        self.run_len = 0;
        let text = core::str::from_utf8(&buffer[..len]).unwrap_or("");
        self.launch(text);
    }

    /// Handle one pointer event, on the button's *edges*.
    ///
    /// `pressed` is the up→down transition, `released` the down→up one;
    /// a motion event while the button is held is neither, which is
    /// what keeps a hover from launching anything.
    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        let panel_top = self.height as i32 - self.panel.height() as i32;
        let held = event.buttons & INPUT_BUTTON_PRIMARY != 0;
        let was_held = self.prev_buttons & INPUT_BUTTON_PRIMARY != 0;
        let pressed = held && !was_held;
        let released = !held && was_held;
        let secondary = event.buttons & INPUT_BUTTON_SECONDARY != 0;
        let secondary_pressed = secondary && self.prev_buttons & INPUT_BUTTON_SECONDARY == 0;
        self.prev_buttons = event.buttons;
        self.pointer_x = event.x;
        self.pointer_y = event.y;

        // The Run dialog is modal: its buttons and field take the
        // pointer, and Escape (in on_key) or Cancel closes it.
        if self.run_open {
            if self.run_ok.on_pointer(event.x, event.y, held) {
                self.run_submit();
                return true;
            }
            if self.run_cancel.on_pointer(event.x, event.y, held) {
                self.run_open = false;
                return true;
            }
            if secondary_pressed {
                self.run_open = false;
                return true;
            }
            return false;
        }

        // About dialog is modal: a press closes it.
        if self.about {
            if pressed || secondary_pressed {
                self.about = false;
                return true;
            }
            return false;
        }

        // The EDE menu, if open, eats everything above the panel.
        if self.start_menu && event.y < panel_top {
            if secondary_pressed {
                // Right-click climbs one page, closing at the root.
                if !self.menu_back() {
                    self.close_menu();
                }
                return true;
            }
            if pressed {
                return self.click_ede_menu(event.x, event.y);
            }
            return false;
        }

        // Panel hit test — actions fire on the press edge only.
        if event.y >= panel_top {
            if pressed {
                let panel_y = event.y - panel_top;
                let hit = self.panel.hit(event.x, panel_y);
                match self.panel.action_for(hit) {
                    Some(PanelAction::OpenStartMenu) => {
                        self.start_menu = !self.start_menu;
                        if self.start_menu {
                            // Every opening starts at the root page
                            // with a clean filter.
                            self.launchpad.clear();
                            self.menu_page = MenuPage::Root;
                            self.menu_selection = 0;
                        }
                    }
                    Some(PanelAction::Launch(name)) => self.launch(name),
                    Some(PanelAction::Activate(id)) => {
                        let _ = graphics::compositor_activate(id);
                    }
                    Some(PanelAction::Minimize(id)) => {
                        let _ = graphics::compositor_minimize(id, true);
                    }
                    Some(PanelAction::ShowPanel) => self.panel.set_hider_open(true),
                    None => {}
                }
                return true;
            }
            return false;
        }

        // Balloon hit test: a press on the balloon dismisses it.
        if event.y >= panel_top - 96 && event.x >= (self.width as i32) - 280 {
            if pressed {
                if let Some(note) = self.notify.current() {
                    let id = note.id;
                    self.notify.dismiss(id);
                    return true;
                }
            }
            return false;
        }

        // A drag resolves here: movement makes it a move, a still
        // release activates the icon the press landed on.
        if let Some(drag) = self.drag {
            if held {
                let dx = event.x - drag.origin_x;
                let dy = event.y - drag.origin_y;
                if !drag.moved && dx.abs() + dy.abs() > 3 {
                    if let Some(active) = self.drag.as_mut() {
                        active.moved = true;
                    }
                }
                if let Some(active) = self.drag {
                    if active.moved {
                        let x = event.x - active.grab_dx;
                        let y = event.y - active.grab_dy;
                        let index = active.index;
                        self.layer.move_to(index, x, y);
                        return true;
                    }
                }
                // Held but not yet moved: nothing on screen changed.
                return false;
            }
            if released {
                let drag = self.drag.take().expect("checked above");
                if !drag.moved {
                    let mut name = [0u8; 48];
                    let len = self
                        .layer
                        .icon(drag.index)
                        .map(|icon| {
                            let bytes = icon.exec().as_bytes();
                            let take = bytes.len().min(name.len());
                            name[..take].copy_from_slice(&bytes[..take]);
                            take
                        })
                        .unwrap_or(0);
                    self.layer.select(None);
                    self.launch(core::str::from_utf8(&name[..len]).unwrap_or(""));
                    return true;
                }
                // The release just ends a drag whose move was already
                // drawn: no further change.
                return false;
            }
            self.drag = None;
        }

        // Right-click: select the icon under the pointer, or — on empty
        // ground — close the menu and clear the selection. A full
        // context menu is EDE's `ede-desktop` menu, which this port does
        // not yet carry; what must not happen is a right click that
        // does nothing at all.
        if secondary_pressed {
            if let Some(index) = self.layer.hit(event.x, event.y) {
                let was_selected = self.layer.icon(index).is_some_and(|icon| icon.selected);
                if !was_selected {
                    self.layer.select(Some(index));
                    return true;
                }
                return false;
            }
            let mut changed = false;
            if self.start_menu {
                self.start_menu = false;
                self.launchpad.clear();
                changed = true;
            }
            if self.layer.selected_count() > 0 {
                self.layer.select(None);
                changed = true;
            }
            return changed;
        }

        // Desktop icons: a press selects; a press on the already
        // selected icon begins a drag — or, without movement, launches.
        if pressed {
            if let Some(index) = self.layer.hit(event.x, event.y) {
                let was_selected = self.layer.icon(index).is_some_and(|icon| icon.selected);
                if was_selected && !self.desktop_conf.one_click_exec {
                    if let Some(icon) = self.layer.icon(index) {
                        self.drag = Some(Drag {
                            index,
                            grab_dx: event.x - icon.cell.x,
                            grab_dy: event.y - icon.cell.y,
                            origin_x: event.x,
                            origin_y: event.y,
                            moved: false,
                        });
                    }
                    // Arming a drag draws nothing until it moves.
                    return false;
                } else if self.desktop_conf.one_click_exec {
                    // One-click activation: launch on the press, like
                    // EDE's `Icons/one_click_exec = true`.
                    let mut name = [0u8; 48];
                    let len = self
                        .layer
                        .icon(index)
                        .map(|icon| {
                            let bytes = icon.exec().as_bytes();
                            let take = bytes.len().min(name.len());
                            name[..take].copy_from_slice(&bytes[..take]);
                            take
                        })
                        .unwrap_or(0);
                    self.layer.select(Some(index));
                    self.launch(core::str::from_utf8(&name[..len]).unwrap_or(""));
                } else {
                    self.layer.select(Some(index));
                }
                return true;
            }
            let changed = self.layer.selected_count() > 0;
            self.layer.select(None);
            return changed;
        }

        // Track hover for the tooltip. The state always advances, but
        // the screen changes only when the hover target or the
        // tooltip's visibility does — a motion event across empty
        // wallpaper costs no repaint; the cursor is the compositor's
        // to draw.
        let old_hover = self.hover;
        let old_tip = self.tip.visible;
        let hover = self.layer.hit(event.x, event.y);
        if hover == self.hover {
            self.hover_ticks = self.hover_ticks.saturating_add(1);
        } else {
            self.hover = hover;
            self.hover_ticks = 0;
        }
        if let Some(index) = hover {
            let name = self.layer.icon(index).map(|i| i.label()).unwrap_or("");
            self.tip.set_text(name);
            self.tip.visible = self.hover_ticks >= self.tip.delay;
        } else {
            self.tip.leave();
        }
        self.hover != old_hover || self.tip.visible != old_tip
    }

    /// A press inside the open start menu: a row launches, the About
    /// row opens About, anywhere else closes the menu.
    /// A press inside the open start menu: a row launches, the About
    /// row opens About, empty space inside the popup does nothing, and
    /// a press outside closes the menu. Returns whether the screen
    /// changed.
    /// A press inside the open EDE menu: the header goes up a page,
    /// a row activates, and a press outside closes the menu.
    fn click_ede_menu(&mut self, x: i32, y: i32) -> bool {
        let rows = self.menu_row_count();
        let by = self.menu_top();
        // The header: "<" goes up one page on subpages.
        if y >= by && y < by + MENU_HEADER_H && x >= MENU_X && x < MENU_X + MENU_WIDTH {
            if self.menu_page != MenuPage::Root {
                return self.menu_back();
            }
            return false;
        }
        for row in 0..rows {
            let row_y = by + MENU_HEADER_H + (row as i32) * MENU_ROW_PITCH;
            if y >= row_y && y < row_y + MENU_ROW_PITCH && x >= MENU_X && x < MENU_X + MENU_WIDTH {
                return self.activate_menu_row(row);
            }
        }
        // Outside the popup: close.
        let height = self.menu_height();
        let inside = y >= by && y < by + height && x >= MENU_X && x < MENU_X + MENU_WIDTH;
        if !inside {
            self.close_menu();
            return true;
        }
        false
    }

    /// Activate row `row` of the current menu page: descend into a
    /// submenu, launch an application, activate a window, open the
    /// Run dialog, or show About.
    fn activate_menu_row(&mut self, row: usize) -> bool {
        match self.menu_page {
            MenuPage::Root => match row {
                0 => {
                    self.menu_page = MenuPage::Programs;
                    self.menu_selection = 0;
                }
                1 => {
                    self.menu_page = MenuPage::WindowList;
                    self.menu_selection = 0;
                }
                2 => self.open_run_dialog(),
                _ => {
                    self.about = true;
                    self.close_menu();
                }
            },
            MenuPage::Programs => {
                if self.launchpad.category_at(row).is_some() {
                    self.menu_page = MenuPage::Category(row);
                    self.menu_selection = 0;
                }
            }
            MenuPage::Category(category) => {
                let Some(name) = self.launchpad.category_at(category) else {
                    return false;
                };
                let Some(index) = self.launchpad.matching_in_category(name).nth(row) else {
                    return false;
                };
                let Some(entry) = self.launchpad.entry(index) else {
                    return false;
                };
                let mut command = [0u8; 48];
                let bytes = entry.exec().as_bytes();
                let take = bytes.len().min(command.len());
                command[..take].copy_from_slice(&bytes[..take]);
                self.close_menu();
                self.launch(core::str::from_utf8(&command[..take]).unwrap_or(""));
            }
            MenuPage::WindowList => {
                let Some(entry) = self.window_list.get(row) else {
                    return false;
                };
                let id = entry.id;
                self.close_menu();
                let _ = graphics::compositor_activate(id);
            }
        }
        true
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    typeface::set_face(HAN_SANS);

    let _ = console::write_str("[NOVA] desktop-shell ready\r\n");

    // Lay the desktop out for the mode the GPU actually scans out. The
    // device is the authority: a size assumed here would be wrong on any
    // resolution but that one, and the wallpaper, panel and dialog
    // placement all follow from it.
    let (width, height) = match graphics::display_query() {
        Ok(mode) if mode.width >= 320 && mode.height >= 240 => (mode.width, mode.height),
        _ => (DEFAULT_WIDTH, DEFAULT_HEIGHT),
    };

    let _ = console::write_fmt(format_args!(
        "[NOVA] desktop: {}x{} from the display service\r\n",
        width, height
    ));

    let mut window = match Window::open_bottom(width, height) {
        Ok(window) => window,
        Err(error) => {
            let _ = console::write_fmt(format_args!(
                "desktop-shell: window failed {}\r\n",
                error.name()
            ));
            return -1;
        }
    };
    let _ = graphics::compositor_move(window.surface, 0, 0);

    let mut canvas = Canvas::new(window.buffer, width, height);
    let mut shell = Shell::new(width, height);
    shell.own_surface = window.surface;
    shell.refresh_clock();
    let mut events = [InputEvent::EMPTY; MAX_EVENTS];

    shell.draw(&mut canvas);
    if let Err(error) = window.present() {
        let _ = console::write_fmt(format_args!(
            "desktop-shell: present failed {}\r\n",
            error.name()
        ));
        return -1;
    }

    loop {
        let count = window.drain(&mut events).unwrap_or(0);
        let mut dirty = false;

        for event in &events[..count] {
            match event.kind {
                INPUT_EVENT_POINTER => {
                    dirty |= shell.on_pointer(event);
                }
                INPUT_EVENT_KEY => {
                    dirty |= shell.on_key(event);
                }
                _ => {}
            }
        }
        shell.ticks = shell.ticks.saturating_add(1);
        let balloons = shell.notify.pending();
        shell.notify.tick();
        if shell.notify.pending() != balloons {
            // A balloon expired: the corner changed.
            dirty = true;
        }
        if shell.ticks % 16 == 0 {
            dirty |= shell.drain_notifications();
        }
        if shell.ticks % 64 == 0 {
            dirty |= shell.refresh_windows();
        }
        if shell.ticks % 512 == 0 {
            // The wall clock is an RTC read: poll it when the minute
            // can have turned, not every frame.
            dirty |= shell.refresh_clock();
        }
        dirty |= shell.tick_hider();
        if dirty {
            shell.draw(&mut canvas);
            if let Err(error) = window.present() {
                let _ = console::write_fmt(format_args!(
                    "desktop-shell: present failed {}\r\n",
                    error.name()
                ));
                return -1;
            }
        }

        yield_now();
    }
}
