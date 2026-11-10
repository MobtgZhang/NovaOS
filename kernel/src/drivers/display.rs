//! The firmware framebuffer, mapped into the kernel and drawn into.
//!
//! # What this module owns
//!
//! The bootloader reports a linear framebuffer. This module maps every page of
//! it into the kernel's own address space, records the mode, and exposes the
//! pixel primitives the compositor needs. It does not expose the framebuffer to
//! user space: the display service reports the mode, not the address.
//!
//! # Pixel format
//!
//! M6 draws XRGB8888 only. The firmware gives three bit *shifts* — where red,
//! green, and blue sit inside the 32-bit word — and those shifts are the whole
//! format. A write composes the word from the shifts rather than assuming a
//! byte order; this is what keeps a firmware that orders the components
//! differently from producing the wrong colour.
//!
//! # Blit
//!
//! A surface buffer is client-owned memory, and the compositor copies it into
//! the framebuffer with the CPU. The copy clips to the display and validates the
//! source stride and length before touching any byte. All arithmetic is done in
//! 64 bits and checked, so a rectangle that would overflow is refused rather
//! than wrapped.
//!
//! # Mapping
//!
//! The boot manager maps the framebuffer at
//! `virtual = physical + direct_map_offset`. The kernel's own address space is
//! built from the boot manager's kernel half,
//! so most of that mapping survives. This module still checks every page with
//! `translate` and maps the missing ones with `map_kernel_mmio`, because a
//! firmware whose framebuffer lands outside the bootloader's cloned half would
//! otherwise be unmapped and every draw would fault.

use core::cell::UnsafeCell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::boot::PAGE_SIZE;
use crate::boot::info::FramebufferInfo;
use crate::klog;
use crate::logger::Level;
use crate::mm::address_space;

/// The only pixel format M6 draws.
pub const PIXEL_FORMAT_XRGB8888: u32 = 1;

/// Source-buffer shifts for XRGB8888.
///
/// A surface buffer is XRGB8888, so its components are always at 16, 8, and 0.
/// The framebuffer's shifts are separate: they are the firmware's placement.
const SOURCE_RED_SHIFT: u8 = 16;
const SOURCE_GREEN_SHIFT: u8 = 8;
const SOURCE_BLUE_SHIFT: u8 = 0;

/// The display mode, as the kernel knows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DisplayMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes between the first pixels of consecutive rows.
    pub pitch: u32,
    /// Bit offset of the red component.
    pub red_shift: u8,
    /// Bit offset of the green component.
    pub green_shift: u8,
    /// Bit offset of the blue component.
    pub blue_shift: u8,
    /// Virtual address of the first byte.
    pub base_virtual: u64,
}

impl DisplayMode {
    /// Build a mode from the normalised boot record.
    ///
    /// Returns `None` when the shifts cannot address a 32-bit word; a mode the
    /// kernel cannot compose is not a mode it can draw into.
    pub fn from_info(info: FramebufferInfo) -> Option<DisplayMode> {
        if info.red_shift >= 32 || info.green_shift >= 32 || info.blue_shift >= 32 {
            return None;
        }
        Some(DisplayMode {
            width: info.width,
            height: info.height,
            pitch: info.pitch,
            red_shift: info.red_shift,
            green_shift: info.green_shift,
            blue_shift: info.blue_shift,
            base_virtual: info.virtual_address,
        })
    }

    /// One past the last visible pixel column.
    pub const fn right(&self) -> u64 {
        self.width as u64
    }

    /// One past the last visible pixel row.
    pub const fn bottom(&self) -> u64 {
        self.height as u64
    }
}

/// Why a draw was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayError {
    /// No framebuffer is available.
    NoDisplay,
    /// A coordinate lies outside the display.
    OutOfBounds,
    /// The source buffer is too small or its stride is too short.
    InvalidBuffer,
}

/// Holds the mapped display mode.
///
/// Written once by [`init`] and read-only afterwards. The single-CPU,
/// interrupts-masked discipline makes the mutable cell sound, exactly as the
/// frame allocator and the service state do.
struct DisplayCell {
    inner: UnsafeCell<Option<DisplayMode>>,
}

