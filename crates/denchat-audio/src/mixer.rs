//! Receive side: per-speaker jitter handling, decoding and mixing. Device independent.
//!
//! Split in two so decoding never happens under the lock the output callback takes:
//!
//! * [`Receiver`] runs on the packet's thread. It tracks each speaker's sequence numbers
//!   (wrapping `u16`), drops late and duplicate packets, conceals small gaps (1–4 missing
//!   frames, the last one via codec FEC when available), and treats big jumps, sequence
//!   restarts, codec changes and gaps after a long silence as a fresh talkspurt. It decodes
//!   into a reusable buffer exposed by [`Receiver::samples`].
//! * [`Mixer`] owns per-speaker 48 kHz sample queues. A speaker starts playing once
//!   [`PREBUFFER_MS`] is queued (or nothing more arrives for [`FLUSH_AFTER`]), goes back to
//!   prebuffering after an underrun, and has old audio dropped whenever its queue exceeds
//!   [`MAX_LATENCY_MS`]. [`Mixer::mix`] sums active speakers with a soft clipper.
//!
//! Packets are decoded in arrival order; there is no reordering. A packet that arrives after
//! its slot was concealed is dropped as late.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use denchat_proto::voice::{Codec, FRAME_SAMPLES, SAMPLE_RATE};

use crate::SpeakerStat;
use crate::codec::{self, VoiceDecoder};

/// Queued audio required before a speaker starts (or restarts) playing.
pub const PREBUFFER_MS: u32 = 60;
/// Queue length above which the oldest audio is dropped.
pub const MAX_LATENCY_MS: u32 = 200;
/// Queue length kept after dropping for [`MAX_LATENCY_MS`].
pub const TRIM_TO_MS: u32 = 100;
/// Largest gap (in frames) that is concealed rather than treated as a new talkspurt.
pub const MAX_CONCEAL_FRAMES: u16 = 4;
/// Packets up to this many frames behind the expected sequence are late/duplicate and
/// dropped; further behind means the sender restarted its sequence.
pub const LATE_WINDOW: u16 = 32;
/// After this long without packets, a gap is not concealed (the decoder state is stale).
pub const IDLE_RESET: Duration = Duration::from_millis(500);
/// A speaker counts as speaking for this long after its last audible frame.
pub const SPEAKING_HOLD: Duration = Duration::from_millis(250);
/// Start playing a partial prebuffer when nothing more arrived for this long
/// (a very short talkspurt would otherwise never play).
pub const FLUSH_AFTER: Duration = Duration::from_millis(100);
/// Frames quieter than this RMS do not mark a speaker as speaking.
pub const SPEAKING_RMS: f32 = 0.005;

const SAMPLES_PER_MS: usize = (SAMPLE_RATE / 1000) as usize;

/// Convert milliseconds to 48 kHz samples.
pub const fn ms_to_samples(ms: u32) -> usize {
    ms as usize * SAMPLES_PER_MS
}

/// Peak-follow attack, exponential release; used for every level meter in the crate.
pub(crate) fn smooth_level(prev: f32, rms: f32) -> f32 {
    if rms >= prev { rms } else { prev * 0.8 + rms * 0.2 }
}

pub(crate) fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

/// Transparent below 0.8, then smoothly approaches ±1 (continuous slope at the knee).
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE { x } else { (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()).copysign(x) }
}

// ---------------------------------------------------------------------------------------------
// Receiver

/// What [`Receiver::ingest`] did with a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ingest {
    /// [`Receiver::samples`] holds `concealed` synthesized frames followed by the packet's
    /// own frame. `fresh` = this packet started a new talkspurt (no concealment attempted).
    Frames { concealed: usize, fresh: bool },
    /// Late or duplicate packet; dropped.
    Late,
    /// Codec not supported by this build; dropped.
    Unsupported,
}

struct RxSpeaker {
    decoder: Box<dyn VoiceDecoder>,
    next_seq: u16,
    last_rx: Instant,
}

/// Per-speaker sequence tracking and decoding.
#[derive(Default)]
pub struct Receiver {
    speakers: HashMap<u32, RxSpeaker>,
    out: Vec<f32>,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decoded audio from the last [`Ingest::Frames`] result.
    pub fn samples(&self) -> &[f32] {
        &self.out
    }

    pub fn remove(&mut self, speaker: u32) {
        self.speakers.remove(&speaker);
    }

    pub fn clear(&mut self) {
        self.speakers.clear();
    }

