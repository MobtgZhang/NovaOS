//! User-space wrappers for the M6 display, compositor, surface, and input
//! services, plus the memory-object plumbing a surface buffer needs.
//!
//! # Why these wrappers exist
//!
//! A GUI application never touches the framebuffer. It asks the display
//! service for the mode, creates a surface, allocates a `MemoryObject` for the
//! pixels, maps that object into its own address space, attaches the buffer by
//! capability handle, reports damage, and commits. The compositor owns the
//! placement and copies the buffer into the display.
//!
//! # The buffer is a capability, not an address
//!
//! `MAP` gives the application a user-space pointer to the pages. `attach`
//! carries the *handle* of the object, not that pointer, so the compositor can
//! resolve the handle in the caller's own capability table. A program cannot
//! attach memory it does not hold.
//!
//! # The client does not move focus
//!
//! The input wrappers name a surface, but the input service routes a key only
//! to the compositor's focused surface and routes a pointer through the
//! compositor's own hit test. That is why `Input.key` can be refused even when
//! the caller names a live surface.

use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_abi::layout::{PAGE_SIZE, USER_HEAP_BASE};
use nova_abi::{ObjectType, nr};
use nova_protocols::display::{
    DISPLAY_OP_QUERY, DISPLAY_QUERY_REPLY_SIZE, DISPLAY_VERSION, DisplayQueryReply,
    DisplayQueryRequest,
};
use nova_protocols::input::{
    INPUT_OP_KEY, INPUT_OP_POINTER, INPUT_REPLY_SIZE, INPUT_VERSION, InputKeyRequest,
    InputPointerRequest, KeyState,
};
use nova_protocols::surface::{
    COMPOSITOR_OP_CONFIGURE, COMPOSITOR_OP_FRAME_DONE, COMPOSITOR_OP_MOVE, COMPOSITOR_OP_RAISE,
    CompositorConfigureRequest, CompositorFrameDoneRequest, CompositorMoveRequest,
    CompositorRaiseRequest, PIXEL_FORMAT_XRGB8888, PixelFormat, SURFACE_CREATE_REPLY_SIZE,
    SURFACE_DAMAGE_REPLY_SIZE, SURFACE_DAMAGE_REQUEST_SIZE, SURFACE_OP_ATTACH, SURFACE_OP_COMMIT,
    SURFACE_OP_CREATE, SURFACE_OP_DAMAGE, SURFACE_OP_DESTROY, SURFACE_RECT_SIZE,
    SURFACE_REPLY_SIZE, SURFACE_VERSION, SurfaceAttachRequest, SurfaceCommitRequest,
    SurfaceCreateRequest, SurfaceDamageRequest, SurfaceDestroyRequest, SurfaceRect,
};

use crate::syscall::{self, SyscallError};

/// The service identifiers the kernel publishes.
pub const SERVICE_DISPLAY: u32 = 1;
pub const SERVICE_INPUT: u32 = 2;
pub const SERVICE_COMPOSITOR: u32 = 3;

/// The maximum inline payload the ABI carries.
pub const MAX_PAYLOAD: usize = nova_abi::MAX_INLINE_PAYLOAD;

/// The display mode a client learns from the display service.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DisplayMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes between the first pixels of consecutive rows.
    pub pitch: u32,
    /// The pixel format word.
    pub format: u32,
}

/// Turn a failed syscall into an ABI error code.
fn error_code(error: SyscallError) -> ErrorCode {
    match error {
        SyscallError::Refused(code) => code,
        SyscallError::ShortWrite => ErrorCode::Invalid,
    }
}

/// Ask one in-kernel service for a reply.
///
/// The request and reply are inline payloads bounded by `MAX_PAYLOAD`; pixel
/// data never travels through this path, only through a mapped `MemoryObject`.
pub fn service_request(
    service: u32,
    opcode: u64,
    request: &[u8],
    reply: &mut [u8],
) -> Result<usize, ErrorCode> {
    if request.len() > MAX_PAYLOAD || reply.len() > MAX_PAYLOAD {
        return Err(ErrorCode::Invalid);
    }

    let length = syscall::call(
        nr::SERVICE_REQUEST.raw(),
        [
            service as u64,
            opcode,
            request.as_ptr() as u64,
            request.len() as u64,
            reply.as_mut_ptr() as u64,
            reply.len() as u64,
        ],
    )
    .map_err(error_code)?;

    Ok(length as usize)
}

