//! The input service protocol.
//!
//! # Focus is the compositor's decision
//!
//! `Input.key` names a surface, but naming a surface is not the same as owning
//! focus. The service routes the event to the surface the compositor currently
//! considers focused; a caller that names a different surface does not thereby
//! move focus. That is §15.3 rule 4 and §15.5's boundary: applications may not
//! change focus themselves.
//!
//! # Pointer coordinates are signed
//!
//! A pointer event can land outside a window, and the compositor clips it. The
//! coordinates are `i32`, so a negative coordinate is representable rather than
//! being silently turned into a huge positive one.
//!
//! # Layout is a contract
//!
//! Every request and reply struct is `#[repr(C)]` with compile-time size and
//! offset assertions. The service and its clients are separate binaries.

use crate::ProtocolVersion;

/// The input protocol version this implementation speaks.
pub const INPUT_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// Opcode for a key event.
pub const INPUT_OP_KEY: u64 = 1;

/// Opcode for a pointer event.
pub const INPUT_OP_POINTER: u64 = 2;

/// Opcode for reading the next event addressed to a surface.
pub const INPUT_OP_GET: u64 = 3;

/// Size of a key-event request, in bytes.
pub const INPUT_KEY_REQUEST_SIZE: usize = 12;

/// Size of a pointer-event request, in bytes.
pub const INPUT_POINTER_REQUEST_SIZE: usize = 20;

/// Size of an event-poll request, in bytes.
pub const INPUT_GET_REQUEST_SIZE: usize = 8;

/// Size of an input reply, in bytes.
pub const INPUT_REPLY_SIZE: usize = 12;

/// Size of an event-poll reply, in bytes.
pub const INPUT_GET_REPLY_SIZE: usize = 32;

/// The event kind in a poll reply: no event is pending.
pub const INPUT_EVENT_NONE: u32 = 0;

/// The event kind in a poll reply: a key event.
pub const INPUT_EVENT_KEY: u32 = 1;

/// The event kind in a poll reply: a pointer event.
pub const INPUT_EVENT_POINTER: u32 = 2;

/// Primary pointer button mask.
pub const INPUT_BUTTON_PRIMARY: u32 = 1 << 0;

/// Secondary pointer button mask.
pub const INPUT_BUTTON_SECONDARY: u32 = 1 << 1;

/// Wheel button mask.
pub const INPUT_BUTTON_WHEEL: u32 = 1 << 2;

/// Whether a key is pressed or released.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum KeyState {
    /// The key was released.
    Released = 0,
    /// The key was pressed.
    Pressed = 1,
}

impl KeyState {
    /// The wire value.
    pub const fn as_u16(self) -> u16 {
        match self {
            KeyState::Released => 0,
            KeyState::Pressed => 1,
        }
    }

    /// Recover a state from its wire value.
    pub const fn from_u16(value: u16) -> Option<KeyState> {
        match value {
            0 => Some(KeyState::Released),
            1 => Some(KeyState::Pressed),
            _ => None,
        }
    }

    /// Whether the key is down.
    pub const fn is_pressed(self) -> bool {
        matches!(self, KeyState::Pressed)
    }
}

/// The opcode of an input operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum InputOpcode {
    Key,
    Pointer,
    Get,
}

impl InputOpcode {
    /// The wire value.
    pub const fn as_u64(self) -> u64 {
        match self {
            InputOpcode::Key => INPUT_OP_KEY,
            InputOpcode::Pointer => INPUT_OP_POINTER,
            InputOpcode::Get => INPUT_OP_GET,
        }
    }

    /// Recover an opcode from its wire value.
    pub const fn from_u64(value: u64) -> Option<InputOpcode> {
        match value {
            INPUT_OP_KEY => Some(InputOpcode::Key),
            INPUT_OP_POINTER => Some(InputOpcode::Pointer),
            INPUT_OP_GET => Some(InputOpcode::Get),
            _ => None,
        }
    }
}

/// A key event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InputKeyRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface the event is aimed at.
    pub surface: u32,
    /// The scancode from the input device.
    pub scancode: u16,
    /// Whether the key is pressed or released.
    pub state: KeyState,
}

/// A pointer event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InputPointerRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface the event is aimed at.
    pub surface: u32,
    /// X coordinate, signed and clipped by the compositor.
    pub x: i32,
    /// Y coordinate, signed and clipped by the compositor.
    pub y: i32,
    /// Button mask.
    pub buttons: u32,
}

