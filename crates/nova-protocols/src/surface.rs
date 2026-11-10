//! The surface and compositor protocol.
//!
//! # The split the protocol makes
//!
//! A client creates a surface, attaches a buffer it owns, reports damaged
//! rectangles, and commits a serial. The compositor copies the buffer into the
//! display and reports `frame_done` when the frame has been presented. The
//! client never names the framebuffer and never decides where the window is
//! shown. That is the §15.3 rule: a client may say *what changed*, not *what is
//! on screen*.
//!
//! # Why the buffer is a capability, not an address
//!
//! `Surface.attach` carries a wire handle for a `MemoryObject`, not a pointer.
//! The kernel resolves the handle in the caller's capability table and checks
//! the buffer's declared size, stride, format, and alignment before accepting
//! it. A raw address would let a client attach memory it does not own, which is
//! exactly the `invalid_surface_buffer_denied` case the milestone requires.
//!
//! # Damage rectangles and the inline payload
//!
//! The damage request header is fixed; the rectangles follow in the message's
//! inline payload. `MAX_INLINE_PAYLOAD` is 64 bytes, so a single damage message
//! carries at most three rectangles. A client with more damage splits it into
//! several requests; the protocol's `count` field is what tells the service how
//! many rectangles actually accompany the header.
//!
//! # Layout is a contract
//!
//! Every fixed struct is `#[repr(C)]` with compile-time size and offset
//! assertions. The variable payload starts exactly at the fixed header's end.

use crate::ProtocolVersion;

/// The surface protocol version this implementation speaks.
pub const SURFACE_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// The only pixel format M6 accepts.
pub const PIXEL_FORMAT_XRGB8888: u32 = 1;

/// Opcodes from §15.3's surface protocol draft.
pub const SURFACE_OP_CREATE: u64 = 1;
pub const SURFACE_OP_ATTACH: u64 = 2;
pub const SURFACE_OP_DAMAGE: u64 = 3;
pub const SURFACE_OP_COMMIT: u64 = 4;
pub const SURFACE_OP_DESTROY: u64 = 5;
pub const COMPOSITOR_OP_CONFIGURE: u64 = 6;
pub const COMPOSITOR_OP_FRAME_DONE: u64 = 7;
pub const COMPOSITOR_OP_MOVE: u64 = 8;
pub const COMPOSITOR_OP_RAISE: u64 = 9;

/// The desktop shell's window-management operations.
///
/// A panel needs to see the window list, activate a window, minimize it,
/// and read its title — EWMH's `_NET_CLIENT_LIST` / `_NET_ACTIVE_WINDOW`
/// / `_NET_WM_NAME` in miniature. These are stacking *requests*: they
/// confer no capability over the target's buffer or input, and the
/// compositor remains free to ignore them.
pub const SURFACE_OP_LIST: u64 = 10;
pub const SURFACE_OP_NAME: u64 = 11;
pub const SURFACE_OP_ACTIVATE: u64 = 12;
pub const SURFACE_OP_MINIMIZE: u64 = 13;

/// Size of a surface-create request, in bytes.
pub const SURFACE_CREATE_REQUEST_SIZE: usize = 20;

/// Size of a surface-create reply, in bytes.
pub const SURFACE_CREATE_REPLY_SIZE: usize = 16;

/// Size of a surface-attach request, in bytes.
pub const SURFACE_ATTACH_REQUEST_SIZE: usize = 24;

/// Size of a surface-commit request, in bytes.
pub const SURFACE_COMMIT_REQUEST_SIZE: usize = 16;

/// Size of a surface-destroy request, in bytes.
pub const SURFACE_DESTROY_REQUEST_SIZE: usize = 8;

/// Size of a damage request header, in bytes.
pub const SURFACE_DAMAGE_REQUEST_SIZE: usize = 12;

/// Size of one damage rectangle, in bytes.
pub const SURFACE_RECT_SIZE: usize = 16;

