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
    /// The cues' bus: what it plays goes through this on its way into the mix.
    limiter: Option<Mutex<Limiter>>,
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
            limiter: None,
        })
    }

    /// An effect that stays in the mix, through a limiter.
    fn limited() -> Arc<Source> {
        Arc::new(Source {
            kind: SourceKind::Effect,
            buf: Mutex::new(VecDeque::new()),
            gain: AtomicF32::new(1.),
            peak: AtomicF32::default(),
            one_shot: false,
            max_len: usize::MAX,
            clip: None,
            limiter: Some(Mutex::default()),
        })
    }

    /// Mono samples added onto what is still to play, from now, instead of queued after it.
    fn overlay(&self, samples: &[f32]) {
        let mut buf = self.buf.lock();
        let queued = buf.len() / 2;
        for (i, &s) in samples.iter().enumerate() {
            if i < queued {
                buf[2 * i] += s;
                buf[2 * i + 1] += s;
            } else {
                buf.push_back(s);
                buf.push_back(s);
            }
        }
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
            if let Some(limiter) = &s.limiter {
                let mut limiter = limiter.lock();
                let mut queued = buf.drain(..n);
                for o in out[..n].chunks_exact_mut(2) {
                    let (l, r) = (queued.next().unwrap_or(0.) * gain, queued.next().unwrap_or(0.) * gain);
                    let (l, r) = limiter.process(l, r);
                    o[0] += l;
                    o[1] += r;
                }
                continue;
            }
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

/// The little noises, as the old client's 3.x plays them. Rising means arriving or switching on,
/// falling means leaving or switching off; two notes for other people, a triad for you.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cue {
    /// Someone else arrived in your channel, or left it.
    Join,
    Leave,
    /// You connected to voice, or left it.
    Connect,
    Disconnect,
    /// A screen share started or stopped: brighter than arrivals, since it is news.
    StreamStart,
    StreamStop,
    /// Your microphone: short, so it is over before your next word.
    Mute,
    Unmute,
    /// Your ears: lower than mute, to tell them apart without looking.
    Deafen,
    Undeafen,
    Mention,
}

const C5: f32 = 523.25;
const D5: f32 = 587.33;
const E5: f32 = 659.25;
const G5: f32 = 783.99;
const A5: f32 = 880.;
const B5: f32 = 987.77;
const D6: f32 = 1174.66;
const E6: f32 = 1318.51;
const F6: f32 = 1396.91;
const D4: f32 = 293.66;
const A4: f32 = 440.;

impl Cue {
    /// Its notes (frequency, start in seconds), how long each rings, and its level.
    fn voicing(self) -> (&'static [(f32, f32)], f32, f32) {
        match self {
            Cue::Join => (&[(D5, 0.), (A5, 0.09)], 0.32, 1.),
            Cue::Leave => (&[(A5, 0.), (D5, 0.09)], 0.32, 1.),
            Cue::Connect => (&[(C5, 0.), (E5, 0.07), (G5, 0.14)], 0.4, 1.),
            Cue::Disconnect => (&[(G5, 0.), (E5, 0.07), (C5, 0.14)], 0.4, 1.),
            Cue::StreamStart => (&[(E5, 0.), (B5, 0.07), (E6, 0.14)], 0.45, 0.95),
            Cue::StreamStop => (&[(E6, 0.), (B5, 0.07), (E5, 0.14)], 0.45, 0.95),
            Cue::Mute => (&[(A5, 0.), (D5, 0.055)], 0.2, 0.85),
            Cue::Unmute => (&[(D5, 0.), (A5, 0.055)], 0.2, 0.85),
            Cue::Deafen => (&[(A4, 0.), (D4, 0.07)], 0.28, 0.9),
            Cue::Undeafen => (&[(D4, 0.), (A4, 0.07)], 0.28, 0.9),
            Cue::Mention => (&[(A5, 0.), (D6, 0.08), (F6, 0.16)], 0.3, 0.9),
        }
    }
}

/// Loudest a cue gets at 100%, as a fraction of full scale.
const CUE_PEAK: f32 = 0.32;
/// The partials of each note: multiple of the fundamental, relative level. A bare sine has no
/// harmonics, and the ear judges loudness largely by them.
const PARTIALS: [(f32, f32); 3] = [(1., 1.), (2., 0.32), (3., 0.1)];

/// From `from` to `to` over `span`, exponentially, as Web Audio ramps.
fn ramp(from: f32, to: f32, t: f32, span: f32) -> f32 {
    from * (to / from).powf((t / span).clamp(0., 1.))
}

/// A cue's samples, mono at 48 kHz, at `volume` (0..2, the sound effects volume).
pub fn render(kind: Cue, volume: f32) -> Vec<f32> {
    let (notes, len, level) = kind.voicing();
    let level = volume.clamp(0., 2.) * CUE_PEAK * level;
    if level <= 0. {
        return Vec::new();
    }
    let rate = RATE as f32;
    let total = notes.iter().map(|n| n.1).fold(0., f32::max) + len;
    let mut out = vec![0f32; (total * rate) as usize];
    let n = (len * rate) as usize;
    for &(freq, at) in notes {
        let start = (at * rate) as usize;
        let mut phase = [0f32; PARTIALS.len()];
        for i in 0..n {
            let t = i as f32 / rate;
            // A 6 ms strike, down to a third by a third of the way through, then away.
            let env = if t < 0.006 {
                ramp(0.0001, level, t, 0.006)
            } else if t < len * 0.35 {
                ramp(level, level * 0.35, t - 0.006, len * 0.35 - 0.006)
            } else {
                ramp(level * 0.35, 0.0001, t - len * 0.35, len * 0.65)
            };
            // The settle: 3% sharp at the strike, at pitch 40 ms later. Inaudible as a glide,
            // audible as a softer attack.
            let pitch = freq * ramp(1.03, 1., t, 0.04);
            let mut sum = 0.;
            for (p, &(multiple, weight)) in phase.iter_mut().zip(&PARTIALS) {
                *p = (*p + std::f32::consts::TAU * pitch * multiple / rate) % std::f32::consts::TAU;
                // Higher partials die faster, as on anything struck.
                sum += p.sin() * ramp(weight, weight * 0.05, t, len / multiple);
            }
            if let Some(o) = out.get_mut(start + i) {
                *o += sum * env;
            }
        }
    }
    out
}

