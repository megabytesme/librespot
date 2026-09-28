//! Minimal ABI bindings for the UWP-safe Windows audio APIs used here.
//!
//! `windows-rs` does not generate its Win32 audio module for Rust's legacy
//! ARM32 UWP target.  Keeping this small binding surface local lets the same
//! WASAPI and XAudio2 implementations build for ARM32, ARM64, x86 and x64.

use std::ffi::c_void;
use std::ptr;

pub type HResult = i32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn from_u128(value: u128) -> Self {
        Self {
            data1: (value >> 96) as u32,
            data2: (value >> 80) as u16,
            data3: (value >> 64) as u16,
            data4: (value as u64).to_be_bytes(),
        }
    }
}

pub const IID_IUNKNOWN: Guid = Guid::from_u128(0x00000000_0000_0000_c000_000000000046);

#[repr(C)]
pub struct IUnknownVTable {
    pub query_interface:
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult,
    pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub release: unsafe extern "system" fn(*mut c_void) -> u32,
}

pub unsafe fn vtable<T>(interface: *mut c_void) -> &'static T {
    unsafe { &**(interface.cast::<*const T>()) }
}

#[derive(Debug)]
pub struct ComPtr(*mut c_void);

impl ComPtr {
    pub unsafe fn from_raw(value: *mut c_void) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }

    pub fn as_raw(&self) -> *mut c_void {
        self.0
    }

    pub unsafe fn query(&self, iid: &Guid) -> Result<Self, HResult> {
        let base = unsafe { vtable::<IUnknownVTable>(self.0) };
        let mut value = ptr::null_mut();
        let result = unsafe { (base.query_interface)(self.0, iid, &mut value) };
        if result < 0 {
            Err(result)
        } else {
            unsafe { Self::from_raw(value) }.ok_or(0x8000_4003u32 as i32)
        }
    }
}

impl Clone for ComPtr {
    fn clone(&self) -> Self {
        let base = unsafe { vtable::<IUnknownVTable>(self.0) };
        unsafe { (base.add_ref)(self.0) };
        Self(self.0)
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        let base = unsafe { vtable::<IUnknownVTable>(self.0) };
        unsafe { (base.release)(self.0) };
    }
}

unsafe impl Send for ComPtr {}
unsafe impl Sync for ComPtr {}

#[repr(C, packed(1))]
#[derive(Clone, Copy, Default)]
pub struct WaveFormatEx {
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub average_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    pub extra_size: u16,
}

pub fn succeeded(result: HResult) -> bool {
    result >= 0
}
