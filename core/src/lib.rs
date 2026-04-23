#[macro_use]
extern crate log;

use librespot_protocol as protocol;
use std::ffi::c_void;

#[macro_use]
mod component;

pub mod apresolve;
pub mod audio_key;
pub mod authentication;
pub mod cache;
pub mod cdn_url;
pub mod channel;
pub mod config;
mod connection;
pub mod date;
#[allow(dead_code)]
pub mod dealer;
pub mod deserialize_with;
#[doc(hidden)]
pub mod diffie_hellman;
pub mod error;
pub mod file_id;
pub mod http_client;
pub mod login5;
pub mod mercury;
pub mod packet;
mod proxytunnel;
pub mod session;
mod socket;
#[allow(dead_code)]
pub mod spclient;
pub mod spotify_id;
pub mod spotify_uri;
pub mod token;
#[doc(hidden)]
pub mod util;
pub mod version;

pub use config::SessionConfig;
pub use error::Error;
pub use file_id::FileId;
pub use session::Session;
pub use spotify_id::SpotifyId;
pub use spotify_uri::SpotifyUri;

#[derive(Clone, Copy)]
pub struct UserDataPtr(pub *mut std::ffi::c_void);

unsafe impl Send for UserDataPtr {}
unsafe impl Sync for UserDataPtr {}

pub type LibrespotKeyCallback = extern "C" fn(
    track_id_ptr: *const u8,
    file_id_ptr: *const u8,
    key_out: *mut u8,
    user_data: *mut c_void,
) -> bool;

pub type LibrespotKeySaveCallback =
    extern "C" fn(track_id: *const u8, key_in: *const u8, user_data: *mut c_void);

pub type LibrespotKeyRemoveCallback = extern "C" fn(track_id: *const u8, user_data: *mut c_void);
