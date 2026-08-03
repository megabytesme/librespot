use crate::audio_backend::{SinkError, SinkResult};

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::time::Duration;

use super::windows_native::{
    ComPtr, Guid, HResult, IUnknownVTable, WaveFormatEx, succeeded, vtable,
};
use super::{EffectsConfig, EffectsPreset, effects_snapshot};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
// The inbox XAudio2 implementation used by UWP expects an XAUDIO2_PROCESSOR
// bitmask. XAUDIO2_DEFAULT_PROCESSOR is Processor1, not the zero-valued
// XAUDIO2_USE_DEFAULT_PROCESSOR accepted by the newer desktop redistributable.
const XAUDIO2_DEFAULT_PROCESSOR: u32 = 0x0000_0001;
const XAUDIO2_VOICE_NOPITCH: u32 = 0x0002;
const XAUDIO2_VOICE_NOSRC: u32 = 0x0004;
const EFFECT_OPERATION_SET: u32 = 1;
const PRE_ROLL_MILLISECONDS: u32 = 200;
const MAX_QUEUE_MILLISECONDS: u32 = 500;
const SUBMISSION_CHUNK_MILLISECONDS: u32 = 20;
const EFFECT_EQ: u32 = 0;
const EFFECT_ECHO: u32 = 1;
const EFFECT_LIMITER: u32 = 2;
const EFFECT_REVERB: u32 = 0;
const FX_EQ: Guid = Guid::from_u128(0xf5e01117_d6c4_485a_a3f5_695196f3dbfa);
const FX_ECHO: Guid = Guid::from_u128(0x5039d740_f736_449a_84d3_a56202557b87);
const FX_REVERB: Guid = Guid::from_u128(0x7d9aca56_cb68_4807_b632_b137352e8596);
const FX_MASTERING_LIMITER: Guid = Guid::from_u128(0xc4137916_2be1_46fd_8599_441536f49856);
const RPC_E_CHANGED_MODE: HResult = 0x8001_0106u32 as i32;

#[link(name = "runtimeobject")]
unsafe extern "system" {
    fn RoInitialize(init_type: u32) -> HResult;
    fn RoUninitialize();
}

// Bind XAudio2 2.8 explicitly. The generic Windows 10 SDK `xaudio2.lib`
// imports XAudio2_9.dll, whose explicit-endpoint mastering voices fail with
// E_NOINTERFACE on Windows 10 Mobile. XAudio2 2.8 is the inbox UWP/Phone
// implementation and supports device-id routing on every target architecture.
#[link(name = "xaudio2_8")]
unsafe extern "system" {
    fn XAudio2Create(engine: *mut *mut c_void, flags: u32, processor: u32) -> HResult;
}

#[link(name = "xaudio2_8")]
unsafe extern "C" {
    fn CreateFX(
        class_id: *const Guid,
        effect: *mut *mut c_void,
        init_data: *const c_void,
        init_data_size: u32,
    ) -> HResult;
}

#[repr(C)]
struct XAudio2VTable {
    base: IUnknownVTable,
    register_for_callbacks: usize,
    unregister_for_callbacks: usize,
    create_source_voice: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut c_void,
        *const WaveFormatEx,
        u32,
        f32,
        *mut c_void,
        *const c_void,
        *const XAudio2EffectChain,
    ) -> HResult,
    create_submix_voice: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut c_void,
        u32,
        u32,
        u32,
        u32,
        *const XAudio2VoiceSends,
        *const XAudio2EffectChain,
    ) -> HResult,
    create_mastering_voice: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut c_void,
        u32,
        u32,
        u32,
        *const u16,
        *const XAudio2EffectChain,
        i32,
    ) -> HResult,
    start_engine: unsafe extern "system" fn(*mut c_void) -> HResult,
    stop_engine: unsafe extern "system" fn(*mut c_void),
    commit_changes: unsafe extern "system" fn(*mut c_void, u32) -> HResult,
    get_performance_data: usize,
    set_debug_configuration: usize,
}

