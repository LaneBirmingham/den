//! [`VoiceEngine`]: owns the io and encoder threads and the shared receive-side state.
//!
//! Live controls are atomics read once per 20 ms frame (transmit) or per callback
//! (playback), so every setter is lock-free and callable from any thread.

use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver as ChanRx, RecvTimeoutError, Sender as ChanTx};
use parking_lot::Mutex;
use tailchat_proto::voice::{Codec, FRAME_SAMPLES, MAX_PAYLOAD, SAMPLE_RATE};

use crate::codec::{self, VoiceEncoder};
use crate::device::{self, IoRequest};
use crate::gate::Gate;
use crate::mixer::{Ingest, Mixer, Receiver, rms, smooth_level};
use crate::resample::Resampler;
use crate::{PacketSink, TxMode, VoiceSettings, VoiceStats};

/// How long [`VoiceEngine::start`] waits for the devices to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Capture chunks buffered between the input callback and the encoder thread.
const CAPTURE_QUEUE: usize = 64;
/// Encoder thread wake-up interval when no audio arrives (to notice stop and decay meters).
const POLL: Duration = Duration::from_millis(100);

/// `f32` stored as bits in an `AtomicU32`.
#[derive(Debug)]
pub(crate) struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed)
    }
}

fn mode_to_u8(m: TxMode) -> u8 {
    match m {
        TxMode::VoiceActivity => 0,
        TxMode::PushToTalk => 1,
        TxMode::Open => 2,
    }
}

fn mode_from_u8(v: u8) -> TxMode {
    match v {
        1 => TxMode::PushToTalk,
        2 => TxMode::Open,
        _ => TxMode::VoiceActivity,
    }
}

/// Settings the user can change while the engine runs.
pub(crate) struct Controls {
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub ptt: AtomicBool,
    mode: AtomicU8,
    pub vad_threshold: AtomicF32,
    pub input_gain: AtomicF32,
    pub output_volume: AtomicF32,
    pub bitrate: AtomicU32,
    /// Requested transmit codec ([`Codec::to_byte`]).
    pub tx_codec: AtomicU8,
}

