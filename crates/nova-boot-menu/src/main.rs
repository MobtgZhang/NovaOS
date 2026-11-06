//! NovaOS UEFI boot menu application (the menu layer of the NovaBoot unified
//! boot protocol, UEFI GOP graphical edition).
//!
//! Renders the OS-selection menu straight into the graphics framebuffer under
//! QEMU/OVMF — no text console involved.
//!
//! # Design boundary (important)
//!
//! This application only implements the "UEFI-phase graphical menu". It does
//! **not** hand control to the kernel entry point: NovaOS currently boots via
//! Limine (the `make run` path stays untouched), and a NovaBoot hand-off needs
//! a kernel-side reader that validates the `BootProtocol` snapshot — the next
//! phase. The jump site is marked with a TODO and no kernel boot through the
//! menu is claimed.
//!
//! Menu entries:
//!   1. Desktop (default)   -> boot into the desktop
//!   2. Safe Terminal        -> boot straight into a terminal
//!
//! Four architectures (x86_64, aarch64, riscv64, loongarch64) appear in the
//! protocol model, but only the x86-64 kernel backend is complete; the others
//! are protocol placeholders.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use uefi::Event;
use uefi::prelude::*;
use uefi::proto::console::gop::{BltPixel, GraphicsOutput, PixelFormat};
use uefi::proto::console::text::{Input, Key, ScanCode};
use uefi::table::boot::{BootServices, ScopedProtocol, SearchType};
use uefi::table::runtime::ResetType;

mod font;
use font::{CHAR_SPACING, FONT_HEIGHT, FONT_WIDTH, char_rows, text_width};

/// Colors (`BltPixel::new(red, green, blue)`).
const COLOR_BG: BltPixel = BltPixel::new(20, 20, 35); // dark blue-grey backdrop
const COLOR_PANEL: BltPixel = BltPixel::new(30, 30, 50); // panel body
const COLOR_TITLE_BG: BltPixel = BltPixel::new(40, 40, 70); // title bar
const COLOR_TEXT: BltPixel = BltPixel::new(255, 255, 255); // white text
const COLOR_HIGHLIGHT: BltPixel = BltPixel::new(100, 150, 255); // selection blue
const COLOR_HIGHLIGHT_TEXT: BltPixel = BltPixel::new(255, 255, 255);
const COLOR_DIM: BltPixel = BltPixel::new(150, 150, 170); // hint grey
const COLOR_BORDER: BltPixel = BltPixel::new(90, 90, 120); // panel border

/// A menu entry.
#[derive(Clone, Copy)]
struct MenuItem {
    key: char,           // shortcut '1' / '2'
    label: &'static str, // display text
    desc: &'static str,  // sub-caption
}

/// The two boot options.
const MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        key: '1',
        label: "Desktop",
        desc: "Enter the desktop (default)",
    },
    MenuItem {
        key: '2',
        label: "Safe Terminal",
        desc: "Boot straight to a terminal",
    },
];