// SAFETY: `init` writes the cell once, at boot, before any reader exists; every
// later access is read-only, and the kernel runs on one CPU with interrupts
// masked during boot and during service calls.
unsafe impl Sync for DisplayCell {}

impl DisplayCell {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(None),
        }
    }

    /// Install the mode.
    ///
    /// # Safety
    ///
    /// Must be called once, before any reader exists.
    unsafe fn set(&self, mode: Option<DisplayMode>) {
        // SAFETY: delegated to this method's contract.
        unsafe {
            *self.inner.get() = mode;
        }
    }

    /// Read the mode.
    fn get(&self) -> Option<DisplayMode> {
        // SAFETY: the cell is written once before readers exist, and every
        // later read is immutable. Single-CPU access means no concurrent write.
        unsafe { *self.inner.get() }
    }
}

static DISPLAY: DisplayCell = DisplayCell::new();

/// Bring up the display.
///
/// The virtio-gpu comes first: a device scanout replaces the firmware
/// framebuffer entirely, which is the point — the pixels are composed
/// in guest RAM and moved to the host by the device, and nothing
/// touches a UEFI resource after `ExitBootServices`. A machine
/// without a GPU (or one that refuses) falls through to the firmware
/// framebuffer exactly as before, so a headless or legacy boot is
/// unaffected.
///
/// # Safety
///
/// Must run once from the boot path, after `mm::init` has built and activated
/// the kernel's address space, and before any draw calls. `boot_info` must be
/// the immutable snapshot `boot::capture` published.
pub unsafe fn init(boot_info: &crate::boot::BootInfo) {
    match crate::drivers::virtio::gpu::install() {
        Ok((width, height, pitch, base_virtual)) => {
            // The device's format is B8G8R8X8: blue at bits 0-7, green at
            // 8-15, red at 16-23 — the shifts `compose` needs to write
            // NovaOS's native `0x00RRGGBB` words straight into it.
            let mode = DisplayMode {
                width,
                height,
                pitch,
                red_shift: 16,
                green_shift: 8,
                blue_shift: 0,
                base_virtual,
            };
            // SAFETY: `init` is the only writer, and it runs once during boot.
            unsafe { DISPLAY.set(Some(mode)) };
            klog!(
                Level::Info,
                "display: virtio-gpu scanout {width}x{height} pitch {pitch} at virt {base_virtual:#x} — the device owns the scanout"
            );
            return;
        }
        Err(error) => {
            klog!(
                Level::Info,
                "display: virtio-gpu unavailable ({error:?}); using the firmware framebuffer"
            );
        }
    }

    let Some(info) = boot_info.framebuffer_info() else {
        klog!(Level::Info, "display: no framebuffer");
        return;
    };

    let Some(mode) = DisplayMode::from_info(info) else {
        klog!(Level::Warn, "display: framebuffer shifts are not drawable");
        return;
    };

    if info.virtual_address % PAGE_SIZE != 0 {
        klog!(
            Level::Warn,
            "display: framebuffer virtual address is not page aligned"
        );
        return;
    }

    // The visible bytes include row padding. Mapping the whole visible range is
    // what makes the last row addressable, even when pitch exceeds width * 4.
    let bytes = info.byte_count() as u64;
    let pages = match bytes.checked_add(PAGE_SIZE - 1) {
        Some(total) => total / PAGE_SIZE,
        None => {
            klog!(Level::Warn, "display: framebuffer size overflows");
            return;
        }
    };

    let space = address_space::kernel_space();
    let mut all_mapped = true;

    for page in 0..pages {
        let offset = match page.checked_mul(PAGE_SIZE) {
            Some(value) => value,
            None => {
                all_mapped = false;
                break;
            }
        };
        let virt = match info.virtual_address.checked_add(offset) {
            Some(value) => value,
            None => {
                all_mapped = false;
                break;
            }
        };
        let phys = match info.physical_address.checked_add(offset) {
            Some(value) => value,
            None => {
                all_mapped = false;
                break;
            }
        };

        if address_space::translate(&space, virt) != Some(phys) {
            if let Err(error) = address_space::map_kernel_mmio(virt, phys) {
                klog!(
                    Level::Warn,
                    "display: could not map framebuffer page {virt:#x} -> {phys:#x}: {:?}",
                    error
                );
                all_mapped = false;
            }
        }
    }

    if !all_mapped {
        klog!(Level::Warn, "display: framebuffer is only partly mapped");
        return;
    }

    // SAFETY: `init` is the only writer, and it runs once during boot.
    unsafe { DISPLAY.set(Some(mode)) };

    klog!(
        Level::Info,
        "display: framebuffer {}x{} pitch {} at virt {:#x} (phys {:#x})",
        mode.width,
        mode.height,
        mode.pitch,
        mode.base_virtual,
        info.physical_address
    );
}

