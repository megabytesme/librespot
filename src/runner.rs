use crate::ffi_types::*;
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
use std::ffi::{CString, c_void};
use std::pin::Pin;
use tokio::sync::mpsc;

pub enum LibrespotCommand {
    Load { uri: String, play: bool },
    Play,
    Pause,
    Next,
    Prev,
    SetVolume(u16),
    Seek(u32),
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

pub struct Runner {
    setup: RunnerSetup,
    cmd_rx: mpsc::UnboundedReceiver<LibrespotCommand>,
    callback: LibrespotCallback,
    user_data: *mut c_void,
}

impl Runner {
    pub fn new(
        setup: RunnerSetup,
        cmd_rx: mpsc::UnboundedReceiver<LibrespotCommand>,
        callback: LibrespotCallback,
        user_data: *mut c_void,
    ) -> Self {
        log::info!("Runner::new()");
        log::info!("  device_name={}", setup.device_name);
        log::info!("  device_type={:?}", setup.device_type);
        log::info!("  enable_discovery={}", setup.enable_discovery);
        log::info!("  audio_format={:?}", setup.audio_format);
        log::info!("  player.bitrate={:?}", setup.player_config.bitrate);
        log::info!(
            "  player.normalisation={}",
            setup.player_config.normalisation
        );
        log::info!("  mixer.volume_ctrl={:?}", setup.mixer_config.volume_ctrl);
        log::info!("  tmp_dir={:?}", setup.session_config.tmp_dir);
        log::info!("  initial_creds={}", setup.initial_creds.is_some());

        Self {
            setup,
            cmd_rx,
            callback,
            user_data,
        }
    }

    fn emit(&self, event: LibrespotEvent) {
        unsafe { (self.callback)(&event, self.user_data) };
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

        log::info!("Cache initialised");

        let mut session = Session::new(self.setup.session_config.clone(), cache.clone());
        log::info!("Session created");

        let mixer_builder = mixer::find(None).expect("No mixer found");
        let mixer = mixer_builder(self.setup.mixer_config.clone()).expect("Failed to create mixer");
        log::info!("Mixer created");

        let backend = self.setup.audio_backend;
        log::info!("Audio backend resolved");

        let format = self.setup.audio_format;
        log::info!("Audio format for backend: {:?}", format);

        let player = Player::new(
            self.setup.player_config.clone(),
            session.clone(),
            mixer.get_soft_volume(),
            move || (backend)(None, format),
        );

        log::info!("Player created");

        let mut spirc: Option<Spirc> = None;
        let mut spirc_task: Option<Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = None;
        let mut discovery: Option<Discovery> = None;
        let mut player_rx = player.get_player_event_channel();

        if self.setup.enable_discovery {
            log::info!("Starting discovery service");
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

        log::info!("Runner main loop starting");

        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
                        LibrespotCommand::Stop => {
                            log::info!("Command: Stop");
                            player.stop();
                        }
                        LibrespotCommand::Play => {
                            log::info!("Command: Play");
                            player.play();
                        }
                        LibrespotCommand::Pause => {
                            log::info!("Command: Pause");
                            player.pause();
                        }
                        LibrespotCommand::Next => {
                            log::info!("Command: Next");
                            if let Some(ref s) = spirc { s.next(); }
                        }
                        LibrespotCommand::Prev => {
                            log::info!("Command: Prev");
                            if let Some(ref s) = spirc { s.prev(); }
                        }
                        LibrespotCommand::SetVolume(v) => {
                            log::info!("Command: SetVolume({})", v);
                            mixer.set_volume(v);
                            player.emit_volume_changed_event(v);
                        }
                        LibrespotCommand::Seek(ms) => {
                            log::info!("Command: Seek({})", ms);
                            player.seek(ms);
                        }
                        LibrespotCommand::Load { uri, play } => {
                            log::info!("Command: Load(uri={}, play={})", uri, play);
                            if let Ok(track_uri) = SpotifyUri::from_uri(&uri) {
                                player.load(track_uri, play, 0);
                            } else {
                                log::warn!("Invalid Spotify URI: {}", uri);
                            }
                        }
                        LibrespotCommand::StartDiscovery => {
                            log::info!("Command: StartDiscovery");
                            if discovery.is_none() {
                                discovery = Discovery::builder(
                                    self.setup.session_config.device_id.clone(),
                                    self.setup.session_config.client_id.clone(),
                                )
                                .name(self.setup.device_name.clone())
                                .device_type(self.setup.device_type)
                                .launch()
                                .ok();
                            }
                        }
                        LibrespotCommand::UpdateCredentials { username, auth_data } => {
                            log::info!("Command: UpdateCredentials(user={})", username);
                            last_creds = Some(Credentials::with_password(username, auth_data));
                            connecting = true;
                        }
                    }
                }

