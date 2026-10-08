//! Things the user can do to a process, plus privilege helpers.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, ERROR_PRIVILEGE_NOT_HELD, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED, SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
    TokenPrimary,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWithFullMemory, MiniDumpWithFullMemoryInfo, MiniDumpWithHandleData, MiniDumpWithThreadInfo,
    MiniDumpWithUnloadedModules, MiniDumpWriteDump,
};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    CreateProcessWithTokenW, GetCurrentProcess, GetPriorityClass, GetProcessAffinityMask,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, IsProcessCritical, LOGON_WITH_PROFILE, NORMAL_PRIORITY_CLASS,
    OpenProcessToken, PROCESS_CREATION_FLAGS, PROCESS_DUP_HANDLE, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, PROCESS_VM_READ, REALTIME_PRIORITY_CLASS, STARTUPINFOW,
    SetPriorityClass, SetProcessAffinityMask, TerminateProcess, WaitForSingleObject,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::Result;
use crate::nt::{self, Process};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Realtime,
    High,
    AboveNormal,
    Normal,
    BelowNormal,
    Idle,
}

impl Priority {
    pub const ALL: [Priority; 6] =
        [Self::Realtime, Self::High, Self::AboveNormal, Self::Normal, Self::BelowNormal, Self::Idle];

    pub fn label(self) -> &'static str {
        match self {
            Self::Realtime => "Realtime",
            Self::High => "High",
            Self::AboveNormal => "Above normal",
            Self::Normal => "Normal",
            Self::BelowNormal => "Below normal",
            Self::Idle => "Low",
        }
    }

    fn class(self) -> PROCESS_CREATION_FLAGS {
        match self {
            Self::Realtime => REALTIME_PRIORITY_CLASS,
            Self::High => HIGH_PRIORITY_CLASS,
            Self::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Self::Normal => NORMAL_PRIORITY_CLASS,
            Self::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Self::Idle => IDLE_PRIORITY_CLASS,
        }
    }
}

pub fn terminate(pid: u32) -> Result<()> {
    let p = Process::open(pid, PROCESS_TERMINATE)?;
    unsafe { TerminateProcess(p.0, 1) }
}

/// True when ending the process stops Windows with a bluescreen. A process
/// that cannot even be opened reports false; it cannot be ended either.
pub fn is_critical(pid: u32) -> bool {
    // The kernel itself.
    if pid == 4 {
        return true;
    }
    let Ok(p) = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return false };
    let mut critical = BOOL::default();
    unsafe { IsProcessCritical(p.0, &mut critical).is_ok() && critical.as_bool() }
}

/// Ends the process and starts it again from `command_line`, as the same
/// user and at the same privilege level it had: the new process gets a copy
/// of the old one's token, not Flask's elevated one. Returns the new PID.
/// If the relaunch fails the process stays ended.
pub fn restart(pid: u32, image: &str, command_line: &str) -> Result<u32> {
    let command = if command_line.trim().is_empty() { format!("\"{image}\"") } else { command_line.to_owned() };
    let mut command: Vec<u16> = command.encode_utf16().chain([0]).collect();
    // ponytail: starts in the program's own folder, not wherever the old
    // process happened to be. Read the directory from the PEB if that matters.
    let dir = Path::new(image).parent().map(|d| d.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
    let dir: Vec<u16> = dir.encode_utf16().chain([0]).collect();
    let dir = if dir.len() > 1 { PCWSTR(dir.as_ptr()) } else { PCWSTR::null() };
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut started = PROCESS_INFORMATION::default();

    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE)?;
    unsafe {
        // Taken before the process dies; afterwards there is nothing to copy.
        let mut token = HANDLE::default();
        OpenProcessToken(p.0, TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY, &mut token)?;
        let mut primary = HANDLE::default();
        let copied = DuplicateTokenEx(token, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut primary);
        let _ = CloseHandle(token);
        copied?;

        let result = (|| {
            TerminateProcess(p.0, 1)?;
            // Many programs refuse to start while their old instance lingers.
            WaitForSingleObject(p.0, 5000);
            let mut env = std::ptr::null_mut();
            let _ = CreateEnvironmentBlock(&mut env, Some(primary), false);
            let with_token = CreateProcessWithTokenW(
                primary,
                LOGON_WITH_PROFILE,
                PCWSTR::null(),
                Some(PWSTR(command.as_mut_ptr())),
                CREATE_UNICODE_ENVIRONMENT,
                (!env.is_null()).then_some(env.cast_const()),
                dir,
                &startup,
                &mut started,
            );
            if !env.is_null() {
                let _ = DestroyEnvironmentBlock(env);
            }
            match with_token {
                // Only an unelevated Flask lacks the privilege, and then its
                // own token is no higher than the one being copied.
                Err(e) if e.code() == ERROR_PRIVILEGE_NOT_HELD.to_hresult() => CreateProcessW(
                    PCWSTR::null(),
                    Some(PWSTR(command.as_mut_ptr())),
                    None,
                    None,
                    false,
                    PROCESS_CREATION_FLAGS(0),
                    None,
                    dir,
                    &startup,
                    &mut started,
                ),
                other => other,
            }
        })();
        let _ = CloseHandle(primary);
        result?;
        let _ = CloseHandle(started.hThread);
        let _ = CloseHandle(started.hProcess);
    }
    Ok(started.dwProcessId)
}

