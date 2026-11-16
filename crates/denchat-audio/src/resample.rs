//! Streaming linear-interpolation resampling between device rates and 48 kHz.
//!
//! [`Resampler`] is push-style (feed input chunks, collect whatever output they yield) and
//! is used on the capture side. [`PullResampler`] wraps it for the playback side, where the
//! device asks for an exact number of output samples and we pull 48 kHz audio from the mixer
//! on demand.
//!
//! Linear interpolation is plenty for speech at the usual 44.1/48 kHz device rates. It does
//! no anti-alias filtering, so downsampling from much higher rates (e.g. 96 kHz) can alias
//! a little ultrasonic content into the voice band.

/// Mono streaming resampler with a fractional read position carried across chunks.
#[derive(Clone, Debug)]
pub struct Resampler {
    from: u32,
    to: u32,
    /// Input samples advanced per output sample.
    step: f64,
    /// Read position relative to `prev` (0.0 = exactly at `prev`, 1.0 = first new sample).
    pos: f64,
    /// Last input sample of the previous chunk.
    prev: f32,
}

impl Resampler {
    /// Resample from `from` Hz to `to` Hz. Zero rates are treated as 1 Hz.
    pub fn new(from: u32, to: u32) -> Self {
        let (from, to) = (from.max(1), to.max(1));
        Self { from, to, step: from as f64 / to as f64, pos: 0.0, prev: 0.0 }
    }

    pub fn from_rate(&self) -> u32 {
        self.from
    }

    pub fn to_rate(&self) -> u32 {
        self.to
    }

    /// Input samples consumed per output sample.
    pub fn step(&self) -> f64 {
        self.step
    }

    pub fn is_passthrough(&self) -> bool {
        self.from == self.to
    }

    /// Clear history, as if newly created.
    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = 0.0;
    }

    /// Append the output produced by `input` to `out`.
    ///
    /// Over a long stream the total output length is `total_input * to / from` (±1).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(&last) = input.last() else { return };
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        // Virtual sequence: index 0 = self.prev, index k (k ≥ 1) = input[k - 1].
        let n = input.len() as f64;
        let mut t = self.pos;
        out.reserve(((n - t) / self.step) as usize + 1);
        while t < n {
            let i = t as usize; // t ≥ 0, so this is floor
            let frac = (t - i as f64) as f32;
            let a = if i == 0 { self.prev } else { input[i - 1] };
            let b = input[i];
            out.push(a + (b - a) * frac);
            t += self.step;
        }
        self.pos = t - n;
        self.prev = last;
    }
}

/// Produces exactly as many output samples as requested by pulling input from a source.
///
/// Allocation-free once its internal buffers have grown to the callback size, so it is
/// safe to use from an audio callback.
#[derive(Debug)]
pub struct PullResampler {
    rs: Resampler,
    src: Vec<f32>,
    pending: Vec<f32>,
    read: usize,
}

/// Upper bound on one pull from the source (100 ms at 48 kHz).
const MAX_PULL: usize = 4800;

