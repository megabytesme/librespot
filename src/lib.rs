mod config;
mod ffi_types;
mod logger;
mod runner;

use crate::ffi_types::*;
use crate::runner::{
    AppDataPayload, LibrespotCommand, Runner, RunnerState, alloc_ffi_album, alloc_ffi_artist,
    alloc_ffi_artist_list, alloc_ffi_playlist, alloc_ffi_playlist_list, alloc_ffi_search,
    alloc_ffi_track, alloc_ffi_track_list, alloc_ffi_user_profile,
};
use librespot_core::{FileId, cache::Cache};
use std::ffi::{CStr, CString, c_char};
use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::thread;
use tokio::sync::mpsc;

pub struct LibrespotInstance {
    cmd_tx: mpsc::UnboundedSender<LibrespotCommand>,
    state: Arc<RunnerState>,
    cache: Arc<Cache>,
    _thread_handle: thread::JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct UserDataWrapper(*mut c_void);
unsafe impl Send for UserDataWrapper {}
unsafe impl Sync for UserDataWrapper {}

static LAST_ERROR: OnceLock<Mutex<String>> = OnceLock::new();

fn set_last_error(message: impl Into<String>) {
    let store = LAST_ERROR.get_or_init(|| Mutex::new(String::new()));
    if let Ok(mut slot) = store.lock() {
        *slot = message.into();
    }
}

fn clear_last_error() {
    set_last_error(String::new());
}

fn request_appdata(
    instance: *mut LibrespotInstance,
    kind: i32,
    argument: *const c_char,
) -> Result<AppDataPayload, String> {
    if instance.is_null() || argument.is_null() {
        let err = "instance or argument pointer was null".to_string();
        set_last_error(err.clone());
        return Err(err);
    }

    let argument = match unsafe { CStr::from_ptr(argument) }.to_str() {
        Ok(value) if !value.is_empty() => value.to_owned(),
        _ => {
            let err = "argument string was invalid or empty".to_string();
            set_last_error(err.clone());
            return Err(err);
        }
    };

    let (result_tx, result_rx) = std_mpsc::channel();
    let inst = unsafe { instance.as_ref() };
    let Some(inst) = inst else {
        let err = "instance reference was invalid".to_string();
        set_last_error(err.clone());
        return Err(err);
    };

    if inst
        .cmd_tx
        .send(LibrespotCommand::GetAppData {
            kind,
            argument,
            result_tx,
        })
        .is_err()
    {
        let err = "failed to send GetAppData command to runner".to_string();
        set_last_error(err.clone());
        return Err(err);
    }

    match result_rx.recv() {
        Ok(Ok(payload)) => {
            clear_last_error();
            Ok(payload)
        }
        Ok(Err(err)) => {
            set_last_error(err.clone());
            log::error!("librespot appdata request failed for kind {}: {}", kind, err);
            Err(err)
        }
        Err(err) => {
            let err = format!("failed to receive GetAppData result: {}", err);
            set_last_error(err.clone());
            Err(err)
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_new(
    config: LibrespotConfig,
    callback: LibrespotCallback,
    user_data: *mut c_void,
) -> *mut LibrespotInstance {
    let _ = logger::init_logger(callback, user_data);

    let setup = match config::parse_ffi_config(&config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to parse config: {e}");
            return std::ptr::null_mut();
        }
    };

    let cache = match Cache::new(
        Some(setup.cache_dir.clone()),
        Some(setup.cache_dir.clone()),
        Some(setup.cache_dir.join("audio")),
        Some(setup.persisted_cache_dir.join("audio")),
        Some(1024 * 1024 * 500),
        setup.key_remove_callback,
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Failed to create cache: {e}");
            return std::ptr::null_mut();
        }
    };

    let (tx, rx) = mpsc::unbounded_channel();

    let runner_state = Arc::new(RunnerState::new(setup.audio_format, 44100));
    let state_for_thread = runner_state.clone();
    let cache_for_thread = cache.clone();

    let user_data_wrapper = UserDataWrapper(user_data);

    let thread_handle = thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

        rt.block_on(async move {
            let mut runner = Runner::new(
                setup,
                cache_for_thread,
                rx,
                callback,
                user_data_wrapper,
                state_for_thread,
            );
            runner.run().await;
        });
    });

    let instance = Box::new(LibrespotInstance {
        cmd_tx: tx,
        state: runner_state,
        cache,
        _thread_handle: thread_handle,
    });