/// UEFI entry point.
#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Must run before any allocation or `println!`.
    uefi::helpers::init(&mut system_table).expect("NovaOS UEFI helpers init failed");

    // The whole program runs inside an immutable-borrow closure: `bs` keeps
    // borrowing `system_table`, and both the GOP and the keyboard inputs are
    // opened through it, so no mutable borrow of `system_table` is needed.
    (|| -> Status {
        let bs = system_table.boot_services();

        // ---------- 1. Locate and open the GOP ----------
        let handles = match bs.locate_handle_buffer(SearchType::from_proto::<GraphicsOutput>()) {
            Ok(h) => h,
            Err(error) => {
                uefi::println!("GOP handles lookup failed: {:?}", error.status());
                return error.status();
            }
        };
        if handles.is_empty() {
            uefi::println!("No Graphics Output Protocol device found");
            return Status::UNSUPPORTED;
        }
        let mut gop = match bs.open_protocol_exclusive::<GraphicsOutput>(handles[0]) {
            Ok(g) => g,
            Err(error) => {
                uefi::println!("GOP open failed: {:?}", error.status());
                return error.status();
            }
        };

        // ---------- 2. Open every keyboard input handle ----------
        let stdin_handles = match bs.locate_handle_buffer(SearchType::from_proto::<Input>()) {
            Ok(h) => h,
            Err(_) => {
                uefi::println!("No input device found");
                return Status::UNSUPPORTED;
            }
        };
        if stdin_handles.is_empty() {
            uefi::println!("No input device found");
            return Status::UNSUPPORTED;
        }
        uefi::println!("input handles: {}", stdin_handles.len());
        // Which handle is wired to the real keyboard differs per firmware
        // (OVMF exposes several), so we open and wait on all of them.
        let mut inputs: Vec<ScopedProtocol<Input>> = Vec::new();
        for handle in stdin_handles.iter() {
            if let Ok(protocol) = bs.open_protocol_exclusive::<Input>(*handle) {
                inputs.push(protocol);
            }
        }
        if inputs.is_empty() {
            uefi::println!("no input protocol could be opened");
            return Status::UNSUPPORTED;
        }

        // ---------- 3. Pick a video mode (1024x768 preferred) ----------
        let mut chosen = None;
        let mut first_mode = None;
        for m in gop.modes(bs) {
            let (w, h) = m.info().resolution();
            let f = m.info().pixel_format();
            uefi::println!("GOP mode: {}x{} fmt={:?}", w, h, f);
            if first_mode.is_none() {
                first_mode = Some(m);
            }
            if (w, h) == (1024, 768) {
                chosen = Some(m);
                break;
            }
            if chosen.is_none() && (w, h) == (800, 600) {
                chosen = Some(m);
            }
        }
        let mode = match chosen {
            Some(m) => m,
            None => match first_mode {
                Some(m) => m,
                None => {
                    uefi::println!("No usable GOP mode");
                    return Status::UNSUPPORTED;
                }
            },
        };

        if let Err(error) = gop.set_mode(&mode) {
            uefi::println!("GOP set_mode failed: {:?}", error.status());
            return error.status();
        }

        let info = gop.current_mode_info();
        let (screen_w, screen_h) = info.resolution();
        let stride = info.stride();
        let fmt = info.pixel_format();

        uefi::println!(
            "GOP active: {}x{} stride={} fmt={:?}",
            screen_w,
            screen_h,
            stride,
            fmt
        );

        // ---------- 4. Main loop: draw the menu, wait for keys ----------
        loop {
            {
                let mut screen = borrow_framebuffer(&mut gop, stride, screen_w, screen_h, fmt);
                draw_menu(&mut screen, 0);
            }

            let key = wait_for_key(&mut inputs, bs);

            let mut selected_index: Option<usize> = None;
            match key {
                Key::Special(ScanCode::UP) => {}
                Key::Special(ScanCode::DOWN) => {}
                Key::Printable(ch) => {
                    let c = char::from(ch);
                    // Enter, or the shortcut of the default entry, pick Desktop.
                    if c == '\r' || c == '\n' || c == '1' {
                        selected_index = Some(0);
                    } else if c == '2' {
                        selected_index = Some(1);
                    }
                }
                Key::Special(ScanCode::ESCAPE) => {
                    uefi::println!("boot aborted by user");
                    return Status::ABORTED;
                }
                _ => {}
            }

            if let Some(index) = selected_index {
                let item = MENU_ITEMS[index];

                // Redraw with the selection applied; hold a few frames so the
                // user sees the result.
                for _ in 0..30 {
                    let mut screen = borrow_framebuffer(&mut gop, stride, screen_w, screen_h, fmt);
                    draw_menu(&mut screen, index);
                }

                uefi::println!("selected: {} ({})", item.label, item.desc);

                // TODO: write the NovaBoot BootProtocol snapshot, call
                // ExitBootServices, and jump to the kernel entry point. The
                // kernel side does not read NovaBoot yet, so no jump happens.
                uefi::println!("NOTE: kernel hand-off via NovaBoot is not yet wired.");
                uefi::println!("press any key to reboot");
                let _ = wait_for_key(&mut inputs, bs);
                system_table
                    .runtime_services()
                    .reset(ResetType::WARM, Status::SUCCESS, None);
            }
        }
    })()
}

/// Map the GOP framebuffer into a writable byte slice and wrap it in a
/// [`Screen`] draw context.
fn borrow_framebuffer<'a>(
    gop: &'a mut ScopedProtocol<GraphicsOutput>,
    stride: usize,
    screen_w: usize,
    screen_h: usize,
    fmt: PixelFormat,
) -> Screen<'a> {
    let mut fb = gop.frame_buffer();
    let ptr = fb.as_mut_ptr();
    let len = fb.size();
    // Safety: `ptr`/`len` come from the GOP frame buffer and describe a
    // device-backed memory range; we only write within `stride * height * 4`
    // computed from the same mode info, and never past `len`.
    let slice = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
    Screen {
        fb: slice,
        stride,
        width: screen_w,
        height: screen_h,
        fmt,
    }
}

/// A draw context over the GOP framebuffer: pixel format, stride and bounds
/// bundled so drawing primitives take geometry only, not eight arguments.
struct Screen<'fb> {
    fb: &'fb mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    fmt: PixelFormat,
}

