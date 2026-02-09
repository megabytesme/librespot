use crate::ffi_types::{EventData, EventType, LibrespotCallback, LibrespotEvent};
use log::{Level, Metadata, Record, SetLoggerError};
use std::ffi::{CString, c_void};

static mut GLOBAL_CB: Option<LibrespotCallback> = None;
static mut GLOBAL_CTX: *mut c_void = std::ptr::null_mut();

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            unsafe {
                if let Some(cb) = GLOBAL_CB {
                    let msg = format!("{} - {}", record.level(), record.args());
                    let c_msg = CString::new(msg).unwrap();

                    let mut data: EventData = std::mem::zeroed();
                    data.log_msg = c_msg.as_ptr();

                    let evt = LibrespotEvent {
                        event_type: EventType::LogMessage,
                        data,
                    };
                    cb(&evt, GLOBAL_CTX);
                }
            }
        }
    }

    fn flush(&self) {}
}

static LOGGER: SimpleLogger = SimpleLogger;

pub fn init_logger(cb: LibrespotCallback, ctx: *mut c_void) -> Result<(), SetLoggerError> {
    unsafe {
        GLOBAL_CB = Some(cb);
        GLOBAL_CTX = ctx;
    }
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info))
}
