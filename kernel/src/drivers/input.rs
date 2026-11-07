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
use nova_protocols::input::KeyState;

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
unsafe fn read_byte() -> Option<u8> {
    // SAFETY: `0x64` is the status port and `0x60` is the data port; the
    // driver owns both and runs single-CPU.
    if unsafe { inb(COMMAND_PORT) } & OUTPUT_FULL == 0 {
        return None;
    }
    // SAFETY: the status bit said a byte is ready, and reading the data port
    // acknowledges it.
    Some(unsafe { inb(DATA_PORT) })
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
    }
}

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
        let Some(byte) = (unsafe { read_byte() }) else {
            break;
        };

        // Set 1 scancodes use bit 7 for release. The low seven bits are the
        // scancode the protocol's `Input.key` carries.
        let state = if byte & 0x80 != 0 {
            KeyState::Released
        } else {
            KeyState::Pressed
        };
        let scancode = (byte & 0x7f) as u16;

        if parts
            .input
            .route_key(scancode, state, parts.compositor)
            .is_ok()
        {
            routed += 1;
        }
    }

    if routed > 0 {
        let _ = parts.compositor.present();
    }
    routed
}
