//! The desktop's ears: a tap on the system audio output, reduced to the
//! `AudioFx` rows every fx and particle shader receives.
//!
//! Capture is a `parec` child process reading the default sink's monitor
//! (`@DEFAULT_MONITOR@`) as raw mono float32, its stdout drained by a
//! thread into a small ring. This is the same documented-stopgap family as
//! the app-id placement tag: zero native dependencies, works anywhere
//! PipeWire's pulse shim (or PulseAudio) runs, and the upgrade path is a
//! native PipeWire capture stream behind the same `AudioFx` seam. A missing
//! `parec` logs once and the desktop is simply silent — every shader reads
//! zeros as "be still".
//!
//! Analysis runs at ~30Hz *in the render loop's thread* (a 2048-point FFT
//! is microseconds; threading it would buy nothing but a lock): Hann
//! window, radix-2 FFT, 32 log-spaced bands, and then the part that makes
//! it usable — per-band attack/decay envelopes, a slow AGC so quiet tracks
//! still move a wallpaper, and a bass-onset beat pulse. Smoothing lives
//! here on purpose: raw FFT frames strobe at frame rate, and every shader
//! author would otherwise re-implement the same envelope follower badly.

use std::sync::{Arc, Mutex};

use rill_gpu::AudioFx;

/// Samples per analysis frame. At 44.1kHz this is ~46ms of signal — enough
/// for the ~21.5Hz bin width the bass bands need.
const FFT_N: usize = 2048;
/// Capture rate asked of parec; analysis assumes it.
const RATE: f32 = 44100.0;
/// Seconds between analysis frames (~30Hz).
const ANALYSIS_DT: f32 = 1.0 / 30.0;
/// The spectrum's frequency span: log-spaced from lowest audible bass to
/// the top of what music actually occupies.
const F_LO: f32 = 40.0;
const F_HI: f32 = 16000.0;

pub struct AudioTap {
    /// Newest samples, most recent last; the reader thread appends and
    /// trims. `None` after a failed spawn — permanently silent.
    ring: Option<Arc<Mutex<Vec<f32>>>>,
    child: Option<std::process::Child>,
    last_analysis: std::time::Instant,
    /// Per-band smoothed envelopes (post-AGC), the spectrum rows' source.
    env: [f32; 32],
    /// Smoothed (bass, mid, treble, level).
    summary: [f32; 4],
    /// The AGC's notion of "loud lately" — a slowly decaying peak that
    /// band values are normalised against.
    agc: f32,
    /// Trailing mean of raw kick-band energy, what an onset is measured
    /// against.
    bass_avg: f32,
    /// Raw (pre-envelope) kick-band energy this instant — `pulse.w`, the
    /// thump itself for shaders that want punch rather than pulse.
    kick: f32,
    /// The beat pulse: set to 1 on an onset, decays toward 0.
    beat: f32,
    /// Beats heard so far — the monotonic counter behind `pulse.z`, which
    /// is what lets a stateless shader change *per* beat rather than only
    /// pulse with one. f32-exact far beyond any session's beat budget.
    beat_count: u32,
    /// Raw (unsmoothed) level of the latest frame.
    raw_level: f32,
    /// Seconds of beat refractory left — one onset, one pulse.
    beat_hold: f32,
}

