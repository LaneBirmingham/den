//! cpal glue: device lookup, stream construction and the realtime callbacks.
//!
//! Streams are built with each device's default config (any rate, any channel count) and
//! the default buffer size. Supported sample formats: f32, f64, i8, i16, i24, i32, u8, u16,
//! u32. Callbacks never panic, never block on anything but the mixer's short critical
//! section, and only allocate while their scratch buffers grow to the callback size.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat};
use crossbeam_channel::{Receiver, Sender};
use denchat_proto::voice::SAMPLE_RATE;

use crate::engine::Shared;
use crate::resample::PullResampler;
use crate::testdev;

/// How long a backend may take to build a stream (not honoured by every backend).
const BUILD_TIMEOUT: Duration = Duration::from_secs(5);

/// Audio devices on the default host, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceList {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

/// Enumerate input and output devices on the default host.
pub fn list_devices() -> Result<DeviceList> {
    let host = cpal::default_host();
    let inputs = host.input_devices().context("listing input devices")?.filter_map(|d| device_name(&d)).collect();
    let outputs = host.output_devices().context("listing output devices")?.filter_map(|d| device_name(&d)).collect();
    Ok(DeviceList {
        inputs,
        outputs,
        default_input: host.default_input_device().and_then(|d| device_name(&d)),
        default_output: host.default_output_device().and_then(|d| device_name(&d)),
    })
}

/// Device name without panicking (`Display` on cpal devices errors if the device vanished).
fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_owned())
}

/// What the io thread managed to open.
pub(crate) struct StreamInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

pub(crate) struct Opened {
    pub input: Result<StreamInfo, String>,
    pub output: Result<StreamInfo, String>,
    /// Non-fatal problems, e.g. a configured device that no longer exists.
    pub notes: Vec<String>,
}

pub(crate) struct IoRequest {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub shared: Arc<Shared>,
    pub capture_tx: Sender<Vec<f32>>,
    pub recycle_rx: Receiver<Vec<f32>>,
    pub ready_tx: Sender<Opened>,
    pub stop_rx: Receiver<()>,
}

/// Body of the io thread: open both streams, report, then hold them until told to stop.
///
/// `test:…` names open a virtual device ([`crate::testdev`]) instead: no real device (nor
/// the audio host, if neither side needs one) is touched for that direction.
pub(crate) fn run_io(req: IoRequest) {
    let IoRequest { input_device, output_device, shared, capture_tx, recycle_rx, ready_tx, stop_rx } = req;
    let mut host = None;
    let mut notes = Vec::new();

    let input = match input_device.as_deref().and_then(|n| testdev::parse_input(n).map(|d| (n, d))) {
        Some((name, dev)) => dev.and_then(|d| {
            let cb = InputCallback { channels: 1, capture_tx, recycle_rx };
            testdev::open_input(name, d, cb).map(|(info, r)| (info, Held::Test(r)))
        }),
        None => {
            let host = host.get_or_insert_with(cpal::default_host);
            open_input(host, input_device.as_deref(), &shared, capture_tx, recycle_rx, &mut notes).map(|(i, s)| (i, Held::Cpal(s)))
        }
    };
    let output = match output_device.as_deref().and_then(|n| testdev::parse_output(n).map(|d| (n, d))) {
        Some((name, dev)) => dev.and_then(|d| {
            let cb = OutputCallback::new(1, SAMPLE_RATE, &shared);
            testdev::open_output(name, d, cb).map(|(info, r)| (info, Held::Test(r)))
        }),
        None => {
            let host = host.get_or_insert_with(cpal::default_host);
            open_output(host, output_device.as_deref(), &shared, &mut notes).map(|(i, s)| (i, Held::Cpal(s)))
        }
    };

    let (input, input_stream) = split(input);
    let (output, output_stream) = split(output);
    let any = input_stream.is_some() || output_stream.is_some();
    if ready_tx.send(Opened { input, output, notes }).is_err() || !any {
        return; // start() gave up waiting, or nothing to hold
    }
    // Parked until VoiceEngine drops the sender.
    let _ = stop_rx.recv();
    drop(input_stream);
    drop(output_stream);
}

