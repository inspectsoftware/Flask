//! Which processes count as "apps": the ones that own a window the user can
//! see and switch to. Everything else is background.

use std::collections::HashSet;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

/// Shell surfaces that are visible top-level windows but not apps.
const SHELL_CLASSES: [&str; 4] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

unsafe fn is_app_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }
        // Owned windows are dialogs and popups of some other window.
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return false;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // Cloaked: on another virtual desktop, or a suspended store app's shell.
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        if cloaked != 0 {
            return false;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..len]);
        !SHELL_CLASSES.contains(&class.as_str())
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        if is_app_window(hwnd) {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                (*(lp.0 as *mut HashSet<u32>)).insert(pid);
            }
        }
        true.into()
    }
}

/// PIDs of processes that currently own an app window.
pub fn app_pids() -> HashSet<u32> {
    let mut pids = HashSet::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut pids as *mut HashSet<u32> as isize));
    }
    pids
}

#[cfg(test)]
mod tests {
    #[test]
    fn enumerating_does_not_include_idle_or_system() {
        let pids = super::app_pids();
        assert!(!pids.contains(&0) && !pids.contains(&4));
    }
}