/// The bytes of a `#[repr(C)]` protocol value.
fn protocol_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: every protocol struct is `#[repr(C)]` and its byte size is the
    // asserted wire size. The slice borrows the caller's local value only for
    // the duration of the call that copies it.
    unsafe {
        core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>())
    }
}

/// Read a little-endian `u32` from a reply.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let part = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([part[0], part[1], part[2], part[3]]))
}

/// Read a little-endian `i32` from a reply.
fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let value = read_u32(bytes, offset)?;
    Some(value as i32)
}

/// Whether a service reply carries success.
fn reply_ok(bytes: &[u8]) -> Result<(), ErrorCode> {
    let status = read_i32(bytes, 4).ok_or(ErrorCode::Invalid)?;
    if status == 0 {
        return Ok(());
    }
    match ErrorCode::from_result(-(status as i64)) {
        Some(code) => Err(code),
        None => Err(ErrorCode::Unsupported),
    }
}

/// Query the display mode.
pub fn display_query() -> Result<DisplayMode, ErrorCode> {
    let request = DisplayQueryRequest {
        version: DISPLAY_VERSION,
        flags: 0,
        reserved: 0,
    };
    let mut reply = [0u8; DISPLAY_QUERY_REPLY_SIZE];
    let length = service_request(
        SERVICE_DISPLAY,
        DISPLAY_OP_QUERY,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != DISPLAY_QUERY_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)?;

    let reply_value = DisplayQueryReply {
        version: nova_protocols::ProtocolVersion::new(0, 0),
        status: 0,
        width: read_u32(&reply, 8).ok_or(ErrorCode::Invalid)?,
        height: read_u32(&reply, 12).ok_or(ErrorCode::Invalid)?,
        pitch: read_u32(&reply, 16).ok_or(ErrorCode::Invalid)?,
        format: read_u32(&reply, 20).ok_or(ErrorCode::Invalid)?,
    };
    if reply_value.format != PIXEL_FORMAT_XRGB8888 {
        return Err(ErrorCode::Unsupported);
    }
    Ok(DisplayMode {
        width: reply_value.width,
        height: reply_value.height,
        pitch: reply_value.pitch,
        format: reply_value.format,
    })
}

/// Create a surface and return its identifier.
pub fn surface_create(width: u32, height: u32) -> Result<u32, ErrorCode> {
    let request = SurfaceCreateRequest {
        version: SURFACE_VERSION,
        width,
        height,
        format: PixelFormat::Xrgb8888,
        flags: 0,
    };
    let mut reply = [0u8; SURFACE_CREATE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        SURFACE_OP_CREATE,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_CREATE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)?;
    read_u32(&reply, 8).ok_or(ErrorCode::Invalid)
}

