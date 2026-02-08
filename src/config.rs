use crate::ffi_types::*;
use crate::runner::RunnerSetup;
use librespot_connect::ConnectConfig;
use librespot_core::SessionConfig;
use librespot_core::authentication::Credentials;
use librespot_core::config::DeviceType;
use librespot_playback::config::{PlayerConfig, VolumeCtrl};
use librespot_playback::mixer::MixerConfig;
use sha1::Digest;
use std::ffi::CStr;
use std::path::PathBuf;
use std::str::FromStr;

pub fn parse_ffi_config(c_cfg: &LibrespotConfig) -> Result<RunnerSetup, String> {
    unsafe {
        if c_cfg.device_name.is_null() || c_cfg.device_type.is_null() || c_cfg.cache_dir.is_null() {
            return Err(
                "Mandatory configuration strings (name, type, or cache) are NULL".to_string(),
            );
        }

        let bitrate = match c_cfg.bitrate {
            Bitrate::B96 => librespot_playback::config::Bitrate::Bitrate96,
            Bitrate::B160 => librespot_playback::config::Bitrate::Bitrate160,
            Bitrate::B320 => librespot_playback::config::Bitrate::Bitrate320,
        };

        let rs_format = match c_cfg.format {
            AudioFormat::F64 => librespot_playback::config::AudioFormat::F64,
            AudioFormat::F32 => librespot_playback::config::AudioFormat::F32,
            AudioFormat::S32 => librespot_playback::config::AudioFormat::S32,
            AudioFormat::S24 => librespot_playback::config::AudioFormat::S24,
            AudioFormat::S24_3 => librespot_playback::config::AudioFormat::S24_3,
            AudioFormat::S16 => librespot_playback::config::AudioFormat::S16,
        };

        let device_name = CStr::from_ptr(c_cfg.device_name)
            .to_string_lossy()
            .into_owned();
        let device_type_str = CStr::from_ptr(c_cfg.device_type).to_string_lossy();
        let cache_path = CStr::from_ptr(c_cfg.cache_dir).to_string_lossy();
        let path = PathBuf::from(cache_path.as_ref());

        let initial_creds = if !c_cfg.access_token.is_null() {
            let token = CStr::from_ptr(c_cfg.access_token)
                .to_string_lossy()
                .into_owned();
            log::info!("FFI: Authenticating via Access Token");
            Some(Credentials::with_access_token(token))
        } else if !c_cfg.username.is_null() && !c_cfg.password.is_null() {
            let user = CStr::from_ptr(c_cfg.username)
                .to_string_lossy()
                .into_owned();
            let pass = CStr::from_ptr(c_cfg.password)
                .to_string_lossy()
                .into_owned();
            log::info!("FFI: Authenticating via Password for user: {}", user);
            Some(Credentials::with_password(user, pass))
        } else {
            log::warn!("FFI: No credentials provided in config");
            None
        };

        let device_id_bytes = sha1::Sha1::digest(device_name.as_bytes());
        let device_id = hex::encode(device_id_bytes);

        let session_config = SessionConfig {
            device_id: device_id.clone(),
            tmp_dir: path.clone(),
            ..SessionConfig::default()
        };

        let device_type = DeviceType::from_str(&device_type_str).unwrap_or(DeviceType::Speaker);

        let connect_config = ConnectConfig {
            name: device_name.clone(),
            device_type,
            initial_volume: 50,
            ..ConnectConfig::default()
        };

        let player_config = PlayerConfig {
            bitrate,
            normalisation: c_cfg.enable_volume_normalisation,
            ..PlayerConfig::default()
        };

        let mixer_config = MixerConfig {
            volume_ctrl: VolumeCtrl::Log(60.0),
            ..MixerConfig::default()
        };

        let backend = librespot_playback::audio_backend::find(None)
            .ok_or_else(|| "FFI: No audio backend found".to_string())?;

        log::info!(
            "FFI Configuration successfully parsed for device: {}",
            device_name
        );

        Ok(RunnerSetup {
            connect_config,
            session_config,
            player_config,
            mixer_config,
            audio_backend: backend,
            audio_format: rs_format,
            initial_creds,
            enable_discovery: c_cfg.enable_discovery,
            device_name,
            device_type,
            zeroconf_port: 0,
        })
    }
}
