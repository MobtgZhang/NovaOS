//! The PS/2 keyboard/mouse polling driver.
//!
//! # What this module owns
//!
//! The legacy PS/2 controller is at two IO ports: `0x60` for data and `0x64`
//! for status and commands. This module brings the controller up and polls
//! keyboard scancodes into the kernel's input service. The service, not the
//! application, decides which surface receives the event.
//!
//! # Why polling rather than interrupts
//!
//! M6 has a timer tick and a service dispatcher, but no IRQ capability model
//! for the PS/2 controller yet. Polling is the smallest path that keeps the
//! trusted source in the kernel: the driver reads the device, the service
//! routes the event. An application never touches an IO port.
//!
//! # Mouse data
//!
//! The 8042 data port carries both keyboard scancodes and mouse packet bytes.
//! This prototype polls keyboard scancodes; pointer routing is exercised by the
//! service protocol, whose trusted producer is the driver's pointer path. The
//! protocol refuses an application that names a surface it does not own the
//! focus of — that is the security boundary, not a client-side claim.
//!
//! # Port access discipline
//!
//! The ports are driven through [`crate::arch::x86_64::port_io`], the one
//! module allowed to issue `in`/`out`. This driver is kernel-only and is never
//! reachable from user space.

use crate::arch::x86_64::port_io::{inb, outb};
use crate::klog;
use crate::logger::Level;
use core::sync::atomic::{AtomicBool, Ordering};

use nova_protocols::input::KeyState;

use crate::stdin;

/// The PS/2 data port.
const DATA_PORT: u16 = 0x60;

/// The PS/2 status/command port.
const COMMAND_PORT: u16 = 0x64;

/// Output-buffer-full bit in the status register.
const OUTPUT_FULL: u8 = 1 << 0;

/// Input-buffer-full bit in the status register.
const INPUT_FULL: u8 = 1 << 1;

/// The maximum number of polls one driver call will drain.
///
/// A device that never clears its output buffer would otherwise hold the
/// kernel in a loop. The bound is a quota, not a correctness claim.
const MAX_DRAIN: usize = 32;
/// The bit in the 8042 status register that flags which device produced
/// the byte: 0 = keyboard, 1 = mouse.
const MOUSE_BIT: u8 = 0x20;

/// Track the mouse packet state. Each mouse packet is 3 bytes (flags, dx, dy).
#[derive(Clone, Copy, Debug)]
struct MousePacket {
    bytes: [u8; 3],
    index: usize,
}

impl MousePacket {
    const fn new() -> Self {
        Self {
            bytes: [0; 3],
            index: 0,
        }
    }
    fn feed(&mut self, byte: u8) -> Option<MouseDecoded> {
        self.bytes[self.index] = byte;
        self.index += 1;
        if self.index < 3 {
            return None;
        }
        self.index = 0;
        let flags = self.bytes[0];
        let buttons = flags & 0x07;
        // Set-1 mouse: dx / dy are signed 8-bit magnitudes with the sign in
        // flags bits 4/5. Casting through `i8` performs the sign extension
        // we need, and avoids rewriting by hand.
        let dx = if flags & 0x10 != 0 {
            (self.bytes[1] as i8) as i16
        } else {
            self.bytes[1] as i16
        };
        let dy = if flags & 0x20 != 0 {
            (self.bytes[2] as i8) as i16
        } else {
            self.bytes[2] as i16
        };
        Some(MouseDecoded {
            buttons: buttons as u32,
            dx,
            dy,
        })
    }
}

/// A decoded mouse packet: button mask (low 3 bits) and signed deltas.
#[derive(Clone, Copy, Debug)]
struct MouseDecoded {
    buttons: u32,
    dx: i16,
    dy: i16,
}

/// The mouse packet being assembled, guarded to a static.
///
/// # Safety
///
/// The poller owns this state and runs single-threaded on one CPU. The
/// `poll()` call path is `&mut`-only. Hence `Sync` is hand-written, and no
/// accessor exposes a shared reference. This field must stay private.
static MOUSE_PACKET: MousePacketCell = MousePacketCell::new();

/// A `Sync` wrapper around the mouse packet assembly state.
struct MousePacketCell(core::cell::UnsafeCell<MousePacket>);

impl MousePacketCell {
    const fn new() -> Self {
        Self(core::cell::UnsafeCell::new(MousePacket::new()))
    }
    // SAFETY: single-CPU, interrupts masked, only owner.
    unsafe fn get(&self) -> &mut MousePacket {
        // SAFETY: the caller is the only reference operator of MT single CPU end.
        unsafe { &mut *self.0.get() }
    }
}

// SAFETY: `poll()` runs with one CPU at a time and interrupts masked.
unsafe impl Sync for MousePacketCell {}

