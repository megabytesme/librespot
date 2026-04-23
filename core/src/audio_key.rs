use std::{collections::HashMap, io::Write, time::Duration};

use byteorder::{BigEndian, ByteOrder, WriteBytesExt};
use bytes::Bytes;
use thiserror::Error;
use tokio::sync::oneshot;

use crate::{
    Error, FileId, LibrespotKeyCallback, LibrespotKeySaveCallback, SpotifyId, UserDataPtr,
    packet::PacketType, util::SeqGenerator,
};

use std::ffi::c_void;

#[derive(Debug, Hash, PartialEq, Eq, Copy, Clone)]
pub struct AudioKey(pub [u8; 16]);

#[derive(Debug, Error)]
pub enum AudioKeyError {
    #[error("audio key error")]
    AesKey,
    #[error("other end of channel disconnected")]
    Channel,
    #[error("unexpected packet type {0}")]
    Packet(u8),
    #[error("sequence {0} not pending")]
    Sequence(u32),
    #[error("audio key response timeout")]
    Timeout,
}

impl From<AudioKeyError> for Error {
    fn from(err: AudioKeyError) -> Self {
        match err {
            AudioKeyError::AesKey => Error::unavailable(err),
            AudioKeyError::Channel => Error::aborted(err),
            AudioKeyError::Sequence(_) => Error::aborted(err),
            AudioKeyError::Packet(_) => Error::unimplemented(err),
            AudioKeyError::Timeout => Error::aborted(err),
        }
    }
}

component! {
    AudioKeyManager : AudioKeyManagerInner {
        sequence: SeqGenerator<u32> = SeqGenerator::new(0),
        pending: HashMap<u32, oneshot::Sender<Result<AudioKey, Error>>> = HashMap::new(),
        key_callback: Option<LibrespotKeyCallback> = None,
        key_save_callback: Option<LibrespotKeySaveCallback> = None,
        user_data: Option<UserDataPtr> = None,
    }
}

impl AudioKeyManager {
    pub(crate) fn dispatch(&self, cmd: PacketType, mut data: Bytes) -> Result<(), Error> {
        let seq = BigEndian::read_u32(data.split_to(4).as_ref());

        let sender = self
            .lock(|inner| inner.pending.remove(&seq))
            .ok_or(AudioKeyError::Sequence(seq))?;

        match cmd {
            PacketType::AesKey => {
                let mut key = [0u8; 16];
                key.copy_from_slice(data.as_ref());
                sender
                    .send(Ok(AudioKey(key)))
                    .map_err(|_| AudioKeyError::Channel)?
            }
            PacketType::AesKeyError => {
                error!(
                    "error audio key {:x} {:x}",
                    data.as_ref()[0],
                    data.as_ref()[1]
                );
                sender
                    .send(Err(AudioKeyError::AesKey.into()))
                    .map_err(|_| AudioKeyError::Channel)?
            }
            _ => {
                trace!("Did not expect {cmd:?} AES key packet with data {data:#?}");
                return Err(AudioKeyError::Packet(cmd as u8).into());
            }
        }

        Ok(())
    }

    pub async fn request(&self, track: SpotifyId, file: FileId) -> Result<AudioKey, Error> {
        let frontend_key = self.lock(|inner| {
            if let Some(callback) = inner.key_callback {
                let track_id_str = track.to_base62();
                trace!(
                    "Requesting audio key from frontend for track {}",
                    track_id_str
                );

                let mut key_buffer = [0u8; 16];
                let track_bytes = track.to_raw();
                let file_bytes = file.0;

                let found = callback(
                    track_bytes.as_ptr(),
                    file_bytes.as_ptr(),
                    key_buffer.as_mut_ptr(),
                    inner.user_data.map(|u| u.0).unwrap_or(std::ptr::null_mut()),
                );

                if found {
                    info!("Audio key for track {} provided by frontend", track_id_str);
                    return Some(AudioKey(key_buffer));
                }
            }
            None
        });

        if let Some(key) = frontend_key {
            return Ok(key);
        }

        trace!("Audio key not found in frontend; requesting from Spotify servers");
        let (tx, rx) = oneshot::channel();

        let seq = self.lock(move |inner| {
            let seq = inner.sequence.get();
            inner.pending.insert(seq, tx);
            seq
        });

        self.send_key_request(seq, track, file)?;

        const KEY_RESPONSE_TIMEOUT: Duration = Duration::from_millis(1500);
        match tokio::time::timeout(KEY_RESPONSE_TIMEOUT, rx).await {
            Err(_) => {
                error!("Audio key response timeout for track {}", track.to_base62());
                Err(AudioKeyError::Timeout.into())
            }
            Ok(k) => {
                let result: AudioKey = k.map_err(|_| AudioKeyError::Channel)??;

                self.lock(|inner| {
                    if let Some(save_cb) = inner.key_save_callback {
                        let track_bytes = track.to_raw();
                        save_cb(
                            track_bytes.as_ptr(),
                            result.0.as_ptr(),
                            inner.user_data.map(|u| u.0).unwrap_or(std::ptr::null_mut()),
                        );
                    }
                });

                trace!(
                    "Audio key for track {} received from Spotify",
                    track.to_base62()
                );

                Ok(result)
            }
        }
    }

    fn send_key_request(&self, seq: u32, track: SpotifyId, file: FileId) -> Result<(), Error> {
        let mut data: Vec<u8> = Vec::new();
        data.write_all(&file.0)?;
        data.write_all(&track.to_raw())?;
        data.write_u32::<BigEndian>(seq)?;
        data.write_u16::<BigEndian>(0x0000)?;

        self.session().send_packet(PacketType::RequestKey, data)
    }

    pub fn set_ffi_hooks(
        &self,
        callback: Option<LibrespotKeyCallback>,
        save_callback: Option<LibrespotKeySaveCallback>,
        user_data: *mut c_void,
    ) {
        self.lock(|inner| {
            inner.key_callback = callback;
            inner.key_save_callback = save_callback;
            inner.user_data = Some(UserDataPtr(user_data));
        });
    }
}
