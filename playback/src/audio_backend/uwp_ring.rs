use crate::audio_backend::{Sink, SinkAsBytes, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;

use std::ptr;
use std::sync::Once;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const BUFFER_SIZE: usize = 128 * 1024;

static mut AUDIO_BUFFER: *mut u8 = ptr::null_mut();
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static READ_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static GENERATION_START_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static STATE_VERSION: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" {
    fn VirtualAlloc(
        lp_address: *const std::ffi::c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut std::ffi::c_void;
}

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_READWRITE: u32 = 0x04;

pub(super) fn ensure_buffer_allocated() -> bool {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        let allocation = VirtualAlloc(
            ptr::null(),
            BUFFER_SIZE,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if allocation.is_null() {
            log::error!("UWP ring sink failed to allocate its PCM buffer");
        } else {
            AUDIO_BUFFER = allocation.cast();
        }
    });
    unsafe { !AUDIO_BUFFER.is_null() }
}

pub(super) struct RingBufferSink {
    format: AudioFormat,
    selection_version: u64,
}

impl RingBufferSink {
    pub fn new(format: AudioFormat) -> Self {
        let _ = ensure_buffer_allocated();
        Self {
            format,
            selection_version: super::backend_selection_version(),
        }
    }

    fn backend_selection_changed(&self) -> bool {
        selection_changed(self.selection_version, super::backend_selection_version())
    }
}

fn selection_changed(sink_version: u64, current_version: u64) -> bool {
    sink_version != current_version
}

impl Sink for RingBufferSink {
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

impl SinkAsBytes for RingBufferSink {
    fn write_bytes(&mut self, data: &[u8]) -> SinkResult<()> {
        let len = data.len();
        unsafe {
            if self.backend_selection_changed() {
                // Recreating the managed AudioGraph stops its consumer. A
                // producer already waiting on a full ring must yield here so
                // UwpSink can observe and activate the newly selected backend.
                return Ok(());
            }
            if AUDIO_BUFFER.is_null() {
                return Err(SinkError::NotConnected(
                    "ring buffer was not allocated".into(),
                ));
            }

            loop {
                if self.backend_selection_changed() {
                    return Ok(());
                }
                let write = WRITE_SEQUENCE.load(Ordering::Acquire);
                let read = READ_SEQUENCE.load(Ordering::Acquire);
                let used = write.saturating_sub(read).min(BUFFER_SIZE as u64) as usize;
                if BUFFER_SIZE - used >= len {
                    break;
                }
                thread::yield_now();
                thread::sleep(Duration::from_micros(500));
            }

            let write_sequence = WRITE_SEQUENCE.load(Ordering::Acquire);
            let cursor = write_sequence as usize % BUFFER_SIZE;
            let first_copy = (BUFFER_SIZE - cursor).min(len);
            ptr::copy_nonoverlapping(data.as_ptr(), AUDIO_BUFFER.add(cursor), first_copy);
            if len > first_copy {
                ptr::copy_nonoverlapping(
                    data.as_ptr().add(first_copy),
                    AUDIO_BUFFER,
                    len - first_copy,
                );
            }
            WRITE_SEQUENCE.store(write_sequence + len as u64, Ordering::Release);
        }

        super::super::TOTAL_WRITTEN.fetch_add(len, Ordering::SeqCst);
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
pub extern "C" fn librespot_audio_set_read_cursor(position: usize) {
    let write_sequence = WRITE_SEQUENCE.load(Ordering::Acquire);
    let write_cursor = write_sequence as usize % BUFFER_SIZE;
    let distance = (BUFFER_SIZE + write_cursor - (position % BUFFER_SIZE)) % BUFFER_SIZE;
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_audio_get_state(
    generation: *mut u64,
    generation_start_sequence: *mut u64,
    write_sequence: *mut u64,
) {
    for _ in 0..3 {
        let before = STATE_VERSION.load(Ordering::Acquire);
        let current_generation = GENERATION.load(Ordering::Acquire);
        let current_start = GENERATION_START_SEQUENCE.load(Ordering::Acquire);
        let current_write = WRITE_SEQUENCE.load(Ordering::Acquire);
        let after = STATE_VERSION.load(Ordering::Acquire);
        if before == after && before & 1 == 0 {
            unsafe {
                write_snapshot(
                    generation,
                    generation_start_sequence,
                    write_sequence,
                    current_generation,
                    current_start,
                    current_write,
                );
            }
            return;
        }
        std::hint::spin_loop();
    }

    unsafe {
        write_snapshot(
            generation,
            generation_start_sequence,
            write_sequence,
            GENERATION.load(Ordering::Acquire),
            GENERATION_START_SEQUENCE.load(Ordering::Acquire),
            WRITE_SEQUENCE.load(Ordering::Acquire),
        );
    }
}

unsafe fn write_snapshot(
    generation: *mut u64,
    generation_start_sequence: *mut u64,
    write_sequence: *mut u64,
    current_generation: u64,
    current_start: u64,
    current_write: u64,
) {
    unsafe {
        if let Some(value) = generation.as_mut() {
            *value = current_generation;
        }
        if let Some(value) = generation_start_sequence.as_mut() {
            *value = current_start;
        }
        if let Some(value) = write_sequence.as_mut() {
            *value = current_write;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::selection_changed;

    #[test]
    fn a_blocked_ring_write_yields_to_a_backend_change() {
        assert!(!selection_changed(7, 7));
        assert!(selection_changed(7, 8));
    }
}
