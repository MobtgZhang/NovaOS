//! The kernel-side compositor and surface service.
//!
//! # What the compositor owns
//!
//! A surface is client-drawn memory plus a placement the compositor controls.
//! The client creates a surface, attaches a buffer capability, reports damaged
//! rectangles, and commits a serial. The compositor copies the buffer into the
//! firmware framebuffer with the CPU, clips to the display, and draws surfaces
//! in z-order.
//!
//! # Why the buffer is checked here, not trusted
//!
//! `Surface.attach` carries a wire handle for a `MemoryObject`, not a pointer.
//! The compositor resolves that handle in the *caller's* capability table, so a
//! client cannot attach memory it does not own. The stride, format, dimensions,
//! and the region's length are all checked before a single byte is read. That
//! is the `invalid_surface_buffer_denied` case the milestone names.
//!
//! # Focus is not a client field
//!
//! A client may name a surface in an input request, but that name does not move
//! focus. The compositor picks the topmost surface under the pointer and keeps
//! one focused surface. `Input.key` therefore reaches only the focused surface.
//!
//! # Frame scheduling
//!
//! The compositor marks a surface dirty on commit and draws it when a frame is
//! presented. If nothing is dirty, `present` returns without touching the
//! display, which is the mechanism that keeps an idle GUI from busy-spinning.
//! The timer path can call `present` when a tick arrives; a client that has no
//! damage does not force a redraw.
//!
//! # Bounds
//!
//! Surfaces live in a fixed pool and damage rectangles in a fixed array. The
//! kernel has no allocator, so a full pool or a full damage history is a
//! reportable quota rather than an unbounded allocation.

use nova_abi::cap::Rights;
use nova_abi::error::ErrorCode;
use nova_cap::{CapabilityTable, ObjectRef};
use nova_ipc::Message;
use nova_protocols::negotiate;
use nova_protocols::surface::{
    COMPOSITOR_CONFIGURE_REQUEST_SIZE, COMPOSITOR_FRAME_DONE_REQUEST_SIZE,
    COMPOSITOR_MOVE_REQUEST_SIZE, COMPOSITOR_OP_CONFIGURE, COMPOSITOR_OP_FRAME_DONE,
    COMPOSITOR_OP_MOVE, COMPOSITOR_OP_RAISE, COMPOSITOR_RAISE_REQUEST_SIZE, COMPOSITOR_SCALE_ONE,
    MAX_DAMAGE_RECTS, PixelFormat, SURFACE_ATTACH_REQUEST_SIZE, SURFACE_COMMIT_REQUEST_SIZE,
    SURFACE_CREATE_REPLY_SIZE, SURFACE_CREATE_REQUEST_SIZE, SURFACE_DAMAGE_REPLY_SIZE,
    SURFACE_DAMAGE_REQUEST_SIZE, SURFACE_DESTROY_REQUEST_SIZE, SURFACE_OP_ATTACH,
    SURFACE_OP_COMMIT, SURFACE_OP_CREATE, SURFACE_OP_DAMAGE, SURFACE_OP_DESTROY,
    SURFACE_REPLY_SIZE, SURFACE_VERSION, SurfaceRect, required_buffer_bytes,
};

use crate::drivers::display::{self, DisplayError};
use crate::mm::direct_map;
use crate::services::object::ObjectKind;

/// Maximum surfaces the compositor tracks.
pub const MAX_SURFACES: usize = 16;

/// Maximum damage rectangles accumulated between frames.
///
/// The protocol carries at most three rectangles per message; this bound is the
/// compositor's history for several messages before a frame is presented.
pub const MAX_DAMAGE_ACCUM: usize = 12;

/// The background colour, in RGB bytes.
const BACKGROUND_RED: u8 = 24;
const BACKGROUND_GREEN: u8 = 24;
const BACKGROUND_BLUE: u8 = 28;

/// The first surface's default position.
const DEFAULT_X: i64 = 48;
const DEFAULT_Y: i64 = 48;
/// The cascade step for a newly created surface.
const DEFAULT_X_STEP: i64 = 40;
const DEFAULT_Y_STEP: i64 = 32;

/// A client's buffer, as the compositor knows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SurfaceBuffer {
    /// Physical address of the first byte.
    pub base: u64,
    /// Length of the region, a multiple of the page size.
    pub size: u64,
    /// The registry object the capability names.
    pub object_ref: ObjectRef,
}

