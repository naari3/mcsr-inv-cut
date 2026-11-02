//! Numeric conversions for pixel and time arithmetic.
//!
//! Each helper is used only within the range stated on it, which is why the lossy
//! cast inside is acceptable. Keeping them here confines the casts to one place.

/// Pixel counts and coordinates (frames are at most a few million pixels, far
/// below 2^24, so the conversion is exact).
#[expect(clippy::cast_precision_loss, reason = "pixel counts stay far below 2^24")]
pub fn f32_from(n: usize) -> f32 {
    n as f32
}

/// Ratio of two pixel counts; 0 when the denominator is 0.
pub fn ratio(num: usize, den: usize) -> f32 {
    if den == 0 { 0.0 } else { f32_from(num) / f32_from(den) }
}

/// Frame counts and indices (far below 2^52, so exact).
#[expect(clippy::cast_precision_loss, reason = "frame counts stay far below 2^52")]
pub fn f64_from(n: usize) -> f64 {
    n as f64
}

/// Screen-space coordinates computed in f64 (time axis) for painting.
#[expect(clippy::cast_possible_truncation, reason = "screen coordinates fit in f32")]
pub fn f32_from_f64(v: f64) -> f32 {
    v as f32
}

/// Non-negative float to an index or count. Negative and NaN become 0; float to
/// int casts saturate at the top.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to >= 0 and float-to-int casts saturate"
)]
pub fn to_usize(v: f64) -> usize {
    v.max(0.0) as usize
}

/// Like [`to_usize`] for `u32`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to >= 0 and float-to-int casts saturate"
)]
pub fn to_u32(v: f64) -> u32 {
    v.max(0.0) as u32
}

/// Like [`to_usize`] for `u64`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to >= 0 and float-to-int casts saturate"
)]
pub fn to_u64(v: f64) -> u64 {
    v.max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_int_clamps() {
        assert_eq!(to_usize(-3.0), 0);
        assert_eq!(to_usize(f64::NAN), 0);
        assert_eq!(to_usize(2.9), 2);
        assert_eq!(to_u32(1e20), u32::MAX);
        assert!(ratio(1, 0).abs() < f32::EPSILON);
    }
}