    Box::into_raw(instance)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_free(instance: *mut LibrespotInstance) {
    if !instance.is_null() {
        unsafe {
            let _ = Box::from_raw(instance);
        }
    }
}

fn send_cmd(instance: *mut LibrespotInstance, cmd: LibrespotCommand) {
    let inst = unsafe { instance.as_ref() };
    if let Some(inst) = inst {
        let _ = inst.cmd_tx.send(cmd);
    }
}

fn parse_file_id_hex(s: &str) -> Option<FileId> {
    let bytes = hex::decode(s).ok()?;
    Some(FileId::from_raw(&bytes))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_cache_set_persisted(
    instance: *mut LibrespotInstance,
    file_id_hex: *const c_char,
    persisted: bool,
) -> bool {
    if instance.is_null() || file_id_hex.is_null() {
        return false;
    }

    let inst = unsafe { instance.as_ref() };
    let inst = match inst {
        Some(i) => i,
        None => return false,
    };

    let hex = unsafe { CStr::from_ptr(file_id_hex) };
    let hex = match hex.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let file_id = match parse_file_id_hex(hex) {
        Some(id) => id,
        None => return false,
    };

    inst.cache.set_persisted(file_id, persisted).is_ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_track_set_persisted(
    instance: *mut LibrespotInstance,
    track_uri: *const c_char,
    persisted: bool,
) -> bool {
    if instance.is_null() || track_uri.is_null() {
        return false;
    }

    let track_uri = unsafe { CStr::from_ptr(track_uri) };
    let track_uri = match track_uri.to_str() {
        Ok(s) if !s.is_empty() => s.to_owned(),
        _ => return false,
    };

    let (result_tx, result_rx) = std_mpsc::channel();
    let inst = unsafe { instance.as_ref() };
    let Some(inst) = inst else {
        return false;
    };

    if inst
        .cmd_tx
        .send(LibrespotCommand::SetTrackPersisted {
            track_uri,
            persisted,
            result_tx,
        })
        .is_err()
    {
        return false;
    }

    result_rx.recv().unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_appdata_get(
    instance: *mut LibrespotInstance,
    kind: i32,
    argument: *const c_char,
) -> *mut c_char {
    match request_appdata(instance, kind, argument) {
        Ok(payload) => {
            let json = match serde_json::to_string(&payload) {
                Ok(json) => json,
                Err(err) => {
                    set_last_error(format!("failed to serialize appdata payload: {}", err));
                    return std::ptr::null_mut();
                }
            };
            clear_last_error();
            CString::new(json)
                .map(CString::into_raw)
                .unwrap_or_else(|err| {
                    set_last_error(format!("failed to marshal json as C string: {}", err));
                    std::ptr::null_mut()
                })
        }
        Err(_) => std::ptr::null_mut(),
    }
}

macro_rules! appdata_typed_get {
    ($name:ident, $kind:expr, $variant:path, $alloc:ident, $ret:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            instance: *mut LibrespotInstance,
            argument: *const c_char,
        ) -> *mut $ret {
            match request_appdata(instance, $kind, argument) {
                Ok(payload) => match payload {
                    $variant(inner) => $alloc(inner),
                    _ => {
                        set_last_error("received unexpected appdata payload variant");
                        std::ptr::null_mut()
                    }
                },
                Err(_) => std::ptr::null_mut(),
            }
        }
    };
}

appdata_typed_get!(librespot_track_get, 1, AppDataPayload::Track, alloc_ffi_track, FfiTrack);
appdata_typed_get!(librespot_album_get, 2, AppDataPayload::Album, alloc_ffi_album, FfiAlbum);
appdata_typed_get!(librespot_artist_get, 3, AppDataPayload::Artist, alloc_ffi_artist, FfiArtist);
appdata_typed_get!(
    librespot_playlist_get,
    4,
    AppDataPayload::Playlist,
    alloc_ffi_playlist,
    FfiPlaylist
);
appdata_typed_get!(
    librespot_user_profile_get,
    5,
    AppDataPayload::UserProfile,
    alloc_ffi_user_profile,
    FfiUserProfile
);
appdata_typed_get!(
    librespot_user_playlists_get,
    6,
    AppDataPayload::UserPlaylists,
    alloc_ffi_playlist_list,
    FfiPlaylistList
);
appdata_typed_get!(
    librespot_saved_tracks_get,
    7,
    AppDataPayload::SavedTracks,
    alloc_ffi_track_list,
    FfiTrackList
);
appdata_typed_get!(librespot_search_get, 8, AppDataPayload::Search, alloc_ffi_search, FfiSearch);
appdata_typed_get!(
    librespot_followed_artists_get,
    9,
    AppDataPayload::FollowedArtists,
    alloc_ffi_artist_list,
    FfiArtistList
);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_last_error_get() -> *mut c_char {
    let store = LAST_ERROR.get_or_init(|| Mutex::new(String::new()));
    let message = store
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| "failed to lock last error store".to_string());

    CString::new(message)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_string_free(value: *mut c_char) {
    if !value.is_null() {
        let _ = unsafe { CString::from_raw(value) };
    }
}

unsafe fn free_c_string(value: *mut c_char) {
    if !value.is_null() {
        let _ = CString::from_raw(value);
    }
}

unsafe fn free_boxed_slice<T>(ptr: *mut T, len: usize) {
    if !ptr.is_null() {
        let raw = std::ptr::slice_from_raw_parts_mut(ptr, len);
        let _ = Box::from_raw(raw);
    }
}

unsafe fn free_images(ptr: *mut FfiImage, len: usize) {
    if ptr.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr, len);
    for item in slice {
        free_c_string(item.url);
    }
    free_boxed_slice(ptr, len);
}

unsafe fn free_artist_summaries(ptr: *mut FfiArtistSummary, len: usize) {
    if ptr.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr, len);
    for item in slice {
        free_c_string(item.id);
        free_c_string(item.uri);
        free_c_string(item.name);
    }
    free_boxed_slice(ptr, len);
}

unsafe fn free_album_summary(ptr: *mut FfiAlbumSummary) {
    if ptr.is_null() {
        return;
    }
    let item = &mut *ptr;
    free_c_string(item.id);
    free_c_string(item.uri);
    free_c_string(item.name);
    free_c_string(item.album_type);
    free_c_string(item.release_date);
    free_images(item.images, item.image_count);
    free_artist_summaries(item.artists, item.artist_count);
    let _ = Box::from_raw(ptr);
}

unsafe fn free_album_summaries(ptr: *mut FfiAlbumSummary, len: usize) {
    if ptr.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr, len);
    for item in slice {
        free_c_string(item.id);
        free_c_string(item.uri);
        free_c_string(item.name);
        free_c_string(item.album_type);
        free_c_string(item.release_date);
        free_images(item.images, item.image_count);
        free_artist_summaries(item.artists, item.artist_count);
    }
    free_boxed_slice(ptr, len);
}

