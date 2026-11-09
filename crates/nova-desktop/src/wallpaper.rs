//! The wallpaper: a deep-sea vertical gradient with a horizon glow and a
//! deterministic star field above it.
//!
//! Drawn from pure maths every repaint — no image asset anywhere — because
//! the wallpaper is cheap to compute and because an asset pipeline for one
//! picture is a dependency nobody should take on.

use nova_ui::Canvas;

use crate::{DESKTOP_DEEP, DESKTOP_HORIZON};

/// Paint the background.
///
/// The horizon line sits two-thirds down the surface; below it the colour
/// deepens slightly toward the bottom edge, above it a faint blue glow
/// fades into space. The stars are a fixed pseudo-random pattern — same
/// seed every boot — so the sky looks like a sky, not noise.
pub fn paint(canvas: &mut Canvas) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;
    let horizon = height * 2 / 3;

    // Vertical gradient: DESKTOP_DEEP at the top, HORIZON at the horizon,
    // then a short deep-sea descent toward the bottom edge.
    for y in 0..height {
        let above = y <= horizon;
        let t = if above {
            y as f32 / horizon as f32
        } else if height > horizon {
            (y - horizon) as f32 / (height - horizon) as f32
        } else {
            1.0
        };
        let from = if above { DESKTOP_DEEP } else { DESKTOP_HORIZON };
        let to = if above { DESKTOP_HORIZON } else { DESKTOP_DEEP };
        let colour = blend(from, to, t);
        canvas.fill_rect(nova_ui::Rect::new(0, y, width as u32, 1), colour);
    }

    // Horizon glow: one soft band of the accent colour centred on the line.
    let glow_top = (horizon - 60).max(0);
    let glow_height = 120u32;
    for row in 0..glow_height as i32 {
        let y = glow_top + row;
        if y >= height {
            break;
        }
        let distance = (row as f32 - glow_height as f32 / 2.0).abs() / (glow_height as f32 / 2.0);
        let strength = (1.0 - distance) * (1.0 - distance);
        let alpha = (0.28 * strength * 255.0) as u8;
        canvas.fill_rect(
            nova_ui::Rect::new(0, y, width as u32, 1),
            blend(crate::DESKTOP_HORIZON, crate::ACCENT, strength * 0.35),
        );
        let _ = alpha;
    }

    // Star field above the horizon. A tiny deterministic PRNG (xorshift32)
    // picks positions and brightness; the pattern never changes.
    let mut rng: u32 = 0x51ED_270B;
    for _ in 0..220 {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        let x = (rng % (width.max(1) as u32)) as i32;
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        let y = (rng % (horizon.max(1) as u32)) as i32;
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        let brightness = 60 + (rng & 0x7F);
        let colour = (brightness << 16) | (brightness << 8) | brightness;
        canvas.set_pixel(x, y, colour);
        // A few stars get a bigger cross-sparkle.
        if (rng & 0xF) == 0 {
            if x + 1 < width {
                canvas.set_pixel(x + 1, y, colour);
            }
            if y + 1 < height {
                canvas.set_pixel(x, y + 1, colour);
            }
        }
    }

    // One bright horizon star, slightly right of centre — the NovaOS beacon.
    let beacon_x = width * 3 / 5;
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let x = beacon_x + dx;
            let y = horizon - 160 + dy;
            if x >= 0 && y >= 0 && x < width && y < height {
                canvas.set_pixel(x, y, 0x00F2F6FA);
            }
        }
    }
}

/// Blend two packed `0x00RRGGBB` colours by `t` in `0..=1`.
fn blend(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xFF) as f32;
        let b = ((to >> shift) & 0xFF) as f32;
        (a + (b - a) * t).clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}
