use crate::{UserDataWrapper, ffi_types::*};
use futures_util::StreamExt;
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::{
    Session, SessionConfig, SpotifyUri, authentication::Credentials, cache::Cache,
    config::DeviceType,
};
use librespot_discovery::Discovery;
use librespot_playback::{
    config::{AudioFormat, PlayerConfig},
    mixer::{self, MixerConfig},
    player::{Player, PlayerEvent},
};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, atomic::AtomicU8};
use std::{ffi::CString, sync::atomic::AtomicUsize};
use std::{mem::ManuallyDrop, pin::Pin};
use tokio::sync::mpsc;

#[derive(Debug)]
#[allow(dead_code)]
pub enum LibrespotCommand {
    Load {
        context_uri: String,
        start_from_uri: Option<String>,
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
}

pub struct TrackMetadataInternal {
    pub uri: CString,
    pub name: CString,
    pub artist: CString,
    pub album: CString,
    pub cover_url: CString,
    pub duration_ms: u32,
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
        }
    }
}

pub struct Runner {
    setup: RunnerSetup,
    cmd_rx: mpsc::UnboundedReceiver<LibrespotCommand>,
    callback: LibrespotCallback,
    user_data: UserDataWrapper,
    pub state: Arc<RunnerState>,
}

impl Runner {
    pub fn new(
        setup: RunnerSetup,
        cmd_rx: mpsc::UnboundedReceiver<LibrespotCommand>,
        callback: LibrespotCallback,
        user_data: UserDataWrapper,
        state: Arc<RunnerState>,
    ) -> Self {
        Self {
            setup,
            cmd_rx,
            callback,
            user_data,
            state,
        }
    }

    fn emit(&self, event: LibrespotEvent) {
        (self.callback)(&event, self.user_data.0);
    }

    pub async fn run(&mut self) {
        log::info!("Runner::run() starting");

        let cache = Cache::new(
            Some(self.setup.session_config.tmp_dir.clone()),
            Some(self.setup.session_config.tmp_dir.clone()),
            Some(self.setup.session_config.tmp_dir.join("audio")),
            Some(1024 * 1024 * 500),
        )
        .ok();

        let mut session = Session::new(self.setup.session_config.clone(), cache.clone());
        session
            .audio_key()
            .set_ffi_hooks(self.setup.key_callback, self.user_data.0);
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

        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
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
                                let _ = s.next().map_err(|e| log::error!("Spirc Next failed: {:?}", e));
                            } else {
                                log::warn!("Next command ignored: No active Spirc session");
                            }
                        }
                        LibrespotCommand::Prev => {
                            if let Some(ref s) = spirc {
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
                        LibrespotCommand::Load { context_uri, start_from_uri, play } => {
                            if let Ok(_ctx_uri) = SpotifyUri::from_uri(&context_uri) {
                                if let Some(ref s) = spirc {
                                    let _ = s.activate();

                                    let context_options = LoadContextOptions::Options(Options {
                                        shuffle: self.state.shuffle.load(Ordering::Acquire),
                                        repeat: self.state.repeat.load(Ordering::Acquire) == 1,
                                        repeat_track: self.state.repeat.load(Ordering::Acquire) == 2,
                                    });

                                    let target = start_from_uri.unwrap_or_else(|| context_uri.clone());

                                    let options = LoadRequestOptions {
                                        start_playing: play,
                                        seek_to: 0,
                                        context_options: Some(context_options),
                                        playing_track: Some(PlayingTrack::Uri(target)),
                                    };

                                    let request = LoadRequest::from_context_uri(context_uri, options);
                                    if let Err(e) = s.load(request) {
                                        log::error!("Spirc load failed: {:?}", e);
                                    }
                                } else {
                                    let track_to_load = start_from_uri.unwrap_or(context_uri);
                                    if let Ok(t_uri) = SpotifyUri::from_uri(&track_to_load) {
                                        player.load(t_uri, play, 0);
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

                _ = async {}, if connecting && last_creds.is_some() => {
                    if session.is_invalid() {
                        session = Session::new(self.setup.session_config.clone(), cache.clone());
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
                                self.emit(LibrespotEvent { event_type: EventType::SessionConnected, data: unsafe { std::mem::zeroed() } });
                                connecting = false;
                            }
                            Err(e) => log::error!("Spirc connection failed: {:?}", e),
                        }
                    }
                }

                _ = async {
                    if let Some(t) = spirc_task.as_mut() { t.await; }
                }, if spirc_task.is_some() => {
                    spirc_task = None;
                    spirc = None;
                    self.emit(LibrespotEvent { event_type: EventType::SessionDisconnected, data: unsafe { std::mem::zeroed() } });
                }
            }
        }
    }

    fn handle_player_event(&self, event: PlayerEvent) {
        let mut data: EventData = unsafe { std::mem::zeroed() };
        let mut temp_strings: Vec<CString> = Vec::new();

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

            PlayerEvent::TrackChanged { audio_item } => {
                let duration = audio_item.duration_ms as u32;
                self.state.duration_ms.store(duration, Ordering::Relaxed);

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
}