/// One surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Surface {
    /// The identifier the service assigned.
    pub id: u32,
    /// The task slot that owns the surface.
    pub owner: usize,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes between the first pixels of consecutive rows in the buffer.
    pub stride: u32,
    /// Placement on the display.
    pub x: i64,
    pub y: i64,
    /// Z-order rank; larger is nearer the front.
    pub z: u32,
    /// The last serial the client committed.
    pub serial: u64,
    /// The buffer, if one was attached.
    pub buffer: Option<SurfaceBuffer>,
    /// The pixel format of the attached buffer.
    pub format: u32,
    /// Scale as a percentage.
    pub scale: u16,
    /// Damaged rectangles not yet presented.
    pub damage: [SurfaceRect; MAX_DAMAGE_ACCUM],
    /// How many damage entries are meaningful.
    pub damage_count: usize,
    /// Whether the whole surface must be redrawn (first attach, move, resize).
    pub full: bool,
    /// Whether this surface has pending content.
    pub dirty: bool,
}

impl Surface {
    /// Whether the surface has a buffer.
    pub const fn has_buffer(&self) -> bool {
        self.buffer.is_some()
    }

    /// One past the surface's right edge.
    pub fn right(&self) -> Option<i64> {
        self.x.checked_add(self.width as i64)
    }

    /// One past the surface's bottom edge.
    pub fn bottom(&self) -> Option<i64> {
        self.y.checked_add(self.height as i64)
    }
}

/// The compositor's state.
#[derive(Clone, Copy)]
pub struct CompositorService {
    surfaces: [Option<Surface>; MAX_SURFACES],
    focused: Option<u32>,
    next_id: u32,
    next_z: u32,
    frame_count: u64,
}

impl CompositorService {
    /// An empty compositor.
    pub const fn new() -> Self {
        Self {
            surfaces: [None; MAX_SURFACES],
            focused: None,
            next_id: 1,
            next_z: 1,
            frame_count: 0,
        }
    }

    /// Drop every surface and its focus.
    pub fn reset(&mut self) {
        self.surfaces = [None; MAX_SURFACES];
        self.focused = None;
        self.next_id = 1;
        self.next_z = 1;
        self.frame_count = 0;
    }

    /// The surface with `id`.
    pub fn surface(&self, id: u32) -> Option<&Surface> {
        self.surfaces
            .iter()
            .flatten()
            .find(|surface| surface.id == id)
    }

    /// The surface with `id`, mutably.
    fn surface_mut(&mut self, id: u32) -> Option<&mut Surface> {
        self.surfaces
            .iter_mut()
            .flatten()
            .find(|surface| surface.id == id)
    }

    /// How many surfaces exist.
    pub fn surface_count(&self) -> usize {
        self.surfaces.iter().filter(|entry| entry.is_some()).count()
    }

    /// The focused surface, if any.
    pub fn focused(&self) -> Option<u32> {
        self.focused
    }

    /// How many frames the compositor has presented.
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Whether any surface has pending content.
    pub fn frame_pending(&self) -> bool {
        self.surfaces.iter().flatten().any(|surface| surface.dirty)
    }

    /// Create a surface.
    pub fn create(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<u32, ErrorCode> {
        if width == 0 || height == 0 {
            return Err(ErrorCode::Invalid);
        }

        let Some(slot) = self.surfaces.iter().position(|entry| entry.is_none()) else {
            return Err(ErrorCode::Quota);
        };

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let index = id.saturating_sub(1) as i64;

        let surface = Surface {
            id,
            owner: 0,
            width,
            height,
            stride: width.checked_mul(4).unwrap_or(0),
            x: DEFAULT_X.saturating_add(index.saturating_mul(DEFAULT_X_STEP)),
            y: DEFAULT_Y.saturating_add(index.saturating_mul(DEFAULT_Y_STEP)),
            z: self.next_z,
            serial: 0,
            buffer: None,
            format: format.as_u32(),
            scale: COMPOSITOR_SCALE_ONE,
            damage: [SurfaceRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }; MAX_DAMAGE_ACCUM],
            damage_count: 0,
            full: false,
            dirty: false,
        };

        self.next_z = self.next_z.saturating_add(1);
        self.surfaces[slot] = Some(surface);

        if self.focused.is_none() {
            self.focused = Some(id);
        }
        Ok(id)
    }

    /// Set a surface's owner.
    pub fn set_owner(&mut self, id: u32, owner: usize) -> Result<(), ErrorCode> {
        let Some(surface) = self.surface_mut(id) else {
            return Err(ErrorCode::BadHandle);
        };
        surface.owner = owner;
        Ok(())
    }