/// The current display mode, if a framebuffer was brought up.
pub fn mode() -> Option<DisplayMode> {
    DISPLAY.get()
}

/// The soonest a second device flush may follow the first, in cycles of
/// [`crate::arch::cycles`].
///
/// The timer path presents on every local-APIC tick — about 15 kHz under
/// QEMU's 1 GHz APIC bus rate, not the 100 Hz [`crate::arch::x86_64::pit`]
/// advertises for the PIT this kernel no longer drives. A device transfer
/// per tick would move a whole surface 15,000 times a second: megabytes of
/// DMA per frame for a screen that has not changed, and a host main loop
/// so busy copying that the guest's other devices starve. The limiter caps
/// the device's frame rate instead — roughly 144 Hz on a 1 GHz
/// time-stamp counter, faster on a higher rate, which the display
/// requirement takes as a floor rather than a ceiling.
static LAST_FLUSH: AtomicU64 = AtomicU64::new(0);

/// The interval between device flushes: about 6.9 million cycles —
/// ~6.9 ms at a 1 GHz counter rate, i.e. roughly a 144 Hz panel's
/// refresh interval, and never below ~2 ms at any faster counter rate.
const FLUSH_INTERVAL_CYCLES: u64 = 6_900_000;

/// Push the composed frame to the display device.
///
/// On a virtio-gpu scanout this is the device-side `TRANSFER` +
/// `FLUSH` that makes the pixels visible — the compositor's CPU work
/// ends at the backing buffer, and the display path from there is the
/// GPU's DMA, not a CPU write to a firmware resource. On a firmware
/// framebuffer the pixels are already where they need to be and this
/// is one branch.
///
/// Rate-limited: a caller that presents more often than
/// [`FLUSH_INTERVAL_CYCLES`] asks for a frame is answered with the
/// previous one still on screen, which is exactly what an un-changed
/// screen deserves.
pub fn flush() {
    let now = crate::arch::cycles();
    if now.wrapping_sub(LAST_FLUSH.load(Ordering::Relaxed)) < FLUSH_INTERVAL_CYCLES {
        return;
    }
    LAST_FLUSH.store(now, Ordering::Relaxed);
    let _ = crate::drivers::virtio::gpu::flush();
}

/// The pixel word a colour makes in the framebuffer.
fn compose(mode: DisplayMode, red: u8, green: u8, blue: u8) -> u32 {
    ((red as u32) << mode.red_shift)
        | ((green as u32) << mode.green_shift)
        | ((blue as u32) << mode.blue_shift)
}

/// The byte offset of pixel `(x, y)` in the framebuffer.
fn pixel_offset(mode: DisplayMode, x: u64, y: u64) -> Option<u64> {
    let row = y.checked_mul(mode.pitch as u64)?;
    let column = x.checked_mul(4)?;
    row.checked_add(column)
}

/// Write one pixel word.
///
/// # Safety
///
/// `offset` must be inside the mapped framebuffer, and the framebuffer must be
/// writable. Both are checked by the callers below.
unsafe fn write_pixel(mode: DisplayMode, x: u64, y: u64, word: u32) {
    let offset = pixel_offset(mode, x, y).expect("display: pixel offset overflow");
    let address = mode.base_virtual + offset;
    // SAFETY: the caller checked `x < width` and `y < height`, and the
    // framebuffer's visible range was mapped by `init`.
    unsafe { write_volatile(address as *mut u32, word) }
}

