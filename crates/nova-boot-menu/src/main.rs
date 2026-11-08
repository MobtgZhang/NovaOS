//! NovaOS UEFI boot manager (the NovaBoot unified boot protocol's loader).
//!
//! This application renders the OS-selection menu through the GOP framebuffer,
//! then loads the selected kernel ELF and initramfs off the ESP, builds the
//! early page tables (huge-leaf HHDM over the firmware's regions, kernel
//! higher-half window), enumerates the processors from the firmware's ACPI
//! tables, publishes a [`nova_boot::BootProtocol`] snapshot, and transfers
//! control to the kernel's `nova_boot_entry`.
//!
//! # Boot contract with the kernel
//!
//! The kernel entry point is `nova_boot_entry(snapshot_va: u64) -> !` (SysV,
//! RDI). On entry:
//! - long mode is active, paging is on, CR3 points at the PML4 below;
//! - interrupts are masked (`cli` already done);
//! - RSP points at a boot-manager-allocated, 16-byte-aligned stack;
//! - RDI is the direct-map virtual address of the BootProtocol snapshot.
//!
//! The page tables map:
//! - PML4[0] -> identity-map of the low half, so the boot manager's own code
//!   and stack remain reachable across the CR3 switch;
//! - PML4[256] -> the HHDM window (`virtual = physical + 0xFFFF_8000_0000_0000`),
//!   covering every byte of physical memory the firmware reported;
//! - PML4[511] -> the kernel's higher-half window (`virtual = physical +
//!   0xFFFFFFFF80000000`), covering the kernel image, the bootstrap stack, and
//!   the snapshot page.
//!
//! The HHDM and kernel windows are built from the largest leaves the
//! alignment allows (1 GiB where the CPU supports `PDPE1GB`, else 2 MiB), with
//! 4 KiB pages at the unaligned edges; the kernel's walker understands huge
//! leaves, so no rule forbids them here.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use core::ptr::addr_of_mut;

use uefi::prelude::*;
use uefi::proto::console::gop::{BltPixel, GraphicsOutput, PixelFormat};
use uefi::proto::console::text::{Input, Key, ScanCode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, BootServices, MemoryType, ScopedProtocol, SearchType};
use uefi::{Handle, Status};

use nova_boot::fb_format;
use nova_boot::region;
use nova_boot::{
    BootArch, BootFramebuffer, BootMemoryRegion, BootMode, BootProtocol, MAGIC, MAX_MEMORY_REGIONS,
    PROTOCOL_MAJOR, PROTOCOL_MINOR,
};

use crate::font::{CHAR_SPACING, FONT_HEIGHT, FONT_WIDTH, char_rows, text_width};
use crate::load::{
    kernel_entry_vaddr, kernel_page_count, open_esp, parse_kernel_segments, read_file,
};
use crate::paging::{BOOT_STACK_SIZE, HHDM_OFFSET, KERNEL_LMA, PAGE_SIZE, Tables};

mod acpi;
mod font;
mod load;
mod paging;

/// `\EFI\NovaOS\nova-kernel.elf` on the ESP.
const KERNEL_ELF_PATH: &str = "\\EFI\\NovaOS\\nova-kernel.elf";
/// `\EFI\NovaOS\initramfs` on the ESP.
const INITRAMFS_PATH: &str = "\\EFI\\NovaOS\\initramfs";

/// Snapshot page: one page holds the [`BootProtocol`].
const SNAPSHOT_PAGES: u64 = 1;

/// Colours (`BltPixel::new(red, green, blue)`).
const COLOR_BG: BltPixel = BltPixel::new(20, 20, 35);
const COLOR_PANEL: BltPixel = BltPixel::new(30, 30, 50);
const COLOR_TITLE_BG: BltPixel = BltPixel::new(40, 40, 70);
const COLOR_TEXT: BltPixel = BltPixel::new(255, 255, 255);
const COLOR_HIGHLIGHT: BltPixel = BltPixel::new(100, 150, 255);
const COLOR_HIGHLIGHT_TEXT: BltPixel = BltPixel::new(255, 255, 255);
const COLOR_DIM: BltPixel = BltPixel::new(150, 150, 170);
const COLOR_BORDER: BltPixel = BltPixel::new(90, 90, 120);