/// What the io thread holds open: a real stream, or a virtual test device's thread.
/// Dropping it stops it.
enum Held {
    Cpal(#[expect(dead_code, reason = "held to keep the stream playing")] cpal::Stream),
    Test(#[expect(dead_code, reason = "held to keep the device running")] testdev::Runner),
}

type Opening<S = cpal::Stream> = Result<(StreamInfo, S), String>;

fn split(r: Opening<Held>) -> (Result<StreamInfo, String>, Option<Held>) {
    match r {
        Ok((info, stream)) => (Ok(info), Some(stream)),
        Err(e) => (Err(e), None),
    }
}

fn find_device(host: &cpal::Host, wanted: Option<&str>, input: bool, notes: &mut Vec<String>) -> Result<cpal::Device, String> {
    let dir = if input { "input" } else { "output" };
    if let Some(name) = wanted {
        let devices = if input { host.input_devices() } else { host.output_devices() };
        match devices {
            Ok(mut it) => {
                if let Some(d) = it.find(|d| device_name(d).as_deref() == Some(name)) {
                    return Ok(d);
                }
            }
            Err(e) => tracing::debug!("listing {dir} devices: {e}"),
        }
        notes.push(format!("{dir} device \"{name}\" not found; using the system default"));
    }
    let device = if input { host.default_input_device() } else { host.default_output_device() };
    device.ok_or_else(|| format!("no {dir} device available"))
}

fn stream_error(shared: &Shared, dir: &str, e: cpal::Error) {
    use cpal::ErrorKind;
    match e.kind() {
        ErrorKind::Xrun | ErrorKind::RealtimeDenied => tracing::debug!("{dir} stream: {e}"),
        ErrorKind::DeviceChanged => tracing::info!("{dir} stream rerouted: {e}"),
        _ => shared.report_error(format!("{dir} stream error: {e}")),
    }
}

/// Expands `$build` once per supported sample format with `$t` aliased to that sample type.
macro_rules! build_for_format {
    ($format:expr, $name:expr, |$t:ident| $build:expr) => {
        build_for_format!(@arms $format, $name, $t, $build;
            F32 => f32, F64 => f64, I8 => i8, I16 => i16, I24 => cpal::I24,
            I32 => i32, U8 => u8, U16 => u16, U32 => u32)
    };
    (@arms $format:expr, $name:expr, $t:ident, $build:expr; $($variant:ident => $ty:ty),*) => {
        match $format {
            $(SampleFormat::$variant => {
                type $t = $ty;
                $build
            })*
            other => return Err(format!("\"{}\": unsupported sample format {other}", $name)),
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Input

/// Downmixes each callback to mono and hands it to the encoder thread in a recycled buffer.
pub(crate) struct InputCallback {
    pub channels: usize,
    pub capture_tx: Sender<Vec<f32>>,
    pub recycle_rx: Receiver<Vec<f32>>,
}

impl InputCallback {
    pub fn push<T: Sample>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
        let mut buf = self.recycle_rx.try_recv().unwrap_or_default();
        buf.clear();
        if self.channels <= 1 {
            buf.extend(data.iter().map(|&s| s.to_sample::<f32>()));
        } else {
            let scale = 1.0 / self.channels as f32;
            buf.extend(data.chunks_exact(self.channels).map(|f| f.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() * scale));
        }
        // Full queue = encoder thread stalled; dropping capture is the only sane option.
        let _ = self.capture_tx.try_send(buf);
    }
}

fn open_input(
    host: &cpal::Host,
    wanted: Option<&str>,
    shared: &Arc<Shared>,
    capture_tx: Sender<Vec<f32>>,
    recycle_rx: Receiver<Vec<f32>>,
    notes: &mut Vec<String>,
) -> Opening {
    let device = find_device(host, wanted, true, notes)?;
    let name = device_name(&device).unwrap_or_else(|| "unknown input".into());
    let supported = device.default_input_config().map_err(|e| format!("\"{name}\": {e}"))?;
    let config = supported.config();
    let format = supported.sample_format();
    let err_shared = shared.clone();
    let on_error = move |e| stream_error(&err_shared, "input", e);
    let mut cb = InputCallback { channels: config.channels.max(1) as usize, capture_tx, recycle_rx };

    let stream = build_for_format!(format, name, |T| device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| cb.push(data),
        on_error,
        Some(BUILD_TIMEOUT),
    ))
    .map_err(|e| format!("\"{name}\": {e}"))?;
    stream.play().map_err(|e| format!("\"{name}\": {e}"))?;

    Ok((StreamInfo { name, sample_rate: config.sample_rate, channels: config.channels, format }, stream))
}

// ---------------------------------------------------------------------------------------------
// Output

/// Pulls 48 kHz audio from the mixer, resamples to the device rate, applies volume and
/// copies the mono signal to every device channel.
pub(crate) struct OutputCallback {
    channels: usize,
    shared: Arc<Shared>,
    pump: PullResampler,
    mono: Vec<f32>,
}

impl OutputCallback {
    pub fn new(channels: u16, device_rate: u32, shared: &Arc<Shared>) -> Self {
        Self {
            channels: channels.max(1) as usize,
            shared: shared.clone(),
            pump: PullResampler::new(SAMPLE_RATE, device_rate),
            mono: Vec::with_capacity(8192),
        }
    }

    pub fn fill<T: Sample + FromSample<f32>>(&mut self, data: &mut [T]) {
        if self.shared.controls.deafened.load(Relaxed) {
            data.fill(T::EQUILIBRIUM);
            return;
        }
        let frames = data.len() / self.channels;
        self.mono.resize(frames, 0.0);
        let now = Instant::now();
        let mixer = &self.shared.mixer;
        self.pump.fill(&mut self.mono, |buf| mixer.lock().mix(buf, now));
        let volume = self.shared.controls.output_volume.load();
        for (frame, &s) in data.chunks_exact_mut(self.channels).zip(&self.mono) {
            frame.fill((s * volume).clamp(-1.0, 1.0).to_sample::<T>());
        }
    }
}

fn open_output(host: &cpal::Host, wanted: Option<&str>, shared: &Arc<Shared>, notes: &mut Vec<String>) -> Opening {
    let device = find_device(host, wanted, false, notes)?;
    let name = device_name(&device).unwrap_or_else(|| "unknown output".into());
    let supported = device.default_output_config().map_err(|e| format!("\"{name}\": {e}"))?;
    let config = supported.config();
    let format = supported.sample_format();
    let err_shared = shared.clone();
    let on_error = move |e| stream_error(&err_shared, "output", e);
    let mut cb = OutputCallback::new(config.channels, config.sample_rate, shared);

    let stream = build_for_format!(format, name, |T| device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| cb.fill(data),
        on_error,
        Some(BUILD_TIMEOUT),
    ))
    .map_err(|e| format!("\"{name}\": {e}"))?;
    stream.play().map_err(|e| format!("\"{name}\": {e}"))?;

    Ok((StreamInfo { name, sample_rate: config.sample_rate, channels: config.channels, format }, stream))
}
