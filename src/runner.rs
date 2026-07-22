use crate::{UserDataWrapper, ffi_types::*};
use base64::Engine;
use futures_util::StreamExt;
use librespot_audio::Range;
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::{
    FileId, Session, SessionConfig, SpotifyId, SpotifyUri, authentication::Credentials,
    cache::Cache, config::DeviceType,
};
use librespot_discovery::Discovery;
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use librespot_metadata::{
    Album as LibrespotAlbum, Artist as LibrespotArtist, Episode as LibrespotEpisode,
    Lyrics as LibrespotLyrics, Metadata, Playlist as LibrespotPlaylist, Show as LibrespotShow,
    Track as LibrespotTrack,
    playlist::annotation::PlaylistAnnotation as LibrespotPlaylistAnnotation,
};
use librespot_playback::{
    config::{AudioFormat, PlayerConfig},
    mixer::{self, MixerConfig},
    player::{OfflineTrackMetadata, Player, PlayerEvent},
};
use librespot_protocol::{
    autoplay_context_request::AutoplayContextRequest, context::Context,
    playlist4_external::SelectedListContent,
};
use protobuf::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, atomic::AtomicU8};
use std::{
    ffi::{CString, c_char},
    path::PathBuf,
    sync::atomic::AtomicUsize,
};
use std::{mem::ManuallyDrop, pin::Pin};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{Duration, Instant, sleep, sleep_until};

#[derive(Debug)]
#[allow(dead_code)]
pub enum LibrespotCommand {
    Shutdown,
    Load {
        context_uri: String,
        start_from_uri: Option<String>,
        ordered_track_uris: Option<Vec<String>>,
        play: bool,
    },
    Play,
    Pause,
    Next,
    Prev,
    SetVolume(u16),
    Seek(u32),
    SetShuffle(bool),
    SetRepeatContext(bool),
    SetRepeatTrack(bool),
    StartDiscovery,
    UpdateCredentials {
        username: String,
        auth_data: String,
    },
    SetTrackPersisted {
        track_uri: String,
        persisted: bool,
        result_tx: std_mpsc::Sender<bool>,
    },
    GetAppData {
        kind: i32,
        argument: String,
        result_tx: std_mpsc::Sender<Result<AppDataPayload, String>>,
    },
    Stop,
}

pub struct RunnerSetup {
    pub connect_config: ConnectConfig,
    pub session_config: SessionConfig,
    pub player_config: PlayerConfig,
    pub mixer_config: MixerConfig,
    pub audio_backend: librespot_playback::audio_backend::SinkBuilder,
    pub audio_format: AudioFormat,
    pub initial_creds: Option<Credentials>,
    pub enable_discovery: bool,
    pub device_name: String,
    pub device_type: DeviceType,
    pub zeroconf_port: u16,
    pub key_callback: Option<librespot_core::LibrespotKeyCallback>,
    pub key_save_callback: Option<librespot_core::LibrespotKeySaveCallback>,
    pub key_remove_callback: Option<librespot_core::LibrespotKeyRemoveCallback>,
    pub cache_dir: PathBuf,
    pub persisted_cache_dir: PathBuf,
}

pub struct TrackMetadataInternal {
    pub uri: CString,
    pub name: CString,
    pub artist: CString,
    pub album: CString,
    pub cover_url: CString,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OfflineTrackIndexEntry {
    track_uri: String,
    file_id_hex: String,
    format: i32,
    name: String,
    artist: String,
    album: String,
    cover_url: String,
    duration_ms: u32,
    is_explicit: bool,
}

type SharedOfflineIndex = Arc<Mutex<HashMap<String, OfflineTrackIndexEntry>>>;

const SLOW_APP_DATA_WARNING_MS: u128 = 3_000;

#[derive(Clone)]
struct AppDataWorker {
    offline_index: SharedOfflineIndex,
    lyrics_volatile_dir: PathBuf,
    lyrics_persisted_dir: PathBuf,
}

struct TrackPersistenceWorker {
    session: Session,
    cache: Arc<Cache>,
    offline_index: SharedOfflineIndex,
    offline_index_path: PathBuf,
    download_gate: Arc<Semaphore>,
    lyrics_volatile_dir: PathBuf,
    lyrics_persisted_dir: PathBuf,
    preferred_formats: [AudioFileFormat; 7],
}

impl TrackPersistenceWorker {
    async fn set_track_persisted_async(
        &self,
        track_uri: &str,
        persisted: bool,
    ) -> Result<(), librespot_core::Error> {
        if !persisted {
            self.remove_track_persistence(track_uri)?;
            AppDataWorker::remove_cached_lyrics_from(
                track_uri,
                &self.lyrics_volatile_dir,
                &self.lyrics_persisted_dir,
            );
            return Ok(());
        }

        let _permit = self
            .download_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| librespot_core::Error::cancelled(err.to_string()))?;

        let parsed_uri = SpotifyUri::from_uri(track_uri)?;
        let track_id: librespot_core::SpotifyId = (&parsed_uri).try_into().map_err(|_| {
            librespot_core::Error::invalid_argument("track URI is not a playable Spotify track")
        })?;
        let audio_item = AudioItem::get_file(&self.session, parsed_uri).await?;
        let (format, file_id) = self
            .preferred_formats
            .iter()
            .find_map(|format| {
                audio_item
                    .files
                    .get(format)
                    .copied()
                    .map(|file_id| (*format, file_id))
            })
            .ok_or_else(|| {
                librespot_core::Error::unavailable("track has no supported audio file")
            })?;

        let already_persisted = self
            .cache
            .persisted_file_path(file_id)
            .map(|path| path.exists())
            .unwrap_or(false);

        if already_persisted {
            self.store_persisted_track(track_uri, &audio_item, format, file_id)?;
            let _ = AppDataWorker::move_cached_lyrics_in(
                track_uri,
                true,
                &self.lyrics_volatile_dir,
                &self.lyrics_persisted_dir,
            );
            let _ = AppDataWorker::prefetch_lyrics_payload_for(
                &self.session,
                track_uri,
                &self.offline_index,
                &self.lyrics_volatile_dir,
                &self.lyrics_persisted_dir,
            )
            .await;
            return Ok(());
        }

        let bytes_per_second = 40 * 1024;
        let audio_file =
            librespot_audio::AudioFile::open(&self.session, file_id, bytes_per_second).await?;
        let controller = audio_file.get_stream_loader_controller()?;
        controller.set_random_access_mode();

        let _key = self.session.audio_key().request(track_id, file_id).await?;
        log::info!("Fetched audio key while persisting track <{}>", track_uri);

        let total_len = controller.len();
        let chunk_len = 256 * 1024;
        tokio::task::spawn_blocking(move || {
            let mut offset = 0;
            while offset < total_len {
                let remaining = total_len - offset;
                let next_len = std::cmp::min(chunk_len, remaining);
                controller.fetch_blocking(Range::new(offset, next_len))?;
                offset += next_len;
            }

            Ok::<(), librespot_core::Error>(())
        })
        .await
        .map_err(|err| librespot_core::Error::cancelled(err.to_string()))??;

        for _ in 0..100 {
            if self.cache.file(file_id).is_some() {
                self.cache.set_persisted(file_id, true)?;
                self.store_persisted_track(track_uri, &audio_item, format, file_id)?;

                let _ = AppDataWorker::move_cached_lyrics_in(
                    track_uri,
                    true,
                    &self.lyrics_volatile_dir,
                    &self.lyrics_persisted_dir,
                );

                let _ = AppDataWorker::prefetch_lyrics_payload_for(
                    &self.session,
                    track_uri,
                    &self.offline_index,
                    &self.lyrics_volatile_dir,
                    &self.lyrics_persisted_dir,
                )
                .await;

                return Ok(());
            }

            sleep(Duration::from_millis(100)).await;
        }

        Err(librespot_core::Error::deadline_exceeded(
            "timed out waiting for cached track file",
        ))
    }

    fn remove_track_persistence(&self, track_uri: &str) -> Result<(), librespot_core::Error> {
        let file_id = self
            .offline_index
            .lock()
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?
            .get(track_uri)
            .and_then(|entry| parse_file_id_hex(&entry.file_id_hex));

        if let Some(file_id) = file_id {
            let is_persisted = self
                .cache
                .persisted_file_path(file_id)
                .map(|path| path.exists())
                .unwrap_or(false);

            if is_persisted {
                self.cache.set_persisted(file_id, false)?;
            }
        }

        self.update_index(|index| {
            index.remove(track_uri);
        })
    }