/// A menu entry.
#[derive(Clone, Copy)]
struct MenuItem {
    key: char,
    label: &'static str,
    desc: &'static str,
    boot_mode: u16,
}

/// The two boot options.
const MENU_ITEMS: &[MenuItem] = &[
    MenuItem {
        key: '1',
        label: "Desktop",
        desc: "Enter the desktop (default)",
        boot_mode: BootMode::Desktop.code(),
    },
    MenuItem {
        key: '2',
        label: "Safe Terminal",
        desc: "Boot straight to a terminal",
        boot_mode: BootMode::SafeTerminal.code(),
    },
];

/// The information the kernel entry jump needs, collected by `boot_kernel`
/// and consumed by `main` after `ExitBootServices`.
struct BootHandoff {
    /// Physical address of the PML4, to load into CR3.
    pml4_phys: u64,
    /// Top of the bootstrap stack (16-byte aligned).
    stack_top: u64,
    /// Direct-map virtual address of the BootProtocol snapshot (passed in RDI).
    snapshot_va: u64,
    /// Kernel entry virtual address (jumped to).
    entry_vaddr: u64,
}

/// UEFI entry point.
#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Must run before any allocation or `println!`.
    uefi::helpers::init(&mut system_table).expect("NovaOS UEFI helpers init failed");

    // Enumerate the processors once, while boot services are alive and the
    // firmware's ACPI tables are dereferenceable. The snapshot below carries
    // the plain identifiers, so the kernel's boot path never parses firmware
    // tables; a machine with no ACPI falls back to the bootstrap processor.
    let processors = match acpi::discover(&system_table) {
        Ok(table) => {
            uefi::println!(
                "acpi: {} processors, bsp lapic id {:#x}",
                table.count,
                table.bsp_lapic_id
            );
            table
        }
        Err(reason) => {
            uefi::println!("acpi: {} — single-processor fallback", reason.tag());
            acpi::ProcessorTable::single()
        }
    };

    // Run the menu inside a closure that borrows `system_table` through
    // `boot_services()`. Once the closure returns, the borrow ends and
    // `system_table` can be consumed by `exit_boot_services()`.
    let handoff = (|| -> Result<BootHandoff, Status> {
        let bs = &system_table.boot_services();

        // ---------- 1. Locate and open the GOP ----------
        let handles = match bs.locate_handle_buffer(SearchType::from_proto::<GraphicsOutput>()) {
            Ok(h) => h,
            Err(error) => {
                uefi::println!("GOP handles lookup failed: {:?}", error.status());
                return Err(error.status());
            }
        };
        if handles.is_empty() {
            uefi::println!("No Graphics Output Protocol device found");
            return Err(Status::UNSUPPORTED);
        }
        let mut gop = match bs.open_protocol_exclusive::<GraphicsOutput>(handles[0]) {
            Ok(g) => g,
            Err(error) => {
                uefi::println!("GOP open failed: {:?}", error.status());
                return Err(error.status());
            }
        };

        // ---------- 2. Open every keyboard input handle ----------
        let stdin_handles = match bs.locate_handle_buffer(SearchType::from_proto::<Input>()) {
            Ok(h) => h,
            Err(_) => {
                uefi::println!("No input device found");
                return Err(Status::UNSUPPORTED);
            }
        };
        if stdin_handles.is_empty() {
            uefi::println!("No input device found");
            return Err(Status::UNSUPPORTED);
        }
        uefi::println!("input handles: {}", stdin_handles.len());

        // ---------- 3. Pick a video mode (1024x768 preferred) ----------
        let mut chosen = None;
        let mut first_mode = None;
        for m in gop.modes(bs) {
            let (w, h) = m.info().resolution();
            uefi::println!("GOP mode: {}x{} fmt={:?}", w, h, m.info().pixel_format());
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
                    return Err(Status::UNSUPPORTED);
                }
            },
        };

        if let Err(error) = gop.set_mode(&mode) {
            uefi::println!("GOP set_mode failed: {:?}", error.status());
            return Err(error.status());
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

        // ---------- 4. Open the ESP filesystem ----------
        let mut fs = match open_esp(bs, _handle) {
            Ok(f) => f,
            Err(error) => {
                uefi::println!("ESP filesystem lookup failed: {:?}", error);
                return Err(error);
            }
        };

        // ---------- 5. Main loop: draw the menu, wait for keys ----------
        let mut selected_index: usize = 0;
        loop {
            {
                let mut screen = borrow_framebuffer(&mut gop, stride, screen_w, screen_h, fmt);
                draw_menu(&mut screen, selected_index);
            }

            let key = wait_for_key(&stdin_handles, bs);

            let mut should_boot = false;
            match key {
                Key::Special(ScanCode::UP) => {
                    selected_index = selected_index.saturating_sub(1);
                }
                Key::Special(ScanCode::DOWN) => {
                    selected_index = (selected_index + 1).min(MENU_ITEMS.len() - 1);
                }
                Key::Printable(ch) => {
                    let c = char::from(ch);
                    if c == '\r' || c == '\n' || c == '1' || c == '2' {
                        if c == '1' {
                            selected_index = 0;
                        } else if c == '2' {
                            selected_index = 1;
                        }
                        should_boot = true;
                    } else {
                        continue;
                    }
                }
                Key::Special(ScanCode::ESCAPE) => {
                    uefi::println!("boot aborted by user");
                    return Err(Status::ABORTED);
                }
                _ => {}
            }

            if should_boot {
                // Redraw with the selection applied so the user sees the result.
                {
                    let mut screen = borrow_framebuffer(&mut gop, stride, screen_w, screen_h, fmt);
                    draw_menu(&mut screen, selected_index);
                }
                uefi::println!(
                    "selected: {} ({})",
                    MENU_ITEMS[selected_index].label,
                    MENU_ITEMS[selected_index].desc
                );

                // ---------- 6. Load the kernel and initramfs ----------
                let boot_mode = MENU_ITEMS[selected_index].boot_mode;
                match boot_kernel(
                    bs,
                    &mut gop,
                    &mut fs,
                    screen_w,
                    screen_h,
                    stride,
                    fmt,
                    selected_index as u16,
                    boot_mode,
                    processors,
                ) {
                    Ok(handoff) => return Ok(handoff),
                    Err(error) => {
                        uefi::println!("boot failed: {:?}", error);
                        uefi::println!("press any key to retry");
                        let _ = wait_for_key(&stdin_handles, bs);
                    }
                }
            }
        }
    })();

    // The `bs` borrow ended with the closure. Consume `system_table` to leave
    // UEFI boot services, then jump into the kernel.
    let (_rt, _memory_map) = unsafe { system_table.exit_boot_services(MemoryType::LOADER_DATA) };

    // cli, switch rsp to the bootstrap stack, and jump to the kernel entry
    // with the snapshot VA in RDI.
    match handoff {
        Ok(h) => unsafe {
            paging::enter_new_tables(h.pml4_phys, h.stack_top, h.snapshot_va, h.entry_vaddr);
        },
        Err(status) => status,
    }
}