#[repr(C)]
struct XAudio2VoiceVTable {
    get_voice_details: usize,
    set_output_voices: usize,
    set_effect_chain: usize,
    enable_effect: unsafe extern "system" fn(*mut c_void, u32, u32) -> HResult,
    disable_effect: unsafe extern "system" fn(*mut c_void, u32, u32) -> HResult,
    get_effect_state: usize,
    set_effect_parameters:
        unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32, u32) -> HResult,
    get_effect_parameters: usize,
    set_filter_parameters: usize,
    get_filter_parameters: usize,
    set_output_filter_parameters: usize,
    get_output_filter_parameters: usize,
    set_volume: unsafe extern "system" fn(*mut c_void, f32, u32) -> HResult,
    get_volume: usize,
    set_channel_volumes: usize,
    get_channel_volumes: usize,
    set_output_matrix:
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, *const f32, u32) -> HResult,
    get_output_matrix: usize,
    destroy_voice: unsafe extern "system" fn(*mut c_void),
}

#[repr(C)]
struct XAudio2SourceVoiceVTable {
    base: XAudio2VoiceVTable,
    start: unsafe extern "system" fn(*mut c_void, u32, u32) -> HResult,
    stop: unsafe extern "system" fn(*mut c_void, u32, u32) -> HResult,
    submit_source_buffer:
        unsafe extern "system" fn(*mut c_void, *const XAudio2Buffer, *const c_void) -> HResult,
    flush_source_buffers: unsafe extern "system" fn(*mut c_void) -> HResult,
    discontinuity: usize,
    exit_loop: usize,
    get_state: unsafe extern "system" fn(*mut c_void, *mut XAudio2VoiceState, u32),
    set_frequency_ratio: usize,
    get_frequency_ratio: usize,
    set_source_sample_rate: usize,
}

#[repr(C, packed(1))]
struct XAudio2Buffer {
    flags: u32,
    audio_bytes: u32,
    audio_data: *const u8,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    context: *mut c_void,
}

#[repr(C, packed(1))]
struct XAudio2EffectChain {
    effect_count: u32,
    effect_descriptors: *mut XAudio2EffectDescriptor,
}

#[repr(C, packed(1))]
struct XAudio2EffectDescriptor {
    effect: *mut c_void,
    initial_state: i32,
    output_channels: u32,
}

#[repr(C, packed(1))]
struct XAudio2SendDescriptor {
    flags: u32,
    output_voice: *mut c_void,
}

#[repr(C, packed(1))]
struct XAudio2VoiceSends {
    send_count: u32,
    sends: *mut XAudio2SendDescriptor,
}

#[repr(C, packed(1))]
#[derive(Default)]
struct XAudio2VoiceState {
    current_buffer_context: *mut c_void,
    buffers_queued: u32,
    samples_played: u64,
}

#[repr(C, packed(1))]
struct FxEchoParameters {
    wet_dry_mix: f32,
    feedback: f32,
    delay: f32,
}

#[repr(C, packed(1))]
struct FxEqParameters {
    frequency_center0: f32,
    gain0: f32,
    bandwidth0: f32,
    frequency_center1: f32,
    gain1: f32,
    bandwidth1: f32,
    frequency_center2: f32,
    gain2: f32,
    bandwidth2: f32,
    frequency_center3: f32,
    gain3: f32,
    bandwidth3: f32,
}

#[repr(C, packed(1))]
struct FxMasteringLimiterParameters {
    release: u32,
    loudness: u32,
}

#[repr(C, packed(1))]
struct FxReverbParameters {
    diffusion: f32,
    room_size: f32,
}

struct RoInitialization {
    owned: bool,
}

impl RoInitialization {
    fn new() -> SinkResult<Self> {
        let result = unsafe { RoInitialize(1) }; // RO_INIT_MULTITHREADED
        if succeeded(result) {
            Ok(Self { owned: true })
        } else if result == RPC_E_CHANGED_MODE {
            Ok(Self { owned: false })
        } else {
            Err(SinkError::NotConnected(format!(
                "RoInitialize failed (0x{:08X})",
                result as u32
            )))
        }
    }
}

impl Drop for RoInitialization {
    fn drop(&mut self) {
        if self.owned {
            unsafe { RoUninitialize() };
        }
    }
}

