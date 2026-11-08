//! The one operating-system call the GPU paths need: loading a library by name
//! or path, so a delay-loaded DLL is present before the first call into it.

/// Loads `name` (a DLL name searched the usual way, or a full path); false
/// when Windows cannot.
#[cfg(windows)]
pub fn load_library(name: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut std::ffi::c_void;
    }
    let wide: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call;
    // the module stays loaded for the life of the process, which is the point.
    !unsafe { LoadLibraryW(wide.as_ptr()) }.is_null()
}

#[cfg(not(windows))]
pub fn load_library(_name: &std::ffi::OsStr) -> bool {
    true
}
