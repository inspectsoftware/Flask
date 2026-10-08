//! Per-process details that need a process handle. These are comparatively
//! expensive, so callers fetch them once per process and cache the result.

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, PSID, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleFileNameExW, LIST_MODULES_ALL};
use windows::Win32::System::Threading::{
    OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ, QueryFullProcessImageNameW,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::core::{PCWSTR, PWSTR, w};

use crate::nt::{self, Process};

/// Company name from the file's version resource, e.g. "Microsoft Corporation".
/// This is whatever the file claims; it is not a verified signature.
pub fn company(path: &str) -> Option<String> {
    let wpath: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let wpath = PCWSTR(wpath.as_ptr());
    unsafe {
        let len = GetFileVersionInfoSizeW(wpath, None);
        if len == 0 {
            return None;
        }
        let mut data = vec![0u8; len as usize];
        GetFileVersionInfoW(wpath, None, len, data.as_mut_ptr().cast()).ok()?;

        // The string table is keyed by language and code page.
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut size = 0u32;
        if !VerQueryValueW(data.as_ptr().cast(), w!(r"\VarFileInfo\Translation"), &mut ptr, &mut size).as_bool()
            || size < 4
        {
            return None;
        }
        let (lang, codepage) = (*(ptr as *const u16), *(ptr as *const u16).add(1));
        let key: Vec<u16> = format!(r"\StringFileInfo\{lang:04x}{codepage:04x}\CompanyName")
            .encode_utf16()
            .chain([0])
            .collect();
        if !VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut ptr, &mut size).as_bool() || size == 0 {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr as *const u16, size as usize);
        let text = String::from_utf16_lossy(chars);
        let text = text.trim_end_matches('\0').trim();
        (!text.is_empty()).then(|| text.to_owned())
    }
}

/// Full Win32 path of the process image.
pub fn image_path(pid: u32) -> Option<String> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(p.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).ok()? };
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Full paths of the program and every DLL loaded into the process, in load
/// order. Empty when the process cannot be read.
pub fn modules(pid: u32) -> Vec<String> {
    let Ok(p) = Process::open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ) else { return Vec::new() };
    let mut handles = vec![HMODULE::default(); 512];
    loop {
        let bytes = (handles.len() * size_of::<HMODULE>()) as u32;
        let mut needed = 0u32;
        if unsafe { EnumProcessModulesEx(p.0, handles.as_mut_ptr(), bytes, &mut needed, LIST_MODULES_ALL) }.is_err() {
            return Vec::new();
        }
        let count = needed as usize / size_of::<HMODULE>();
        if count <= handles.len() {
            handles.truncate(count);
            break;
        }
        handles.resize(count + 32, HMODULE::default());
    }
    let mut name = [0u16; 1024];
    handles
        .into_iter()
        .filter_map(|module| {
            let len = unsafe { GetModuleFileNameExW(Some(p.0), Some(module), &mut name) } as usize;
            (len > 0).then(|| String::from_utf16_lossy(&name[..len]))
        })
        .collect()
}

pub fn command_line(pid: u32) -> Option<String> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    // UNICODE_STRING header followed by the characters; u64 storage keeps it aligned.
    let mut buf = vec![0u64; 1024];
    loop {
        let mut needed = 0u32;
        let status = unsafe {
            nt::NtQueryInformationProcess(
                p.raw(),
                nt::PROCESS_COMMAND_LINE_INFORMATION,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        };
        if status == nt::STATUS_INFO_LENGTH_MISMATCH && needed as usize > buf.len() * 8 {
            buf.resize((needed as usize).div_ceil(8), 0);
            continue;
        }
        if status < 0 {
            return None;
        }
        break;
    }
    let len_bytes = (buf[0] & 0xFFFF) as usize;
    let ptr = buf[1] as *const u16;
    let base = buf.as_ptr() as usize;
    let start = ptr as usize;
    if ptr.is_null() || start < base || start + len_bytes > base + buf.len() * 8 {
        return None;
    }
    let chars = unsafe { std::slice::from_raw_parts(ptr, len_bytes / 2) };
    Some(String::from_utf16_lossy(chars))
}

fn lookup_sid(sid: PSID) -> Option<String> {
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut kind = SID_NAME_USE::default();
    unsafe {
        LookupAccountSidW(
            None,
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        )
        .ok()?
    };
    Some(String::from_utf16_lossy(&name[..name_len as usize]))
}

/// Account the process runs as, without the domain part. Windows only
/// discloses this for other accounts' processes to elevated callers.
pub fn user_name(pid: u32) -> Option<String> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(p.0, TOKEN_QUERY, &mut token).ok()? };
    let mut buf = [0u64; 32];
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), (buf.len() * 8) as u32, &mut len)
    };
    unsafe {
        let _ = CloseHandle(token);
    }
    ok.ok()?;
    lookup_sid(unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_for_own_process() {
        let me = std::process::id();
        let path = image_path(me).unwrap();
        assert!(path.to_lowercase().ends_with(".exe"), "{path}");
        assert!(!command_line(me).unwrap().is_empty());
        assert_eq!(user_name(me).unwrap().to_lowercase(), std::env::var("USERNAME").unwrap().to_lowercase());
    }

    #[test]
    fn company_from_version_resource() {
        let system = std::env::var("SystemRoot").unwrap();
        assert_eq!(company(&format!(r"{system}\System32\notepad.exe")).as_deref(), Some("Microsoft Corporation"));
        assert!(company(r"C:\no\such\file.exe").is_none());
    }

    #[test]
    fn missing_process_is_none() {
        // PIDs are multiples of 4, so this one never exists.
        assert!(image_path(0xFFFF_FFF3).is_none());
        assert!(command_line(0xFFFF_FFF3).is_none());
        assert!(user_name(0xFFFF_FFF3).is_none());
    }

    #[test]
    fn modules_of_this_process_include_the_system_loader() {
        let mods = modules(std::process::id());
        assert!(mods.iter().any(|m| m.to_lowercase().ends_with(r"\ntdll.dll")), "{mods:?}");
        assert!(mods[0].to_lowercase().ends_with(".exe"));
        assert!(modules(0xFFFF_FFF3).is_empty());
    }
}