pub(super) struct XAudio2Sink {
    engine: ComPtr,
    mastering_voice: *mut c_void,
    reverb_voice: *mut c_void,
    source_voice: *mut c_void,
    effects: Vec<ComPtr>,
    queued_buffers: VecDeque<Box<[f32]>>,
    pending_samples: Vec<f32>,
    queued_frames: usize,
    pre_roll_frames: usize,
    maximum_queue_frames: usize,
    submission_chunk_frames: usize,
    underruns: u64,
    underrun_reported: bool,
    applied_effects_version: u64,
    sample_rate: u32,
    channels: u16,
    running: bool,
    voice_started: bool,
    _ro_initialization: RoInitialization,
}

unsafe impl Send for XAudio2Sink {}

impl XAudio2Sink {
    pub fn new(sample_rate: u32, channels: u16, device_id: &str) -> SinkResult<Self> {
        let ro_initialization = RoInitialization::new()?;
        let mut engine_raw = ptr::null_mut();
        check_hr("XAudio2Create", unsafe {
            XAudio2Create(&mut engine_raw, 0, XAUDIO2_DEFAULT_PROCESSOR)
        })?;
        let engine = unsafe { ComPtr::from_raw(engine_raw) }
            .ok_or_else(|| SinkError::NotConnected("XAudio2 returned no engine".into()))?;
        let engine_vtable = unsafe { vtable::<XAudio2VTable>(engine.as_raw()) };

        let device_wide: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
        let device = if device_id.is_empty() {
            ptr::null()
        } else {
            device_wide.as_ptr()
        };
        let mut mastering_voice = ptr::null_mut();
        check_hr("IXAudio2::CreateMasteringVoice", unsafe {
            (engine_vtable.create_mastering_voice)(
                engine.as_raw(),
                &mut mastering_voice,
                channels as u32,
                sample_rate,
                0,
                device,
                ptr::null(),
                11, // AudioCategory_Media
            )
        })?;
        if mastering_voice.is_null() {
            return Err(SinkError::NotConnected(
                "XAudio2 returned no mastering voice".into(),
            ));
        }

        let effects = match create_effects() {
            Ok(effects) => effects,
            Err(error) => {
                unsafe { destroy_voice(mastering_voice) };
                return Err(error);
            }
        };

        // Reverb must be mixed in parallel. Putting FXReverb in the serial
        // source chain replaces the dry signal with a fully processed signal,
        // which sounds metallic and makes combinations of effects clip.
        let mut reverb_descriptor = XAudio2EffectDescriptor {
            effect: effects[3].as_raw(),
            initial_state: 0,
            output_channels: channels as u32,
        };
        let reverb_chain = XAudio2EffectChain {
            effect_count: 1,
            effect_descriptors: &mut reverb_descriptor,
        };
        let mut reverb_voice = ptr::null_mut();
        let create_reverb_result = unsafe {
            (engine_vtable.create_submix_voice)(
                engine.as_raw(),
                &mut reverb_voice,
                channels as u32,
                sample_rate,
                0,
                0,
                ptr::null(),
                &reverb_chain,
            )
        };
        if !succeeded(create_reverb_result) || reverb_voice.is_null() {
            unsafe { destroy_voice(mastering_voice) };
            check_hr("IXAudio2::CreateSubmixVoice(reverb)", create_reverb_result)?;
            return Err(SinkError::NotConnected(
                "XAudio2 returned no reverb submix voice".into(),
            ));
        }

        let mut descriptors: Vec<XAudio2EffectDescriptor> = effects[..3]
            .iter()
            .map(|effect| XAudio2EffectDescriptor {
                effect: effect.as_raw(),
                initial_state: 0,
                output_channels: channels as u32,
            })
            .collect();
        let effect_chain = XAudio2EffectChain {
            effect_count: descriptors.len() as u32,
            effect_descriptors: descriptors.as_mut_ptr(),
        };
        let block_align = channels * size_of::<f32>() as u16;
        let format = WaveFormatEx {
            format_tag: WAVE_FORMAT_IEEE_FLOAT,
            channels,
            samples_per_sec: sample_rate,
            average_bytes_per_sec: sample_rate * block_align as u32,
            block_align,
            bits_per_sample: 32,
            extra_size: 0,
        };
        let mut source_voice = ptr::null_mut();
        let mut sends = [
            XAudio2SendDescriptor {
                flags: 0,
                output_voice: mastering_voice,
            },
            XAudio2SendDescriptor {
                flags: 0,
                output_voice: reverb_voice,
            },
        ];
        let send_list = XAudio2VoiceSends {
            send_count: sends.len() as u32,
            sends: sends.as_mut_ptr(),
        };
        let create_source_result = unsafe {
            (engine_vtable.create_source_voice)(
                engine.as_raw(),
                &mut source_voice,
                &format,
                XAUDIO2_VOICE_NOPITCH | XAUDIO2_VOICE_NOSRC,
                1.0,
                ptr::null_mut(),
                (&send_list as *const XAudio2VoiceSends).cast(),
                &effect_chain,
            )
        };
        if !succeeded(create_source_result) || source_voice.is_null() {
            unsafe {
                destroy_voice(reverb_voice);
                destroy_voice(mastering_voice);
            }
            check_hr("IXAudio2::CreateSourceVoice", create_source_result)?;
            return Err(SinkError::NotConnected(
                "XAudio2 returned no source voice".into(),
            ));
        }

        let start_result = unsafe { (engine_vtable.start_engine)(engine.as_raw()) };
        if !succeeded(start_result) {
            unsafe {
                destroy_voice(source_voice);
                destroy_voice(reverb_voice);
                destroy_voice(mastering_voice);
            }
            check_hr("IXAudio2::StartEngine", start_result)?;
        }

        let mut sink = Self {
            engine,
            mastering_voice,
            reverb_voice,
            source_voice,
            effects,
            queued_buffers: VecDeque::new(),
            pending_samples: Vec::new(),
            queued_frames: 0,
            pre_roll_frames: sample_rate as usize * PRE_ROLL_MILLISECONDS as usize / 1_000,
            maximum_queue_frames: sample_rate as usize * MAX_QUEUE_MILLISECONDS as usize / 1_000,
            submission_chunk_frames: (sample_rate as usize
                * SUBMISSION_CHUNK_MILLISECONDS as usize
                / 1_000)
                .max(1),
            underruns: 0,
            underrun_reported: false,
            applied_effects_version: u64::MAX,
            sample_rate,
            channels,
            running: false,
            voice_started: false,
            _ro_initialization: ro_initialization,
        };
        sink.apply_effects_if_changed()?;
        Ok(sink)
    }