pub fn suspend(pid: u32) -> Result<()> {
    let p = Process::open(pid, PROCESS_SUSPEND_RESUME)?;
    nt::check(unsafe { nt::NtSuspendProcess(p.raw()) })
}

pub fn resume(pid: u32) -> Result<()> {
    let p = Process::open(pid, PROCESS_SUSPEND_RESUME)?;
    nt::check(unsafe { nt::NtResumeProcess(p.raw()) })
}

pub fn set_priority(pid: u32, priority: Priority) -> Result<()> {
    let p = Process::open(pid, PROCESS_SET_INFORMATION)?;
    unsafe { SetPriorityClass(p.0, priority.class()) }
}

pub fn priority(pid: u32) -> Option<Priority> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let class = unsafe { GetPriorityClass(p.0) };
    Priority::ALL.into_iter().find(|p| p.class().0 == class)
}

/// (process mask, system mask): bit n set means logical processor n may be
/// used. Both are zero for a process that spans processor groups.
pub fn affinity(pid: u32) -> Option<(usize, usize)> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let (mut process, mut system) = (0usize, 0usize);
    unsafe { GetProcessAffinityMask(p.0, &mut process, &mut system).ok()? };
    Some((process, system))
}

pub fn set_affinity(pid: u32, mask: usize) -> Result<()> {
    let p = Process::open(pid, PROCESS_SET_INFORMATION)?;
    unsafe { SetProcessAffinityMask(p.0, mask) }
}