impl PullResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { rs: Resampler::new(from, to), src: Vec::with_capacity(MAX_PULL), pending: Vec::with_capacity(MAX_PULL * 2), read: 0 }
    }

    /// Fill all of `out`. `source` is called with a buffer to fill completely with input-rate
    /// samples; any surplus output is kept for the next call.
    pub fn fill(&mut self, out: &mut [f32], mut source: impl FnMut(&mut [f32])) {
        let mut written = 0;
        let mut pulls = 0;
        while written < out.len() {
            if self.read >= self.pending.len() {
                self.pending.clear();
                self.read = 0;
                if pulls == 8 {
                    // Cannot happen with sane rates; never spin in an audio callback.
                    out[written..].fill(0.0);
                    return;
                }
                pulls += 1;
                let need = out.len() - written;
                let n = ((need as f64 * self.rs.step()).ceil() as usize + 1).clamp(1, MAX_PULL);
                self.src.resize(n, 0.0);
                source(&mut self.src[..n]);
                self.rs.process(&self.src[..n], &mut self.pending);
                continue;
            }
            let take = (self.pending.len() - self.read).min(out.len() - written);
            out[written..written + take].copy_from_slice(&self.pending[self.read..self.read + take]);
            written += take;
            self.read += take;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total_output(from: u32, to: u32, chunk: usize, chunks: usize) -> usize {
        let mut rs = Resampler::new(from, to);
        let input = vec![0.25f32; chunk];
        let mut out = Vec::new();
        for _ in 0..chunks {
            rs.process(&input, &mut out);
        }
        out.len()
    }

    #[test]
    fn output_length_matches_ratio_over_many_chunks() {
        // 10 s of 44.1 kHz in odd-sized chunks → 10 s of 48 kHz.
        let n = total_output(44_100, 48_000, 441, 1000);
        assert!(n.abs_diff(480_000) <= 1, "{n}");
        let n = total_output(48_000, 44_100, 480, 1000);
        assert!(n.abs_diff(441_000) <= 1, "{n}");
        let n = total_output(16_000, 48_000, 160, 500);
        assert!(n.abs_diff(240_000) <= 1, "{n}");
        // Chunk sizes that don't divide evenly.
        let n = total_output(22_050, 48_000, 37, 2000);
        let expect = 37 * 2000 * 48_000 / 22_050;
        assert!(n.abs_diff(expect) <= 1, "{n} vs {expect}");
        assert_eq!(total_output(48_000, 48_000, 123, 10), 1230);
    }

    #[test]
    fn output_is_continuous_across_chunk_boundaries() {
        let from = 44_100;
        let to = 48_000;
        let f = 440.0f64;
        let input: Vec<f32> = (0..44_100).map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / from as f64).sin() as f32).collect();

        let mut whole = Vec::new();
        Resampler::new(from, to).process(&input, &mut whole);

        let mut chunked = Vec::new();
        let mut rs = Resampler::new(from, to);
        for c in input.chunks(97) {
            rs.process(c, &mut chunked);
        }
        // Float rounding may land one sample either side of an exact boundary.
        assert!(whole.len().abs_diff(chunked.len()) <= 1);
        let max_diff = whole.iter().zip(&chunked).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(max_diff < 1e-4, "chunking changed output by {max_diff}");

        // Largest sample-to-sample jump stays at what a 440 Hz sine allows at 48 kHz.
        let max_step = chunked.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        let bound = (2.0 * std::f64::consts::PI * f / to as f64) as f32 * 1.1;
        assert!(max_step < bound, "discontinuity {max_step} > {bound}");
    }

    #[test]
    fn pull_resampler_fills_exactly_and_keeps_phase() {
        // Source is a 48 kHz ramp continuing the resampler's initial zero history (1, 2, 3...);
        // the consumer is a 44.1 kHz device asking for odd buffer sizes.
        let mut next = 1u32;
        let mut pull = PullResampler::new(48_000, 44_100);
        let mut produced = 0usize;
        let mut last = None::<f32>;
        for size in [441usize, 512, 1, 333, 1024].iter().cycle().take(200) {
            let mut out = vec![f32::NAN; *size];
            pull.fill(&mut out, |buf| {
                for s in buf {
                    *s = next as f32;
                    next += 1;
                }
            });
            assert!(out.iter().all(|s| s.is_finite()));
            for &s in &out {
                if let Some(prev) = last {
                    let d = s - prev;
                    assert!((d - 48_000.0 / 44_100.0).abs() < 0.05, "ramp step {d}");
                }
                last = Some(s);
            }
            produced += size;
        }
        // Source consumption tracks output consumption (only a small surplus is buffered).
        let expected_src = produced as f64 * 48_000.0 / 44_100.0;
        let consumed = (next - 1) as f64;
        assert!((consumed - expected_src).abs() < 8.0, "{consumed} vs {expected_src}");
    }
}