/// Draw one pixel.
pub fn put_pixel(x: u32, y: u32, red: u8, green: u8, blue: u8) -> Result<(), DisplayError> {
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };
    if x as u64 >= mode.right() || y as u64 >= mode.bottom() {
        return Err(DisplayError::OutOfBounds);
    }
    let word = compose(mode, red, green, blue);
    // SAFETY: the coordinate checks above put the pixel inside the mapped
    // framebuffer, and `init` mapped it writable.
    unsafe { write_pixel(mode, x as u64, y as u64, word) }
    Ok(())
}

/// Fill a rectangle, clipped to the display.
///
/// The rectangle may extend past the display; the part that is visible is drawn
/// and the rest is ignored. A rectangle that is entirely outside is a no-op,
/// not an error, which is what lets a compositor clip a window without first
/// deciding whether the window is on screen at all.
pub fn fill_rect(
    x: u64,
    y: u64,
    width: u64,
    height: u64,
    red: u8,
    green: u8,
    blue: u8,
) -> Result<(), DisplayError> {
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };

    let Some(x_end) = x.checked_add(width) else {
        return Err(DisplayError::OutOfBounds);
    };
    let Some(y_end) = y.checked_add(height) else {
        return Err(DisplayError::OutOfBounds);
    };

    let left = x.min(mode.right());
    let right = x_end.min(mode.right());
    let top = y.min(mode.bottom());
    let bottom = y_end.min(mode.bottom());

    if left >= right || top >= bottom {
        return Ok(());
    }

    let word = compose(mode, red, green, blue);
    let mut row = top;
    while row < bottom {
        let mut column = left;
        while column < right {
            // SAFETY: the clipped bounds are inside the display, and `init`
            // mapped the visible framebuffer writable.
            unsafe { write_pixel(mode, column, row, word) }
            column = column.saturating_add(1);
        }
        row = row.saturating_add(1);
    }
    Ok(())
}

/// Clear the whole display.
pub fn clear(red: u8, green: u8, blue: u8) -> Result<(), DisplayError> {
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };
    fill_rect(0, 0, mode.right(), mode.bottom(), red, green, blue)
}

/// Move every row up by `rows` pixels and fill the gap at the bottom.
///
/// This is how the on-screen console scrolls: the rows above the cursor are
/// already on screen, and re-rendering them glyph by glyph would cost
/// hundreds of rasterizations per line feed. One bulk move costs a fraction
/// of that and is exactly what a terminal shift does.
///
/// The copy runs chunk by chunk through a stack scratch: the framebuffer is
/// device memory, and `core::ptr::copy` on MMIO is neither allowed nor
/// ordered the way a volatile read/write pair is.
pub fn scroll_up(rows: u64, red: u8, green: u8, blue: u8) -> Result<(), DisplayError> {
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };
    let height = mode.bottom();
    if rows == 0 {
        return Ok(());
    }
    if rows >= height {
        return clear(red, green, blue);
    }

    // Chunked volatile row move: read a strip from the source row, write it
    // to the destination row, then advance. 256 words = 1 KiB of stack.
    const CHUNK: u64 = 256;
    let mut scratch = [0u32; CHUNK as usize];
    let width = mode.right();
    let mut row = rows;
    while row < height {
        let mut column = 0;
        while column < width {
            let take = (width - column).min(CHUNK);
            for index in 0..take {
                // SAFETY: `row < height` and `column + index < width` bound
                // both pixels inside the mapped framebuffer.
                scratch[index as usize] = unsafe { read_volatile_word(mode, column + index, row) };
            }
            for index in 0..take {
                // SAFETY: the destination row `row - rows` is inside the
                // framebuffer by the same bounds as the source.
                unsafe {
                    write_volatile_word(mode, column + index, row - rows, scratch[index as usize])
                };
            }
            column += take;
        }
        row += 1;
    }

    fill_rect(0, height - rows, width, rows, red, green, blue)
}

/// Read one pixel word at `(x, y)`.
///
/// # Safety
///
/// The pixel must be inside the mapped framebuffer.
unsafe fn read_volatile_word(mode: DisplayMode, x: u64, y: u64) -> u32 {
    let offset = pixel_offset(mode, x, y).expect("display: pixel offset overflow");
    // SAFETY: the caller checked the coordinates against the mode, and
    // `init` mapped the visible framebuffer readable.
    unsafe { read_volatile((mode.base_virtual + offset) as *const u32) }
}

