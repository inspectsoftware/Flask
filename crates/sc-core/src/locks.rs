//! Which processes have a file open, answered by the Restart Manager: the
//! same service installers use to find what is in the way of replacing a file.

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};
use windows::core::{PCWSTR, PWSTR};

fn check(code: WIN32_ERROR) -> crate::Result<()> {
    if code == ERROR_SUCCESS { Ok(()) } else { Err(crate::Error::from_hresult(code.to_hresult())) }
}

/// PIDs of the processes holding `path` open. Can take a noticeable moment,
/// so call it off the UI thread. Works for files, not folders.
pub fn holders(path: &str) -> crate::Result<Vec<u32>> {
    let wpath: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let mut session = 0u32;
    unsafe {
        check(RmStartSession(&mut session, None, PWSTR(key.as_mut_ptr())))?;
        let result = (|| {
            check(RmRegisterResources(session, Some(&[PCWSTR(wpath.as_ptr())]), None, None))?;
            let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
            // The list can grow between asking for its size and reading it.
            for _ in 0..4 {
                let (mut needed, mut count, mut reasons) = (0u32, infos.len() as u32, 0u32);
                let buf = (!infos.is_empty()).then_some(infos.as_mut_ptr());
                let code = RmGetList(session, &mut needed, &mut count, buf, &mut reasons);
                if code == ERROR_MORE_DATA {
                    infos = vec![RM_PROCESS_INFO::default(); needed as usize + 4];
                    continue;
                }
                check(code)?;
                return Ok(infos[..count as usize].iter().map(|i| i.Process.dwProcessId).collect());
            }
            Ok(Vec::new())
        })();
        let _ = RmEndSession(session);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn finds_the_process_holding_a_file() {
        let path = std::env::temp_dir().join(format!("sc-core-locks-{}.txt", std::process::id()));
        // The child inherits the file as its output and keeps it open while it waits.
        let out = std::fs::File::create(&path).unwrap();
        let mut child = Command::new("cmd").args(["/c", "pause"]).stdin(Stdio::piped()).stdout(out).spawn().unwrap();

        let found = holders(path.to_str().unwrap()).unwrap();
        assert!(found.contains(&child.id()), "{found:?}");

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!holders(path.to_str().unwrap()).unwrap().contains(&child.id()));
        std::fs::remove_file(&path).unwrap();
    }
}