    /// Attach a buffer to a surface.
    ///
    /// The handle is resolved in the caller's capability table with `Rights::MAP`:
    /// the buffer must be a memory object the caller is allowed to map, not an
    /// arbitrary path or endpoint.
    pub fn attach(
        &mut self,
        id: u32,
        owner: usize,
        table: &CapabilityTable,
        objects: &crate::services::object::ObjectRegistry,
        stride: u32,
        format: PixelFormat,
        handle: nova_abi::cap::CapHandle,
    ) -> Result<(), ErrorCode> {
        let resolved = crate::services::cap::resolve(table, objects, handle, Rights::MAP)?;
        let ObjectKind::MemoryObject { base, size } = resolved.kind else {
            return Err(ErrorCode::Denied);
        };

        let Some(existing) = self.surface(id) else {
            return Err(ErrorCode::BadHandle);
        };
        if existing.owner != owner {
            return Err(ErrorCode::Denied);
        }
        let width = existing.width;
        let height = existing.height;

        // A buffer capability names one object; two surfaces sharing it would
        // make closing one window free the other's pixels. Refuse the second
        // attachment rather than hiding the double ownership.
        if self.surfaces.iter().flatten().any(|other| {
            other.id != id
                && other
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.object_ref == resolved.object_ref)
        }) {
            return Err(ErrorCode::Denied);
        }

        let required = required_buffer_bytes(width, height, stride).ok_or(ErrorCode::Invalid)?;
        if required > size {
            return Err(ErrorCode::Invalid);
        }
        if stride % 4 != 0 {
            return Err(ErrorCode::Invalid);
        }

