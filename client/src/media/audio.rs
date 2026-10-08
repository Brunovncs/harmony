//! The sound of a call, done here rather than by WebRTC's device module, so each person can have
//! their own volume (to 350%), deafening silences voices but not the soundpad, and the echo
//! canceller hears exactly what the speakers play.
//!
//! The speakers play a mix of sources (each voice, a screen's audio, cues, soundpad clips). The
//! microphone goes through the audio processing module (echo cancellation and noise suppression,
//! fed the mix as its reference), then the input volume and the noise gate, then to whatever
//! WebRTC source is listening. Everything inside runs at 48 kHz.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use libwebrtc::native::apm::AudioProcessingModule;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
/// 10 ms of mono audio: the unit WebRTC and the processing module work in.
pub const FRAME: usize = (RATE / 100) as usize;
/// The most a voice may queue before its oldest audio is dropped, so delay can't build up.
const MAX_VOICE_MS: usize = 160;
/// Volumes go to 350%, as in the old client.
pub const MAX_GAIN: f32 = 3.5;

/// An f32 kept in an atomic, for values the audio thread reads every callback.
#[derive(Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> AtomicF32 {
        AtomicF32(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
    /// Raises the value to `v` if it is higher; returns nothing.
    pub fn max(&self, v: f32) {
        let mut cur = self.0.load(Ordering::Relaxed);
        while f32::from_bits(cur) < v {
            match self.0.compare_exchange_weak(cur, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(x) => cur = x,
            }
        }
    }
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    /// Someone's microphone: silenced by deafening.
    Voice,
    /// A screen share's sound.
    Stream,
    /// Cues and soundpad clips.
    Effect,
}

/// Something the speakers play: interleaved stereo at 48 kHz, pushed by whoever produces it.
pub struct Source {
    pub kind: SourceKind,
    buf: Mutex<VecDeque<f32>>,
    pub gain: AtomicF32,
    /// The loudest 10 ms (RMS) since the meter last looked, for speaking rings.
    pub peak: AtomicF32,
    /// Removed from the mix once drained.
    pub one_shot: bool,
    max_len: usize,
    /// A soundpad clip, played straight from its shared samples (and how far it got) instead of
    /// being copied into `buf`.
    clip: Option<(Arc<[i16]>, AtomicUsize)>,
}

impl Source {
    pub fn new(kind: SourceKind, gain: f32, one_shot: bool) -> Arc<Source> {
        let max_len = if one_shot { usize::MAX } else { MAX_VOICE_MS * RATE as usize / 1000 * 2 };
        Arc::new(Source {
            kind,
            buf: Mutex::new(VecDeque::new()),
            gain: AtomicF32::new(gain),
            peak: AtomicF32::default(),
            one_shot,
            max_len,
            clip: None,
        })
    }

    /// Mono or stereo samples in -1..1 at 48 kHz.
    pub fn push(&self, samples: &[f32], channels: usize) {
        let mut sum = 0.;
        let mut buf = self.buf.lock();
        if channels == 1 {
            for &s in samples {
                buf.push_back(s);
                buf.push_back(s);
                sum += s * s;
            }
        } else {
            for f in samples.chunks_exact(channels) {
                buf.push_back(f[0]);
                buf.push_back(f[1]);
                sum += 0.25 * (f[0] + f[1]) * (f[0] + f[1]);
            }
        }
        let over = buf.len().saturating_sub(self.max_len);
        if over > 0 {
            buf.drain(..over & !1);
        }
        drop(buf);
        let frames = (samples.len() / channels.max(1)).max(1);
        self.peak.max((sum / frames as f32).sqrt());
    }

    pub fn push_i16(&self, samples: &[i16], channels: usize) {
        let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.).collect();
        if std::env::var_os("HARMONY_AUDIO_DEBUG").is_some() {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(200) {
                let rms = (f.iter().map(|x| x * x).sum::<f32>() / f.len().max(1) as f32).sqrt();
                log::info!("remote audio: frame {n}, {} samples x{channels}, rms {rms:.4}", samples.len());
            }
        }
        self.push(&f, channels);
    }

    fn is_empty(&self) -> bool {
        match &self.clip {
            Some((samples, at)) => at.load(Ordering::Relaxed) >= samples.len(),
            None => self.buf.lock().is_empty(),
        }
    }
}

