//! Virtual audio devices for tests: voice without a microphone or speakers.
//!
//! Any device name starting with `test:` is one of these instead of a real device, and no
//! real device is opened for that direction:
//!
//! | Name | Direction | What it does |
//! | --- | --- | --- |
//! | `test:silence` | input | digital silence |
//! | `test:sine:<hz>` | input | a steady tone at amplitude 0.3 (`test:sine` = 440 Hz) |
//! | `test:file:<path.wav>` | input | loops a WAV file (PCM 8/16/24/32-bit or float, any rate, any channels) |
//! | `test:null` | output | plays nothing |
//! | `test:capture:<path.wav>` | output | records what would be played (48 kHz mono 16-bit WAV) |
//!
//! Each runs on its own thread at real-time pace and drives the same callbacks a cpal stream
//! would, so the encoder, gate, jitter buffers, mixer and [`VoiceStats`](crate::VoiceStats)
//! behave exactly as with real hardware. A capture file's header is rewritten every 100 ms,
//! so a test can read the file while voice runs; it is final once the engine is dropped.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use denchat_proto::voice::SAMPLE_RATE;

use crate::device::{InputCallback, OutputCallback, StreamInfo};

/// Prefix that marks a device name as virtual.
pub const PREFIX: &str = "test:";

/// Virtual inputs, as `(name, what it does)`, for device listings.
pub const TEST_INPUTS: &[(&str, &str)] = &[
    ("test:silence", "test device: silence"),
    ("test:sine:<hz>", "test device: a tone, e.g. test:sine:440"),
    ("test:file:<path.wav>", "test device: loops a WAV file"),
];

/// Virtual outputs, as `(name, what it does)`, for device listings.
pub const TEST_OUTPUTS: &[(&str, &str)] =
    &[("test:null", "test device: plays nothing"), ("test:capture:<path.wav>", "test device: records playback to a WAV file")];

/// Amplitude of `test:sine` (RMS ≈ 0.21: well above the default VAD threshold).
pub const SINE_AMPLITUDE: f32 = 0.3;

/// Whether `name` is a virtual device (`test:…`, in any case).
pub fn is_test_device(name: &str) -> bool {
    without_prefix(name).is_some()
}

/// What follows `test:` (matched in any case, so `Test:sine` never falls back to a real device).
fn without_prefix(name: &str) -> Option<&str> {
    let head = name.get(..PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX).then(|| &name[PREFIX.len()..])
}

