//! Voice codecs: one 20 ms mono frame at 48 kHz in, one datagram payload out, and back.
//!
//! * [`Codec::Opus`] (feature `opus`): libopus in VOIP mode with in-band FEC, so a single
//!   lost frame can be rebuilt from the packet that follows it ([`VoiceDecoder::recover`]).
//! * [`Codec::Pcm16`]: always available fallback. The 48 kHz frame is decimated to 16 kHz
//!   by averaging groups of 3 samples (a cheap box low-pass) and sent as 320 little-endian
//!   `i16`s (640 bytes). The decoder linearly interpolates back to 48 kHz.
//!
//! Every decoder call appends exactly [`FRAME_SAMPLES`] samples at 48 kHz, whether it
//! decodes a payload or conceals a lost one, so jitter-buffer arithmetic stays in frames.

use anyhow::{Result, ensure};
use denchat_proto::voice::{Codec, FRAME_SAMPLES};

/// PCM16 wire sample rate.
pub const PCM_RATE: u32 = 16_000;
/// 48 kHz samples per 16 kHz PCM16 sample.
const PCM_DECIMATION: usize = (denchat_proto::voice::SAMPLE_RATE / PCM_RATE) as usize;
/// 16 kHz samples in one 20 ms PCM16 frame.
pub const PCM_FRAME_SAMPLES: usize = FRAME_SAMPLES / PCM_DECIMATION;

/// Encodes 20 ms mono frames at 48 kHz.
pub trait VoiceEncoder: Send {
    fn codec(&self) -> Codec;
    /// Encode exactly [`FRAME_SAMPLES`] samples in -1..1 into one payload
    /// (at most [`denchat_proto::voice::MAX_PAYLOAD`] bytes).
    fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>>;
    /// Change the target bitrate (bits per second). No-op for fixed-rate codecs.
    fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
        Ok(())
    }
}

/// Decodes payloads back to 20 ms mono frames at 48 kHz.
pub trait VoiceDecoder: Send {
    fn codec(&self) -> Codec;
    /// Append exactly [`FRAME_SAMPLES`] samples to `out`: the decoded `payload`, or a
    /// concealment frame when `payload` is `None` (lost packet). On error `out` is unchanged.
    fn decode(&mut self, payload: Option<&[u8]>, out: &mut Vec<f32>) -> Result<()>;
    /// Conceal the frame lost immediately before `next`, using redundancy carried in `next`
    /// when the codec has any (Opus in-band FEC). Defaults to plain concealment.
    fn recover(&mut self, next: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let _ = next;
        self.decode(None, out)
    }
    /// Forget stream state, e.g. when a new talkspurt does not continue the old one.
    fn reset(&mut self) {}
}

/// Codecs this build can encode and decode, best first.
pub fn supported_codecs() -> Vec<Codec> {
    if cfg!(feature = "opus") { vec![Codec::Opus, Codec::Pcm16] } else { vec![Codec::Pcm16] }
}

/// Whether this build can encode and decode `codec`.
pub fn is_supported(codec: Codec) -> bool {
    match codec {
        Codec::Pcm16 => true,
        Codec::Opus => cfg!(feature = "opus"),
    }
}

/// Create an encoder. `bitrate` is in bits per second and only used by Opus.
pub fn encoder(codec: Codec, bitrate: u32) -> Result<Box<dyn VoiceEncoder>> {
    match codec {
        Codec::Pcm16 => Ok(Box::new(PcmEncoder)),
        #[cfg(feature = "opus")]
        Codec::Opus => Ok(Box::new(opus_codec::OpusEncoder::new(bitrate)?)),
        #[cfg(not(feature = "opus"))]
        Codec::Opus => {
            let _ = bitrate;
            anyhow::bail!("this build has no Opus support")
        }
    }
}

