//! Reading what a C caller passed in, refusing what cannot be read.

use std::ffi::{CStr, c_char};

use crate::error::{EDGE_EAR_NOT_UTF8, EDGE_EAR_NULL_ARGUMENT, fail_with};

/// Borrow a required string. `Err` is the code to return.
pub fn required_str<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, i32> {
    if ptr.is_null() {
        return Err(fail_with(
            EDGE_EAR_NULL_ARGUMENT,
            &format!("{what} must not be null"),
        ));
    }
    // Safe as long as the caller kept its promise that this is a
    // null-terminated string it owns for the length of the call.
    let raw = unsafe { CStr::from_ptr(ptr) };
    raw.to_str()
        .map_err(|_| fail_with(EDGE_EAR_NOT_UTF8, &format!("{what} is not valid UTF-8")))
}

/// Borrow a string that may be absent.
pub fn optional_str<'a>(ptr: *const c_char, what: &str) -> Result<Option<&'a str>, i32> {
    if ptr.is_null() {
        return Ok(None);
    }
    required_str(ptr, what).map(Some)
}

/// Refuse a null out parameter rather than write through it.
pub fn out_ptr<T>(ptr: *mut T, what: &str) -> Result<&'static mut T, i32> {
    if ptr.is_null() {
        return Err(fail_with(
            EDGE_EAR_NULL_ARGUMENT,
            &format!("{what} must not be null"),
        ));
    }
    Ok(unsafe { &mut *ptr })
}

/// Seconds from C, refused if they could not be a duration.
pub fn duration(seconds: f64, what: &str) -> Result<std::time::Duration, i32> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(fail_with(
            crate::error::EDGE_EAR_INVALID_VALUE,
            &format!("{what} must be zero or more seconds, not {seconds}"),
        ));
    }
    Ok(std::time::Duration::from_secs_f64(seconds))
}
