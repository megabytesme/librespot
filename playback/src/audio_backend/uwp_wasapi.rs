use crate::audio_backend::{SinkError, SinkResult};

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::windows_native::{
    ComPtr, Guid, HResult, IID_IUNKNOWN, IUnknownVTable, WaveFormatEx, succeeded, vtable,
};

const DEFAULT_RENDER_DEVICE: &str = "{E6327CAD-DCEC-4949-AE8A-991E976A79D2}";
const IID_AUDIO_CLIENT: Guid = Guid::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
const IID_AUDIO_CLIENT2: Guid = Guid::from_u128(0x726778cd_f60a_4eda_82de_e47610cd78aa);
const IID_AUDIO_RENDER_CLIENT: Guid = Guid::from_u128(0xf294acfc_3146_4483_a7bf_addca7c260e2);
const IID_ACTIVATION_HANDLER: Guid = Guid::from_u128(0x41d949ab_9862_444a_80f6_c261334da5eb);
const IID_IAGILE_OBJECT: Guid = Guid::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);
const RPC_E_CHANGED_MODE: HResult = 0x8001_0106u32 as i32;
const E_NOINTERFACE: HResult = 0x8000_4002u32 as i32;
const E_POINTER: HResult = 0x8000_4003u32 as i32;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const BUFFER_DURATION_100NS: i64 = 1_000_000; // 100 ms
const STREAM_FLAGS: u32 = 0x8000_0000 | 0x0800_0000; // AUTOCONVERTPCM | SRC_DEFAULT_QUALITY

#[link(name = "runtimeobject")]
unsafe extern "system" {
    fn RoInitialize(init_type: u32) -> HResult;
    fn RoUninitialize();
}

#[link(name = "mmdevapi")]
unsafe extern "system" {
    fn ActivateAudioInterfaceAsync(
        device_interface_path: *const u16,
        iid: *const Guid,
        activation_params: *const c_void,
        completion_handler: *mut c_void,
        activation_operation: *mut *mut c_void,
    ) -> HResult;
}

#[repr(C)]
struct ActivationOperationVTable {
    base: IUnknownVTable,
    get_activate_result:
        unsafe extern "system" fn(*mut c_void, *mut HResult, *mut *mut c_void) -> HResult,
}

#[repr(C)]
struct AudioClientVTable {
    base: IUnknownVTable,
    initialize: unsafe extern "system" fn(
        *mut c_void,
        i32,
        u32,
        i64,
        i64,
        *const WaveFormatEx,
        *const Guid,
    ) -> HResult,
    get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> HResult,
    get_stream_latency: usize,
    get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> HResult,
    is_format_supported: usize,
    get_mix_format: usize,
    get_device_period: usize,
    start: unsafe extern "system" fn(*mut c_void) -> HResult,
    stop: unsafe extern "system" fn(*mut c_void) -> HResult,
    reset: unsafe extern "system" fn(*mut c_void) -> HResult,
    set_event_handle: usize,
    get_service: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult,
}

#[repr(C)]
struct AudioClient2VTable {
    base: AudioClientVTable,
    is_offload_capable: usize,
    set_client_properties:
        unsafe extern "system" fn(*mut c_void, *const AudioClientProperties) -> HResult,
    get_buffer_size_limits: usize,
}

#[repr(C)]
struct AudioRenderClientVTable {
    base: IUnknownVTable,
    get_buffer: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HResult,
    release_buffer: unsafe extern "system" fn(*mut c_void, u32, u32) -> HResult,
}

#[repr(C)]
struct AudioClientProperties {
    size: u32,
    is_offload: i32,
    category: i32,
    options: i32,
}

type ActivationResult = Arc<(Mutex<Option<(HResult, usize)>>, Condvar)>;

#[repr(C)]
struct ActivationHandlerVTable {
    base: IUnknownVTable,
    activate_completed: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HResult,
}

#[repr(C)]
struct ActivationHandler {
    vtable: *const ActivationHandlerVTable,
    references: AtomicU32,
    result: ActivationResult,
}

unsafe extern "system" fn handler_query_interface(
    this: *mut c_void,
    iid: *const Guid,
    object: *mut *mut c_void,
) -> HResult {
    if iid.is_null() || object.is_null() {
        return E_POINTER;
    }
    unsafe { *object = ptr::null_mut() };
    if unsafe { *iid } != IID_IUNKNOWN
        && unsafe { *iid } != IID_ACTIVATION_HANDLER
        && unsafe { *iid } != IID_IAGILE_OBJECT
    {
        return E_NOINTERFACE;
    }
    unsafe { *object = this };
    unsafe { handler_add_ref(this) };
    0
}

unsafe extern "system" fn handler_add_ref(this: *mut c_void) -> u32 {
    let handler = unsafe { &*this.cast::<ActivationHandler>() };
    handler.references.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn handler_release(this: *mut c_void) -> u32 {
    let handler = unsafe { &*this.cast::<ActivationHandler>() };
    let remaining = handler.references.fetch_sub(1, Ordering::Release) - 1;
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(unsafe { Box::from_raw(this.cast::<ActivationHandler>()) });
    }
    remaining
}