impl AudioTap {
    /// Start the tap. Failure to spawn is not an error state the caller
    /// sees — it is a silent desktop, logged once here.
    pub fn start() -> AudioTap {
        let mut child = std::process::Command::new("parec")
            .args([
                "--raw",
                "--format=float32le",
                "--rate=44100",
                "--channels=1",
                "--latency-msec=30",
                "-d",
                "@DEFAULT_MONITOR@",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let ring = match &mut child {
            Ok(c) => {
                let stdout = c.stdout.take().expect("piped above");
                let ring = Arc::new(Mutex::new(Vec::<f32>::new()));
                let writer = Arc::clone(&ring);
                std::thread::Builder::new()
                    .name("audio-tap".into())
                    .spawn(move || drain(stdout, writer))
                    .expect("spawn audio reader thread");
                println!("rill-compositor: audio tap on @DEFAULT_MONITOR@");
                Some(ring)
            }
            Err(e) => {
                eprintln!(
                    "rill-compositor: no audio tap (parec: {e}) — shaders hear silence"
                );
                None
            }
        };
        AudioTap {
            ring,
            child: child.ok(),
            last_analysis: std::time::Instant::now(),
            env: [0.0; 32],
            summary: [0.0; 4],
            agc: 1e-4,
            bass_avg: 0.0,
            kick: 0.0,
            beat: 0.0,
            beat_count: 0,
            raw_level: 0.0,
            beat_hold: 0.0,
        }
    }

    /// The current `AudioFx` rows, re-analysed at most every ~33ms. Cheap
    /// to call every frame.
    pub fn fx(&mut self) -> AudioFx {
        let dt = self.last_analysis.elapsed().as_secs_f32();
        if dt >= ANALYSIS_DT {
            self.last_analysis = std::time::Instant::now();
            self.analyse(dt);
        }
        let mut spectrum = [[0.0f32; 4]; 8];
        for (i, v) in self.env.iter().enumerate() {
            spectrum[i / 4][i % 4] = *v;
        }
        AudioFx {
            bands: self.summary,
            pulse: [self.beat, self.raw_level, self.beat_count as f32, self.kick],
            spectrum,
        }
    }

    /// Whether anything is audibly happening — the render loop's cue to
    /// keep frames coming while a reactive shader has something to react to.
    pub fn active(&self) -> bool {
        self.summary[3] > 0.005 || self.beat > 0.01
    }

    fn analyse(&mut self, dt: f32) {
        let Some(ring) = self.ring.clone() else { return };
        let mut samples = [0.0f32; FFT_N];
        let got = {
            let buf = ring.lock().unwrap();
            let n = buf.len().min(FFT_N);
            if n > 0 {
                samples[FFT_N - n..].copy_from_slice(&buf[buf.len() - n..]);
            }
            n
        };
        if got == 0 {
            // No fresh signal: let everything decay so the desktop settles
            // rather than freezing mid-gesture.
            self.decay(dt);
            return;
        }

        // Raw level (RMS) before windowing — the honest "this instant".
        let rms =
            (samples.iter().map(|s| s * s).sum::<f32>() / FFT_N as f32).sqrt();
        self.raw_level = rms;

        // Hann window, then FFT magnitudes for the positive bins.
        let mut re = samples;
        let mut im = [0.0f32; FFT_N];
        for (i, s) in re.iter_mut().enumerate() {
            let w = 0.5
                - 0.5
                    * (std::f32::consts::TAU * i as f32 / (FFT_N - 1) as f32)
                        .cos();
            *s *= w;
        }
        fft(&mut re, &mut im);

        // 32 log-spaced bands over F_LO..F_HI, each the mean magnitude of
        // its bin span. Log spacing is what makes bass/mid/treble occupy
        // comparable numbers of bands rather than bass owning three bins.
        let mut bands = [0.0f32; 32];
        let bin_hz = RATE / FFT_N as f32;
        for (b, out) in bands.iter_mut().enumerate() {
            let t0 = b as f32 / 32.0;
            let t1 = (b + 1) as f32 / 32.0;
            let f0 = F_LO * (F_HI / F_LO).powf(t0);
            let f1 = F_LO * (F_HI / F_LO).powf(t1);
            let k0 = ((f0 / bin_hz) as usize).clamp(1, FFT_N / 2 - 1);
            let k1 = ((f1 / bin_hz) as usize).clamp(k0 + 1, FFT_N / 2);
            let mut acc = 0.0;
            for k in k0..k1 {
                acc += (re[k] * re[k] + im[k] * im[k]).sqrt();
            }
            *out = acc / (k1 - k0) as f32;
        }

        // AGC: normalise against a slowly-decaying recent peak, so a quiet
        // track fills the range and a loud one does not pin everything at 1.
        let peak = bands.iter().cloned().fold(0.0f32, f32::max);
        self.agc = (self.agc * (-dt / 3.0).exp()).max(peak).max(1e-4);
        for b in bands.iter_mut() {
            *b = (*b / self.agc).min(1.0);
        }

        // Envelopes: near-instant attack, ~250ms decay. Decay is expressed
        // per elapsed second so a stalled frame does not freeze the fall.
        let fall = (-dt / 0.25).exp();
        for (env, new) in self.env.iter_mut().zip(bands.iter()) {
            *env = if *new > *env { *new } else { *env * fall };
        }

        // Summary rows from the smoothed bands: 32 log bands split ~evenly
        // by ear — bass 40..250Hz, mid 250..2.5k, treble above.
        let mean = |r: std::ops::Range<usize>| {
            let n = r.len() as f32;
            self.env[r].iter().sum::<f32>() / n
        };
        let (bass, mid, treble) = (mean(0..9), mean(9..21), mean(21..32));
        let level_now = (self.raw_level / (self.agc / 4.0).max(1e-4)).min(1.0);
        let level =
            if level_now > self.summary[3] { level_now } else { self.summary[3] * fall };
        self.summary = [bass, mid, treble, level];

        // Beat: detected on the *raw* (pre-envelope) energy of the kick
        // band alone — the bottom six log bands, ~40–120Hz. Two deliberate
        // choices, both learned from watching it miss:
        //  - Raw, not the display envelopes: the 250ms decay that makes the
        //    spectrum pleasant to look at flattens exactly the transient an
        //    onset detector needs, so smoothed kicks barely cleared their
        //    own average. Raw kicks spike several times over it.
        //  - Kick band, not the summary bass (which reaches ~215Hz): snares
        //    and low vocals live up there, and they were firing the beat.
        let kick_now = bands[..6].iter().sum::<f32>() / 6.0;
        self.kick = kick_now;
        self.beat_hold = (self.beat_hold - dt).max(0.0);
        let onset = kick_now > self.bass_avg * 1.6
            && kick_now > 0.15
            && self.beat_hold <= 0.0;
        if onset {
            self.beat = 1.0;
            self.beat_hold = 0.12;
            self.beat_count = self.beat_count.wrapping_add(1);
        } else {
            self.beat *= (-dt / 0.15).exp();
        }
        // The average follows over ~a second, so a rolling bassline raises
        // the bar under itself and stops reading as endless onsets.
        self.bass_avg += (kick_now - self.bass_avg) * (dt / 1.0).min(1.0);
    }

    /// No signal: everything falls at its own rate, nothing sticks.
    fn decay(&mut self, dt: f32) {
        let fall = (-dt / 0.25).exp();
        for e in self.env.iter_mut() {
            *e *= fall;
        }
        for s in self.summary.iter_mut() {
            *s *= fall;
        }
        self.beat *= (-dt / 0.15).exp();
        self.raw_level = 0.0;
        self.kick = 0.0;
    }
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The reader thread: parec's stdout → the ring, newest last, trimmed to
/// one analysis window plus slack. Exits when the pipe closes.
fn drain(mut stdout: std::process::ChildStdout, ring: Arc<Mutex<Vec<f32>>>) {
    use std::io::Read;
    let mut chunk = [0u8; 4096];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        carry.extend_from_slice(&chunk[..n]);
        let whole = carry.len() / 4 * 4;
        let samples: Vec<f32> = carry[..whole]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        carry.drain(..whole);
        let mut buf = ring.lock().unwrap();
        buf.extend_from_slice(&samples);
        let len = buf.len();
        if len > FFT_N * 2 {
            buf.drain(..len - FFT_N * 2);
        }
    }
}

/// In-place iterative radix-2 Cooley–Tukey. `re`/`im` are the signal in and
/// the spectrum out. Hand-rolled on purpose: the compositor's dependency
/// list is deliberately thin, and 2048 points at 30Hz is microseconds.
fn fft(re: &mut [f32; FFT_N], im: &mut [f32; FFT_N]) {
    // Bit-reversal permutation.
    let bits = FFT_N.trailing_zeros();
    for i in 0..FFT_N {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= FFT_N {
        let ang = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..FFT_N).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in start..start + len / 2 {
                let (ar, ai) = (re[k], im[k]);
                let (br, bi) = (re[k + len / 2], im[k + len / 2]);
                let (tr, ti) = (br * cr - bi * ci, br * ci + bi * cr);
                re[k] = ar + tr;
                im[k] = ai + ti;
                re[k + len / 2] = ar - tr;
                im[k + len / 2] = ai - ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
    // Normalise so magnitudes are amplitude-like rather than growing with N.
    for k in 0..FFT_N {
        re[k] /= (FFT_N as f32) / 2.0;
        im[k] /= (FFT_N as f32) / 2.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFT must put a pure tone's energy in the right bin — the one
    /// property everything downstream (bands, bass, beat) rests on.
    #[test]
    fn fft_localises_a_tone() {
        let mut re = [0.0f32; FFT_N];
        let mut im = [0.0f32; FFT_N];
        let bin = 64;
        for (i, s) in re.iter_mut().enumerate() {
            *s = (std::f32::consts::TAU * bin as f32 * i as f32 / FFT_N as f32)
                .cos();
        }
        fft(&mut re, &mut im);
        let mag =
            |k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt();
        assert!(mag(bin) > 0.9, "tone bin carries the energy: {}", mag(bin));
        let elsewhere: f32 =
            (1..FFT_N / 2).filter(|k| (*k as i32 - bin as i32).abs() > 2).map(mag).fold(0.0, f32::max);
        assert!(
            elsewhere < 0.05,
            "energy must not smear across the spectrum: {elsewhere}"
        );
    }

    /// Silence in, zeros out — the contract every shader relies on.
    #[test]
    fn silence_reads_as_zero() {
        let mut tap = AudioTap {
            ring: Some(Arc::new(Mutex::new(Vec::new()))),
            child: None,
            last_analysis: std::time::Instant::now() - std::time::Duration::from_secs(1),
            env: [0.5; 32],
            summary: [0.5; 4],
            agc: 1.0,
            bass_avg: 0.5,
            kick: 0.5,
            beat: 1.0,
            beat_count: 7,
            raw_level: 0.5,
            beat_hold: 0.0,
        };
        // Several empty-analysis rounds: everything must decay toward zero.
        for _ in 0..40 {
            tap.last_analysis -= std::time::Duration::from_millis(100);
            let _ = tap.fx();
        }
        let fx = tap.fx();
        assert!(fx.bands.iter().all(|v| *v < 0.01), "bands decay: {:?}", fx.bands);
        assert!(fx.pulse[0] < 0.01, "beat decays: {}", fx.pulse[0]);
    }
}
