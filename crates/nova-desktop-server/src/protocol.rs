//! The Wayland-shaped wire protocol between clients and the desktop server.
//!
//! # The contract
//!
//! Clients and the server speak byte messages, in both directions. The
//! messages are append-only and versioned like every other protocol in
//! NovaOS: nothing is renumbered, nothing is reused, and a client that
//! predates an operation gets `Unsupported` rather than a surprise.
//!
//! The shape mirrors Wayland, trimmed to what NovaOS already has:
//!
//! - A **surface** is a rectangle of pixels a client paints. The server owns
//!   placement; the client owns content. `attach` hands the server the buffer;
//!   `damage` tells it what changed; `commit` publishes the frame.
//! - The **seat** is the pointer and the keyboard. The server routes input
//!   to the surface under the pointer (or the focused surface for keys) and
//!   never lets a client inject input.
//! - **Frame callbacks** (`frame_done`) pace the client: a client that wants
//!   to animate needs to know when its frame landed, which is what this
//!   event is for.

/// The maximum payload of a wire message.
///
/// Kept equal to the ABI's inline-message bound; duplicated here on purpose
/// so this crate builds with no dependencies.
pub const MAX_INLINE_PAYLOAD: usize = 64;

/// The protocol version. Minor bumps are append-only.
pub const VERSION: u16 = 1;

/// The version of the protocol, carried by every message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Version {
    /// The major version: different majors never negotiate.
    pub major: u16,
    /// The minor version: append-only, negotiable downward.
    pub minor: u16,
}

impl Version {
    /// The current version.
    pub const CURRENT: Version = Version {
        major: 1,
        minor: VERSION,
    };
}

/// The maximum payload of a wire message.
pub const MAX_MESSAGE: usize = MAX_INLINE_PAYLOAD;

/// A client's request to the server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// Create a new surface of the given size.
    CreateSurface {
        /// The client's width hint, in pixels.
        width: u32,
        /// The client's height hint, in pixels.
        height: u32,
    },
    /// Hand the server a new buffer for the surface, atomically replacing
    /// the old one when the client next commits.
    Attach {
        /// The surface to attach to.
        surface: u32,
        /// The buffer's width, in pixels.
        width: u32,
        /// The buffer's height, in pixels.
        height: u32,
        /// The bytes per row of the buffer.
        stride: u32,
    },
    /// Mark a rectangle of the surface damaged.
    Damage {
        /// The surface whose pixels changed.
        surface: u32,
        /// The damaged rectangle, in surface-local pixels.
        x: i32,
        /// The damaged rectangle's top, in surface-local pixels.
        y: i32,
        /// The damaged rectangle's width.
        width: u32,
        /// The damaged rectangle's height.
        height: u32,
    },
    /// Publish the pending attach and damages as one frame.
    Commit {
        /// The surface to commit.
        surface: u32,
    },
    /// Ask the server to move the surface on screen.
    ///
    /// The server may refuse or adjust the position — placement is the
    /// server's business, not the client's.
    Place {
        /// The surface to move.
        surface: u32,
        /// The requested x, in output pixels.
        x: i32,
        /// The requested y, in output pixels.
        y: i32,
    },
    /// Raise the surface to the top of the z-order, or lower it.
    Raise {
        /// The surface to raise.
        surface: u32,
    },
    /// Destroy the surface. All state tied to it goes with it.
    DestroySurface {
        /// The surface to destroy.
        surface: u32,
    },
}

impl Request {
    /// The opcode carried on the wire.
    pub const fn opcode(&self) -> u16 {
        match self {
            Request::CreateSurface { .. } => 1,
            Request::Attach { .. } => 2,
            Request::Damage { .. } => 3,
            Request::Commit { .. } => 4,
            Request::Place { .. } => 5,
            Request::Raise { .. } => 6,
            Request::DestroySurface { .. } => 7,
        }
    }
}

/// A server's event to a client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A surface was created; the reply carries the server's id for it.
    SurfaceCreated {
        /// The id the server assigned.
        id: u32,
    },
    /// The frame that included this client's surface landed on the display.
    ///
    /// The client may now start rendering its next frame.
    FrameDone {
        /// The surface whose frame landed.
        surface: u32,
        /// The server's frame counter.
        serial: u32,
    },
    /// The pointer entered the surface.
    PointerEnter {
        /// The surface the pointer is now over.
        surface: u32,
    },
    /// The pointer left the surface.
    PointerLeave {
        /// The surface the pointer is no longer over.
        surface: u32,
    },
    /// The pointer moved over the surface, in surface-local coordinates.
    PointerMotion {
        /// The surface the pointer is over.
        surface: u32,
        /// The pointer x, in surface-local pixels.
        x: i32,
        /// The pointer y, in surface-local pixels.
        y: i32,
    },
    /// A button was pressed or released over the surface.
    PointerButton {
        /// The surface the pointer is over.
        surface: u32,
        /// Which button: 1 = primary, 2 = middle, 4 = secondary.
        button: u32,
        /// Whether the button went down (1) or up (0).
        state: u32,
    },
    /// A key was pressed or released while this surface had focus.
    KeyEvent {
        /// The focused surface.
        surface: u32,
        /// The set-1 scancode of the key.
        scancode: u32,
        /// Whether the key went down (1) or up (0).
        state: u32,
    },
    /// The server's answer to [`Request::Raise`].
    Raised {
        /// The surface that was raised.
        surface: u32,
    },
    /// The server's answer to [`Request::DestroySurface`].
    Destroyed {
        /// The surface that was destroyed.
        surface: u32,
    },
}

impl Event {
    /// The opcode carried on the wire.
    pub const fn opcode(&self) -> u16 {
        match self {
            Event::SurfaceCreated { .. } => 1,
            Event::FrameDone { .. } => 2,
            Event::PointerEnter { .. } => 3,
            Event::PointerLeave { .. } => 4,
            Event::PointerMotion { .. } => 5,
            Event::PointerButton { .. } => 6,
            Event::KeyEvent { .. } => 7,
            Event::Raised { .. } => 8,
            Event::Destroyed { .. } => 9,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_are_stable_and_distinct() {
        let requests = [
            Request::CreateSurface {
                width: 1,
                height: 1,
            },
            Request::Attach {
                surface: 1,
                width: 1,
                height: 1,
                stride: 1,
            },
            Request::Damage {
                surface: 1,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            Request::Commit { surface: 1 },
            Request::Place {
                surface: 1,
                x: 0,
                y: 0,
            },
            Request::Raise { surface: 1 },
            Request::DestroySurface { surface: 1 },
        ];
        let opcodes: [u16; 7] = requests.map(|r| r.opcode());
        for (index, opcode) in opcodes.iter().enumerate() {
            assert!(*opcode >= 1, "opcode {index} is zero");
            for (other_index, other) in opcodes.iter().enumerate() {
                if index != other_index {
                    assert_ne!(opcode, other, "opcode {index} collides");
                }
            }
        }
    }

    #[test]
    fn max_message_matches_the_abi_bound() {
        // The bound is fixed by the ABI; a change here is a protocol break,
        // not a tweak.
        assert_eq!(MAX_INLINE_PAYLOAD, 64);
    }
}