/// Maximum rectangles one damage message can carry.
///
/// The inline payload limit leaves room for exactly three 16-byte rectangles
/// after the 12-byte header. This is a hard protocol bound, not a policy: a
/// request that claims more has payload bytes the message does not contain.
pub const MAX_DAMAGE_RECTS: usize = 3;

/// Size of a compositor-configure request, in bytes.
pub const COMPOSITOR_CONFIGURE_REQUEST_SIZE: usize = 20;

/// Size of a compositor-frame-done request, in bytes.
pub const COMPOSITOR_FRAME_DONE_REQUEST_SIZE: usize = 16;

/// Size of a compositor-move request, in bytes.
pub const COMPOSITOR_MOVE_REQUEST_SIZE: usize = 16;

/// Size of a compositor-raise request, in bytes.
pub const COMPOSITOR_RAISE_REQUEST_SIZE: usize = 8;

/// Size of a window-list request (`version`, `offset`), in bytes.
pub const SURFACE_LIST_REQUEST_SIZE: usize = 8;

/// Size of a window-list reply, in bytes.
///
/// The inline payload is 64 bytes: a 16-byte header (`version`,
/// `status`, `count`, `total`) plus four 12-byte entries (`id`, `z`,
/// `flags`). A longer list pages through repeated requests, the same
/// way the file service pages a directory.
pub const SURFACE_LIST_REPLY_SIZE: usize = 64;

/// How many window entries one list reply carries.
pub const WINDOW_LIST_PAGE: usize = 4;

/// Size of a window-name request (`version`, `surface`), in bytes.
pub const SURFACE_NAME_REQUEST_SIZE: usize = 8;

/// Size of a window-name reply, in bytes.
///
/// Header (`version`, `status`, `length`) plus up to 40 name bytes.
pub const SURFACE_NAME_REPLY_SIZE: usize = 52;

/// The longest window name a reply carries.
pub const WINDOW_NAME_MAX: usize = 40;

/// Size of a window-activate request (`version`, `surface`), in bytes.
pub const SURFACE_ACTIVATE_REQUEST_SIZE: usize = 8;

/// Size of a window-minimize request (`version`, `surface`, `flag`),
/// in bytes.
pub const SURFACE_MINIMIZE_REQUEST_SIZE: usize = 12;

/// Flag bit: the surface has keyboard focus.
pub const WINDOW_FLAG_FOCUSED: u32 = 0x1;
/// Flag bit: the surface is minimized.
pub const WINDOW_FLAG_MINIMIZED: u32 = 0x2;

/// Size of a reply that carries only a status, in bytes.
pub const SURFACE_REPLY_SIZE: usize = 12;

/// Size of a damage reply, in bytes.
pub const SURFACE_DAMAGE_REPLY_SIZE: usize = 12;

/// One scale unit; a configure request's `scale` is a percentage.
pub const COMPOSITOR_SCALE_ONE: u16 = 100;

/// The pixel format M6 accepts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PixelFormat {
    /// 32-bit XRGB8888: eight bits each of red, green, and blue, with the
    /// fourth byte reserved.
    Xrgb8888 = PIXEL_FORMAT_XRGB8888,
}

impl PixelFormat {
    /// The wire value.
    pub const fn as_u32(self) -> u32 {
        match self {
            PixelFormat::Xrgb8888 => PIXEL_FORMAT_XRGB8888,
        }
    }

    /// Recover a format from its wire value.
    ///
    /// Returns `None` for any format this kernel does not draw. A reply that
    /// interpreted an unknown format as XRGB8888 would render garbage.
    pub const fn from_u32(value: u32) -> Option<PixelFormat> {
        match value {
            PIXEL_FORMAT_XRGB8888 => Some(PixelFormat::Xrgb8888),
            _ => None,
        }
    }

    /// Bytes per pixel.
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Xrgb8888 => 4,
        }
    }
}

/// The opcode of a surface or compositor operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum SurfaceOpcode {
    Create,
    Attach,
    Damage,
    Commit,
    Destroy,
    Configure,
    FrameDone,
    Move,
    Raise,
    List,
    Name,
    Activate,
    Minimize,
}

