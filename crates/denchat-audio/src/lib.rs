//! Voice engine for den: capture, encoding, jitter buffering, mixing and playback.
//!
//! The crate does no networking. The core crate owns the UDP socket: it hands every
//! encoded outgoing frame to the network through a [`PacketSink`], and feeds every
//! relayed incoming packet in through [`VoiceEngine::receive`].
//!
//! # Pipeline
//!
//! ```text
//! transmit:
//!   mic (cpal input callback, device rate, N channels)
//!     └─ downmix to mono f32 ──channel──▶ encoder thread
//!          resample device rate → 48 kHz        (resample::Resampler)
//!          cut 960-sample (20 ms) frames, apply input gain, measure RMS
//!          gate: VAD with 300 ms hangover / push-to-talk / open; mute closes it  (gate::Gate)
//!          encode (Opus VOIP or 16 kHz PCM16)   (codec)
//!          PacketSink::send_voice(seq, codec, payload)   seq counts sent frames only
//!
//! receive:
//!   VoiceEngine::receive(speaker, seq, codec, payload)   (any thread, usually UDP)
//!     └─ mixer::Receiver: per-speaker sequence tracking + decoder
//!          drop late/duplicate packets, conceal 1–4 lost frames, restart on big jumps
//!     └─ mixer::Mixer: per-speaker 48 kHz sample queues
//!          60 ms prebuffer before playout, 200 ms latency cap (drops oldest)
//!   speakers (cpal output callback)
//!     └─ Mixer::mix (sum + soft clip) → resample 48 kHz → device rate
//!          (resample::PullResampler) → volume → copy mono to every channel
//! ```
//!
//! # Threads
//!
//! * **io thread** (`den-audio-io`) builds the cpal streams, reports success back to
//!   [`VoiceEngine::start`], then parks until the engine is dropped. cpal streams are not
//!   `Sync` on every backend, so they never leave this thread and [`VoiceEngine`] stays
//!   `Send + Sync`.
//! * **encoder thread** (`den-audio-enc`) runs only when an input device opened. It
//!   does all transmit-side DSP and calls [`PacketSink::send_voice`].
//! * **cpal callback threads** (owned by the OS audio stack) do the minimum: downmix into
//!   a recycled buffer on input, and mix/resample/convert on output.
//! * [`VoiceEngine::receive`] decodes on the caller's thread, so decoding never runs in
//!   the output callback.
//! * **test device threads** (`den-audio-test`) stand in for the cpal callbacks when a
//!   device name starts with `test:` ([`testdev`]): a tone, silence or a WAV file in,
//!   nothing or a WAV recording out, at real-time pace, with no audio hardware needed.

pub mod codec;
mod device;
mod engine;
pub mod gate;
pub mod mixer;
pub mod resample;
pub mod testdev;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
pub use denchat_proto::voice::{Codec, FRAME_SAMPLES, SAMPLE_RATE};

pub use codec::supported_codecs;
pub use device::{DeviceList, list_devices};
pub use engine::VoiceEngine;
pub use testdev::{TEST_INPUTS, TEST_OUTPUTS, is_test_device};

/// How the transmit gate decides when to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TxMode {
    /// Send while the mic RMS is above [`VoiceSettings::vad_threshold`], plus a ~300 ms hangover.
    #[default]
    VoiceActivity,
    /// Send only while [`VoiceEngine::set_ptt`] is held.
    PushToTalk,
    /// Always send (unless muted).
    Open,
}

/// Settings used when starting a [`VoiceEngine`]. Most can be changed live via setters.
#[derive(Clone, Debug)]
pub struct VoiceSettings {
    /// Input device name as reported by [`list_devices`]; `None` = system default.
    /// A name that no longer exists falls back to the default (noted in `last_error`).
    /// `test:silence`, `test:sine:<hz>` or `test:file:<path.wav>` is a virtual device
    /// ([`testdev`]).
    pub input_device: Option<String>,
    /// Output device name as reported by [`list_devices`]; `None` = system default.
    /// `test:null` or `test:capture:<path.wav>` is a virtual device ([`testdev`]).
    pub output_device: Option<String>,
    pub mode: TxMode,
    /// RMS gate threshold in 0..1 for [`TxMode::VoiceActivity`].
    pub vad_threshold: f32,
    /// Opus bitrate in bits per second (ignored by PCM16).
    pub bitrate: u32,
    /// Linear gain applied to the mic before level metering, gating and encoding.
    pub input_gain: f32,
    /// Linear gain applied to the mixed output.
    pub output_volume: f32,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            mode: TxMode::VoiceActivity,
            vad_threshold: 0.015,
            bitrate: 32_000,
            input_gain: 1.0,
            output_volume: 1.0,
        }
    }
}

/// Receives encoded frames to transmit.
///
/// Called from the engine's encoder thread for each encoded frame to transmit. Must not
/// block for long: the encoder thread drops capture data while it is stalled.
pub trait PacketSink: Send + Sync + 'static {
    fn send_voice(&self, seq: u16, codec: Codec, payload: &[u8]);
}

