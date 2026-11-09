//! The Wayland damage model.
//!
//! A *damage* is the set of pixels a client knows it changed since the last
//! commit. The server never scans buffers looking for differences — that is
//! what the client's explicit damage is for. Accumulating damage is what
//! makes a mouse cursor move without repainting the desktop underneath it.
//!
//! # The rules
//!
//! 1. Damage is *per surface* and *clipped to the surface's own bounds*.
//!    A client cannot damage a neighbour's pixels.
//! 2. Damage rectangles are *merged* when they overlap or touch, because
//!    ten overlapping rectangles rasterise the same pixels ten times.
//! 3. Too many rectangles collapse into one bounding box; the memory cost is
//!    bounded, always.
//! 4. The union of a frame's damages is clipped to the *output* — pixels
//!    outside the display are never painted.

/// One damaged rectangle, in surface-local coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DamageRect {
    /// Left edge, in surface-local pixels.
    pub x: i32,
    /// Top edge, in surface-local pixels.
    pub y: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

impl DamageRect {
    /// A rectangle from its origin and size.
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// The rectangle, clipped to `w`×`h` bounds (the surface's own size).
    pub fn clipped(&self, bounds_w: u32, bounds_h: u32) -> Option<Self> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + self.w as i32).min(bounds_w as i32);
        let y1 = (self.y + self.h as i32).min(bounds_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// The union of two rectangles, as their bounding box.
    pub fn union(&self, other: &Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.w as i32).max(other.x + other.w as i32);
        let y1 = (self.y + self.h as i32).max(other.y + other.h as i32);
        Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    /// Whether two rectangles overlap or touch.
    pub fn intersects_or_touches(&self, other: &Self) -> bool {
        self.x < other.x + other.w as i32
            && self.x + self.w as i32 > other.x
            && self.y < other.y + other.h as i32
            && self.y + self.h as i32 > other.y
    }

    /// Whether the rectangle is empty.
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// The maximum number of damage rectangles kept per surface.
///
/// Beyond this the accumulator folds them into one bounding box — bounded
/// memory always wins over fine-grained dirtiness.
pub const MAX_DAMAGE_RECTS: usize = 4;

/// One surface's accumulated damage.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SurfaceDamage {
    /// The rectangles currently pending, at most [`MAX_DAMAGE_RECTS`].
    pub rects: [DamageRect; MAX_DAMAGE_RECTS],
    /// Whether `rects` holds a pending rectangle.
    pub count: usize,
    /// Whether the whole surface must be repainted (too many overlapping
    /// damages, or an explicit full damage).
    pub full: bool,
}

impl SurfaceDamage {
    /// A clean surface: nothing pending.
    pub const fn new() -> Self {
        Self {
            rects: [DamageRect::new(0, 0, 0, 0); MAX_DAMAGE_RECTS],
            count: 0,
            full: false,
        }
    }

    /// Add one damaged rectangle, merging or folding as needed.
    pub fn add(&mut self, rect: DamageRect) {
        if rect.is_empty() {
            return;
        }
        if self.full {
            return;
        }
        // Merge into an existing rect when they overlap or touch.
        let mut merged = false;
        for existing in self.rects.iter_mut().take(self.count) {
            if existing.intersects_or_touches(&rect) {
                *existing = existing.union(&rect);
                merged = true;
                break;
            }
        }
        if merged {
            return;
        }
        if self.count < MAX_DAMAGE_RECTS {
            self.rects[self.count] = rect;
            self.count += 1;
            return;
        }
        // Too many distinct rects: fold everything — including the new one —
        // into a single bounding box.
        let mut bounds = rect;
        for existing in self.rects.iter().take(self.count) {
            bounds = bounds.union(existing);
        }
        self.rects[0] = bounds;
        self.count = 1;
    }

    /// Mark the whole surface damaged.
    pub const fn damage_full(&mut self) {
        self.full = true;
        self.count = 0;
    }

    /// Forget every pending rectangle.
    pub fn clear(&mut self) {
        self.count = 0;
        self.full = false;
    }
}

