use crate::audio_backend::{Open, Sink, SinkAsBytes, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;

use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const BUFFER_SIZE: usize = 128 * 1024;

static mut AUDIO_BUFFER: *mut u8 = ptr::null_mut();
// Monotonic sequences make full and empty unambiguous and let the managed
// consumer retain a precise boundary between adjacent decoded tracks.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static READ_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static GENERATION_START_SEQUENCE: AtomicU64 = AtomicU64::new(0);
// Even values are stable. Odd values mean a generation boundary is being
// published. This provides one coherent FFI snapshot without a mutex.
static STATE_VERSION: AtomicU32 = AtomicU32::new(0);

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
    fn begin_generation(&mut self) -> u64 {
        STATE_VERSION.fetch_add(1, Ordering::AcqRel);
        let start_sequence = WRITE_SEQUENCE.load(Ordering::Acquire);
        GENERATION_START_SEQUENCE.store(start_sequence, Ordering::Release);
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        STATE_VERSION.fetch_add(1, Ordering::Release);
        generation
    }

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
                let wp = WRITE_SEQUENCE.load(Ordering::Acquire);
                let rp = READ_SEQUENCE.load(Ordering::Acquire);
                let used = wp.saturating_sub(rp).min(cap as u64) as usize;
                let free = cap - used;

                if free >= len {
                    break;
                } else {
                    thread::yield_now();
                    thread::sleep(Duration::from_micros(500));
                }
            }

            let write_sequence = WRITE_SEQUENCE.load(Ordering::Acquire);
            let wp = write_sequence as usize % cap;
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

            WRITE_SEQUENCE.store(write_sequence + len as u64, Ordering::Release);
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
    WRITE_SEQUENCE.load(Ordering::Acquire) as usize % BUFFER_SIZE
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_set_read_cursor(pos: usize) {
    let write_sequence = WRITE_SEQUENCE.load(Ordering::Acquire);
    let write_cursor = write_sequence as usize % BUFFER_SIZE;
    let distance = (BUFFER_SIZE + write_cursor - (pos % BUFFER_SIZE)) % BUFFER_SIZE;
    READ_SEQUENCE.store(
        write_sequence.saturating_sub(distance as u64),
        Ordering::Release,
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn librespot_audio_set_read_sequence(sequence: u64) {
    let write_sequence = WRITE_SEQUENCE.load(Ordering::Acquire);
    READ_SEQUENCE.store(sequence.min(write_sequence), Ordering::Release);
}

/// Returns a coherent, allocation-free snapshot for the AudioGraph quantum
/// callback. The callback uses the generation start as a hard track boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_audio_get_state(
    generation: *mut u64,
    generation_start_sequence: *mut u64,
    write_sequence: *mut u64,
) {
    // Keep this bounded for the real-time AudioGraph callback. Publication is
    // only three atomic operations; if it overlaps all attempts, the last
    // snapshot is still safe because the start is stored before the generation
    // is advanced and no PCM for that generation has been written yet.
    for _ in 0..3 {
        let before = STATE_VERSION.load(Ordering::Acquire);
        let current_generation = GENERATION.load(Ordering::Acquire);
        let current_start = GENERATION_START_SEQUENCE.load(Ordering::Acquire);
        let current_write = WRITE_SEQUENCE.load(Ordering::Acquire);
        let after = STATE_VERSION.load(Ordering::Acquire);

        if before == after && before & 1 == 0 {
            unsafe {
                if !generation.is_null() {
                    *generation = current_generation;
                }
                if !generation_start_sequence.is_null() {
                    *generation_start_sequence = current_start;
                }
                if !write_sequence.is_null() {
                    *write_sequence = current_write;
                }
            }
            return;
        }

        std::hint::spin_loop();
    }

    let current_generation = GENERATION.load(Ordering::Acquire);
    let current_start = GENERATION_START_SEQUENCE.load(Ordering::Acquire);
    let current_write = WRITE_SEQUENCE.load(Ordering::Acquire);
    unsafe {
        if !generation.is_null() {
            *generation = current_generation;
        }
        if !generation_start_sequence.is_null() {
            *generation_start_sequence = current_start;
        }
        if !write_sequence.is_null() {
            *write_sequence = current_write;
        }
    }
}
