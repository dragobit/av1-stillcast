//! C ABI shim over [`crate::api`]. Thin by design: bytes in → bytes out,
//! caller-owned buffers. See `include/stillcast.h` for the C contract.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;

use crate::api::{expand_ivf, ExpandParams};

thread_local! {
    static LAST_ERROR: RefCell<Option<std::ffi::CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: Option<String>) {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = msg.and_then(|m| std::ffi::CString::new(m).ok());
    });
}

/// Latest error message on this thread, or NULL when the last call
/// succeeded. The pointer is valid until the next stillcast call on the
/// same thread; do not free it.
#[no_mangle]
pub extern "C" fn stillcast_last_error() -> *const std::ffi::c_char {
    LAST_ERROR.with(|e| {
        e.borrow()
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
    })
}

/// Expand a 2-frame IVF encode into a long static-video AV1 IVF stream.
///
/// Returns a malloc'd buffer of `*out_len` bytes (free with
/// `stillcast_free`), or NULL on error (see `stillcast_last_error`).
/// `fps == 0` keeps the input timebase; `gop_size` must be >= 2.
#[no_mangle]
pub extern "C" fn stillcast_expand(
    input: *const u8,
    input_len: usize,
    fps: u32,
    total_frames: u64,
    gop_size: u64,
    decoder_model: bool,
    out_len: *mut usize,
) -> *mut u8 {
    if !out_len.is_null() {
        unsafe { *out_len = 0 };
    }
    if input.is_null() || out_len.is_null() {
        set_last_error(Some("null pointer argument".into()));
        return ptr::null_mut();
    }
    let data = unsafe { slice::from_raw_parts(input, input_len) };
    let params = ExpandParams {
        fps: (fps > 0).then_some(fps),
        total_frames,
        gop_size,
        decoder_model,
    };
    match catch_unwind(AssertUnwindSafe(|| expand_ivf(data, &params))) {
        Ok(Ok(bytes)) => {
            unsafe { *out_len = bytes.len() };
            let mut b = bytes.into_boxed_slice();
            let p = b.as_mut_ptr();
            std::mem::forget(b);
            set_last_error(None);
            p
        }
        Ok(Err(e)) => {
            set_last_error(Some(format!("{e:#}")));
            ptr::null_mut()
        }
        Err(_) => {
            set_last_error(Some("internal panic".into()));
            ptr::null_mut()
        }
    }
}

/// Free a buffer returned by `stillcast_expand`. `len` must be the value
/// the call wrote into `out_len`.
///
/// # Safety
/// `p` must come from `stillcast_expand` and be freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn stillcast_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)));
    }
}