/// Map the GOP framebuffer into a writable byte slice and wrap it in a
/// [`Screen`] draw context.
fn borrow_framebuffer(
    gop: &mut GraphicsOutput,
    stride: usize,
    screen_w: usize,
    screen_h: usize,
    fmt: PixelFormat,
) -> Screen<'_> {
    let mut fb = gop.frame_buffer();
    let ptr = fb.as_mut_ptr();
    let len = fb.size();
    let slice = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
    Screen {
        fb: slice,
        stride,
        width: screen_w,
        height: screen_h,
        fmt,
    }
}

/// A draw context over the GOP framebuffer.
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

    screen.fill_rect(0, 0, screen_w, screen_h, COLOR_BG);

    let panel_w = 640.min(screen_w);
    let panel_h = 420.min(screen_h);
    let panel_x = (screen_w - panel_w) / 2;
    let panel_y = (screen_h - panel_h) / 2;
    screen.fill_rect(panel_x, panel_y, panel_w, panel_h, COLOR_PANEL);
    screen.rect_outline(panel_x, panel_y, panel_w, panel_h, COLOR_BORDER);

    let title_h = 56;
    screen.fill_rect(panel_x, panel_y, panel_w, title_h, COLOR_TITLE_BG);
    screen.draw_text(
        panel_x + 24,
        panel_y + 16,
        "NovaOS Boot Menu",
        COLOR_TEXT,
        2,
    );

    let item_h = 64;
    let item_y0 = panel_y + title_h + 32;
    let gap = 20;

    for (index, item) in MENU_ITEMS.iter().enumerate() {
        let item_y = item_y0 + index * (item_h + gap);
        let is_selected = index == selected;

        if is_selected {
            screen.fill_rect(panel_x + 40, item_y, panel_w - 80, item_h, COLOR_HIGHLIGHT);
        }

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

        screen.draw_text(panel_x + 110, item_y + 38, item.desc, COLOR_DIM, 1);
    }

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

