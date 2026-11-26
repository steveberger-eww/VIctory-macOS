//! dbghelp.dll: the title's crash handler writes a minidump through
//! `MiniDumpWriteDump`. Under Wine that call suspends every thread through
//! kernel32 directly, past the guard in `kernel` (`SuspendThread`), and
//! reads regions `VirtualQuery` does not show (D3DMetal's mappings). In a
//! run of Grand Theft Auto VI that ended at the title's own assert, it then
//! faulted inside dbghelp, so the title's report never finished. VIctory's
//! crash report is written before the title's handler runs, so the dump is
//! refused instead; the rest of dbghelp is the host's.

#![allow(non_snake_case)] // exports keep their Windows names

use windows_sys::Win32::Foundation::{ERROR_NOT_SUPPORTED, SetLastError};

extern "system" fn MiniDumpWriteDump(
    _process: isize,
    _process_id: u32,
    _file: isize,
    _dump_type: u32,
    _exception: *const core::ffi::c_void,
    _user_streams: *const core::ffi::c_void,
    _callback: *const core::ffi::c_void,
) -> i32 {
    tracing::warn!("dbghelp: the title asked for a minidump; refused (VIctory's crash report has the faulting thread)");
    // SAFETY: sets this thread's last error, nothing else.
    unsafe { SetLastError(ERROR_NOT_SUPPORTED) };
    0
}

pub(crate) fn module() -> crate::HleModule {
    crate::HleModule {
        name: "dbghelp",
        dlls: &["dbghelp.dll"],
        exports: vec![crate::hle_export!(MiniDumpWriteDump)],
    }
}
