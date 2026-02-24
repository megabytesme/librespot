use crate::audio_backend::{Open, Sink, SinkAsBytes, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;

use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const BUFFER_SIZE: usize = 128 * 1024;

static mut AUDIO_BUFFER: *mut u8 = ptr::null_mut();
static WRITE_POS: AtomicUsize = AtomicUsize::new(0);
static READ_POS: AtomicUsize = AtomicUsize::new(0);

static AUDIO_FORMAT: AtomicU32 = AtomicU32::new(0);
static SAMPLE_RATE: AtomicU32 = AtomicU32::new(44100);
static CHANNELS: AtomicU32 = AtomicU32::new(2);

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
        AUDIO_FORMAT.store(format as u32, Ordering::Release);
        SAMPLE_RATE.store(44100, Ordering::Release);
        CHANNELS.store(2, Ordering::Release);

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
        let len = data.len();
        unsafe {
            if AUDIO_BUFFER.is_null() {
                return Err(SinkError::NotConnected("Buffer not allocated".into()));
            }

            let cap = BUFFER_SIZE;

            loop {
                let wp = WRITE_POS.load(Ordering::Acquire);
                let rp = READ_POS.load(Ordering::Acquire);
                let used = (cap + wp - rp) % cap;
                let free = cap - used - 1;

                if free >= len {
                    break;
                } else {
                    thread::yield_now();
                    thread::sleep(Duration::from_micros(500));
                }
            }

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

            WRITE_POS.store((wp + len) % cap, Ordering::Release);
        }
        super::TOTAL_WRITTEN.fetch_add(len, Ordering::SeqCst);
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_audio_get_buffer() -> *mut u8 {
    unsafe { AUDIO_BUFFER }
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_get_capacity() -> usize {
    BUFFER_SIZE
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_get_write_cursor() -> usize {
    WRITE_POS.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_set_read_cursor(pos: usize) {
    READ_POS.store(pos % BUFFER_SIZE, Ordering::Release);
}