/// What the speakers play, and the mix the echo canceller listens to.
pub struct Mixer {
    sources: Mutex<Vec<Arc<Source>>>,
    pub deafened: AtomicBool,
    /// The mix, mono, for the echo canceller's reference.
    far: Mutex<VecDeque<f32>>,
}

impl Mixer {
    fn new() -> Arc<Mixer> {
        Arc::new(Mixer { sources: Mutex::new(Vec::new()), deafened: AtomicBool::new(false), far: Mutex::new(VecDeque::new()) })
    }

    pub fn add(&self, s: Arc<Source>) {
        self.sources.lock().push(s);
    }

    pub fn remove(&self, s: &Arc<Source>) {
        self.sources.lock().retain(|x| !Arc::ptr_eq(x, s));
    }

    /// Fills `out` (interleaved stereo, 48 kHz).
    fn mix(&self, out: &mut [f32]) {
        out.fill(0.);
        let deaf = self.deafened.load(Ordering::Relaxed);
        let mut sources = self.sources.lock();
        for s in sources.iter() {
            let gain = if deaf && s.kind == SourceKind::Voice { 0. } else { s.gain.get() };
            if let Some((samples, at)) = &s.clip {
                let from = at.load(Ordering::Relaxed).min(samples.len());
                let n = out.len().min(samples.len() - from);
                for (o, &v) in out.iter_mut().zip(&samples[from..from + n]) {
                    *o += v as f32 / 32768. * gain;
                }
                at.store(from + n, Ordering::Relaxed);
                continue;
            }
            let mut buf = s.buf.lock();
            let n = out.len().min(buf.len());
            for (o, v) in out.iter_mut().zip(buf.drain(..n)) {
                *o += v * gain;
            }
        }
        sources.retain(|s| !(s.one_shot && s.is_empty()));
        drop(sources);
        for o in out.iter_mut() {
            *o = soft_clip(*o);
        }
        let mut far = self.far.lock();
        for f in out.chunks_exact(2) {
            far.push_back(0.5 * (f[0] + f[1]));
        }
        let over = far.len().saturating_sub(RATE as usize / 2);
        far.drain(..over);
    }
}

/// Keeps loud mixes (several people at 350%) from wrapping around.
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.9 { x } else { x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) / 0.1).tanh()) }
}

/// A linear resampler for interleaved audio, keeping its place between calls.
pub struct Resampler {
    from: u32,
    to: u32,
    channels: usize,
    pos: f64,
    last: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Resampler {
        Resampler { from, to, channels, pos: 0., last: vec![0.; channels] }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / ch;
        let step = self.from as f64 / self.to as f64;
        let at = |i: isize, c: usize, last: &[f32]| if i < 0 { last[c] } else { input[i as usize * ch + c] };
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize;
            let frac = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let a = at(i - 1, c, &self.last);
                let b = at(i, c, &self.last);
                out.push(a + (b - a) * frac);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.last.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

pub fn input_devices() -> Vec<DeviceInfo> {
    list(cpal::default_host().input_devices().ok().map(|d| d.collect()).unwrap_or_default())
}

pub fn output_devices() -> Vec<DeviceInfo> {
    list(cpal::default_host().output_devices().ok().map(|d| d.collect()).unwrap_or_default())
}

fn list(devices: Vec<cpal::Device>) -> Vec<DeviceInfo> {
    devices
        .into_iter()
        .filter_map(|d| Some(DeviceInfo { id: d.id().ok()?.to_string(), name: d.description().ok()?.name().to_string() }))
        .collect()
}

fn find(id: &str, input: bool) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if !id.is_empty()
        && let Ok(parsed) = id.parse::<cpal::DeviceId>()
        && let Some(d) = host.device_by_id(&parsed)
    {
        return Some(d);
    }
    // The saved device is gone (unplugged): use the default, without forgetting the choice.
    if input { host.default_input_device() } else { host.default_output_device() }
}

/// The microphone after processing: what the call publishes and what the meter shows.
pub struct Mic {
    /// Input volume, 0..2.
    pub gain: AtomicF32,
    /// Gate threshold as RMS; 0 is off.
    pub gate: AtomicF32,
    pub muted: AtomicBool,
    pub echo_cancellation: AtomicBool,
    pub noise_suppression: AtomicBool,
    /// Loudest 10 ms since last read, after the input volume, before the gate.
    pub level: AtomicF32,
    /// Whether the gate let the last frame through.
    pub open: AtomicBool,
    /// Where processed 10 ms frames go: the voice publisher, when there is one.
    sink: Mutex<Option<MicSink>>,
    users: AtomicU64,
}

/// Takes each processed 10 ms of microphone audio.
pub type MicSink = Box<dyn FnMut(&[i16]) + Send>;

/// Commands for the audio thread, which owns the device streams.
enum Command {
    Output(String),
    Input(Option<String>),
}

pub struct Audio {
    pub mixer: Arc<Mixer>,
    pub mic: Arc<Mic>,
    commands: mpsc::Sender<Command>,
    input_id: Mutex<String>,
}

static AUDIO: OnceLock<Audio> = OnceLock::new();

/// The one audio engine, started the first time it is needed.
pub fn audio() -> &'static Audio {
    AUDIO.get_or_init(Audio::start)
}

