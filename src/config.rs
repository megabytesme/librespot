use crate::ffi_types::*;
use crate::runner::RunnerSetup;
use librespot_connect::ConnectConfig;
use librespot_core::SessionConfig;
use librespot_core::authentication::Credentials;
use librespot_core::config::DeviceType;
use librespot_playback::config::{
    AudioFormat as RsAudioFormat, Bitrate as RsBitrate, PlayerConfig, VolumeCtrl,
};
use librespot_playback::mixer::MixerConfig;
use sha1::Digest;
use std::ffi::CStr;
use std::path::PathBuf;
use std::str::FromStr;

pub fn parse_ffi_config(c_cfg: &LibrespotConfig) -> Result<RunnerSetup, String> {
    unsafe {
        let device_name = CStr::from_ptr(c_cfg.device_name)
            .to_string_lossy()
            .into_owned();
        let device_type_str = CStr::from_ptr(c_cfg.device_type).to_string_lossy();
        let cache_path = CStr::from_ptr(c_cfg.cache_dir).to_string_lossy();

        let path = PathBuf::from(cache_path.as_ref());

        let device_id_bytes = sha1::Sha1::digest(device_name.as_bytes());
        let device_id = hex::encode(device_id_bytes);

        let session_config = SessionConfig {
            device_id,
            tmp_dir: path.clone(),
            ..SessionConfig::default()
        };

        let connect_config = ConnectConfig {
            name: device_name.clone(),
            device_type: DeviceType::from_str(&device_type_str).unwrap_or(DeviceType::Speaker),
            initial_volume: 50,
            ..ConnectConfig::default()
        };

        let bitrate = match c_cfg.bitrate {
            Bitrate::B96 => RsBitrate::Bitrate96,
            Bitrate::B160 => RsBitrate::Bitrate160,
            Bitrate::B320 => RsBitrate::Bitrate320,
        };

        let rs_format = match c_cfg.format {
            AudioFormat::F32 => RsAudioFormat::F32,
            AudioFormat::S16 => RsAudioFormat::S16,
            AudioFormat::S24 => RsAudioFormat::S24,
            AudioFormat::S32 => RsAudioFormat::S32,
        };

        let player_config = PlayerConfig {
            bitrate,
            normalisation: c_cfg.enable_volume_normalisation,
            ..PlayerConfig::default()
        };

        let mixer_config = MixerConfig {
            volume_ctrl: VolumeCtrl::Log(VolumeCtrl::DEFAULT_DB_RANGE),
            ..MixerConfig::default()
        };

        let backend =
            librespot_playback::audio_backend::find(None).ok_or("No audio backend found")?;

        let initial_creds = if !c_cfg.access_token.is_null() {
            let token = CStr::from_ptr(c_cfg.access_token)
                .to_string_lossy()
                .into_owned();
            Some(Credentials::with_access_token(token))
        } else if !c_cfg.username.is_null() && !c_cfg.password.is_null() {
            let user = CStr::from_ptr(c_cfg.username)
                .to_string_lossy()
                .into_owned();
            let pass = CStr::from_ptr(c_cfg.password)
                .to_string_lossy()
                .into_owned();
            Some(Credentials::with_password(user, pass))
        } else if !c_cfg.auth_blob.is_null() && !c_cfg.username.is_null() {
            let user = CStr::from_ptr(c_cfg.username)
                .to_string_lossy()
                .into_owned();
            let blob_b64 = CStr::from_ptr(c_cfg.auth_blob)
                .to_string_lossy()
                .into_owned();

            log::warn!("auth_blob provided but blob auth not implemented in FFI; ignoring");
            None
        } else {
            None
        };

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
            device_type: DeviceType::from_str(&device_type_str).unwrap_or(DeviceType::Speaker),
            zeroconf_port: 0,
        })
    }
}