/// Wait until the controller can accept a command byte.
///
/// Returns `false` if the input buffer never clears. The caller logs the
/// failure rather than hanging the boot.
unsafe fn input_buffer_clear() -> bool {
    for _ in 0..MAX_DRAIN {
        // SAFETY: `0x64` is the controller's status port, and the caller is
        // the kernel's boot path with the controller present.
        if unsafe { inb(COMMAND_PORT) } & INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

/// Read one byte from the controller's output buffer.
///
/// Returns `None` when no byte is ready. Reading the data port consumes the
/// byte, which is exactly what polling needs.
/// One byte from the 8042 output port, paired with the device that sent it.
struct PortByte {
    /// The raw data byte.
    byte: u8,
    /// Whether the data port reports a mouse produced the byte.
    from_mouse: bool,
}

unsafe fn read_byte() -> Option<PortByte> {
    // SAFETY: `0x64` is the status port and `0x60` is the data port; the
    // driver owns both and runs single-CPU.
    let status = unsafe { inb(COMMAND_PORT) };
    if status & OUTPUT_FULL == 0 {
        return None;
    }
    // SAFETY: the status bit said a byte is ready, and reading the data port
    // acknowledges it.
    let byte = unsafe { inb(DATA_PORT) };
    Some(PortByte {
        byte,
        from_mouse: status & MOUSE_BIT != 0,
    })
}

/// Send a command byte to the controller.
unsafe fn write_command(command: u8) -> bool {
    if !unsafe { input_buffer_clear() } {
        return false;
    }
    // SAFETY: the command port accepts byte-sized writes, and the input buffer
    // is clear.
    unsafe { outb(COMMAND_PORT, command) };
    true
}

/// Drain any stale bytes the firmware left in the output buffer.
unsafe fn drain_output() {
    for _ in 0..MAX_DRAIN {
        if unsafe { read_byte() }.is_none() {
            break;
        }
    }
}

/// The mouse's absolute coordinate, owned by the poller.
///
/// # Safety
///
/// The polling path is single CPU at a time (interrupts masked) and the
/// cell is only aliased inside this module.
struct MousePosCell(core::cell::UnsafeCell<(u32, u32)>);
impl MousePosCell {
    const fn new() -> Self {
        Self(core::cell::UnsafeCell::new((512, 384)))
    }
    // SAFETY: poller-single-instance; returns copy; never aliased.
    unsafe fn get_copy(&self) -> (u32, u32) {
        // SAFETY: the caller is the only operator of the cell.
        unsafe { *self.0.get() }
    }
    // SAFETY: poller-single-instance; the cell is never aliased by two
    // callers, as the poller is `&mut`-owned from one path at a time.
    unsafe fn set(&self, value: (u32, u32)) {
        // SAFETY: the caller is the only operator of the cell.
        unsafe { *self.0.get() = value };
    }
}

// SAFETY: poller-single-instance and interrupts are masked on the caller.
unsafe impl Sync for MousePosCell {}

static MOUSE_POS: MousePosCell = MousePosCell::new();

pub fn mouse_position() -> Option<(u32, u32)> {
    // SAFETY: poller-single-instance.
    Some(unsafe { MOUSE_POS.get_copy() })
}

/// The mouse's position is clamped to the display's own edges: the
/// coordinate is a *point*, and it must be able to reach every pixel of
/// the desktop. The sprite drawn from it is clipped by
/// [`crate::drivers::display::put_pixel`], which drops pixels outside the
/// framebuffer — the arrow loses a few edge pixels at the border rather
/// than the user losing access to the edge.
fn mouse_move(dx: i16, dy: i16) {
    // SAFETY: poller-single-instance; the cell is never aliased by two
    // callers, as the poller is `&mut`-owned from one path at a time.
    let pos = unsafe { MOUSE_POS.get_copy() };
    // Do not allow underflow.
    let new_x = pos.0 as i32 + dx as i32;
    let new_y = pos.1 as i32 - dy as i32;
    // The whole desktop is reachable; the sprite is clipped, not the
    // coordinate.
    let (limit_x, limit_y) = match crate::drivers::display::mode() {
        Some(mode) => (
            mode.width.saturating_sub(1) as i32,
            mode.height.saturating_sub(1) as i32,
        ),
        None => (i32::MAX, i32::MAX),
    };
    // SAFETY: poller-single-instance.
    unsafe {
        MOUSE_POS.set((
            new_x.clamp(0, limit_x) as u32,
            new_y.clamp(0, limit_y) as u32,
        ));
    }
}

/// Bring up the PS/2 controller.
///
/// # Safety
///
/// Must run once from the boot path, with interrupts masked, after the
/// architecture has initialised IO-port access. It only touches the two PS/2
/// ports.
pub unsafe fn init() {
    unsafe {
        drain_output();

        // Enable keyboard scanning. This is a keyboard command sent through the
        // controller's data port; the controller forwards it to the keyboard.
        if input_buffer_clear() {
            // SAFETY: the data port is the keyboard command channel.
            outb(DATA_PORT, 0xF4);
        }

        // Enable the mouse. The controller command is 0xA8; a controller that
        // rejects it is logged, not treated as a boot failure.
        if write_command(0xA8) {
            klog!(Level::Info, "input: PS/2 controller ready");
        } else {
            klog!(
                Level::Warn,
                "input: PS/2 controller command was not accepted"
            );
        }

        // Turn on mouse *data reporting* as well: 0xA8 only makes the aux
        // port visible, it does not make the device stream. The controller
        // command 0xD4 routes the next data byte to the mouse, and 0xF4 is
        // the mouse's "enable data reporting" command. Without this the
        // PS/2 mouse never sends a packet, and the cursor is dead while
        // every keyboard key works. The mouse ACKs with 0xFA; a controller
        // that does not answer is logged, not treated as a boot failure.
        if write_command(0xD4) && input_buffer_clear() {
            // SAFETY: the aux data port is the mouse command channel and
            // the input buffer was just confirmed clear.
            unsafe { outb(DATA_PORT, 0xF4) };
            klog!(Level::Info, "input: PS/2 mouse data reporting enabled");
        } else {
            klog!(Level::Warn, "input: PS/2 mouse did not accept 0xF4");
        }
    }
}

/// The two shift keys. Tracked here because they are the only modifiers
/// that ever flip the decode of a printable key. Caps lock is not modelled.
static SHIFT_HELD: AtomicBool = AtomicBool::new(false);

/// Poll the PS/2 keyboard and route any scancode to the focused surface.
///
/// # Safety
///
/// Called from the timer path, with interrupts masked and with the service
/// state installed. The driver owns the two PS/2 ports.
pub unsafe fn poll() -> usize {
    // SAFETY: the service state is installed during boot and the timer path
    // holds no other mutable borrow of it.
    let mut kernel = crate::services::state();
    let parts = kernel.split();
    let mut routed = 0usize;

    for _ in 0..MAX_DRAIN {
        let Some(port_byte) = (unsafe { read_byte() }) else {
            break;
        };

        // The mouse has its own packet framing; keyboard bytes are never
        // interpreted as mouse packets. Mouse packets are 3 bytes; the
        // header carries button bits, overflow markers and the sign of the
        // deltas. We track the cursor position locally, then forward delta
        // routing through the input service so the compositor can move the
        // focused window's pointer.
        if port_byte.from_mouse {
            let packet = unsafe { MOUSE_PACKET.get() };
            // The only command ever sent to the aux device is 0xF4 (enable
            // data reporting), whose ACK is 0xFA. An 0xFA that arrives at a
            // packet boundary is always that ACK and can never start a
            // packet; consuming it here keeps the three-byte framing
            // intact for every byte after it. A mid-packet 0xFA is left
            // alone — framing resyncs on the next three-byte boundary.
            if port_byte.byte == 0xFA && packet.index == 0 {
                klog!(Level::Debug, "mouse: dropping stray 0xFA (0xF4 ACK)");
                continue;
            }
            if let Some(decoded) = packet.feed(port_byte.byte) {
                // Update the absolute position first, then route it: the
                // compositor's hit test and the on-screen arrow both read
                // the same cell.
                mouse_move(decoded.dx, decoded.dy);
                let _btn = decoded.buttons;
                // Route pointer through service; let GUI apps react to clicks.
                let (mx, my) = mouse_position().unwrap_or((0, 0));
                let _ =
                    parts
                        .input
                        .route_pointer(i64::from(mx), i64::from(my), _btn, parts.compositor);
                // Pointer motion is a frame with something to show: paint it.
                parts.compositor.note_pointer_moved();
                routed += 1;
            }
            continue;
        }

        let byte = port_byte.byte;

        // Decode the byte and feed stdin: the emergency console has no
        // window to route to, so bytes land in the kernel's stdin ring and
        // every user-space `read(0)` picks them up. The compositor gets a
        // copy through the usual route so the GUI keeps working as before.
        let scancode_u8 = stdin::scancode_of(byte);
        let state = if stdin::is_press(byte) {
            KeyState::Pressed
        } else {
            KeyState::Released
        };

        // Shift tracking is local to the scancode stream, not shared with
        // the service layer; an extra copy keeps the two independent.
        match scancode_u8 {
            stdin::LSHIFT | stdin::RSHIFT => {
                SHIFT_HELD.store(state == KeyState::Pressed, Ordering::Relaxed);
            }
            _ => {
                if state == KeyState::Pressed {
                    let ascii =
                        stdin::decode_press(scancode_u8, SHIFT_HELD.load(Ordering::Relaxed));
                    if ascii != 0 {
                        // SAFETY: interrupts are masked, as this function's
                        // contract requires.
                        unsafe { stdin::write_byte(ascii) };
                    }
                }
            }
        }

        let scancode = scancode_u8 as u16;
        if parts
            .input
            .route_key(scancode, state, parts.compositor)
            .is_ok()
        {
            routed += 1;
        }
    }

    // Present whatever is pending, whether or not this packet routed an
    // event. The compositor's `present` returns immediately with nothing
    // to do, and when there is something to do this is the *timer* that
    // paces it: the local-APIC tick runs far faster than any display
    // needs (about 15 kHz under QEMU's 1 GHz APIC bus), and it is the
    // dirty-driven present — plus the rate limiter in
    // `display::flush` — that turns those ticks into roughly a hundred
    // device frames a second instead of thousands.
    let _ = parts.compositor.present();
    routed
}
