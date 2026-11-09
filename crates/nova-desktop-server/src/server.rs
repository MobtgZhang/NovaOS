//! The desktop server state: surfaces, z-order, and the frame decision.
//!
//! This module is the policy core in exactly the sense Wayland means it:
//! it knows which surfaces exist, which one is under the pointer, which one
//! has keyboard focus, and what the next frame must repaint. It owns no
//! pixels; the kernel owns the framebuffer and calls [`Self::plan`] before
//! every repaint to learn what changed.
//!
//! [`Self::plan`] deliberately returns the *minimal* correct answer — the
//! union of every damaged surface clipped to the output, plus whatever
//! transient extra damage (the old cursor cell) the caller registers. If a
//! client committed a full-damage frame, the whole output repaints; anything
//! less and only the dirty region does.
//!
//! # Why the surfaces are inline
//!
//! Everything in this crate is a value with a fixed footprint: no allocator,
//! no globals, no `Vec`. A desktop that hiccuped because the frame planner
//! needed a heap would be a desktop nobody trusts.

use crate::damage::{DamageAccumulator, DamageRect, Repaint, SurfaceDamage};
use crate::seat::Seat;

/// The number of surfaces the server tracks, in-line.
pub const MAX_SURFACES: usize = 16;

/// One surface: a client-drawn rectangle of pixels, placed by the server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Surface {
    /// The server's id for this surface.
    pub id: u32,
    /// The surface's position, in output pixels.
    pub x: i32,
    /// The surface's position, in output pixels.
    pub y: i32,
    /// The surface's width, in pixels.
    pub width: u32,
    /// The surface's height, in pixels.
    pub height: u32,
    /// The stacking order; higher is on top.
    pub z: u32,
    /// Whether the surface currently has a buffer attached.
    pub has_buffer: bool,
    /// The surface's pending damage, local to its own bounds.
    pub damage: SurfaceDamage,
}

impl Surface {
    /// A new, bufferless surface sitting at the back of the z-order.
    const fn new(id: u32, width: u32, height: u32, z: u32) -> Self {
        Self {
            id,
            x: 0,
            y: 0,
            width,
            height,
            z,
            has_buffer: false,
            damage: SurfaceDamage::new(),
        }
    }

    /// The surface's right edge (exclusive), in output pixels.
    pub const fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    /// The surface's bottom edge (exclusive), in output pixels.
    pub const fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    /// Whether the surface has a buffer and overlaps the rectangle.
    pub fn intersects(&self, x: i32, y: i32, w: u32, h: u32) -> bool {
        self.has_buffer
            && x < self.right()
            && y < self.bottom()
            && x + w as i32 > self.x
            && y + h as i32 > self.y
    }

    /// Whether the surface covers the point.
    pub fn covers(&self, x: i32, y: i32) -> bool {
        self.has_buffer && x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
}

/// What the compositor should do with the next presented frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Frame {
    /// Nothing to do.
    Nothing,
    /// Repaint these regions (surface-local damage union, output-local).
    Regions([DamageRect; crate::damage::MAX_DAMAGE_RECTS], usize),
    /// Repaint the whole output.
    Full,
}

impl Frame {
    /// Whether the frame has nothing to show.
    pub const fn is_nothing(&self) -> bool {
        matches!(self, Frame::Nothing)
    }
}

/// The desktop server: surfaces, seat, and the frame decision.
#[derive(Clone, Copy, Debug)]
pub struct DesktopServer {
    surfaces: [Option<Surface>; MAX_SURFACES],
    seat: Seat,
    next_id: u32,
    next_z: u32,
    output_w: u32,
    output_h: u32,
    /// Extra damage the server adds for its own chrome (the cursor).
    cursor_damage: Option<DamageRect>,
}

impl DesktopServer {
    /// An empty server serving an output of `output_w` × `output_h`.
    pub const fn new(output_w: u32, output_h: u32) -> Self {
        Self {
            surfaces: [None; MAX_SURFACES],
            seat: Seat::new(),
            next_id: 1,
            next_z: 1,
            output_w,
            output_h,
            cursor_damage: None,
        }
    }

    /// The surface with `id`.
    pub fn surface(&self, id: u32) -> Option<&Surface> {
        self.surfaces.iter().flatten().find(|s| s.id == id)
    }

