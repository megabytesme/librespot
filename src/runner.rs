use crate::ffi_types::*;
use futures_util::StreamExt;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{
    Session, SessionConfig, SpotifyId, SpotifyUri, authentication::Credentials, cache::Cache,
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
use std::str::FromStr;
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
        // Explicit type annotation for the task handle
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
                    LibrespotCommand::Play => player.play(),
                    LibrespotCommand::Pause => player.pause(),

                    // Route through Spirc (the Connect controller) for playlist logic
                    LibrespotCommand::Next => {
                        if let Some(ref s) = spirc { s.next(); }
                    },
                    LibrespotCommand::Prev => {
                        if let Some(ref s) = spirc { s.prev(); }
                    },

                    // Use the mixer directly for volume
                    LibrespotCommand::SetVolume(v) => {
                        mixer.set_volume(v);
                        // Also notify the player so it can emit events to other controllers
                        player.emit_volume_changed_event(v);
                    },

                    LibrespotCommand::Seek(ms) => player.seek(ms),

                    LibrespotCommand::Load { uri, play } => {
                        // Correct conversion to SpotifyUri for this version of the player
                        if let Ok(track_uri) = SpotifyUri::from_uri(&uri) {
                            player.load(track_uri, play, 0);
                        }
                    }

                    LibrespotCommand::StartDiscovery => {
                        if discovery.is_none() {
                            discovery = Discovery::builder(self.setup.session_config.device_id.clone(), self.setup.session_config.client_id.clone())
                                .name(self.setup.device_name.clone())
                                .device_type(self.setup.device_type)
                                .launch()
                                .ok();
                        }
                    },

                    LibrespotCommand::UpdateCredentials { username, auth_data } => {
                        last_creds = Some(Credentials::with_password(username, auth_data));
                        connecting = true;
                    }
                }
            }
                        Some(event) = player_rx.recv() => {
                            self.handle_player_event(event);
                        }

                        // Explicit match for discovery future
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
                                if let Some(s) = spirc.take() {
                                    let _ = s.shutdown();
                                }

                                // Fix explicit type inference for tuple return
                                let spirc_res = Spirc::new(
                                    self.setup.connect_config.clone(),
                                    session.clone(),
                                    c,
                                    player.clone(),
                                    mixer.clone()
                                ).await;

                                if let Ok((s, task)) = spirc_res {
                                    spirc = Some(s);
                                    // Explicitly box and pin the opaque future returned by librespot
                                    spirc_task = Some(Box::pin(task));

                                    self.emit(LibrespotEvent {
                                        event_type: EventType::SessionConnected,
                                        data: unsafe { std::mem::zeroed() }
                                    });
                                    connecting = false;
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
