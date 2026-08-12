use std::{
    ffi::{c_char, c_void},
    mem::ManuallyDrop,
};

use librespot_core::{LibrespotKeyCallback, LibrespotKeyRemoveCallback, LibrespotKeySaveCallback};

#[repr(C)]
pub struct LibrespotConfig {
    pub device_name: *const c_char,
    pub device_type: *const c_char,
    pub cache_dir: *const c_char,
    pub persisted_cache_dir: *const c_char,
    pub enable_discovery: bool,
    pub enable_volume_normalisation: bool,
    pub bitrate: Bitrate,
    pub format: AudioFormat,
    pub initial_volume: u16,
    pub username: *const c_char,
    pub password: *const c_char,
    pub auth_blob: *const c_char,
    pub access_token: *const c_char,
    pub playback_credentials: *const c_char,
    pub key_callback: Option<LibrespotKeyCallback>,
    pub key_save_callback: Option<LibrespotKeySaveCallback>,
    pub key_remove_callback: Option<LibrespotKeyRemoveCallback>,
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
    ShuffleChanged = 10,
    RepeatChanged = 11,
    AutoPlayChanged = 12,
    Seeked = 13,
    PositionCorrection = 14,
    PlaybackLoading = 15,
    PlaybackUnavailable = 16,
    EndOfTrack = 17,
    ClientChanged = 18,
    ExplicitFilterChanged = 19,
    PlayRequestIdChanged = 20,
    AddedToQueue = 21,
    Preloading = 22,
    TimeToPreloadNextTrack = 23,
    PositionChanged = 24,
    PlaybackKeyUnavailable = 25,
    PlaybackAuthorizationRejected = 26,
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
pub struct EventData {
    pub play_request_id: u64,
    pub track_uri: *const c_char,
    pub position_ms: u32,
    pub duration_ms: u32,
    pub volume: u16,
    pub is_playing: bool,
    pub shuffle: bool,
    pub repeat_mode: u32,
    pub auto_play: bool,
    pub filter_explicit: bool,
    pub track: ManuallyDrop<TrackMetadata>,
    pub session_user: *const c_char,
    pub client_name: *const c_char,
    pub log_msg: *const c_char,
    pub audio_generation: u64,
    pub was_preloaded: bool,
}

#[repr(C)]
pub struct LibrespotEvent {
    pub event_type: EventType,
    pub data: EventData,
}

pub type LibrespotCallback = extern "C" fn(*const LibrespotEvent, *mut c_void);

#[repr(C)]
pub struct FfiImage {
    pub url: *mut c_char,
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
pub struct FfiArtistSummary {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
}

#[repr(C)]
pub struct FfiAlbumSummary {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub album_type: *mut c_char,
    pub release_date: *mut c_char,
    pub total_tracks: i32,
    pub images: *mut FfiImage,
    pub image_count: usize,
    pub artists: *mut FfiArtistSummary,
    pub artist_count: usize,
}

#[repr(C)]
pub struct FfiSimpleTrack {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub duration_ms: i32,
    pub disc_number: i32,
    pub track_number: i32,
    pub artists: *mut FfiArtistSummary,
    pub artist_count: usize,
}

#[repr(C)]
pub struct FfiTrack {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub duration_ms: i32,
    pub disc_number: i32,
    pub track_number: i32,
    pub artists: *mut FfiArtistSummary,
    pub artist_count: usize,
    pub album: *mut FfiAlbumSummary,
}

#[repr(C)]
pub struct FfiAlbum {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub album_type: *mut c_char,
    pub release_date: *mut c_char,
    pub total_tracks: i32,
    pub images: *mut FfiImage,
    pub image_count: usize,
    pub artists: *mut FfiArtistSummary,
    pub artist_count: usize,
    pub tracks: *mut FfiSimpleTrack,
    pub track_count: usize,
}

#[repr(C)]
pub struct FfiArtist {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub images: *mut FfiImage,
    pub image_count: usize,
    pub albums: *mut FfiAlbumSummary,
    pub album_count: usize,
}

#[repr(C)]
pub struct FfiOwner {
    pub id: *mut c_char,
    pub display_name: *mut c_char,
}

#[repr(C)]
pub struct FfiPlaylistSummary {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub images: *mut FfiImage,
    pub image_count: usize,
}

#[repr(C)]
pub struct FfiPlaylist {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub name: *mut c_char,
    pub images: *mut FfiImage,
    pub image_count: usize,
    pub owner: *mut FfiOwner,
    pub tracks: *mut FfiTrack,
    pub track_count: usize,
}

#[repr(C)]
pub struct FfiUserProfile {
    pub id: *mut c_char,
    pub uri: *mut c_char,
    pub display_name: *mut c_char,
    pub email: *mut c_char,
    pub country: *mut c_char,
    pub images: *mut FfiImage,
    pub image_count: usize,
}

#[repr(C)]
pub struct FfiPlaylistList {
    pub items: *mut FfiPlaylistSummary,
    pub item_count: usize,
}

#[repr(C)]
pub struct FfiTrackList {
    pub items: *mut FfiTrack,
    pub item_count: usize,
}

#[repr(C)]
pub struct FfiArtistList {
    pub items: *mut FfiArtistSummary,
    pub item_count: usize,
}

#[repr(C)]
pub struct FfiSearch {
    pub tracks: *mut FfiTrack,
    pub track_count: usize,
    pub albums: *mut FfiAlbumSummary,
    pub album_count: usize,
    pub artists: *mut FfiArtistSummary,
    pub artist_count: usize,
    pub playlists: *mut FfiPlaylistSummary,
    pub playlist_count: usize,
}