    fn store_persisted_track(
        &self,
        track_uri: &str,
        audio_item: &AudioItem,
        format: AudioFileFormat,
        file_id: FileId,
    ) -> Result<(), librespot_core::Error> {
        let artist = match &audio_item.unique_fields {
            librespot_metadata::audio::UniqueFields::Track { artists, .. } => artists
                .0
                .first()
                .map(|artist| artist.name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let album = match &audio_item.unique_fields {
            librespot_metadata::audio::UniqueFields::Track { album, .. } => album.clone(),
            _ => String::new(),
        };
        let cover_url = audio_item
            .covers
            .first()
            .map(|cover| cover.url.clone())
            .unwrap_or_default();

        self.update_index(|index| {
            index.insert(
                track_uri.to_owned(),
                OfflineTrackIndexEntry {
                    track_uri: track_uri.to_owned(),
                    file_id_hex: file_id.to_string(),
                    format: format as i32,
                    name: audio_item.name.clone(),
                    artist,
                    album,
                    cover_url,
                    duration_ms: audio_item.duration_ms,
                    is_explicit: audio_item.is_explicit,
                },
            );
        })
    }

    fn update_index<F>(&self, update: F) -> Result<(), librespot_core::Error>
    where
        F: FnOnce(&mut HashMap<String, OfflineTrackIndexEntry>),
    {
        let mut index = self
            .offline_index
            .lock()
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        update(&mut index);
        save_offline_index(&self.offline_index_path, &index);
        Ok(())
    }
}

pub struct RunnerState {
    pub is_playing: AtomicBool,
    pub shuffle: AtomicBool,
    pub repeat: AtomicU32, // 0: Off, 1: Context, 2: Track
    pub position_ms: AtomicU32,
    pub sync_write_pos: AtomicUsize,
    pub duration_ms: AtomicU32,
    pub volume: AtomicU16,
    pub current_track: RwLock<Option<TrackMetadataInternal>>,
    pub sample_rate: AtomicU32,
    pub bytes_per_sample: AtomicU8,
    pub audio_generation: AtomicU64,
}

impl RunnerState {
    pub fn new(format: librespot_playback::config::AudioFormat, sample_rate: u32) -> Self {
        let bytes = match format {
            librespot_playback::config::AudioFormat::F64 => 8,
            librespot_playback::config::AudioFormat::F32 => 4,
            librespot_playback::config::AudioFormat::S32 => 4,
            librespot_playback::config::AudioFormat::S24 => 4,
            librespot_playback::config::AudioFormat::S24_3 => 3,
            librespot_playback::config::AudioFormat::S16 => 2,
        };

        Self {
            is_playing: AtomicBool::new(false),
            shuffle: AtomicBool::new(false),
            repeat: AtomicU32::new(0),
            position_ms: AtomicU32::new(0),
            sync_write_pos: AtomicUsize::new(0),
            duration_ms: AtomicU32::new(0),
            volume: AtomicU16::new(0),
            current_track: RwLock::new(None),
            sample_rate: AtomicU32::new(sample_rate),
            bytes_per_sample: AtomicU8::new(bytes),
            audio_generation: AtomicU64::new(0),
        }
    }
}

pub struct Runner {
    setup: RunnerSetup,
    cache: Arc<Cache>,
    cmd_rx: mpsc::UnboundedReceiver<LibrespotCommand>,
    callback: LibrespotCallback,
    user_data: UserDataWrapper,
    pub state: Arc<RunnerState>,
    offline_index_path: PathBuf,
    offline_index: SharedOfflineIndex,
    download_gate: Arc<Semaphore>,
    lyrics_volatile_dir: PathBuf,
    lyrics_persisted_dir: PathBuf,
}

impl Runner {
    pub fn new(
        setup: RunnerSetup,
        cache: Arc<Cache>,
        cmd_rx: mpsc::UnboundedReceiver<LibrespotCommand>,
        callback: LibrespotCallback,
        user_data: UserDataWrapper,
        state: Arc<RunnerState>,
    ) -> Self {
        let offline_index_path = setup.persisted_cache_dir.join("offline-index.json");
        let offline_index = Arc::new(Mutex::new(load_offline_index(&offline_index_path)));
        let lyrics_volatile_dir = setup.cache_dir.join("lyrics");
        let lyrics_persisted_dir = setup.persisted_cache_dir.join("lyrics");
        Self {
            setup,
            cache,
            cmd_rx,
            callback,
            user_data,
            state,
            offline_index_path,
            offline_index,
            download_gate: Arc::new(Semaphore::new(1)),
            lyrics_volatile_dir,
            lyrics_persisted_dir,
        }
    }

    fn emit(&self, event: LibrespotEvent) {
        (self.callback)(&event, self.user_data.0);
    }

    fn app_data_worker(&self) -> AppDataWorker {
        AppDataWorker {
            offline_index: self.offline_index.clone(),
            lyrics_volatile_dir: self.lyrics_volatile_dir.clone(),
            lyrics_persisted_dir: self.lyrics_persisted_dir.clone(),
        }
    }

    fn try_load_offline_track(
        &self,
        player: &Player,
        track_uri: &str,
        play: bool,
        position_ms: u32,
    ) -> bool {
        let t_uri = match SpotifyUri::from_uri(track_uri) {
            Ok(uri) => uri,
            Err(_) => return false,
        };

        let entry = self
            .offline_index
            .lock()
            .ok()
            .and_then(|index| index.get(track_uri).cloned());

        let entry = match entry {
            Some(entry) => entry,
            None => return false,
        };

        let file_id = match parse_file_id_hex(&entry.file_id_hex) {
            Some(file_id) => file_id,
            None => {
                log::warn!(
                    "Offline index entry for <{}> has invalid file id <{}>",
                    track_uri,
                    entry.file_id_hex
                );
                return false;
            }
        };

        let format = match AudioFileFormat::try_from(entry.format) {
            Ok(format) => format,
            Err(_) => {
                log::warn!(
                    "Offline index entry for <{}> has invalid audio format {}",
                    track_uri,
                    entry.format
                );
                return false;
            }
        };

        log::info!(
            "Loading persisted offline track <{}> without Spotify Connect transport",
            track_uri
        );
        player.load_offline(
            t_uri,
            file_id,
            format,
            OfflineTrackMetadata {
                name: entry.name.clone(),
                artist: entry.artist.clone(),
                album: entry.album.clone(),
                cover_url: entry.cover_url.clone(),
                duration_ms: entry.duration_ms,
                is_explicit: entry.is_explicit,
            },
            play,
            position_ms,
        );
        true
    }

    pub async fn run(&mut self) {
        log::info!("Runner::run() starting");

        let mut session = Session::new(
            self.setup.session_config.clone(),
            Some((*self.cache).clone()),
        );

        Self::attach_audio_key_hooks(
            &session,
            self.setup.key_callback,
            self.setup.key_save_callback,
            self.user_data.0,
        );

        let mixer_builder = mixer::find(None).expect("No mixer found");
        let mixer = mixer_builder(self.setup.mixer_config.clone()).expect("Failed to create mixer");
        let backend = self.setup.audio_backend;
        let format = self.setup.audio_format;
        let device = self.setup.device_name.clone();

        let player = Player::new(
            self.setup.player_config.clone(),
            session.clone(),
            mixer.get_soft_volume(),
            move || {
                log::info!(
                    "Opening audio sink: format={:?}, device={:?}",
                    format,
                    device
                );

                let mut sink = (backend)(Some(device.clone()), format);

                if let Err(e) = sink.start() {
                    log::error!("Failed to start audio sink: {}", e);
                }

                sink
            },
        );

        let mut spirc: Option<Spirc> = None;
        let mut spirc_task: Option<Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = None;
        let mut discovery: Option<Discovery> = None;
        let mut player_rx = player.get_player_event_channel();

        if self.setup.enable_discovery {
            discovery = Discovery::builder(
                self.setup.session_config.device_id.clone(),
                self.setup.session_config.client_id.clone(),
            )
            .name(self.setup.device_name.clone())
            .device_type(self.setup.device_type)
            .port(self.setup.zeroconf_port)
            .launch()
            .ok();
        }

        let mut last_creds = self.setup.initial_creds.clone();
        let mut connecting = last_creds.is_some();
        let mut next_connect_attempt = Instant::now();
        let mut connect_backoff = Duration::from_secs(1);

        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
                        LibrespotCommand::Shutdown => {
                            log::info!("Runner shutdown requested");
                            player.stop();
                            if let Some(s) = spirc.take() {
                                let _ = s.shutdown();
                            }
                            break;
                        }
                        LibrespotCommand::Stop => player.stop(),
                        LibrespotCommand::Play => {
                            if let Some(ref s) = spirc {
                                let _ = s.activate().map_err(|e| log::error!("Failed to activate Spirc: {:?}", e));
                            }
                            player.play();
                        }
                        LibrespotCommand::Pause => player.pause(),
                        LibrespotCommand::Next => {
                            if let Some(ref s) = spirc {
                                let _ = s.activate().map_err(|e| log::warn!("Failed to activate Spirc before Next: {:?}", e));
                                let _ = s.next().map_err(|e| log::error!("Spirc Next failed: {:?}", e));
                            } else {
                                log::warn!("Next command ignored: No active Spirc session");
                            }
                        }
                        LibrespotCommand::Prev => {
                            if let Some(ref s) = spirc {
                                let _ = s.activate().map_err(|e| log::warn!("Failed to activate Spirc before Previous: {:?}", e));
                                let _ = s.prev().map_err(|e| log::error!("Spirc Prev failed: {:?}", e));
                            } else {
                                log::warn!("Previous command ignored: No active Spirc session");
                            }
                        }
                        LibrespotCommand::Seek(ms) => player.seek(ms),
                        LibrespotCommand::SetVolume(v) => {
                            mixer.set_volume(v);
                            self.state.volume.store(v, Ordering::Relaxed);
                            player.emit_volume_changed_event(v);
                        }
                        LibrespotCommand::SetShuffle(enabled) => {
                            if let Some(ref s) = spirc {
                                let _ = s.shuffle(enabled).map_err(|e| log::error!("Failed to set shuffle: {:?}", e));
                            }
                        }
                        LibrespotCommand::SetRepeatContext(enabled) => {
                            if let Some(ref s) = spirc {
                                let _ = s.repeat(enabled).map_err(|e| log::error!("Failed to set repeat context: {:?}", e));
                            }
                        }
                        LibrespotCommand::SetRepeatTrack(enabled) => {
                            if let Some(ref s) = spirc {
                                let _ = s.repeat_track(enabled).map_err(|e| log::error!("Failed to set repeat track: {:?}", e));
                            }
                        }
                        LibrespotCommand::Load { context_uri, start_from_uri, ordered_track_uris, play } => {
                            if let Ok(_ctx_uri) = SpotifyUri::from_uri(&context_uri) {
                                player.stop();
                                self.state.position_ms.store(0, Ordering::Release);
                                self.state
                                    .sync_write_pos
                                    .store(librespot_playback::audio_backend::get_write_pos(), Ordering::Release);

                                let track_to_load = start_from_uri
                                    .clone()
                                    .unwrap_or_else(|| context_uri.clone());
                                let loaded_direct_offline_track = start_from_uri.is_none()
                                    && track_to_load.starts_with("spotify:track:")
                                    && self.try_load_offline_track(&player, &track_to_load, play, 0);

                                if loaded_direct_offline_track {
                                    continue;
                                }

                                if let Some(ref s) = spirc {
                                    let _ = s.activate();

                                    let context_options = LoadContextOptions::Options(Options {
                                        shuffle: self.state.shuffle.load(Ordering::Acquire),
                                        repeat: self.state.repeat.load(Ordering::Acquire) == 1,
                                        repeat_track: self.state.repeat.load(Ordering::Acquire) == 2,
                                    });

                                    let options = LoadRequestOptions {
                                        start_playing: play,
                                        seek_to: 0,
                                        context_options: Some(context_options),
                                        playing_track: Some(PlayingTrack::Uri(track_to_load)),
                                    };

                                    let request = match ordered_track_uris {
                                        Some(tracks) if !tracks.is_empty() => {
                                            LoadRequest::from_tracks_with_context_uri(
                                                tracks,
                                                context_uri,
                                                options,
                                            )
                                        }
                                        _ => LoadRequest::from_context_uri(context_uri, options),
                                    };
                                    if let Err(e) = s.load(request) {
                                        log::error!("Spirc load failed: {:?}", e);
                                    }
                                } else {
                                    if let Ok(t_uri) = SpotifyUri::from_uri(&track_to_load) {
                                        if !self.try_load_offline_track(&player, &track_to_load, play, 0) {
                                            player.load(t_uri, play, 0);
                                        }
                                    }
                                }
                            }
                        }
                        LibrespotCommand::UpdateCredentials { username, auth_data } => {
                            log::info!("Updating credentials: User {}", username);
                            last_creds = Some(Credentials::with_password(username, auth_data));
                            connecting = true;
                            if let Some(s) = spirc.take() { let _ = s.shutdown(); }
                            spirc_task = None;
                        }
                        LibrespotCommand::SetTrackPersisted { track_uri, persisted, result_tx } => {
                            let worker = TrackPersistenceWorker {
                                session: session.clone(),
                                cache: self.cache.clone(),
                                offline_index: self.offline_index.clone(),
                                offline_index_path: self.offline_index_path.clone(),
                                download_gate: self.download_gate.clone(),
                                lyrics_volatile_dir: self.lyrics_volatile_dir.clone(),
                                lyrics_persisted_dir: self.lyrics_persisted_dir.clone(),
                                preferred_formats: self.preferred_formats(),
                            };

                            tokio::spawn(async move {
                                let result = worker.set_track_persisted_async(&track_uri, persisted).await;
                                if let Err(ref err) = result {
                                    log::error!(
                                        "Failed to set persisted={} for track <{}>: {:?}",
                                        persisted,
                                        track_uri,
                                        err
                                    );
                                }
                                let _ = result_tx.send(result.is_ok());
                            });
                        }
                        LibrespotCommand::GetAppData { kind, argument, result_tx } => {
                            let worker = self.app_data_worker();
                            let session = session.clone();

                            tokio::spawn(async move {
                                let start = Instant::now();
                                let result = worker
                                    .get_app_data(&session, kind, &argument)
                                    .await
                                    .map_err(|err| err.to_string());
                                let elapsed_ms = start.elapsed().as_millis();

                                if let Err(err) = &result {
                                    log::warn!(
                                        "librespot app data request kind={} failed after {}ms: {}",
                                        kind,
                                        elapsed_ms,
                                        err
                                    );
                                } else if elapsed_ms >= SLOW_APP_DATA_WARNING_MS {
                                    log::warn!(
                                        "slow librespot app data request kind={} elapsed_ms={}",
                                        kind,
                                        elapsed_ms
                                    );
                                }

                                let _ = result_tx.send(result);
                            });
                        }
                        LibrespotCommand::StartDiscovery => {
                            if discovery.is_none() {
                                log::info!("Starting discovery broadcast");
                                discovery = Discovery::builder(
                                    self.setup.session_config.device_id.clone(),
                                    self.setup.session_config.client_id.clone(),
                                )
                                .name(self.setup.device_name.clone())
                                .device_type(self.setup.device_type)
                                .port(self.setup.zeroconf_port)
                                .launch()
                                .ok();
                            }
                        }
                    }
                }

                Some(event) = player_rx.recv() => {
                    if let PlayerEvent::TrackChanged { ref audio_item, .. } = event {
                        let track_uri = audio_item.uri.clone();
                        let lyrics_session = session.clone();
                        let offline_index = self.offline_index.clone();
                        let volatile_dir = self.lyrics_volatile_dir.clone();
                        let persisted_dir = self.lyrics_persisted_dir.clone();
                        tokio::spawn(async move {
                            if let Err(err) = AppDataWorker::prefetch_lyrics_payload_for(
                                &lyrics_session,
                                &track_uri,
                                &offline_index,
                                &volatile_dir,
                                &persisted_dir,
                            ).await {
                                log::debug!("Unable to prefetch lyrics for {}: {:?}", track_uri, err);
                            }
                        });
                    }

                    self.handle_player_event(event);
                }

                creds_opt = async {
                    match discovery.as_mut() {
                        Some(d) => d.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(creds) = creds_opt {
                        last_creds = Some(creds);
                        connecting = true;
                    }
                }

                _ = sleep_until(next_connect_attempt), if connecting && last_creds.is_some() => {
                    if session.is_invalid() {
                        session = Session::new(
                            self.setup.session_config.clone(),
                            Some((*self.cache).clone()),
                        );
                        Self::attach_audio_key_hooks(
                            &session,
                            self.setup.key_callback,
                            self.setup.key_save_callback,
                            self.user_data.0,
                        );
                        player.set_session(session.clone());
                    }

                    if let Some(c) = last_creds.clone() {
                        if let Some(s) = spirc.take() { let _ = s.shutdown(); }
                        let spirc_res = Spirc::new(
                            self.setup.connect_config.clone(),
                            session.clone(),
                            c,
                            player.clone(),
                            mixer.clone()
                        ).await;

                        match spirc_res {
                            Ok((s, task)) => {
                                spirc = Some(s);
                                spirc_task = Some(Box::pin(task));
                                connect_backoff = Duration::from_secs(1);
                                next_connect_attempt = Instant::now();
                                self.emit(LibrespotEvent {
                                    event_type: EventType::SessionConnected,
                                    data: unsafe { std::mem::zeroed() }
                                });
                                connecting = false;
                            }
                            Err(e) => {
                                log::error!(
                                    "Spirc connection failed: {:?}. Retrying in {:?}.",
                                    e,
                                    connect_backoff
                                );
                                next_connect_attempt = Instant::now() + connect_backoff;
                                connect_backoff = std::cmp::min(
                                    connect_backoff.saturating_mul(2),
                                    Duration::from_secs(30),
                                );
                            }
                        }
                    }
                }

                _ = async {
                    if let Some(t) = spirc_task.as_mut() { t.await; }
                }, if spirc_task.is_some() => {
                    spirc_task = None;
                    spirc = None;
                    self.emit(LibrespotEvent {
                        event_type: EventType::SessionDisconnected,
                        data: unsafe { std::mem::zeroed() }
                    });

                    if last_creds.is_some() {
                        let retry_delay = connect_backoff;
                        connecting = true;
                        next_connect_attempt = Instant::now() + retry_delay;
                        connect_backoff = std::cmp::min(
                            connect_backoff.saturating_mul(2),
                            Duration::from_secs(30),
                        );
                        log::warn!(
                            "Spirc task ended unexpectedly. Reconnecting in {:?}.",
                            retry_delay
                        );
                    }
                }
            }
        }
    }

    fn attach_audio_key_hooks(
        session: &Session,
        key_callback: Option<librespot_core::LibrespotKeyCallback>,
        key_save_callback: Option<librespot_core::LibrespotKeySaveCallback>,
        user_data: *mut std::ffi::c_void,
    ) {
        session
            .audio_key()
            .set_ffi_hooks(key_callback, key_save_callback, user_data);
        log::info!("Attached frontend audio-key hooks to session");
    }

    fn handle_player_event(&self, event: PlayerEvent) {
        let mut data: EventData = unsafe { std::mem::zeroed() };
        let mut temp_strings: Vec<CString> = Vec::new();
        let event_audio_generation = match &event {
            PlayerEvent::Seeked {
                audio_generation, ..
            }
            | PlayerEvent::TrackChanged {
                audio_generation, ..
            } => Some(*audio_generation),
            _ => None,
        };
        data.audio_generation = event_audio_generation
            .unwrap_or_else(|| self.state.audio_generation.load(Ordering::Acquire));

        match event {
            PlayerEvent::Playing {
                play_request_id,
                ref track_id,
                position_ms,
            }
            | PlayerEvent::Paused {
                play_request_id,
                ref track_id,
                position_ms,
            }
            | PlayerEvent::Loading {
                play_request_id,
                ref track_id,
                position_ms,
            }
            | PlayerEvent::Seeked {
                play_request_id,
                ref track_id,
                position_ms,
                ..
            }
            | PlayerEvent::PositionCorrection {
                play_request_id,
                ref track_id,
                position_ms,
            }
            | PlayerEvent::PositionChanged {
                play_request_id,
                ref track_id,
                position_ms,
            } => {
                let wp = librespot_playback::audio_backend::get_write_pos();
                self.state.position_ms.store(position_ms, Ordering::Release);
                self.state.sync_write_pos.store(wp, Ordering::Release);

                let uri = CString::new(track_id.to_string()).unwrap_or_default();
                data.track_uri = uri.as_ptr();
                temp_strings.push(uri);

                data.play_request_id = play_request_id;
                data.position_ms = position_ms;

                if let Some(audio_generation) = event_audio_generation {
                    data.audio_generation = audio_generation;
                    self.state
                        .audio_generation
                        .store(audio_generation, Ordering::Release);
                }

                let event_type = match event {
                    PlayerEvent::Playing { .. } => {
                        self.state.is_playing.store(true, Ordering::Release);
                        data.is_playing = true;
                        EventType::PlaybackResumed
                    }
                    PlayerEvent::Paused { .. } => {
                        self.state.is_playing.store(false, Ordering::Release);
                        data.is_playing = false;
                        EventType::PlaybackPaused
                    }
                    PlayerEvent::Loading { .. } => EventType::PlaybackLoading,
                    PlayerEvent::Seeked { .. } => EventType::Seeked,
                    PlayerEvent::PositionCorrection { .. } => EventType::PositionCorrection,
                    _ => EventType::PositionChanged,
                };

                self.emit(LibrespotEvent { event_type, data });
            }

            PlayerEvent::TrackChanged {
                audio_item,
                play_request_id,
                audio_generation,
                was_preloaded,
            } => {
                let duration = audio_item.duration_ms as u32;
                self.state.duration_ms.store(duration, Ordering::Relaxed);
                self.state.position_ms.store(0, Ordering::Release);
                self.state.sync_write_pos.store(
                    librespot_playback::audio_backend::get_write_pos(),
                    Ordering::Release,
                );
                self.state
                    .audio_generation
                    .store(audio_generation, Ordering::Release);

                let artist_name = match &audio_item.unique_fields {
                    librespot_metadata::audio::UniqueFields::Track { artists, .. } => artists
                        .0
                        .first()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| "Unknown Artist".to_string()),
                    _ => "Unknown Artist".to_string(),
                };

                let album_name = match &audio_item.unique_fields {
                    librespot_metadata::audio::UniqueFields::Track { album, .. } => album.clone(),
                    _ => "Unknown Album".to_string(),
                };

                let internal = TrackMetadataInternal {
                    uri: CString::new(audio_item.uri.clone()).unwrap_or_default(),
                    name: CString::new(audio_item.name.clone()).unwrap_or_default(),
                    artist: CString::new(artist_name).unwrap_or_default(),
                    album: CString::new(album_name).unwrap_or_default(),
                    cover_url: audio_item
                        .covers
                        .first()
                        .map(|c| CString::new(c.url.clone()).unwrap_or_default())
                        .unwrap_or_default(),
                    duration_ms: duration,
                };

                let meta = TrackMetadata {
                    uri: internal.uri.as_ptr(),
                    name: internal.name.as_ptr(),
                    artist: internal.artist.as_ptr(),
                    album: internal.album.as_ptr(),
                    cover_url: internal.cover_url.as_ptr(),
                    duration_ms: duration,
                };

                if let Ok(mut current) = self.state.current_track.write() {
                    *current = Some(internal);
                }

                data.duration_ms = duration;
                data.track = ManuallyDrop::new(meta);
                data.play_request_id = play_request_id;
                data.audio_generation = audio_generation;
                data.was_preloaded = was_preloaded;

                self.emit(LibrespotEvent {
                    event_type: EventType::TrackChanged,
                    data,
                });
            }

            PlayerEvent::VolumeChanged { volume } => {
                self.state.volume.store(volume, Ordering::Relaxed);
                data.volume = volume;
                self.emit(LibrespotEvent {
                    event_type: EventType::VolumeChanged,
                    data,
                });
            }

            PlayerEvent::ShuffleChanged { shuffle } => {
                self.state.shuffle.store(shuffle, Ordering::Release);
                data.shuffle = shuffle;
                self.emit(LibrespotEvent {
                    event_type: EventType::ShuffleChanged,
                    data,
                });
            }

            PlayerEvent::RepeatChanged { context, track } => {
                let mode = if track {
                    2
                } else if context {
                    1
                } else {
                    0
                };
                self.state.repeat.store(mode, Ordering::Release);
                data.repeat_mode = mode;
                self.emit(LibrespotEvent {
                    event_type: EventType::RepeatChanged,
                    data,
                });
            }

            PlayerEvent::AutoPlayChanged { auto_play } => {
                data.auto_play = auto_play;
                self.emit(LibrespotEvent {
                    event_type: EventType::AutoPlayChanged,
                    data,
                });
            }

            PlayerEvent::FilterExplicitContentChanged { filter } => {
                data.filter_explicit = filter;
                self.emit(LibrespotEvent {
                    event_type: EventType::ExplicitFilterChanged,
                    data,
                });
            }

            PlayerEvent::SessionConnected { ref user_name, .. }
            | PlayerEvent::SessionDisconnected { ref user_name, .. } => {
                let user = CString::new(user_name.clone()).unwrap_or_default();
                data.session_user = user.as_ptr();
                temp_strings.push(user);

                let event_type = if matches!(event, PlayerEvent::SessionConnected { .. }) {
                    EventType::SessionConnected
                } else {
                    EventType::SessionDisconnected
                };

                self.emit(LibrespotEvent { event_type, data });
            }

            PlayerEvent::SessionClientChanged {
                ref client_name, ..
            } => {
                let name = CString::new(client_name.clone()).unwrap_or_default();
                data.client_name = name.as_ptr();
                temp_strings.push(name);

                self.emit(LibrespotEvent {
                    event_type: EventType::ClientChanged,
                    data,
                });
            }

            PlayerEvent::AddedToQueue { ref track_id }
            | PlayerEvent::Preloading { ref track_id } => {
                let uri = CString::new(track_id.to_string()).unwrap_or_default();
                data.track_uri = uri.as_ptr();
                temp_strings.push(uri);

                let event_type = if matches!(event, PlayerEvent::AddedToQueue { .. }) {
                    EventType::AddedToQueue
                } else {
                    EventType::Preloading
                };

                self.emit(LibrespotEvent { event_type, data });
            }

            PlayerEvent::TimeToPreloadNextTrack {
                play_request_id,
                ref track_id,
            } => {
                let uri = CString::new(track_id.to_string()).unwrap_or_default();
                data.track_uri = uri.as_ptr();
                data.play_request_id = play_request_id;
                temp_strings.push(uri);

                self.emit(LibrespotEvent {
                    event_type: EventType::TimeToPreloadNextTrack,
                    data,
                });
            }

            PlayerEvent::Stopped {
                play_request_id,
                ref track_id,
            }
            | PlayerEvent::EndOfTrack {
                play_request_id,
                ref track_id,
            }
            | PlayerEvent::Unavailable {
                play_request_id,
                ref track_id,
            } => {
                let uri = CString::new(track_id.to_string()).unwrap_or_default();
                data.track_uri = uri.as_ptr();
                data.play_request_id = play_request_id;
                temp_strings.push(uri);

                let event_type = match event {
                    PlayerEvent::Stopped { .. } => {
                        self.state.is_playing.store(false, Ordering::Release);
                        data.is_playing = false;
                        EventType::PlaybackStopped
                    }
                    PlayerEvent::EndOfTrack { .. } => EventType::EndOfTrack,
                    _ => EventType::PlaybackUnavailable,
                };

                self.emit(LibrespotEvent { event_type, data });
            }

            PlayerEvent::PlayRequestIdChanged { play_request_id } => {
                data.play_request_id = play_request_id;
                self.emit(LibrespotEvent {
                    event_type: EventType::PlayRequestIdChanged,
                    data,
                });
            }
        }
    }

    fn preferred_formats(&self) -> [AudioFileFormat; 7] {
        match self.setup.player_config.bitrate {
            librespot_playback::config::Bitrate::Bitrate96 => [
                AudioFileFormat::OGG_VORBIS_96,
                AudioFileFormat::MP3_96,
                AudioFileFormat::OGG_VORBIS_160,
                AudioFileFormat::MP3_160,
                AudioFileFormat::MP3_256,
                AudioFileFormat::OGG_VORBIS_320,
                AudioFileFormat::MP3_320,
            ],
            librespot_playback::config::Bitrate::Bitrate160 => [
                AudioFileFormat::OGG_VORBIS_160,
                AudioFileFormat::MP3_160,
                AudioFileFormat::OGG_VORBIS_96,
                AudioFileFormat::MP3_96,
                AudioFileFormat::MP3_256,
                AudioFileFormat::OGG_VORBIS_320,
                AudioFileFormat::MP3_320,
            ],
            librespot_playback::config::Bitrate::Bitrate320 => [
                AudioFileFormat::OGG_VORBIS_320,
                AudioFileFormat::MP3_320,
                AudioFileFormat::MP3_256,
                AudioFileFormat::OGG_VORBIS_160,
                AudioFileFormat::MP3_160,
                AudioFileFormat::OGG_VORBIS_96,
                AudioFileFormat::MP3_96,
            ],
        }
    }
}