    /// Process one packet from `speaker`.
    pub fn ingest(&mut self, speaker: u32, seq: u16, codec: Codec, payload: &[u8], now: Instant) -> Ingest {
        self.out.clear();

        let fresh_decoder = match self.speakers.get(&speaker) {
            Some(s) if s.decoder.codec() == codec => None,
            _ => match codec::decoder(codec) {
                Ok(d) => Some(d),
                Err(_) => return Ingest::Unsupported,
            },
        };

        let (state, fresh) = match fresh_decoder {
            Some(decoder) => {
                let state = self.speakers.entry(speaker).insert_entry(RxSpeaker { decoder, next_seq: seq, last_rx: now });
                (state.into_mut(), true)
            }
            None => {
                let Some(state) = self.speakers.get_mut(&speaker) else {
                    return Ingest::Unsupported; // unreachable: checked above
                };
                (state, false)
            }
        };

        let mut concealed = 0;
        let mut fresh = fresh;
        if !fresh {
            // Signed distance from the expected sequence number, wrapping.
            let ahead = seq.wrapping_sub(state.next_seq);
            let behind = state.next_seq.wrapping_sub(seq);
            let idle = now.saturating_duration_since(state.last_rx) > IDLE_RESET;
            if ahead == 0 {
                // In order.
            } else if (1..=LATE_WINDOW).contains(&behind) && !idle {
                return Ingest::Late;
            } else if ahead <= MAX_CONCEAL_FRAMES && !idle {
                for k in 0..ahead {
                    let res = if k + 1 == ahead {
                        state.decoder.recover(payload, &mut self.out)
                    } else {
                        state.decoder.decode(None, &mut self.out)
                    };
                    if res.is_err() {
                        conceal(state.decoder.as_mut(), &mut self.out);
                    }
                }
                concealed = ahead as usize;
            } else {
                // Big jump forward, sequence restart, or a gap after a long silence.
                fresh = true;
                state.decoder.reset();
            }
        }

        if let Err(e) = state.decoder.decode(Some(payload), &mut self.out) {
            tracing::debug!(speaker, seq, "voice decode failed, concealing: {e:#}");
            conceal(state.decoder.as_mut(), &mut self.out);
        }
        state.next_seq = seq.wrapping_add(1);
        state.last_rx = now;
        Ingest::Frames { concealed, fresh }
    }
}

