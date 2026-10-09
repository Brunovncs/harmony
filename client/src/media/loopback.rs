//! What the computer is playing, for a screen share's sound, through Windows' process loopback
//! (Windows 10 2004 and later): everything except Harmony's own process tree, so viewers don't
//! hear the call back, or only one application's, for a window share.

use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_source::native::NativeAudioSource;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Everything the computer plays except this process tree.
    Excluding(u32),
    /// Only this process tree.
    Only(u32),
}

pub struct Loopback {
    stop: Arc<AtomicBool>,
    pub source: NativeAudioSource,
}

impl Loopback {
    pub fn start(mode: Mode) -> Loopback {
        let _rt = crate::core::runtime().enter();
        let source = NativeAudioSource::new(
            AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false },
            RATE,
            CHANNELS,
            0,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (source.clone(), stop.clone());
        std::thread::Builder::new()
            .name("harmony-loopback".into())
            .spawn(move || {
                #[cfg(windows)]
                if let Err(e) = imp::run(mode, &s, &st) {
                    log::warn!("system sound for the share is unavailable: {e}");
                }
                let _ = (&s, &st, mode);
            })
            .ok();
        Loopback { stop, source }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sends 10 ms frames of interleaved stereo i16 to the source, buffering the remainder.
struct Framer {
    pending: Vec<i16>,
}

impl Framer {
    fn push(&mut self, source: &NativeAudioSource, samples: &[i16]) {
        self.pending.extend_from_slice(samples);
        let frame = (RATE / 100 * CHANNELS) as usize;
        while self.pending.len() >= frame {
            let chunk: Vec<i16> = self.pending.drain(..frame).collect();
            let f = AudioFrame { data: chunk.into(), sample_rate: RATE, num_channels: CHANNELS, samples_per_channel: RATE / 100 };
            let _ = futures::executor::block_on(source.capture_frame(&f));
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use parking_lot::{Condvar, Mutex};
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
    use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{IUnknown, Interface, implement};

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Done(Arc<(Mutex<bool>, Condvar)>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
        fn ActivateCompleted(&self, _: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
            let (m, cv) = &*self.0;
            *m.lock() = true;
            cv.notify_all();
            Ok(())
        }
    }

    pub fn run(mode: Mode, source: &NativeAudioSource, stop: &AtomicBool) -> anyhow::Result<()> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let result = capture(mode, source, stop);
            CoUninitialize();
            result
        }
    }

    unsafe fn capture(mode: Mode, source: &NativeAudioSource, stop: &AtomicBool) -> anyhow::Result<()> {
        let (pid, loopback_mode) = match mode {
            Mode::Excluding(pid) => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
            Mode::Only(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        };
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: loopback_mode },
            },
        };
        // Never dropped: PROPVARIANT's drop is PropVariantClear, which hands a VT_BLOB's data to
        // CoTaskMemFree, and this blob points at `params` on the stack. Freeing it corrupted the
        // heap, and Harmony crashed as the share's sound stopped.
        let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                    ..Default::default()
                }),
            },
        });
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let handler: IActivateAudioInterfaceCompletionHandler = Done(signal.clone()).into();
        let op = unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop), &handler)? };
        {
            let (m, cv) = &*signal;
            let mut done = m.lock();
            while !*done {
                if cv.wait_for(&mut done, std::time::Duration::from_secs(5)).timed_out() {
                    anyhow::bail!("Windows did not start the loopback");
                }
            }
        }
        let mut hr = windows::core::HRESULT(0);
        let mut unknown: Option<IUnknown> = None;
        unsafe { op.GetActivateResult(&mut hr, &mut unknown)? };
        hr.ok()?;
        let client: IAudioClient = unknown.ok_or_else(|| anyhow::anyhow!("no audio client"))?.cast()?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * CHANNELS * 2,
            nBlockAlign: (CHANNELS * 2) as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                200_000,
                0,
                &format,
                None,
            )?;
        }
        let event = unsafe { CreateEventW(None, false, false, None)? };
        unsafe { client.SetEventHandle(event)? };
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        unsafe { client.Start()? };
        let mut framer = Framer { pending: Vec::new() };
        while !stop.load(Ordering::Relaxed) {
            if unsafe { WaitForSingleObject(event, 200) } != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                let packet = unsafe { capture.GetNextPacketSize()? };
                if packet == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
                let n = (frames * CHANNELS) as usize;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    framer.push(source, &vec![0i16; n]);
                } else {
                    let samples = unsafe { std::slice::from_raw_parts(data as *const i16, n) };
                    framer.push(source, samples);
                }
                unsafe { capture.ReleaseBuffer(frames)? };
            }
        }
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(event);
        }
        Ok(())
    }
}

/// The process that owns a window, for capturing only that application's sound.
#[cfg(windows)]
pub fn window_process(hwnd: u64) -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd as _, &mut pid) };
    (pid != 0).then_some(pid)
}

#[cfg(not(windows))]
pub fn window_process(_: u64) -> Option<u32> {
    None
}