/// What the frame should repaint, as one value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repaint {
    /// Nothing to do — the display already shows this frame.
    Nothing,
    /// Repaint these regions of the output (in output-local pixels).
    Regions([DamageRect; MAX_DAMAGE_RECTS], usize),
    /// Repaint the whole output.
    Full,
}

impl Repaint {
    /// Whether the frame has nothing to show.
    pub const fn is_nothing(&self) -> bool {
        matches!(self, Repaint::Nothing)
    }
}

/// The accumulator that decides what a frame must repaint.
///
/// Holds per-surface damage plus one *extra* rectangle the server adds on
/// its own behalf — the mouse cursor's previous position, which is not part
/// of any client surface but must vanish when the cursor moves.
#[derive(Clone, Copy, Debug)]
pub struct DamageAccumulator {
    surfaces: [SurfaceDamage; MAX_SURFACES_TRACKED],
    /// One transient extra rectangle (e.g. the old cursor cell).
    extra: Option<DamageRect>,
    /// Output size, for clipping the union.
    output_w: u32,
    output_h: u32,
}

/// The number of surfaces whose damage the accumulator tracks inline.
///
/// Matches the kernel compositor's own per-frame workspace; an app surface
/// count beyond this is folded into `full` behaviour rather than tracked.
pub const MAX_SURFACES_TRACKED: usize = 16;

impl DamageAccumulator {
    /// An empty accumulator with the output size to clip unions into.
    pub const fn new(output_w: u32, output_h: u32) -> Self {
        Self {
            surfaces: [SurfaceDamage::new(); MAX_SURFACES_TRACKED],
            extra: None,
            output_w,
            output_h,
        }
    }

    /// Damage one rectangle of surface `index`.
    ///
    /// Rectangles are clipped to the surface's `bounds` here — the caller is
    /// expected to pass the surface's width and height, and the damage ends
    /// up clipped to it, so a buggy client cannot scribble outside its
    /// window.
    pub fn damage(&mut self, index: usize, rect: DamageRect, bounds_w: u32, bounds_h: u32) {
        let Some(surface) = self.surfaces.get_mut(index) else {
            return;
        };
        if let Some(clipped) = rect.clipped(bounds_w, bounds_h) {
            surface.add(clipped);
        }
    }

    /// Damage the whole surface `index`.
    pub fn damage_full(&mut self, index: usize) {
        if let Some(surface) = self.surfaces.get_mut(index) {
            surface.damage_full();
        }
    }

    /// Set the transient extra damage (cursor's old position).
    pub fn set_extra(&mut self, rect: DamageRect) {
        self.extra = Some(rect);
    }

    /// Decide the repaint plan for one frame.
    ///
    /// The rule is straight from Wayland: if anyone asked for a full repaint,
    /// the frame is full; otherwise paint the bounding box of everything
    /// damaged, clipped to the output. One rectangle, no ambiguity.
    pub fn plan(&self) -> Repaint {
        let mut bounds: Option<DamageRect> = None;
        for surface in &self.surfaces {
            if surface.full {
                return Repaint::Full;
            }
            for rect in surface.rects.iter().take(surface.count) {
                bounds = Some(match bounds {
                    Some(acc) => acc.union(rect),
                    None => *rect,
                });
            }
        }
        if let Some(extra) = self.extra {
            bounds = Some(match bounds {
                Some(acc) => acc.union(&extra),
                None => extra,
            });
        }
        let Some(bounds) = bounds else {
            return Repaint::Nothing;
        };
        let Some(clipped) = DamageRect {
            x: 0,
            y: 0,
            w: self.output_w,
            h: self.output_h,
        }
        .intersect(&bounds) else {
            // Nothing intersects the output: the damage is all off-screen.
            return Repaint::Nothing;
        };
        Repaint::Regions([clipped; MAX_DAMAGE_RECTS], 1)
    }

    /// Forget every pending damage.
    pub fn clear(&mut self) {
        for surface in &mut self.surfaces {
            surface.clear();
        }
        self.extra = None;
    }
}