    /// A mutable reference to the surface with `id`.
    fn surface_mut(&mut self, id: u32) -> Option<&mut Surface> {
        self.surfaces.iter_mut().flatten().find(|s| s.id == id)
    }

    /// The number of live surfaces.
    pub fn surface_count(&self) -> usize {
        self.surfaces.iter().flatten().count()
    }

    /// Create a new surface of `width`×`height`, when a slot is free.
    pub fn create(&mut self, width: u32, height: u32) -> Option<u32> {
        let slot = self.surfaces.iter().position(|entry| entry.is_none())?;
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(1);
        self.surfaces[slot] = Some(Surface::new(id, width, height, z));
        Some(id)
    }

    /// Attach a buffer to a surface (the buffer itself is the kernel's).
    pub fn attach(&mut self, id: u32) -> bool {
        match self.surface_mut(id) {
            Some(surface) => {
                surface.has_buffer = true;
                true
            }
            None => false,
        }
    }

    /// Damage a rectangle of the surface, clipped to the surface's bounds.
    pub fn damage(&mut self, id: u32, rect: DamageRect) -> bool {
        let Some(surface) = self.surface_mut(id) else {
            return false;
        };
        if let Some(clipped) = rect.clipped(surface.width, surface.height) {
            surface.damage.add(clipped);
        }
        true
    }

    /// Mark the whole surface damaged.
    pub fn damage_full(&mut self, id: u32) -> bool {
        match self.surface_mut(id) {
            Some(surface) => {
                surface.damage.damage_full();
                true
            }
            None => false,
        }
    }

    /// The seat, for reading focus and pointer position.
    pub const fn seat(&self) -> &Seat {
        &self.seat
    }

    /// Update the pointer position, returning the surface under it.
    pub fn pointer_moved(&mut self, x: i32, y: i32) -> Option<u32> {
        self.seat.pointer_moved(x, y);
        self.hit_test(x, y)
    }

