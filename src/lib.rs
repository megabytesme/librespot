mod config;
mod ffi_types;
mod logger;
mod runner;

use crate::ffi_types::*;
use crate::runner::{LibrespotCommand, Runner};
use std::ffi::{CStr, c_char};
use std::os::raw::c_void;
use std::thread;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

pub struct LibrespotInstance {
    cmd_tx: mpsc::UnboundedSender<LibrespotCommand>,
    _thread_handle: thread::JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct UserDataWrapper(*mut c_void);
unsafe impl Send for UserDataWrapper {}
unsafe impl Sync for UserDataWrapper {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_new(
    config: LibrespotConfig,
    callback: LibrespotCallback,
    user_data: *mut c_void,
) -> *mut LibrespotInstance {
    let _ = logger::init_logger(callback, user_data);

    let setup = match config::parse_ffi_config(&config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to parse config: {e}");
            return std::ptr::null_mut();
        }
    };

    let (tx, rx) = mpsc::unbounded_channel();

    let user_data_wrapper = UserDataWrapper(user_data);

    let thread_handle = thread::spawn(move || {
        let user_data_wrapper = user_data_wrapper;
        let callback = callback;
        let rx = rx;
        let setup = setup;

        let rt = Runtime::new().expect("Failed to create Tokio runtime");

        rt.block_on(async move {
            let raw_ptr = user_data_wrapper.0;

            let mut runner = Runner::new(setup, rx, callback, raw_ptr);
            runner.run().await;
        });
    });

    let instance = Box::new(LibrespotInstance {
        cmd_tx: tx,
        _thread_handle: thread_handle,
    });

    Box::into_raw(instance)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_free(instance: *mut LibrespotInstance) {
    if !instance.is_null() {
        let _ = unsafe { Box::from_raw(instance) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_load(
    instance: *mut LibrespotInstance,
    uri: *const c_char,
    play: bool,
) {
    let uri_str = unsafe { CStr::from_ptr(uri).to_string_lossy().into_owned() };
    unsafe { send_cmd(instance, LibrespotCommand::Load { uri: uri_str, play }) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_play(instance: *mut LibrespotInstance) {
    unsafe { send_cmd(instance, LibrespotCommand::Play) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_pause(instance: *mut LibrespotInstance) {
    unsafe { send_cmd(instance, LibrespotCommand::Pause) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_next(instance: *mut LibrespotInstance) {
    unsafe { send_cmd(instance, LibrespotCommand::Next) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_prev(instance: *mut LibrespotInstance) {
    unsafe { send_cmd(instance, LibrespotCommand::Prev) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_set_volume(instance: *mut LibrespotInstance, volume: u16) {
    unsafe { send_cmd(instance, LibrespotCommand::SetVolume(volume)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_seek(instance: *mut LibrespotInstance, position_ms: u32) {
    unsafe { send_cmd(instance, LibrespotCommand::Seek(position_ms)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_start_discovery(instance: *mut LibrespotInstance) {
    unsafe { send_cmd(instance, LibrespotCommand::StartDiscovery) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn librespot_update_credentials(
    instance: *mut LibrespotInstance,
    username: *const c_char,
    auth_data: *const c_char,
) {
    let u = unsafe { CStr::from_ptr(username).to_string_lossy().into_owned() };
    let a = unsafe { CStr::from_ptr(auth_data).to_string_lossy().into_owned() };

    unsafe {
        send_cmd(
            instance,
            LibrespotCommand::UpdateCredentials {
                username: u,
                auth_data: a,
            },
        );
    }
}

unsafe fn send_cmd(instance: *mut LibrespotInstance, cmd: LibrespotCommand) {
    unsafe {
        if let Some(inst) = instance.as_ref() {
            let _ = inst.cmd_tx.send(cmd);
        }
    }
}