    fn source_vtable(&self) -> &XAudio2SourceVoiceVTable {
        unsafe { vtable::<XAudio2SourceVoiceVTable>(self.source_voice) }
    }

    pub fn start(&mut self) -> SinkResult<()> {
        if self.running {
            return Ok(());
        }
        self.running = true;
        self.start_voice_if_ready(false)
    }

    pub fn stop(&mut self) -> SinkResult<()> {
        if self.running {
            self.submit_pending_tail()?;
            self.start_voice_if_ready(true)?;
            if self.voice_started {
                self.wait_until_queued_at_most(0)?;
            }
            self.queued_buffers.clear();
            self.pending_samples.clear();
            self.queued_frames = 0;
            if self.voice_started {
                check_hr("IXAudio2SourceVoice::Stop", unsafe {
                    (self.source_vtable().stop)(self.source_voice, 0, 0)
                })?;
                self.voice_started = false;
            }
            self.running = false;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> SinkResult<()> {
        if self.voice_started {
            check_hr("IXAudio2SourceVoice::Stop", unsafe {
                (self.source_vtable().stop)(self.source_voice, 0, 0)
            })?;
            self.voice_started = false;
        }
        check_hr("IXAudio2SourceVoice::FlushSourceBuffers", unsafe {
            (self.source_vtable().flush_source_buffers)(self.source_voice)
        })?;
        self.queued_buffers.clear();
        self.pending_samples.clear();
        self.queued_frames = 0;
        self.underrun_reported = false;
        Ok(())
    }

    pub fn write_f32(&mut self, samples: &[f32]) -> SinkResult<()> {
        if samples.len() % self.channels as usize != 0 {
            return Err(SinkError::InvalidParams(
                "XAudio2 packet was not aligned to a complete audio frame".into(),
            ));
        }
        if !self.running {
            self.start()?;
        }
        self.apply_effects_if_changed()?;
        self.reap_completed_buffers();
        if self.voice_started && self.queued_buffers.is_empty() && !self.underrun_reported {
            self.underruns = self.underruns.wrapping_add(1);
            self.underrun_reported = true;
            if self.underruns == 1 || self.underruns % 25 == 0 {
                log::warn!(
                    "XAudio2 producer reached an empty queue (underruns={})",
                    self.underruns
                );
            }
        }

        self.pending_samples.extend_from_slice(samples);
        let chunk_samples = self.submission_chunk_frames * self.channels as usize;
        while self.pending_samples.len() >= chunk_samples {
            let remainder = self.pending_samples.split_off(chunk_samples);
            let chunk = std::mem::replace(&mut self.pending_samples, remainder);
            self.submit_samples(chunk)?;
        }
        Ok(())
    }

    fn submit_samples(&mut self, samples: Vec<f32>) -> SinkResult<()> {
        let additional_frames = samples.len() / self.channels as usize;
        self.wait_for_queue_capacity(additional_frames)?;
        let owned = samples.into_boxed_slice();
        let buffer = XAudio2Buffer {
            flags: 0,
            audio_bytes: (owned.len() * size_of::<f32>()) as u32,
            audio_data: owned.as_ptr().cast(),
            play_begin: 0,
            play_length: 0,
            loop_begin: 0,
            loop_length: 0,
            loop_count: 0,
            context: ptr::null_mut(),
        };
        check_hr("IXAudio2SourceVoice::SubmitSourceBuffer", unsafe {
            (self.source_vtable().submit_source_buffer)(self.source_voice, &buffer, ptr::null())
        })?;
        self.queued_buffers.push_back(owned);
        self.queued_frames = self.queued_frames.saturating_add(additional_frames);
        self.underrun_reported = false;
        self.start_voice_if_ready(false)
    }

    fn submit_pending_tail(&mut self) -> SinkResult<()> {
        if self.pending_samples.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_samples);
        self.submit_samples(pending)
    }

    fn start_voice_if_ready(&mut self, force: bool) -> SinkResult<()> {
        if self.voice_started
            || !self.running
            || self.queued_frames == 0
            || (!force && self.queued_frames < self.pre_roll_frames)
        {
            return Ok(());
        }

        check_hr("IXAudio2SourceVoice::Start", unsafe {
            (self.source_vtable().start)(self.source_voice, 0, 0)
        })?;
        self.voice_started = true;
        log::debug!(
            "XAudio2 source started with {:.1} ms queued",
            self.queued_frames as f64 * 1_000.0 / self.sample_rate as f64
        );
        Ok(())
    }

    fn queued_count(&self) -> u32 {
        let mut state = XAudio2VoiceState::default();
        unsafe { (self.source_vtable().get_state)(self.source_voice, &mut state, 0) };
        state.buffers_queued
    }

    fn reap_completed_buffers(&mut self) {
        let queued = self.queued_count() as usize;
        while self.queued_buffers.len() > queued {
            if let Some(buffer) = self.queued_buffers.pop_front() {
                self.queued_frames = self
                    .queued_frames
                    .saturating_sub(buffer.len() / self.channels as usize);
            }
        }
    }

    fn wait_for_queue_capacity(&mut self, additional_frames: usize) -> SinkResult<()> {
        let mut spins = 0u32;
        loop {
            self.reap_completed_buffers();
            if self.queued_buffers.is_empty()
                || self.queued_frames.saturating_add(additional_frames) <= self.maximum_queue_frames
            {
                return Ok(());
            }

            std::thread::sleep(Duration::from_millis(1));
            spins += 1;
            if spins >= 30_000 {
                return Err(SinkError::OnWrite(
                    "XAudio2 queue did not make room for PCM within 30 seconds".into(),
                ));
            }
        }
    }

    fn wait_until_queued_at_most(&mut self, maximum: u32) -> SinkResult<()> {
        let mut spins = 0u32;
        while self.queued_count() > maximum {
            std::thread::sleep(Duration::from_millis(1));
            spins += 1;
            if spins >= 30_000 {
                return Err(SinkError::OnWrite(
                    "XAudio2 did not release submitted PCM within 30 seconds".into(),
                ));
            }
        }
        Ok(())
    }

    fn apply_effects_if_changed(&mut self) -> SinkResult<()> {
        let config = effects_snapshot();
        if config.version == self.applied_effects_version {
            return Ok(());
        }
        self.configure_equalizer(&config)?;
        self.configure_echo(&config)?;
        self.configure_reverb(&config)?;
        self.configure_limiter(&config)?;
        self.configure_headroom(&config)?;
        let engine = unsafe { vtable::<XAudio2VTable>(self.engine.as_raw()) };
        check_hr("IXAudio2::CommitChanges(effects)", unsafe {
            (engine.commit_changes)(self.engine.as_raw(), EFFECT_OPERATION_SET)
        })?;
        self.applied_effects_version = config.version;
        Ok(())
    }

    fn configure_equalizer(&self, config: &EffectsConfig) -> SinkResult<()> {
        let enabled = config.preset != EffectsPreset::None;
        if !enabled {
            return self.set_effect_enabled(self.source_voice, EFFECT_EQ, false);
        }
        let centers = [100.0f32, 500.0, 2_000.0, 10_000.0];
        let gains = equalizer_gains(config, &centers);
        self.set_effect_parameters(
            self.source_voice,
            EFFECT_EQ,
            &FxEqParameters {
                frequency_center0: centers[0],
                gain0: db_to_gain(gains[0]),
                bandwidth0: 1.0,
                frequency_center1: centers[1],
                gain1: db_to_gain(gains[1]),
                bandwidth1: 1.0,
                frequency_center2: centers[2],
                gain2: db_to_gain(gains[2]),
                bandwidth2: 1.0,
                frequency_center3: centers[3],
                gain3: db_to_gain(gains[3]),
                bandwidth3: 1.0,
            },
        )?;
        self.set_effect_enabled(self.source_voice, EFFECT_EQ, true)
    }

    fn configure_echo(&self, config: &EffectsConfig) -> SinkResult<()> {
        if !config.echo {
            return self.set_effect_enabled(self.source_voice, EFFECT_ECHO, false);
        }
        let strength = config.strength;
        self.set_effect_parameters(
            self.source_voice,
            EFFECT_ECHO,
            &FxEchoParameters {
                wet_dry_mix: 0.04 + 0.20 * strength,
                feedback: 0.05 + 0.25 * strength,
                delay: 100.0 + 180.0 * strength,
            },
        )?;
        self.set_effect_enabled(self.source_voice, EFFECT_ECHO, true)
    }

    fn configure_reverb(&self, config: &EffectsConfig) -> SinkResult<()> {
        if config.reverb {
            self.set_effect_parameters(
                self.reverb_voice,
                EFFECT_REVERB,
                &FxReverbParameters {
                    diffusion: 0.55 + 0.30 * config.strength,
                    room_size: 0.30 + 0.35 * config.strength,
                },
            )?;
            self.set_effect_enabled(self.reverb_voice, EFFECT_REVERB, true)?;
        } else {
            self.set_effect_enabled(self.reverb_voice, EFFECT_REVERB, false)?;
        }

        self.set_send_gain(self.mastering_voice, 1.0)?;
        self.set_send_gain(self.reverb_voice, reverb_wet_gain(config))
    }

    fn configure_limiter(&self, config: &EffectsConfig) -> SinkResult<()> {
        if !config.limiter {
            return self.set_effect_enabled(self.source_voice, EFFECT_LIMITER, false);
        }
        self.set_effect_parameters(
            self.source_voice,
            EFFECT_LIMITER,
            &FxMasteringLimiterParameters {
                release: 6,
                loudness: 1_000,
            },
        )?;
        self.set_effect_enabled(self.source_voice, EFFECT_LIMITER, true)
    }

    fn configure_headroom(&self, config: &EffectsConfig) -> SinkResult<()> {
        let voice = unsafe { vtable::<XAudio2VoiceVTable>(self.source_voice) };
        check_hr("IXAudio2Voice::SetVolume(effect headroom)", unsafe {
            (voice.set_volume)(
                self.source_voice,
                effect_headroom_gain(config),
                EFFECT_OPERATION_SET,
            )
        })
    }

    fn set_send_gain(&self, destination: *mut c_void, gain: f32) -> SinkResult<()> {
        let channels = self.channels as usize;
        let mut matrix = vec![0.0f32; channels * channels];
        for channel in 0..channels {
            matrix[channel + channels * channel] = gain;
        }
        let voice = unsafe { vtable::<XAudio2VoiceVTable>(self.source_voice) };
        check_hr("IXAudio2Voice::SetOutputMatrix", unsafe {
            (voice.set_output_matrix)(
                self.source_voice,
                destination,
                self.channels as u32,
                self.channels as u32,
                matrix.as_ptr(),
                EFFECT_OPERATION_SET,
            )
        })
    }

    fn set_effect_enabled(
        &self,
        voice_pointer: *mut c_void,
        index: u32,
        enabled: bool,
    ) -> SinkResult<()> {
        let base = unsafe { vtable::<XAudio2VoiceVTable>(voice_pointer) };
        let result = if enabled {
            unsafe { (base.enable_effect)(voice_pointer, index, EFFECT_OPERATION_SET) }
        } else {
            unsafe { (base.disable_effect)(voice_pointer, index, EFFECT_OPERATION_SET) }
        };
        check_hr("IXAudio2Voice effect state", result)
    }

    fn set_effect_parameters<T>(
        &self,
        voice_pointer: *mut c_void,
        index: u32,
        parameters: &T,
    ) -> SinkResult<()> {
        let voice = unsafe { vtable::<XAudio2VoiceVTable>(voice_pointer) };
        check_hr("IXAudio2Voice::SetEffectParameters", unsafe {
            (voice.set_effect_parameters)(
                voice_pointer,
                index,
                (parameters as *const T).cast(),
                size_of::<T>() as u32,
                EFFECT_OPERATION_SET,
            )
        })
    }
}

impl Drop for XAudio2Sink {
    fn drop(&mut self) {
        let _ = self.stop();
        unsafe {
            destroy_voice(self.source_voice);
            destroy_voice(self.reverb_voice);
            destroy_voice(self.mastering_voice);
            let engine = vtable::<XAudio2VTable>(self.engine.as_raw());
            (engine.stop_engine)(self.engine.as_raw());
        }
        self.effects.clear();
    }
}

fn create_effects() -> SinkResult<Vec<ComPtr>> {
    [FX_EQ, FX_ECHO, FX_MASTERING_LIMITER, FX_REVERB]
        .iter()
        .map(create_effect)
        .collect()
}

unsafe fn destroy_voice(voice: *mut c_void) {
    if !voice.is_null() {
        let vtable = unsafe { vtable::<XAudio2VoiceVTable>(voice) };
        unsafe { (vtable.destroy_voice)(voice) };
    }
}

fn create_effect(class_id: &Guid) -> SinkResult<ComPtr> {
    let mut raw = ptr::null_mut();
    check_hr("CreateFX", unsafe {
        CreateFX(class_id, &mut raw, ptr::null(), 0)
    })?;
    unsafe { ComPtr::from_raw(raw) }
        .ok_or_else(|| SinkError::NotConnected("CreateFX returned no XAPO".into()))
}

fn equalizer_gains(config: &EffectsConfig, centers: &[f32; 4]) -> [f32; 4] {
    if config.preset == EffectsPreset::Equalizer {
        return [
            config.equalizer_gains_db[0],
            config.equalizer_gains_db[1],
            (config.equalizer_gains_db[2] + config.equalizer_gains_db[3]) * 0.5,
            config.equalizer_gains_db[4],
        ];
    }
    centers.map(|frequency| preset_gain(config.preset, frequency, config.strength))
}

fn reverb_wet_gain(config: &EffectsConfig) -> f32 {
    if config.reverb {
        0.05 + 0.18 * config.strength.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn effect_headroom_gain(config: &EffectsConfig) -> f32 {
    let centers = [100.0f32, 500.0, 2_000.0, 10_000.0];
    let maximum_boost_db = if config.preset == EffectsPreset::None {
        0.0
    } else {
        equalizer_gains(config, &centers)
            .into_iter()
            .fold(0.0f32, f32::max)
    };
    let equalizer_headroom = db_to_gain(-maximum_boost_db);
    let parallel_mix_headroom = 1.0 / (1.0 + reverb_wet_gain(config));
    let echo_headroom = if config.echo { 0.95 } else { 1.0 };
    (equalizer_headroom * parallel_mix_headroom * echo_headroom).clamp(0.1, 1.0)
}

fn preset_gain(preset: EffectsPreset, frequency: f32, strength: f32) -> f32 {
    let gain = match preset {
        EffectsPreset::BassBoost if frequency <= 125.0 => 8.0,
        EffectsPreset::BassBoost if frequency <= 500.0 => 5.0,
        EffectsPreset::BassBoost if frequency <= 2_000.0 => 2.0,
        EffectsPreset::BassBoost if frequency <= 6_000.0 => -1.0,
        EffectsPreset::BassBoost => -2.0,
        EffectsPreset::VocalBoost if frequency <= 125.0 => -3.0,
        EffectsPreset::VocalBoost if frequency <= 500.0 => -1.0,
        EffectsPreset::VocalBoost if frequency <= 4_000.0 => 5.0,
        EffectsPreset::VocalBoost if frequency <= 8_000.0 => 4.0,
        EffectsPreset::VocalBoost => 2.0,
        EffectsPreset::Warm if frequency <= 125.0 => 4.0,
        EffectsPreset::Warm if frequency <= 500.0 => 3.0,
        EffectsPreset::Warm if frequency <= 2_000.0 => 1.0,
        EffectsPreset::Warm if frequency <= 6_000.0 => -1.0,
        EffectsPreset::Warm => -2.0,
        _ => 0.0,
    };
    gain * strength.clamp(0.0, 1.0)
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32
        .powf(db.clamp(-18.0, 18.0) / 20.0)
        .clamp(0.126, 7.94)
}

fn check_hr(operation: &str, result: HResult) -> SinkResult<()> {
    if succeeded(result) {
        Ok(())
    } else {
        Err(SinkError::NotConnected(format!(
            "{operation} failed (0x{:08X})",
            result as u32
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_curves_match_the_managed_backend() {
        assert_eq!(preset_gain(EffectsPreset::BassBoost, 100.0, 0.5), 4.0);
        assert_eq!(preset_gain(EffectsPreset::VocalBoost, 2_000.0, 1.0), 5.0);
        assert_eq!(preset_gain(EffectsPreset::Warm, 10_000.0, 1.0), -2.0);
    }

    #[test]
    fn custom_five_bands_map_deterministically_to_xapo_four_bands() {
        let config = EffectsConfig {
            preset: EffectsPreset::Equalizer,
            equalizer_gains_db: [1.0, 2.0, 3.0, 5.0, 7.0],
            ..EffectsConfig::default()
        };
        assert_eq!(
            equalizer_gains(&config, &[100.0, 500.0, 2_000.0, 10_000.0]),
            [1.0, 2.0, 4.0, 7.0]
        );
    }

    #[test]
    fn native_effects_reserve_headroom_and_keep_reverb_subtle() {
        let disabled = EffectsConfig::default();
        assert_eq!(effect_headroom_gain(&disabled), 1.0);
        assert_eq!(reverb_wet_gain(&disabled), 0.0);

        let bass_and_reverb = EffectsConfig {
            preset: EffectsPreset::BassBoost,
            strength: 1.0,
            reverb: true,
            ..EffectsConfig::default()
        };
        assert!(reverb_wet_gain(&bass_and_reverb) <= 0.23);
        assert!(effect_headroom_gain(&bass_and_reverb) < db_to_gain(-7.9));
    }

    #[test]
    fn source_voice_uses_an_exact_unpitched_clock() {
        assert_eq!(XAUDIO2_DEFAULT_PROCESSOR, 0x0000_0001);
        assert_eq!(XAUDIO2_VOICE_NOPITCH | XAUDIO2_VOICE_NOSRC, 0x0006);
    }

    #[test]
    fn queue_depth_matches_the_managed_backends_resilience() {
        let pre_roll_frames = 44_100 * PRE_ROLL_MILLISECONDS / 1_000;
        let maximum_frames = 44_100 * MAX_QUEUE_MILLISECONDS / 1_000;
        let chunk_frames = 44_100 * SUBMISSION_CHUNK_MILLISECONDS / 1_000;
        assert_eq!(pre_roll_frames, 8_820);
        assert_eq!(maximum_frames, 22_050);
        assert_eq!(maximum_frames / chunk_frames, 25);
        assert!(maximum_frames / chunk_frames < 64);
    }
}