impl AppDataWorker {
    async fn get_app_data(
        &self,
        session: &Session,
        kind: i32,
        argument: &str,
    ) -> Result<AppDataPayload, librespot_core::Error> {
        let payload = match kind {
            1 => AppDataPayload::Track(self.fetch_track_payload(session, argument).await?),
            2 => AppDataPayload::Album(self.fetch_album_payload(session, argument).await?),
            3 => AppDataPayload::Artist(self.fetch_artist_payload(session, argument).await?),
            4 => AppDataPayload::Playlist(self.fetch_playlist_payload(session, argument).await?),
            5 => AppDataPayload::UserProfile(
                self.fetch_user_profile_payload(session, argument).await?,
            ),
            6 => AppDataPayload::UserPlaylists(
                self.fetch_user_playlists_payload(session, argument).await?,
            ),
            7 => AppDataPayload::SavedTracks(
                self.fetch_saved_tracks_payload(session, argument).await?,
            ),
            8 => AppDataPayload::Search(self.fetch_search_payload(session, argument).await?),
            9 => AppDataPayload::FollowedArtists(
                self.fetch_followed_artists_payload(session, argument)
                    .await?,
            ),
            10 => AppDataPayload::Lyrics(self.fetch_lyrics_payload(session, argument).await?),
            11 => AppDataPayload::LyricsForImage(
                self.fetch_lyrics_for_image_payload(session, argument)
                    .await?,
            ),
            12 => AppDataPayload::Episode(self.fetch_episode_payload(session, argument).await?),
            13 => AppDataPayload::Show(self.fetch_show_payload(session, argument).await?),
            14 => AppDataPayload::PlaylistAnnotation(
                self.fetch_playlist_annotation_payload(session, argument)
                    .await?,
            ),
            15 => AppDataPayload::UserFollowersJson(
                self.fetch_user_followers_json_payload(session, argument)
                    .await?,
            ),
            16 => AppDataPayload::UserFollowingJson(
                self.fetch_user_following_json_payload(session, argument)
                    .await?,
            ),
            17 => AppDataPayload::RadioForTrackJson(
                self.fetch_radio_for_track_payload(session, argument)
                    .await?,
            ),
            18 => AppDataPayload::ApolloStationJson(
                self.fetch_apollo_station_payload(session, argument).await?,
            ),
            19 => {
                AppDataPayload::NextPageJson(self.fetch_next_page_payload(session, argument).await?)
            }
            20 => AppDataPayload::AudioStorageJson(
                self.fetch_audio_storage_payload(session, argument).await?,
            ),
            21 => AppDataPayload::AudioPreviewBinary(
                self.fetch_audio_preview_payload(session, argument).await?,
            ),
            22 => AppDataPayload::HeadFileBinary(
                self.fetch_head_file_payload(session, argument).await?,
            ),
            23 => AppDataPayload::ImageBinary(self.fetch_image_payload(session, argument).await?),
            24 => AppDataPayload::ContextJson(self.fetch_context_payload(session, argument).await?),
            25 => AppDataPayload::AutoplayContextJson(
                self.fetch_autoplay_context_payload(session, argument)
                    .await?,
            ),
            26 => {
                AppDataPayload::RootlistJson(self.fetch_rootlist_payload(session, argument).await?)
            }
            _ => {
                return Err(librespot_core::Error::invalid_argument(
                    "unknown app data request kind",
                ));
            }
        };
        Ok(payload)
    }