/// Writes a full-memory dump of the process to `path`, as Task Manager's
/// "Create dump file" does. Can take seconds, so call it off the UI thread.
/// The file holds everything the process had in memory, secrets included.
pub fn write_dump(pid: u32, path: &Path) -> Result<()> {
    let p = Process::open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_DUP_HANDLE)?;
    let file = std::fs::File::create(path)?;
    let kind = MiniDumpWithFullMemory
        | MiniDumpWithHandleData
        | MiniDumpWithUnloadedModules
        | MiniDumpWithFullMemoryInfo
        | MiniDumpWithThreadInfo;
    let result = unsafe { MiniDumpWriteDump(p.0, pid, HANDLE(file.as_raw_handle()), kind, None, None, None) };
    drop(file);
    if result.is_err() {
        // Do not leave a truncated dump behind looking like a real one.
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Identity of a process for tree walks: (pid, parent pid, creation time).
pub type TreeNode = (u32, u32, i64);

/// All descendants of `root`, deepest first, so killing in order never leaves
/// an orphan that could respawn its children. A "child" created before its
/// claimed parent is ignored: its real parent exited and the PID was reused.
pub fn descendants(root: u32, procs: &[TreeNode]) -> Vec<u32> {
    let Some(&(_, _, root_created)) = procs.iter().find(|p| p.0 == root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut frontier = vec![(root, root_created)];
    while let Some((parent, parent_created)) = frontier.pop() {
        for &(pid, ppid, created) in procs {
            if ppid == parent && pid != parent && pid != root && created >= parent_created && !out.contains(&pid) {
                out.push(pid);
                frontier.push((pid, created));
            }
        }
    }
    out.reverse();
    out
}

/// Terminates `root` and everything below it. Returns how many processes were
/// terminated and the last error, if any of them could not be.
pub fn terminate_tree(root: u32, procs: &[TreeNode]) -> (usize, Option<crate::Error>) {
    let mut killed = 0;
    let mut last_err = None;
    for pid in descendants(root, procs).into_iter().chain([root]) {
        match terminate(pid) {
            Ok(()) => killed += 1,
            Err(e) => last_err = Some(e),
        }
    }
    (killed, last_err)
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Lets an elevated instance open processes owned by other users. Has no
/// effect when not elevated.
pub fn enable_debug_privilege() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut luid = LUID::default();
        let mut ok = LookupPrivilegeValueW(None, SE_DEBUG_NAME, &mut luid).is_ok();
        if ok {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            ok = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None).is_ok()
                && windows::core::Error::from_thread().code().is_ok();
        }
        let _ = CloseHandle(token);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn descendants_are_deepest_first_and_skip_reused_pids() {
        let procs = [
            (10, 1, 100),
            (20, 10, 110),
            (30, 20, 120),
            (40, 10, 115),
            // Claims parent 10 but predates it: stale parent PID.
            (50, 10, 50),
            (60, 99, 130),
        ];
        let d = descendants(10, &procs);
        assert_eq!(d.len(), 3);
        assert!(d.contains(&20) && d.contains(&30) && d.contains(&40));
        let pos = |p| d.iter().position(|&x| x == p).unwrap();
        assert!(pos(30) < pos(20));
        assert!(descendants(60, &procs).is_empty());
        assert!(descendants(777, &procs).is_empty());
    }

    #[test]
    fn self_parented_entry_does_not_loop() {
        assert!(descendants(0, &[(0, 0, 0), (4, 0, 1)]).contains(&4));
    }

    #[test]
    fn priority_suspend_resume_terminate_on_child() {
        let mut child = Command::new("cmd").args(["/c", "pause"]).stdin(std::process::Stdio::piped()).spawn().unwrap();
        let pid = child.id();

        assert_eq!(priority(pid), Some(Priority::Normal));
        set_priority(pid, Priority::BelowNormal).unwrap();
        assert_eq!(priority(pid), Some(Priority::BelowNormal));

        suspend(pid).unwrap();
        resume(pid).unwrap();

        let (_, system) = affinity(pid).unwrap();
        if system != 0 {
            let one_core = 1 << system.trailing_zeros();
            set_affinity(pid, one_core).unwrap();
            assert_eq!(affinity(pid), Some((one_core, system)));
        }

        assert!(is_critical(4) && !is_critical(pid));

        let dump = std::env::temp_dir().join(format!("sc-core-test-{pid}.dmp"));
        write_dump(pid, &dump).unwrap();
        assert!(std::fs::metadata(&dump).unwrap().len() > 0);
        std::fs::remove_file(&dump).unwrap();

        terminate(pid).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(1));
    }

    #[test]
    fn restart_brings_back_the_same_program_under_a_new_pid() {
        let mut child = Command::new("cmd").args(["/c", "pause"]).stdin(std::process::Stdio::piped()).spawn().unwrap();
        let old = child.id();
        let image = crate::procinfo::image_path(old).unwrap();
        // /k keeps the new one alive without needing input.
        let new = restart(old, &image, &format!("\"{image}\" /k")).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(1));
        assert_ne!(new, old);
        assert!(crate::procinfo::image_path(new).unwrap().eq_ignore_ascii_case(&image));
        terminate(new).unwrap();
        assert!(restart(0xFFFF_FFF3, &image, "").is_err());
    }

    #[test]
    fn missing_process_reports_error() {
        assert!(terminate(0xFFFF_FFF3).is_err());
        assert!(suspend(0xFFFF_FFF3).is_err());
        assert!(priority(0xFFFF_FFF3).is_none());
        assert!(affinity(0xFFFF_FFF3).is_none());
        let dump = std::env::temp_dir().join("sc-core-test-missing.dmp");
        assert!(write_dump(0xFFFF_FFF3, &dump).is_err());
        assert!(!dump.exists());
    }
}
