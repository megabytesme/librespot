use crate::ffi_types::*;
use crate::runner::RunnerSetup;
use librespot_connect::ConnectConfig;
use librespot_core::SessionConfig;
use librespot_core::authentication::Credentials;
use librespot_core::config::DeviceType;
use librespot_playback::config::{PlayerConfig, VolumeCtrl};
use librespot_playback::mixer::MixerConfig;
use librespot_protocol::authentication::AuthenticationType;
use sha1::Digest;
use std::ffi::CStr;
use std::path::PathBuf;
use std::str::FromStr;

pub fn parse_ffi_config(c_cfg: &LibrespotConfig) -> Result<RunnerSetup, String> {
    unsafe {
        if c_cfg.device_name.is_null()
            || c_cfg.device_type.is_null()
            || c_cfg.cache_dir.is_null()
            || c_cfg.persisted_cache_dir.is_null()
        {
            return Err(
                "Mandatory configuration strings (name, type, cache_dir, persisted_cache_dir) are NULL"
                    .to_string(),
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
        let persisted_cache_path = CStr::from_ptr(c_cfg.persisted_cache_dir).to_string_lossy();

        let cache_dir = PathBuf::from(cache_path.as_ref());
        let persisted_cache_dir = PathBuf::from(persisted_cache_path.as_ref());

        let initial_creds = if !c_cfg.playback_credentials.is_null() {
            let json = CStr::from_ptr(c_cfg.playback_credentials)
                .to_string_lossy()
                .into_owned();
            let credentials: Credentials = serde_json::from_str(&json)
                .map_err(|err| format!("FFI: Invalid playback credentials: {err}"))?;
            if credentials.auth_type
                != AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS
                || credentials.username.as_deref().is_none_or(str::is_empty)
                || credentials.auth_data.is_empty()
            {
                return Err("FFI: Playback credentials were incomplete or had the wrong authentication type".to_string());
            }
            log::info!("FFI: Authenticating via stored playback credentials");
            Some(credentials)
        } else if !c_cfg.access_token.is_null() {
            let token = CStr::from_ptr(c_cfg.access_token)
                .to_string_lossy()
                .into_owned();
            log::info!("FFI: Authenticating via playback bootstrap token");
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
            tmp_dir: cache_dir.clone(),
            ..SessionConfig::default()
        };

        let device_type = DeviceType::from_str(&device_type_str).unwrap_or(DeviceType::Speaker);

        let connect_config = ConnectConfig {
            name: device_name.clone(),
            device_type,
            initial_volume: c_cfg.initial_volume,
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
            key_callback: c_cfg.key_callback,
            key_save_callback: c_cfg.key_save_callback,
            key_remove_callback: c_cfg.key_remove_callback,
            cache_dir,
            persisted_cache_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn config_with_authorization(
        device_name: &CString,
        device_type: &CString,
        cache_dir: &CString,
        persisted_cache_dir: &CString,
        access_token: *const std::ffi::c_char,
        playback_credentials: *const std::ffi::c_char,
    ) -> LibrespotConfig {
        LibrespotConfig {
            device_name: device_name.as_ptr(),
            device_type: device_type.as_ptr(),
            cache_dir: cache_dir.as_ptr(),
            persisted_cache_dir: persisted_cache_dir.as_ptr(),
            enable_discovery: false,
            enable_volume_normalisation: false,
            bitrate: Bitrate::B160,
            format: AudioFormat::S16,
            initial_volume: 32_768,
            username: std::ptr::null(),
            password: std::ptr::null(),
            auth_blob: std::ptr::null(),
            access_token,
            playback_credentials,
            key_callback: None,
            key_save_callback: None,
            key_remove_callback: None,
        }
    }

    fn required_strings() -> (CString, CString, CString, CString) {
        (
            CString::new("LibreSpotUWP test").unwrap(),
            CString::new("speaker").unwrap(),
            CString::new("cache").unwrap(),
            CString::new("persisted").unwrap(),
        )
    }

    #[test]
    fn stored_playback_credentials_take_precedence_over_bootstrap_token() {
        let (device_name, device_type, cache_dir, persisted_cache_dir) = required_strings();
        let expected = Credentials {
            username: Some("spotify-user".to_owned()),
            auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
            auth_data: vec![1, 2, 3, 4],
        };
        let credentials = CString::new(serde_json::to_string(&expected).unwrap()).unwrap();
        let token = CString::new("temporary-keymaster-token").unwrap();
        let config = config_with_authorization(
            &device_name,
            &device_type,
            &cache_dir,
            &persisted_cache_dir,
            token.as_ptr(),
            credentials.as_ptr(),
        );

        let setup = parse_ffi_config(&config).expect("stored credential should be accepted");
        assert_eq!(setup.initial_creds, Some(expected));
    }

    #[test]
    fn keymaster_token_is_only_used_as_a_bootstrap_credential() {
        let (device_name, device_type, cache_dir, persisted_cache_dir) = required_strings();
        let token = CString::new("temporary-keymaster-token").unwrap();
        let config = config_with_authorization(
            &device_name,
            &device_type,
            &cache_dir,
            &persisted_cache_dir,
            token.as_ptr(),
            std::ptr::null(),
        );

        let setup = parse_ffi_config(&config).expect("bootstrap token should be accepted");
        let credentials = setup.initial_creds.expect("bootstrap credentials missing");
        assert_eq!(
            credentials.auth_type,
            AuthenticationType::AUTHENTICATION_SPOTIFY_TOKEN
        );
        assert_eq!(credentials.auth_data, b"temporary-keymaster-token");
    }

    #[test]
    fn non_stored_credentials_are_rejected_from_persistent_boundary() {
        let (device_name, device_type, cache_dir, persisted_cache_dir) = required_strings();
        let invalid = Credentials::with_access_token("web-api-token");
        let credentials = CString::new(serde_json::to_string(&invalid).unwrap()).unwrap();
        let config = config_with_authorization(
            &device_name,
            &device_type,
            &cache_dir,
            &persisted_cache_dir,
            std::ptr::null(),
            credentials.as_ptr(),
        );

        let result = parse_ffi_config(&config);
        let error = match result {
            Ok(_) => panic!("non-stored credentials crossed the persistent boundary"),
            Err(error) => error,
        };
        assert!(error.contains("wrong authentication type"));
    }
}
