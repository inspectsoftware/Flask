//! Native API declarations that windows-rs either omits or exposes only in a
//! cut-down documented form.

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_ACCESS_RIGHTS};

pub const SYSTEM_PROCESS_INFORMATION: u32 = 5;
pub const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
pub const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

#[link(name = "ntdll")]
unsafe extern "system" {
    pub fn NtQuerySystemInformation(
        class: u32,
        info: *mut c_void,
        len: u32,
        ret_len: *mut u32,
    ) -> i32;
    pub fn NtQueryInformationProcess(
        process: *mut c_void,
        class: u32,
        info: *mut c_void,
        len: u32,
        ret_len: *mut u32,
    ) -> i32;
    pub fn NtSuspendProcess(process: *mut c_void) -> i32;
    pub fn NtResumeProcess(process: *mut c_void) -> i32;
    pub fn RtlNtStatusToDosError(status: i32) -> u32;
}

pub fn check(status: i32) -> crate::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    let win32 = unsafe { RtlNtStatusToDosError(status) };
    Err(crate::Error::from_hresult(windows::core::HRESULT::from_win32(win32)))
}

/// Process handle that closes itself.
pub struct Process(pub HANDLE);

impl Process {
    pub fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> crate::Result<Self> {
        unsafe { OpenProcess(access, false, pid).map(Self) }
    }

    pub fn raw(&self) -> *mut c_void {
        self.0.0
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