        let Some(surface) = self.surface_mut(id) else {
            return Err(ErrorCode::BadHandle);
        };
        surface.buffer = Some(SurfaceBuffer {
            base,
            size,
            object_ref: resolved.object_ref,
        });
        surface.stride = stride;
        surface.format = format.as_u32();
        surface.full = true;
        surface.dirty = true;
        Ok(())
    }

    /// Mark damaged rectangles.
    pub fn damage(
        &mut self,
        id: u32,
        owner: usize,
        rectangles: &[SurfaceRect],
    ) -> Result<usize, ErrorCode> {
        let surface = self.owned_mut(id, owner)?;

        let mut accepted = 0usize;
        for rect in rectangles {
            if !rect.is_non_empty() || rect.width == 0 || rect.height == 0 {
                return Err(ErrorCode::Invalid);
            }
            let Some(end_x) = rect.end_x() else {
                return Err(ErrorCode::Invalid);
            };
            let Some(end_y) = rect.end_y() else {
                return Err(ErrorCode::Invalid);
            };
            if rect.x < 0 || rect.y < 0 {
                return Err(ErrorCode::Invalid);
            }
            if end_x > surface.width as i64 || end_y > surface.height as i64 {
                return Err(ErrorCode::Invalid);
            }

            if surface.damage_count < MAX_DAMAGE_ACCUM {
                surface.damage[surface.damage_count] = *rect;
                surface.damage_count += 1;
                accepted += 1;
            } else {
                // Too much history to keep per rectangle: redraw the whole
                // surface when the frame is presented.
                surface.full = true;
                surface.damage_count = 0;
                accepted += 0;
                break;
            }
        }

        surface.dirty = true;
        Ok(accepted)
    }

    /// Commit a surface's content.
    pub fn commit(&mut self, id: u32, owner: usize, serial: u64) -> Result<u64, ErrorCode> {
        let surface = self.owned_mut(id, owner)?;
        surface.serial = serial;
        surface.dirty = true;
        if surface.damage_count == 0 {
            // A commit with no damage means the client redrew the whole buffer.
            surface.full = true;
        }
        Ok(serial)
    }

    /// Destroy a surface, releasing the buffer it attached.
    ///
    /// The buffer is freed only if its object still lives in the registry. A
    /// client that destroyed the memory object first has already returned its
    /// frames, so this must not free them a second time.
    pub fn destroy(
        &mut self,
        id: u32,
        owner: usize,
        objects: &mut crate::services::object::ObjectRegistry,
    ) -> Result<(), ErrorCode> {
        let Some(slot) = self
            .surfaces
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|surface| surface.id == id))
        else {
            return Err(ErrorCode::BadHandle);
        };
        let surface = self.surfaces[slot].expect("the selected surface exists");
        if surface.owner != owner {
            return Err(ErrorCode::Denied);
        }
        if let Some(buffer) = surface.buffer {
            if let Some(kind) = objects.get(buffer.object_ref) {
                if let ObjectKind::MemoryObject { .. } = *kind {
                    crate::services::dispatch::free_memory_object(buffer.base, buffer.size);
                    objects.destroy(buffer.object_ref);
                }
            }
        }
        self.surfaces[slot] = None;
        if self.focused == Some(id) {
            // Focus moves to the topmost remaining surface, so a closed window
            // does not leave the desktop without a receiver.
            self.focused = self.topmost();
        }
        Ok(())
    }

    /// Move a surface.
    pub fn move_surface(&mut self, id: u32, owner: usize, x: i64, y: i64) -> Result<(), ErrorCode> {
        let surface = self.owned_mut(id, owner)?;
        // A negative position is representable on the wire but the compositor
        // clips at the display's edge; clamping here keeps every draw offset
        // non-negative. The old and new regions are both redrawn.
        let new_x = x.max(0);
        let new_y = y.max(0);
        if new_x == surface.x && new_y == surface.y {
            return Ok(());
        }
        surface.x = new_x;
        surface.y = new_y;
        surface.full = true;
        surface.dirty = true;
        Ok(())
    }

    /// Raise a surface in the z-order.
    pub fn raise(&mut self, id: u32, owner: usize) -> Result<(), ErrorCode> {
        let _ = self.owned_mut(id, owner)?;
        self.raise_unchecked(id)
    }

    /// Raise a surface the compositor itself selected, such as the topmost
    /// surface under a click.
    fn raise_unchecked(&mut self, id: u32) -> Result<(), ErrorCode> {
        let z = self.next_z;
        {
            let Some(surface) = self.surface_mut(id) else {
                return Err(ErrorCode::BadHandle);
            };
            surface.z = z;
            surface.full = true;
            surface.dirty = true;
        }
        self.next_z = z.saturating_add(1);
        Ok(())
    }

    /// Set focus to a surface.
    pub fn set_focus(&mut self, id: u32) -> Result<(), ErrorCode> {
        if self.surface(id).is_none() {
            return Err(ErrorCode::BadHandle);
        }
        self.focused = Some(id);
        Ok(())
    }

    /// Clear focus.
    pub fn clear_focus(&mut self) {
        self.focused = None;
    }

    /// The topmost surface in z-order.
    fn topmost(&self) -> Option<u32> {
        let mut best: Option<&Surface> = None;
        for surface in self.surfaces.iter().flatten() {
            if best.is_none_or(|current| surface.z > current.z) {
                best = Some(surface);
            }
        }
        best.map(|surface| surface.id)
    }

    /// The topmost surface whose buffer covers `(x, y)`.
    pub fn pointer_at(&self, x: i64, y: i64) -> Option<u32> {
        let mut best: Option<&Surface> = None;
        for surface in self.surfaces.iter().flatten() {
            if !surface.has_buffer() {
                continue;
            }
            let Some(right) = surface.right() else {
                continue;
            };
            let Some(bottom) = surface.bottom() else {
                continue;
            };
            if x >= surface.x && x < right && y >= surface.y && y < bottom {
                if best.is_none_or(|current| surface.z > current.z) {
                    best = Some(surface);
                }
            }
        }
        best.map(|surface| surface.id)
    }

    /// Route a pointer event to the topmost surface under the pointer.
    ///
    /// A primary button press moves focus and raises the clicked surface. The
    /// request's named surface is deliberately not used.
    pub fn pointer_event(&mut self, x: i64, y: i64, buttons: u32) -> Option<u32> {
        if buttons & nova_protocols::input::INPUT_BUTTON_PRIMARY == 0 {
            return self.focused;
        }

        let hit = self.pointer_at(x, y);
        if let Some(id) = hit {
            self.focused = Some(id);
            let _ = self.raise_unchecked(id);
        } else {
            self.focused = None;
        }
        hit
    }

    /// Present pending frames.
    ///
    /// Returns how many surfaces were drawn. A compositor with nothing dirty
    /// returns `0` without touching the display, which is what lets an idle
    /// desktop stop drawing.
    pub fn present(&mut self) -> Result<usize, DisplayError> {
        if !self.frame_pending() {
            return Ok(0);
        }
        let Some(mode) = display::mode() else {
            return Err(DisplayError::NoDisplay);
        };
        let _ = mode;

        let full = self
            .surfaces
            .iter()
            .flatten()
            .any(|surface| surface.full && surface.has_buffer());

        if full {
            display::clear(BACKGROUND_RED, BACKGROUND_GREEN, BACKGROUND_BLUE)?;
            self.draw_all()?;
        } else {
            self.draw_damage()?;
        }

        for surface in self.surfaces.iter_mut().flatten() {
            surface.dirty = false;
            surface.full = false;
            surface.damage_count = 0;
        }
        self.frame_count = self.frame_count.saturating_add(1);
        // Draw the mouse pointer on top of every surface.
        if let Some((x, y)) = crate::drivers::input::mouse_position() {
            draw_mouse_pointer(x, y)?;
        }

        Ok(self.surface_count())
    }

    /// Draw every buffer-bearing surface in z-order.
    fn draw_all(&mut self) -> Result<(), DisplayError> {
        let mut last_z: i64 = -1;
        for _ in 0..MAX_SURFACES {
            let mut best: Option<(u32, usize)> = None;
            for (index, entry) in self.surfaces.iter().enumerate() {
                let Some(surface) = entry else {
                    continue;
                };
                if !surface.has_buffer() || (surface.z as i64) <= last_z {
                    continue;
                }
                if best.is_none_or(|(z, _)| surface.z < z) {
                    best = Some((surface.z, index));
                }
            }
            let Some((z, index)) = best else {
                break;
            };
            last_z = z as i64;
            let surface = self.surfaces[index].expect("the selected entry exists");
            self.blit_full(&surface)?;
        }
        Ok(())
    }

    /// Draw the damaged rectangles of every dirty surface.
    fn draw_damage(&mut self) -> Result<(), DisplayError> {
        for entry in self.surfaces.iter() {
            let Some(surface) = entry else {
                continue;
            };
            if !surface.dirty || !surface.has_buffer() {
                continue;
            }
            if surface.full {
                self.blit_full(surface)?;
                continue;
            }
            for rect in surface.damage.iter().take(surface.damage_count) {
                self.blit_rect(surface, *rect)?;
            }
        }
        Ok(())
    }

    /// Blit a surface's whole buffer at its placement.
    fn blit_full(&self, surface: &Surface) -> Result<(), DisplayError> {
        let Some(buffer) = surface.buffer else {
            return Err(DisplayError::InvalidBuffer);
        };
        let source = match usize::try_from(buffer.size) {
            Ok(size) => {
                // SAFETY: the buffer was allocated from the kernel's frame
                // allocator and remains owned while the surface exists; the
                // direct map is live for the kernel's whole lifetime.
                unsafe { direct_map::frame_bytes(buffer.base, size) }
            }
            Err(_) => return Err(DisplayError::InvalidBuffer),
        };
        display::blit(
            source,
            surface.stride,
            surface.x as u64,
            surface.y as u64,
            surface.width as u64,
            surface.height as u64,
        )
    }

    /// Blit one damaged rectangle.
    fn blit_rect(&self, surface: &Surface, rect: SurfaceRect) -> Result<(), DisplayError> {
        let Some(buffer) = surface.buffer else {
            return Err(DisplayError::InvalidBuffer);
        };
        let source = match usize::try_from(buffer.size) {
            Ok(size) => {
                // SAFETY: as in `blit_full`: the buffer is kernel-owned while
                // the surface exists, and the direct map is live.
                unsafe { direct_map::frame_bytes(buffer.base, size) }
            }
            Err(_) => return Err(DisplayError::InvalidBuffer),
        };
        let x = surface.x.saturating_add(rect.x as i64).max(0) as u64;
        let y = surface.y.saturating_add(rect.y as i64).max(0) as u64;
        display::blit(
            source,
            surface.stride,
            x,
            y,
            rect.width as u64,
            rect.height as u64,
        )
    }

    /// Remove every surface owned by `owner` and release its buffers.
    ///
    /// Called when a task dies. A crashed GUI process must not leave its window,
    /// its focus, or its frames behind.
    pub fn remove_owner(
        &mut self,
        owner: usize,
        objects: &mut crate::services::object::ObjectRegistry,
    ) -> usize {
        let mut removed = 0usize;
        let mut focus_lost = false;

        for index in 0..MAX_SURFACES {
            let Some(surface) = self.surfaces[index] else {
                continue;
            };
            if surface.owner != owner {
                continue;
            }
            if let Some(buffer) = surface.buffer {
                if let Some(kind) = objects.get(buffer.object_ref) {
                    if let ObjectKind::MemoryObject { .. } = *kind {
                        crate::services::dispatch::free_memory_object(buffer.base, buffer.size);
                        objects.destroy(buffer.object_ref);
                    }
                }
            }
            if self.focused == Some(surface.id) {
                focus_lost = true;
            }
            self.surfaces[index] = None;
            removed += 1;
        }

        if focus_lost {
            self.focused = self.topmost();
        }
        removed
    }

    /// Handle one compositor-protocol request.
    pub fn handle_message(
        &mut self,
        request: &Message,
        owner: usize,
        table: &CapabilityTable,
        objects: &mut crate::services::object::ObjectRegistry,
    ) -> Message {
        let opcode = request.opcode();
        let payload = request.payload();

        match opcode {
            SURFACE_OP_CREATE => self.handle_create(payload, owner),
            SURFACE_OP_ATTACH => self.handle_attach(payload, owner, table, objects),
            SURFACE_OP_DAMAGE => self.handle_damage(payload, owner),
            SURFACE_OP_COMMIT => self.handle_commit(payload, owner),
            SURFACE_OP_DESTROY => self.handle_destroy(payload, owner, objects),
            COMPOSITOR_OP_CONFIGURE => self.handle_configure(payload, owner),
            COMPOSITOR_OP_FRAME_DONE => self.handle_frame_done(payload, owner),
            COMPOSITOR_OP_MOVE => self.handle_move(payload, owner),
            COMPOSITOR_OP_RAISE => self.handle_raise(payload, owner),
            _ => reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE),
        }
    }

    /// The surface `id`, mutably, if `owner` owns it.
    fn owned_mut(&mut self, id: u32, owner: usize) -> Result<&mut Surface, ErrorCode> {
        let Some(surface) = self.surface_mut(id) else {
            return Err(ErrorCode::BadHandle);
        };
        if surface.owner != owner {
            return Err(ErrorCode::Denied);
        }
        Ok(surface)
    }

    fn handle_create(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() != SURFACE_CREATE_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_CREATE_REPLY_SIZE);
        }
        let Some(width) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        };
        let Some(height) = read_u32(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        };
        let Some(format_word) = read_u32(payload, 12) else {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        };
        let Some(flags) = read_u32(payload, 16) else {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        };
        if flags != 0 {
            return reply_error(ErrorCode::Invalid, SURFACE_CREATE_REPLY_SIZE);
        }
        let Some(format) = PixelFormat::from_u32(format_word) else {
            return reply_error(ErrorCode::Unsupported, SURFACE_CREATE_REPLY_SIZE);
        };

        match self.create(width, height, format) {
            Ok(id) => {
                let _ = self.set_owner(id, owner);
                reply_create(id)
            }
            Err(code) => reply_error(code, SURFACE_CREATE_REPLY_SIZE),
        }
    }

    fn handle_attach(
        &mut self,
        payload: &[u8],
        owner: usize,
        table: &CapabilityTable,
        objects: &crate::services::object::ObjectRegistry,
    ) -> Message {
        if payload.len() != SURFACE_ATTACH_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(surface) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(stride) = read_u32(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(format_word) = read_u32(payload, 12) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(format) = PixelFormat::from_u32(format_word) else {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        };
        let Some(handle_word) = read_u64(payload, 16) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let handle = nova_abi::cap::CapHandle::from_raw(handle_word);

        match self.attach(surface, owner, table, objects, stride, format, handle) {
            Ok(()) => reply_status(0),
            Err(code) => reply_error(code, SURFACE_REPLY_SIZE),
        }
    }

    fn handle_damage(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() < SURFACE_DAMAGE_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_DAMAGE_REPLY_SIZE);
        }
        let Some(surface) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        };
        let Some(count) = read_u16(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        };
        let Some(reserved) = read_u16(payload, 10) else {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        };
        if reserved != 0 || count > MAX_DAMAGE_RECTS as u16 {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        }

        let expected = SURFACE_DAMAGE_REQUEST_SIZE + (count as usize) * 16;
        if payload.len() != expected {
            return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
        }

        let mut rects = [SurfaceRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }; MAX_DAMAGE_RECTS];
        for index in 0..count as usize {
            let offset = SURFACE_DAMAGE_REQUEST_SIZE + index * 16;
            let Some(x) = read_i32(payload, offset) else {
                return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
            };
            let Some(y) = read_i32(payload, offset + 4) else {
                return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
            };
            let Some(width) = read_u32(payload, offset + 8) else {
                return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
            };
            let Some(height) = read_u32(payload, offset + 12) else {
                return reply_error(ErrorCode::Invalid, SURFACE_DAMAGE_REPLY_SIZE);
            };
            rects[index] = SurfaceRect {
                x,
                y,
                width,
                height,
            };
        }

        match self.damage(surface, owner, &rects[..count as usize]) {
            Ok(accepted) => reply_damage(accepted as u16),
            Err(code) => reply_error(code, SURFACE_DAMAGE_REPLY_SIZE),
        }
    }

    fn handle_commit(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() != SURFACE_COMMIT_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(surface) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(serial) = read_u64(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };

        match self.commit(surface, owner, serial) {
            Ok(_) => reply_status(0),
            Err(code) => reply_error(code, SURFACE_REPLY_SIZE),
        }
    }

    fn handle_destroy(
        &mut self,
        payload: &[u8],
        owner: usize,
        objects: &mut crate::services::object::ObjectRegistry,
    ) -> Message {
        if payload.len() != SURFACE_DESTROY_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(surface) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };

        match self.destroy(surface, owner, objects) {
            Ok(()) => reply_status(0),
            Err(code) => reply_error(code, SURFACE_REPLY_SIZE),
        }
    }

    fn handle_configure(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() != COMPOSITOR_CONFIGURE_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(id) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(width) = read_u32(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(height) = read_u32(payload, 12) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(scale) = read_u16(payload, 16) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(reserved) = read_u16(payload, 18) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if reserved != 0 || width == 0 || height == 0 || scale == 0 {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }

        let surface = match self.owned_mut(id, owner) {
            Ok(surface) => surface,
            Err(code) => return reply_error(code, SURFACE_REPLY_SIZE),
        };
        if let Some(buffer) = surface.buffer {
            let Some(required) = required_buffer_bytes(width, height, surface.stride) else {
                return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
            };
            if required > buffer.size {
                return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
            }
        }
        surface.width = width;
        surface.height = height;
        surface.scale = scale;
        surface.full = true;
        surface.dirty = true;
        reply_status(0)
    }

    fn handle_frame_done(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() != COMPOSITOR_FRAME_DONE_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(id) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(serial) = read_u64(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let surface = match self.owned_mut(id, owner) {
            Ok(surface) => surface,
            Err(code) => return reply_error(code, SURFACE_REPLY_SIZE),
        };
        surface.serial = serial;
        reply_status(0)
    }

    fn handle_move(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() != COMPOSITOR_MOVE_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(id) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(x) = read_i32(payload, 8) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        let Some(y) = read_i32(payload, 12) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };

        match self.move_surface(id, owner, x as i64, y as i64) {
            Ok(()) => reply_status(0),
            Err(code) => reply_error(code, SURFACE_REPLY_SIZE),
        }
    }

    fn handle_raise(&mut self, payload: &[u8], owner: usize) -> Message {
        if payload.len() != COMPOSITOR_RAISE_REQUEST_SIZE {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };
        if negotiate(SURFACE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported, SURFACE_REPLY_SIZE);
        }
        let Some(id) = read_u32(payload, 4) else {
            return reply_error(ErrorCode::Invalid, SURFACE_REPLY_SIZE);
        };

        match self.raise(id, owner) {
            Ok(()) => reply_status(0),
            Err(code) => reply_error(code, SURFACE_REPLY_SIZE),
        }
    }
}

