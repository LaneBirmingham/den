//! Transmit gate: decides, frame by frame, whether the mic is sent.
//!
//! * [`TxMode::VoiceActivity`]: open while the frame RMS is at or above the threshold, and
//!   for a [`VAD_HANGOVER_MS`] hangover afterwards so word endings and short pauses are not
//!   chopped.
//! * [`TxMode::PushToTalk`]: open exactly while the key is held.
//! * [`TxMode::Open`]: always open.
//!
//! Mute overrides every mode. The gate itself is pure state; the encoder thread feeds it the
//! RMS of each 20 ms frame and copies the live controls in before each call.

use denchat_proto::voice::FRAME_MS;

use crate::TxMode;

/// How long VAD stays open after the last frame above threshold.
pub const VAD_HANGOVER_MS: u32 = 300;
/// [`VAD_HANGOVER_MS`] in 20 ms frames.
pub const HANGOVER_FRAMES: u32 = VAD_HANGOVER_MS / FRAME_MS;

#[derive(Clone, Debug)]
pub struct Gate {
    mode: TxMode,
    threshold: f32,
    ptt: bool,
    muted: bool,
    hangover: u32,
    open: bool,
}

impl Gate {
    pub fn new(mode: TxMode, threshold: f32) -> Self {
        Self { mode, threshold, ptt: false, muted: false, hangover: 0, open: false }
    }

    pub fn set_mode(&mut self, mode: TxMode) {
        if mode != self.mode {
            self.mode = mode;
            self.hangover = 0;
        }
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    pub fn set_ptt(&mut self, pressed: bool) {
        self.ptt = pressed;
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Whether the last processed frame was let through.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Feed one frame's RMS; returns whether that frame should be sent.
    pub fn process(&mut self, rms: f32) -> bool {
        self.open = if self.muted {
            self.hangover = 0;
            false
        } else {
            match self.mode {
                TxMode::Open => true,
                TxMode::PushToTalk => self.ptt,
                TxMode::VoiceActivity => {
                    if rms >= self.threshold {
                        self.hangover = HANGOVER_FRAMES;
                        true
                    } else if self.hangover > 0 {
                        self.hangover -= 1;
                        true
                    } else {
                        false
                    }
                }
            }
        };
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vad_hangover_holds_for_300ms() {
        let mut g = Gate::new(TxMode::VoiceActivity, 0.1);
        assert!(!g.process(0.05));
        assert!(g.process(0.2));
        for i in 0..HANGOVER_FRAMES {
            assert!(g.process(0.0), "hangover frame {i} should stay open");
        }
        assert!(!g.process(0.0), "closes after {VAD_HANGOVER_MS} ms");
        assert_eq!(HANGOVER_FRAMES, 15);

        // Speech during hangover restarts it.
        assert!(g.process(0.5));
        for _ in 0..10 {
            assert!(g.process(0.0));
        }
        assert!(g.process(0.3));
        for _ in 0..HANGOVER_FRAMES {
            assert!(g.process(0.0));
        }
        assert!(!g.process(0.0));
    }

    #[test]
    fn mute_closes_every_mode_and_cancels_hangover() {
        let mut g = Gate::new(TxMode::VoiceActivity, 0.1);
        assert!(g.process(1.0));
        g.set_muted(true);
        assert!(!g.process(1.0));
        g.set_muted(false);
        assert!(!g.process(0.0), "hangover was cancelled by mute");

        g.set_mode(TxMode::Open);
        assert!(g.process(0.0));
        g.set_muted(true);
        assert!(!g.process(0.0));
        g.set_mode(TxMode::PushToTalk);
        g.set_ptt(true);
        assert!(!g.process(1.0));
    }

    #[test]
    fn push_to_talk_follows_key_without_hangover() {
        let mut g = Gate::new(TxMode::PushToTalk, 0.1);
        assert!(!g.process(1.0));
        g.set_ptt(true);
        assert!(g.process(0.0));
        assert!(g.is_open());
        g.set_ptt(false);
        assert!(!g.process(1.0));
    }
}
