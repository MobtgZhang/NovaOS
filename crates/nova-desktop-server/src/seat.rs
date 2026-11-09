//! The seat: the pointer and the keyboard, and who has them.
//!
//! One pointer, one keyboard, N surfaces. The seat routes each event to the
//! one surface it belongs to — pointer events to whatever is under the
//! pointer, key events to whatever has focus. A client never asks the seat
//! directly; it receives what the seat decided to send it.
//!
//! Focus follows the pointer: a primary press focuses the surface underneath
//! it (Wayland's default focus mode). Key events go to the focused surface,
//! with a caveat that matters for the emergency terminal — a surface that
//! asked for keyboard grab owns every key until it releases the grab.

/// A seat event, already routed by the server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeatEvent {
    /// The pointer entered the surface.
    PointerEnter {
        /// The surface entered.
        surface: u32,
    },
    /// The pointer left the surface.
    PointerLeave {
        /// The surface left.
        surface: u32,
    },
    /// The pointer moved over the surface.
    PointerMotion {
        /// The surface entered.
        surface: u32,
        /// The pointer x in surface-local pixels.
        x: i32,
        /// The pointer y in surface-local pixels.
        y: i32,
    },
    /// A button was pressed or released.
    PointerButton {
        /// The surface the pointer is over.
        surface: u32,
        /// 1 = primary, 2 = middle, 4 = secondary.
        button: u32,
        /// 1 = down, 0 = up.
        state: u32,
    },
    /// A key was pressed or released on the focused surface.
    Key {
        /// The focused surface.
        surface: u32,
        /// The set-1 scancode.
        scancode: u32,
        /// 1 = down, 0 = up.
        state: u32,
    },
}

/// The seat's decision about the next event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeatDecision {
    /// Deliver this event to `surface`.
    Deliver(SeatEvent),
    /// The event has nowhere to go and is dropped.
    Drop,
}

/// One seat: pointer position, button mask, and keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seat {
    /// The pointer's position, in output-local pixels.
    pub pointer_x: i32,
    /// The pointer's position, in output-local pixels.
    pub pointer_y: i32,
    /// The currently held buttons: bit 0 primary, bit 1 middle, bit 2 secondary.
    pub buttons: u32,
    /// The surface the keyboard is focused on, if any.
    pub focused: Option<u32>,
}

impl Seat {
    /// A neutral seat: pointer at the origin, nothing pressed, no focus.
    pub const fn new() -> Self {
        Self {
            pointer_x: 0,
            pointer_y: 0,
            buttons: 0,
            focused: None,
        }
    }

    /// Update the pointer position after a movement.
    pub fn pointer_moved(&mut self, x: i32, y: i32) {
        self.pointer_x = x;
        self.pointer_y = y;
    }

    /// Update the button mask after a press or release.
    pub fn button(&mut self, button: u32, down: bool) {
        if down {
            self.buttons |= 1 << button;
        } else {
            self.buttons &= !(1 << button);
        }
    }

    /// Give the keyboard to a surface (focus follows the pointer).
    pub const fn focus(&mut self, surface: Option<u32>) {
        self.focused = surface;
    }
}

impl Default for Seat {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_track_their_bits() {
        let mut seat = Seat::new();
        seat.button(0, true);
        assert_eq!(seat.buttons, 0b001);
        seat.button(2, true);
        assert_eq!(seat.buttons, 0b101);
        seat.button(0, false);
        assert_eq!(seat.buttons, 0b100);
    }

    #[test]
    fn focus_follows_pointer_by_assignment() {
        let mut seat = Seat::new();
        seat.focus(Some(7));
        assert_eq!(seat.focused, Some(7));
        seat.focus(None);
        assert_eq!(seat.focused, None);
    }
}
