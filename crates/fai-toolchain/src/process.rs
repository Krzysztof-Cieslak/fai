//! Windows process inheritance shared by the launcher and compiler.

/// Prevents extra copies of our standard handles from leaking into descendants.
///
/// `Command` duplicates the requested stdio handles, but `CreateProcess` also
/// inherits every other inheritable handle. Clearing the originals before each
/// forwarding hop lets a later detached daemon close the caller's pipes fully.
/// This does not close the handles or change the child's requested stdio.
#[allow(unsafe_code)]
pub fn prevent_stdio_inheritance() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `id` is a documented standard-handle selector. The returned
        // handle is borrowed from the process, or is a null/invalid sentinel.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: this only clears the inheritance flag on a borrowed standard
        // handle. Missing or non-settable handles require no cleanup.
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}