/// Attach a mapped `MemoryObject` buffer to a surface.
pub fn surface_attach(surface: u32, buffer: CapHandle, stride: u32) -> Result<(), ErrorCode> {
    let request = SurfaceAttachRequest {
        version: SURFACE_VERSION,
        surface,
        stride,
        format: PixelFormat::Xrgb8888,
        buffer_handle: buffer.raw(),
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        SURFACE_OP_ATTACH,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Report one damaged rectangle.
pub fn surface_damage(surface: u32, rect: SurfaceRect) -> Result<(), ErrorCode> {
    let header = SurfaceDamageRequest {
        version: SURFACE_VERSION,
        surface,
        count: 1,
        reserved: 0,
    };
    let mut payload = [0u8; SURFACE_DAMAGE_REQUEST_SIZE + SURFACE_RECT_SIZE];
    payload[..SURFACE_DAMAGE_REQUEST_SIZE].copy_from_slice(protocol_bytes(&header));
    payload[SURFACE_DAMAGE_REQUEST_SIZE..SURFACE_DAMAGE_REQUEST_SIZE + 4]
        .copy_from_slice(&rect.x.to_le_bytes());
    payload[SURFACE_DAMAGE_REQUEST_SIZE + 4..SURFACE_DAMAGE_REQUEST_SIZE + 8]
        .copy_from_slice(&rect.y.to_le_bytes());
    payload[SURFACE_DAMAGE_REQUEST_SIZE + 8..SURFACE_DAMAGE_REQUEST_SIZE + 12]
        .copy_from_slice(&rect.width.to_le_bytes());
    payload[SURFACE_DAMAGE_REQUEST_SIZE + 12..SURFACE_DAMAGE_REQUEST_SIZE + 16]
        .copy_from_slice(&rect.height.to_le_bytes());

    let mut reply = [0u8; SURFACE_DAMAGE_REPLY_SIZE];
    let length = service_request(SERVICE_COMPOSITOR, SURFACE_OP_DAMAGE, &payload, &mut reply)?;
    if length != SURFACE_DAMAGE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Commit a surface's content with a client serial.
pub fn surface_commit(surface: u32, serial: u64) -> Result<(), ErrorCode> {
    let request = SurfaceCommitRequest {
        version: SURFACE_VERSION,
        surface,
        serial,
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        SURFACE_OP_COMMIT,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Destroy a surface. The compositor releases the attached buffer.
pub fn surface_destroy(surface: u32) -> Result<(), ErrorCode> {
    let request = SurfaceDestroyRequest {
        version: SURFACE_VERSION,
        surface,
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        SURFACE_OP_DESTROY,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Move a surface.
pub fn compositor_move(surface: u32, x: i32, y: i32) -> Result<(), ErrorCode> {
    let request = CompositorMoveRequest {
        version: SURFACE_VERSION,
        surface,
        x,
        y,
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        COMPOSITOR_OP_MOVE,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Raise a surface in the z-order.
pub fn compositor_raise(surface: u32) -> Result<(), ErrorCode> {
    let request = CompositorRaiseRequest {
        version: SURFACE_VERSION,
        surface,
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        COMPOSITOR_OP_RAISE,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Reconfigure a surface's size and scale.
pub fn compositor_configure(
    surface: u32,
    width: u32,
    height: u32,
    scale: u16,
) -> Result<(), ErrorCode> {
    let request = CompositorConfigureRequest {
        version: SURFACE_VERSION,
        surface,
        width,
        height,
        scale,
        reserved: 0,
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        COMPOSITOR_OP_CONFIGURE,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Tell the compositor that a committed frame was presented.
pub fn compositor_frame_done(surface: u32, serial: u64) -> Result<(), ErrorCode> {
    let request = CompositorFrameDoneRequest {
        version: SURFACE_VERSION,
        surface,
        serial,
    };
    let mut reply = [0u8; SURFACE_REPLY_SIZE];
    let length = service_request(
        SERVICE_COMPOSITOR,
        COMPOSITOR_OP_FRAME_DONE,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != SURFACE_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Send a key event for a surface. The service refuses it unless the
/// compositor has focused that surface.
pub fn input_key(surface: u32, scancode: u16, state: KeyState) -> Result<(), ErrorCode> {
    let request = InputKeyRequest {
        version: INPUT_VERSION,
        surface,
        scancode,
        state,
    };
    let mut reply = [0u8; INPUT_REPLY_SIZE];
    let length = service_request(
        SERVICE_INPUT,
        INPUT_OP_KEY,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != INPUT_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Send a pointer event. Focus is decided by the compositor's hit test.
pub fn input_pointer(surface: u32, x: i32, y: i32, buttons: u32) -> Result<(), ErrorCode> {
    let request = InputPointerRequest {
        version: INPUT_VERSION,
        surface,
        x,
        y,
        buttons,
    };
    let mut reply = [0u8; INPUT_REPLY_SIZE];
    let length = service_request(
        SERVICE_INPUT,
        INPUT_OP_POINTER,
        protocol_bytes(&request),
        &mut reply,
    )?;
    if length != INPUT_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    reply_ok(&reply)
}

/// Create a shareable `MemoryObject` and return its capability.
pub fn memory_object(size: u64, rights: Rights) -> Result<CapHandle, ErrorCode> {
    let value = syscall::call(
        nr::OBJECT_CREATE.raw(),
        [
            ObjectType::MemoryObject as u64,
            rights.bits() as u64,
            size,
            0,
            0,
            0,
        ],
    )
    .map_err(error_code)?;
    Ok(CapHandle::from_raw(value as u64))
}

/// Map a `MemoryObject` into this program's reserved heap region.
pub fn map_memory(buffer: CapHandle, size: u64) -> Result<*mut u8, ErrorCode> {
    let aligned = size.checked_add(PAGE_SIZE - 1).ok_or(ErrorCode::Invalid)? & !(PAGE_SIZE - 1);
    if aligned == 0 || aligned > nova_abi::layout::USER_HEAP_SIZE {
        return Err(ErrorCode::Invalid);
    }

    let pages = syscall::call(
        nr::MAP.raw(),
        [buffer.raw(), USER_HEAP_BASE, aligned, 0, 0, 0],
    )
    .map_err(error_code)?;
    if pages == 0 {
        return Err(ErrorCode::Invalid);
    }
    Ok(USER_HEAP_BASE as *mut u8)
}

/// Destroy a `MemoryObject`. A surface destroy already releases the buffer,
/// so a program should call this only when it has not destroyed the surface.
pub fn destroy_object(buffer: CapHandle) -> Result<(), ErrorCode> {
    syscall::call(nr::OBJECT_DESTROY.raw(), [buffer.raw(), 0, 0, 0, 0, 0]).map_err(error_code)?;
    Ok(())
}

/// Fill a mapped XRGB8888 buffer with one colour word.
pub fn fill_word(buffer: *mut u8, pixels: usize, word: u32) {
    for index in 0..pixels {
        let offset = index * 4;
        // SAFETY: the caller mapped at least `pixels * 4` bytes of this buffer
        // into its own address space, and the write stays inside that region.
        unsafe {
            let target = buffer.add(offset) as *mut u32;
            *target = word;
        }
    }
}

/// Draw one rect of `word` pixels into a mapped buffer with a stride.
pub fn fill_rect(
    buffer: *mut u8,
    stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    word: u32,
) {
    for row in y..y + height {
        for column in x..x + width {
            let offset = row * stride + column * 4;
            // SAFETY: the surface was created with this stride and these
            // dimensions, and the mapped buffer is large enough for both.
            unsafe {
                let target = buffer.add(offset) as *mut u32;
                *target = word;
            }
        }
    }
}

/// The XRGB8888 word for a colour.
pub const fn colour(red: u8, green: u8, blue: u8) -> u32 {
    ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

/// Draw a simple window and commit it.
pub fn open_window(width: u32, height: u32) -> Result<(u32, *mut u8), ErrorCode> {
    let bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ErrorCode::Invalid)?;
    let stride = width.checked_mul(4).ok_or(ErrorCode::Invalid)?;

    let surface = surface_create(width, height)?;
    let buffer = match memory_object(
        bytes,
        Rights::MAP | Rights::CONTROL | Rights::READ | Rights::WRITE,
    ) {
        Ok(buffer) => buffer,
        Err(error) => {
            let _ = surface_destroy(surface);
            return Err(error);
        }
    };
    let pointer = match map_memory(buffer, bytes) {
        Ok(pointer) => pointer,
        Err(error) => {
            let _ = destroy_object(buffer);
            let _ = surface_destroy(surface);
            return Err(error);
        }
    };
    if let Err(error) = surface_attach(surface, buffer, stride) {
        let _ = destroy_object(buffer);
        let _ = surface_destroy(surface);
        return Err(error);
    }
    Ok((surface, pointer))
}
