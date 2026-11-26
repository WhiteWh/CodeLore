//! Load an explicitly selected libclang without linking it into lore.
use std::ffi::{c_char, c_void, CStr};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::LoreError;

pub(super) struct Library(*mut c_void);

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
}

#[cfg(unix)]
#[cfg_attr(any(target_os = "linux", target_os = "android"), link(name = "dl"))]
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

impl Library {
    pub fn open(path: &Path) -> Result<Self, LoreError> {
        let path = std::fs::canonicalize(path).map_err(|e| super::error(format!("{}: {e}", path.display())))?;
        // Clang can register process/thread teardown handlers. Keep each loaded
        // module mapped until process exit, even after disposing its indexes.
        static LOADED: OnceLock<Mutex<BTreeMap<PathBuf, usize>>> = OnceLock::new();
        let mut loaded = LOADED.get_or_init(Mutex::default).lock().map_err(super::error)?;
        if let Some(&handle) = loaded.get(&path) {
            return Ok(Self(handle as *mut c_void));
        }
        #[cfg(windows)]
        let handle = {
            use std::os::windows::ffi::OsStrExt;
            let name: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
            // Search the library directory and the OS default directories for dependencies.
            unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), 0x100 | 0x1000) }
        };
        #[cfg(unix)]
        let handle = {
            let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(super::error)?;
            unsafe { dlopen(name.as_ptr(), 2) } // RTLD_NOW
        };
        if handle.is_null() {
            #[cfg(windows)]
            let message = std::io::Error::last_os_error().to_string();
            #[cfg(unix)]
            let message = unsafe {
                let cause = dlerror();
                if cause.is_null() { "dlopen failed".to_owned() } else { CStr::from_ptr(cause).to_string_lossy().into_owned() }
            };
            return Err(super::error(format!("could not load {}: {message}", path.display())));
        }
        loaded.insert(path, handle as usize);
        Ok(Self(handle))
    }

    pub fn symbol(&self, name: &CStr) -> Result<*mut c_void, LoreError> {
        #[cfg(windows)]
        let address = unsafe { GetProcAddress(self.0, name.as_ptr()) };
        #[cfg(unix)]
        let address = unsafe { dlsym(self.0, name.as_ptr()) };
        if address.is_null() {
            return Err(super::error(format!("libclang lacks {}", name.to_string_lossy())));
        }
        Ok(address)
    }
}