/// A reply that carries only a status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InputReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// A request for the next event addressed to a surface.
///
/// The surface is the one the caller owns; the service refuses a surface the
/// caller does not own, so a poll cannot read another window's input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InputGetRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface whose queue is read.
    pub surface: u32,
}

/// One event as delivered by an event poll.
///
/// The fixed header is common to both event kinds; the kind field says which
/// of the two payloads is meaningful. Key events fill `state` and `scancode`;
/// pointer events fill `x`, `y` and `buttons`. Fields belonging to the other
/// kind are zero, so a client that reads them gets a defined value rather
/// than stale bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct InputGetReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// One of `INPUT_EVENT_NONE`, `INPUT_EVENT_KEY`, `INPUT_EVENT_POINTER`.
    pub kind: u32,
    /// Key state, for a key event; zero otherwise.
    pub state: u16,
    /// Scancode, for a key event; zero otherwise.
    pub scancode: u16,
    /// X coordinate, for a pointer event; zero otherwise.
    pub x: i32,
    /// Y coordinate, for a pointer event; zero otherwise.
    pub y: i32,
    /// Button mask, for a pointer event; zero otherwise.
    pub buttons: u32,
    /// The surface the event is addressed to.
    pub surface: u32,
}

const _: () = assert!(size_of::<InputKeyRequest>() == INPUT_KEY_REQUEST_SIZE);
const _: () = assert!(size_of::<InputPointerRequest>() == INPUT_POINTER_REQUEST_SIZE);
const _: () = assert!(size_of::<InputReply>() == INPUT_REPLY_SIZE);
const _: () = assert!(size_of::<InputGetRequest>() == INPUT_GET_REQUEST_SIZE);
const _: () = assert!(size_of::<InputGetReply>() == INPUT_GET_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(InputKeyRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(InputKeyRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(InputKeyRequest, scancode) == 8);
const _: () = assert!(core::mem::offset_of!(InputKeyRequest, state) == 10);

const _: () = assert!(core::mem::offset_of!(InputPointerRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(InputPointerRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(InputPointerRequest, x) == 8);
const _: () = assert!(core::mem::offset_of!(InputPointerRequest, y) == 12);
const _: () = assert!(core::mem::offset_of!(InputPointerRequest, buttons) == 16);

const _: () = assert!(core::mem::offset_of!(InputReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(InputReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(InputReply, reserved) == 8);

const _: () = assert!(core::mem::offset_of!(InputGetRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(InputGetRequest, surface) == 4);

const _: () = assert!(core::mem::offset_of!(InputGetReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(InputGetReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(InputGetReply, kind) == 8);
const _: () = assert!(core::mem::offset_of!(InputGetReply, state) == 12);
const _: () = assert!(core::mem::offset_of!(InputGetReply, scancode) == 14);
const _: () = assert!(core::mem::offset_of!(InputGetReply, x) == 16);
const _: () = assert!(core::mem::offset_of!(InputGetReply, y) == 20);
const _: () = assert!(core::mem::offset_of!(InputGetReply, buttons) == 24);
const _: () = assert!(core::mem::offset_of!(InputGetReply, surface) == 28);

#[cfg(test)]
mod tests {
    use super::{
        INPUT_BUTTON_PRIMARY, INPUT_EVENT_KEY, INPUT_EVENT_NONE, INPUT_EVENT_POINTER, INPUT_OP_GET,
        INPUT_OP_KEY, INPUT_OP_POINTER, INPUT_VERSION, InputGetReply, InputGetRequest,
        InputKeyRequest, InputOpcode, InputPointerRequest, InputReply, KeyState,
    };
    use crate::ProtocolVersion;

    #[test]
    fn the_version_is_checked_before_anything_else() {
        assert_eq!(INPUT_VERSION, ProtocolVersion::new(1, 0));
        assert!(crate::negotiate(INPUT_VERSION, INPUT_VERSION).is_ok());
        assert!(crate::negotiate(INPUT_VERSION, ProtocolVersion::new(2, 0)).is_err());
    }

    #[test]
    fn key_states_round_trip_and_reject_the_unknown() {
        assert_eq!(KeyState::Pressed.as_u16(), 1);
        assert_eq!(KeyState::Released.as_u16(), 0);
        assert_eq!(KeyState::from_u16(1), Some(KeyState::Pressed));
        assert_eq!(KeyState::from_u16(0), Some(KeyState::Released));
        assert_eq!(KeyState::from_u16(2), None);
        assert!(KeyState::Pressed.is_pressed());
        assert!(!KeyState::Released.is_pressed());
    }

    #[test]
    fn the_opcodes_round_trip_and_reject_the_unknown() {
        assert_eq!(InputOpcode::Key.as_u64(), INPUT_OP_KEY);
        assert_eq!(InputOpcode::Pointer.as_u64(), INPUT_OP_POINTER);
        assert_eq!(InputOpcode::Get.as_u64(), INPUT_OP_GET);
        assert_eq!(InputOpcode::from_u64(INPUT_OP_KEY), Some(InputOpcode::Key));
        assert_eq!(
            InputOpcode::from_u64(INPUT_OP_POINTER),
            Some(InputOpcode::Pointer)
        );
        assert_eq!(InputOpcode::from_u64(INPUT_OP_GET), Some(InputOpcode::Get));
        assert_eq!(InputOpcode::from_u64(9999), None);
    }

    #[test]
    fn a_poll_request_only_names_a_surface() {
        let request = InputGetRequest {
            version: INPUT_VERSION,
            surface: 3,
        };
        assert_eq!(request.surface, 3);
        assert_eq!(size_of::<InputGetRequest>(), 8);
        assert_eq!(core::mem::offset_of!(InputGetRequest, surface), 4);
    }

    #[test]
    fn a_poll_reply_zeroes_the_fields_of_the_other_kind() {
        let key = InputGetReply {
            version: INPUT_VERSION,
            status: 0,
            kind: INPUT_EVENT_KEY,
            state: KeyState::Pressed.as_u16(),
            scancode: 0x1e,
            x: 0,
            y: 0,
            buttons: 0,
            surface: 2,
        };
        assert_eq!(key.kind, INPUT_EVENT_KEY);
        assert_eq!(key.scancode, 0x1e);
        assert_eq!(key.x, 0);

        let pointer = InputGetReply {
            version: INPUT_VERSION,
            status: 0,
            kind: INPUT_EVENT_POINTER,
            state: 0,
            scancode: 0,
            x: -4,
            y: 9,
            buttons: INPUT_BUTTON_PRIMARY,
            surface: 2,
        };
        assert_eq!(pointer.kind, INPUT_EVENT_POINTER);
        assert_eq!(pointer.x, -4);
        assert_eq!(pointer.scancode, 0);

        let empty = InputGetReply {
            kind: INPUT_EVENT_NONE,
            ..key
        };
        assert_eq!(empty.kind, INPUT_EVENT_NONE);
    }

    #[test]
    fn pointer_coordinates_are_signed() {
        let request = InputPointerRequest {
            version: INPUT_VERSION,
            surface: 1,
            x: -12,
            y: -7,
            buttons: INPUT_BUTTON_PRIMARY,
        };
        assert_eq!(request.x, -12);
        assert_eq!(request.y, -7);
        assert_eq!(request.buttons, INPUT_BUTTON_PRIMARY);
    }

    #[test]
    fn the_layouts_are_stable() {
        assert_eq!(size_of::<InputKeyRequest>(), 12);
        assert_eq!(size_of::<InputPointerRequest>(), 20);
        assert_eq!(size_of::<InputReply>(), 12);
        assert_eq!(size_of::<InputGetRequest>(), 8);
        assert_eq!(size_of::<InputGetReply>(), 32);

        assert_eq!(core::mem::offset_of!(InputKeyRequest, version), 0);
        assert_eq!(core::mem::offset_of!(InputKeyRequest, surface), 4);
        assert_eq!(core::mem::offset_of!(InputKeyRequest, scancode), 8);
        assert_eq!(core::mem::offset_of!(InputKeyRequest, state), 10);

        assert_eq!(core::mem::offset_of!(InputPointerRequest, version), 0);
        assert_eq!(core::mem::offset_of!(InputPointerRequest, surface), 4);
        assert_eq!(core::mem::offset_of!(InputPointerRequest, x), 8);
        assert_eq!(core::mem::offset_of!(InputPointerRequest, y), 12);
        assert_eq!(core::mem::offset_of!(InputPointerRequest, buttons), 16);

        assert_eq!(core::mem::offset_of!(InputReply, version), 0);
        assert_eq!(core::mem::offset_of!(InputReply, status), 4);
        assert_eq!(core::mem::offset_of!(InputReply, reserved), 8);
    }
}