impl SurfaceOpcode {
    /// The wire value.
    pub const fn as_u64(self) -> u64 {
        match self {
            SurfaceOpcode::Create => SURFACE_OP_CREATE,
            SurfaceOpcode::Attach => SURFACE_OP_ATTACH,
            SurfaceOpcode::Damage => SURFACE_OP_DAMAGE,
            SurfaceOpcode::Commit => SURFACE_OP_COMMIT,
            SurfaceOpcode::Destroy => SURFACE_OP_DESTROY,
            SurfaceOpcode::Configure => COMPOSITOR_OP_CONFIGURE,
            SurfaceOpcode::FrameDone => COMPOSITOR_OP_FRAME_DONE,
            SurfaceOpcode::Move => COMPOSITOR_OP_MOVE,
            SurfaceOpcode::Raise => COMPOSITOR_OP_RAISE,
            SurfaceOpcode::List => SURFACE_OP_LIST,
            SurfaceOpcode::Name => SURFACE_OP_NAME,
            SurfaceOpcode::Activate => SURFACE_OP_ACTIVATE,
            SurfaceOpcode::Minimize => SURFACE_OP_MINIMIZE,
        }
    }

    /// Recover an opcode from its wire value.
    pub const fn from_u64(value: u64) -> Option<SurfaceOpcode> {
        match value {
            SURFACE_OP_CREATE => Some(SurfaceOpcode::Create),
            SURFACE_OP_ATTACH => Some(SurfaceOpcode::Attach),
            SURFACE_OP_DAMAGE => Some(SurfaceOpcode::Damage),
            SURFACE_OP_COMMIT => Some(SurfaceOpcode::Commit),
            SURFACE_OP_DESTROY => Some(SurfaceOpcode::Destroy),
            COMPOSITOR_OP_CONFIGURE => Some(SurfaceOpcode::Configure),
            COMPOSITOR_OP_FRAME_DONE => Some(SurfaceOpcode::FrameDone),
            COMPOSITOR_OP_MOVE => Some(SurfaceOpcode::Move),
            COMPOSITOR_OP_RAISE => Some(SurfaceOpcode::Raise),
            SURFACE_OP_LIST => Some(SurfaceOpcode::List),
            SURFACE_OP_NAME => Some(SurfaceOpcode::Name),
            SURFACE_OP_ACTIVATE => Some(SurfaceOpcode::Activate),
            SURFACE_OP_MINIMIZE => Some(SurfaceOpcode::Minimize),
            _ => None,
        }
    }
}

/// A request to create a surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceCreateRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The pixel format. Only XRGB8888 is accepted.
    pub format: PixelFormat,
    /// Reserved flags. Must be zero.
    pub flags: u32,
}

/// A reply to a surface-create request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceCreateReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// The surface identifier the service assigned.
    pub surface: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// A request to attach a client-owned buffer to a surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceAttachRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface to attach the buffer to.
    pub surface: u32,
    /// Bytes between the first pixels of consecutive rows.
    pub stride: u32,
    /// The pixel format of the buffer.
    pub format: PixelFormat,
    /// The wire handle of the buffer's `MemoryObject`.
    pub buffer_handle: u64,
}

/// A request reporting damaged rectangles.
///
/// The rectangles follow this header in the payload, `count` of them, each
/// [`SURFACE_RECT_SIZE`] bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceDamageRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface whose content changed.
    pub surface: u32,
    /// How many rectangles follow the header.
    pub count: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// One damage rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceRect {
    /// Left coordinate.
    pub x: i32,
    /// Top coordinate.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl SurfaceRect {
    /// One past the right edge, as a 64-bit value.
    ///
    /// The checked form is deliberate: coordinates and widths are wire words,
    /// and adding them in 32 bits can overflow before any clip check sees the
    /// result.
    pub fn end_x(&self) -> Option<i64> {
        (self.x as i64).checked_add(self.width as i64)
    }

    /// One past the bottom edge, as a 64-bit value.
    pub fn end_y(&self) -> Option<i64> {
        (self.y as i64).checked_add(self.height as i64)
    }

    /// Whether the rectangle has a positive area.
    pub const fn is_non_empty(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// A request to commit a surface's content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceCommitRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface being committed.
    pub surface: u32,
    /// The client's serial for this commit.
    pub serial: u64,
}

