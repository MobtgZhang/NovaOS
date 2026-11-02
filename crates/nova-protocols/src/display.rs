//! The display service protocol.
//!
//! # What the display server answers
//!
//! The display server owns the firmware framebuffer. A client does not draw
//! into it: it asks for the mode (geometry, pitch, and pixel format) and then
//! draws into a *surface* buffer whose pixels the compositor copies into the
//! display. That split is the Wayland-style model §15.1 describes: clients draw,
//! the compositor manages the display and focus.
//!
//! # Why the mode is data, not a pointer
//!
//! The reply carries the mode words, not the framebuffer address. A client
//! cannot write the screen through this protocol, and a framebuffer address in
//! a reply would be an authority a capability never granted. The display server
//! is the only code that touches the framebuffer.
//!
//! # Layout is a contract
//!
//! Both structs are `#[repr(C)]` with compile-time size and offset assertions.
//! The service and its clients are separate binaries; a field that moved would
//! be read as a different message, not as a compile error.

use crate::ProtocolVersion;
use crate::surface::PIXEL_FORMAT_XRGB8888;

/// The display protocol version this implementation speaks.
pub const DISPLAY_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// Opcode for a mode query.
pub const DISPLAY_OP_QUERY: u64 = 1;

/// Size of a mode-query request, in bytes.
pub const DISPLAY_QUERY_REQUEST_SIZE: usize = 12;

/// Size of a mode-query reply, in bytes.
pub const DISPLAY_QUERY_REPLY_SIZE: usize = 24;

/// The opcode of a display operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum DisplayOpcode {
    /// Ask for the current display mode.
    Query,
}

impl DisplayOpcode {
    /// The wire value.
    pub const fn as_u64(self) -> u64 {
        match self {
            DisplayOpcode::Query => DISPLAY_OP_QUERY,
        }
    }

    /// Recover an opcode from its wire value.
    ///
    /// Returns `None` for an unknown value: answering an unknown opcode with a
    /// nearby one would run the wrong operation.
    pub const fn from_u64(value: u64) -> Option<DisplayOpcode> {
        match value {
            DISPLAY_OP_QUERY => Some(DisplayOpcode::Query),
            _ => None,
        }
    }
}

/// A request for the current display mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct DisplayQueryRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// A reply carrying the current display mode.
///
/// The `status` field is the ABI error code, or zero on success. On an error
/// the geometry fields are zero, because a mode that does not exist must not
/// be reported as a mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct DisplayQueryReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Bytes between the first pixels of consecutive rows.
    pub pitch: u32,
    /// The pixel format word; M6 accepts only XRGB8888.
    pub format: u32,
}

const _: () = assert!(size_of::<DisplayQueryRequest>() == DISPLAY_QUERY_REQUEST_SIZE);
const _: () = assert!(size_of::<DisplayQueryReply>() == DISPLAY_QUERY_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(DisplayQueryRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(DisplayQueryRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(DisplayQueryRequest, reserved) == 8);

const _: () = assert!(core::mem::offset_of!(DisplayQueryReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(DisplayQueryReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(DisplayQueryReply, width) == 8);
const _: () = assert!(core::mem::offset_of!(DisplayQueryReply, height) == 12);
const _: () = assert!(core::mem::offset_of!(DisplayQueryReply, pitch) == 16);
const _: () = assert!(core::mem::offset_of!(DisplayQueryReply, format) == 20);

impl DisplayQueryRequest {
    /// Whether the request carries no reserved flags.
    pub const fn is_valid(&self) -> bool {
        self.flags == 0 && self.reserved == 0
    }
}

impl DisplayQueryReply {
    /// A successful reply for a mode.
    pub const fn ok(width: u32, height: u32, pitch: u32) -> Self {
        DisplayQueryReply {
            version: DISPLAY_VERSION,
            status: 0,
            width,
            height,
            pitch,
            format: PIXEL_FORMAT_XRGB8888,
        }
    }

    /// A reply carrying an error.
    pub const fn error(code: nova_abi::error::ErrorCode) -> Self {
        DisplayQueryReply {
            version: DISPLAY_VERSION,
            status: code.code(),
            width: 0,
            height: 0,
            pitch: 0,
            format: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DISPLAY_OP_QUERY, DISPLAY_QUERY_REPLY_SIZE, DISPLAY_QUERY_REQUEST_SIZE, DISPLAY_VERSION,
        DisplayOpcode, DisplayQueryReply, DisplayQueryRequest,
    };
    use crate::ProtocolVersion;
    use crate::surface::PIXEL_FORMAT_XRGB8888;

    #[test]
    fn the_version_is_checked_before_anything_else() {
        assert_eq!(DISPLAY_VERSION, ProtocolVersion::new(1, 0));
        assert!(crate::negotiate(DISPLAY_VERSION, DISPLAY_VERSION).is_ok());
        assert!(crate::negotiate(DISPLAY_VERSION, ProtocolVersion::new(2, 0)).is_err());
        assert!(crate::negotiate(DISPLAY_VERSION, ProtocolVersion::new(1, 1)).is_err());
    }

    #[test]
    fn the_layouts_are_stable() {
        assert_eq!(size_of::<DisplayQueryRequest>(), DISPLAY_QUERY_REQUEST_SIZE);
        assert_eq!(size_of::<DisplayQueryReply>(), DISPLAY_QUERY_REPLY_SIZE);

        assert_eq!(core::mem::offset_of!(DisplayQueryRequest, version), 0);
        assert_eq!(core::mem::offset_of!(DisplayQueryRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(DisplayQueryRequest, reserved), 8);

        assert_eq!(core::mem::offset_of!(DisplayQueryReply, version), 0);
        assert_eq!(core::mem::offset_of!(DisplayQueryReply, status), 4);
        assert_eq!(core::mem::offset_of!(DisplayQueryReply, width), 8);
        assert_eq!(core::mem::offset_of!(DisplayQueryReply, height), 12);
        assert_eq!(core::mem::offset_of!(DisplayQueryReply, pitch), 16);
        assert_eq!(core::mem::offset_of!(DisplayQueryReply, format), 20);
    }

    #[test]
    fn the_opcode_round_trips_and_reject_the_unknown() {
        assert_eq!(DisplayOpcode::Query.as_u64(), DISPLAY_OP_QUERY);
        assert_eq!(
            DisplayOpcode::from_u64(DISPLAY_OP_QUERY),
            Some(DisplayOpcode::Query)
        );
        assert_eq!(DisplayOpcode::from_u64(9999), None);
    }

    #[test]
    fn a_mode_reply_names_the_only_supported_format() {
        let reply = DisplayQueryReply::ok(1280, 800, 5120);
        assert_eq!(reply.status, 0);
        assert_eq!(reply.format, PIXEL_FORMAT_XRGB8888);
        assert_eq!(reply.width, 1280);
        assert_eq!(reply.height, 800);
        assert_eq!(reply.pitch, 5120);
    }

    #[test]
    fn an_error_reply_does_not_claim_a_mode() {
        let reply = DisplayQueryReply::error(nova_abi::error::ErrorCode::Unsupported);
        assert_eq!(reply.status, nova_abi::error::ErrorCode::Unsupported.code());
        assert_eq!(reply.width, 0);
        assert_eq!(reply.height, 0);
        assert_eq!(reply.format, 0);
    }

    #[test]
    fn a_request_with_flags_is_not_valid() {
        let request = DisplayQueryRequest {
            version: DISPLAY_VERSION,
            flags: 1,
            reserved: 0,
        };
        assert!(!request.is_valid());
    }
}
