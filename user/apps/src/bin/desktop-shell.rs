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
//!
//! # Widgets are data, not code
//!
//! Which icons appear, what the panel shows, what the start menu offers,
//! all come from `.desktop` files in the VFS — the same freedesktop
//! convention EDE used. The shell hardcodes nothing about applications.

use nova_apps::graphics::{InputEvent, Window};
use nova_apps::{console, fsclient, process, yield_now};
use nova_desktop::wallpaper;
use nova_ede_about::AboutContent;
use nova_ede_desktop::DesktopLayer;
use nova_ede_launch::Launchpad;
use nova_ede_notifyd::NotifyBus;
use nova_ede_panel::{self, Action as PanelAction, Panel};
use nova_ede_tip::Tooltip;
use nova_edelib::DesktopFile;
use nova_protocols::input::INPUT_EVENT_POINTER;
use nova_ui::{Canvas, Rect, typeface};

static HAN_SANS: &[u8] = include_bytes!("../../../../assets/fonts/han-sans-gui.ttf");

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;
const MAX_EVENTS: usize = 8;

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
        };
        shell.scan_desktop();
        shell
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
    fn drain_notifications(&mut self) {
        let mut page = [0u8; fsclient::PAGE_BYTES];
        let Ok(info) = fsclient::list_at(b"/var/run/notify", 0, &mut page) else {
            return;
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
            if let Ok(text) = core::str::from_utf8(&contents[..count]) {
                let _ = self.notify.post_wire(text);
            }
            let _ = fsclient::delete(full);
        }
    }

    fn draw(&mut self, canvas: &mut Canvas) {
        wallpaper::paint(canvas);

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
        let panel_top = HEIGHT as i32 - nova_ede_panel::PANEL_HEIGHT as i32;
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
    }

    fn draw_start_menu(&mut self, canvas: &mut Canvas) {
        let bx = 8;
        let by = HEIGHT as i32 - nova_ede_panel::PANEL_HEIGHT as i32 - 264;
        // Deep slate popup with an accent edge.
        canvas.fill_rect(Rect::new(bx, by, 240, 260), 0x00152022);
        canvas.stroke_rect(Rect::new(bx, by, 240, 260), 0x0068AAC4);
        // Header: comet + wordmark + tagline.
        nova_ede_panel::draw_comet(canvas, bx + 18, by + 16, 0x0068AAC4);
        canvas.draw_text(bx + 44, by + 12, "NovaOS", 0x00E6ECF2, Some(0x00152022));
        canvas.draw_text(
            bx + 44,
            by + 30,
            "EDE, reborn in Rust",
            0x007E8896,
            Some(0x00152022),
        );
        canvas.fill_rect(Rect::new(bx, by + 58, 240, 1), 0x001F3142);
        // Apps from the VFS + About entry.
        let mut row = 0u32;
        for index in 0..self.layer.len() {
            let Some(icon) = self.layer.icon(index) else {
                continue;
            };
            let y = by + 70 + (row as i32) * 34;
            canvas.fill_rect(Rect::new(bx + 8, y, 224, 28), 0x001B2836);
            nova_ede_panel::glyph(canvas, icon.exec(), bx + 22, y + 14);
            canvas.draw_text(bx + 40, y + 7, icon.label(), 0x00E6ECF2, Some(0x001B2836));
            row += 1;
        }
        let y = by + 70 + (row as i32) * 34;
        canvas.fill_rect(Rect::new(bx + 8, y, 224, 28), 0x001B2836);
        canvas.draw_text(bx + 40, y + 7, "About NovaOS", 0x00E6A65C, Some(0x001B2836));
        // Footer: the credit line, the EDE lineage and the GPL.
        canvas.fill_rect(Rect::new(bx, by + 236, 240, 1), 0x001F3142);
        canvas.draw_text(
            bx + 12,
            by + 244,
            "NovaOS EDE 0.1.0 - GPL-2.0",
            0x00406077,
            Some(0x00152022),
        );
    }

    fn launch(&self, name: &str) {
        let _ = console::write_fmt(format_args!("ede-shell: launch {name}\r\n"));
        let _ = process::exec(name);
    }

    fn on_pointer(&mut self, event: &InputEvent) -> bool {
        // The panel lives in the bottom strip of the surface.
        let panel_top = HEIGHT as i32 - nova_ede_panel::PANEL_HEIGHT as i32;
        let panel_y = event.y - panel_top;

        // About dialog is modal: any click closes it.
        if self.about {
            self.about = false;
            return true;
        }

        // The start menu, if open, eats clicks.
        if self.start_menu {
            let hit_panel = event.y >= panel_top;
            if !hit_panel {
                // Which row was clicked?
                let mut row = 0i32;
                for index in 0..self.layer.len() {
                    let y =
                        HEIGHT as i32 - nova_ede_panel::PANEL_HEIGHT as i32 - 264 + 70 + row * 34;
                    if event.y >= y && event.y < y + 28 && event.x >= 8 && event.x < 240 {
                        let Some(icon) = self.layer.icon(index) else {
                            continue;
                        };
                        let mut name = [0u8; 48];
                        let bytes = icon.exec().as_bytes();
                        name[..bytes.len()].copy_from_slice(bytes);
                        self.start_menu = false;
                        self.launch(core::str::from_utf8(&name[..bytes.len()]).unwrap_or(""));
                        return true;
                    }
                    row += 1;
                }
                let y = HEIGHT as i32 - nova_ede_panel::PANEL_HEIGHT as i32 - 264 + 70 + row * 34;
                if event.y >= y && event.y < y + 36 && event.x >= 8 && event.x < 240 {
                    self.about = true;
                    self.start_menu = false;
                    return true;
                }
                // Clicked elsewhere: close the menu.
                self.start_menu = false;
                return true;
            }
        }

        // Panel hit test.
        if panel_y >= 0 && panel_y < nova_ede_panel::PANEL_HEIGHT as i32 {
            let hit = self.panel.hit(event.x, panel_y);
            match self.panel.action_for(hit) {
                Some(PanelAction::OpenStartMenu) => {
                    self.start_menu = !self.start_menu;
                    return true;
                }
                Some(PanelAction::Launch(name)) => {
                    self.launch(name);
                    return true;
                }
                None => {}
            }
            return true;
        }

        // Balloon hit test: clicking the balloon dismisses it.
        if event.y >= panel_top - 96 && event.y < panel_top && event.x >= (WIDTH as i32) - 280 {
            // Dismiss = clear all pending.
            self.notify = nova_ede_notifyd::NotifyBus::new();
            return true;
        }

        // Track hover for the tooltip, whatever the event kind is.
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

        // Desktop icons: single click selects, second click launches.
        if event.buttons & nova_protocols::input::INPUT_BUTTON_PRIMARY != 0 {
            if let Some(index) = self.layer.hit(event.x, event.y) {
                let was_selected = self.layer.icon(index).map(|i| i.selected).unwrap_or(false);
                if was_selected {
                    let mut name = [0u8; 48];
                    let len = self.layer.icon(index).map(|i| i.exec().len()).unwrap_or(0);
                    name[..len].copy_from_slice(
                        self.layer
                            .icon(index)
                            .map(|i| i.exec().as_bytes())
                            .unwrap_or(&[]),
                    );
                    self.layer.select(None);
                    self.launch(core::str::from_utf8(&name[..len]).unwrap_or(""));
                } else {
                    self.layer.select(Some(index));
                }
                return true;
            }
            self.layer.select(None);
        }
        true
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
    let _ = nova_apps::graphics::compositor_move(window.surface, 0, 0);

    let mut canvas = Canvas::new(window.buffer, WIDTH, HEIGHT);
    let mut shell = Shell::new();
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
                _ => {}
            }
        }
        shell.ticks = shell.ticks.saturating_add(1);
        shell.notify.tick();
        if shell.ticks % 16 == 0 {
            shell.drain_notifications();
        }
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
