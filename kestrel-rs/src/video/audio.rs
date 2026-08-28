//! Sound output.
//!
//! Kestrel ships as an archive you copy onto a machine and run, so audio must
//! not turn ALSA into a build-time or install-time requirement. Two consequences
//! shape this module:
//!
//!   * `libasound.so.2` is opened with `dlopen` at runtime rather than linked.
//!     Nothing is needed to *build* Kestrel, and a machine without ALSA still
//!     runs it — video plays and audio reports itself unavailable.
//!   * The library is not bundled. libasound finds its output plugins and its
//!     configuration through system paths, which is how it reaches PipeWire or
//!     PulseAudio; a copy carried in the archive would look for both in
//!     directories that do not exist on the target and fail to open a device at
//!     all. The system's own copy is the one that works.
//!
//! Only the handful of entry points a playback-only client needs are bound.

use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr, CString};
use std::sync::Arc;

use libloading::{Library, Symbol};
use log::{debug, info, warn};

/// SND_PCM_STREAM_PLAYBACK
const STREAM_PLAYBACK: c_int = 0;
/// SND_PCM_FORMAT_S16_LE
const FORMAT_S16_LE: c_int = 2;
/// SND_PCM_ACCESS_RW_INTERLEAVED
const ACCESS_RW_INTERLEAVED: c_int = 3;

/// How much audio the device buffers. Long enough to ride out a scheduling
/// hiccup, short enough that sound stays with the picture.
const LATENCY_US: c_uint = 120_000;

type PcmOpen = unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int, c_int) -> c_int;
type PcmSetParams =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int;
type PcmWritei = unsafe extern "C" fn(*mut c_void, *const c_void, c_ulong) -> c_long;
type PcmRecover = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type PcmClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type PcmDrop = unsafe extern "C" fn(*mut c_void) -> c_int;
type StrError = unsafe extern "C" fn(c_int) -> *const c_char;

#[allow(non_camel_case_types)]
type c_ulong = std::ffi::c_ulong;

/// The pieces of libasound this client uses, resolved once.
struct Alsa {
    // Keeps the library mapped: the function pointers below point into it.
    _library: Library,
    open: PcmOpen,
    set_params: PcmSetParams,
    writei: PcmWritei,
    recover: PcmRecover,
    close: PcmClose,
    drop_pending: PcmDrop,
    strerror: StrError,
}

impl Alsa {
    fn load() -> Option<Arc<Alsa>> {
        // Only the SONAME is tried. "libasound.so" is part of the development
        // package and is absent on the machines this has to run on.
        let library = match unsafe { Library::new("libasound.so.2") } {
            Ok(library) => library,
            Err(err) => {
                info!("no ALSA on this system, so audio is unavailable: {err}");
                return None;
            }
        };

        // SAFETY: each signature matches alsa-lib's public API, and the library
        // is moved into the struct so the pointers stay valid for its lifetime.
        unsafe {
            let get = |name: &[u8]| -> Option<*const c_void> {
                let symbol: Symbol<*const c_void> = library.get(name).ok()?;
                Some(*symbol)
            };
            let open = *library.get::<PcmOpen>(b"snd_pcm_open\0").ok()?;
            let set_params = *library.get::<PcmSetParams>(b"snd_pcm_set_params\0").ok()?;
            let writei = *library.get::<PcmWritei>(b"snd_pcm_writei\0").ok()?;
            let recover = *library.get::<PcmRecover>(b"snd_pcm_recover\0").ok()?;
            let close = *library.get::<PcmClose>(b"snd_pcm_close\0").ok()?;
            let drop_pending = *library.get::<PcmDrop>(b"snd_pcm_drop\0").ok()?;
            let strerror = *library.get::<StrError>(b"snd_strerror\0").ok()?;
            let _ = get;

            Some(Arc::new(Alsa {
                _library: library,
                open,
                set_params,
                writei,
                recover,
                close,
                drop_pending,
                strerror,
            }))
        }
    }