/// A request to destroy a surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceDestroyRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface to destroy.
    pub surface: u32,
}

/// A request to reconfigure a surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CompositorConfigureRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface being reconfigured.
    pub surface: u32,
    /// New width in pixels.
    pub width: u32,
    /// New height in pixels.
    pub height: u32,
    /// Scale as a percentage; [`COMPOSITOR_SCALE_ONE`] means unchanged.
    pub scale: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// A request reporting that a frame has been presented.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CompositorFrameDoneRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface whose frame was presented.
    pub surface: u32,
    /// The serial the client committed.
    pub serial: u64,
}

/// A request to move a surface to a new position.
///
/// Positions are signed because a window manager may place a window past an
/// edge; the compositor clips when it draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CompositorMoveRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface being moved.
    pub surface: u32,
    /// New left coordinate.
    pub x: i32,
    /// New top coordinate.
    pub y: i32,
}

/// A request to raise a surface in the z-order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct CompositorRaiseRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// The surface to raise.
    pub surface: u32,
}

/// A reply that carries only a status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// A reply to a damage request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SurfaceDamageReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// How many rectangles were accepted.
    pub accepted: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

const _: () = assert!(size_of::<SurfaceCreateRequest>() == SURFACE_CREATE_REQUEST_SIZE);
const _: () = assert!(size_of::<SurfaceCreateReply>() == SURFACE_CREATE_REPLY_SIZE);
const _: () = assert!(size_of::<SurfaceAttachRequest>() == SURFACE_ATTACH_REQUEST_SIZE);
const _: () = assert!(size_of::<SurfaceDamageRequest>() == SURFACE_DAMAGE_REQUEST_SIZE);
const _: () = assert!(size_of::<SurfaceRect>() == SURFACE_RECT_SIZE);
const _: () = assert!(size_of::<SurfaceCommitRequest>() == SURFACE_COMMIT_REQUEST_SIZE);
const _: () = assert!(size_of::<SurfaceDestroyRequest>() == SURFACE_DESTROY_REQUEST_SIZE);
const _: () = assert!(size_of::<CompositorConfigureRequest>() == COMPOSITOR_CONFIGURE_REQUEST_SIZE);
const _: () =
    assert!(size_of::<CompositorFrameDoneRequest>() == COMPOSITOR_FRAME_DONE_REQUEST_SIZE);
const _: () = assert!(size_of::<CompositorMoveRequest>() == COMPOSITOR_MOVE_REQUEST_SIZE);
const _: () = assert!(size_of::<CompositorRaiseRequest>() == COMPOSITOR_RAISE_REQUEST_SIZE);
const _: () = assert!(size_of::<SurfaceReply>() == SURFACE_REPLY_SIZE);
const _: () = assert!(size_of::<SurfaceDamageReply>() == SURFACE_DAMAGE_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(SurfaceCreateRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateRequest, width) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateRequest, height) == 8);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateRequest, format) == 12);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateRequest, flags) == 16);

const _: () = assert!(core::mem::offset_of!(SurfaceCreateReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateReply, surface) == 8);
const _: () = assert!(core::mem::offset_of!(SurfaceCreateReply, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(SurfaceAttachRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceAttachRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceAttachRequest, stride) == 8);
const _: () = assert!(core::mem::offset_of!(SurfaceAttachRequest, format) == 12);
const _: () = assert!(core::mem::offset_of!(SurfaceAttachRequest, buffer_handle) == 16);

const _: () = assert!(core::mem::offset_of!(SurfaceDamageRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceDamageRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceDamageRequest, count) == 8);
const _: () = assert!(core::mem::offset_of!(SurfaceDamageRequest, reserved) == 10);

const _: () = assert!(core::mem::offset_of!(SurfaceRect, x) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceRect, y) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceRect, width) == 8);
const _: () = assert!(core::mem::offset_of!(SurfaceRect, height) == 12);

