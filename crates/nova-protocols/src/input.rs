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
//! Both request structs are `#[repr(C)]` with compile-time size and offset
//! assertions. The service and its clients are separate binaries.

use crate::ProtocolVersion;

/// The input protocol version this implementation speaks.
pub const INPUT_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// Opcode for a key event.
pub const INPUT_OP_KEY: u64 = 1;

/// Opcode for a pointer event.
pub const INPUT_OP_POINTER: u64 = 2;

/// Size of a key-event request, in bytes.
pub const INPUT_KEY_REQUEST_SIZE: usize = 12;

/// Size of a pointer-event request, in bytes.
pub const INPUT_POINTER_REQUEST_SIZE: usize = 20;

/// Size of an input reply, in bytes.
pub const INPUT_REPLY_SIZE: usize = 12;

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
}

impl InputOpcode {
    /// The wire value.
    pub const fn as_u64(self) -> u64 {
        match self {
            InputOpcode::Key => INPUT_OP_KEY,
            InputOpcode::Pointer => INPUT_OP_POINTER,
        }
    }

    /// Recover an opcode from its wire value.
    pub const fn from_u64(value: u64) -> Option<InputOpcode> {
        match value {
            INPUT_OP_KEY => Some(InputOpcode::Key),
            INPUT_OP_POINTER => Some(InputOpcode::Pointer),
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

const _: () = assert!(size_of::<InputKeyRequest>() == INPUT_KEY_REQUEST_SIZE);
const _: () = assert!(size_of::<InputPointerRequest>() == INPUT_POINTER_REQUEST_SIZE);
const _: () = assert!(size_of::<InputReply>() == INPUT_REPLY_SIZE);

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

#[cfg(test)]
mod tests {
    use super::{
        INPUT_BUTTON_PRIMARY, INPUT_OP_KEY, INPUT_OP_POINTER, INPUT_VERSION, InputKeyRequest,
        InputOpcode, InputPointerRequest, InputReply, KeyState,
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
        assert_eq!(InputOpcode::from_u64(INPUT_OP_KEY), Some(InputOpcode::Key));
        assert_eq!(
            InputOpcode::from_u64(INPUT_OP_POINTER),
            Some(InputOpcode::Pointer)
        );
        assert_eq!(InputOpcode::from_u64(9999), None);
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