/// How often a virtual device runs its callback.
const PERIOD: Duration = Duration::from_millis(10);
/// Further behind than this (the machine slept, a debugger paused us): skip ahead instead of
/// delivering the backlog in a burst.
const MAX_LAG: Duration = Duration::from_millis(200);
/// How often a capture file's header is brought up to date.
const HEADER_EVERY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TestInput {
    Silence,
    Sine(f32),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TestOutput {
    Null,
    Capture(PathBuf),
}

/// `None` if `name` isn't a test device; otherwise the parsed device or why it's invalid.
pub(crate) fn parse_input(name: &str) -> Option<Result<TestInput, String>> {
    let rest = without_prefix(name)?;
    let (kind, arg) = rest.split_once(':').map_or((rest, None), |(k, a)| (k, Some(a)));
    Some(match (kind.to_ascii_lowercase().as_str(), arg) {
        ("silence", None) => Ok(TestInput::Silence),
        ("sine", None) => Ok(TestInput::Sine(440.0)),
        ("sine", Some(hz)) => match hz.trim().parse::<f32>() {
            Ok(f) if f > 0.0 && f < SAMPLE_RATE as f32 / 2.0 => Ok(TestInput::Sine(f)),
            _ => Err(format!("\"{name}\": the tone must be between 0 and {} Hz", SAMPLE_RATE / 2)),
        },
        ("file", Some(p)) if !p.is_empty() => Ok(TestInput::File(PathBuf::from(p))),
        _ => Err(format!("unknown test input \"{name}\" (try test:silence, test:sine:440 or test:file:<path.wav>)")),
    })
}

/// `None` if `name` isn't a test device; otherwise the parsed device or why it's invalid.
pub(crate) fn parse_output(name: &str) -> Option<Result<TestOutput, String>> {
    let rest = without_prefix(name)?;
    let (kind, arg) = rest.split_once(':').map_or((rest, None), |(k, a)| (k, Some(a)));
    Some(match (kind.to_ascii_lowercase().as_str(), arg) {
        ("null", None) => Ok(TestOutput::Null),
        ("capture", Some(p)) if !p.is_empty() => Ok(TestOutput::Capture(PathBuf::from(p))),
        _ => Err(format!("unknown test output \"{name}\" (try test:null or test:capture:<path.wav>)")),
    })
}

/// A virtual device's thread; stops and joins when dropped.
pub(crate) struct Runner {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn spawn(name: &str, body: impl FnOnce(&AtomicBool) + Send + 'static) -> Result<Runner, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = thread::Builder::new()
        .name("den-audio-test".into())
        .spawn(move || body(&flag))
        .map_err(|e| format!("\"{name}\": starting the test device: {e}"))?;
    Ok(Runner { stop, thread: Some(thread) })
}

/// Call `tick(n)` with the number of samples due, `rate` samples per second of wall time,
/// in chunks of [`PERIOD`], until `stop` is set.
fn pace(rate: u32, stop: &AtomicBool, mut tick: impl FnMut(usize)) {
    let rate = f64::from(rate.max(1));
    let chunk = ((rate * PERIOD.as_secs_f64()) as u64).max(1);
    let max_lag = (rate * MAX_LAG.as_secs_f64()) as u64;
    let start = Instant::now();
    let mut done: u64 = 0;
    while !stop.load(Relaxed) {
        let due = (start.elapsed().as_secs_f64() * rate) as u64;
        if due > done + max_lag {
            done = due - chunk;
        }
        while due >= done + chunk {
            tick(chunk as usize);
            done += chunk;
        }
        let next = start + Duration::from_secs_f64((done + chunk) as f64 / rate);
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs

/// A source of mono samples at `rate`.
enum Source {
    Silence,
    Sine { phase: f32, step: f32 },
    Loop { samples: Vec<f32>, pos: usize },
}

impl Source {
    fn fill(&mut self, out: &mut [f32]) {
        match self {
            Source::Silence => out.fill(0.0),
            Source::Sine { phase, step } => {
                for s in out {
                    *s = SINE_AMPLITUDE * phase.sin();
                    *phase += *step;
                    if *phase >= std::f32::consts::TAU {
                        *phase -= std::f32::consts::TAU;
                    }
                }
            }
            Source::Loop { samples, pos } => {
                for s in out {
                    *s = samples[*pos];
                    *pos = (*pos + 1) % samples.len();
                }
            }
        }
    }
}

pub(crate) fn open_input(name: &str, dev: TestInput, mut cb: InputCallback) -> Result<(StreamInfo, Runner), String> {
    let (mut source, rate) = match dev {
        TestInput::Silence => (Source::Silence, SAMPLE_RATE),
        TestInput::Sine(hz) => (Source::Sine { phase: 0.0, step: std::f32::consts::TAU * hz / SAMPLE_RATE as f32 }, SAMPLE_RATE),
        TestInput::File(path) => {
            let wav = read_wav(&path).map_err(|e| format!("\"{name}\": {e}"))?;
            if wav.samples.is_empty() {
                return Err(format!("\"{name}\": {} has no audio", path.display()));
            }
            (Source::Loop { samples: wav.samples, pos: 0 }, wav.sample_rate)
        }
    };
    let runner = spawn(name, move |stop| {
        let mut buf = Vec::new();
        pace(rate, stop, |n| {
            buf.resize(n, 0.0);
            source.fill(&mut buf);
            cb.push(&buf);
        });
    })?;
    Ok((StreamInfo { name: name.to_owned(), sample_rate: rate, channels: 1, format: cpal::SampleFormat::F32 }, runner))
}

// ---------------------------------------------------------------------------------------------
// Outputs

pub(crate) fn open_output(name: &str, dev: TestOutput, mut cb: OutputCallback) -> Result<(StreamInfo, Runner), String> {
    let mut wav = match dev {
        TestOutput::Null => None,
        TestOutput::Capture(path) => {
            Some(WavWriter::create(&path, SAMPLE_RATE).map_err(|e| format!("\"{name}\": {}: {e}", path.display()))?)
        }
    };
    let runner = spawn(name, move |stop| {
        let mut buf = Vec::new();
        let mut last_header = Instant::now();
        pace(SAMPLE_RATE, stop, |n| {
            buf.resize(n, 0.0);
            cb.fill(&mut buf);
            if let Some(w) = wav.as_mut() {
                let mut res = w.write(&buf);
                if res.is_ok() && last_header.elapsed() >= HEADER_EVERY {
                    last_header = Instant::now();
                    res = w.sync();
                }
                if let Err(e) = res {
                    tracing::warn!("test capture: {e}");
                    wav = None;
                }
            }
        });
        if let Some(mut w) = wav
            && let Err(e) = w.sync()
        {
            tracing::warn!("test capture: {e}");
        }
    })?;
    Ok((StreamInfo { name: name.to_owned(), sample_rate: SAMPLE_RATE, channels: 1, format: cpal::SampleFormat::F32 }, runner))
}

// ---------------------------------------------------------------------------------------------
// WAV files

/// A decoded WAV file, downmixed to mono.
#[derive(Clone, Debug, PartialEq)]
pub struct Wav {
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

/// Read a WAV file (PCM 8/16/24/32-bit or IEEE float 32/64-bit, any channel count), downmixed
/// to mono. Handy for checking what a `test:capture:` device recorded.
pub fn read_wav(path: &Path) -> std::io::Result<Wav> {
    parse_wav(&std::fs::read(path)?).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

fn parse_wav(b: &[u8]) -> Result<Wav, String> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut fmt = None;
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let size = u32_at(i + 4) as usize;
        let body = i + 8;
        // A capture still being written may claim less data than the file holds, and one
        // whose header was never updated claims none: then take what's there. Otherwise
        // the size is right, and chunks after the data (LIST, id3, cue) aren't audio.
        let end = if id == b"data" {
            let claimed = body + size;
            let next = claimed + (size & 1);
            let chunk_follows = next + 8 <= b.len() && b[next..next + 4].iter().all(|c| c.is_ascii_alphanumeric() || *c == b' ');
            let exact = size != 0 && size != u32::MAX as usize && claimed <= b.len() && (next >= b.len() || chunk_follows);
            if exact { claimed } else { b.len() }
        } else {
            (body + size).min(b.len())
        };
        if id == b"fmt " {
            if end - body < 16 {
                return Err("short fmt chunk".into());
            }
            let mut format = u16_at(body);
            if format == 0xFFFE && end - body >= 26 {
                // WAVE_FORMAT_EXTENSIBLE: the real format is the sub-format GUID's first word.
                format = u16_at(body + 24);
            }
            fmt = Some((format, u16_at(body + 2).max(1), u32_at(body + 4), u16_at(body + 14)));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt.ok_or("data before fmt")?;
            let data = &b[body..end];
            let decode: fn(&[u8]) -> f32 = match (format, bits) {
                (1, 8) => |s| (f32::from(s[0]) - 128.0) / 128.0,
                (1, 16) => |s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0,
                (1, 24) => |s| (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
                (1, 32) => |s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
                (3, 32) => |s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                (3, 64) => |s| f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32,
                _ => return Err(format!("unsupported WAV encoding (format {format}, {bits}-bit)")),
            };
            let width = usize::from(bits / 8);
            let frame = width * usize::from(channels);
            let scale = 1.0 / f32::from(channels);
            let samples = data
                .chunks_exact(frame)
                .map(|f| f.chunks_exact(width).map(decode).sum::<f32>() * scale)
                .map(|s| if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 })
                .collect();
            return Ok(Wav { sample_rate: rate.max(1), samples });
        }
        i = body + size + (size & 1);
    }
    Err("no audio data".into())
}

/// Streams mono 16-bit PCM to a WAV file. [`WavWriter::sync`] makes the header match the
/// data written so far, so the file is valid while it's still being written.
pub(crate) struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u32,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> std::io::Result<WavWriter> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut w = WavWriter { file: BufWriter::new(File::create(path)?), data_bytes: 0 };
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&36u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&1u16.to_le_bytes()); // mono
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes per second
        h.extend_from_slice(&2u16.to_le_bytes()); // block align
        h.extend_from_slice(&16u16.to_le_bytes()); // bits
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes());
        w.file.write_all(&h)?;
        w.file.flush()?;
        Ok(w)
    }

    pub fn write(&mut self, samples: &[f32]) -> std::io::Result<()> {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            self.file.write_all(&v.to_le_bytes())?;
        }
        self.data_bytes = self.data_bytes.saturating_add(samples.len() as u32 * 2);
        Ok(())
    }

    /// Flush and rewrite the sizes in the header.
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&self.data_bytes.saturating_add(36).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.data_bytes.to_le_bytes())?;
        // Back to the end of *our* data, and cut anything past it: when voice
        // restarts, the old engine's writer of this path can still be flushing
        // its last buffer as the new one starts over.
        let end = 44 + u64::from(self.data_bytes);
        f.set_len(end)?;
        f.seek(SeekFrom::Start(end))?;
        f.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("den-audio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn names_parse() {
        assert_eq!(parse_input("Built-in Mic"), None);
        assert_eq!(parse_input("test:silence"), Some(Ok(TestInput::Silence)));
        assert_eq!(parse_input("test:sine"), Some(Ok(TestInput::Sine(440.0))));
        assert_eq!(parse_input("test:sine:1000"), Some(Ok(TestInput::Sine(1000.0))));
        assert!(matches!(parse_input("test:sine:99999"), Some(Err(_))));
        assert!(matches!(parse_input("test:sine:abc"), Some(Err(_))));
        // Paths keep their colons (Windows drive letters).
        assert_eq!(parse_input("test:file:C:\\a.wav"), Some(Ok(TestInput::File(PathBuf::from("C:\\a.wav")))));
        assert!(matches!(parse_input("test:nope"), Some(Err(_))));
        assert!(matches!(parse_input("test:file:"), Some(Err(_))));
        assert_eq!(parse_output("Speakers"), None);
        assert_eq!(parse_output("test:null"), Some(Ok(TestOutput::Null)));
        assert_eq!(parse_output("test:capture:/tmp/x.wav"), Some(Ok(TestOutput::Capture(PathBuf::from("/tmp/x.wav")))));
        assert!(matches!(parse_output("test:sine:440"), Some(Err(_))));
        assert!(is_test_device("test:null") && !is_test_device("Speakers"));
    }

    #[test]
    fn wav_roundtrip_and_partial_header() {
        let path = tmp("roundtrip.wav");
        let mut w = WavWriter::create(&path, 16_000).unwrap();
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 1600.0) - 0.5).collect();
        w.write(&samples[..800]).unwrap();
        w.sync().unwrap();
        // Valid mid-write.
        assert_eq!(read_wav(&path).unwrap().samples.len(), 800);
        w.write(&samples[800..]).unwrap();
        w.file.flush().unwrap();
        // Header says 800, file holds 1600: take what's there.
        assert_eq!(read_wav(&path).unwrap().samples.len(), 1600);
        w.sync().unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(back.sample_rate, 16_000);
        assert!(back.samples.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn wav_reads_stereo_and_float() {
        fn wav(format: u16, channels: u16, bits: u16, data: &[u8]) -> Vec<u8> {
            let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&format.to_le_bytes());
            b.extend_from_slice(&channels.to_le_bytes());
            b.extend_from_slice(&44_100u32.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&(channels * bits / 8).to_le_bytes());
            b.extend_from_slice(&bits.to_le_bytes());
            b.extend_from_slice(b"LIST\x03\0\0\0abc\0"); // odd-sized chunk to skip, with its pad byte
            b.extend_from_slice(b"data");
            b.extend_from_slice(&(data.len() as u32).to_le_bytes());
            b.extend_from_slice(data);
            b
        }
        let mut stereo16 = Vec::new();
        for (l, r) in [(16384i16, 0i16), (-16384, -16384)] {
            stereo16.extend_from_slice(&l.to_le_bytes());
            stereo16.extend_from_slice(&r.to_le_bytes());
        }
        let w = parse_wav(&wav(1, 2, 16, &stereo16)).unwrap();
        assert_eq!(w.sample_rate, 44_100);
        assert_eq!(w.samples, vec![0.25, -0.5]);
        let float: Vec<u8> = [0.5f32, -0.25].iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(parse_wav(&wav(3, 1, 32, &float)).unwrap().samples, vec![0.5, -0.25]);
        assert!(parse_wav(b"nope").is_err());
        assert!(parse_wav(&wav(2, 1, 16, &[0, 0])).is_err(), "ADPCM isn't supported");

        // Chunks after the data, and an odd-sized 8-bit data chunk's pad byte, aren't audio.
        let mut tagged = wav(1, 1, 8, &[128, 192, 64]);
        tagged.push(0);
        tagged.extend_from_slice(b"id3 \x04\0\0\0\xff\xff\xff\xff");
        assert_eq!(parse_wav(&tagged).unwrap().samples, vec![0.0, 0.5, -0.5]);
    }

    #[test]
    fn a_restarted_capture_drops_the_old_tail() {
        let path = tmp("restart.wav");
        let mut old = WavWriter::create(&path, 16_000).unwrap();
        old.write(&[0.5; 1000]).unwrap();
        old.sync().unwrap();
        // Voice restarts: a new writer starts over while the old one still flushes.
        let mut new = WavWriter::create(&path, 16_000).unwrap();
        old.write(&[0.5; 500]).unwrap();
        old.sync().unwrap();
        new.write(&[-0.25; 300]).unwrap();
        new.sync().unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(back.samples.len(), 300);
        assert!(back.samples.iter().all(|s| (s + 0.25).abs() < 1e-3));
    }

    #[test]
    fn test_prefix_in_any_case() {
        assert!(is_test_device("Test:Sine:440"));
        assert_eq!(parse_input("TEST:Sine:1000"), Some(Ok(TestInput::Sine(1000.0))));
        assert_eq!(parse_output("Test:NULL"), Some(Ok(TestOutput::Null)));
        assert_eq!(parse_input("tes"), None);
    }

    /// RMS and frequency (from upward zero crossings) of the audible part of a recording,
    /// skipping the first 100 ms of it (prebuffer ramp, codec start-up).
    fn analyse(w: &Wav) -> (f32, f32) {
        let loud = |s: &f32| s.abs() > 0.05;
        let first = w.samples.iter().position(loud).expect("recording has audio");
        let last = w.samples.iter().rposition(loud).unwrap();
        let seg = &w.samples[(first + w.sample_rate as usize / 10).min(last)..last];
        assert!(seg.len() > w.sample_rate as usize / 4, "only {} audible samples", seg.len());
        let rms = (seg.iter().map(|s| s * s).sum::<f32>() / seg.len() as f32).sqrt();
        let ups = seg.windows(2).filter(|p| p[0] < 0.0 && p[1] >= 0.0).count();
        (rms, ups as f32 * w.sample_rate as f32 / seg.len() as f32)
    }

    fn settings(input: &str, output: &str, mode: crate::TxMode) -> crate::VoiceSettings {
        crate::VoiceSettings { input_device: Some(input.into()), output_device: Some(output.into()), mode, ..Default::default() }
    }

    /// A tone goes in, through the encoder, gate, decoder, jitter buffer and mixer, and the
    /// same tone comes out in the capture file, for every codec this build has.
    #[test]
    fn sine_in_capture_out() {
        for codec in crate::supported_codecs() {
            let path = tmp(&format!("sine-{codec:?}.wav"));
            let out = format!("test:capture:{}", path.display());
            let stats =
                crate::loopback_test(settings("test:sine:440", &out, crate::TxMode::Open), codec, Duration::from_millis(1200)).unwrap();
            assert_eq!(stats.input_device.as_deref(), Some("test:sine:440"));
            assert_eq!(stats.output_device.as_deref(), Some(out.as_str()));
            assert_eq!(stats.tx_codec, Some(codec));
            assert!(stats.tx_packets >= 40 && stats.rx_packets >= 40, "{stats:?}");
            let expect = SINE_AMPLITUDE / 2f32.sqrt();
            assert!((stats.input_level - expect).abs() < 0.03, "mic level {} (expected ~{expect})", stats.input_level);
            let wav = read_wav(&path).unwrap();
            assert_eq!(wav.sample_rate, SAMPLE_RATE);
            let (rms, hz) = analyse(&wav);
            assert!((0.12..0.3).contains(&rms), "{codec:?}: output RMS {rms}");
            assert!((hz - 440.0).abs() < 20.0, "{codec:?}: output at {hz} Hz");
        }
    }

    #[test]
    fn silence_stays_off_the_air() {
        let stats = crate::loopback_test(
            settings("test:silence", "test:null", crate::TxMode::VoiceActivity),
            crate::Codec::Pcm16,
            Duration::from_millis(500),
        )
        .unwrap();
        assert_eq!(stats.input_device.as_deref(), Some("test:silence"));
        assert_eq!(stats.output_device.as_deref(), Some("test:null"));
        assert!(stats.input_level < 1e-4, "level {}", stats.input_level);
        assert!(!stats.transmitting);
        assert_eq!(stats.tx_packets, 0);
    }

    /// A WAV at another rate is resampled on the way in.
    #[test]
    fn file_input_is_resampled() {
        let src = tmp("tone-16k.wav");
        let mut w = WavWriter::create(&src, 16_000).unwrap();
        let tone: Vec<f32> = (0..16_000).map(|i| 0.4 * (std::f32::consts::TAU * 1000.0 * i as f32 / 16_000.0).sin()).collect();
        w.write(&tone).unwrap();
        w.sync().unwrap();
        drop(w);
        let cap = tmp("file-out.wav");
        let input = format!("test:file:{}", src.display());
        let stats = crate::loopback_test(
            settings(&input, &format!("test:capture:{}", cap.display()), crate::TxMode::Open),
            crate::Codec::Pcm16,
            Duration::from_millis(1000),
        )
        .unwrap();
        assert_eq!(stats.input_device.as_deref(), Some(input.as_str()));
        let (rms, hz) = analyse(&read_wav(&cap).unwrap());
        assert!((rms - 0.4 / 2f32.sqrt()).abs() < 0.06, "output RMS {rms}");
        assert!((hz - 1000.0).abs() < 30.0, "output at {hz} Hz");
    }

    #[test]
    fn bad_test_devices_are_reported() {
        let sink: Arc<dyn crate::PacketSink> = Arc::new(Nothing);
        let missing = tmp("no-such.wav");
        let input = format!("test:file:{}", missing.display());
        let e = crate::VoiceEngine::start(settings(&input, "test:null", crate::TxMode::Open), sink.clone()).unwrap();
        let st = e.stats();
        assert_eq!(st.input_device, None);
        assert_eq!(st.output_device.as_deref(), Some("test:null"));
        assert!(st.last_error.as_deref().is_some_and(|m| m.contains("listen-only") && m.contains("no-such.wav")), "{st:?}");
        drop(e);
        let err = crate::VoiceEngine::start(settings("test:bogus", "test:sine:440", crate::TxMode::Open), sink).err().unwrap();
        assert!(format!("{err:#}").contains("unknown test"), "{err:#}");
    }

    struct Nothing;
    impl crate::PacketSink for Nothing {
        fn send_voice(&self, _: u16, _: crate::Codec, _: &[u8]) {}
    }

    #[test]
    fn pacing_is_real_time() {
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        let mut total = 0usize;
        // Time of the last tick, not of `pace` returning: a busy machine can oversleep after it.
        let mut at = Duration::ZERO;
        pace(48_000, &stop, |n| {
            total += n;
            at = start.elapsed();
            if at >= Duration::from_millis(300) {
                stop.store(true, Relaxed);
            }
        });
        let expected = 48_000.0 * at.as_secs_f64();
        assert!((total as f64 - expected).abs() < 48_000.0 * 0.03, "{total} samples in {at:?}");
    }
}
