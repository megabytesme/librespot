use std::ffi::{c_char, c_void};

#[repr(C)]
pub struct LibrespotConfig {
    pub device_name: *const c_char,
    pub device_type: *const c_char,
    pub cache_dir: *const c_char,
    pub enable_discovery: bool,
    pub enable_volume_normalisation: bool,
    pub bitrate: Bitrate,
    pub format: AudioFormat,
    pub username: *const c_char,
    pub password: *const c_char,
    pub auth_blob: *const c_char,
    pub access_token: *const c_char,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Bitrate {
    B96 = 96,
    B160 = 160,
    B320 = 320,
}

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum AudioFormat {
    F64 = 0,
    F32 = 1,
    S32 = 2,
    S24 = 3,
    S24_3 = 4,
    S16 = 5,
}

#[repr(C)]
pub enum EventType {
    LogMessage = 0,
    SessionConnected = 1,
    SessionDisconnected = 2,
    AuthNeeded = 3,
    TrackChanged = 4,
    PlaybackPaused = 5,
    PlaybackResumed = 6,
    PlaybackStopped = 7,
    VolumeChanged = 8,
    Panic = 9,
}

#[repr(C)]
pub struct TrackMetadata {
    pub uri: *const c_char,
    pub name: *const c_char,
    pub artist: *const c_char,
    pub album: *const c_char,
    pub cover_url: *const c_char,
    pub duration_ms: u32,
}

#[repr(C)]
pub union EventData {
    pub log_msg: *const c_char,
    pub track: std::mem::ManuallyDrop<TrackMetadata>,
    pub volume: u16,
    pub session_user: *const c_char,
}

#[repr(C)]
pub struct LibrespotEvent {
    pub event_type: EventType,
    pub data: EventData,
}

pub type LibrespotCallback = extern "C" fn(*const LibrespotEvent, *mut c_void);