impl Screen<'_> {
    /// Write one pixel, clipped to the screen bounds.
    fn set_pixel(&mut self, x: usize, y: usize, color: BltPixel) {
        if x >= self.width || y >= self.height {
            return;
        }
        let (r, g, b) = (color.red, color.green, color.blue);
        let base = (y * self.stride + x) * 4;
        if base + 3 >= self.fb.len() {
            return;
        }
        // GOP exposes little-endian 32bpp pixels: the `Rgb` format puts red in
        // the low byte, every other format (Bgr, Bitmask in QEMU) puts blue
        // there. Anything we cannot address directly would be BltOnly and is
        // treated like Bgr.
        let (c0, c1, c2) = if self.fmt == PixelFormat::Rgb {
            (r, g, b)
        } else {
            (b, g, r)
        };
        self.fb[base] = c0;
        self.fb[base + 1] = c1;
        self.fb[base + 2] = c2;
        self.fb[base + 3] = 0;
    }

    /// Fill a solid rectangle, clipped.
    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: BltPixel) {
        for row in y..y.saturating_add(h) {
            for col in x..x.saturating_add(w) {
                self.set_pixel(col, row, color);
            }
        }
    }

    /// Draw a 1-pixel rectangle outline, clipped.
    fn rect_outline(&mut self, x: usize, y: usize, w: usize, h: usize, color: BltPixel) {
        if w == 0 || h == 0 {
            return;
        }
        let bottom = y.saturating_add(h).saturating_sub(1);
        let right = x.saturating_add(w).saturating_sub(1);
        for cx in x..=right {
            self.set_pixel(cx, y, color);
            self.set_pixel(cx, bottom, color);
        }
        for cy in y..=bottom {
            self.set_pixel(x, cy, color);
            self.set_pixel(right, cy, color);
        }
    }

    /// Draw text, scaling each 5x7 glyph by `scale`, clipped.
    fn draw_text(&mut self, mut x: usize, y: usize, text: &str, color: BltPixel, scale: usize) {
        for ch in text.chars() {
            if let Some(rows) = char_rows(ch) {
                for row in 0..FONT_HEIGHT {
                    for col in 0..FONT_WIDTH {
                        if rows[row * FONT_WIDTH + col] {
                            let px = x + col * scale;
                            let py = y + row * scale;
                            for dy in 0..scale {
                                for dx in 0..scale {
                                    self.set_pixel(px + dx, py + dy, color);
                                }
                            }
                        }
                    }
                }
            }
            x = x.saturating_add((FONT_WIDTH + CHAR_SPACING) * scale);
            if x >= self.width {
                break;
            }
        }
    }
}

/// Draw the complete menu screen.
fn draw_menu(screen: &mut Screen, selected: usize) {
    let (screen_w, screen_h) = (screen.width, screen.height);

    // Backdrop.
    screen.fill_rect(0, 0, screen_w, screen_h, COLOR_BG);

    // Panel and border.
    let panel_w = 640.min(screen_w);
    let panel_h = 420.min(screen_h);
    let panel_x = (screen_w - panel_w) / 2;
    let panel_y = (screen_h - panel_h) / 2;
    screen.fill_rect(panel_x, panel_y, panel_w, panel_h, COLOR_PANEL);
    screen.rect_outline(panel_x, panel_y, panel_w, panel_h, COLOR_BORDER);

    // Title bar.
    let title_h = 56;
    screen.fill_rect(panel_x, panel_y, panel_w, title_h, COLOR_TITLE_BG);
    screen.draw_text(
        panel_x + 24,
        panel_y + 16,
        "NovaOS Boot Menu",
        COLOR_TEXT,
        2,
    );

    // Entries.
    let item_h = 64;
    let item_y0 = panel_y + title_h + 32;
    let gap = 20;

    for (index, item) in MENU_ITEMS.iter().enumerate() {
        let item_y = item_y0 + index * (item_h + gap);
        let is_selected = index == selected;

        // Highlight bar on the selected entry.
        if is_selected {
            screen.fill_rect(
                panel_x + 40,
                item_y,
                panel_w - 80,
                item_h,
                COLOR_HIGHLIGHT,
            );
        }

        // Shortcut number.
        let mut key_text = String::new();
        key_text.push(item.key);
        key_text.push('.');
        key_text.push(' ');
        screen.draw_text(
            panel_x + 64,
            item_y + 14,
            &key_text,
            if is_selected {
                COLOR_HIGHLIGHT_TEXT
            } else {
                COLOR_DIM
            },
            2,
        );

        // Label.
        screen.draw_text(
            panel_x + 110,
            item_y + 14,
            item.label,
            if is_selected {
                COLOR_HIGHLIGHT_TEXT
            } else {
                COLOR_TEXT
            },
            2,
        );

        // Sub-caption.
        screen.draw_text(panel_x + 110, item_y + 38, item.desc, COLOR_DIM, 1);
    }

    // Footer hint.
    let hint = "Up/Down: navigate   Enter: select   1/2: direct   Esc: abort";
    let hint_w = text_width(hint);
    screen.draw_text(
        panel_x + (panel_w - hint_w) / 2,
        panel_y + panel_h - 32,
        hint,
        COLOR_DIM,
        1,
    );
}

/// Block until any input handle delivers a key, and return it.
fn wait_for_key(inputs: &mut [ScopedProtocol<Input>], bs: &BootServices) -> Key {
    loop {
        // Collect every handle's key event (`Event` is not `Clone`, so fetch
        // them fresh each round).
        let mut events: Vec<Event> = Vec::new();
        for input in inputs.iter_mut() {
            if let Some(event) = input.wait_for_key_event() {
                events.push(event);
            }
        }
        if events.is_empty() {
            continue;
        }
        let _ = bs.wait_for_event(&mut events);
        for input in inputs.iter_mut() {
            if let Ok(Some(key)) = input.read_key() {
                uefi::println!("key received: {:?}", key);
                return key;
            }
        }
    }
}