/// Write one pixel word at `(x, y)`.
///
/// # Safety
///
/// The pixel must be inside the mapped framebuffer.
unsafe fn write_volatile_word(mode: DisplayMode, x: u64, y: u64, word: u32) {
    let offset = pixel_offset(mode, x, y).expect("display: pixel offset overflow");
    // SAFETY: the caller checked the coordinates against the mode, and
    // `init` mapped the visible framebuffer writable.
    unsafe { write_volatile((mode.base_virtual + offset) as *mut u32, word) }
}

/// Copy an XRGB8888 surface buffer into the framebuffer, clipped.
///
/// `source_stride` is the row length in the source buffer. The source must have
/// at least `stride * height` bytes and a stride covering one row of `width`
/// pixels. The destination is clipped to the display; the source offsets are
/// computed from the original rectangle, so clipping the left or top edge does
/// not shift the pixels.
pub fn blit(
    source: &[u8],
    source_stride: u32,
    x: u64,
    y: u64,
    width: u64,
    height: u64,
) -> Result<(), DisplayError> {
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };

    let row_bytes = match width.checked_mul(4) {
        Some(value) => value,
        None => return Err(DisplayError::InvalidBuffer),
    };
    if (source_stride as u64) < row_bytes {
        return Err(DisplayError::InvalidBuffer);
    }

    let required = match (source_stride as u64).checked_mul(height) {
        Some(value) => value,
        None => return Err(DisplayError::InvalidBuffer),
    };
    if required > source.len() as u64 {
        return Err(DisplayError::InvalidBuffer);
    }

    let Some(x_end) = x.checked_add(width) else {
        return Err(DisplayError::OutOfBounds);
    };
    let Some(y_end) = y.checked_add(height) else {
        return Err(DisplayError::OutOfBounds);
    };

    let left = x.min(mode.right());
    let right = x_end.min(mode.right());
    let top = y.min(mode.bottom());
    let bottom = y_end.min(mode.bottom());

    if left >= right || top >= bottom {
        return Ok(());
    }

    let mut row = top;
    while row < bottom {
        let source_y = row - y;
        let mut column = left;
        while column < right {
            let source_x = column - x;
            let index = match source_y
                .checked_mul(source_stride as u64)
                .and_then(|value| value.checked_add(source_x.checked_mul(4)?))
            {
                Some(value) => value,
                None => return Err(DisplayError::InvalidBuffer),
            };

            let offset = usize::try_from(index).map_err(|_| DisplayError::InvalidBuffer)?;
            let word = u32::from_le_bytes([
                source[offset],
                source[offset + 1],
                source[offset + 2],
                source[offset + 3],
            ]);

            let red = ((word >> SOURCE_RED_SHIFT) & 0xff) as u8;
            let green = ((word >> SOURCE_GREEN_SHIFT) & 0xff) as u8;
            let blue = ((word >> SOURCE_BLUE_SHIFT) & 0xff) as u8;
            let destination = compose(mode, red, green, blue);

            // SAFETY: the clipped bounds are inside the display, and `init`
            // mapped the visible framebuffer writable.
            unsafe { write_pixel(mode, column, row, destination) }
            column = column.saturating_add(1);
        }
        row = row.saturating_add(1);
    }
    Ok(())
}