impl Audio {
    fn start() -> Audio {
        let mixer = Mixer::new();
        let mic = Arc::new(Mic {
            gain: AtomicF32::new(1.),
            gate: AtomicF32::new(0.),
            muted: AtomicBool::new(false),
            echo_cancellation: AtomicBool::new(true),
            noise_suppression: AtomicBool::new(true),
            level: AtomicF32::default(),
            open: AtomicBool::new(false),
            sink: Mutex::new(None),
            users: AtomicU64::new(0),
        });
        let near = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        {
            let (mixer, near) = (mixer.clone(), near.clone());
            std::thread::Builder::new().name("harmony-audio".into()).spawn(move || device_thread(rx, mixer, near)).ok();
        }
        {
            let (mixer, mic, near) = (mixer.clone(), mic.clone(), near.clone());
            std::thread::Builder::new().name("harmony-mic".into()).spawn(move || dsp_thread(mixer, mic, near)).ok();
        }
        let _ = tx.send(Command::Output(String::new()));
        Audio { mixer, mic, commands: tx, input_id: Mutex::new(String::new()) }
    }

    pub fn set_output(&self, id: &str) {
        let _ = self.commands.send(Command::Output(id.to_string()));
    }

    /// Reopens the capture only when the device actually changes.
    pub fn set_input(&self, id: &str) {
        {
            let mut current = self.input_id.lock();
            if *current == id {
                return;
            }
            *current = id.to_string();
        }
        if self.mic.users.load(Ordering::Relaxed) > 0 {
            let _ = self.commands.send(Command::Input(Some(id.to_string())));
        }
    }

    /// Opens the microphone for as long as the returned guard lives (a call, the settings meter).
    pub fn acquire_mic(&'static self) -> MicGuard {
        if self.mic.users.fetch_add(1, Ordering::Relaxed) == 0 {
            let _ = self.commands.send(Command::Input(Some(self.input_id.lock().clone())));
        }
        MicGuard(self)
    }

    /// Where processed microphone frames go (10 ms, mono, 48 kHz).
    pub fn set_mic_sink(&self, sink: Option<MicSink>) {
        *self.mic.sink.lock() = sink;
    }
}

pub struct MicGuard(&'static Audio);

impl Drop for MicGuard {
    fn drop(&mut self) {
        if self.0.mic.users.fetch_sub(1, Ordering::Relaxed) == 1 {
            let _ = self.0.commands.send(Command::Input(None));
        }
    }
}

fn device_thread(rx: mpsc::Receiver<Command>, mixer: Arc<Mixer>, near: Arc<(Mutex<VecDeque<f32>>, Condvar)>) {
    let mut output: Option<cpal::Stream> = None;
    let mut input: Option<cpal::Stream> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Output(id) => {
                output = None;
                match open_output(&id, mixer.clone()) {
                    Ok(s) => output = Some(s),
                    Err(e) => log::warn!("speakers did not open: {e}"),
                }
            }
            Command::Input(id) => {
                input = None;
                near.0.lock().clear();
                if let Some(id) = id {
                    match open_input(&id, near.clone()) {
                        Ok(s) => input = Some(s),
                        Err(e) => log::warn!("microphone did not open: {e}"),
                    }
                }
            }
        }
    }
    drop((output, input));
}

