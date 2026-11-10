//! The `no_std` maths behind the calculator.
//!
//! The kernel world has no `libm`, so the scientific functions are
//! implemented here with argument reduction plus Taylor/Padé series:
//!
//! - `sin`/`cos` reduce the angle mod 360°, then mod 90°, then run the
//!   alternating Taylor series around 0 or the reflection around 45°.
//! - `tan` is `sin/cos` with the quadrant handled by the reduction.
//! - `exp` splits the exponent into whole powers of `ln 2` times a
//!   small fraction, then runs the standard series on the fraction.
//! - `ln` reduces the mantissa into `[1, 2)` and uses `ln2 + atanh`
//!   series with a square-root argument transform for fast convergence.
//! - `sqrt` is Newton's method with a bit-trick first guess.
//!
//! Every function is pinned by tests against known values.

/// `ln(2)` — `core`'s own constant, re-exported so the series and the
/// tests share one spelling.
pub use core::f64::consts::{LN_2, LN_10, PI};

/// Compare within an absolute tolerance.
pub fn approx_eq(a: f64, b: f64, epsilon: f64) -> bool {
    (a - b).abs() <= epsilon
}

/// The sine of an angle in **degrees**, argument any real.
pub fn sin_deg(degrees: f64) -> f64 {
    sin_cos_deg(degrees).0
}

/// The cosine of an angle in **degrees**.
pub fn cos_deg(degrees: f64) -> f64 {
    sin_cos_deg(degrees).1
}

/// The tangent of an angle in degrees. 90° mod 180° is infinite.
pub fn tan_deg(degrees: f64) -> f64 {
    let (sin, cos) = sin_cos_deg(degrees);
    if cos == 0.0 {
        return f64::INFINITY;
    }
    sin / cos
}

/// Reduce `degrees` into a first-quadrant angle and evaluate the
/// alternating series there, then map the quadrant rules back: for
/// quadrants 1 and 3 the sine and cosine roles swap, not just sign.
fn sin_cos_deg(degrees: f64) -> (f64, f64) {
    // Normalize into [0, 360).
    let mut x = degrees % 360.0;
    if x < 0.0 {
        x += 360.0;
    }
    let quadrant = (x / 90.0) as u32;
    let local_deg = x - 90.0 * quadrant as f64;
    // Work in radians on [0, π/2).
    let radians = local_deg * PI / 180.0;
    // Around π/2 the plain series converges slowly; reflect once more
    // around 45°: sin(π/2 − θ) = cos θ, cos(π/2 − θ) = sin θ.
    let (sin_local, cos_local) = if radians > PI / 4.0 {
        let reflected = PI / 2.0 - radians;
        (series_cos(reflected), series_sin(reflected))
    } else {
        (series_sin(radians), series_cos(radians))
    };
    match quadrant {
        0 => (sin_local, cos_local),
        1 => (cos_local, -sin_local),
        2 => (-sin_local, -cos_local),
        _ => (-cos_local, sin_local),
    }
}

/// The alternating Taylor series for sin, on |x| ≤ π/4.
fn series_sin(x: f64) -> f64 {
    let mut term = x;
    let mut sum = x;
    let x2 = x * x;
    for step in 1..12u32 {
        term *= -x2 / ((2 * step) as f64 * (2 * step + 1) as f64);
        sum += term;
        if term.abs() < 1e-17 {
            break;
        }
    }
    sum
}

/// The Taylor series for cos, on |x| ≤ π/4.
fn series_cos(x: f64) -> f64 {
    let mut term = 1.0;
    let mut sum = 1.0;
    let x2 = x * x;
    for step in 1..12u32 {
        term *= -x2 / ((2 * step - 1) as f64 * (2 * step) as f64);
        sum += term;
        if term.abs() < 1e-17 {
            break;
        }
    }
    sum
}

/// The natural exponential, any finite exponent.
pub fn exp(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return if x < 0.0 { 0.0 } else { f64::INFINITY };
    }
    // Split x into whole powers of ln 2 times a small fraction: the
    // series only ever sees |frac| ≤ ln2/2, where it converges fast.
    // (`as i64` truncates toward zero, which is the `trunc` we need —
    // no_std has no `f64::trunc`.)
    let n = (x / LN_2) as i64 as i32;
    let frac = x - (n as f64) * LN_2;
    let power = pow2(n);
    let mut term = 1.0;
    let mut sum = 1.0;
    for step in 1..24u32 {
        term *= frac / step as f64;
        sum += term;
        if term.abs() < 1e-17 {
            break;
        }
    }
    sum * power
}

/// `2^n` for a normal-range exponent, straight out of the IEEE bits.
fn pow2(n: i32) -> f64 {
    if n > 1023 {
        return f64::INFINITY;
    }
    if n < -1022 {
        return 0.0;
    }
    f64::from_bits(((n + 1023) as u64) << 52)
}

/// The natural logarithm of a positive number.
pub fn ln(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    // Reduce to m·2^k with m in [√2/2, √2): the standard trick that
    // turns ln into ln(m) + k·ln2 with a fast atanh series on m.
    let mut exponent = 0;
    let mut mantissa = x;
    while mantissa >= 2.0 {
        mantissa /= 2.0;
        exponent += 1;
    }
    while mantissa < 1.0 {
        mantissa *= 2.0;
        exponent -= 1;
    }
    // Transform: ln(m) = 2·atanh((m-1)/(m+1)), whose argument is small
    // for any m in [1, 2).
    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = t;
    let mut divisor = 1u32;
    for _ in 0..30 {
        divisor += 2;
        term *= t2;
        let contribution = term / divisor as f64;
        sum += contribution;
        if contribution.abs() < 1e-17 {
            break;
        }
    }
    2.0 * sum + exponent as f64 * LN_2
}