unsafe extern "system" fn handler_activate_completed(
    this: *mut c_void,
    operation: *mut c_void,
) -> HResult {
    let mut activation_result = E_POINTER;
    let mut activated = ptr::null_mut();
    let call_result = if operation.is_null() {
        E_POINTER
    } else {
        let operation_vtable = unsafe { vtable::<ActivationOperationVTable>(operation) };
        unsafe {
            (operation_vtable.get_activate_result)(
                operation,
                &mut activation_result,
                &mut activated,
            )
        }
    };
    if !succeeded(call_result) {
        activation_result = call_result;
    }

    let handler = unsafe { &*this.cast::<ActivationHandler>() };
    let (lock, ready) = &*handler.result;
    if let Ok(mut result) = lock.lock() {
        *result = Some((activation_result, activated as usize));
        ready.notify_all();
    }
    0
}

static ACTIVATION_HANDLER_VTABLE: ActivationHandlerVTable = ActivationHandlerVTable {
    base: IUnknownVTable {
        query_interface: handler_query_interface,
        add_ref: handler_add_ref,
        release: handler_release,
    },
    activate_completed: handler_activate_completed,
};

struct HandlerReference(*mut c_void);

impl Drop for HandlerReference {
    fn drop(&mut self) {
        unsafe { handler_release(self.0) };
    }
}

pub(super) struct WasapiSink {
    audio_client: ComPtr,
    render_client: ComPtr,
    buffer_frames: u32,
    channels: u16,
    running: bool,
    ro_initialized: bool,
}

impl WasapiSink {
    pub fn new(sample_rate: u32, channels: u16, device_id: &str) -> SinkResult<Self> {
        let initialize_result = unsafe { RoInitialize(1) }; // RO_INIT_MULTITHREADED
        let ro_initialized = if succeeded(initialize_result) {
            true
        } else if initialize_result == RPC_E_CHANGED_MODE {
            false
        } else {
            return Err(hresult_error("RoInitialize", initialize_result));
        };

        let result = Self::activate(sample_rate, channels, device_id);
        if result.is_err() && ro_initialized {
            unsafe { RoUninitialize() };
        }
        result.map(|mut sink| {
            sink.ro_initialized = ro_initialized;
            sink
        })
    }

    fn activate(sample_rate: u32, channels: u16, device_id: &str) -> SinkResult<Self> {
        let activation: ActivationResult = Arc::new((Mutex::new(None), Condvar::new()));
        let handler = Box::new(ActivationHandler {
            vtable: &ACTIVATION_HANDLER_VTABLE,
            references: AtomicU32::new(1),
            result: activation.clone(),
        });
        let handler_raw = Box::into_raw(handler).cast::<c_void>();
        let _handler_reference = HandlerReference(handler_raw);

        let endpoint = if device_id.is_empty() {
            DEFAULT_RENDER_DEVICE
        } else {
            device_id
        };
        let endpoint_wide: Vec<u16> = endpoint.encode_utf16().chain(Some(0)).collect();
        let mut operation_raw = ptr::null_mut();
        let result = unsafe {
            ActivateAudioInterfaceAsync(
                endpoint_wide.as_ptr(),
                &IID_AUDIO_CLIENT,
                ptr::null(),
                handler_raw,
                &mut operation_raw,
            )
        };
        check_hr("ActivateAudioInterfaceAsync", result)?;
        let _operation = unsafe { ComPtr::from_raw(operation_raw) }.ok_or_else(|| {
            SinkError::NotConnected("WASAPI returned no activation operation".into())
        })?;

        let (lock, ready) = &*activation;
        let guard = lock
            .lock()
            .map_err(|_| SinkError::NotConnected("WASAPI activation lock was poisoned".into()))?;
        let (mut guard, timeout) = ready
            .wait_timeout_while(guard, Duration::from_secs(10), |value| value.is_none())
            .map_err(|_| SinkError::NotConnected("WASAPI activation wait failed".into()))?;
        if timeout.timed_out() && guard.is_none() {
            return Err(SinkError::NotConnected(
                "WASAPI activation timed out".into(),
            ));
        }
        let (activation_result, audio_client_raw) = guard.take().ok_or_else(|| {
            SinkError::NotConnected("WASAPI returned no activation result".into())
        })?;
        check_hr("WASAPI activation", activation_result)?;
        let audio_client = unsafe { ComPtr::from_raw(audio_client_raw as *mut c_void) }
            .ok_or_else(|| SinkError::NotConnected("WASAPI returned no IAudioClient".into()))?;

        if let Ok(client2) = unsafe { audio_client.query(&IID_AUDIO_CLIENT2) } {
            let properties = AudioClientProperties {
                size: size_of::<AudioClientProperties>() as u32,
                is_offload: 0,
                category: 11, // AudioCategory_Media
                options: 0,
            };
            let client2_vtable = unsafe { vtable::<AudioClient2VTable>(client2.as_raw()) };
            check_hr("IAudioClient2::SetClientProperties", unsafe {
                (client2_vtable.set_client_properties)(client2.as_raw(), &properties)
            })?;
        }

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
        let client_vtable = unsafe { vtable::<AudioClientVTable>(audio_client.as_raw()) };
        check_hr("IAudioClient::Initialize", unsafe {
            (client_vtable.initialize)(
                audio_client.as_raw(),
                0, // AUDCLNT_SHAREMODE_SHARED
                STREAM_FLAGS,
                BUFFER_DURATION_100NS,
                0,
                &format,
                ptr::null(),
            )
        })?;

        let mut buffer_frames = 0;
        check_hr("IAudioClient::GetBufferSize", unsafe {
            (client_vtable.get_buffer_size)(audio_client.as_raw(), &mut buffer_frames)
        })?;
        let mut render_client_raw = ptr::null_mut();
        check_hr("IAudioClient::GetService", unsafe {
            (client_vtable.get_service)(
                audio_client.as_raw(),
                &IID_AUDIO_RENDER_CLIENT,
                &mut render_client_raw,
            )
        })?;
        let render_client = unsafe { ComPtr::from_raw(render_client_raw) }.ok_or_else(|| {
            SinkError::NotConnected("WASAPI returned no IAudioRenderClient".into())
        })?;

        Ok(Self {
            audio_client,
            render_client,
            buffer_frames,
            channels,
            running: false,
            ro_initialized: false,
        })
    }

