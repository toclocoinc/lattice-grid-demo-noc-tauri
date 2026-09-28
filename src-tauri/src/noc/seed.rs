//! A deterministic generator seeded from a string, so a run repeats.
//!
//! A straight port of `seeded()` in the JavaScript demo: FNV-1a over the
//! UTF-16 code units of the seed, then a mulberry32 step per draw, so the
//! Rust wall invents the same availability figures and circuit loads as the
//! browser one did.

pub struct Seeded {
    state: u32,
}

impl Seeded {
    pub fn new(text: &str) -> Self {
        let mut hash: u32 = 2_166_136_261;
        for unit in text.encode_utf16() {
            hash ^= u32::from(unit);
            hash = hash.wrapping_mul(16_777_619);
        }
        Self { state: hash }
    }

    /// The next draw, uniform on `[0, 1)`.
    pub fn next(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut value = self.state;
        value = (value ^ (value >> 15)).wrapping_mul(value | 1);
        value ^= value.wrapping_add((value ^ (value >> 7)).wrapping_mul(value | 61));
        f64::from(value ^ (value >> 14)) / 4_294_967_296.0
    }
}

/// `Math.round(value * 10 ** places) / 10 ** places`, as the page rounds.
pub fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}

pub fn clamp(value: f64, low: f64, high: f64) -> f64 {
    value.max(low).min(high)
}
