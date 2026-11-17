//! How loud someone is, as numbers a frontend can draw.
//!
//! The audio engine reports smoothed RMS (0..1, after the speaker's own input
//! gain). People hear loudness logarithmically, so every meter and speaking
//! indicator works in dBFS:
//!
//! * [`meter`] / [`meter_to_rms`]: a wide scale for settings meters, wide
//!   enough to place the voice-activity threshold on it.
//! * [`intensity`]: the speech range only, for speaking indicators. A
//!   frontend should always show *something* while someone speaks (even at
//!   intensity 0) and scale up from there.
//!
//! Remote speakers are measured on the decoded audio, i.e. after their own
//! input gain, so everyone's indicator follows how loud they actually come
//! through (what their settings make of their voice).

/// Left edge of a settings meter.
pub const METER_FLOOR_DB: f32 = -60.0;
/// Right edge of a settings meter.
pub const METER_CEIL_DB: f32 = -6.0;
/// Quiet speech: a speaking indicator at its minimum.
pub const TALK_FLOOR_DB: f32 = -45.0;
/// Loud speech: a speaking indicator at its maximum.
pub const TALK_CEIL_DB: f32 = -14.0;

/// RMS (0..1) to dBFS, floored at -120.
pub fn db(rms: f32) -> f32 {
    20.0 * rms.max(1e-6).log10()
}

fn span(rms: f32, floor: f32, ceil: f32) -> f32 {
    ((db(rms) - floor) / (ceil - floor)).clamp(0.0, 1.0)
}

/// Position (0..1) of `rms` on a settings meter.
pub fn meter(rms: f32) -> f32 {
    span(rms, METER_FLOOR_DB, METER_CEIL_DB)
}

/// Inverse of [`meter`]: the RMS at meter position `pos` (0..1).
pub fn meter_to_rms(pos: f32) -> f32 {
    let d = METER_FLOOR_DB + pos.clamp(0.0, 1.0) * (METER_CEIL_DB - METER_FLOOR_DB);
    10f32.powf(d / 20.0)
}

/// How strongly to show someone talking (0..1) at `rms`.
pub fn intensity(rms: f32) -> f32 {
    span(rms, TALK_FLOOR_DB, TALK_CEIL_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_round_trips_and_clamps() {
        for pos in [0.0, 0.25, 0.5, 0.9, 1.0] {
            assert!((meter(meter_to_rms(pos)) - pos).abs() < 1e-4, "{pos}");
        }
        assert_eq!(meter(0.0), 0.0);
        assert_eq!(meter(1.0), 1.0);
    }

    #[test]
    fn intensity_spans_speech() {
        assert_eq!(intensity(0.001), 0.0, "below the floor");
        let quiet = intensity(0.01);
        let normal = intensity(0.05);
        let loud = intensity(0.2);
        assert!(0.0 < quiet && quiet < normal && normal < loud && loud <= 1.0);
        assert_eq!(intensity(0.5), 1.0);
    }
}
