//! Opens, pins and loads a plugin DLL the way docs/load-policy.md ("No check-then-swap gap")
//! says:
//!
//! 1. open the file with `FILE_SHARE_READ` only, so nobody can write or delete it meanwhile;
//! 2. hash it through that handle and compare with the pin (`E_HASH`);
//! 3. `LoadLibraryExW` on the absolute path while the handle is still open, searching only the
//!    plugin's own folder and System32 for its dependencies; then close the handle;
//! 4. resolve `w2r_plugin_main` (`E_ENTRY`).
//!
//! The registry then runs checks 9 and 10 on the entry point.

use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use sha2::{Digest, Sha256};
use w2r_abi::{W2R_PLUGIN_MAIN_SYMBOL, W2rPluginMain};
use windows_sys::Win32::Foundation::{GetLastError, HMODULE};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use crate::dispatch::LoadError;

/// A loaded plugin module. Never freed: there is no live unload in version 1, and `FreeLibrary`
/// on a module that faulted is not safe.
pub struct Dll {
    module: HMODULE,
    pub entry: W2rPluginMain,
    pub sha256: [u8; 32],
}

impl Dll {
    pub fn module(&self) -> HMODULE {
        self.module
    }

    /// The plugin's optional `w2r_map_draw` export.
    pub fn map_draw(&self) -> Option<w2r_abi::W2rMapDrawFn> {
        let sym = unsafe { GetProcAddress(self.module, w2r_abi::W2R_MAP_DRAW_SYMBOL.as_ptr().cast()) }?;
        Some(unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, w2r_abi::W2rMapDrawFn>(sym) })
    }
}

fn entry_error(detail: String) -> LoadError {
    LoadError {
        code: "E_ENTRY",
        detail,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_reader(mut r: impl Read) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buf[..n]);
    }
}

/// SHA-256 of a file. The manager pins this; tests use it to pin their fixtures.
pub fn sha256_of(path: &Path) -> io::Result<[u8; 32]> {
    sha256_reader(File::open(path)?)
}

pub fn load(path: &Path, pin: &[u8; 32]) -> Result<Dll, LoadError> {
    // LOAD_LIBRARY_SEARCH_* flags require an absolute path.
    let abs = std::path::absolute(path).map_err(|e| entry_error(format!("bad path: {e}")))?;

    let file = File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&abs)
        .map_err(|e| entry_error(format!("cannot open {}: {e}", abs.display())))?;
    let sha256 = sha256_reader(&file).map_err(|e| entry_error(format!("cannot read: {e}")))?;
    if &sha256 != pin {
        return Err(LoadError {
            code: "E_HASH",
            detail: format!("sha256 {} differs from pin {}", hex(&sha256), hex(pin)),
        });
    }

    let wide: Vec<u16> = abs.as_os_str().encode_wide().chain([0]).collect();
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            core::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    // Writers were locked out from the hash until here.
    drop(file);
    if module.is_null() {
        let err = unsafe { GetLastError() };
        return Err(entry_error(format!("LoadLibraryExW failed: error {err}")));
    }

    let Some(sym) = (unsafe { GetProcAddress(module, W2R_PLUGIN_MAIN_SYMBOL.as_ptr().cast()) })
    else {
        return Err(entry_error("no w2r_plugin_main export".into()));
    };
    // Same size and ABI (cdecl on x86 is what both `extern "system"` and `extern "C"` mean
    // there); the header fixes the real signature.
    let entry: W2rPluginMain = unsafe { core::mem::transmute(sym) };
    Ok(Dll {
        module,
        entry,
        sha256,
    })
}