const _: () = assert!(core::mem::offset_of!(SurfaceCommitRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceCommitRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceCommitRequest, serial) == 8);

const _: () = assert!(core::mem::offset_of!(SurfaceDestroyRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceDestroyRequest, surface) == 4);

const _: () = assert!(core::mem::offset_of!(CompositorConfigureRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(CompositorConfigureRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(CompositorConfigureRequest, width) == 8);
const _: () = assert!(core::mem::offset_of!(CompositorConfigureRequest, height) == 12);
const _: () = assert!(core::mem::offset_of!(CompositorConfigureRequest, scale) == 16);
const _: () = assert!(core::mem::offset_of!(CompositorConfigureRequest, reserved) == 18);

const _: () = assert!(core::mem::offset_of!(CompositorFrameDoneRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(CompositorFrameDoneRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(CompositorFrameDoneRequest, serial) == 8);

const _: () = assert!(core::mem::offset_of!(CompositorMoveRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(CompositorMoveRequest, surface) == 4);
const _: () = assert!(core::mem::offset_of!(CompositorMoveRequest, x) == 8);
const _: () = assert!(core::mem::offset_of!(CompositorMoveRequest, y) == 12);

const _: () = assert!(core::mem::offset_of!(CompositorRaiseRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(CompositorRaiseRequest, surface) == 4);

const _: () = assert!(core::mem::offset_of!(SurfaceReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceReply, reserved) == 8);

const _: () = assert!(core::mem::offset_of!(SurfaceDamageReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(SurfaceDamageReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(SurfaceDamageReply, accepted) == 8);
const _: () = assert!(core::mem::offset_of!(SurfaceDamageReply, reserved) == 10);

// The damage header plus the maximum number of rectangles must fit the ABI's
// single inline payload; otherwise a full damage message would not travel.
const _: () = assert!(
    SURFACE_DAMAGE_REQUEST_SIZE + MAX_DAMAGE_RECTS * SURFACE_RECT_SIZE
        <= nova_abi::ipc::MAX_INLINE_PAYLOAD
);

/// Bytes a surface buffer must occupy for `width`, `height`, and `stride`.
///
/// Returns `None` when the arithmetic overflows or the stride is too small for
/// one row of pixels. A caller that gets `None` has a buffer the compositor
/// must refuse, not one it may try to read.
pub fn required_buffer_bytes(width: u32, height: u32, stride: u32) -> Option<u64> {
    let row = (width as u64).checked_mul(4)?;
    if (stride as u64) < row {
        return None;
    }
    (stride as u64).checked_mul(height as u64)
}

/// Whether a stride is at least one row of `width` XRGB8888 pixels.
pub fn stride_is_sufficient(width: u32, stride: u32) -> bool {
    match (width as u64).checked_mul(4) {
        Some(row) => (stride as u64) >= row,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMPOSITOR_SCALE_ONE, CompositorConfigureRequest, MAX_DAMAGE_RECTS, PIXEL_FORMAT_XRGB8888,
        PixelFormat, SURFACE_OP_ATTACH, SURFACE_OP_COMMIT, SURFACE_OP_CREATE, SURFACE_OP_DAMAGE,
        SURFACE_OP_DESTROY, SURFACE_VERSION, SurfaceAttachRequest, SurfaceCommitRequest,
        SurfaceCreateReply, SurfaceCreateRequest, SurfaceDamageRequest, SurfaceDestroyRequest,
        SurfaceRect, required_buffer_bytes, stride_is_sufficient,
    };
    use crate::ProtocolVersion;

    #[test]
    fn the_version_is_checked_before_anything_else() {
        assert_eq!(SURFACE_VERSION, ProtocolVersion::new(1, 0));
        assert!(crate::negotiate(SURFACE_VERSION, SURFACE_VERSION).is_ok());
        assert!(crate::negotiate(SURFACE_VERSION, ProtocolVersion::new(2, 0)).is_err());
    }

    #[test]
    fn only_xrgb8888_is_accepted() {
        assert_eq!(PixelFormat::from_u32(1), Some(PixelFormat::Xrgb8888));
        assert_eq!(PixelFormat::Xrgb8888.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::from_u32(2), None);
        assert_eq!(PixelFormat::from_u32(0), None);
        assert_eq!(PIXEL_FORMAT_XRGB8888, 1);
    }

    #[test]
    fn the_opcodes_round_trip_and_reject_the_unknown() {
        let opcodes = [
            SURFACE_OP_CREATE,
            SURFACE_OP_ATTACH,
            SURFACE_OP_DAMAGE,
            SURFACE_OP_COMMIT,
            SURFACE_OP_DESTROY,
        ];
        for opcode in opcodes {
            assert_eq!(
                super::SurfaceOpcode::from_u64(opcode).unwrap().as_u64(),
                opcode
            );
        }
        assert_eq!(super::SurfaceOpcode::from_u64(9999), None);
    }

    #[test]
    fn a_damage_message_carries_at_most_three_rectangles() {
        assert_eq!(MAX_DAMAGE_RECTS, 3);
    }

    #[test]
    fn rectangle_edges_are_checked_not_wrapped() {
        let rect = SurfaceRect {
            x: i32::MAX,
            y: 0,
            width: 2,
            height: 1,
        };
        assert_eq!(rect.end_x(), Some(2_147_483_649));
        assert_eq!(rect.end_y(), Some(1));
        assert!(rect.end_x().unwrap() > i32::MAX as i64);

        let valid = SurfaceRect {
            x: -4,
            y: -2,
            width: 8,
            height: 4,
        };
        assert_eq!(valid.end_x(), Some(4));
        assert_eq!(valid.end_y(), Some(2));
    }

    #[test]
    fn a_stride_must_cover_a_whole_row() {
        assert!(stride_is_sufficient(10, 40));
        assert!(!stride_is_sufficient(10, 39));
        assert_eq!(required_buffer_bytes(10, 2, 40), Some(80));
        assert_eq!(required_buffer_bytes(10, 2, 39), None);
        assert_eq!(required_buffer_bytes(u32::MAX, 2, u32::MAX), None);
    }

    #[test]
    fn the_layouts_are_stable() {
        assert_eq!(size_of::<SurfaceCreateRequest>(), 20);
        assert_eq!(size_of::<SurfaceCreateReply>(), 16);
        assert_eq!(size_of::<SurfaceAttachRequest>(), 24);
        assert_eq!(size_of::<SurfaceDamageRequest>(), 12);
        assert_eq!(size_of::<SurfaceRect>(), 16);
        assert_eq!(size_of::<SurfaceCommitRequest>(), 16);
        assert_eq!(size_of::<SurfaceDestroyRequest>(), 8);
        assert_eq!(size_of::<CompositorConfigureRequest>(), 20);

        assert_eq!(core::mem::offset_of!(SurfaceCreateRequest, version), 0);
        assert_eq!(core::mem::offset_of!(SurfaceCreateRequest, width), 4);
        assert_eq!(core::mem::offset_of!(SurfaceCreateRequest, height), 8);
        assert_eq!(core::mem::offset_of!(SurfaceCreateRequest, format), 12);
        assert_eq!(core::mem::offset_of!(SurfaceCreateRequest, flags), 16);

        assert_eq!(
            core::mem::offset_of!(SurfaceAttachRequest, buffer_handle),
            16
        );
        assert_eq!(core::mem::offset_of!(SurfaceDamageRequest, count), 8);
        assert_eq!(core::mem::offset_of!(SurfaceRect, width), 8);
        assert_eq!(core::mem::offset_of!(SurfaceCommitRequest, serial), 8);
        assert_eq!(core::mem::offset_of!(CompositorConfigureRequest, scale), 16);
    }

    #[test]
    fn the_scale_unit_is_a_percentage() {
        assert_eq!(COMPOSITOR_SCALE_ONE, 100);
    }
}