impl Default for CompositorService {
    fn default() -> Self {
        Self::new()
    }
}

/// A create reply.
fn reply_create(id: u32) -> Message {
    let mut bytes = [0u8; SURFACE_CREATE_REPLY_SIZE];
    write_version(&mut bytes, SURFACE_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..12].copy_from_slice(&id.to_le_bytes());
    bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

/// A status reply.
fn reply_status(status: i32) -> Message {
    let mut bytes = [0u8; SURFACE_REPLY_SIZE];
    write_version(&mut bytes, SURFACE_VERSION);
    bytes[4..8].copy_from_slice(&status.to_le_bytes());
    bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

/// An error reply.
fn reply_error(code: ErrorCode, size: usize) -> Message {
    // Both surface reply layouts are twelve bytes; this is the larger of the
    // two sizes, as a constant so the array length stays a compile-time value.
    const MAX_REPLY_SIZE: usize = SURFACE_DAMAGE_REPLY_SIZE;
    let mut bytes = [0u8; MAX_REPLY_SIZE];
    let used = size.min(bytes.len());
    write_version(&mut bytes, SURFACE_VERSION);
    if used >= 8 {
        bytes[4..8].copy_from_slice(&code.code().to_le_bytes());
    }
    bytes[8..used].fill(0);
    Message::with_payload(0, &bytes[..used]).unwrap_or_else(|_| Message::new(0))
}

/// A damage reply.
fn reply_damage(accepted: u16) -> Message {
    let mut bytes = [0u8; SURFACE_DAMAGE_REPLY_SIZE];
    write_version(&mut bytes, SURFACE_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..10].copy_from_slice(&accepted.to_le_bytes());
    bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

fn write_version(bytes: &mut [u8], version: nova_protocols::ProtocolVersion) {
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
}

fn read_version(payload: &[u8], offset: usize) -> Option<nova_protocols::ProtocolVersion> {
    let major = read_u16(payload, offset)?;
    let minor = read_u16(payload, offset.checked_add(2)?)?;
    Some(nova_protocols::ProtocolVersion::new(major, minor))
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let bytes = payload.get(offset..end)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let bytes = payload.get(offset..end)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_i32(payload: &[u8], offset: usize) -> Option<i32> {
    let end = offset.checked_add(4)?;
    let bytes = payload.get(offset..end)?;
    Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(payload: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(payload.get(offset..end)?);
    Some(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::{CompositorService, MAX_DAMAGE_ACCUM, Surface, SurfaceRect};
    use nova_protocols::surface::{PixelFormat, required_buffer_bytes};

    fn rect(width: u32, height: u32) -> SurfaceRect {
        SurfaceRect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn an_empty_compositor_has_no_pending_frame() {
        let mut compositor = CompositorService::new();
        assert_eq!(compositor.surface_count(), 0);
        assert!(!compositor.frame_pending());
        assert_eq!(compositor.present().unwrap_or(0), 0);
    }

    #[test]
    fn create_requires_a_positive_size_and_the_only_format() {
        let mut compositor = CompositorService::new();
        assert!(compositor.create(0, 10, PixelFormat::Xrgb8888).is_err());
        assert!(compositor.create(10, 0, PixelFormat::Xrgb8888).is_err());
        assert!(compositor.create(10, 10, PixelFormat::Xrgb8888).is_ok());
        assert_eq!(compositor.surface_count(), 1);
    }

    #[test]
    fn damage_rectangles_must_lie_inside_the_surface() {
        let mut compositor = CompositorService::new();
        let id = compositor.create(10, 10, PixelFormat::Xrgb8888).unwrap();
        assert!(compositor.damage(id, 0, &[rect(4, 4)]).is_ok());
        assert!(
            compositor
                .damage(
                    id,
                    0,
                    &[SurfaceRect {
                        x: 8,
                        y: 0,
                        width: 4,
                        height: 4
                    }]
                )
                .is_err()
        );
    }

    #[test]
    fn a_stride_must_cover_the_surface() {
        let surface = Surface {
            id: 1,
            owner: 0,
            width: 10,
            height: 4,
            stride: 40,
            x: 0,
            y: 0,
            z: 1,
            serial: 0,
            buffer: None,
            format: 1,
            scale: 100,
            damage: [SurfaceRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }; MAX_DAMAGE_ACCUM],
            damage_count: 0,
            full: false,
            dirty: false,
        };
        assert_eq!(
            required_buffer_bytes(surface.width, surface.height, surface.stride),
            Some(160)
        );
        assert_eq!(required_buffer_bytes(10, 4, 39), None);
    }
}

/// Draw the mouse pointer on the screen — a classic arrow.
///
/// The cursor is drawn as the last step of `present()`: all window blits are already complete when we write it, so the pointer is the topmost thing on the screen.
fn draw_mouse_pointer(x: u32, y: u32) -> Result<(), DisplayError> {
    // Rows: each entry is a sequence of `#` (black border), `.` (white fill),
    // or ' ' (transparent). Pointer tip is the first row.
    const PATTERN: &[&str] = &[
        "#", "##", "#.#", "#..#", "#...#", "#....#", "#######", "#.....#", "#..#..#", "#.#.#.#",
        "##...##", "..   ..",
    ];

    if let Some(mode) = display::mode() {
        for (row, pattern) in PATTERN.iter().enumerate() {
            let Some(row_u32) = row.try_into().ok() else {
                continue;
            };
            for (col, ch) in pattern.bytes().enumerate() {
                let (r, g, b) = match ch {
                    b'#' => (0, 0, 0),
                    b'.' => (255, 255, 255),
                    _ => continue,
                };
                let Some(col_u32) = col.try_into().ok() else {
                    continue;
                };
                let _ = put_pixel(
                    x.saturating_add(col_u32),
                    y.saturating_add(row_u32),
                    r,
                    g,
                    b,
                );
            }
        }
    }
    Ok(())
}

fn put_pixel(x: u32, y: u32, r: u8, g: u8, b: u8) -> Result<(), DisplayError> {
    display::put_pixel(x, y, r, g, b)
}