fn open_output(id: &str, mixer: Arc<Mixer>) -> anyhow::Result<cpal::Stream> {
    let device = find(id, false).ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let config = device.default_output_config()?;
    let (rate, channels) = (config.sample_rate(), config.channels() as usize);
    let mut resampler = Resampler::new(RATE, rate, 2);
    // Kept between callbacks: after the first few they only reuse their capacity.
    let mut mixed = Vec::new();
    let mut res = Vec::new();
    let mut ready: VecDeque<f32> = VecDeque::new();
    let fill = move |out: &mut [f32]| {
        let frames = out.len() / channels;
        // Mix in 48 kHz stereo blocks until the device's rate and size are covered.
        while ready.len() / 2 < frames {
            let need = ((frames - ready.len() / 2) as f64 * RATE as f64 / rate as f64).ceil() as usize + 2;
            mixed.resize(need * 2, 0.);
            mixer.mix(&mut mixed);
            res.clear();
            resampler.process(&mixed, &mut res);
            ready.extend(res.iter().copied());
        }
        for f in out.chunks_exact_mut(channels) {
            let (l, r) = (ready.pop_front().unwrap_or(0.), ready.pop_front().unwrap_or(0.));
            match channels {
                1 => f[0] = 0.5 * (l + r),
                _ => {
                    f[0] = l;
                    f[1] = r;
                    for x in f.iter_mut().skip(2) {
                        *x = 0.;
                    }
                }
            }
        }
    };
    let mut fill = fill;
    let err = |e| log::warn!("speakers: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(config.config(), move |o: &mut [f32], _| fill(o), err, None)?,
        cpal::SampleFormat::I16 => {
            let mut tmp = Vec::new();
            device.build_output_stream::<i16, _, _>(
                config.config(),
                move |o: &mut [i16], _| {
                    tmp.resize(o.len(), 0.);
                    fill(&mut tmp);
                    for (d, s) in o.iter_mut().zip(&tmp) {
                        *d = (s.clamp(-1., 1.) * 32767.) as i16;
                    }
                },
                err,
                None,
            )?
        }
        other => anyhow::bail!("speakers want {other:?} samples"),
    };
    stream.play()?;
    Ok(stream)
}