/// Snapshot of the engine's state, cheap enough to poll every UI frame.
#[derive(Clone, Debug, Default)]
pub struct VoiceStats {
    /// Smoothed RMS (0..1) of the mic after input gain. Reported even while muted.
    pub input_level: f32,
    /// The gate is open and frames are being sent.
    pub transmitting: bool,
    /// Codec currently used for transmit; `None` when there is no input device.
    pub tx_codec: Option<Codec>,
    /// Input device name; `None` = no input (listen-only).
    pub input_device: Option<String>,
    /// Output device name; `None` = no output (transmit-only).
    pub output_device: Option<String>,
    /// Remote speakers currently known to the mixer, sorted by id.
    pub speakers: Vec<SpeakerStat>,
    /// Most recent problem worth showing the user (device fallback, stream error, ...).
    pub last_error: Option<String>,
    /// Voice packets sent since the engine started (only while the gate is open).
    pub tx_packets: u64,
    /// Relayed voice packets received since the engine started (counted even when deafened).
    pub rx_packets: u64,
}

/// Per-speaker playout state.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerStat {
    pub id: u32,
    /// Smoothed RMS (0..1) of the speaker's decoded audio; 0 when not speaking.
    pub level: f32,
    /// Audible audio (or, while deafened, any packet) was received within the last ~250 ms.
    /// Near-silent frames (e.g. from a peer in [`TxMode::Open`]) do not count.
    pub speaking: bool,
    /// Decoded audio queued for playout.
    pub buffered_ms: u32,
}

/// Speaker id used for the local echo in [`loopback_test`] and [`MicTest`].
pub const LOOPBACK_SPEAKER: u32 = u32::MAX;

/// A voice engine with no network, for settings screens: meters the mic
/// (through the real gate, so [`VoiceStats::transmitting`] shows whether you'd
/// be heard) and, with loopback on, plays what would be sent back to you.
pub struct MicTest {
    engine: Arc<VoiceEngine>,
    sink: Arc<EchoSink>,
}

struct EchoSink {
    engine: std::sync::OnceLock<std::sync::Weak<VoiceEngine>>,
    echo: std::sync::atomic::AtomicBool,
}

impl PacketSink for EchoSink {
    fn send_voice(&self, seq: u16, codec: Codec, payload: &[u8]) {
        if self.echo.load(std::sync::atomic::Ordering::Relaxed)
            && let Some(engine) = self.engine.get().and_then(std::sync::Weak::upgrade)
        {
            engine.receive(LOOPBACK_SPEAKER, seq, codec, payload);
        }
    }
}

impl MicTest {
    /// Open the devices in `settings`. Blocks while they open (up to ~10 s).
    pub fn start(settings: VoiceSettings) -> Result<MicTest> {
        let sink = Arc::new(EchoSink { engine: Default::default(), echo: Default::default() });
        let engine = Arc::new(VoiceEngine::start(settings, sink.clone())?);
        let _ = sink.engine.set(Arc::downgrade(&engine));
        Ok(MicTest { engine, sink })
    }

    pub fn engine(&self) -> &VoiceEngine {
        &self.engine
    }

    /// Hear yourself (use headphones: there is no echo cancellation).
    pub fn set_loopback(&self, on: bool) {
        let was = self.sink.echo.swap(on, std::sync::atomic::Ordering::Relaxed);
        if was && !on {
            self.engine.remove_speaker(LOOPBACK_SPEAKER);
        }
    }

    pub fn loopback(&self) -> bool {
        self.sink.echo.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Mic -> encode -> decode -> speakers locally for `duration` (echo test), returns stats at the end.
///
/// Uses `settings.mode` for gating, so pass [`TxMode::Open`] to hear everything. Use
/// headphones: there is no echo cancellation, so open speakers will feed back.
/// Fails if either the input or the output device cannot be opened.
pub fn loopback_test(settings: VoiceSettings, codec: Codec, duration: Duration) -> Result<VoiceStats> {
    struct ChannelSink(crossbeam_channel::Sender<(u16, Codec, Vec<u8>)>);
    impl PacketSink for ChannelSink {
        fn send_voice(&self, seq: u16, codec: Codec, payload: &[u8]) {
            let _ = self.0.try_send((seq, codec, payload.to_vec()));
        }
    }

    let (tx, rx) = crossbeam_channel::bounded(256);
    let engine = VoiceEngine::start(settings, Arc::new(ChannelSink(tx)))?;
    let stats = engine.stats();
    if stats.input_device.is_none() || stats.output_device.is_none() {
        bail!(
            "loopback test needs both an input and an output device: {}",
            stats.last_error.unwrap_or_else(|| "device unavailable".into())
        );
    }
    engine.set_tx_codec(codec);

    let deadline = Instant::now() + duration;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = (deadline - now).min(Duration::from_millis(50));
        match rx.recv_timeout(wait) {
            Ok((seq, codec, payload)) => engine.receive(LOOPBACK_SPEAKER, seq, codec, &payload),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(engine.stats())
}
