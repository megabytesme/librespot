use crate::audio_backend::{Open, Sink, SinkAsBytes, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

const BUFFER_SIZE: usize = 1024 * 1024;

static mut AUDIO_BUFFER: *mut u8 = ptr::null_mut();
static WRITE_POS: AtomicUsize = AtomicUsize::new(0);
static READ_POS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" {
    fn VirtualAlloc(
        lpAddress: *const std::ffi::c_void,
        dwSize: usize,
        flAllocationType: u32,
        flProtect: u32,
    ) -> *mut std::ffi::c_void;
}

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_READWRITE: u32 = 0x04;

pub struct UwpSink {
    format: AudioFormat,
}

impl UwpSink {
    pub const NAME: &'static str = "uwp";

    pub fn open(_device: Option<String>, format: AudioFormat) -> Self {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| unsafe {
            let ptr = VirtualAlloc(
                ptr::null(),
                BUFFER_SIZE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if !ptr.is_null() {
                AUDIO_BUFFER = ptr as *mut u8;
            } else {
                eprintln!("UWP SINK FATAL: Failed to allocate ring buffer");
            }
        });

        Self { format }
    }
}

impl Open for UwpSink {
    fn open(device: Option<String>, format: AudioFormat) -> Self {
        UwpSink::open(device, format)
    }
}

impl Sink for UwpSink {
    sink_as_bytes!();
}

impl SinkAsBytes for UwpSink {
    fn write_bytes(&mut self, data: &[u8]) -> SinkResult<()> {
        unsafe {
            if AUDIO_BUFFER.is_null() {
                return Err(SinkError::NotConnected("Buffer not allocated".into()));
            }

            let len = data.len();
            let cap = BUFFER_SIZE;
            let wp = WRITE_POS.load(Ordering::Acquire);

            let first = cap - wp;
            let first_copy = first.min(len);

            ptr::copy_nonoverlapping(data.as_ptr(), AUDIO_BUFFER.add(wp), first_copy);

            if len > first_copy {
                ptr::copy_nonoverlapping(
                    data.as_ptr().add(first_copy),
                    AUDIO_BUFFER,
                    len - first_copy,
                );
            }

            let new_wp = (wp + len) % cap;
            WRITE_POS.store(new_wp, Ordering::Release);
        }

        Ok(())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_audio_get_buffer() -> *mut u8 {
    unsafe { AUDIO_BUFFER }
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_get_capacity() -> usize {
    1024 * 1024
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_get_write_cursor() -> usize {
    WRITE_POS.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_set_read_cursor(pos: usize) {
    READ_POS.store(pos % BUFFER_SIZE, Ordering::Release);

    if pos == 0xDEADBEEF {
        eprintln!("UWP: Resetting cursor");
    }
}