/// Concealment that cannot fail: falls back to a frame of silence.
fn conceal(decoder: &mut dyn VoiceDecoder, out: &mut Vec<f32>) {
    if decoder.decode(None, out).is_err() {
        out.resize(out.len() + FRAME_SAMPLES, 0.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Mixer

struct Playout {
    queue: VecDeque<f32>,
    playing: bool,
    last_push: Instant,
    last_voice: Option<Instant>,
    level: f32,
}

/// Per-speaker playout queues and the final mix.
#[derive(Default)]
pub struct Mixer {
    speakers: HashMap<u32, Playout>,
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    fn speaker(&mut self, id: u32, now: Instant) -> &mut Playout {
        self.speakers.entry(id).or_insert_with(|| Playout {
            queue: VecDeque::with_capacity(ms_to_samples(MAX_LATENCY_MS) + 2 * FRAME_SAMPLES),
            playing: false,
            last_push: now,
            last_voice: None,
            level: 0.0,
        })
    }

    /// Queue decoded 48 kHz audio for `speaker`.
    pub fn push(&mut self, speaker: u32, samples: &[f32], now: Instant) {
        let p = self.speaker(speaker, now);
        p.queue.extend(samples);
        p.last_push = now;
        for frame in samples.chunks(FRAME_SAMPLES) {
            let r = rms(frame);
            p.level = smooth_level(p.level, r);
            if r >= SPEAKING_RMS {
                p.last_voice = Some(now);
            }
        }
        if p.queue.len() > ms_to_samples(MAX_LATENCY_MS) {
            let excess = p.queue.len() - ms_to_samples(TRIM_TO_MS);
            p.queue.drain(..excess);
        }
    }

    /// Mark `speaker` as speaking without queueing audio (used while deafened).
    pub fn touch(&mut self, speaker: u32, now: Instant) {
        self.speaker(speaker, now).last_voice = Some(now);
    }

    /// Overwrite `out` with the mix of all playing speakers. Never allocates.
    pub fn mix(&mut self, out: &mut [f32], now: Instant) {
        out.fill(0.0);
        let prebuffer = ms_to_samples(PREBUFFER_MS);
        for p in self.speakers.values_mut() {
            if !p.playing {
                let stalled = now.saturating_duration_since(p.last_push) >= FLUSH_AFTER;
                p.playing = p.queue.len() >= prebuffer || (stalled && !p.queue.is_empty());
                if !p.playing {
                    continue;
                }
            }
            let n = out.len().min(p.queue.len());
            for (o, s) in out.iter_mut().zip(p.queue.drain(..n)) {
                *o += s;
            }
            if p.queue.is_empty() {
                // Underrun (or end of talkspurt): rebuild the prebuffer before resuming.
                p.playing = false;
            }
        }
        for s in out.iter_mut() {
            *s = soft_clip(*s);
        }
    }

    pub fn remove(&mut self, speaker: u32) {
        self.speakers.remove(&speaker);
    }

    /// Drop all queued audio (speakers stay known).
    pub fn clear(&mut self) {
        for p in self.speakers.values_mut() {
            p.queue.clear();
            p.playing = false;
        }
    }

    /// Samples queued for `speaker`, if known.
    pub fn buffered(&self, speaker: u32) -> Option<usize> {
        self.speakers.get(&speaker).map(|p| p.queue.len())
    }

    /// Whether `speaker` is past its prebuffer and playing.
    pub fn is_playing(&self, speaker: u32) -> bool {
        self.speakers.get(&speaker).is_some_and(|p| p.playing)
    }

    pub fn stats(&self, now: Instant) -> Vec<SpeakerStat> {
        let mut v: Vec<SpeakerStat> = self
            .speakers
            .iter()
            .map(|(&id, p)| {
                let speaking = p.last_voice.is_some_and(|t| now.saturating_duration_since(t) < SPEAKING_HOLD);
                SpeakerStat {
                    id,
                    level: if speaking { p.level } else { 0.0 },
                    speaking,
                    buffered_ms: (p.queue.len() / SAMPLES_PER_MS) as u32,
                }
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encoder;

    const FRAME: Duration = Duration::from_millis(20);

    /// A PCM16 packet whose samples all equal `value` (so frames are identifiable).
    fn packet(value: f32) -> Vec<u8> {
        encoder(Codec::Pcm16, 0).unwrap().encode(&[value; FRAME_SAMPLES]).unwrap()
    }

    struct Harness {
        rx: Receiver,
        mixer: Mixer,
        now: Instant,
    }

    impl Harness {
        fn new() -> Self {
            Self { rx: Receiver::new(), mixer: Mixer::new(), now: Instant::now() }
        }

        fn recv(&mut self, seq: u16, value: f32) -> Ingest {
            let res = self.rx.ingest(1, seq, Codec::Pcm16, &packet(value), self.now);
            if let Ingest::Frames { .. } = res {
                self.mixer.push(1, self.rx.samples(), self.now);
            }
            res
        }

        fn advance(&mut self, d: Duration) {
            self.now += d;
        }

        fn mix(&mut self, n: usize) -> Vec<f32> {
            let mut out = vec![0.0; n];
            self.mixer.mix(&mut out, self.now);
            out
        }

        fn buffered_frames(&self) -> usize {
            self.mixer.buffered(1).unwrap_or(0) / FRAME_SAMPLES
        }
    }

    #[test]
    fn prebuffers_60ms_before_playing() {
        let mut h = Harness::new();
        h.recv(100, 0.1);
        h.recv(101, 0.1);
        assert!(h.mix(FRAME_SAMPLES).iter().all(|&s| s == 0.0), "still prebuffering");
        assert_eq!(h.buffered_frames(), 2);
        h.recv(102, 0.1);
        let out = h.mix(FRAME_SAMPLES);
        assert!(h.mixer.is_playing(1));
        assert!((out[FRAME_SAMPLES / 2] - 0.1).abs() < 1e-3);
        assert_eq!(h.buffered_frames(), 2);

        // Drain completely → underrun → prebuffer again.
        h.mix(2 * FRAME_SAMPLES);
        assert!(!h.mixer.is_playing(1));
        h.recv(103, 0.1);
        assert!(h.mix(FRAME_SAMPLES).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn short_talkspurt_is_flushed_when_stream_stalls() {
        let mut h = Harness::new();
        h.recv(7, 0.2);
        assert!(h.mix(FRAME_SAMPLES).iter().all(|&s| s == 0.0));
        h.advance(FLUSH_AFTER);
        let out = h.mix(FRAME_SAMPLES);
        assert!(out.iter().skip(10).all(|&s| (s - 0.2).abs() < 1e-3));
    }

    #[test]
    fn conceals_small_gaps_with_wrapping_sequence() {
        let mut h = Harness::new();
        assert_eq!(h.recv(65534, 0.1), Ingest::Frames { concealed: 0, fresh: true });
        // 65535 and 0 are lost; 1 arrives.
        assert_eq!(h.recv(1, 0.1), Ingest::Frames { concealed: 2, fresh: false });
        assert_eq!(h.rx.samples().len(), 3 * FRAME_SAMPLES);
        assert_eq!(h.buffered_frames(), 4);
        // Exactly the maximum concealable gap.
        assert_eq!(h.recv(1 + MAX_CONCEAL_FRAMES + 1, 0.1), Ingest::Frames { concealed: MAX_CONCEAL_FRAMES as usize, fresh: false });
    }

    #[test]
    fn big_jump_or_restart_starts_new_talkspurt() {
        let mut h = Harness::new();
        h.recv(10, 0.1);
        let jump = 11 + MAX_CONCEAL_FRAMES + 1;
        assert_eq!(h.recv(jump, 0.1), Ingest::Frames { concealed: 0, fresh: true });
        // Far behind the expected sequence: the sender restarted.
        let restarted = jump.wrapping_sub(LATE_WINDOW + 5);
        assert_eq!(h.recv(restarted, 0.1), Ingest::Frames { concealed: 0, fresh: true });
    }

    #[test]
    fn gap_after_long_silence_is_not_concealed() {
        let mut h = Harness::new();
        h.recv(10, 0.1);
        h.advance(IDLE_RESET + FRAME);
        assert_eq!(h.recv(12, 0.1), Ingest::Frames { concealed: 0, fresh: true });
        // In-order packets after silence continue the stream normally.
        h.advance(IDLE_RESET + FRAME);
        assert_eq!(h.recv(13, 0.1), Ingest::Frames { concealed: 0, fresh: false });
    }

    #[test]
    fn drops_late_and_duplicate_packets() {
        let mut h = Harness::new();
        h.recv(500, 0.1);
        h.recv(502, 0.1); // 501 concealed
        assert_eq!(h.recv(501, 0.5), Ingest::Late, "arrived after its slot was concealed");
        assert_eq!(h.recv(502, 0.5), Ingest::Late, "duplicate");
        assert_eq!(h.buffered_frames(), 3);
        // Late packets never reached the queue.
        let out = h.mix(3 * FRAME_SAMPLES);
        assert!(out.iter().all(|&s| s < 0.2));
        assert_eq!(h.recv(503, 0.1), Ingest::Frames { concealed: 0, fresh: false });
    }

    #[test]
    fn latency_is_capped_by_dropping_oldest() {
        let mut h = Harness::new();
        let max_frames = (MAX_LATENCY_MS / 20) as u16;
        for i in 0..max_frames {
            h.recv(i, 0.1);
        }
        assert_eq!(h.mixer.buffered(1), Some(ms_to_samples(MAX_LATENCY_MS)));
        // One more frame (identifiable value) exceeds the cap.
        h.recv(max_frames, 0.9);
        let buffered = h.mixer.buffered(1).unwrap();
        assert_eq!(buffered, ms_to_samples(TRIM_TO_MS));
        assert!(buffered <= ms_to_samples(MAX_LATENCY_MS));
        // The newest audio survived: the tail of the queue is the 0.9 frame.
        let out = h.mix(buffered);
        assert!((out[buffered - 1] - soft_clip(0.9)).abs() < 1e-3);
        assert_eq!(h.mixer.stats(h.now)[0].buffered_ms, 0);
    }

    #[test]
    fn mix_sums_speakers_with_soft_clip() {
        let mut m = Mixer::new();
        let now = Instant::now();
        let loud = vec![0.7; ms_to_samples(PREBUFFER_MS)];
        m.push(1, &loud, now);
        m.push(2, &loud, now);
        let mut out = vec![0.0; 100];
        m.mix(&mut out, now);
        assert!(out.iter().all(|&s| s > 0.8 && s < 1.0), "{}", out[0]);
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(-10.0) >= -1.0);
    }

    #[test]
    fn stats_report_speaking_and_level() {
        let mut m = Mixer::new();
        let now = Instant::now();
        m.push(3, &[0.3; FRAME_SAMPLES], now);
        m.push(4, &[0.0; FRAME_SAMPLES], now);
        m.touch(5, now);
        let s = m.stats(now);
        assert_eq!(s.iter().map(|s| s.id).collect::<Vec<_>>(), [3, 4, 5]);
        assert!(s[0].speaking && s[0].level > 0.25);
        assert_eq!(s[0].buffered_ms, 20);
        assert!(!s[1].speaking, "silent frames are not speech");
        assert!(s[2].speaking);
        let later = m.stats(now + SPEAKING_HOLD);
        assert!(later.iter().all(|s| !s.speaking && s.level == 0.0));
        m.remove(3);
        assert_eq!(m.stats(now).len(), 2);
    }
}