fn open_input(id: &str, near: Arc<(Mutex<VecDeque<f32>>, Condvar)>) -> anyhow::Result<cpal::Stream> {
    let device = find(id, true).ok_or_else(|| anyhow::anyhow!("no microphone"))?;
    let config = device.default_input_config()?;
    let (rate, channels) = (config.sample_rate(), config.channels() as usize);
    let mut resampler = Resampler::new(rate, RATE, 1);
    let mut mono = Vec::new();
    let mut out = Vec::new();
    let mut take = move |input: &[f32]| {
        mono.clear();
        mono.extend(input.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
        out.clear();
        resampler.process(&mono, &mut out);
        let (buf, cv) = &*near;
        let mut b = buf.lock();
        b.extend(out.iter().copied());
        let over = b.len().saturating_sub(RATE as usize / 2);
        b.drain(..over);
        cv.notify_one();
    };
    let err = |e| log::warn!("microphone: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(config.config(), move |i, _| take(i), err, None)?,
        cpal::SampleFormat::I16 => {
            let mut tmp = Vec::new();
            device.build_input_stream::<i16, _, _>(
                config.config(),
                move |i: &[i16], _| {
                    tmp.clear();
                    tmp.extend(i.iter().map(|&s| s as f32 / 32768.));
                    take(&tmp);
                },
                err,
                None,
            )?
        }
        other => anyhow::bail!("the microphone gives {other:?} samples"),
    };
    stream.play()?;
    Ok(stream)
}

/// Turns raw microphone audio into what the call sends: echo cancelled against the mix, noise
/// suppressed, at the input volume, gated.
fn dsp_thread(mixer: Arc<Mixer>, mic: Arc<Mic>, near: Arc<(Mutex<VecDeque<f32>>, Condvar)>) {
    let mut apm: Option<(bool, bool, AudioProcessingModule)> = None;
    let mut frame = vec![0f32; FRAME];
    let mut pcm = vec![0i16; FRAME];
    let mut far = vec![0i16; FRAME];
    // The speakers' mix, taken from the mixer whole and worked through here, so the output
    // callback never waits on the echo canceller for its lock.
    let mut far_in: VecDeque<f32> = VecDeque::new();
    let mut far_pending: VecDeque<f32> = VecDeque::new();
    let mut gate_gain = 0f32;
    let mut held_until = Instant::now();
    loop {
        {
            let (buf, cv) = &*near;
            let mut b = buf.lock();
            while b.len() < FRAME {
                cv.wait_for(&mut b, Duration::from_millis(200));
            }
            for (d, s) in frame.iter_mut().zip(b.drain(..FRAME)) {
                *d = s;
            }
        }
        let (aec, ns) = (mic.echo_cancellation.load(Ordering::Relaxed), mic.noise_suppression.load(Ordering::Relaxed));
        if apm.as_ref().is_none_or(|(a, n, _)| (*a, *n) != (aec, ns)) {
            apm = Some((aec, ns, AudioProcessingModule::new(aec, false, true, ns)));
        }
        let (_, _, module) = apm.as_mut().unwrap();
        // The far end first: everything the speakers played since the last frame. The swap
        // hands the mixer back an empty queue with its capacity.
        std::mem::swap(&mut *mixer.far.lock(), &mut far_in);
        far_pending.append(&mut far_in);
        let over = far_pending.len().saturating_sub(RATE as usize / 2);
        far_pending.drain(..over);
        while far_pending.len() >= FRAME {
            for (d, s) in far.iter_mut().zip(far_pending.drain(..FRAME)) {
                *d = (s.clamp(-1., 1.) * 32767.) as i16;
            }
            let _ = module.process_reverse_stream(&mut far, RATE as i32, 1);
        }
        for (d, s) in pcm.iter_mut().zip(&frame) {
            *d = (s.clamp(-1., 1.) * 32767.) as i16;
        }
        let _ = module.process_stream(&mut pcm, RATE as i32, 1);

        let gain = mic.gain.get();
        let mut sum = 0f32;
        for s in pcm.iter_mut() {
            let v = (*s as f32 * gain).clamp(-32768., 32767.);
            sum += (v / 32768.) * (v / 32768.);
            *s = v as i16;
        }
        let rms = (sum / FRAME as f32).sqrt();
        mic.level.max(rms);

        // The noise gate: opens fast, holds 300 ms, closes softly.
        let threshold = mic.gate.get();
        let open = threshold <= 0. || rms >= threshold;
        if open {
            held_until = Instant::now() + Duration::from_millis(300);
        }
        let target = if open || Instant::now() < held_until { 1. } else { 0. };
        let muted = mic.muted.load(Ordering::Relaxed);
        mic.open.store(target > 0. && !muted, Ordering::Relaxed);
        // Per-sample smoothing: about 5 ms to open, 40 ms to close.
        let coef = if target > gate_gain { 1. - (-1. / (0.005 * RATE as f32)).exp() } else { 1. - (-1. / (0.040 * RATE as f32)).exp() };
        for s in pcm.iter_mut() {
            gate_gain += (target - gate_gain) * coef;
            *s = if muted { 0 } else { (*s as f32 * gate_gain.clamp(0., 1.)) as i16 };
        }
        if let Some(sink) = mic.sink.lock().as_mut() {
            static FRAMES: AtomicU64 = AtomicU64::new(0);
            if FRAMES.fetch_add(1, Ordering::Relaxed).is_multiple_of(500) {
                log::debug!("microphone: frame to the publisher, rms {rms:.4}");
            }
            sink(&pcm);
        }
    }
}

/// A sine cue, the old client's join, leave, live and mention sounds.
pub fn cue(kind: Cue) {
    let notes: &[(f32, f32)] = match kind {
        Cue::Join => &[(587.33, 0.), (880., 0.09)],
        Cue::Leave => &[(880., 0.), (587.33, 0.09)],
        Cue::Live => &[(1046.5, 0.)],
        Cue::Mention => &[(880., 0.), (1174.66, 0.08), (1396.91, 0.16)],
    };
    let len = 0.22;
    let total = notes.iter().map(|n| n.1).fold(0., f32::max) + len;
    let mut out = vec![0f32; (total * RATE as f32) as usize];
    for &(freq, at) in notes {
        let start = (at * RATE as f32) as usize;
        let n = (len * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.015).min(1.);
            let release = (1. - t / len).max(0.).powi(2);
            if let Some(o) = out.get_mut(start + i) {
                *o += (std::f32::consts::TAU * freq * t).sin() * 0.07 * attack * release;
            }
        }
    }
    play(&out, 1, 1.);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cue {
    Join,
    Leave,
    Live,
    Mention,
}

/// Plays samples once (48 kHz) at a volume.
pub fn play(samples: &[f32], channels: usize, gain: f32) {
    let s = Source::new(SourceKind::Effect, gain, true);
    s.push(samples, channels);
    audio().mixer.add(s);
}

/// Plays a clip (interleaved stereo at 48 kHz) once without copying it, so a clip can be shared
/// by every play of it.
pub fn play_shared(samples: Arc<[i16]>, gain: f32) {
    let s = Source {
        kind: SourceKind::Effect,
        buf: Mutex::new(VecDeque::new()),
        gain: AtomicF32::new(gain),
        peak: AtomicF32::default(),
        one_shot: true,
        max_len: 0,
        clip: Some((samples, AtomicUsize::new(0))),
    };
    audio().mixer.add(Arc::new(s));
}

/// Decodes a sound file (mp3, wav, ogg/vorbis, flac) to 48 kHz stereo.
pub fn decode(bytes: Vec<u8>) -> anyhow::Result<Vec<f32>> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let probed = symphonia::default::get_probe().format(&Hint::new(), mss, &FormatOptions::default(), &MetadataOptions::default())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or_else(|| anyhow::anyhow!("no audio in the file"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
    let mut out = Vec::new();
    let mut resampler: Option<Resampler> = None;
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let spec = *decoded.spec();
        let ch = spec.channels.count();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let stereo: Vec<f32> = buf.samples().chunks_exact(ch).flat_map(|f| if ch == 1 { [f[0], f[0]] } else { [f[0], f[1]] }).collect();
        let r = resampler.get_or_insert_with(|| Resampler::new(spec.rate, RATE, 2));
        r.process(&stereo, &mut out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_length_in_proportion() {
        let mut r = Resampler::new(44_100, 48_000, 1);
        let mut out = Vec::new();
        for _ in 0..10 {
            r.process(&[0.5; 441], &mut out);
        }
        assert!((out.len() as i64 - 4800).abs() <= 2, "{}", out.len());
        assert!(out.iter().skip(2).all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn sources_drop_old_audio_rather_than_lag() {
        let s = Source::new(SourceKind::Voice, 1., false);
        s.push(&vec![0.1; RATE as usize], 1);
        assert!(s.buf.lock().len() <= MAX_VOICE_MS * RATE as usize / 1000 * 2);
    }

    #[test]
    fn the_mix_applies_gains_and_deafening() {
        let m = Mixer::new();
        let voice = Source::new(SourceKind::Voice, 2., false);
        let fx = Source::new(SourceKind::Effect, 1., true);
        voice.push(&[0.1; 4], 1);
        fx.push(&[0.2; 4], 1);
        m.add(voice.clone());
        m.add(fx);
        let mut out = vec![0.; 8];
        m.mix(&mut out);
        assert!((out[0] - 0.4).abs() < 1e-6);
        m.deafened.store(true, Ordering::Relaxed);
        voice.push(&[0.1; 4], 1);
        m.mix(&mut out);
        assert_eq!(out[0], 0.);
        assert_eq!(m.sources.lock().len(), 1, "the drained effect is gone");
    }
}