/// Create a decoder.
pub fn decoder(codec: Codec) -> Result<Box<dyn VoiceDecoder>> {
    match codec {
        Codec::Pcm16 => Ok(Box::new(PcmDecoder::default())),
        #[cfg(feature = "opus")]
        Codec::Opus => Ok(Box::new(opus_codec::OpusDecoder::new()?)),
        #[cfg(not(feature = "opus"))]
        Codec::Opus => anyhow::bail!("this build has no Opus support"),
    }
}

fn check_frame(frame: &[f32]) -> Result<()> {
    ensure!(frame.len() == FRAME_SAMPLES, "encoder expects {FRAME_SAMPLES} samples per frame, got {}", frame.len());
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// PCM16

/// 48 kHz f32 → 16 kHz i16 LE. Stateless.
#[derive(Debug, Default)]
pub struct PcmEncoder;

impl VoiceEncoder for PcmEncoder {
    fn codec(&self) -> Codec {
        Codec::Pcm16
    }

    fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>> {
        check_frame(frame)?;
        let mut out = Vec::with_capacity(PCM_FRAME_SAMPLES * 2);
        for group in frame.as_chunks::<PCM_DECIMATION>().0 {
            let avg = group.iter().sum::<f32>() / PCM_DECIMATION as f32;
            let s = (avg.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        Ok(out)
    }
}

/// 16 kHz i16 LE → 48 kHz f32 with linear interpolation across frame boundaries.
///
/// Output lags the input by one 16 kHz sample (~62 µs) so every output sample can be
/// interpolated between two known input samples without waiting for the next frame.
#[derive(Debug, Default)]
pub struct PcmDecoder {
    /// Last 16 kHz sample of the previous frame (interpolation start point).
    prev: f32,
    /// Last 48 kHz sample emitted (fade-out start point for concealment).
    last_out: f32,
}

/// Concealment fades from the last output sample to silence over this many samples (5 ms).
const PCM_CONCEAL_FADE: usize = 240;

impl VoiceDecoder for PcmDecoder {
    fn codec(&self) -> Codec {
        Codec::Pcm16
    }

    fn decode(&mut self, payload: Option<&[u8]>, out: &mut Vec<f32>) -> Result<()> {
        let Some(payload) = payload else {
            // Fade out rather than repeat: repeating a 20 ms frame buzzes at 50 Hz.
            let start = self.last_out;
            out.extend(
                (0..FRAME_SAMPLES).map(
                    |i| {
                        if i < PCM_CONCEAL_FADE { start * (1.0 - (i + 1) as f32 / PCM_CONCEAL_FADE as f32) } else { 0.0 }
                    },
                ),
            );
            self.prev = 0.0;
            self.last_out = 0.0;
            return Ok(());
        };
        ensure!(!payload.is_empty() && payload.len() % 2 == 0, "bad PCM16 payload length {}", payload.len());

        // Virtual 16 kHz sequence: v[0] = previous frame's last sample, v[1..] = this frame
        // (short payloads are zero-padded, long ones truncated to one frame).
        let sample =
            |i: usize| -> f32 { payload.get(i * 2..i * 2 + 2).map_or(0.0, |b| i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32) };
        out.reserve(FRAME_SAMPLES);
        let mut a = self.prev;
        for i in 0..PCM_FRAME_SAMPLES {
            let b = sample(i);
            for k in 0..PCM_DECIMATION {
                out.push(a + (b - a) * (k as f32 / PCM_DECIMATION as f32));
            }
            a = b;
        }
        self.prev = a;
        self.last_out = out.last().copied().unwrap_or(0.0);
        Ok(())
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------------------------
// Opus

#[cfg(feature = "opus")]
mod opus_codec {
    use anyhow::{Context, Result};
    use denchat_proto::voice::{Codec, FRAME_SAMPLES, MAX_PAYLOAD, SAMPLE_RATE};
    use opus::{Application, Bitrate, Channels, Signal};

    use super::{VoiceDecoder, VoiceEncoder, check_frame};

    /// Largest Opus packet duration (120 ms) at 48 kHz, so oversized foreign packets decode.
    const MAX_OPUS_SAMPLES: usize = 5760;

    pub struct OpusEncoder {
        enc: opus::Encoder,
        buf: Vec<u8>,
    }

    impl OpusEncoder {
        pub fn new(bitrate: u32) -> Result<Self> {
            let mut enc = opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).context("creating Opus encoder")?;
            enc.set_signal(Signal::Voice)?;
            enc.set_inband_fec(true)?;
            // Tells the encoder how much redundancy is worth spending on FEC.
            enc.set_packet_loss_perc(10)?;
            let mut this = Self { enc, buf: vec![0; MAX_PAYLOAD] };
            this.set_bitrate(bitrate)?;
            Ok(this)
        }
    }

    impl VoiceEncoder for OpusEncoder {
        fn codec(&self) -> Codec {
            Codec::Opus
        }

        fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>> {
            check_frame(frame)?;
            let n = self.enc.encode_float(frame, &mut self.buf)?;
            Ok(self.buf[..n].to_vec())
        }

        fn set_bitrate(&mut self, bps: u32) -> Result<()> {
            let bps = bps.clamp(6_000, 256_000) as i32;
            self.enc.set_bitrate(Bitrate::Bits(bps))?;
            Ok(())
        }
    }

    pub struct OpusDecoder {
        dec: opus::Decoder,
        scratch: Vec<f32>,
    }

    impl OpusDecoder {
        pub fn new() -> Result<Self> {
            Ok(Self {
                dec: opus::Decoder::new(SAMPLE_RATE, Channels::Mono).context("creating Opus decoder")?,
                scratch: vec![0.0; MAX_OPUS_SAMPLES],
            })
        }

        /// Append `n` decoded samples from scratch, padded/truncated to one frame.
        fn emit(&self, n: usize, out: &mut Vec<f32>) {
            let n = n.min(FRAME_SAMPLES);
            out.extend_from_slice(&self.scratch[..n]);
            out.resize(out.len() + (FRAME_SAMPLES - n), 0.0);
        }
    }

    impl VoiceDecoder for OpusDecoder {
        fn codec(&self) -> Codec {
            Codec::Opus
        }

        fn decode(&mut self, payload: Option<&[u8]>, out: &mut Vec<f32>) -> Result<()> {
            let n = match payload {
                Some(p) => {
                    anyhow::ensure!(!p.is_empty(), "empty Opus payload");
                    self.dec.decode_float(p, &mut self.scratch, false)?
                }
                // Empty input = packet-loss concealment for the size of the output buffer.
                None => self.dec.decode_float(&[], &mut self.scratch[..FRAME_SAMPLES], false)?,
            };
            self.emit(n, out);
            Ok(())
        }

        fn recover(&mut self, next: &[u8], out: &mut Vec<f32>) -> Result<()> {
            if next.is_empty() {
                return self.decode(None, out);
            }
            let n = self.dec.decode_float(next, &mut self.scratch[..FRAME_SAMPLES], true)?;
            self.emit(n, out);
            Ok(())
        }

        fn reset(&mut self) {
            let _ = self.dec.reset_state();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denchat_proto::voice::{MAX_PAYLOAD, SAMPLE_RATE};

    fn sine(freq: f32, amp: f32, frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Roundtrip `input` through a codec, returning the concatenated decoded audio.
    fn roundtrip(codec: Codec, input: &[f32]) -> Vec<f32> {
        let mut enc = encoder(codec, 32_000).unwrap();
        let mut dec = decoder(codec).unwrap();
        let mut out = Vec::new();
        for frame in input.as_chunks::<FRAME_SAMPLES>().0 {
            let payload = enc.encode(frame).unwrap();
            assert!(payload.len() <= MAX_PAYLOAD);
            let before = out.len();
            dec.decode(Some(&payload), &mut out).unwrap();
            assert_eq!(out.len() - before, FRAME_SAMPLES);
        }
        out
    }

    /// Best normalized error between `a` and `b` delayed by 0..max_delay samples.
    fn aligned_error(a: &[f32], b: &[f32], skip: usize, max_delay: usize) -> f32 {
        (0..max_delay)
            .map(|d| {
                let n = a.len() - skip - max_delay;
                let err: Vec<f32> = (skip..skip + n).map(|i| a[i] - b[i + d]).collect();
                rms(&err) / rms(&a[skip..skip + n])
            })
            .fold(f32::INFINITY, f32::min)
    }

    #[test]
    fn pcm_roundtrip_preserves_sine() {
        let input = sine(440.0, 0.5, 10);
        let out = roundtrip(Codec::Pcm16, &input);
        assert_eq!(out.len(), input.len());
        let err = aligned_error(&input, &out, FRAME_SAMPLES, 8);
        assert!(err < 0.02, "relative error {err}");
    }

    #[test]
    fn pcm_payload_size_and_clamping() {
        let mut enc = PcmEncoder;
        let loud = vec![2.0; FRAME_SAMPLES];
        let p = enc.encode(&loud).unwrap();
        assert_eq!(p.len(), PCM_FRAME_SAMPLES * 2);
        assert_eq!(i16::from_le_bytes([p[0], p[1]]), i16::MAX);
        assert!(enc.encode(&loud[..100]).is_err());
    }

    #[test]
    fn pcm_concealment_fades_to_silence() {
        let mut dec = PcmDecoder::default();
        let mut enc = PcmEncoder;
        let mut out = Vec::new();
        dec.decode(Some(&enc.encode(&[0.5; FRAME_SAMPLES]).unwrap()), &mut out).unwrap();
        out.clear();
        dec.decode(None, &mut out).unwrap();
        assert_eq!(out.len(), FRAME_SAMPLES);
        assert!(out[0] > 0.4 && out[0] <= 0.5);
        assert!(out[PCM_CONCEAL_FADE..].iter().all(|&s| s == 0.0));
        assert!(dec.decode(Some(&[1, 2, 3]), &mut out).is_err());
        assert_eq!(out.len(), FRAME_SAMPLES, "failed decode leaves out unchanged");
    }

    #[test]
    fn supported_list_matches_features() {
        let list = supported_codecs();
        assert_eq!(list.last(), Some(&Codec::Pcm16));
        assert_eq!(list.contains(&Codec::Opus), cfg!(feature = "opus"));
        assert_eq!(encoder(Codec::Opus, 32_000).is_ok(), cfg!(feature = "opus"));
    }

    #[cfg(feature = "opus")]
    #[test]
    fn opus_roundtrip_preserves_sine() {
        let input = sine(440.0, 0.5, 50);
        let out = roundtrip(Codec::Opus, &input);
        assert_eq!(out.len(), input.len());
        // Skip the first 200 ms (encoder warm-up); Opus has a fixed algorithmic delay.
        let skip = FRAME_SAMPLES * 10;
        let err = aligned_error(&input, &out, skip, 600);
        assert!(err < 0.1, "relative error {err}");
        let level = rms(&out[skip..]) / rms(&input[skip..]);
        assert!((0.7..1.3).contains(&level), "level ratio {level}");
    }

    #[cfg(feature = "opus")]
    #[test]
    fn opus_concealment_and_fec_produce_full_frames() {
        let input = sine(300.0, 0.3, 4);
        let mut enc = encoder(Codec::Opus, 32_000).unwrap();
        let mut dec = decoder(Codec::Opus).unwrap();
        let packets: Vec<Vec<u8>> = input.as_chunks::<FRAME_SAMPLES>().0.iter().map(|f| enc.encode(f).unwrap()).collect();
        let mut out = Vec::new();
        dec.decode(Some(&packets[0]), &mut out).unwrap();
        dec.decode(None, &mut out).unwrap();
        dec.recover(&packets[3], &mut out).unwrap();
        dec.decode(Some(&packets[3]), &mut out).unwrap();
        assert_eq!(out.len(), 4 * FRAME_SAMPLES);
        assert!(dec.decode(Some(&[]), &mut out).is_err());
        assert_eq!(out.len(), 4 * FRAME_SAMPLES);
    }
}