    fn message(&self, code: c_int) -> String {
        // SAFETY: snd_strerror returns a static NUL-terminated string.
        unsafe {
            let text = (self.strerror)(code);
            if text.is_null() {
                format!("error {code}")
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        }
    }
}

/// An open playback device.
pub struct Playback {
    alsa: Arc<Alsa>,
    pcm: *mut c_void,
    pub rate: u32,
    pub channels: u16,
}

// The handle is only ever used from the thread that owns the Playback, but that
// thread is not the one that created it.
unsafe impl Send for Playback {}

impl Playback {
    /// Open the default device for 16-bit interleaved playback.
    ///
    /// `rate` is a request, not a guarantee: ALSA is told it may resample, so a
    /// device that cannot do 16 kHz still plays the cameras' audio.
    pub fn open(rate: u32, channels: u16) -> Option<Playback> {
        let alsa = Alsa::load()?;
        let name = CString::new("default").ok()?;
        let mut pcm: *mut c_void = std::ptr::null_mut();

        // SAFETY: the pointers are valid for the duration of the calls.
        unsafe {
            let code = (alsa.open)(&mut pcm, name.as_ptr(), STREAM_PLAYBACK, 0);
            if code < 0 {
                warn!("could not open an audio device: {}", alsa.message(code));
                return None;
            }
            let code = (alsa.set_params)(
                pcm,
                FORMAT_S16_LE,
                ACCESS_RW_INTERLEAVED,
                channels as c_uint,
                rate as c_uint,
                1, // allow the library to resample
                LATENCY_US,
            );
            if code < 0 {
                warn!("audio device rejected {rate} Hz: {}", alsa.message(code));
                (alsa.close)(pcm);
                return None;
            }
        }

        debug!("audio device open at {rate} Hz, {channels} channel(s)");
        Some(Playback {
            alsa,
            pcm,
            rate,
            channels,
        })
    }

    /// Write interleaved samples, blocking until the device has taken them.
    ///
    /// An underrun is recovered from rather than reported: it means the network
    /// or the decoder fell behind, which is normal for live video, and the right
    /// response is to carry on rather than tear the stream down.
    pub fn write(&mut self, samples: &[i16]) -> bool {
        if samples.is_empty() || self.channels == 0 {
            return true;
        }
        let mut offset = 0usize;
        let frame = self.channels as usize;

        while offset < samples.len() {
            let frames = (samples.len() - offset) / frame;
            if frames == 0 {
                break;
            }
            // SAFETY: the slice covers `frames * channels` samples from offset.
            let written = unsafe {
                (self.alsa.writei)(
                    self.pcm,
                    samples[offset..].as_ptr() as *const c_void,
                    frames as c_ulong,
                )
            };
            if written < 0 {
                // SAFETY: recovering a PCM handle we own.
                let recovered = unsafe { (self.alsa.recover)(self.pcm, written as c_int, 1) };
                if recovered < 0 {
                    warn!("audio stopped: {}", self.alsa.message(recovered as c_int));
                    return false;
                }
                continue;
            }
            offset += written as usize * frame;
        }
        true
    }

    /// Throw away anything still queued, for when playback is switched off and
    /// the last second of the previous camera should not keep playing.
    pub fn discard(&mut self) {
        // SAFETY: the handle is ours and still open.
        unsafe {
            (self.alsa.drop_pending)(self.pcm);
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        // SAFETY: closing a handle we opened, exactly once.
        unsafe {
            (self.alsa.drop_pending)(self.pcm);
            (self.alsa.close)(self.pcm);
        }
    }
}

/// Whether this system can play sound at all.
pub fn available() -> bool {
    Alsa::load().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the whole runtime-loading path: dlopen, symbol resolution, device
    /// open, and a real write that the device accepts.
    ///
    /// Skipped rather than failed where there is no sound card, because that is
    /// exactly the situation this module is built to survive — a headless
    /// machine must still run Kestrel.
    #[test]
    fn a_tone_reaches_the_sound_card() {
        let Some(mut device) = Playback::open(16_000, 1) else {
            eprintln!("no audio device here; skipping");
            return;
        };
        assert_eq!(device.rate, 16_000);

        // A tenth of a second of 440 Hz, quiet.
        let samples: Vec<i16> = (0..1_600)
            .map(|n| {
                let t = n as f32 / 16_000.0;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 2_000.0) as i16
            })
            .collect();
        assert!(device.write(&samples), "the device should accept samples");
        device.discard();
    }

    #[test]
    fn reporting_availability_never_panics() {
        // Whatever the answer, asking must be safe on any machine.
        let _ = available();
    }
}
