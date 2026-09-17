//! Handing what the library logs to the application.

use std::ffi::{CString, c_char, c_void};
use std::sync::{OnceLock, RwLock};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// How much to hand over, most to least severe. Each level includes
/// the ones above it.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum edge_ear_log_level {
    /// Nothing is handed over.
    EDGE_EAR_LOG_OFF = 0,
    /// Something failed.
    EDGE_EAR_LOG_ERROR = 1,
    /// Something is wrong but the library carried on.
    EDGE_EAR_LOG_WARN = 2,
    /// Devices opening, capture starting and stopping, the wake word.
    EDGE_EAR_LOG_INFO = 3,
    /// Detail for working out why something behaved as it did.
    EDGE_EAR_LOG_DEBUG = 4,
    /// Everything. Nothing is logged per block of audio even here.
    EDGE_EAR_LOG_TRACE = 5,
}

/// Called for every message the library logs. Both strings stop being
/// valid when it returns, so anything kept must be copied first.
#[allow(non_camel_case_types)]
pub type edge_ear_log_cb = Option<
    unsafe extern "C" fn(
        level: edge_ear_log_level,
        target: *const c_char,
        message: *const c_char,
        user: *mut c_void,
    ),
>;

/// Where the messages go, and how far down. The raw pointer is why
/// this is not `Send` on its own: the caller owns what it points to.
#[derive(Clone, Copy)]
struct Sink {
    callback: edge_ear_log_cb,
    user: *mut c_void,
    level: LevelFilter,
}

unsafe impl Send for Sink {}
unsafe impl Sync for Sink {}

static SINK: RwLock<Option<Sink>> = RwLock::new(None);
static BRIDGE: Bridge = Bridge;
static INSTALLED: OnceLock<bool> = OnceLock::new();

struct Bridge;

impl Log for Bridge {
    fn enabled(&self, metadata: &Metadata) -> bool {
        held().is_some_and(|sink| metadata.level() <= sink.level)
    }

    fn log(&self, record: &Record) {
        // Copied out and the lock let go of before the call, so a
        // callback that sets another one cannot wedge the thread.
        let Some(sink) = held() else { return };
        let Some(callback) = sink.callback else {
            return;
        };
        if record.level() > sink.level {
            return;
        }

        // These own the strings for the length of the call and are
        // dropped after it, which is exactly the promised lifetime.
        let Ok(target) = CString::new(record.target()) else {
            return;
        };
        let Ok(message) = CString::new(record.args().to_string()) else {
            return;
        };
        unsafe {
            callback(
                level_out(record.level()),
                target.as_ptr(),
                message.as_ptr(),
                sink.user,
            )
        };
    }

    fn flush(&self) {}
}

fn held() -> Option<Sink> {
    *SINK.read().unwrap_or_else(|e| e.into_inner())
}

fn level_out(level: Level) -> edge_ear_log_level {
    match level {
        Level::Error => edge_ear_log_level::EDGE_EAR_LOG_ERROR,
        Level::Warn => edge_ear_log_level::EDGE_EAR_LOG_WARN,
        Level::Info => edge_ear_log_level::EDGE_EAR_LOG_INFO,
        Level::Debug => edge_ear_log_level::EDGE_EAR_LOG_DEBUG,
        Level::Trace => edge_ear_log_level::EDGE_EAR_LOG_TRACE,
    }
}

fn level_in(level: edge_ear_log_level) -> LevelFilter {
    match level {
        edge_ear_log_level::EDGE_EAR_LOG_OFF => LevelFilter::Off,
        edge_ear_log_level::EDGE_EAR_LOG_ERROR => LevelFilter::Error,
        edge_ear_log_level::EDGE_EAR_LOG_WARN => LevelFilter::Warn,
        edge_ear_log_level::EDGE_EAR_LOG_INFO => LevelFilter::Info,
        edge_ear_log_level::EDGE_EAR_LOG_DEBUG => LevelFilter::Debug,
        edge_ear_log_level::EDGE_EAR_LOG_TRACE => LevelFilter::Trace,
    }
}

/// Point the library's log at this callback, or nowhere when it is
/// none. False means something else logs for this process already.
pub fn point_at(callback: edge_ear_log_cb, level: edge_ear_log_level, user: *mut c_void) -> bool {
    if !INSTALLED.get_or_init(|| log::set_logger(&BRIDGE).is_ok()) {
        return false;
    }

    let wanted = match callback {
        Some(_) => level_in(level),
        None => LevelFilter::Off,
    };
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = callback.map(|_| Sink {
        callback,
        user,
        level: wanted,
    });
    // Set last, so no thread formats a message the sink is not yet
    // ready for.
    log::set_max_level(wanted);
    true
}