    async fn fetch_track_payload(
        &self,
        session: &Session,
        track_uri: &str,
    ) -> Result<TrackPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(track_uri)?;
        let track = LibrespotTrack::get(session, &uri).await?;
        let album = self.map_album_summary_payload(session, &track.album);
        Ok(self.map_track_payload(&track, album))
    }

    async fn fetch_album_payload(
        &self,
        session: &Session,
        album_uri: &str,
    ) -> Result<AlbumPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(album_uri)?;
        let album = LibrespotAlbum::get(session, &uri).await?;
        let mut tracks = Vec::new();

        for track_uri in album.tracks() {
            match LibrespotTrack::get(session, track_uri).await {
                Ok(track) => tracks.push(self.map_simple_track_payload(&track)),
                Err(err) => log::warn!(
                    "Skipping album track {} for {}: {}",
                    track_uri.to_uri(),
                    album_uri,
                    err
                ),
            }
        }

        Ok(AlbumPayload {
            id: album.id.to_id(),
            uri: album.id.to_uri(),
            name: album.name,
            album_type: format!("{:?}", album.album_type).to_lowercase(),
            images: self.map_images(session, &album.covers),
            artists: self.map_artists(&album.artists),
            release_date: self.format_date(&album.date),
            total_tracks: tracks.len() as i32,
            tracks,
        })
    }

    async fn fetch_artist_payload(
        &self,
        session: &Session,
        artist_uri: &str,
    ) -> Result<ArtistPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(artist_uri)?;
        let artist = LibrespotArtist::get(session, &uri).await?;

        let mut albums = Vec::new();
        for album_uri in artist.albums_current() {
            match self
                .fetch_album_summary_by_uri(session, &album_uri.to_uri())
                .await
            {
                Ok(album) => albums.push(album),
                Err(err) => log::warn!(
                    "Skipping artist album {} for {}: {}",
                    album_uri.to_uri(),
                    artist_uri,
                    err
                ),
            }
        }

        Ok(ArtistPayload {
            id: artist.id.to_id(),
            uri: artist.id.to_uri(),
            name: artist.name,
            images: self.map_images(session, &artist.portraits),
            albums,
        })
    }

    async fn fetch_playlist_payload(
        &self,
        session: &Session,
        playlist_uri: &str,
    ) -> Result<PlaylistPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(playlist_uri)?;
        let playlist = LibrespotPlaylist::get(session, &uri).await?;
        let mut items = Vec::new();

        for item in playlist.contents.items.iter() {
            if matches!(item.id, SpotifyUri::Track { .. }) {
                match LibrespotTrack::get(session, &item.id).await {
                    Ok(track) => items.push(PlaylistTrackPayload {
                        track: self.map_track_payload(
                            &track,
                            self.map_album_summary_payload(session, &track.album),
                        ),
                    }),
                    Err(err) => log::warn!(
                        "Skipping playlist track {} for {}: {}",
                        item.id.to_uri(),
                        playlist_uri,
                        err
                    ),
                }
            }
        }

        let image_url = playlist
            .attributes
            .picture_sizes
            .first()
            .map(|picture| picture.url.clone())
            .unwrap_or_default();

        Ok(PlaylistPayload {
            id: playlist.id.to_id(),
            uri: playlist.id.to_uri(),
            name: playlist.attributes.name,
            images: if image_url.is_empty() {
                Vec::new()
            } else {
                vec![ImagePayload {
                    url: image_url,
                    width: 0,
                    height: 0,
                }]
            },
            owner: OwnerPayload {
                id: match &playlist.id {
                    SpotifyUri::Playlist { user, .. } => user.clone().unwrap_or_default(),
                    _ => String::new(),
                },
                display_name: match &playlist.id {
                    SpotifyUri::Playlist { user, .. } => user.clone().unwrap_or_default(),
                    _ => String::new(),
                },
            },
            tracks: items,
        })
    }

    async fn fetch_playlist_summary_by_uri(
        &self,
        session: &Session,
        playlist_uri: &str,
    ) -> Result<PlaylistSummaryPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(playlist_uri)?;
        let playlist = LibrespotPlaylist::get(session, &uri).await?;
        let image_url = playlist
            .attributes
            .picture_sizes
            .first()
            .map(|picture| picture.url.clone())
            .unwrap_or_default();

        Ok(PlaylistSummaryPayload {
            id: playlist.id.to_id(),
            uri: playlist.id.to_uri(),
            name: playlist.attributes.name,
            images: if image_url.is_empty() {
                Vec::new()
            } else {
                vec![ImagePayload {
                    url: image_url,
                    width: 0,
                    height: 0,
                }]
            },
        })
    }

    async fn fetch_album_summary_by_uri(
        &self,
        session: &Session,
        album_uri: &str,
    ) -> Result<AlbumSummaryPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(album_uri)?;
        let album = LibrespotAlbum::get(session, &uri).await?;
        Ok(self.map_album_summary_payload(session, &album))
    }

    fn map_album_summary_payload(
        &self,
        session: &Session,
        album: &LibrespotAlbum,
    ) -> AlbumSummaryPayload {
        AlbumSummaryPayload {
            id: album.id.to_id(),
            uri: album.id.to_uri(),
            name: album.name.clone(),
            album_type: format!("{:?}", album.album_type).to_lowercase(),
            images: self.map_images(session, &album.covers),
            artists: self.map_artists(&album.artists),
        }
    }

    fn map_track_payload(
        &self,
        track: &LibrespotTrack,
        album: AlbumSummaryPayload,
    ) -> TrackPayload {
        TrackPayload {
            id: track.id.to_id(),
            uri: track.id.to_uri(),
            name: track.name.clone(),
            duration_ms: track.duration,
            disc_number: track.disc_number,
            track_number: track.number,
            artists: self.map_artists(&track.artists),
            album,
        }
    }

    fn map_simple_track_payload(&self, track: &LibrespotTrack) -> SimpleTrackPayload {
        SimpleTrackPayload {
            id: track.id.to_id(),
            uri: track.id.to_uri(),
            name: track.name.clone(),
            duration_ms: track.duration,
            disc_number: track.disc_number,
            track_number: track.number,
            artists: self.map_artists(&track.artists),
        }
    }

    fn map_artists(
        &self,
        artists: &librespot_metadata::artist::Artists,
    ) -> Vec<ArtistSummaryPayload> {
        artists
            .iter()
            .map(|artist| ArtistSummaryPayload {
                id: artist.id.to_id(),
                uri: artist.id.to_uri(),
                name: artist.name.clone(),
            })
            .collect()
    }

    fn map_images(
        &self,
        session: &Session,
        images: &librespot_metadata::image::Images,
    ) -> Vec<ImagePayload> {
        images
            .iter()
            .filter_map(|image| {
                self.image_url_for(session, &image.id)
                    .map(|url| ImagePayload {
                        url,
                        width: image.width,
                        height: image.height,
                    })
            })
            .collect()
    }

    fn image_url_for(&self, session: &Session, file_id: &FileId) -> Option<String> {
        session
            .get_user_attribute("image-url")
            .map(|template| template.replace("{file_id}", &file_id.to_base16()))
    }

    fn format_date(&self, date: &librespot_core::date::Date) -> String {
        format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )
    }

    async fn fetch_user_profile_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<UserProfilePayload, librespot_core::Error> {
        let username = self.resolve_username(session, argument)?;
        let bytes = session
            .spclient()
            .get_user_profile(&username, Some(20), Some(20))
            .await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;

        let display_name = Self::json_string(&value, &["name", "display_name"])
            .unwrap_or_else(|| username.clone());
        let image_url = Self::find_first_image_url(&value);

        Ok(UserProfilePayload {
            id: username.clone(),
            uri: format!("spotify:user:{username}"),
            display_name,
            email: String::new(),
            country: session.country(),
            images: if let Some(url) = image_url {
                vec![ImagePayload {
                    url,
                    width: 0,
                    height: 0,
                }]
            } else {
                Vec::new()
            },
        })
    }

    async fn fetch_user_playlists_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<PlaylistListPayload, librespot_core::Error> {
        let username = self.resolve_username(session, argument)?;
        match self
            .fetch_user_playlists_from_rootlist(session, &username)
            .await
        {
            Ok(payload) if !payload.items.is_empty() => Ok(payload),
            Ok(_) => {
                log::warn!(
                    "Rootlist returned no playlists for <{}>; falling back to profile playlist discovery",
                    username
                );
                self.fetch_user_playlists_from_profile(session, &username)
                    .await
            }
            Err(err) => {
                log::warn!(
                    "Rootlist playlist discovery failed for <{}>: {}. Falling back to profile playlist discovery",
                    username,
                    err
                );
                self.fetch_user_playlists_from_profile(session, &username)
                    .await
            }
        }
    }

    async fn fetch_user_playlists_from_rootlist(
        &self,
        session: &Session,
        username: &str,
    ) -> Result<PlaylistListPayload, librespot_core::Error> {
        let bytes = session
            .spclient()
            .get_rootlist_for_user(username, 0, Some(200))
            .await?;
        self.map_playlist_list_payload_from_bytes(session, &bytes, 200)
            .await
    }

    async fn fetch_user_playlists_from_profile(
        &self,
        session: &Session,
        username: &str,
    ) -> Result<PlaylistListPayload, librespot_core::Error> {
        let bytes = session
            .spclient()
            .get_user_profile(username, Some(200), None)
            .await?;
        self.map_playlist_list_payload_from_bytes(session, &bytes, 200)
            .await
    }

    async fn map_playlist_list_payload_from_bytes(
        &self,
        session: &Session,
        bytes: &[u8],
        limit: usize,
    ) -> Result<PlaylistListPayload, librespot_core::Error> {
        let uris = match SelectedListContent::parse_from_bytes(bytes) {
            Ok(rootlist) => {
                let mut uris = Vec::new();
                let mut seen = std::collections::HashSet::new();

                if let Some(contents) = rootlist.contents.as_ref() {
                    for item in &contents.items {
                        let uri = item.uri();
                        if uri.starts_with("spotify:playlist:") && seen.insert(uri.to_string()) {
                            uris.push(uri.to_string());
                        }
                    }
                }

                uris
            }
            Err(proto_err) => match serde_json::from_slice::<serde_json::Value>(bytes) {
                Ok(value) => {
                    let mut uris = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    Self::collect_json_spotify_uris_in_order(
                        &value,
                        "spotify:playlist:",
                        &mut uris,
                        &mut seen,
                    );
                    uris
                }
                Err(err) => {
                    let preview = String::from_utf8_lossy(bytes)
                        .chars()
                        .take(240)
                        .collect::<String>()
                        .replace('\r', "\\r")
                        .replace('\n', "\\n");
                    log::warn!(
                        "Failed to parse playlist list payload as protobuf ({}) or JSON ({}). Preview: {}",
                        proto_err,
                        err,
                        preview
                    );
                    Self::extract_spotify_uris_from_bytes(bytes, "spotify:playlist:")
                }
            },
        };

        if uris.is_empty() {
            return Err(librespot_core::Error::failed_precondition(
                "no playlist uris found in playlist list payload",
            ));
        }

        let mut playlists = Vec::new();
        for uri in uris.into_iter().take(limit) {
            match self.fetch_playlist_summary_by_uri(session, &uri).await {
                Ok(playlist) => playlists.push(playlist),
                Err(err) => log::warn!("Skipping user playlist {}: {}", uri, err),
            }
        }

        Ok(PlaylistListPayload { items: playlists })
    }

    async fn fetch_saved_tracks_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<TrackListPayload, librespot_core::Error> {
        let username = self.resolve_username(session, argument)?;
        let context = session
            .spclient()
            .get_context(&format!("spotify:user:{username}:collection"))
            .await?;

        let track_uris = self.collect_context_uris(&context, "track");
        let mut items = Vec::new();
        for uri in track_uris {
            match self.fetch_track_payload(session, &uri).await {
                Ok(payload) => items.push(payload),
                Err(err) => log::warn!("Skipping saved track {}: {}", uri, err),
            }
        }

        Ok(TrackListPayload { items })
    }

    async fn fetch_search_payload(
        &self,
        session: &Session,
        query: &str,
    ) -> Result<SearchPayload, librespot_core::Error> {
        let search_uri = format!("spotify:search:{}", query.replace(' ', "+"));
        let context = session.spclient().get_context(&search_uri).await?;

        let track_uris = self.collect_context_uris(&context, "track");
        let album_uris = self.collect_context_uris(&context, "album");
        let artist_uris = self.collect_context_uris(&context, "artist");
        let playlist_uris = self.collect_context_uris(&context, "playlist");

        let mut tracks = Vec::new();
        for uri in track_uris.into_iter().take(20) {
            tracks.push(self.fetch_track_payload(session, &uri).await?);
        }

        let mut albums = Vec::new();
        for uri in album_uris.into_iter().take(20) {
            albums.push(self.fetch_album_summary_by_uri(session, &uri).await?);
        }

        let mut artists = Vec::new();
        for uri in artist_uris.into_iter().take(20) {
            let artist = self.fetch_artist_payload(session, &uri).await?;
            artists.push(ArtistSummaryPayload {
                id: artist.id,
                uri: artist.uri,
                name: artist.name,
            });
        }

        let mut playlists = Vec::new();
        for uri in playlist_uris.into_iter().take(20) {
            playlists.push(self.fetch_playlist_summary_by_uri(session, &uri).await?);
        }

        Ok(SearchPayload {
            tracks,
            albums,
            artists,
            playlists,
        })
    }

    async fn fetch_followed_artists_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<ArtistListPayload, librespot_core::Error> {
        let username = self.resolve_username(session, argument)?;
        let bytes = session.spclient().get_user_following(&username).await?;
        let mut uris = match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => {
                let mut uris = Vec::new();
                Self::collect_json_spotify_uris(&value, "spotify:artist:", &mut uris);
                uris.sort();
                uris.dedup();
                uris
            }
            Err(err) => {
                let preview = String::from_utf8_lossy(bytes.as_ref())
                    .chars()
                    .take(240)
                    .collect::<String>()
                    .replace('\r', "\\r")
                    .replace('\n', "\\n");
                log::warn!(
                    "Failed to parse followed artists payload. Preview: {}",
                    preview
                );
                Self::extract_spotify_uris_from_bytes(bytes.as_ref(), "spotify:artist:")
            }
        };

        if uris.is_empty() {
            return Err(librespot_core::Error::failed_precondition(
                "no artist uris found in followed artists payload",
            ));
        }

        let mut items = Vec::new();
        for uri in uris.into_iter().take(50) {
            match self.fetch_artist_payload(session, &uri).await {
                Ok(artist) => items.push(ArtistSummaryPayload {
                    id: artist.id,
                    uri: artist.uri,
                    name: artist.name,
                }),
                Err(err) => log::warn!("Skipping followed artist {}: {}", uri, err),
            }
        }

        Ok(ArtistListPayload { items })
    }

    fn resolve_username(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<String, librespot_core::Error> {
        if !argument.is_empty() && argument != "current" {
            return Ok(argument.to_owned());
        }

        let username = session.username();
        if !username.is_empty() {
            return Ok(username);
        }

        Err(librespot_core::Error::failed_precondition(
            "no username available for current-user librespot request",
        ))
    }

    async fn fetch_lyrics_payload(
        &self,
        session: &Session,
        track_uri: &str,
    ) -> Result<LyricsPayload, librespot_core::Error> {
        if let Some(cached) = self.read_cached_lyrics(track_uri)? {
            log::info!("Returning cached lyrics for {}", track_uri);
            return Ok(cached);
        }

        let track_id = Self::parse_track_id(track_uri)?;
        let lyrics = LibrespotLyrics::get(session, &track_id).await?;
        let payload = Self::map_lyrics_payload(&lyrics);
        self.write_cached_lyrics(track_uri, &payload)?;
        Ok(payload)
    }

    async fn fetch_lyrics_for_image_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<LyricsPayload, librespot_core::Error> {
        let request: LyricsForImageRequest = serde_json::from_str(argument)
            .map_err(|err| librespot_core::Error::invalid_argument(err.to_string()))?;
        if let Some(cached) = self.read_cached_lyrics(&request.track_uri)? {
            log::info!(
                "Returning cached lyrics for {} (image variant request)",
                request.track_uri
            );
            return Ok(cached);
        }
        let track_id = Self::parse_track_id(&request.track_uri)?;
        let image_id = Self::parse_file_id(&request.image_id_hex)?;
        let lyrics = LibrespotLyrics::get_for_image(session, &track_id, &image_id).await?;
        let payload = Self::map_lyrics_payload(&lyrics);
        self.write_cached_lyrics(&request.track_uri, &payload)?;
        Ok(payload)
    }

    async fn fetch_episode_payload(
        &self,
        session: &Session,
        episode_uri: &str,
    ) -> Result<EpisodePayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(episode_uri)?;
        let episode = LibrespotEpisode::get(session, &uri).await?;
        Ok(self.map_episode_payload(&episode))
    }

    async fn fetch_show_payload(
        &self,
        session: &Session,
        show_uri: &str,
    ) -> Result<ShowPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(show_uri)?;
        let show = LibrespotShow::get(session, &uri).await?;
        Ok(self.map_show_payload(&show))
    }

    async fn fetch_playlist_annotation_payload(
        &self,
        session: &Session,
        playlist_uri: &str,
    ) -> Result<PlaylistAnnotationPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(playlist_uri)?;
        let annotation = LibrespotPlaylistAnnotation::get(session, &uri).await?;
        Ok(self.map_playlist_annotation_payload(&annotation))
    }

    async fn fetch_user_followers_json_payload(
        &self,
        session: &Session,
        username: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let bytes = session.spclient().get_user_followers(username).await?;
        Self::json_payload_from_bytes(bytes)
    }

    async fn fetch_user_following_json_payload(
        &self,
        session: &Session,
        username: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let bytes = session.spclient().get_user_following(username).await?;
        Self::json_payload_from_bytes(bytes)
    }

    async fn fetch_radio_for_track_payload(
        &self,
        session: &Session,
        track_uri: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let uri = SpotifyUri::from_uri(track_uri)?;
        let bytes = session.spclient().get_radio_for_track(&uri).await?;
        Self::json_payload_from_bytes(bytes)
    }

    async fn fetch_apollo_station_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let request: ApolloStationRequest = serde_json::from_str(argument)
            .map_err(|err| librespot_core::Error::invalid_argument(err.to_string()))?;
        let previous_tracks = request
            .previous_track_uris
            .iter()
            .map(|uri| Self::parse_track_id(uri))
            .collect::<Result<Vec<_>, _>>()?;
        let bytes = session
            .spclient()
            .get_apollo_station(
                &request.scope,
                &request.context_uri,
                request.count,
                previous_tracks,
                request.autoplay.unwrap_or(false),
            )
            .await?;
        Self::json_payload_from_bytes(bytes)
    }

    async fn fetch_next_page_payload(
        &self,
        session: &Session,
        next_page_uri: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let bytes = session.spclient().get_next_page(next_page_uri).await?;
        Self::json_payload_from_bytes(bytes)
    }

    async fn fetch_audio_storage_payload(
        &self,
        session: &Session,
        file_id_hex: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let file_id = Self::parse_file_id(file_id_hex)?;
        let bytes = session.spclient().get_audio_storage(&file_id).await?;
        Self::json_payload_from_bytes(bytes)
    }

    async fn fetch_audio_preview_payload(
        &self,
        session: &Session,
        preview_id_hex: &str,
    ) -> Result<BinaryPayload, librespot_core::Error> {
        let file_id = Self::parse_file_id(preview_id_hex)?;
        let bytes = session.spclient().get_audio_preview(&file_id).await?;
        Ok(Self::binary_payload(
            preview_id_hex,
            "audio/mpeg",
            bytes.as_ref(),
        ))
    }

    async fn fetch_head_file_payload(
        &self,
        session: &Session,
        file_id_hex: &str,
    ) -> Result<BinaryPayload, librespot_core::Error> {
        let file_id = Self::parse_file_id(file_id_hex)?;
        let bytes = session.spclient().get_head_file(&file_id).await?;
        Ok(Self::binary_payload(
            file_id_hex,
            "application/octet-stream",
            bytes.as_ref(),
        ))
    }

    async fn fetch_image_payload(
        &self,
        session: &Session,
        image_id_hex: &str,
    ) -> Result<BinaryPayload, librespot_core::Error> {
        let file_id = Self::parse_file_id(image_id_hex)?;
        let bytes = session.spclient().get_image(&file_id).await?;
        Ok(Self::binary_payload(
            image_id_hex,
            "image/jpeg",
            bytes.as_ref(),
        ))
    }

    async fn fetch_context_payload(
        &self,
        session: &Session,
        context_uri: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let context = session.spclient().get_context(context_uri).await?;
        Self::json_payload_from_context(&context)
    }

    async fn fetch_autoplay_context_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let request: AutoplayContextRequestPayload = serde_json::from_str(argument)
            .map_err(|err| librespot_core::Error::invalid_argument(err.to_string()))?;
        let request_json = serde_json::to_string(&request)
            .map_err(|err| librespot_core::Error::invalid_argument(err.to_string()))?;
        let proto_request =
            protobuf_json_mapping::parse_from_str::<AutoplayContextRequest>(&request_json)
                .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        let context = session
            .spclient()
            .get_autoplay_context(&proto_request)
            .await?;
        Self::json_payload_from_context(&context)
    }

    async fn fetch_rootlist_payload(
        &self,
        session: &Session,
        argument: &str,
    ) -> Result<JsonPayload, librespot_core::Error> {
        let request = if argument.trim().starts_with('{') {
            serde_json::from_str::<RootlistRequest>(argument)
                .map_err(|err| librespot_core::Error::invalid_argument(err.to_string()))?
        } else {
            RootlistRequest {
                from: argument.trim().parse::<usize>().unwrap_or(0),
                length: None,
            }
        };

        let bytes = session
            .spclient()
            .get_rootlist(request.from, request.length)
            .await?;
        Self::json_payload_from_bytes(bytes)
    }

    fn collect_context_uris(&self, context: &Context, kind: &str) -> Vec<String> {
        let mut uris = Vec::new();

        if let Some(uri) = context.uri.as_ref() {
            if uri.starts_with(&format!("spotify:{kind}:")) {
                uris.push(uri.clone());
            }
        }

        for page in &context.pages {
            if let Some(page_url) = page.page_url.as_ref() {
                let uri = self.page_url_to_uri(page_url);
                if uri.starts_with(&format!("spotify:{kind}:")) {
                    uris.push(uri);
                }
            }

            for track in &page.tracks {
                if let Some(uri) = track.uri.as_ref() {
                    if uri.starts_with(&format!("spotify:{kind}:")) {
                        uris.push(uri.clone());
                    }
                }
            }
        }

        uris.sort();
        uris.dedup();
        uris
    }

    fn page_url_to_uri(&self, page_url: &str) -> String {
        let split = if let Some(rest) = page_url.strip_prefix("hm://") {
            rest.split('/')
        } else {
            page_url.split('/')
        };

        split
            .skip_while(|s| s != &"spotify")
            .take(3)
            .collect::<Vec<&str>>()
            .join(":")
    }

    fn json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
        for key in keys {
            if let Some(candidate) = value.get(*key) {
                if let Some(text) = candidate.as_str() {
                    return Some(text.to_owned());
                }
                if let Some(inner) = candidate.get("value").and_then(|it| it.as_str()) {
                    return Some(inner.to_owned());
                }
            }
        }
        None
    }

    fn find_first_image_url(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(images) = map.get("images").and_then(|it| it.as_array()) {
                    for image in images {
                        if let Some(url) = image.get("url").and_then(|it| it.as_str()) {
                            return Some(url.to_owned());
                        }
                    }
                }

                for child in map.values() {
                    if let Some(url) = Self::find_first_image_url(child) {
                        return Some(url);
                    }
                }
                None
            }
            serde_json::Value::Array(items) => items.iter().find_map(Self::find_first_image_url),
            _ => None,
        }
    }

    fn collect_json_spotify_uris(
        value: &serde_json::Value,
        prefix: &str,
        output: &mut Vec<String>,
    ) {
        match value {
            serde_json::Value::String(text) => {
                if text.starts_with(prefix) {
                    output.push(text.clone());
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    Self::collect_json_spotify_uris(item, prefix, output);
                }
            }
            serde_json::Value::Object(map) => {
                for child in map.values() {
                    Self::collect_json_spotify_uris(child, prefix, output);
                }
            }
            _ => {}
        }
    }

    fn collect_json_spotify_uris_in_order(
        value: &serde_json::Value,
        prefix: &str,
        output: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        match value {
            serde_json::Value::String(text) => {
                if text.starts_with(prefix) && seen.insert(text.clone()) {
                    output.push(text.clone());
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    Self::collect_json_spotify_uris_in_order(item, prefix, output, seen);
                }
            }
            serde_json::Value::Object(map) => {
                for child in map.values() {
                    Self::collect_json_spotify_uris_in_order(child, prefix, output, seen);
                }
            }
            _ => {}
        }
    }

    fn extract_spotify_uris_from_bytes(bytes: &[u8], prefix: &str) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        let mut output = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut search_start = 0usize;

        while let Some(found) = text[search_start..].find(prefix) {
            let start = search_start + found;
            let mut end = start;

            for ch in text[start..].chars() {
                let allowed = ch.is_ascii_alphanumeric() || ch == ':' || ch == '_';
                if !allowed {
                    break;
                }
                end += ch.len_utf8();
            }

            if end > start {
                let candidate = &text[start..end];
                if seen.insert(candidate.to_string()) {
                    output.push(candidate.to_string());
                }
            }

            search_start = start.saturating_add(prefix.len());
            if search_start >= text.len() {
                break;
            }
        }

        output
    }

    fn parse_file_id(file_id_hex: &str) -> Result<FileId, librespot_core::Error> {
        let bytes = hex::decode(file_id_hex)
            .map_err(|err| librespot_core::Error::invalid_argument(err.to_string()))?;
        Ok(FileId::from_raw(&bytes))
    }

    fn parse_track_id(track_uri: &str) -> Result<SpotifyId, librespot_core::Error> {
        match SpotifyUri::from_uri(track_uri)? {
            SpotifyUri::Track { id } => Ok(id),
            _ => Err(librespot_core::Error::invalid_argument("track_uri")),
        }
    }

    fn json_payload_from_bytes(bytes: bytes::Bytes) -> Result<JsonPayload, librespot_core::Error> {
        let value = serde_json::from_slice(bytes.as_ref()).map_err(|err| {
            let preview = String::from_utf8_lossy(bytes.as_ref())
                .chars()
                .take(240)
                .collect::<String>()
                .replace('\r', "\\r")
                .replace('\n', "\\n");
            log::warn!("Failed to parse JSON payload. Preview: {}", preview);
            librespot_core::Error::failed_precondition(err.to_string())
        })?;
        Ok(JsonPayload { value })
    }

    fn json_payload_from_context(context: &Context) -> Result<JsonPayload, librespot_core::Error> {
        let json = protobuf_json_mapping::print_to_string(context)
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        let value = serde_json::from_str(&json)
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        Ok(JsonPayload { value })
    }

    fn binary_payload(file_id_hex: &str, content_type: &str, bytes: &[u8]) -> BinaryPayload {
        BinaryPayload {
            file_id_hex: file_id_hex.to_owned(),
            content_type: content_type.to_owned(),
            byte_length: bytes.len(),
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    fn map_lyrics_payload(lyrics: &LibrespotLyrics) -> LyricsPayload {
        LyricsPayload {
            provider: lyrics.lyrics.provider.clone(),
            provider_display_name: lyrics.lyrics.provider_display_name.clone(),
            language: lyrics.lyrics.language.clone(),
            sync_type: format!("{:?}", lyrics.lyrics.sync_type),
            has_vocal_removal: lyrics.has_vocal_removal,
            is_dense_typeface: lyrics.lyrics.is_dense_typeface,
            is_rtl_language: lyrics.lyrics.is_rtl_language,
            sync_lyrics_uri: lyrics.lyrics.sync_lyrics_uri.clone(),
            lines: lyrics
                .lyrics
                .lines
                .iter()
                .map(|line| LyricsLinePayload {
                    start_time_ms: line.start_time_ms.clone(),
                    end_time_ms: line.end_time_ms.clone(),
                    words: line.words.clone(),
                })
                .collect(),
            colors: LyricsColorsPayload {
                background: lyrics.colors.background,
                text: lyrics.colors.text,
                highlight_text: lyrics.colors.highlight_text,
            },
        }
    }

    fn is_track_persisted_in(offline_index: &SharedOfflineIndex, track_uri: &str) -> bool {
        offline_index
            .lock()
            .map(|index| index.contains_key(track_uri))
            .unwrap_or(false)
    }

    fn lyrics_path_for(
        track_uri: &str,
        persisted: bool,
        volatile_dir: &PathBuf,
        persisted_dir: &PathBuf,
    ) -> PathBuf {
        let root = if persisted {
            persisted_dir
        } else {
            volatile_dir
        };
        root.join(format!(
            "{}.lyrics.json",
            Self::sha1_hex(track_uri.as_bytes())
        ))
    }

    fn read_cached_lyrics(
        &self,
        track_uri: &str,
    ) -> Result<Option<LyricsPayload>, librespot_core::Error> {
        Self::read_cached_lyrics_from(
            track_uri,
            &self.offline_index,
            &self.lyrics_volatile_dir,
            &self.lyrics_persisted_dir,
        )
    }

    fn read_cached_lyrics_from(
        track_uri: &str,
        offline_index: &SharedOfflineIndex,
        volatile_dir: &PathBuf,
        persisted_dir: &PathBuf,
    ) -> Result<Option<LyricsPayload>, librespot_core::Error> {
        let preferred_persisted = Self::is_track_persisted_in(offline_index, track_uri);
        let primary =
            Self::lyrics_path_for(track_uri, preferred_persisted, volatile_dir, persisted_dir);
        let secondary =
            Self::lyrics_path_for(track_uri, !preferred_persisted, volatile_dir, persisted_dir);

        for path in [primary, secondary] {
            match fs::read_to_string(&path) {
                Ok(json) => {
                    let payload = serde_json::from_str::<LyricsPayload>(&json).map_err(|err| {
                        librespot_core::Error::failed_precondition(err.to_string())
                    })?;
                    return Ok(Some(payload));
                }
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(librespot_core::Error::failed_precondition(err.to_string()));
                }
            }
        }

        Ok(None)
    }

    fn write_cached_lyrics(
        &self,
        track_uri: &str,
        payload: &LyricsPayload,
    ) -> Result<(), librespot_core::Error> {
        Self::write_cached_lyrics_to(
            track_uri,
            payload,
            &self.offline_index,
            &self.lyrics_volatile_dir,
            &self.lyrics_persisted_dir,
        )
    }

    fn write_cached_lyrics_to(
        track_uri: &str,
        payload: &LyricsPayload,
        offline_index: &SharedOfflineIndex,
        volatile_dir: &PathBuf,
        persisted_dir: &PathBuf,
    ) -> Result<(), librespot_core::Error> {
        let persisted = Self::is_track_persisted_in(offline_index, track_uri);
        let target = Self::lyrics_path_for(track_uri, persisted, volatile_dir, persisted_dir);
        let alternate = Self::lyrics_path_for(track_uri, !persisted, volatile_dir, persisted_dir);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        }
        let json = serde_json::to_string_pretty(payload)
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        fs::write(&target, json)
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        let _ = fs::remove_file(alternate);
        Ok(())
    }

    fn move_cached_lyrics_in(
        track_uri: &str,
        persisted: bool,
        volatile_dir: &PathBuf,
        persisted_dir: &PathBuf,
    ) -> Result<(), librespot_core::Error> {
        let source = Self::lyrics_path_for(track_uri, !persisted, volatile_dir, persisted_dir);
        let target = Self::lyrics_path_for(track_uri, persisted, volatile_dir, persisted_dir);
        if !source.exists() {
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        }
        fs::rename(&source, &target)
            .or_else(|_| {
                fs::copy(&source, &target)
                    .and_then(|_| fs::remove_file(&source))
                    .map(|_| ())
            })
            .map_err(|err| librespot_core::Error::failed_precondition(err.to_string()))?;
        Ok(())
    }

    fn remove_cached_lyrics_from(track_uri: &str, volatile_dir: &PathBuf, persisted_dir: &PathBuf) {
        let _ = fs::remove_file(Self::lyrics_path_for(
            track_uri,
            false,
            volatile_dir,
            persisted_dir,
        ));
        let _ = fs::remove_file(Self::lyrics_path_for(
            track_uri,
            true,
            volatile_dir,
            persisted_dir,
        ));
    }

    async fn prefetch_lyrics_payload_for(
        session: &Session,
        track_uri: &str,
        offline_index: &SharedOfflineIndex,
        volatile_dir: &PathBuf,
        persisted_dir: &PathBuf,
    ) -> Result<(), librespot_core::Error> {
        if Self::read_cached_lyrics_from(track_uri, offline_index, volatile_dir, persisted_dir)?
            .is_some()
        {
            return Ok(());
        }

        let track_id = Self::parse_track_id(track_uri)?;
        let lyrics = LibrespotLyrics::get(session, &track_id).await?;
        let payload = Self::map_lyrics_payload(&lyrics);
        Self::write_cached_lyrics_to(
            track_uri,
            &payload,
            offline_index,
            volatile_dir,
            persisted_dir,
        )?;
        Ok(())
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        use sha1::Digest;
        let digest = sha1::Sha1::digest(bytes);
        hex::encode(digest)
    }

    fn map_episode_payload(&self, episode: &LibrespotEpisode) -> EpisodePayload {
        EpisodePayload {
            id: episode.id.to_id(),
            uri: episode.id.to_uri(),
            name: episode.name.clone(),
            description: episode.description.clone(),
            duration_ms: episode.duration,
            number: episode.number,
            publish_time: self.format_date(&episode.publish_time),
            language: episode.language.clone(),
            is_explicit: episode.is_explicit,
            show_name: episode.show_name.clone(),
            covers: self.map_images_from_images(&episode.covers),
            freeze_frames: self.map_images_from_images(&episode.freeze_frames),
            audio_files: self.map_audio_files(&episode.audio),
            audio_previews: self.map_audio_files(&episode.audio_previews),
            video_files: self.map_file_ids(&episode.videos),
            video_previews: self.map_file_ids(&episode.video_previews),
            restrictions: self.map_restrictions(&episode.restrictions),
            availability: self.map_availabilities(&episode.availability),
            keywords: episode.keywords.clone(),
            allow_background_playback: episode.allow_background_playback,
            external_url: episode.external_url.clone(),
            episode_type: format!("{:?}", episode.episode_type),
            has_music_and_talk: episode.has_music_and_talk,
            content_ratings: self.map_content_ratings(&episode.content_rating),
            is_audiobook_chapter: episode.is_audiobook_chapter,
        }
    }

    fn map_show_payload(&self, show: &LibrespotShow) -> ShowPayload {
        ShowPayload {
            id: show.id.to_id(),
            uri: show.id.to_uri(),
            name: show.name.clone(),
            description: show.description.clone(),
            publisher: show.publisher.clone(),
            language: show.language.clone(),
            is_explicit: show.is_explicit,
            covers: self.map_images_from_images(&show.covers),
            episode_uris: show.episodes.iter().map(SpotifyUri::to_uri).collect(),
            copyrights: self.map_copyrights(&show.copyrights),
            restrictions: self.map_restrictions(&show.restrictions),
            keywords: show.keywords.clone(),
            media_type: format!("{:?}", show.media_type),
            consumption_order: format!("{:?}", show.consumption_order),
            availability: self.map_availabilities(&show.availability),
            trailer_uri: show.trailer_uri.as_ref().map(SpotifyUri::to_uri),
            has_music_and_talk: show.has_music_and_talk,
            is_audiobook: show.is_audiobook,
        }
    }

    fn map_playlist_annotation_payload(
        &self,
        annotation: &LibrespotPlaylistAnnotation,
    ) -> PlaylistAnnotationPayload {
        PlaylistAnnotationPayload {
            description: annotation.description.clone(),
            picture: annotation.picture.clone(),
            transcoded_pictures: annotation
                .transcoded_pictures
                .iter()
                .map(|picture| TranscodedPicturePayload {
                    target_name: picture.target_name.clone(),
                    uri: picture.uri.to_uri(),
                })
                .collect(),
            has_abuse_reporting: annotation.has_abuse_reporting,
            abuse_report_state: format!("{:?}", annotation.abuse_report_state),
        }
    }

    fn map_images_from_images(
        &self,
        images: &librespot_metadata::image::Images,
    ) -> Vec<ImageRefPayload> {
        images
            .iter()
            .map(|image| ImageRefPayload {
                file_id_hex: image.id.to_base16(),
                size: format!("{:?}", image.size),
                width: image.width,
                height: image.height,
            })
            .collect()
    }

    fn map_audio_files(
        &self,
        files: &librespot_metadata::audio::AudioFiles,
    ) -> Vec<AudioFilePayload> {
        files
            .iter()
            .map(|(format, file_id)| AudioFilePayload {
                format: format!("{:?}", format),
                file_id_hex: file_id.to_base16(),
                mime_type: librespot_metadata::audio::AudioFiles::mime_type(*format)
                    .map(str::to_owned),
            })
            .collect()
    }

    fn map_file_ids<T>(&self, files: &T) -> Vec<String>
    where
        T: std::ops::Deref<Target = Vec<FileId>>,
    {
        files.iter().map(FileId::to_base16).collect()
    }

    fn map_restrictions(
        &self,
        restrictions: &librespot_metadata::restriction::Restrictions,
    ) -> Vec<RestrictionPayload> {
        restrictions
            .iter()
            .map(|restriction| RestrictionPayload {
                restriction_type: format!("{:?}", restriction.restriction_type),
                catalogues: restriction
                    .catalogues
                    .iter()
                    .map(|catalogue| format!("{:?}", catalogue))
                    .collect(),
                catalogue_strs: restriction.catalogue_strs.clone(),
                countries_allowed: restriction.countries_allowed.clone(),
                countries_forbidden: restriction.countries_forbidden.clone(),
            })
            .collect()
    }

    fn map_availabilities(
        &self,
        availability: &librespot_metadata::availability::Availabilities,
    ) -> Vec<AvailabilityPayload> {
        availability
            .iter()
            .map(|entry| AvailabilityPayload {
                catalogue_strs: entry.catalogue_strs.clone(),
                start: self.format_date(&entry.start),
            })
            .collect()
    }

    fn map_content_ratings(
        &self,
        ratings: &librespot_metadata::content_rating::ContentRatings,
    ) -> Vec<ContentRatingPayload> {
        ratings
            .iter()
            .map(|rating| ContentRatingPayload {
                country: rating.country.clone(),
                tags: rating.tags.clone(),
            })
            .collect()
    }

    fn map_copyrights(
        &self,
        copyrights: &librespot_metadata::copyright::Copyrights,
    ) -> Vec<CopyrightPayload> {
        copyrights
            .iter()
            .map(|copyright| CopyrightPayload {
                copyright_type: format!("{:?}", copyright.copyright_type),
                text: copyright.text.clone(),
            })
            .collect()
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
pub(crate) enum AppDataPayload {
    Track(TrackPayload),
    Album(AlbumPayload),
    Artist(ArtistPayload),
    Playlist(PlaylistPayload),
    UserProfile(UserProfilePayload),
    UserPlaylists(PlaylistListPayload),
    SavedTracks(TrackListPayload),
    Search(SearchPayload),
    FollowedArtists(ArtistListPayload),
    Lyrics(LyricsPayload),
    LyricsForImage(LyricsPayload),
    Episode(EpisodePayload),
    Show(ShowPayload),
    PlaylistAnnotation(PlaylistAnnotationPayload),
    UserFollowersJson(JsonPayload),
    UserFollowingJson(JsonPayload),
    RadioForTrackJson(JsonPayload),
    ApolloStationJson(JsonPayload),
    NextPageJson(JsonPayload),
    AudioStorageJson(JsonPayload),
    AudioPreviewBinary(BinaryPayload),
    HeadFileBinary(BinaryPayload),
    ImageBinary(BinaryPayload),
    ContextJson(JsonPayload),
    AutoplayContextJson(JsonPayload),
    RootlistJson(JsonPayload),
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImagePayload {
    url: String,
    width: i32,
    height: i32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtistSummaryPayload {
    id: String,
    uri: String,
    name: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AlbumSummaryPayload {
    id: String,
    uri: String,
    name: String,
    album_type: String,
    images: Vec<ImagePayload>,
    artists: Vec<ArtistSummaryPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SimpleTrackPayload {
    id: String,
    uri: String,
    name: String,
    duration_ms: i32,
    disc_number: i32,
    track_number: i32,
    artists: Vec<ArtistSummaryPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackPayload {
    id: String,
    uri: String,
    name: String,
    duration_ms: i32,
    disc_number: i32,
    track_number: i32,
    artists: Vec<ArtistSummaryPayload>,
    album: AlbumSummaryPayload,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AlbumPayload {
    id: String,
    uri: String,
    name: String,
    album_type: String,
    images: Vec<ImagePayload>,
    artists: Vec<ArtistSummaryPayload>,
    release_date: String,
    total_tracks: i32,
    tracks: Vec<SimpleTrackPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtistPayload {
    id: String,
    uri: String,
    name: String,
    images: Vec<ImagePayload>,
    albums: Vec<AlbumSummaryPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistTrackPayload {
    track: TrackPayload,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerPayload {
    id: String,
    display_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistPayload {
    id: String,
    uri: String,
    name: String,
    images: Vec<ImagePayload>,
    owner: OwnerPayload,
    tracks: Vec<PlaylistTrackPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserProfilePayload {
    id: String,
    uri: String,
    display_name: String,
    email: String,
    country: String,
    images: Vec<ImagePayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistSummaryPayload {
    id: String,
    uri: String,
    name: String,
    images: Vec<ImagePayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaylistListPayload {
    items: Vec<PlaylistSummaryPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackListPayload {
    items: Vec<TrackPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtistListPayload {
    items: Vec<ArtistSummaryPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchPayload {
    tracks: Vec<TrackPayload>,
    albums: Vec<AlbumSummaryPayload>,
    artists: Vec<ArtistSummaryPayload>,
    playlists: Vec<PlaylistSummaryPayload>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonPayload {
    value: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BinaryPayload {
    file_id_hex: String,
    content_type: String,
    byte_length: usize,
    base64: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsPayload {
    provider: String,
    provider_display_name: String,
    language: String,
    sync_type: String,
    has_vocal_removal: bool,
    is_dense_typeface: bool,
    is_rtl_language: bool,
    sync_lyrics_uri: String,
    lines: Vec<LyricsLinePayload>,
    colors: LyricsColorsPayload,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsLinePayload {
    start_time_ms: String,
    end_time_ms: String,
    words: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsColorsPayload {
    background: i32,
    text: i32,
    highlight_text: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EpisodePayload {
    id: String,
    uri: String,
    name: String,
    description: String,
    duration_ms: i32,
    number: i32,
    publish_time: String,
    language: String,
    is_explicit: bool,
    show_name: String,
    covers: Vec<ImageRefPayload>,
    freeze_frames: Vec<ImageRefPayload>,
    audio_files: Vec<AudioFilePayload>,
    audio_previews: Vec<AudioFilePayload>,
    video_files: Vec<String>,
    video_previews: Vec<String>,
    restrictions: Vec<RestrictionPayload>,
    availability: Vec<AvailabilityPayload>,
    keywords: Vec<String>,
    allow_background_playback: bool,
    external_url: String,
    episode_type: String,
    has_music_and_talk: bool,
    content_ratings: Vec<ContentRatingPayload>,
    is_audiobook_chapter: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShowPayload {
    id: String,
    uri: String,
    name: String,
    description: String,
    publisher: String,
    language: String,
    is_explicit: bool,
    covers: Vec<ImageRefPayload>,
    episode_uris: Vec<String>,
    copyrights: Vec<CopyrightPayload>,
    restrictions: Vec<RestrictionPayload>,
    keywords: Vec<String>,
    media_type: String,
    consumption_order: String,
    availability: Vec<AvailabilityPayload>,
    trailer_uri: Option<String>,
    has_music_and_talk: bool,
    is_audiobook: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistAnnotationPayload {
    description: String,
    picture: String,
    transcoded_pictures: Vec<TranscodedPicturePayload>,
    has_abuse_reporting: bool,
    abuse_report_state: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscodedPicturePayload {
    target_name: String,
    uri: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageRefPayload {
    file_id_hex: String,
    size: String,
    width: i32,
    height: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioFilePayload {
    format: String,
    file_id_hex: String,
    mime_type: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RestrictionPayload {
    restriction_type: String,
    catalogues: Vec<String>,
    catalogue_strs: Vec<String>,
    countries_allowed: Option<Vec<String>>,
    countries_forbidden: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailabilityPayload {
    catalogue_strs: Vec<String>,
    start: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContentRatingPayload {
    country: String,
    tags: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CopyrightPayload {
    copyright_type: String,
    text: String,
}

fn ffi_string(value: &str) -> *mut c_char {
    CString::new(value.replace('\0', "\\0"))
        .unwrap_or_default()
        .into_raw()
}

fn ffi_box<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}

fn ffi_vec<T>(values: Vec<T>) -> (*mut T, usize) {
    let mut values = values.into_boxed_slice();
    let len = values.len();
    let ptr = values.as_mut_ptr();
    std::mem::forget(values);
    (ptr, len)
}

pub(crate) fn alloc_ffi_track(payload: TrackPayload) -> *mut FfiTrack {
    ffi_box(build_ffi_track(payload))
}

pub(crate) fn alloc_ffi_album(payload: AlbumPayload) -> *mut FfiAlbum {
    ffi_box(build_ffi_album(payload))
}

pub(crate) fn alloc_ffi_artist(payload: ArtistPayload) -> *mut FfiArtist {
    ffi_box(build_ffi_artist(payload))
}

pub(crate) fn alloc_ffi_playlist(payload: PlaylistPayload) -> *mut FfiPlaylist {
    ffi_box(build_ffi_playlist(payload))
}

pub(crate) fn alloc_ffi_user_profile(payload: UserProfilePayload) -> *mut FfiUserProfile {
    ffi_box(build_ffi_user_profile(payload))
}

pub(crate) fn alloc_ffi_playlist_list(payload: PlaylistListPayload) -> *mut FfiPlaylistList {
    ffi_box(build_ffi_playlist_list(payload))
}

pub(crate) fn alloc_ffi_track_list(payload: TrackListPayload) -> *mut FfiTrackList {
    ffi_box(build_ffi_track_list(payload))
}

pub(crate) fn alloc_ffi_artist_list(payload: ArtistListPayload) -> *mut FfiArtistList {
    ffi_box(build_ffi_artist_list(payload))
}

pub(crate) fn alloc_ffi_search(payload: SearchPayload) -> *mut FfiSearch {
    ffi_box(build_ffi_search(payload))
}

fn build_ffi_image(payload: ImagePayload) -> FfiImage {
    FfiImage {
        url: ffi_string(&payload.url),
        width: payload.width,
        height: payload.height,
    }
}

fn build_ffi_artist_summary(payload: ArtistSummaryPayload) -> FfiArtistSummary {
    FfiArtistSummary {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
    }
}

fn build_ffi_album_summary(payload: AlbumSummaryPayload) -> FfiAlbumSummary {
    let (images, image_count) = ffi_vec(payload.images.into_iter().map(build_ffi_image).collect());
    let (artists, artist_count) = ffi_vec(
        payload
            .artists
            .into_iter()
            .map(build_ffi_artist_summary)
            .collect(),
    );

    FfiAlbumSummary {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        album_type: ffi_string(&payload.album_type),
        release_date: ffi_string(""),
        total_tracks: 0,
        images,
        image_count,
        artists,
        artist_count,
    }
}

fn build_ffi_simple_track(payload: SimpleTrackPayload) -> FfiSimpleTrack {
    let (artists, artist_count) = ffi_vec(
        payload
            .artists
            .into_iter()
            .map(build_ffi_artist_summary)
            .collect(),
    );

    FfiSimpleTrack {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        duration_ms: payload.duration_ms,
        disc_number: payload.disc_number,
        track_number: payload.track_number,
        artists,
        artist_count,
    }
}

fn build_ffi_track(payload: TrackPayload) -> FfiTrack {
    let (artists, artist_count) = ffi_vec(
        payload
            .artists
            .into_iter()
            .map(build_ffi_artist_summary)
            .collect(),
    );

    FfiTrack {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        duration_ms: payload.duration_ms,
        disc_number: payload.disc_number,
        track_number: payload.track_number,
        artists,
        artist_count,
        album: ffi_box(build_ffi_album_summary(payload.album)),
    }
}

fn build_ffi_album(payload: AlbumPayload) -> FfiAlbum {
    let (images, image_count) = ffi_vec(payload.images.into_iter().map(build_ffi_image).collect());
    let (artists, artist_count) = ffi_vec(
        payload
            .artists
            .into_iter()
            .map(build_ffi_artist_summary)
            .collect(),
    );
    let (tracks, track_count) = ffi_vec(
        payload
            .tracks
            .into_iter()
            .map(build_ffi_simple_track)
            .collect(),
    );

    FfiAlbum {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        album_type: ffi_string(&payload.album_type),
        release_date: ffi_string(&payload.release_date),
        total_tracks: payload.total_tracks,
        images,
        image_count,
        artists,
        artist_count,
        tracks,
        track_count,
    }
}

fn build_ffi_artist(payload: ArtistPayload) -> FfiArtist {
    let (images, image_count) = ffi_vec(payload.images.into_iter().map(build_ffi_image).collect());
    let (albums, album_count) = ffi_vec(
        payload
            .albums
            .into_iter()
            .map(build_ffi_album_summary)
            .collect(),
    );

    FfiArtist {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        images,
        image_count,
        albums,
        album_count,
    }
}

fn build_ffi_owner(payload: OwnerPayload) -> FfiOwner {
    FfiOwner {
        id: ffi_string(&payload.id),
        display_name: ffi_string(&payload.display_name),
    }
}

fn build_ffi_playlist_summary(payload: PlaylistSummaryPayload) -> FfiPlaylistSummary {
    let (images, image_count) = ffi_vec(payload.images.into_iter().map(build_ffi_image).collect());

    FfiPlaylistSummary {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        images,
        image_count,
    }
}

fn build_ffi_playlist(payload: PlaylistPayload) -> FfiPlaylist {
    let (images, image_count) = ffi_vec(payload.images.into_iter().map(build_ffi_image).collect());
    let (tracks, track_count) = ffi_vec(
        payload
            .tracks
            .into_iter()
            .map(|item| build_ffi_track(item.track))
            .collect(),
    );

    FfiPlaylist {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        name: ffi_string(&payload.name),
        images,
        image_count,
        owner: ffi_box(build_ffi_owner(payload.owner)),
        tracks,
        track_count,
    }
}

fn build_ffi_user_profile(payload: UserProfilePayload) -> FfiUserProfile {
    let (images, image_count) = ffi_vec(payload.images.into_iter().map(build_ffi_image).collect());

    FfiUserProfile {
        id: ffi_string(&payload.id),
        uri: ffi_string(&payload.uri),
        display_name: ffi_string(&payload.display_name),
        email: ffi_string(&payload.email),
        country: ffi_string(&payload.country),
        images,
        image_count,
    }
}

fn build_ffi_playlist_list(payload: PlaylistListPayload) -> FfiPlaylistList {
    let (items, item_count) = ffi_vec(
        payload
            .items
            .into_iter()
            .map(build_ffi_playlist_summary)
            .collect(),
    );

    FfiPlaylistList { items, item_count }
}

fn build_ffi_track_list(payload: TrackListPayload) -> FfiTrackList {
    let (items, item_count) = ffi_vec(payload.items.into_iter().map(build_ffi_track).collect());
    FfiTrackList { items, item_count }
}

fn build_ffi_artist_list(payload: ArtistListPayload) -> FfiArtistList {
    let (items, item_count) = ffi_vec(
        payload
            .items
            .into_iter()
            .map(build_ffi_artist_summary)
            .collect(),
    );
    FfiArtistList { items, item_count }
}

fn build_ffi_search(payload: SearchPayload) -> FfiSearch {
    let (tracks, track_count) = ffi_vec(payload.tracks.into_iter().map(build_ffi_track).collect());
    let (albums, album_count) = ffi_vec(
        payload
            .albums
            .into_iter()
            .map(build_ffi_album_summary)
            .collect(),
    );
    let (artists, artist_count) = ffi_vec(
        payload
            .artists
            .into_iter()
            .map(build_ffi_artist_summary)
            .collect(),
    );
    let (playlists, playlist_count) = ffi_vec(
        payload
            .playlists
            .into_iter()
            .map(build_ffi_playlist_summary)
            .collect(),
    );

    FfiSearch {
        tracks,
        track_count,
        albums,
        album_count,
        artists,
        artist_count,
        playlists,
        playlist_count,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsForImageRequest {
    track_uri: String,
    image_id_hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApolloStationRequest {
    scope: String,
    context_uri: String,
    count: Option<usize>,
    previous_track_uris: Vec<String>,
    autoplay: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutoplayContextRequestPayload {
    context_uri: String,
    recent_track_uris: Vec<String>,
    is_video: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootlistRequest {
    from: usize,
    length: Option<usize>,
}

fn load_offline_index(path: &PathBuf) -> HashMap<String, OfflineTrackIndexEntry> {
    match fs::read_to_string(path) {
        Ok(json) => serde_json::from_str::<Vec<OfflineTrackIndexEntry>>(&json)
            .map(|items| {
                items
                    .into_iter()
                    .map(|item| (item.track_uri.clone(), item))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn save_offline_index(path: &PathBuf, index: &HashMap<String, OfflineTrackIndexEntry>) {
    let entries: Vec<_> = index.values().cloned().collect();
    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        let _ = fs::write(path, json);
    }
}

fn parse_file_id_hex(hex: &str) -> Option<FileId> {
    let bytes = hex::decode(hex).ok()?;
    Some(FileId::from_raw(&bytes))
}