    /// The topmost surface covering `(x, y)`, if any.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<u32> {
        let mut best: Option<&Surface> = None;
        for surface in self.surfaces.iter().flatten() {
            if !surface.covers(x, y) {
                continue;
            }
            if best.is_none_or(|current| surface.z > current.z) {
                best = Some(surface);
            }
        }
        best.map(|surface| surface.id)
    }

    /// Click the seat: focus and raise the surface under the pointer.
    pub fn click(&mut self, x: i32) -> Option<u32> {
        self.click_at(x, 0)
    }

    /// Click the seat at an output-local coordinate.
    pub fn click_at(&mut self, x: i32, y: i32) -> Option<u32> {
        self.seat.pointer_moved(x, y);
        let hit = self.hit_test(x, y);
        match hit {
            Some(id) => {
                self.raise(id);
                self.seat.focus(Some(id));
            }
            None => self.seat.focus(None),
        }
        hit
    }

    /// Raise the surface to the top of the stacking order.
    pub fn raise(&mut self, id: u32) -> bool {
        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(1);
        match self.surface_mut(id) {
            Some(surface) => {
                surface.z = z;
                true
            }
            None => false,
        }
    }

    /// Move the surface to `(x, y)`. The server decides placement; a client
    /// asks, but the last word is the server's.
    pub fn place(&mut self, id: u32, x: i32, y: i32) -> bool {
        match self.surface_mut(id) {
            Some(surface) => {
                surface.x = x;
                surface.y = y;
                true
            }
            None => false,
        }
    }

    /// Destroy the surface, returning the slot to the pool.
    pub fn destroy(&mut self, id: u32) -> bool {
        for entry in self.surfaces.iter_mut() {
            if entry.as_ref().is_some_and(|surface| surface.id == id) {
                *entry = None;
                return true;
            }
        }
        false
    }

    /// Register transient damage the server adds for its own chrome.
    ///
    /// The cursor is the motivating case: when the pointer moves, the cell it
    /// occupied outlives the frame, so that whole cell is damaged even though
    /// no client asked for it.
    pub fn set_cursor_damage(&mut self, rect: DamageRect) {
        self.cursor_damage = Some(rect);
    }

    /// Decide the frame the compositor should paint.
    ///
    /// After `plan` the caller presents and then calls [`Self::clear`].
    pub fn plan(&self) -> Frame {
        let mut accumulator = DamageAccumulator::new(self.output_w, self.output_h);
        for surface in self.surfaces.iter().flatten() {
            if surface.damage.full {
                return Frame::Full;
            }
            // Translate surface-local damage into output-local damage.
            for rect in surface.damage.rects.iter().take(surface.damage.count) {
                let output_rect =
                    DamageRect::new(rect.x + surface.x, rect.y + surface.y, rect.w, rect.h);
                accumulator.damage(0, output_rect, self.output_w, self.output_h);
            }
        }
        if let Some(cursor) = self.cursor_damage {
            accumulator.set_extra(cursor);
        }
        match accumulator.plan() {
            Repaint::Nothing => Frame::Nothing,
            Repaint::Full => Frame::Full,
            Repaint::Regions(rects, count) => Frame::Regions(rects, count),
        }
    }

    /// Forget all pending damage, after a frame is presented.
    pub fn clear(&mut self) {
        for surface in self.surfaces.iter_mut().flatten() {
            surface.damage.clear();
        }
        self.cursor_damage = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> DesktopServer {
        DesktopServer::new(1024, 768)
    }

    #[test]
    fn create_attach_and_hit_test() {
        let mut server = server();
        let id = server.create(200, 100).unwrap();
        assert_eq!(server.surface_count(), 1);
        assert!(!server.hit_test(10, 10).is_some());
        assert!(server.attach(id));
        server.place(id, 30, 40);
        assert_eq!(server.hit_test(50, 60), Some(id));
        assert_eq!(server.hit_test(0, 0), None);
    }

    #[test]
    fn damage_is_clipped_to_the_surface_bounds() {
        let mut server = server();
        let id = server.create(100, 100).unwrap();
        server.attach(id);
        server.damage(id, DamageRect::new(90, 90, 30, 30));
        match server.plan() {
            Frame::Regions(rects, count) => {
                assert_eq!(count, 1);
                assert_eq!(rects[0], DamageRect::new(90, 90, 10, 10));
            }
            other => panic!("expected regions, got {other:?}"),
        }
    }

    #[test]
    fn full_damage_forces_a_full_frame() {
        let mut server = server();
        let id = server.create(100, 100).unwrap();
        server.attach(id);
        server.damage_full(id);
        assert_eq!(server.plan(), Frame::Full);
        server.clear();
        assert_eq!(server.plan(), Frame::Nothing);
    }

    #[test]
    fn raise_swaps_topmost_surface() {
        let mut server = server();
        let a = server.create(100, 100).unwrap();
        let b = server.create(100, 100).unwrap();
        server.attach(a);
        server.attach(b);
        assert_eq!(server.hit_test(10, 10), Some(b));
        server.raise(a);
        assert_eq!(server.hit_test(10, 10), Some(a));
    }

    #[test]
    fn click_focuses_and_raises_the_surface_under_the_pointer() {
        let mut server = server();
        let a = server.create(100, 100).unwrap();
        let b = server.create(100, 100).unwrap();
        server.attach(a);
        server.attach(b);
        server.place(a, 0, 0);
        server.place(b, 150, 150);
        assert_eq!(server.click_at(25, 25), Some(a));
        assert_eq!(server.seat().focused, Some(a));
        assert_eq!(server.click_at(175, 175), Some(b));
        assert_eq!(server.seat().focused, Some(b));
        assert_eq!(server.click_at(999, 999), None);
        assert_eq!(server.seat().focused, None);
    }

    #[test]
    fn destroy_returns_the_surface() {
        let mut server = server();
        let id = server.create(100, 100).unwrap();
        assert_eq!(server.surface_count(), 1);
        assert!(server.destroy(id));
        assert_eq!(server.surface_count(), 0);
        assert_eq!(server.surface(id), None);
    }

    #[test]
    fn frame_nothing_when_no_damage() {
        let server = server();
        assert_eq!(server.plan(), Frame::Nothing);
    }

    #[test]
    fn cursor_damage_joins_the_union() {
        let mut server = server();
        let id = server.create(100, 100).unwrap();
        server.attach(id);
        server.damage(id, DamageRect::new(90, 80, 4, 4));
        server.set_cursor_damage(DamageRect::new(0, 0, 16, 12));
        match server.plan() {
            Frame::Regions(rects, count) => {
                assert_eq!(count, 1);
                assert_eq!(rects[0], DamageRect::new(0, 0, 94, 84));
            }
            other => panic!("expected regions, got {other:?}"),
        }
    }
}
