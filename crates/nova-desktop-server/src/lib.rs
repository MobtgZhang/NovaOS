//! NovaOS's desktop server core: the Wayland-shaped engine between
//! applications that draw their own pixels and the display that shows them.
//!
//! # What a desktop server is, on Linux
//!
//! A Wayland compositor is a user-space process that owns the display and the
//! seat: clients hand it surfaces of pixels, report which rectangles changed,
//! and receive input routed back to the surface under the pointer. The kernel
//! is deliberately dumb in that model — it forwards hardware events and
//! nothing more.
//!
//! # What this is here
//!
//! NovaOS has no user-space service that owns the framebuffer yet; the
//! compositor lives in the kernel (`kernel/src/services/compositor.rs`) the
//! same way the file and input services do. This crate is the *policy* half
//! of that service, lifted out so it can be built and tested twice: once for
//! the bare-metal kernel and once for the host test runner. What remains in
//! the kernel is the *mechanism* — the framebuffer mapping, the blits, the
//! page-table dance. Everything that decides *what* needs painting, *where*
//! the pointer goes, and *who owns the keyboard* lives here.
//!
//! # The three modules
//!
//! - [`protocol`] — the wire messages clients and the server exchange,
//!   versioned like everything else that crosses a boundary in this system.
//! - [`damage`] — the Wayland damage model: bounded accumulation of
//!   dirty rectangles per surface, a clip to the output, and a decision of
//!   whether a frame repaints everything or only the damaged regions.
//! - [`server`] — the server state: surfaces, z-order, the seat (pointer
//!   position and focused surface), hit testing, and the frame decision.
//!
//! # Why the damage model is the whole point
//!
//! A desktop that repaints the whole screen on every mouse tick flickers and
//! burns frames. Wayland's answer is explicit damage: a client (or here, the
//! cursor code) names the rectangles that changed, and the compositor
//! repaints exactly those. The kernel uses [`damage::DamageAccumulator`] for
//! its partial-repaint paths; the full-repaint fallback survives only for
//! cases that genuinely need it (a surface going full-damage, a first attach,
//! a resize).

#![no_std]
#![forbid(unsafe_code)]

pub mod damage;
pub mod protocol;
pub mod seat;
pub mod server;

pub use damage::{DamageAccumulator, Repaint, SurfaceDamage};
pub use protocol::{Event, Request, Version};
pub use seat::{Seat, SeatEvent};
pub use server::{DesktopServer, Frame, Surface};