    pub fn start(&mut self) -> SinkResult<()> {
        if self.running {
            return Ok(());
        }
        let client = unsafe { vtable::<AudioClientVTable>(self.audio_client.as_raw()) };
        check_hr("IAudioClient::Start", unsafe {
            (client.start)(self.audio_client.as_raw())
        })?;
        self.running = true;
        Ok(())
    }

    pub fn stop(&mut self) -> SinkResult<()> {
        if !self.running {
            return Ok(());
        }
        let client = unsafe { vtable::<AudioClientVTable>(self.audio_client.as_raw()) };
        check_hr("IAudioClient::Stop", unsafe {
            (client.stop)(self.audio_client.as_raw())
        })?;
        self.running = false;
        self.flush()
    }

    pub fn flush(&mut self) -> SinkResult<()> {
        let restart = self.running;
        let client = unsafe { vtable::<AudioClientVTable>(self.audio_client.as_raw()) };
        if restart {
            check_hr("IAudioClient::Stop", unsafe {
                (client.stop)(self.audio_client.as_raw())
            })?;
            self.running = false;
        }
        check_hr("IAudioClient::Reset", unsafe {
            (client.reset)(self.audio_client.as_raw())
        })?;
        if restart {
            self.start()?;
        }
        Ok(())
    }

    pub fn write_f32(&mut self, samples: &[f32]) -> SinkResult<()> {
        if samples.len() % self.channels as usize != 0 {
            return Err(SinkError::InvalidParams(
                "WASAPI packet was not aligned to a complete audio frame".into(),
            ));
        }
        if !self.running {
            self.start()?;
        }

        let client = unsafe { vtable::<AudioClientVTable>(self.audio_client.as_raw()) };
        let renderer = unsafe { vtable::<AudioRenderClientVTable>(self.render_client.as_raw()) };
        let mut frame_offset = 0usize;
        let total_frames = samples.len() / self.channels as usize;
        while frame_offset < total_frames {
            let mut padding = 0;
            check_hr("IAudioClient::GetCurrentPadding", unsafe {
                (client.get_current_padding)(self.audio_client.as_raw(), &mut padding)
            })?;
            let available = self.buffer_frames.saturating_sub(padding) as usize;
            if available == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            let frames = available.min(total_frames - frame_offset);
            let mut destination = ptr::null_mut();
            check_hr("IAudioRenderClient::GetBuffer", unsafe {
                (renderer.get_buffer)(self.render_client.as_raw(), frames as u32, &mut destination)
            })?;
            let sample_offset = frame_offset * self.channels as usize;
            let sample_count = frames * self.channels as usize;
            unsafe {
                ptr::copy_nonoverlapping(
                    samples.as_ptr().add(sample_offset).cast::<u8>(),
                    destination,
                    sample_count * size_of::<f32>(),
                );
            }
            check_hr("IAudioRenderClient::ReleaseBuffer", unsafe {
                (renderer.release_buffer)(self.render_client.as_raw(), frames as u32, 0)
            })?;
            frame_offset += frames;
        }
        Ok(())
    }
}

impl Drop for WasapiSink {
    fn drop(&mut self) {
        let _ = self.stop();
        if self.ro_initialized {
            unsafe { RoUninitialize() };
        }
    }
}

fn check_hr(operation: &str, result: HResult) -> SinkResult<()> {
    if succeeded(result) {
        Ok(())
    } else {
        Err(hresult_error(operation, result))
    }
}

fn hresult_error(operation: &str, result: HResult) -> SinkError {
    SinkError::NotConnected(format!("{operation} failed (0x{:08X})", result as u32))
}
