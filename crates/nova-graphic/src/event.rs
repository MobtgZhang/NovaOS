//! Input event types — a Qt6-flavoured minimal set.

/// Which mouse button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Mouse button state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseState {
    Pressed,
    Released,
}

/// One input event. Coordinates are in widget-space (pixels).
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub kind: EventKind,
}

/// Categories of input event.
#[derive(Clone, Copy, Debug)]
pub enum EventKind {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        x: i32,
        y: i32,
        button: MouseButton,
        state: MouseState,
    },
    KeyChar {
        ch: char,
    },
    KeyEnter,
    KeyBackspace,
    KeyEscape,
    FocusIn,
    FocusOut,
    Noop,
}