impl Controls {
    pub fn mode(&self) -> TxMode {
        mode_from_u8(self.mode.load(Relaxed))
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Status {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub last_error: Option<String>,
}

/// State shared by the engine handle, the encoder thread and the cpal callbacks.
pub(crate) struct Shared {
    pub controls: Controls,
    pub receiver: Mutex<Receiver>,
    pub mixer: Mutex<Mixer>,
    pub input_level: AtomicF32,
    pub transmitting: AtomicBool,
    /// Codec the encoder is actually using (0 = none yet).
    pub active_codec: AtomicU8,
    pub stop: AtomicBool,
    pub status: Mutex<Status>,
    warned_unsupported: AtomicBool,
    /// Packets handed to the sink / fed to [`VoiceEngine::receive`].
    pub tx_packets: AtomicU64,
    pub rx_packets: AtomicU64,
}

impl Shared {
    pub fn new(s: &VoiceSettings) -> Self {
        let best = codec::supported_codecs()[0];
        Self {
            controls: Controls {
                muted: AtomicBool::new(false),
                deafened: AtomicBool::new(false),
                ptt: AtomicBool::new(false),
                mode: AtomicU8::new(mode_to_u8(s.mode)),
                vad_threshold: AtomicF32::new(s.vad_threshold.clamp(0.0, 1.0)),
                input_gain: AtomicF32::new(s.input_gain.clamp(0.0, 16.0)),
                output_volume: AtomicF32::new(s.output_volume.clamp(0.0, 4.0)),
                bitrate: AtomicU32::new(s.bitrate),
                tx_codec: AtomicU8::new(best.to_byte()),
            },
            receiver: Mutex::new(Receiver::new()),
            mixer: Mutex::new(Mixer::new()),
            input_level: AtomicF32::new(0.0),
            transmitting: AtomicBool::new(false),
            active_codec: AtomicU8::new(0),
            stop: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
            warned_unsupported: AtomicBool::new(false),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
        }
    }

    /// Log a problem and surface it in [`VoiceStats::last_error`].
    pub fn report_error(&self, msg: String) {
        tracing::warn!("{msg}");
        self.status.lock().last_error = Some(msg);
    }
}

/// The running voice engine. Dropping it stops the streams and joins its threads.
///
/// `Send + Sync`: every method takes `&self`, so it can live in an `Arc` and be called from
/// UI, network and control threads at once.
pub struct VoiceEngine {
    shared: Arc<Shared>,
    has_output: bool,
    stop_tx: Option<ChanTx<()>>,
    io_thread: Option<JoinHandle<()>>,
    encoder_thread: Option<JoinHandle<()>>,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VoiceEngine>();
};

impl VoiceEngine {
    /// Open the input and output devices and start transmitting/receiving.
    ///
    /// If only one side opens, the engine runs listen-only (no input) or transmit-only
    /// (no output) and says so in [`VoiceStats::last_error`]. Fails only if neither opens.
    pub fn start(settings: VoiceSettings, sink: Arc<dyn PacketSink>) -> Result<VoiceEngine> {
        let shared = Arc::new(Shared::new(&settings));
        let (capture_tx, capture_rx) = crossbeam_channel::bounded::<Vec<f32>>(CAPTURE_QUEUE);
        let (recycle_tx, recycle_rx) = crossbeam_channel::bounded::<Vec<f32>>(CAPTURE_QUEUE);
        for _ in 0..8 {
            let _ = recycle_tx.try_send(Vec::with_capacity(4096));
        }
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

        let request = IoRequest {
            input_device: settings.input_device.clone(),
            output_device: settings.output_device.clone(),
            shared: shared.clone(),
            capture_tx,
            recycle_rx,
            ready_tx,
            stop_rx,
        };
        let io_thread = thread::Builder::new()
            .name("tailchat-audio-io".into())
            .spawn(move || device::run_io(request))
            .context("spawning audio io thread")?;

        // On timeout the io thread is left detached; it exits as soon as the backend returns
        // because `ready_rx` and `stop_tx` are gone.
        let opened =
            ready_rx.recv_timeout(OPEN_TIMEOUT).map_err(|_| anyhow::anyhow!("audio devices did not open within {OPEN_TIMEOUT:?}"))?;

        if let (Err(ie), Err(oe)) = (&opened.input, &opened.output) {
            let _ = io_thread.join();
            bail!("no usable audio devices (input: {ie}; output: {oe})");
        }

        {
            let mut st = shared.status.lock();
            st.last_error = opened.notes.last().cloned();
            match &opened.input {
                Ok(info) => {
                    tracing::info!("voice input: {} ({} Hz, {} ch, {})", info.name, info.sample_rate, info.channels, info.format);
                    st.input_device = Some(info.name.clone());
                }
                Err(e) => {
                    tracing::warn!("voice input unavailable, listen-only: {e}");
                    st.last_error = Some(format!("listen-only: {e}"));
                }
            }
            match &opened.output {
                Ok(info) => {
                    tracing::info!("voice output: {} ({} Hz, {} ch, {})", info.name, info.sample_rate, info.channels, info.format);
                    st.output_device = Some(info.name.clone());
                }
                Err(e) => {
                    tracing::warn!("voice output unavailable: {e}");
                    st.last_error = Some(format!("no audio output: {e}"));
                }
            }
        }

        let encoder_thread = match &opened.input {
            Ok(info) => {
                let ctx = EncoderCtx { shared: shared.clone(), sink, capture_rx, recycle_tx, input_rate: info.sample_rate };
                Some(
                    thread::Builder::new()
                        .name("tailchat-audio-enc".into())
                        .spawn(move || run_encoder(ctx))
                        .context("spawning voice encoder thread")?,
                )
            }
            Err(_) => None,
        };

        Ok(VoiceEngine { shared, has_output: opened.output.is_ok(), stop_tx: Some(stop_tx), io_thread: Some(io_thread), encoder_thread })
    }

    /// Feed one relayed packet. Called from a UDP thread; decodes on the caller's thread.
    pub fn receive(&self, speaker: u32, seq: u16, codec: Codec, payload: &[u8]) {
        self.shared.rx_packets.fetch_add(1, Relaxed);
        let now = Instant::now();
        if !self.has_output || self.shared.controls.deafened.load(Relaxed) {
            // Nothing will be played: skip decoding but keep speaking indicators alive.
            self.shared.mixer.lock().touch(speaker, now);
            return;
        }
        // Lock order: receiver, then mixer. The output callback only takes the mixer.
        let mut rx = self.shared.receiver.lock();
        match rx.ingest(speaker, seq, codec, payload, now) {
            Ingest::Frames { .. } => self.shared.mixer.lock().push(speaker, rx.samples(), now),
            Ingest::Late => {}
            Ingest::Unsupported => {
                if !self.shared.warned_unsupported.swap(true, Relaxed) {
                    self.shared.report_error(format!("received {codec:?} voice, which this build cannot decode"));
                }
            }
        }
    }

    /// Forget a speaker's decoder and queued audio (call when they leave).
    pub fn remove_speaker(&self, speaker: u32) {
        self.shared.receiver.lock().remove(speaker);
        self.shared.mixer.lock().remove(speaker);
    }

    /// Stop transmitting. Mic level is still metered.
    pub fn set_muted(&self, muted: bool) {
        self.shared.controls.muted.store(muted, Relaxed);
    }

    /// Silence playback and drop incoming audio. Does not imply mute.
    pub fn set_deafened(&self, deafened: bool) {
        if self.shared.controls.deafened.swap(deafened, Relaxed) != deafened {
            self.shared.mixer.lock().clear();
        }
    }

    pub fn set_ptt(&self, pressed: bool) {
        self.shared.controls.ptt.store(pressed, Relaxed);
    }

    pub fn set_mode(&self, mode: TxMode) {
        self.shared.controls.mode.store(mode_to_u8(mode), Relaxed);
    }

    /// VAD RMS threshold, clamped to 0..1.
    pub fn set_vad_threshold(&self, t: f32) {
        self.shared.controls.vad_threshold.store(t.clamp(0.0, 1.0));
    }

    /// Output gain, clamped to 0..4.
    pub fn set_output_volume(&self, v: f32) {
        self.shared.controls.output_volume.store(v.clamp(0.0, 4.0));
    }

    /// Mic gain, clamped to 0..16.
    pub fn set_input_gain(&self, g: f32) {
        self.shared.controls.input_gain.store(g.clamp(0.0, 16.0));
    }

    /// Opus target bitrate in bits per second (clamped by the encoder to 6–256 kbps).
    pub fn set_bitrate(&self, bps: u32) {
        self.shared.controls.bitrate.store(bps, Relaxed);
    }

    /// Choose the transmit codec. Falls back to PCM16 if this build lacks `codec`.
    pub fn set_tx_codec(&self, codec: Codec) {
        let codec = if codec::is_supported(codec) {
            codec
        } else {
            tracing::warn!("{codec:?} is not supported by this build; transmitting PCM16");
            Codec::Pcm16
        };
        self.shared.controls.tx_codec.store(codec.to_byte(), Relaxed);
    }

    pub fn stats(&self) -> VoiceStats {
        let st = self.shared.status.lock().clone();
        let speakers = self.shared.mixer.lock().stats(Instant::now());
        VoiceStats {
            input_level: self.shared.input_level.load(),
            transmitting: self.shared.transmitting.load(Relaxed),
            tx_codec: st.input_device.as_ref().and_then(|_| Codec::from_byte(self.shared.active_codec.load(Relaxed))),
            input_device: st.input_device,
            output_device: st.output_device,
            speakers,
            last_error: st.last_error,
            tx_packets: self.shared.tx_packets.load(Relaxed),
            rx_packets: self.shared.rx_packets.load(Relaxed),
        }
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        // Dropping the sender wakes the io thread, which drops the streams.
        self.stop_tx.take();
        let me = thread::current().id();
        for handle in [self.io_thread.take(), self.encoder_thread.take()].into_iter().flatten() {
            // A PacketSink could drop the last engine handle from the encoder thread itself.
            if handle.thread().id() != me {
                let _ = handle.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Encoder thread

struct EncoderCtx {
    shared: Arc<Shared>,
    sink: Arc<dyn PacketSink>,
    capture_rx: ChanRx<Vec<f32>>,
    recycle_tx: ChanTx<Vec<f32>>,
    input_rate: u32,
}

fn run_encoder(ctx: EncoderCtx) {
    let EncoderCtx { shared, sink, capture_rx, recycle_tx, input_rate } = ctx;
    let mut resampler = Resampler::new(input_rate, SAMPLE_RATE);
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 8);
    let mut tx = Transmitter::new(&shared);

    while !shared.stop.load(Relaxed) {
        match capture_rx.recv_timeout(POLL) {
            Ok(buf) => {
                resampler.process(&buf, &mut pcm);
                let _ = recycle_tx.try_send(buf);
            }
            Err(RecvTimeoutError::Timeout) => {
                // The input stream stalled (device unplugged, permission pending, ...).
                tx.idle(&shared);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let mut start = 0;
        while pcm.len() - start >= FRAME_SAMPLES {
            tx.process_frame(&mut pcm[start..start + FRAME_SAMPLES], &shared, sink.as_ref());
            start += FRAME_SAMPLES;
        }
        pcm.drain(..start);
    }
    shared.transmitting.store(false, Relaxed);
    shared.input_level.store(0.0);
}

/// Per-frame transmit logic: gain, metering, gate, codec management and sequence numbers.
pub(crate) struct Transmitter {
    gate: Gate,
    encoder: Option<Box<dyn VoiceEncoder>>,
    bitrate: u32,
    seq: u16,
    level: f32,
    /// Previous frame, sent ahead of the first frame when the gate opens so word onsets
    /// that were just under the VAD threshold are not clipped.
    preroll: Vec<f32>,
    have_preroll: bool,
    encode_errors: u32,
}

impl Transmitter {
    pub fn new(shared: &Shared) -> Self {
        let c = &shared.controls;
        Self {
            gate: Gate::new(c.mode(), c.vad_threshold.load()),
            encoder: None,
            bitrate: 0,
            // Random start so a restarted sender is unlikely to look like late packets.
            seq: RandomState::new().hash_one(Instant::now()) as u16,
            level: 0.0,
            preroll: vec![0.0; FRAME_SAMPLES],
            have_preroll: false,
            encode_errors: 0,
        }
    }

    /// No audio is arriving: close the gate and let the meter fall.
    pub fn idle(&mut self, shared: &Shared) {
        self.level = smooth_level(self.level, 0.0);
        self.have_preroll = false;
        shared.input_level.store(self.level);
        shared.transmitting.store(false, Relaxed);
    }

    /// Process one 48 kHz frame (modified in place by the input gain).
    pub fn process_frame(&mut self, frame: &mut [f32], shared: &Shared, sink: &dyn PacketSink) {
        let c = &shared.controls;
        let gain = c.input_gain.load();
        for s in frame.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
        let r = rms(frame);
        self.level = smooth_level(self.level, r);
        shared.input_level.store(self.level);

        self.gate.set_mode(c.mode());
        self.gate.set_threshold(c.vad_threshold.load());
        self.gate.set_ptt(c.ptt.load(Relaxed));
        self.gate.set_muted(c.muted.load(Relaxed));
        let was_open = self.gate.is_open();
        let open = self.gate.process(r);

        self.sync_encoder(shared);
        if open {
            if !was_open && self.have_preroll {
                let preroll = std::mem::take(&mut self.preroll);
                self.send(&preroll, shared, sink);
                self.preroll = preroll;
            }
            self.send(frame, shared, sink);
        }
        shared.transmitting.store(open, Relaxed);
        self.preroll.copy_from_slice(frame);
        self.have_preroll = true;
    }

    /// Rebuild the encoder if the requested codec changed; apply bitrate changes.
    fn sync_encoder(&mut self, shared: &Shared) {
        let c = &shared.controls;
        let want = Codec::from_byte(c.tx_codec.load(Relaxed)).unwrap_or(Codec::Pcm16);
        let bitrate = c.bitrate.load(Relaxed);
        match &mut self.encoder {
            Some(enc) if enc.codec() == want => {
                if bitrate != self.bitrate {
                    if let Err(e) = enc.set_bitrate(bitrate) {
                        tracing::warn!("setting bitrate {bitrate}: {e:#}");
                    }
                    self.bitrate = bitrate;
                }
                return;
            }
            _ => {}
        }
        let enc = codec::encoder(want, bitrate).unwrap_or_else(|e| {
            shared.report_error(format!("{want:?} encoder unavailable, using PCM16: {e:#}"));
            c.tx_codec.store(Codec::Pcm16.to_byte(), Relaxed);
            Box::new(codec::PcmEncoder)
        });
        tracing::debug!("voice transmit codec: {:?}", enc.codec());
        shared.active_codec.store(enc.codec().to_byte(), Relaxed);
        self.encoder = Some(enc);
        self.bitrate = bitrate;
    }

    fn send(&mut self, frame: &[f32], shared: &Shared, sink: &dyn PacketSink) {
        let Some(enc) = self.encoder.as_mut() else {
            return;
        };
        let result = enc.encode(frame).and_then(|p| {
            anyhow::ensure!(p.len() <= MAX_PAYLOAD, "encoded frame is {} bytes", p.len());
            Ok(p)
        });
        match result {
            Ok(payload) => {
                sink.send_voice(self.seq, enc.codec(), &payload);
                shared.tx_packets.fetch_add(1, Relaxed);
                self.seq = self.seq.wrapping_add(1);
            }
            Err(e) => {
                self.encode_errors += 1;
                if self.encode_errors == 1 || self.encode_errors.is_power_of_two() {
                    shared.report_error(format!("voice encode failed ({} times): {e:#}", self.encode_errors));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(u16, Codec, usize)>>);
    impl PacketSink for RecordingSink {
        fn send_voice(&self, seq: u16, codec: Codec, payload: &[u8]) {
            self.0.lock().push((seq, codec, payload.len()));
        }
    }

    fn frame(v: f32) -> Vec<f32> {
        vec![v; FRAME_SAMPLES]
    }

    #[test]
    fn transmitter_gates_preroll_and_counts_only_sent_frames() {
        let shared = Shared::new(&VoiceSettings::default());
        shared.controls.tx_codec.store(Codec::Pcm16.to_byte(), Relaxed);
        let sink = RecordingSink::default();
        let mut tx = Transmitter::new(&shared);

        // Quiet frames are not sent; the codec is still reported.
        for _ in 0..5 {
            tx.process_frame(&mut frame(0.001), &shared, &sink);
        }
        assert!(sink.0.lock().is_empty());
        assert!(!shared.transmitting.load(Relaxed));
        assert_eq!(Codec::from_byte(shared.active_codec.load(Relaxed)), Some(Codec::Pcm16));

        // Speech opens the gate: preroll frame + current frame, consecutive seqs.
        tx.process_frame(&mut frame(0.2), &shared, &sink);
        assert!(shared.transmitting.load(Relaxed));
        assert!(shared.input_level.load() > 0.15);
        {
            let sent = sink.0.lock();
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[1].0, sent[0].0.wrapping_add(1));
            assert!(sent.iter().all(|&(_, c, len)| c == Codec::Pcm16 && len == 640));
        }

        // Hangover then close; a later talkspurt continues the sequence without a gap.
        for _ in 0..30 {
            tx.process_frame(&mut frame(0.0), &shared, &sink);
        }
        assert!(!shared.transmitting.load(Relaxed));
        let before = sink.0.lock().len();
        assert_eq!(before, 2 + crate::gate::HANGOVER_FRAMES as usize);
        tx.process_frame(&mut frame(0.3), &shared, &sink);
        let sent = sink.0.lock();
        assert_eq!(sent.len(), before + 2);
        for w in sent.windows(2) {
            assert_eq!(w[1].0, w[0].0.wrapping_add(1), "seq must be contiguous");
        }
        assert_eq!(shared.tx_packets.load(Relaxed), sent.len() as u64, "tx counter matches what was sent");
    }

    #[test]
    fn transmitter_respects_mute_gain_and_codec_switch() {
        let shared = Shared::new(&VoiceSettings { mode: TxMode::Open, ..Default::default() });
        let sink = RecordingSink::default();
        let mut tx = Transmitter::new(&shared);

        shared.controls.muted.store(true, Relaxed);
        shared.controls.input_gain.store(2.0);
        tx.process_frame(&mut frame(0.1), &shared, &sink);
        assert!(sink.0.lock().is_empty());
        assert!((shared.input_level.load() - 0.2).abs() < 1e-4, "level is after gain, even muted");

        shared.controls.muted.store(false, Relaxed);
        tx.process_frame(&mut frame(0.1), &shared, &sink);
        let best = codec::supported_codecs()[0];
        assert_eq!(sink.0.lock().last().map(|p| p.1), Some(best));

        shared.controls.tx_codec.store(Codec::Pcm16.to_byte(), Relaxed);
        tx.process_frame(&mut frame(0.1), &shared, &sink);
        assert_eq!(sink.0.lock().last().map(|p| p.1), Some(Codec::Pcm16));
    }
}