unsafe fn free_simple_tracks(ptr: *mut FfiSimpleTrack, len: usize) {
    if ptr.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr, len);
    for item in slice {
        free_c_string(item.id);
        free_c_string(item.uri);
        free_c_string(item.name);
        free_artist_summaries(item.artists, item.artist_count);
    }
    free_boxed_slice(ptr, len);
}

unsafe fn free_tracks(ptr: *mut FfiTrack, len: usize) {
    if ptr.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr, len);
    for item in slice {
        free_c_string(item.id);
        free_c_string(item.uri);
        free_c_string(item.name);
        free_artist_summaries(item.artists, item.artist_count);
        free_album_summary(item.album);
    }
    free_boxed_slice(ptr, len);
}

unsafe fn free_owner(ptr: *mut FfiOwner) {
    if ptr.is_null() {
        return;
    }
    let item = &mut *ptr;
    free_c_string(item.id);
    free_c_string(item.display_name);
    let _ = Box::from_raw(ptr);
}

unsafe fn free_playlist_summaries(ptr: *mut FfiPlaylistSummary, len: usize) {
    if ptr.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr, len);
    for item in slice {
        free_c_string(item.id);
        free_c_string(item.uri);
        free_c_string(item.name);
        free_images(item.images, item.image_count);
    }
    free_boxed_slice(ptr, len);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_track_free(value: *mut FfiTrack) {
    free_tracks(value, 1);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_album_free(value: *mut FfiAlbum) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_c_string(item.id);
    free_c_string(item.uri);
    free_c_string(item.name);
    free_c_string(item.album_type);
    free_c_string(item.release_date);
    free_images(item.images, item.image_count);
    free_artist_summaries(item.artists, item.artist_count);
    free_simple_tracks(item.tracks, item.track_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_artist_free(value: *mut FfiArtist) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_c_string(item.id);
    free_c_string(item.uri);
    free_c_string(item.name);
    free_images(item.images, item.image_count);
    free_album_summaries(item.albums, item.album_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_playlist_free(value: *mut FfiPlaylist) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_c_string(item.id);
    free_c_string(item.uri);
    free_c_string(item.name);
    free_images(item.images, item.image_count);
    free_owner(item.owner);
    free_tracks(item.tracks, item.track_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_user_profile_free(value: *mut FfiUserProfile) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_c_string(item.id);
    free_c_string(item.uri);
    free_c_string(item.display_name);
    free_c_string(item.email);
    free_c_string(item.country);
    free_images(item.images, item.image_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_playlist_list_free(value: *mut FfiPlaylistList) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_playlist_summaries(item.items, item.item_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_track_list_free(value: *mut FfiTrackList) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_tracks(item.items, item.item_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_artist_list_free(value: *mut FfiArtistList) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_artist_summaries(item.items, item.item_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_search_free(value: *mut FfiSearch) {
    if value.is_null() {
        return;
    }
    let item = &mut *value;
    free_tracks(item.tracks, item.track_count);
    free_album_summaries(item.albums, item.album_count);
    free_artist_summaries(item.artists, item.artist_count);
    free_playlist_summaries(item.playlists, item.playlist_count);
    let _ = Box::from_raw(value);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_load(
    instance: *mut LibrespotInstance,
    context_uri: *const c_char,
    start_uri: *const c_char,
    play: bool,
) {
    let context_str = unsafe { CStr::from_ptr(context_uri) }
        .to_string_lossy()
        .into_owned();

    let start_str = if !start_uri.is_null() {
        Some(
            unsafe { CStr::from_ptr(start_uri) }
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };

    send_cmd(
        instance,
        LibrespotCommand::Load {
            context_uri: context_str,
            start_from_uri: start_str,
            play,
        },
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_play(instance: *mut LibrespotInstance) {
    send_cmd(instance, LibrespotCommand::Play);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_pause(instance: *mut LibrespotInstance) {
    send_cmd(instance, LibrespotCommand::Pause);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_stop(instance: *mut LibrespotInstance) {
    send_cmd(instance, LibrespotCommand::Stop);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_next(instance: *mut LibrespotInstance) {
    send_cmd(instance, LibrespotCommand::Next);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_prev(instance: *mut LibrespotInstance) {
    send_cmd(instance, LibrespotCommand::Prev);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_seek(instance: *mut LibrespotInstance, pos_ms: u32) {
    send_cmd(instance, LibrespotCommand::Seek(pos_ms));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_set_volume(instance: *mut LibrespotInstance, volume: u16) {
    send_cmd(instance, LibrespotCommand::SetVolume(volume));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_set_shuffle(instance: *mut LibrespotInstance, enabled: bool) {
    send_cmd(instance, LibrespotCommand::SetShuffle(enabled));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_set_repeat(instance: *mut LibrespotInstance, mode: u32) {
    send_cmd(instance, LibrespotCommand::SetRepeatContext(mode >= 1));
    send_cmd(instance, LibrespotCommand::SetRepeatTrack(mode == 2));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_get_position_ms(instance: *mut LibrespotInstance) -> u32 {
    let inst = unsafe { instance.as_ref() };
    if let Some(inst) = inst {
        let state = &inst.state;

        let base_ms = state.position_ms.load(Ordering::Acquire);
        let sync_wp = state.sync_write_pos.load(Ordering::Acquire);

        let current_wp = librespot_playback::audio_backend::get_write_pos();

        if current_wp <= sync_wp {
            return base_ms;
        }

        let bytes_delta = current_wp - sync_wp;

        let sample_rate = state.sample_rate.load(Ordering::Acquire) as f32;
        let bytes_per_sample = state.bytes_per_sample.load(Ordering::Acquire) as f32;
        let channels = 2.0;

        let bytes_per_ms = (sample_rate * channels * bytes_per_sample) / 1000.0;

        if bytes_per_ms <= 0.0 {
            return base_ms;
        }

        let drift_ms = (bytes_delta as f32 / bytes_per_ms) as u32;
        return base_ms + drift_ms;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_get_duration_ms(instance: *mut LibrespotInstance) -> u32 {
    let inst = unsafe { instance.as_ref() };
    inst.map_or(0, |i| i.state.duration_ms.load(Ordering::Acquire))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_get_current_track_info(
    instance: *mut LibrespotInstance,
) -> TrackMetadata {
    let inst = unsafe { instance.as_ref() };
    if let Some(inst) = inst {
        if let Ok(current) = inst.state.current_track.read() {
            if let Some(ref meta) = *current {
                return TrackMetadata {
                    uri: meta.uri.as_ptr(),
                    name: meta.name.as_ptr(),
                    artist: meta.artist.as_ptr(),
                    album: meta.album.as_ptr(),
                    cover_url: meta.cover_url.as_ptr(),
                    duration_ms: meta.duration_ms,
                };
            }
        }
    }

    unsafe { std::mem::zeroed() }
}
