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
use nova_ui::{Canvas, Rect, typeface};

static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;
const MAX_EVENTS: usize = 8;

/// The start menu's geometry, in surface coordinates.
const MENU_WIDTH: i32 = 240;
const MENU_OFFSET: i32 = 264;
const MENU_ROWS_TOP: i32 = 70;
const MENU_ROW_PITCH: i32 = 34;
const MENU_ROW_HEIGHT: i32 = 28;
/// The menu fits four application rows above the About row.
const MAX_MENU_ROWS: usize = 4;

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
    /// Ticks the pointer has been away from an auto-hiding panel.
    hider_ticks: u32,
    /// The window list's fingerprint as last published: membership and
    /// state, not the round trip, is what warrants a redraw.
    windows_fingerprint: Option<u64>,
    /// The name of a notification file that resisted deletion: its
    /// content is posted once, not again on every drain.
    stuck_notify: [u8; 32],
    stuck_notify_len: usize,
}

impl Shell {
    fn new() -> Self {
        let mut shell = Self {
            layer: DesktopLayer::new(),
            start_menu: false,
            about: false,
            launchpad: Launchpad::new(),
            notify: NotifyBus::new(),
            panel: Panel::new(WIDTH),
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
            hider_ticks: 0,
            windows_fingerprint: None,
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
        let panel_top = HEIGHT as i32 - self.panel.height() as i32;
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

    /// The start menu's top edge, in surface coordinates.
    fn menu_top(&self) -> i32 {
        HEIGHT as i32 - self.panel.height() as i32 - MENU_OFFSET
    }

    /// How many application rows the menu shows right now.
    fn menu_rows(&self) -> usize {
        self.launchpad.matches().min(MAX_MENU_ROWS)
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
            canvas.fill_rect(Rect::new(0, 0, WIDTH, HEIGHT), color);
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
                canvas.fill_rect(cell, 0x0068AAC4);
                canvas.fill_rect(cell.inset(2), 0x001A2A3A);
            }
            // Glyph plate and a simple glyph per app.
            let gx = cell.x + 28;
            let gy = cell.y + 8;
            canvas.fill_rect(Rect::new(gx, gy, 48, 40), 0x001A2A3A);
            canvas.stroke_rect(Rect::new(gx, gy, 48, 40), 0x0068AAC4);
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
                    let bx = (icon.cell.x + 60).min(WIDTH as i32 - w - 8);
                    let by = icon.cell.y + 84;
                    canvas.fill_rect(Rect::new(bx, by, w as u32, 26), 0x00FFF7B0);
                    canvas.draw_text(bx + 10, by + 5, text, 0x00201A08, Some(0x00FFF7B0));
                }
            }
        }

        // The panel.
        let panel_top = HEIGHT as i32 - self.panel.height() as i32;
        self.panel
            .draw(canvas, panel_top, self.notify.pending() > 0);

        // The start menu, if open.
        if self.start_menu {
            self.draw_start_menu(canvas);
        }

        // The notification balloon, if any.
        if let Some(note) = self.notify.current() {
            let bx = WIDTH as i32 - 280;
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
    }

    fn draw_about(&self, canvas: &mut Canvas) {
        let about = AboutContent::novaos();
        let bx = (WIDTH as i32 - 360) / 2;
        let by = (HEIGHT as i32 - 280) / 2;
        canvas.fill_rect(Rect::new(bx, by, 360, 280), 0x001B2530);
        canvas.stroke_rect(Rect::new(bx, by, 360, 280), 0x0068AAC4);
        canvas.draw_text(
            bx + 20,
            by + 16,
            about.product,
            0x00E6ECF2,
            Some(0x001B2530),
        );
        canvas.draw_text(
            bx + 20,
            by + 40,
            about.version,
            0x0068AAC4,
            Some(0x001B2530),
        );
        canvas.draw_text(
            bx + 20,
            by + 62,
            about.tagline,
            0x00C8D4DE,
            Some(0x001B2530),
        );
        for (index, line) in about.credits.iter().enumerate() {
            canvas.draw_text(
                bx + 20,
                by + 92 + index as i32 * 18,
                line,
                0x008A93A4,
                Some(0x001B2530),
            );
        }
        canvas.draw_text(
            bx + 20,
            by + 250,
            "click anywhere to close",
            0x005A6270,
            Some(0x001B2530),
        );
    }

    fn draw_start_menu(&mut self, canvas: &mut Canvas) {
        let bx = 8;
        let by = self.menu_top();
        // Deep slate popup with an accent edge.
        canvas.fill_rect(Rect::new(bx, by, MENU_WIDTH as u32, 260), 0x00152022);
        canvas.stroke_rect(Rect::new(bx, by, MENU_WIDTH as u32, 260), 0x0068AAC4);
        // Header: comet + wordmark + tagline (or the active filter).
        nova_ede_panel::draw_comet(canvas, bx + 18, by + 16, 0x0068AAC4);
        canvas.draw_text(bx + 44, by + 12, "NovaOS", 0x00E6ECF2, Some(0x00152022));
        let filter = self.launchpad.filter();
        let subtitle = if filter.is_empty() {
            "EDE, reborn in Rust"
        } else {
            filter
        };
        canvas.draw_text(bx + 44, by + 30, subtitle, 0x007E8896, Some(0x00152022));
        canvas.fill_rect(Rect::new(bx, by + 58, MENU_WIDTH as u32, 1), 0x001F3142);
        // The matching applications, one row each, selected row lit.
        let selected = self.launchpad.selected_index();
        let mut row = 0u32;
        for index in self.launchpad.matching_indices() {
            if row as usize >= MAX_MENU_ROWS {
                break;
            }
            let Some(entry) = self.launchpad.entry(index) else {
                continue;
            };
            let y = by + MENU_ROWS_TOP + (row as i32) * MENU_ROW_PITCH;
            let bg = if row as usize == selected {
                0x0023485C
            } else {
                0x001B2836
            };
            canvas.fill_rect(
                Rect::new(bx + 8, y, (MENU_WIDTH - 16) as u32, MENU_ROW_HEIGHT as u32),
                bg,
            );
            nova_ede_panel::glyph(canvas, entry.exec(), bx + 22, y + 14);
            canvas.draw_text(bx + 40, y + 7, entry.name(), 0x00E6ECF2, Some(bg));
            row += 1;
        }
        // The About row.
        let y = by + MENU_ROWS_TOP + (row as i32) * MENU_ROW_PITCH;
        canvas.fill_rect(
            Rect::new(bx + 8, y, (MENU_WIDTH - 16) as u32, MENU_ROW_HEIGHT as u32),
            0x001B2836,
        );
        canvas.draw_text(bx + 40, y + 7, "About NovaOS", 0x00E6A65C, Some(0x001B2836));
        // Footer: the credit line, the EDE lineage and the GPL.
        canvas.fill_rect(Rect::new(bx, by + 236, MENU_WIDTH as u32, 1), 0x001F3142);
        canvas.draw_text(
            bx + 12,
            by + 244,
            "NovaOS EDE 0.1.0 - GPL-2.0",
            0x00406077,
            Some(0x00152022),
        );
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
    fn menu_entry(&self, row: usize) -> Option<nova_ede_launch::AppEntry> {
        let index = self.launchpad.matching_indices().nth(row)?;
        self.launchpad.entry(index)
    }

    /// A key while the start menu is open: type to filter, arrows to
    /// move the selection, Enter to launch, Escape to close.
    fn on_key(&mut self, event: &InputEvent) -> bool {
        let Some(key) = self
            .decoder
            .on_input(event.kind, event.state, event.scancode)
        else {
            return false;
        };
        if !self.start_menu {
            if key == Key::Escape && self.about {
                self.about = false;
                return true;
            }
            return false;
        }
        match key {
            Key::Escape => {
                self.start_menu = false;
                self.launchpad.clear();
                true
            }
            Key::Char(byte) => {
                self.launchpad.type_char(byte);
                true
            }
            Key::Backspace => {
                self.launchpad.backspace();
                true
            }
            Key::Up => {
                self.launchpad.move_selection(-1);
                true
            }
            Key::Down => {
                self.launchpad.move_selection(1);
                true
            }
            Key::Enter => {
                if let Some(entry) = self.menu_entry(self.launchpad.selected_index()) {
                    let mut name = [0u8; 48];
                    let bytes = entry.exec().as_bytes();
                    let take = bytes.len().min(name.len());
                    name[..take].copy_from_slice(&bytes[..take]);
                    self.start_menu = false;
                    self.launchpad.clear();
                    self.launch(core::str::from_utf8(&name[..take]).unwrap_or(""));
                }
                true
            }
            _ => false,
        }
    }

    /// Handle one pointer event, on the button's *edges*.
    ///
    /// `pressed` is the up→down transition, `released` the down→up one;
    /// a motion event while the button is held is neither, which is
    /// what keeps a hover from launching anything.
    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        let panel_top = HEIGHT as i32 - self.panel.height() as i32;
        let held = event.buttons & INPUT_BUTTON_PRIMARY != 0;
        let was_held = self.prev_buttons & INPUT_BUTTON_PRIMARY != 0;
        let pressed = held && !was_held;
        let released = !held && was_held;
        let secondary = event.buttons & INPUT_BUTTON_SECONDARY != 0;
        let secondary_pressed = secondary && self.prev_buttons & INPUT_BUTTON_SECONDARY == 0;
        self.prev_buttons = event.buttons;
        self.pointer_x = event.x;
        self.pointer_y = event.y;

        // About dialog is modal: a press closes it.
        if self.about {
            if pressed || secondary_pressed {
                self.about = false;
                return true;
            }
            return false;
        }

        // The start menu, if open, eats everything above the panel.
        if self.start_menu && event.y < panel_top {
            if secondary_pressed {
                self.start_menu = false;
                self.launchpad.clear();
                return true;
            }
            if pressed {
                return self.click_start_menu(event.x, event.y);
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
                            self.launchpad.clear();
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
        if event.y >= panel_top - 96 && event.x >= (WIDTH as i32) - 280 {
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
    fn click_start_menu(&mut self, x: i32, y: i32) -> bool {
        let by = self.menu_top();
        let rows = self.menu_rows();
        for row in 0..rows {
            let row_y = by + MENU_ROWS_TOP + (row as i32) * MENU_ROW_PITCH;
            if y >= row_y && y < row_y + MENU_ROW_HEIGHT && x >= 8 && x < 8 + MENU_WIDTH {
                if let Some(entry) = self.menu_entry(row) {
                    let mut name = [0u8; 48];
                    let bytes = entry.exec().as_bytes();
                    let take = bytes.len().min(name.len());
                    name[..take].copy_from_slice(&bytes[..take]);
                    self.start_menu = false;
                    self.launchpad.clear();
                    self.launch(core::str::from_utf8(&name[..take]).unwrap_or(""));
                    return true;
                }
                return false;
            }
        }
        let about_y = by + MENU_ROWS_TOP + (rows as i32) * MENU_ROW_PITCH;
        if y >= about_y && y < about_y + MENU_ROW_HEIGHT && x >= 8 && x < 8 + MENU_WIDTH {
            self.about = true;
            self.start_menu = false;
            self.launchpad.clear();
            return true;
        }
        // Inside the popup but on no row: leave it open. Outside: close.
        let inside = y >= by && y < by + 260 && x >= 8 && x < 8 + MENU_WIDTH;
        if !inside {
            self.start_menu = false;
            self.launchpad.clear();
            return true;
        }
        false
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i64 {
    typeface::set_face(HAN_SANS);

    let _ = console::write_str("[NOVA] desktop-shell ready\r\n");

    let mut window = match Window::open(WIDTH, HEIGHT) {
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

    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);
    let mut shell = Shell::new();
    shell.own_surface = window.surface;
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
