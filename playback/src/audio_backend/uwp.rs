use crate::audio_backend::{Open, Sink, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;

use std::ffi::{CStr, CString, c_char};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

#[path = "uwp_ring.rs"]
mod ring;
#[path = "uwp_wasapi.rs"]
mod wasapi;
#[path = "uwp_windows.rs"]
mod windows_native;
#[path = "uwp_xaudio2.rs"]
mod xaudio2;
#[path = "uwp_xaudio2_endpoint.rs"]
mod xaudio2_endpoint;

use ring::RingBufferSink;
use wasapi::WasapiSink;
use xaudio2::XAudio2Sink;
use xaudio2_endpoint::XAudio2EndpointSink;

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum WindowsAudioBackend {
    RingBuffer = 0,
    Wasapi = 1,
    XAudio2 = 2,
}

impl WindowsAudioBackend {
    fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::RingBuffer),
            1 => Some(Self::Wasapi),
            2 => Some(Self::XAudio2),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum EffectsPreset {
    None = 0,
    BassBoost = 1,
    VocalBoost = 2,
    Warm = 3,
    Equalizer = 4,
}

impl EffectsPreset {
    fn from_raw(value: u32) -> Self {
        match value {
            1 => Self::BassBoost,
            2 => Self::VocalBoost,
            3 => Self::Warm,
            4 => Self::Equalizer,
            _ => Self::None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct EffectsConfig {
    pub preset: EffectsPreset,
    pub strength: f32,
    pub echo: bool,
    pub reverb: bool,
    pub limiter: bool,
    pub equalizer_gains_db: [f32; 5],
    pub version: u64,
}

impl Default for EffectsConfig {
    fn default() -> Self {
        Self {
            preset: EffectsPreset::None,
            strength: 1.0,
            echo: false,
            reverb: false,
            limiter: false,
            equalizer_gains_db: [0.0; 5],
            version: 0,
        }
    }
}

static SELECTED_BACKEND: AtomicU32 = AtomicU32::new(WindowsAudioBackend::XAudio2 as u32);
static BACKEND_VERSION: AtomicU64 = AtomicU64::new(1);
static SELECTION_VALIDATED: AtomicBool = AtomicBool::new(false);
static NATIVE_GENERATION: AtomicU64 = AtomicU64::new(0);
static OUTPUT_DEVICE: OnceLock<RwLock<String>> = OnceLock::new();
static EFFECTS: OnceLock<RwLock<EffectsConfig>> = OnceLock::new();
static LAST_BACKEND_ERROR: OnceLock<RwLock<String>> = OnceLock::new();

fn output_device_store() -> &'static RwLock<String> {
    OUTPUT_DEVICE.get_or_init(|| RwLock::new(String::new()))
}

fn effects_store() -> &'static RwLock<EffectsConfig> {
    EFFECTS.get_or_init(|| RwLock::new(EffectsConfig::default()))
}

fn backend_error_store() -> &'static RwLock<String> {
    LAST_BACKEND_ERROR.get_or_init(|| RwLock::new(String::new()))
}

fn set_backend_error(message: impl Into<String>) {
    if let Ok(mut error) = backend_error_store().write() {
        *error = message.into();
    }
}

fn clear_backend_error() {
    set_backend_error(String::new());
}

fn selected_backend() -> WindowsAudioBackend {
    WindowsAudioBackend::from_raw(SELECTED_BACKEND.load(Ordering::Acquire))
        .unwrap_or(WindowsAudioBackend::XAudio2)
}

pub(super) fn backend_selection_version() -> u64 {
    BACKEND_VERSION.load(Ordering::Acquire)
}

fn selected_device() -> String {
    output_device_store()
        .read()
        .map(|value| value.clone())
        .unwrap_or_default()
}

pub(super) fn effects_snapshot() -> EffectsConfig {
    effects_store()
        .read()
        .map(|value| value.clone())
        .unwrap_or_default()
}

enum ActiveSink {
    None,
    Ring(RingBufferSink),
    Wasapi(WasapiSink),
    XAudio2(XAudio2Sink),
    XAudio2Endpoint(XAudio2EndpointSink),
}

impl ActiveSink {
    fn start(&mut self) -> SinkResult<()> {
        match self {
            Self::None => Ok(()),
            Self::Ring(sink) => sink.start(),
            Self::Wasapi(sink) => sink.start(),
            Self::XAudio2(sink) => sink.start(),
            Self::XAudio2Endpoint(sink) => sink.start(),
        }
    }

    fn stop(&mut self) -> SinkResult<()> {
        match self {
            Self::None => Ok(()),
            Self::Ring(sink) => sink.stop(),
            Self::Wasapi(sink) => sink.stop(),
            Self::XAudio2(sink) => sink.stop(),
            Self::XAudio2Endpoint(sink) => sink.stop(),
        }
    }
}

/// Runtime-switchable UWP audio sink.
///
/// RingBuffer preserves the original Rust-to-C# AudioGraph pipeline. WASAPI and
/// XAudio2 render inside this Rust DLL and never create or consume that ring.
pub struct UwpSink {
    format: AudioFormat,
    active: ActiveSink,
    applied_version: u64,
    running: bool,
}

impl UwpSink {
    pub const NAME: &'static str = "uwp";

    fn create_active(
        format: AudioFormat,
        backend: WindowsAudioBackend,
        device: &str,
    ) -> SinkResult<ActiveSink> {
        match backend {
            WindowsAudioBackend::RingBuffer => Ok(ActiveSink::Ring(RingBufferSink::new(format))),
            WindowsAudioBackend::Wasapi => {
                WasapiSink::new(SAMPLE_RATE, CHANNELS, device).map(ActiveSink::Wasapi)
            }
            WindowsAudioBackend::XAudio2 => match XAudio2Sink::new(SAMPLE_RATE, CHANNELS, device) {
                Ok(sink) => Ok(ActiveSink::XAudio2(sink)),
                Err(xaudio_error) if !device.is_empty() => {
                    log::warn!(
                        "XAudio2 rejected explicit endpoint {device:?} ({xaudio_error}); using the Rust effects compatibility renderer with native WASAPI endpoint transport"
                    );
                    XAudio2EndpointSink::new(SAMPLE_RATE, CHANNELS, device)
                            .map(ActiveSink::XAudio2Endpoint)
                            .map_err(|wasapi_error| {
                                SinkError::NotConnected(format!(
                                    "XAudio2 endpoint failed ({xaudio_error}); compatibility endpoint failed ({wasapi_error})"
                                ))
                            })
                }
                Err(error) => Err(error),
            },
        }
    }

    fn probe(backend: WindowsAudioBackend, device: &str) -> SinkResult<()> {
        if backend == WindowsAudioBackend::RingBuffer {
            return if ring::ensure_buffer_allocated() {
                Ok(())
            } else {
                Err(SinkError::NotConnected(
                    "unable to allocate the managed ring buffer".into(),
                ))
            };
        }

        drop(Self::create_active(AudioFormat::F32, backend, device)?);
        Ok(())
    }

    fn ensure_selected(&mut self) -> SinkResult<()> {
        let requested_version = backend_selection_version();
        if self.applied_version == requested_version && !matches!(self.active, ActiveSink::None) {
            return Ok(());
        }

        let requested_backend = selected_backend();
        let requested_device = selected_device();
        let mut replacement =
            Self::create_active(self.format, requested_backend, &requested_device)?;

        if self.running {
            replacement.start()?;
        }

        // Do not tear down healthy playback until the replacement has opened
        // and, when appropriate, started successfully.
        let mut previous = std::mem::replace(&mut self.active, replacement);
        self.applied_version = requested_version;
        if let Err(error) = previous.stop() {
            log::warn!("Unable to stop the previous UWP audio backend cleanly: {error}");
        }

        log::info!(
            "UWP audio backend active: {:?}, device={:?}",
            requested_backend,
            requested_device
        );
        Ok(())
    }

    fn packet_as_f32(
        packet: AudioPacket,
        converter: &mut Converter,
    ) -> SinkResult<(Vec<f32>, usize)> {
        match packet {
            AudioPacket::Samples(samples) => {
                let sample_count = samples.len();
                let converted: Vec<f32> = converter.f64_to_f32(&samples).to_vec();
                Ok((converted, sample_count))
            }
            AudioPacket::Raw(_) => Err(SinkError::InvalidParams(
                "native Windows PCM backends do not accept passthrough packets".into(),
            )),
        }
    }

    fn bytes_per_sample(format: AudioFormat) -> usize {
        match format {
            AudioFormat::F64 => 8,
            AudioFormat::F32 | AudioFormat::S32 | AudioFormat::S24 => 4,
            AudioFormat::S24_3 => 3,
            AudioFormat::S16 => 2,
        }
    }
}

impl Open for UwpSink {
    fn open(_device: Option<String>, format: AudioFormat) -> Self {
        Self {
            format,
            active: ActiveSink::None,
            applied_version: 0,
            running: false,
        }
    }
}

impl Sink for UwpSink {
    fn begin_generation(&mut self) -> u64 {
        if let Err(err) = self.ensure_selected() {
            log::error!("Failed to select UWP audio backend at generation boundary: {err}");
        }

        match &mut self.active {
            ActiveSink::Ring(sink) => sink.begin_generation(),
            ActiveSink::Wasapi(sink) => {
                let _ = sink.flush();
                NATIVE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
            }
            ActiveSink::XAudio2(sink) => {
                let _ = sink.flush();
                NATIVE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
            }
            ActiveSink::XAudio2Endpoint(sink) => {
                let _ = sink.flush();
                NATIVE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
            }
            ActiveSink::None => NATIVE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1,
        }
    }

    fn start(&mut self) -> SinkResult<()> {
        let was_running = self.running;
        self.ensure_selected()?;

        // A runtime replacement is started by ensure_selected while playback
        // is already running. Avoid issuing a second Start call to that sink.
        if was_running {
            return Ok(());
        }

        let result = match &mut self.active {
            ActiveSink::None => Ok(()),
            ActiveSink::Ring(sink) => sink.start(),
            ActiveSink::Wasapi(sink) => sink.start(),
            ActiveSink::XAudio2(sink) => sink.start(),
            ActiveSink::XAudio2Endpoint(sink) => sink.start(),
        };
        if result.is_ok() {
            self.running = true;
        }
        result
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.running = false;
        self.active.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        self.ensure_selected()?;
        if matches!(self.active, ActiveSink::Ring(_)) {
            if let ActiveSink::Ring(sink) = &mut self.active {
                return sink.write(packet, converter);
            }
        }

        let (samples, sample_count) = Self::packet_as_f32(packet, converter)?;
        match &mut self.active {
            ActiveSink::Wasapi(sink) => sink.write_f32(&samples)?,
            ActiveSink::XAudio2(sink) => sink.write_f32(&samples)?,
            ActiveSink::XAudio2Endpoint(sink) => sink.write_f32(&samples)?,
            ActiveSink::None | ActiveSink::Ring(_) => {
                return Err(SinkError::NotConnected(
                    "native Windows audio backend was not initialized".into(),
                ));
            }
        }

        super::TOTAL_WRITTEN.fetch_add(
            sample_count * Self::bytes_per_sample(self.format),
            Ordering::SeqCst,
        );
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_audio_set_backend(kind: u32, device_id: *const c_char) -> bool {
    let Some(kind) = WindowsAudioBackend::from_raw(kind) else {
        set_backend_error(format!("Unknown audio backend value {kind}"));
        return false;
    };

    let device = if device_id.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(device_id) }
            .to_string_lossy()
            .into_owned()
    };

    if kind == WindowsAudioBackend::RingBuffer && !ring::ensure_buffer_allocated() {
        set_backend_error("Unable to allocate the managed ring buffer");
        return false;
    }

    let current_device = selected_device();
    let device_changed = current_device != device;
    let backend_changed = SELECTED_BACKEND.load(Ordering::Acquire) != kind as u32;
    if !backend_changed && !device_changed && SELECTION_VALIDATED.load(Ordering::Acquire) {
        clear_backend_error();
        return true;
    }

    // Validate the complete native graph before publishing the new selection.
    // This allows the managed settings workflow to roll back without stopping
    // the active renderer when a platform rejects a backend or endpoint.
    if let Err(error) = UwpSink::probe(kind, &device) {
        let message =
            format!("Audio backend probe failed for {kind:?}, device={device:?}: {error}");
        set_backend_error(&message);
        log::error!("{message}");
        return false;
    }

    if let Ok(mut selected) = output_device_store().write() {
        *selected = device;
    } else {
        set_backend_error("Unable to update the selected audio output device");
        return false;
    }

    if backend_changed || device_changed {
        SELECTED_BACKEND.store(kind as u32, Ordering::Release);
        BACKEND_VERSION.fetch_add(1, Ordering::AcqRel);
    }
    SELECTION_VALIDATED.store(true, Ordering::Release);
    clear_backend_error();
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_get_backend() -> u32 {
    SELECTED_BACKEND.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_get_last_error() -> *mut c_char {
    let message = backend_error_store()
        .read()
        .map(|value| value.clone())
        .unwrap_or_else(|_| "Unable to read the native audio error".to_string());
    if message.is_empty() {
        return std::ptr::null_mut();
    }

    CString::new(message)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_audio_set_effects(
    preset: u32,
    strength: f32,
    echo: bool,
    reverb: bool,
    limiter: bool,
    gains_db: *const f32,
    gain_count: usize,
) {
    let mut gains = [0.0f32; 5];
    if !gains_db.is_null() {
        let count = gain_count.min(gains.len());
        let source = unsafe { std::slice::from_raw_parts(gains_db, count) };
        gains[..count].copy_from_slice(source);
    }
    for gain in &mut gains {
        *gain = gain.clamp(-18.0, 18.0);
    }

    if let Ok(mut config) = effects_store().write() {
        config.preset = EffectsPreset::from_raw(preset);
        config.strength = if strength.is_finite() {
            strength.clamp(0.0, 1.0)
        } else {
            1.0
        };
        config.echo = echo;
        config.reverb = reverb;
        config.limiter = limiter;
        config.equalizer_gains_db = gains;
        config.version = config.version.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_values_are_stable_for_ffi() {
        assert_eq!(WindowsAudioBackend::RingBuffer as u32, 0);
        assert_eq!(WindowsAudioBackend::Wasapi as u32, 1);
        assert_eq!(WindowsAudioBackend::XAudio2 as u32, 2);
        assert_eq!(WindowsAudioBackend::from_raw(99), None);
        assert_eq!(selected_backend(), WindowsAudioBackend::XAudio2);
    }

    #[test]
    fn effects_input_is_clamped() {
        let gains = [-100.0, -9.0, 0.0, 9.0, 100.0];
        unsafe {
            librespot_audio_set_effects(4, 2.0, true, false, true, gains.as_ptr(), gains.len());
        }
        let config = effects_snapshot();
        assert_eq!(config.preset, EffectsPreset::Equalizer);
        assert_eq!(config.strength, 1.0);
        assert_eq!(config.equalizer_gains_db, [-18.0, -9.0, 0.0, 9.0, 18.0]);
    }

    #[test]
    fn ring_backend_allocates_its_managed_player_buffer() {
        assert!(ring::ensure_buffer_allocated());
        assert!(!unsafe { ring::librespot_audio_get_buffer() }.is_null());
        assert_eq!(ring::librespot_audio_get_capacity(), 128 * 1024);
    }

    #[test]
    #[ignore = "requires a Windows audio endpoint"]
    fn wasapi_backend_opens_and_renders_silence() {
        let device_id = std::env::var("LIBRESPOT_TEST_AUDIO_DEVICE_ID").unwrap_or_default();
        let mut sink = WasapiSink::new(SAMPLE_RATE, CHANNELS, &device_id).unwrap();
        sink.write_f32(&vec![0.0; SAMPLE_RATE as usize / 20 * CHANNELS as usize])
            .unwrap();
        sink.stop().unwrap();
    }

    #[test]
    #[ignore = "requires a Windows audio endpoint"]
    fn xaudio2_backend_opens_and_renders_tone() {
        let started = std::time::Instant::now();
        let mut sink = XAudio2Sink::new(SAMPLE_RATE, CHANNELS, "").unwrap();
        let half_second: Vec<f32> = (0..SAMPLE_RATE as usize / 2)
            .flat_map(|frame| {
                let phase = std::f32::consts::TAU * 440.0 * frame as f32 / SAMPLE_RATE as f32;
                [phase.sin() * 0.1; CHANNELS as usize]
            })
            .collect();
        sink.write_f32(&half_second).unwrap();

        let gains = [6.0, -3.0, 4.0, -2.0, 5.0];
        unsafe {
            librespot_audio_set_effects(4, 0.75, true, true, true, gains.as_ptr(), gains.len());
        }
        sink.write_f32(&half_second).unwrap();
        sink.stop().unwrap();
        unsafe {
            librespot_audio_set_effects(0, 0.0, false, false, false, std::ptr::null(), 0);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(850)
                && elapsed <= std::time::Duration::from_millis(1_350),
            "one second of XAudio2 PCM rendered in {elapsed:?}"
        );
    }

    #[test]
    #[ignore = "requires a Windows audio endpoint"]
    fn live_backend_switches_keep_the_sink_writable() {
        fn select(backend: WindowsAudioBackend) {
            assert!(unsafe { librespot_audio_set_backend(backend as u32, std::ptr::null()) });
        }

        fn silence() -> AudioPacket {
            AudioPacket::Samples(vec![0.0; SAMPLE_RATE as usize * CHANNELS as usize / 50])
        }

        select(WindowsAudioBackend::Wasapi);
        let mut sink = UwpSink::open(None, AudioFormat::F64);
        let mut converter = Converter::new(None);
        sink.start().unwrap();
        sink.write(silence(), &mut converter).unwrap();

        select(WindowsAudioBackend::XAudio2);
        sink.write(silence(), &mut converter).unwrap();

        select(WindowsAudioBackend::RingBuffer);
        sink.write(silence(), &mut converter).unwrap();

        select(WindowsAudioBackend::Wasapi);
        sink.write(silence(), &mut converter).unwrap();
        sink.stop().unwrap();
    }
}