/// Wait for a key, but auto-select the default entry ('1' = Desktop) after
/// `AUTO_BOOT_SECONDS` of inactivity so a headless run can proceed unattended.
///
/// Returns `Ok(key)` on a real keypress, or the default key on timeout.
fn wait_for_key(stdin_handles: &uefi::table::boot::HandleBuffer, bs: &BootServices) -> Key {
    const AUTO_BOOT_SECONDS: usize = 5;
    const STALL_US: usize = 100_000; // 100 ms
    let mut waited_us: usize = 0;

    loop {
        let mut inputs: Vec<ScopedProtocol<Input>> = Vec::new();
        for handle in stdin_handles.iter() {
            if let Ok(protocol) = bs.open_protocol_exclusive::<Input>(*handle) {
                inputs.push(protocol);
            }
        }
        if inputs.is_empty() {
            continue;
        }

        // Non-blocking poll of every handle.
        for input in inputs.iter_mut() {
            match input.read_key() {
                Ok(Some(key)) => {
                    uefi::println!("key received: {:?}", key);
                    return key;
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }

        if waited_us >= AUTO_BOOT_SECONDS * 1_000_000 {
            uefi::println!("auto-boot: selecting Desktop after timeout");
            return Key::Printable(
                uefi::Char16::try_from('1' as u16).expect("'1' is valid UTF-16"),
            );
        }

        bs.stall(STALL_US);
        waited_us += STALL_US;
    }
}

/// Load the kernel, build the NovaBoot snapshot, and return the hand-off data.
/// This never touches `ExitBootServices`: that is `main`'s job, so that the
/// `system_table` borrow from `boot_services()` ends before we consume
/// `system_table` itself.
#[allow(clippy::too_many_arguments)]
fn boot_kernel(
    bs: &BootServices,
    gop: &mut GraphicsOutput,
    fs: &mut SimpleFileSystem,
    screen_w: usize,
    screen_h: usize,
    stride: usize,
    fmt: PixelFormat,
    selected_entry: u16,
    boot_mode: u16,
    processors: acpi::ProcessorTable,
) -> Result<BootHandoff, String> {
    // ---- 6a. Read the kernel ELF and initramfs ----
    let elf_data = read_file(fs, KERNEL_ELF_PATH)
        .map_err(|error| format!("cannot read kernel: {:?}", error))?;
    uefi::println!("kernel ELF: {} bytes", elf_data.len());
    let initramfs_data = read_file(fs, INITRAMFS_PATH)
        .map_err(|error| format!("cannot read initramfs: {:?}", error))?;
    uefi::println!("initramfs: {} bytes", initramfs_data.len());

    let segments = parse_kernel_segments(&elf_data)?;
    let entry_vaddr = kernel_entry_vaddr(&elf_data)?;
    let kernel_pages = kernel_page_count(&segments);
    uefi::println!(
        "kernel segments: {} entries, {} pages, entry {:#x}",
        segments.len(),
        kernel_pages,
        entry_vaddr
    );

    // ---- 6b. Allocate loader-owned pages ----
    // Bootstrap stack: 32 KiB, page-aligned.
    let stack_pages = BOOT_STACK_SIZE.div_ceil(PAGE_SIZE);
    let stack_frame = paging_allocate_pages(bs, stack_pages as usize)
        .map_err(|error| format!("cannot allocate boot stack: {:?}", error))?;
    let stack_phys = stack_frame.phys();
    let stack_top = stack_phys + BOOT_STACK_SIZE;

    // Snapshot page: one page for the BootProtocol.
    let snapshot_frame = paging_allocate_pages(bs, SNAPSHOT_PAGES as usize)
        .map_err(|error| format!("cannot allocate snapshot page: {:?}", error))?;
    let snapshot_phys = snapshot_frame.phys();

    // ---- 6c. Query the firmware memory map ----
    let memory_map = bs
        .memory_map(MemoryType::LOADER_DATA)
        .map_err(|error| format!("GetMemoryMap failed: {:?}", error))?;
    let regions = memory_map_to_regions(&memory_map);
    let mut tables =
        Tables::new(bs).map_err(|error| format!("cannot allocate page tables: {:?}", error))?;

    // HHDM window over every reported memory region.
    let hhdm_regions: Vec<(u64, u64)> = regions.iter().map(|&(s, e, _)| (s, e)).collect();
    tables
        .map_hhdm(bs, &hhdm_regions)
        .map_err(|error| format!("cannot map HHDM window: {:?}", error))?;

    // Kernel window: image at KERNEL_LMA, stack and snapshot at their
    // KERNEL_OFFSET aliases.
    tables
        .map_kernel_window(bs, kernel_pages, stack_phys, snapshot_phys)
        .map_err(|error| format!("cannot map kernel window: {:?}", error))?;

    // ---- 6e. Copy the kernel to its physical load address ----
    for seg in &segments {
        let dest = seg.paddr as *mut u8;
        let src = seg.data.as_ptr();
        let count = seg.filesz as usize;
        // SAFETY: the destination is physical memory below 1 MiB plus the
        // segment extents, identity-mapped by the firmware's current tables and
        // by our new PML4[511]; both are active until CR3 is reloaded.
        unsafe {
            core::ptr::copy_nonoverlapping(src, dest, count);
        }
        // Zero the .bss portion (memsz - filesz).
        if seg.memsz > seg.filesz {
            let bss_start = (seg.paddr + seg.filesz) as *mut u8;
            let bss_len = (seg.memsz - seg.filesz) as usize;
            unsafe {
                core::ptr::write_bytes(bss_start, 0, bss_len);
            }
        }
    }

    // ---- 6f. Copy the initramfs to its allocated pages ----
    let initramfs_pages = (initramfs_data.len() as u64).div_ceil(PAGE_SIZE);
    let initramfs_frame = if initramfs_pages > 0 {
        paging_allocate_pages(bs, initramfs_pages as usize)
            .map_err(|error| format!("cannot allocate initramfs pages: {:?}", error))?
    } else {
        // Allocate a dummy page so the pointer is non-null even when empty.
        paging_allocate_pages(bs, 1)
            .map_err(|error| format!("cannot allocate dummy page: {:?}", error))?
    };
    let initramfs_phys = initramfs_frame.phys();
    if !initramfs_data.is_empty() {
        unsafe {
            core::ptr::copy_nonoverlapping(
                initramfs_data.as_ptr(),
                initramfs_phys as *mut u8,
                initramfs_data.len(),
            );
        }
    }

    // Map the initramfs region into the HHDM window too. It was allocated after
    // the first memory-map query, so it may not be covered by any reported
    // region. Map each page at its HHDM alias so `capture` can read it through
    // the direct map before ExitBootServices reclaims the loader's pages.
    let initramfs_end = initramfs_phys + initramfs_pages * PAGE_SIZE;
    let mut addr = initramfs_phys & !(PAGE_SIZE - 1);
    while addr < initramfs_end {
        tables
            .map_hhdm_page(bs, addr)
            .map_err(|error| format!("cannot map initramfs at HHDM: {:?}", error))?;
        addr += PAGE_SIZE;
    }

    // ---- 6g. Build the BootProtocol snapshot ----
    let snapshot = snapshot_frame.phys() as *mut BootProtocol;
    // SAFETY: the snapshot page was zeroed by `paging_allocate_pages`.
    let mut count: usize = 0;
    unsafe {
        addr_of_mut!((*snapshot).magic).write(MAGIC);
        addr_of_mut!((*snapshot).major).write(PROTOCOL_MAJOR);
        addr_of_mut!((*snapshot).minor).write(PROTOCOL_MINOR);
        addr_of_mut!((*snapshot).architecture).write(BootArch::X86_64.code());
        addr_of_mut!((*snapshot).flags).write(0);
        addr_of_mut!((*snapshot).kernel_entry).write(entry_vaddr);
        addr_of_mut!((*snapshot).kernel_load_address).write(KERNEL_LMA);
        addr_of_mut!((*snapshot).direct_map_offset).write(HHDM_OFFSET);

        addr_of_mut!((*snapshot).initramfs_start).write(initramfs_phys);
        addr_of_mut!((*snapshot).initramfs_length).write(initramfs_data.len() as u64);

        // The processors ACPI described, or the single-processor fallback.
        // The bootstrap processor is always listed first by convention, but
        // the kernel locates it by `bsp_lapic_id` rather than by position.
        addr_of_mut!((*snapshot).cpu_count).write(processors.count);
        addr_of_mut!((*snapshot).bsp_lapic_id).write(processors.bsp_lapic_id);
        for (index, id) in processors.ids.iter().enumerate() {
            addr_of_mut!((*snapshot).cpu_ids[index]).write(*id);
        }

        // Memory regions.
        for &(start, end, kind) in regions.iter() {
            if count >= MAX_MEMORY_REGIONS {
                break;
            }
            let length = end.saturating_sub(start);
            if length == 0 {
                continue;
            }
            let base = start & !(PAGE_SIZE - 1);
            let aligned_length = (end - base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            addr_of_mut!((*snapshot).memory[count]).write(BootMemoryRegion {
                start: base,
                length: aligned_length,
                kind,
            });
            count += 1;
        }
        addr_of_mut!((*snapshot).memory_count).write(count as u16);

        // Framebuffer.
        let fb = build_framebuffer(gop, screen_w, screen_h, stride, fmt);
        addr_of_mut!((*snapshot).framebuffer).write(fb);

        addr_of_mut!((*snapshot).selected_entry).write(selected_entry);
        addr_of_mut!((*snapshot).boot_mode).write(boot_mode);
        addr_of_mut!((*snapshot).reserved).write(0);
        // The FADT's wall-clock century register, for the kernel's CMOS
        // reader. Zero when no FADT was found — the kernel infers instead.
        addr_of_mut!((*snapshot).acpi_century).write(processors.rtc_century);
    }

    // Validate the snapshot before committing.
    let snapshot_ref = unsafe { &*snapshot };
    if let Err(error) = snapshot_ref.validate() {
        return Err(format!("snapshot validation failed: {}", error.tag()));
    }

    uefi::println!(
        "snapshot: mode={} regions={} entry {:#x}",
        BootMode::from_code(boot_mode)
            .map(|m| m.name())
            .unwrap_or("none"),
        count,
        entry_vaddr
    );

    // Return the hand-off data; `main` performs ExitBootServices and the jump,
    // so that the `system_table` borrow from `boot_services()` ends first.
    let snapshot_va = snapshot_phys.wrapping_add(HHDM_OFFSET);
    Ok(BootHandoff {
        pml4_phys: tables.pml4_phys(),
        stack_top,
        snapshot_va,
        entry_vaddr,
    })
}

/// Allocate `count` contiguous pages, zeroed, from UEFI boot-services memory.
fn paging_allocate_pages(bs: &BootServices, count: usize) -> Result<paging::Frame, Status> {
    let addr = bs
        .allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, count)
        .map_err(|error| error.status())?;
    // Pages are already zeroed by UEFI; re-zero for cleanliness.
    unsafe {
        let slice = core::slice::from_raw_parts_mut(addr as *mut u8, count * PAGE_SIZE as usize);
        for byte in slice.iter_mut() {
            *byte = 0;
        }
    }
    Ok(paging::Frame::from_phys(addr))
}

/// Convert a UEFI memory map into protocol memory regions.
///
/// Conversion rule:
/// - `EfiConventionalMemory`, `EfiBootServicesCode`, `EfiBootServicesData`
///   -> `region::USABLE` (0)
/// - `EfiLoaderData` -> `region::RESERVED` (1)
/// - everything else -> `region::RESERVED` (non-usable)
fn memory_map_to_regions(map: &uefi::table::boot::MemoryMap) -> Vec<(u64, u64, u16)> {
    let mut regions = Vec::new();
    let mut index = 0;
    while let Some(desc) = map.get(index) {
        let start = desc.phys_start;
        let pages = desc.page_count;
        let end = start.saturating_add(pages * PAGE_SIZE);
        let kind = match desc.ty {
            uefi::table::boot::MemoryType::CONVENTIONAL => region::USABLE,
            uefi::table::boot::MemoryType::BOOT_SERVICES_CODE => region::USABLE,
            uefi::table::boot::MemoryType::BOOT_SERVICES_DATA => region::USABLE,
            uefi::table::boot::MemoryType::LOADER_DATA => region::RESERVED,
            _ => region::RESERVED,
        };
        regions.push((start, end, kind));
        index += 1;
    }
    regions
}

/// Build the protocol framebuffer record from the active GOP mode.
///
/// Pixel-format mapping:
/// - `Rgb` -> `fb_format::RGB888X`
/// - `Bgr` / `Bitmask` -> `fb_format::XRGB8888`
/// - `BltOnly` -> absent (`present = 0`)
fn build_framebuffer(
    gop: &mut GraphicsOutput,
    screen_w: usize,
    screen_h: usize,
    stride: usize,
    fmt: PixelFormat,
) -> BootFramebuffer {
    // The GOP frame buffer's virtual address under UEFI boot services is the
    // identity-mapped physical address on OVMF/QEMU, which is all the boot
    // protocol needs to publish.
    let fb_phys = gop.frame_buffer().as_mut_ptr() as u64;
    let (format, present) = match fmt {
        PixelFormat::Rgb => (fb_format::RGB888X, 1),
        PixelFormat::Bgr | PixelFormat::Bitmask => (fb_format::XRGB8888, 1),
        PixelFormat::BltOnly => (0, 0),
    };

    BootFramebuffer {
        present,
        address: if present == 0 { 0 } else { fb_phys },
        width: screen_w as u32,
        height: screen_h as u32,
        stride: stride as u32,
        format,
    }
}
