use crate::{UserDataWrapper, ffi_types::*};
use futures_util::StreamExt;
use librespot_connect::{ConnectConfig, Spirc};
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
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::{ffi::CString, sync::atomic::AtomicUsize};
use tokio::sync::mpsc;

pub enum LibrespotCommand {
    Load { uri: String, play: bool },
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
    UpdateCredentials { username: String, auth_data: String },
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
}

impl RunnerState {
    pub fn new() -> Self {
        Self {
            is_playing: AtomicBool::new(false),
            shuffle: AtomicBool::new(false),
            repeat: AtomicU32::new(0),
            position_ms: AtomicU32::new(0),
            sync_write_pos: AtomicUsize::new(0),
            duration_ms: AtomicU32::new(0),
            volume: AtomicU16::new(0),
            current_track: RwLock::new(None),
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
    ) -> Self {
        Self {
            setup,
            cmd_rx,
            callback,
            user_data,
            state: Arc::new(RunnerState::new()),
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
        let mixer_builder = mixer::find(None).expect("No mixer found");
        let mixer = mixer_builder(self.setup.mixer_config.clone()).expect("Failed to create mixer");
        let backend = self.setup.audio_backend;
        let format = self.setup.audio_format;

        let player = Player::new(
            self.setup.player_config.clone(),
            session.clone(),
            mixer.get_soft_volume(),
            move || (backend)(None, format),
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
                // UI/Hardware Commands
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
                        LibrespotCommand::Stop => player.stop(),
                        LibrespotCommand::Play => player.play(),
                        LibrespotCommand::Pause => player.pause(),
                        LibrespotCommand::Next => { if let Some(ref s) = spirc { s.next(); } }
                        LibrespotCommand::Prev => { if let Some(ref s) = spirc { s.prev(); } }
                        LibrespotCommand::Seek(ms) => player.seek(ms),
                        LibrespotCommand::SetVolume(v) => {
                            mixer.set_volume(v);
                            self.state.volume.store(v, Ordering::Relaxed);
                            player.emit_volume_changed_event(v);
                        }
                        LibrespotCommand::SetShuffle(enabled) => {
                            if let Some(ref s) = spirc { let _ = s.shuffle(enabled); }
                        }
                        LibrespotCommand::SetRepeatContext(enabled) => {
                            if let Some(ref s) = spirc { let _ = s.repeat(enabled); }
                        }
                        LibrespotCommand::SetRepeatTrack(enabled) => {
                            if let Some(ref s) = spirc { let _ = s.repeat_track(enabled); }
                        }
                        LibrespotCommand::Load { uri, play } => {
                            if let Ok(track_uri) = SpotifyUri::from_uri(&uri) {
                                player.load(track_uri, play, 0);
                            }
                        }
                        LibrespotCommand::UpdateCredentials { username, auth_data } => {
                            last_creds = Some(Credentials::with_password(username, auth_data));
                            connecting = true;
                        }
                        LibrespotCommand::StartDiscovery => {}
                    }
                }

                // Events coming from Librespot
                Some(event) = player_rx.recv() => {
                    self.handle_player_event(event);
                }

                // Discovery and Session Management
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
        match event {
            PlayerEvent::Playing { position_ms, .. }
            | PlayerEvent::Paused { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. } => {
                let current_wp = librespot_playback::audio_backend::get_write_pos();
                self.state.position_ms.store(position_ms, Ordering::Release);
                self.state
                    .sync_write_pos
                    .store(current_wp, Ordering::Release);

                if let PlayerEvent::Playing { .. } = event {
                    self.state.is_playing.store(true, Ordering::Release);
                    self.emit(LibrespotEvent {
                        event_type: EventType::PlaybackResumed,
                        data: unsafe { std::mem::zeroed() },
                    });
                } else if let PlayerEvent::Paused { .. } = event {
                    self.state.is_playing.store(false, Ordering::Release);
                    self.emit(LibrespotEvent {
                        event_type: EventType::PlaybackPaused,
                        data: unsafe { std::mem::zeroed() },
                    });
                }
            }
            PlayerEvent::TrackChanged { audio_item } => {
                let duration = audio_item.duration_ms as u32;
                self.state.duration_ms.store(duration, Ordering::Relaxed);

                let internal = TrackMetadataInternal {
                    uri: CString::new(audio_item.uri.clone()).unwrap_or_default(),
                    name: CString::new(audio_item.name.clone()).unwrap_or_default(),
                    artist: CString::new("").unwrap(),
                    album: CString::new("").unwrap(),
                    cover_url: CString::new("").unwrap(),
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

                self.emit(LibrespotEvent {
                    event_type: EventType::TrackChanged,
                    data: EventData {
                        track: std::mem::ManuallyDrop::new(meta),
                    },
                });
            }
            PlayerEvent::Stopped { .. } => {
                self.state.is_playing.store(false, Ordering::Release);
                self.state.position_ms.store(0, Ordering::Release);
            }
            PlayerEvent::VolumeChanged { volume } => {
                self.state.volume.store(volume, Ordering::Relaxed);
                self.emit(LibrespotEvent {
                    event_type: EventType::VolumeChanged,
                    data: EventData { volume },
                });
            }
            _ => {}
        }
    }
}