impl DamageRect {
    /// The intersection with another rectangle, or `None` when disjoint.
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.w as i32).min(other.x + other.w as i32);
        let y1 = (self.y + self.h as i32).min(other.y + other.h as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_damage_merges_into_one_box() {
        let mut damage = SurfaceDamage::new();
        damage.add(DamageRect::new(10, 10, 20, 20));
        damage.add(DamageRect::new(25, 25, 20, 20));
        assert_eq!(damage.count, 1);
        assert_eq!(damage.rects[0], DamageRect::new(10, 10, 35, 35));
    }

    #[test]
    fn disjoint_rects_stay_separate_until_the_cap() {
        let mut damage = SurfaceDamage::new();
        damage.add(DamageRect::new(0, 0, 10, 10));
        damage.add(DamageRect::new(100, 100, 10, 10));
        assert_eq!(damage.count, 2);
    }

    #[test]
    fn too_many_rects_fold_to_one_bounding_box() {
        let mut damage = SurfaceDamage::new();
        for (i, rect) in [
            (10, 10, 5, 5),
            (100, 10, 5, 5),
            (10, 100, 5, 5),
            (100, 100, 5, 5),
            (300, 300, 5, 5),
        ]
        .into_iter()
        .enumerate()
        {
            damage.add(DamageRect::new(rect.0, rect.1, rect.2, rect.3));
            if i < 4 {
                assert!(damage.count <= MAX_DAMAGE_RECTS);
            }
        }
        assert_eq!(damage.count, 1);
        assert_eq!(damage.rects[0], DamageRect::new(10, 10, 295, 295));
    }

    #[test]
    fn clipped_drops_offscreen_parts() {
        let rect = DamageRect::new(-5, -5, 20, 20);
        assert_eq!(rect.clipped(100, 100), Some(DamageRect::new(0, 0, 15, 15)));
        assert_eq!(rect.clipped(3, 3), Some(DamageRect::new(0, 0, 3, 3)));
        assert_eq!(DamageRect::new(50, 50, 5, 5).clipped(40, 40), None);
    }

    #[test]
    fn accumulator_reports_nothing_when_clean() {
        let accumulator = DamageAccumulator::new(1024, 768);
        assert_eq!(accumulator.plan(), Repaint::Nothing);
    }

    #[test]
    fn accumulator_full_repaint_when_surface_is_full() {
        let mut accumulator = DamageAccumulator::new(1024, 768);
        accumulator.damage_full(0);
        assert_eq!(accumulator.plan(), Repaint::Full);
    }

    #[test]
    fn accumulator_clips_the_union_to_the_output() {
        let mut accumulator = DamageAccumulator::new(100, 100);
        // Damage entirely outside the output: nothing to paint.
        accumulator.damage(0, DamageRect::new(500, 500, 10, 10), 1000, 1000);
        assert_eq!(accumulator.plan(), Repaint::Nothing);
        // Damage partially outside: the inside is painted.
        let mut accumulator = DamageAccumulator::new(100, 100);
        accumulator.damage(0, DamageRect::new(90, 90, 20, 20), 1000, 1000);
        match accumulator.plan() {
            Repaint::Regions(rects, count) => {
                assert_eq!(count, 1);
                assert_eq!(rects[0], DamageRect::new(90, 90, 10, 10));
            }
            other => panic!("expected a region plan, got {other:?}"),
        }
    }

    #[test]
    fn cursor_extra_damage_reaches_the_union() {
        let mut accumulator = DamageAccumulator::new(1024, 768);
        accumulator.damage(0, DamageRect::new(100, 100, 10, 10), 500, 500);
        accumulator.set_extra(DamageRect::new(700, 500, 16, 12));
        match accumulator.plan() {
            Repaint::Regions(rects, count) => {
                assert_eq!(count, 1);
                assert_eq!(rects[0], DamageRect::new(100, 100, 616, 412));
            }
            other => panic!("expected a region plan, got {other:?}"),
        }
    }
}