                Some(event) = player_rx.recv() => {
                    log::info!("PlayerEvent received: {:?}", event);
                    self.handle_player_event(event);
                }

                creds_opt = async {
                    match discovery.as_mut() {
                        Some(d) => d.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(creds) = creds_opt {
                        log::info!("Discovery provided credentials");
                        last_creds = Some(creds);
                        connecting = true;
                    }
                }

                _ = async {}, if connecting && last_creds.is_some() => {
                    log::info!("Connecting with new credentials");

                    if session.is_invalid() {
                        log::info!("Session invalid, recreating");
                        session = Session::new(self.setup.session_config.clone(), cache.clone());
                        player.set_session(session.clone());
                    }

                    if let Some(c) = last_creds.clone() {
                        if let Some(s) = spirc.take() {
                            log::info!("Shutting down old Spirc");
                            let _ = s.shutdown();
                        }

                        let spirc_res = Spirc::new(
                            self.setup.connect_config.clone(),
                            session.clone(),
                            c,
                            player.clone(),
                            mixer.clone()
                        ).await;

                        match spirc_res {
                            Ok((s, task)) => {
                                log::info!("Spirc connected");
                                spirc = Some(s);
                                spirc_task = Some(Box::pin(task));
                                self.emit(LibrespotEvent {
                                    event_type: EventType::SessionConnected,
                                    data: unsafe { std::mem::zeroed() }
                                });
                                connecting = false;
                            }
                            Err(e) => {
                                log::error!("Spirc connection failed: {:?}", e);
                            }
                        }
                    }
                }

                _ = async {
                    if let Some(t) = spirc_task.as_mut() { t.await; }
                }, if spirc_task.is_some() => {
                    log::warn!("Spirc task ended");
                    spirc_task = None;
                    spirc = None;
                    self.emit(LibrespotEvent {
                        event_type: EventType::SessionDisconnected,
                        data: unsafe { std::mem::zeroed() }
                    });
                }
            }
        }
    }

    fn handle_player_event(&self, event: PlayerEvent) {
        match event {
            PlayerEvent::TrackChanged { audio_item } => {
                let c_uri = CString::new(audio_item.uri).unwrap_or_default();
                let c_name = CString::new(audio_item.name).unwrap_or_default();
                let empty = CString::new("").unwrap();

                let meta = TrackMetadata {
                    uri: c_uri.as_ptr(),
                    name: c_name.as_ptr(),
                    artist: empty.as_ptr(),
                    album: empty.as_ptr(),
                    cover_url: empty.as_ptr(),
                    duration_ms: audio_item.duration_ms as u32,
                };

                let evt = LibrespotEvent {
                    event_type: EventType::TrackChanged,
                    data: EventData {
                        track: std::mem::ManuallyDrop::new(meta),
                    },
                };
                self.emit(evt);
            }
            PlayerEvent::Paused { .. } => {
                self.emit(LibrespotEvent {
                    event_type: EventType::PlaybackPaused,
                    data: unsafe { std::mem::zeroed() },
                });
            }
            PlayerEvent::Playing { .. } => {
                self.emit(LibrespotEvent {
                    event_type: EventType::PlaybackResumed,
                    data: unsafe { std::mem::zeroed() },
                });
            }
            PlayerEvent::VolumeChanged { volume } => {
                self.emit(LibrespotEvent {
                    event_type: EventType::VolumeChanged,
                    data: EventData { volume },
                });
            }
            _ => {}
        }
    }
}