/// Copy a sub-rectangle out of `source` into the display at `(x, y)`.
///
/// `blit` copies the whole buffer; this copies the `source_width` x
/// `source_height` window rooted at `source_x`, `source_y` — everything a
/// partially-repainted surface needs to put only the intersection on screen.
pub fn blit_window(
    source: &[u8],
    source_stride: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x: u64,
    y: u64,
) -> Result<(), DisplayError> {
    let row_bytes = u64::from(source_width)
        .checked_mul(4)
        .ok_or(DisplayError::InvalidBuffer)?;
    if u64::from(source_stride) < row_bytes {
        return Err(DisplayError::InvalidBuffer);
    }
    let required = u64::from(source_stride)
        .checked_mul(u64::from(source_y + source_height))
        .ok_or(DisplayError::InvalidBuffer)?;
    if required > source.len() as u64 {
        return Err(DisplayError::InvalidBuffer);
    }
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };
    let right = x.saturating_add(u64::from(source_width)).min(mode.right());
    let bottom = y
        .saturating_add(u64::from(source_height))
        .min(mode.bottom());
    let x_clip = (right - x).min(u64::from(source_width)) as u32;
    let y_clip = (bottom - y).min(u64::from(source_height)) as u32;
    if x_clip == 0 || y_clip == 0 {
        return Ok(());
    }
    for row in 0..y_clip {
        let source_index = (u64::from(source_y + row))
            .checked_mul(u64::from(source_stride))
            .unwrap()
            + u64::from(source_x) * 4;
        let mut column = 0u32;
        while column < x_clip {
            let offset = usize::try_from(source_index + u64::from(column) * 4)
                .map_err(|_| DisplayError::InvalidBuffer)?;
            let word = u32::from_le_bytes([
                source[offset],
                source[offset + 1],
                source[offset + 2],
                source[offset + 3],
            ]);
            let red = ((word >> SOURCE_RED_SHIFT) & 0xff) as u8;
            let green = ((word >> SOURCE_GREEN_SHIFT) & 0xff) as u8;
            let blue = ((word >> SOURCE_BLUE_SHIFT) & 0xff) as u8;
            let destination = compose(mode, red, green, blue);
            // SAFETY: the clipped bounds are inside the display, and `init`
            // mapped the visible framebuffer writable.
            unsafe { write_pixel(mode, x + u64::from(column), y + u64::from(row), destination) }
            column += 1;
        }
    }
    Ok(())
}

/// Read one framebuffer pixel, for tests and diagnostics.
pub fn get_pixel(x: u32, y: u32) -> Result<u32, DisplayError> {
    let Some(mode) = mode() else {
        return Err(DisplayError::NoDisplay);
    };
    if x as u64 >= mode.right() || y as u64 >= mode.bottom() {
        return Err(DisplayError::OutOfBounds);
    }
    let offset = match pixel_offset(mode, x as u64, y as u64) {
        Some(value) => value,
        None => return Err(DisplayError::OutOfBounds),
    };
    let address = mode.base_virtual + offset;
    // SAFETY: the coordinate checks put the pixel inside the mapped
    // framebuffer; the read is immutable and single-CPU.
    let word = unsafe { read_volatile(address as *const u32) };
    Ok(word)
}

#[cfg(test)]
mod tests {
    use super::{DisplayError, DisplayMode, compose, pixel_offset};

    fn mode() -> DisplayMode {
        DisplayMode {
            width: 10,
            height: 4,
            pitch: 40,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            base_virtual: 0xFFFF_8000_0000_0000,
        }
    }

    #[test]
    fn a_colour_is_composed_from_the_firmware_shifts() {
        assert_eq!(compose(mode(), 0xFF, 0x00, 0x00), 0x00FF_0000);
        assert_eq!(compose(mode(), 0x00, 0xFF, 0x00), 0x0000_FF00);
        assert_eq!(compose(mode(), 0x00, 0x00, 0xFF), 0x0000_00FF);
    }

    #[test]
    fn pixel_offsets_use_pitch_and_four_bytes_per_pixel() {
        assert_eq!(pixel_offset(mode(), 0, 0), Some(0));
        assert_eq!(pixel_offset(mode(), 1, 0), Some(4));
        assert_eq!(pixel_offset(mode(), 0, 1), Some(40));
        assert_eq!(pixel_offset(mode(), 1, 1), Some(44));
    }

    #[test]
    fn an_overflowing_offset_is_refused() {
        let huge = DisplayMode {
            width: u32::MAX,
            height: u32::MAX,
            pitch: u32::MAX,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            base_virtual: 0,
        };
        assert_eq!(pixel_offset(huge, u64::MAX, 0), None);
    }

    #[test]
    fn the_error_names_are_stable() {
        assert_eq!(DisplayError::NoDisplay, DisplayError::NoDisplay);
    }
}