/// Plays a cue at `volume` (0..2). Every cue goes through one limiter, so two landing together
/// (someone joins as someone else starts streaming) cannot clip.
pub fn cue(kind: Cue, volume: f32) {
    let samples = render(kind, volume);
    if !samples.is_empty() {
        cue_bus().overlay(&samples);
    }
}

fn cue_bus() -> &'static Arc<Source> {
    static BUS: OnceLock<Arc<Source>> = OnceLock::new();
    BUS.get_or_init(|| {
        let bus = Source::limited();
        audio().mixer.add(bus.clone());
        bus
    })
}

/// A compressor hard enough to call a limiter: above -6 dBFS, 12:1. The attack is instant, so
/// nothing gets past it; the release takes about 120 ms.
#[derive(Default)]
struct Limiter {
    envelope: f32,
}

const LIMIT_AT: f32 = 0.5;
const LIMIT_RATIO: f32 = 12.;

impl Limiter {
    fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let release = (-1. / (0.12 * RATE as f32)).exp();
        let peak = l.abs().max(r.abs());
        self.envelope = if peak > self.envelope { peak } else { self.envelope * release };
        if self.envelope <= LIMIT_AT {
            return (l, r);
        }
        let g = LIMIT_AT * (self.envelope / LIMIT_AT).powf(1. / LIMIT_RATIO) / self.envelope;
        (l * g, r * g)
    }
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
        limiter: None,
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

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0., |p, s| p.max(s.abs()))
    }

    /// 16-bit mono at 48 kHz, for listening to what a test rendered.
    fn write_wav(path: &std::path::Path, samples: &[f32]) {
        let data: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1., 1.) * 32767.) as i16).to_le_bytes()).collect();
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        for v in [16u32, 1 | 1 << 16, RATE, RATE * 2, 2 | 16 << 16] {
            w.extend_from_slice(&v.to_le_bytes());
        }
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        std::fs::write(path, w).unwrap();
    }

    /// Set `HARMONY_CUE_WAVS` to a folder to get every cue, and the worst overlap, as WAV files.
    #[test]
    fn cues_are_loud_and_two_together_do_not_clip() {
        let wavs = std::env::var_os("HARMONY_CUE_WAVS").map(std::path::PathBuf::from);
        use Cue::*;
        for kind in [Join, Leave, Connect, Disconnect, StreamStart, StreamStop, Mute, Unmute, Deafen, Undeafen, Mention] {
            let s = render(kind, 1.);
            let p = peak(&s);
            println!("{kind:?}: {:.0} ms, peak {p:.3}", s.len() as f32 / RATE as f32 * 1000.);
            assert!((0.2..0.5).contains(&p), "{kind:?} peaks at {p}");
            assert!(s.iter().all(|x| x.is_finite()));
            assert!(s.last().unwrap().abs() < 0.001, "{kind:?} ends in a click");
            if let Some(dir) = &wavs {
                write_wav(&dir.join(format!("{kind:?}.wav")), &s);
            }
        }
        assert!(render(Cue::Join, 0.).is_empty());
        assert!((peak(&render(Cue::Join, 2.)) / peak(&render(Cue::Join, 1.)) - 2.).abs() < 0.01);

        // The two longest at 200%, the second 10 ms after the first: past full scale summed.
        let (a, b) = (render(Cue::StreamStart, 2.), render(Cue::Connect, 2.));
        let lag = 480;
        let raw: Vec<f32> = (0..a.len().max(b.len() + lag))
            .map(|i| a.get(i).unwrap_or(&0.) + i.checked_sub(lag).and_then(|j| b.get(j)).unwrap_or(&0.))
            .collect();
        println!("overlap at 200%: raw peak {:.3}", peak(&raw));
        assert!(peak(&raw) > 1.);
        let m = Mixer::new();
        let bus = Source::limited();
        m.add(bus.clone());
        bus.overlay(&a);
        let mut out = vec![0.; 2 * lag];
        m.mix(&mut out);
        let mut mixed: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
        bus.overlay(&b);
        let mut out = vec![0.; 2 * raw.len()];
        m.mix(&mut out);
        mixed.extend(out.chunks_exact(2).map(|f| f[0]));
        let limited = peak(&mixed);
        println!("overlap at 200%: through the limiter {limited:.3}");
        // Under the soft clip's knee too: the limiter did this, not the clip.
        assert!(limited < 0.9, "{limited}");
        assert!(bus.is_empty() && m.sources.lock().len() == 1, "the bus stays for the next cue");
        if let Some(dir) = &wavs {
            write_wav(&dir.join("overlap-raw.wav"), &raw);
            write_wav(&dir.join("overlap-limited.wav"), &mixed);
        }
    }
}