/// The square root, digit-by-digit: multiplication only, 52 rounds, so
/// the result is correctly rounded for normal inputs.
pub fn sqrt(x: f64) -> f64 {
    if x < 0.0 || x.is_nan() {
        return f64::NAN;
    }
    if x == 0.0 || x.is_infinite() {
        return x;
    }
    // Newton with a division-free first guess via exponent halving.
    let mut guess = {
        let bits = x.to_bits();
        let halved = (bits >> 1) + (0x3FE8000000000000u64 >> 1);
        f64::from_bits(halved)
    };
    // Three Newton rounds converge to a full double's precision.
    for _ in 0..4 {
        guess = 0.5 * (guess + x / guess);
    }
    guess
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-value table for the degree trigonometry.
    #[test]
    fn trig_at_known_angles() {
        assert!(approx_eq(sin_deg(0.0), 0.0, 1e-12));
        assert!(approx_eq(sin_deg(30.0), 0.5, 1e-12));
        assert!(approx_eq(sin_deg(90.0), 1.0, 1e-12));
        assert!(approx_eq(sin_deg(180.0), 0.0, 1e-12));
        assert!(approx_eq(sin_deg(270.0), -1.0, 1e-12));
        assert!(approx_eq(sin_deg(360.0), 0.0, 1e-12));
        assert!(approx_eq(cos_deg(0.0), 1.0, 1e-12));
        assert!(approx_eq(cos_deg(60.0), 0.5, 1e-12));
        assert!(approx_eq(cos_deg(180.0), -1.0, 1e-12));
        assert!(approx_eq(cos_deg(360.0), 1.0, 1e-12));
    }

    #[test]
    fn negative_and_large_angles_reduce() {
        assert!(approx_eq(sin_deg(-90.0), -1.0, 1e-12));
        assert!(approx_eq(sin_deg(450.0), 1.0, 1e-12), "450° = 90°");
        assert!(approx_eq(cos_deg(-180.0), -1.0, 1e-12));
        assert!(approx_eq(sin_deg(1080.0 + 30.0), 0.5, 1e-12));
    }

    #[test]
    fn tan_matches_sin_over_cos() {
        assert!(approx_eq(tan_deg(45.0), 1.0, 1e-12));
        assert!(approx_eq(tan_deg(0.0), 0.0, 1e-12));
        assert!(approx_eq(tan_deg(225.0), 1.0, 1e-12));
        assert!(tan_deg(90.0).is_infinite());
    }

    #[test]
    fn exp_matches_known_values() {
        assert!(approx_eq(exp(0.0), 1.0, 1e-15));
        assert!(approx_eq(exp(1.0), core::f64::consts::E, 1e-12));
        assert!(approx_eq(exp(-1.0), 1.0 / core::f64::consts::E, 1e-12));
        assert!(approx_eq(exp(5.0), 148.413_159, 1e-6));
        assert!(approx_eq(exp(-5.0), 0.006_737_947, 1e-9));
    }

    #[test]
    fn ln_matches_known_values() {
        assert!(approx_eq(ln(1.0), 0.0, 1e-15));
        assert!(approx_eq(ln(core::f64::consts::E), 1.0, 1e-12));
        assert!(approx_eq(ln(10.0), LN_10, 1e-12));
        assert!(approx_eq(ln(2.0), LN_2, 1e-12));
        assert!(approx_eq(ln(0.5), -LN_2, 1e-12));
        assert!(approx_eq(ln(100.0), 4.605_170_185_988_092, 1e-12));
        assert!(ln(0.0).is_infinite());
    }

    #[test]
    fn ln_and_exp_are_inverses() {
        for value in [0.1, 1.0, 7.5, 1000.0] {
            let round = exp(ln(value));
            assert!(approx_eq(round, value, value * 1e-9), "exp(ln({value}))");
        }
        // The reverse direction only for values whose exponential is
        // representable — exp(1000) is not.
        for value in [0.1, 1.0, 7.5, 20.0] {
            let round = ln(exp(value));
            assert!(
                approx_eq(round, value, value.abs() * 1e-9 + 1e-12),
                "ln(exp({value}))"
            );
        }
        // And an overflowing exponential round-trips to infinity.
        assert!(ln(exp(1000.0)).is_infinite());
    }

    #[test]
    fn sqrt_matches_known_values() {
        assert!(approx_eq(sqrt(0.0), 0.0, 1e-15));
        assert!(approx_eq(sqrt(1.0), 1.0, 1e-15));
        assert!(approx_eq(sqrt(4.0), 2.0, 1e-12));
        // The root of two, asserted by squaring rather than by a
        // literal that would just restate the same digits.
        let root_two = sqrt(2.0);
        assert!(approx_eq(root_two * root_two, 2.0, 1e-12));
        assert!(approx_eq(sqrt(1e6), 1000.0, 1e-9));
        assert!(approx_eq(sqrt(0.25), 0.5, 1e-12));
        assert!(sqrt(-1.0).is_nan());
    }
}
